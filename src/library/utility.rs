//       ___           ___           ___           ___
//      /\__\         /\  \         /\  \         /\__\
//     /:/  /         \:\  \        \:\  \       /::|  |
//    /:/__/           \:\  \        \:\  \     /:|:|  |
//   /::\  \ ___       /::\  \       /::\  \   /:/|:|__|__
//  /:/\:\  /\__\     /:/\:\__\     /:/\:\__\ /:/ |::::\__\
//  \/__\:\/:/  /    /:/  \/__/    /:/  \/__/ \/__/~~/:/  /
//       \::/  /    /:/  /        /:/  /            /:/  /
//       /:/  /     \/__/         \/__/            /:/  /
//      /:/  /                                    /:/  /
//      \/__/                                     \/__/
//
// Copyright (c) 2023, Robert Swinford <robert.swinford<...at...>gmail.com>
//
// For the full copyright and license information, please view the LICENSE file
// that was distributed with this source code.

use std::{
    borrow::Cow,
    fs::{create_dir_all, read_dir, set_permissions, FileType},
    io::{self, Read, Write},
    iter::Iterator,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::SystemTime,
};

use crossbeam_channel::{Receiver, TryRecvError};
use lscolors::{Colorable, LsColors, Style};
use nu_ansi_term::Style as AnsiTermStyle;
use number_prefix::NumberPrefix;
use once_cell::sync::Lazy;
use time::{format_description, OffsetDateTime, UtcOffset};
use which::which;

use crate::data::paths::{BasicDirEntryInfo, PathData, PHANTOM_DATE};
use crate::data::selection::SelectionCandidate;
use crate::library::diff_copy::diff_copy;
use crate::library::results::{HttmError, HttmResult};
use crate::parse::aliases::FilesystemType;
use crate::GLOBAL_CONFIG;
use crate::{config::generate::PrintMode, data::paths::PathMetadata};
use crate::{BTRFS_SNAPPER_HIDDEN_DIRECTORY, ZFS_SNAPSHOT_DIRECTORY};
use std::process::Command as ExecProcess;

pub fn user_has_effective_root() -> HttmResult<()> {
    if !nix::unistd::geteuid().is_root() {
        return Err(HttmError::new("Superuser privileges are require to execute.").into());
    }

    Ok(())
}

pub fn user_has_zfs_allow_snap_priv(new_file_path: &Path) -> HttmResult<()> {
    let zfs_command = which("zfs")?;

    let pathdata = PathData::from(new_file_path);

    let dataset_mount =
        pathdata.proximate_dataset(&GLOBAL_CONFIG.dataset_collection.map_of_datasets)?;

    let dataset_name = match GLOBAL_CONFIG
        .dataset_collection
        .map_of_datasets
        .get(dataset_mount)
    {
        Some(md) => &md.source,
        None => return Err(HttmError::new("Could not obtain source dataset for mount: ").into()),
    };

    let dataset_name = &dataset_name.to_string_lossy();
    let process_args = vec!["allow", dataset_name];

    let process_output = ExecProcess::new(zfs_command).args(&process_args).output()?;
    let stderr_string = std::str::from_utf8(&process_output.stderr)?.trim();
    let stdout_string = std::str::from_utf8(&process_output.stdout)?.trim();

    // stderr_string is a string not an error, so here we build an err or output
    if !stderr_string.is_empty() {
        let msg = "httm was unable to determine 'zfs allow' for the path given. The 'zfs' command issued the following error: ".to_owned() + stderr_string;

        return Err(HttmError::new(&msg).into());
    }

    let user_name = std::env::var("USER")?;

    if !stdout_string.contains(&user_name)
        || !stdout_string.contains("mount")
        || !stdout_string.contains("snapshot")
    {
        let msg = "User does not have 'zfs allow' privileges for the path given.";

        return Err(HttmError::new(msg).into());
    }

    Ok(())
}

pub fn delimiter() -> char {
    if matches!(GLOBAL_CONFIG.print_mode, PrintMode::RawZero) {
        '\0'
    } else {
        '\n'
    }
}

pub enum Never {}

pub fn is_channel_closed(chan: &Receiver<Never>) -> bool {
    match chan.try_recv() {
        Ok(never) => match never {},
        Err(TryRecvError::Disconnected) => true,
        Err(TryRecvError::Empty) => false,
    }
}

const TMP_SUFFIX: &str = ".tmp";

pub fn make_tmp_path(path: &Path) -> PathBuf {
    let path_string = path.to_string_lossy().to_string();
    let res = path_string + TMP_SUFFIX;
    PathBuf::from(res)
}

pub fn copy_attributes(src: &Path, dst: &Path) -> HttmResult<()> {
    let src_metadata = src.symlink_metadata()?;

    // Mode
    {
        set_permissions(dst, src_metadata.permissions())?
    }

    // ACLs - requires libacl1-dev to build
    #[cfg(feature = "acls")]
    {
        if let Ok(acls) = exacl::getfacl(src, None) {
            acls.into_iter()
                .try_for_each(|acl| exacl::setfacl(&[dst], &[acl], None))?;
        }
    }

    // Ownership
    {
        let dst_uid = src_metadata.uid();
        let dst_gid = src_metadata.gid();

        nix::unistd::chown(dst, Some(dst_uid.into()), Some(dst_gid.into()))?
    }

    // XAttrs
    {
        if let Ok(xattrs) = xattr::list(src) {
            xattrs
                .flat_map(|attr| xattr::get(src, attr.clone()).map(|opt_value| (attr, opt_value)))
                .filter_map(|(attr, opt_value)| opt_value.map(|value| (attr, value)))
                .try_for_each(|(attr, value)| xattr::set(dst, attr, value.as_slice()))?
        }
    }

    // Timestamps
    {
        use filetime::FileTime;

        let mtime = FileTime::from_last_modification_time(&src_metadata);
        let atime = FileTime::from_last_access_time(&src_metadata);

        // does not follow symlinks
        filetime::set_symlink_file_times(dst, atime, mtime)?
    }

    Ok(())
}

pub fn preserve_recursive(src: &Path, dst: &Path) -> HttmResult<()> {
    let dst_pathdata: PathData = dst.into();

    let proximate_dataset_mount =
        dst_pathdata.proximate_dataset(&GLOBAL_CONFIG.dataset_collection.map_of_datasets)?;

    let relative_path_components_len = dst_pathdata
        .relative_path(proximate_dataset_mount)?
        .to_path_buf()
        .components()
        .count();

    src.ancestors()
        .zip(dst.ancestors())
        .take(relative_path_components_len)
        .try_for_each(|(src_ancestor, dst_ancestor)| copy_attributes(src_ancestor, dst_ancestor))
}

pub fn copy_direct(src: &Path, dst: &Path, should_preserve: bool) -> HttmResult<()> {
    if src.is_dir() {
        create_dir_all(dst)?;
    } else {
        generate_dst_parent(dst)?;

        if src.is_symlink() {
            let link_target = std::fs::read_link(src)?;
            std::os::unix::fs::symlink(link_target, dst)?;
        }

        if src.is_file() {
            diff_copy(src, dst)?;
        }
    }

    if should_preserve {
        preserve_recursive(src, dst)?;
    }

    Ok(())
}

pub fn generate_dst_parent(dst: &Path) -> HttmResult<()> {
    if let Some(dst_parent) = dst.parent() {
        create_dir_all(dst_parent)?;
    } else {
        let msg = format!("Could not detect a parent for destination file: {:?}", dst);
        return Err(HttmError::new(&msg).into());
    }

    Ok(())
}

pub fn copy_recursive(src: &Path, dst: &Path, should_preserve: bool) -> HttmResult<()> {
    if src.is_dir() {
        copy_direct(src, dst, should_preserve)?;

        for entry in read_dir(src)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let entry_src = entry.path();
            let entry_dst = dst.join(entry.file_name());

            if entry_src.exists() {
                if file_type.is_dir() {
                    copy_recursive(&entry_src, &entry_dst, should_preserve)?;
                } else {
                    copy_direct(src, dst, should_preserve)?;
                }
            }
        }
    } else {
        copy_direct(src, dst, should_preserve)?;
    }

    Ok(())
}

pub fn remove_recursive(src: &Path) -> HttmResult<()> {
    if src.is_dir() {
        let entries = read_dir(src)?;

        for entry in entries {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let path = entry.path();

            if path.exists() {
                if file_type.is_dir() {
                    remove_recursive(&path)?
                } else {
                    std::fs::remove_file(path)?
                }
            }
        }

        if src.exists() {
            std::fs::remove_dir_all(src)?
        }
    } else if src.exists() {
        std::fs::remove_file(src)?
    }

    Ok(())
}

pub fn read_stdin() -> HttmResult<Vec<PathData>> {
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let mut buffer = Vec::new();
    stdin.read_to_end(&mut buffer)?;

    let buffer_string = std::str::from_utf8(&buffer)?;

    let broken_string = if buffer_string.contains(['\n', '\0']) {
        // always split on newline or null char, if available
        buffer_string
            .split(&['\n', '\0'])
            .filter(|s| !s.is_empty())
            .map(PathData::from)
            .collect()
    } else if buffer_string.contains('\"') {
        buffer_string
            .split('\"')
            // unquoted paths should have excess whitespace trimmed
            .map(str::trim)
            // remove any empty strings
            .filter(|s| !s.is_empty())
            .map(PathData::from)
            .collect()
    } else {
        buffer_string
            .split_ascii_whitespace()
            .filter(|s| !s.is_empty())
            .map(PathData::from)
            .collect()
    };

    Ok(broken_string)
}

pub fn find_common_path<I, P>(paths: I) -> Option<PathBuf>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let mut path_iter = paths.into_iter();
    let initial_value = path_iter.next()?.as_ref().to_path_buf();

    path_iter.try_fold(initial_value, |acc, path| cmp_path(acc, path))
}

fn cmp_path<A: AsRef<Path>, B: AsRef<Path>>(a: A, b: B) -> Option<PathBuf> {
    // skip the root dir,
    let a_components = a.as_ref().components();
    let b_components = b.as_ref().components();

    let common_path: PathBuf = a_components
        .zip(b_components)
        .take_while(|(a_path, b_path)| a_path == b_path)
        .map(|(a_path, _b_path)| a_path)
        .collect();

    if common_path.components().count() > 1 {
        Some(common_path)
    } else {
        None
    }
}

pub fn print_output_buf(output_buf: String) -> HttmResult<()> {
    // mutex keeps threads from writing over each other
    let out = std::io::stdout();
    let mut out_locked = out.lock();
    out_locked.write_all(output_buf.as_bytes())?;
    out_locked.flush().map_err(std::convert::Into::into)
}

// is this path/dir_entry something we should count as a directory for our purposes?
pub fn httm_is_dir<'a, T>(entry: &'a T) -> bool
where
    T: HttmIsDir<'a> + ?Sized,
{
    let path = entry.path();

    match entry.filetype() {
        Ok(file_type) => match file_type {
            file_type if file_type.is_dir() => true,
            file_type if file_type.is_file() => false,
            file_type if file_type.is_symlink() => {
                // canonicalize will read_link/resolve the link for us
                match path.canonicalize() {
                    Ok(link_target) if !link_target.is_dir() => false,
                    Ok(link_target) => path.ancestors().all(|ancestor| ancestor != link_target),
                    // we get an error? still pass the path on, as we get a good path from the dir entry
                    _ => false,
                }
            }
            // char, block, etc devices(?), None/Errs are not dirs, and we have a good path to pass on, so false
            _ => false,
        },
        _ => false,
    }
}

pub trait HttmIsDir<'a> {
    fn httm_is_dir(&self) -> bool;
    fn filetype(&self) -> Result<FileType, std::io::Error>;
    fn path(&'a self) -> &'a Path;
}

impl<T: AsRef<Path>> HttmIsDir<'_> for T {
    fn httm_is_dir(&self) -> bool {
        httm_is_dir(self)
    }
    fn filetype(&self) -> Result<FileType, std::io::Error> {
        Ok(self.as_ref().symlink_metadata()?.file_type())
    }
    fn path(&self) -> &Path {
        self.as_ref()
    }
}

impl<'a> HttmIsDir<'a> for PathData {
    fn httm_is_dir(&self) -> bool {
        httm_is_dir(self)
    }
    fn filetype(&self) -> Result<FileType, std::io::Error> {
        Ok(self.path_buf.symlink_metadata()?.file_type())
    }
    fn path(&'a self) -> &'a Path {
        &self.path_buf
    }
}

impl<'a> HttmIsDir<'a> for BasicDirEntryInfo {
    fn httm_is_dir(&self) -> bool {
        httm_is_dir(self)
    }
    fn filetype(&self) -> Result<FileType, std::io::Error> {
        //  of course, this is a placeholder error, we just need an error to report back
        //  why not store the error in the struct instead?  because it's more complex.  it might
        //  make it harder to copy around etc
        self.file_type
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
    fn path(&'a self) -> &'a Path {
        &self.path
    }
}

static ENV_LS_COLORS: Lazy<LsColors> = Lazy::new(|| LsColors::from_env().unwrap_or_default());
static PHANTOM_STYLE: Lazy<AnsiTermStyle> = Lazy::new(|| {
    Style::to_nu_ansi_term_style(
        &Style::from_ansi_sequence("38;2;250;200;200;1;0").unwrap_or_default(),
    )
});

pub fn paint_string<T>(path: T, display_name: &str) -> Cow<str>
where
    T: PaintString,
{
    if path.is_phantom() {
        // paint all other phantoms/deleted files the same color, light pink
        return Cow::Owned(PHANTOM_STYLE.paint(display_name).to_string());
    }

    if let Some(style) = path.ls_style() {
        let ansi_style: &AnsiTermStyle = &Style::to_nu_ansi_term_style(style);
        return Cow::Owned(ansi_style.paint(display_name).to_string());
    }

    // if a non-phantom file that should not be colored (sometimes -- your regular files)
    // or just in case if all else fails, don't paint and return string
    Cow::Borrowed(display_name)
}

pub trait PaintString {
    fn ls_style(&self) -> Option<&'_ lscolors::style::Style>;
    fn is_phantom(&self) -> bool;
}

impl PaintString for &PathData {
    fn ls_style(&self) -> Option<&lscolors::style::Style> {
        ENV_LS_COLORS.style_for_path(&self.path_buf)
    }
    fn is_phantom(&self) -> bool {
        self.metadata.is_none()
    }
}

impl PaintString for &SelectionCandidate {
    fn ls_style(&self) -> Option<&lscolors::style::Style> {
        ENV_LS_COLORS.style_for(self)
    }
    fn is_phantom(&self) -> bool {
        self.file_type().is_none()
    }
}

pub fn fs_type_from_hidden_dir(dataset_mount: &Path) -> Option<FilesystemType> {
    // set fstype, known by whether there is a ZFS hidden snapshot dir in the root dir
    if dataset_mount
        .join(ZFS_SNAPSHOT_DIRECTORY)
        .symlink_metadata()
        .is_ok()
    {
        Some(FilesystemType::Zfs)
    } else if dataset_mount
        .join(BTRFS_SNAPPER_HIDDEN_DIRECTORY)
        .symlink_metadata()
        .is_ok()
    {
        Some(FilesystemType::Btrfs)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DateFormat {
    Display,
    Timestamp,
}

static DATE_FORMAT_DISPLAY: &str =
    "[weekday repr:short] [month repr:short] [day] [hour]:[minute]:[second] [year]";
static DATE_FORMAT_TIMESTAMP: &str = "[year]-[month]-[day]-[hour]:[minute]:[second]";

pub fn date_string(
    utc_offset: UtcOffset,
    system_time: &SystemTime,
    date_format: DateFormat,
) -> String {
    let date_time: OffsetDateTime = (*system_time).into();

    let parsed_format = format_description::parse(date_string_format(&date_format))
        .expect("timestamp date format is invalid");

    let raw_string = date_time
        .to_offset(utc_offset)
        .format(&parsed_format)
        .expect("timestamp date format could not be applied to the date supplied");

    if utc_offset == UtcOffset::UTC {
        return match &date_format {
            DateFormat::Timestamp => raw_string + "_UTC",
            DateFormat::Display => raw_string + " UTC",
        };
    }

    raw_string
}

fn date_string_format<'a>(format: &DateFormat) -> &'a str {
    match format {
        DateFormat::Display => DATE_FORMAT_DISPLAY,
        DateFormat::Timestamp => DATE_FORMAT_TIMESTAMP,
    }
}

pub fn display_human_size(size: u64) -> String {
    let size = size as f64;

    match NumberPrefix::binary(size) {
        NumberPrefix::Standalone(bytes) => format!("{bytes} bytes"),
        NumberPrefix::Prefixed(prefix, n) => format!("{n:.1} {prefix}B"),
    }
}

pub fn is_metadata_same<T>(src: T, dst: T) -> HttmResult<()>
where
    T: ComparePathMetadata,
{
    if src.opt_metadata().is_none() {
        let msg = format!("WARNING: Metadata not found: {:?}", src.path());
        return Err(HttmError::new(&msg).into());
    }

    if src.path().is_symlink() && src.path().read_link().ok() != dst.path().read_link().ok() {
        let msg = format!("WARNING: Symlink do not match: {:?}", src.path());
        return Err(HttmError::new(&msg).into());
    }

    if src.opt_metadata() != dst.opt_metadata() {
        let msg = format!(
            "WARNING: Metadata mismatch: {:?} !-> {:?}",
            src.path(),
            dst.path()
        );
        return Err(HttmError::new(&msg).into());
    }

    Ok(())
}

pub trait ComparePathMetadata {
    fn opt_metadata(&self) -> Option<PathMetadata>;
    fn path(&self) -> &Path;
}

impl<T: AsRef<Path>> ComparePathMetadata for T {
    fn opt_metadata(&self) -> Option<PathMetadata> {
        // never follow symlinks for comparison
        let opt_md = self.as_ref().symlink_metadata().ok();

        opt_md.map(|md| PathMetadata {
            size: md.len(),
            modify_time: md.modified().unwrap_or(PHANTOM_DATE),
        })
    }

    fn path(&self) -> &Path {
        self.as_ref()
    }
}
