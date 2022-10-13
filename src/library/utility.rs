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
// (c) Robert Swinford <robert.swinford<...at...>gmail.com>
//
// For the full copyright and license information, please view the LICENSE file
// that was distributed with this source code.

use std::{
    borrow::Cow,
    fs::{copy, create_dir_all, read_dir, DirEntry, FileType},
    io::{self, Read, Write},
    path::{Component::RootDir, Path, PathBuf},
    time::SystemTime,
};

use lscolors::{Colorable, LsColors, Style};
use time::{format_description, OffsetDateTime};

use crate::data::paths::{BasicDirEntryInfo, PathData};
use crate::exec::display_main::display_exec;
use crate::exec::interactive::SelectionCandidate;
use crate::library::results::{HttmError, HttmResult};
use crate::{config::init::Config, data::filesystem_map::MapLiveToSnaps};
use crate::{FilesystemType, BTRFS_SNAPPER_HIDDEN_DIRECTORY, ZFS_SNAPSHOT_DIRECTORY};

const TMP_SUFFIX: &str = ".tmp";

pub fn make_tmp_path(path: &Path) -> PathBuf {
    let path_string = path.to_string_lossy().to_string();
    let res = path_string + TMP_SUFFIX;
    PathBuf::from(res)
}

pub fn print_display_map(config: &Config, map_live_to_snaps: MapLiveToSnaps) -> HttmResult<()> {
    let output_buf = display_exec(config, &map_live_to_snaps)?;
    print_output_buf(output_buf)?;
    Ok(())
}

pub fn copy_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    if PathBuf::from(src).is_dir() {
        create_dir_all(&dst)?;
        for entry in read_dir(src)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                copy_recursive(&entry.path(), &dst.join(&entry.file_name()))?;
            } else {
                copy(&entry.path(), &dst.join(&entry.file_name()))?;
            }
        }
    } else {
        copy(src, dst)?;
    }
    Ok(())
}

pub fn read_stdin() -> HttmResult<Vec<String>> {
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let mut buffer = Vec::new();
    stdin.read_to_end(&mut buffer)?;

    let buffer_string = std::str::from_utf8(&buffer)?;

    let broken_string: Vec<String> = if buffer_string.contains(&['\n', '\0']) {
        // always split on newline or null char, if available
        buffer_string
            .split(&['\n', '\0'])
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned())
            .collect()
    } else if buffer_string.contains('\"') {
        buffer_string
            .split('\"')
            // unquoted paths should have excess whitespace trimmed
            .map(|s| s.trim())
            // remove any empty strings
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned())
            .collect::<Vec<String>>()
    } else {
        buffer_string
            .split_ascii_whitespace()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned())
            .collect()
    };

    Ok(broken_string)
}

pub fn get_common_path<I, P>(paths: I) -> Option<PathBuf>
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

    if !common_path.as_os_str().is_empty() && common_path.as_os_str() != RootDir.as_os_str() {
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
    out_locked.flush()?;

    Ok(())
}

// is this path/dir_entry something we should count as a directory for our purposes?
pub fn httm_is_dir<T>(entry: &T) -> bool
where
    T: HttmIsDir,
{
    let path = entry.get_path();
    match entry.get_filetype() {
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

pub trait HttmIsDir {
    fn get_filetype(&self) -> Result<FileType, std::io::Error>;
    fn get_path(&self) -> PathBuf;
}

impl HttmIsDir for Path {
    fn get_filetype(&self) -> Result<FileType, std::io::Error> {
        Ok(self.metadata()?.file_type())
    }
    fn get_path(&self) -> PathBuf {
        self.to_path_buf()
    }
}

impl HttmIsDir for PathData {
    fn get_filetype(&self) -> Result<FileType, std::io::Error> {
        Ok(self.path_buf.metadata()?.file_type())
    }
    fn get_path(&self) -> PathBuf {
        self.path_buf.clone()
    }
}

impl HttmIsDir for DirEntry {
    fn get_filetype(&self) -> Result<FileType, std::io::Error> {
        self.file_type()
    }
    fn get_path(&self) -> PathBuf {
        self.path()
    }
}

impl HttmIsDir for BasicDirEntryInfo {
    fn get_filetype(&self) -> Result<FileType, std::io::Error> {
        //  of course, this is a placeholder error, we just need an error to report back
        //  why not store the error in the struct instead?  because it's more complex.  it might
        //  make it harder to copy around etc
        self.file_type
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
    fn get_path(&self) -> PathBuf {
        self.path.clone()
    }
}

lazy_static! {
    static ref PHANTOM_STYLE: Style =
        Style::from_ansi_sequence("38;2;250;200;200;1;0").unwrap_or_default();
    static ref ENV_LS_COLORS: LsColors = LsColors::from_env().unwrap_or_default();
}

pub fn paint_string<T>(path: T, display_name: &str) -> Cow<str>
where
    T: PaintString,
{
    if path.get_is_phantom() {
        // paint all other phantoms/deleted files the same color, light pink
        let ansi_style = &Style::to_ansi_term_style(&PHANTOM_STYLE);
        Cow::Owned(ansi_style.paint(display_name).to_string())
    } else if let Some(style) = path.get_ls_style() {
        let ansi_style = &Style::to_ansi_term_style(style);
        Cow::Owned(ansi_style.paint(display_name).to_string())
    } else {
        // if a non-phantom file that should not be colored (sometimes -- your regular files)
        // or just in case if all else fails, don't paint and return string
        Cow::Borrowed(display_name)
    }
}

pub trait PaintString {
    fn get_ls_style(&self) -> Option<&'_ lscolors::style::Style>;
    fn get_is_phantom(&self) -> bool;
}

impl PaintString for &PathData {
    fn get_ls_style(&self) -> Option<&lscolors::style::Style> {
        ENV_LS_COLORS.style_for_path(self.path_buf.as_path())
    }
    fn get_is_phantom(&self) -> bool {
        self.metadata.is_none()
    }
}

impl PaintString for &SelectionCandidate {
    fn get_ls_style(&self) -> Option<&lscolors::style::Style> {
        ENV_LS_COLORS.style_for(self)
    }
    fn get_is_phantom(&self) -> bool {
        self.file_type().is_none()
    }
}

pub fn get_fs_type_from_hidden_dir(dataset_mount: &Path) -> HttmResult<FilesystemType> {
    // set fstype, known by whether there is a ZFS hidden snapshot dir in the root dir
    let fs_type = if dataset_mount
        .join(ZFS_SNAPSHOT_DIRECTORY)
        .metadata()
        .is_ok()
    {
        FilesystemType::Zfs
    } else if dataset_mount
        .join(BTRFS_SNAPPER_HIDDEN_DIRECTORY)
        .metadata()
        .is_ok()
    {
        FilesystemType::Btrfs
    } else {
        return Err(HttmError::new(
                "Requesting a filesystem type from path is only available for ZFS datasets and btrfs datasets snapshot-ed via snapper.",
            )
            .into());
    };

    Ok(fs_type)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DateFormat {
    Display,
    Timestamp,
}

static DATE_FORMAT_DISPLAY: &str =
    "[weekday repr:short] [month repr:short] [day] [hour]:[minute]:[second] [year]";
static DATE_FORMAT_TIMESTAMP: &str = "[year]-[month]-[day]-[hour]:[minute]:[second]";

pub fn get_date(config: &Config, system_time: &SystemTime, format: DateFormat) -> String {
    let date_time: OffsetDateTime = (*system_time).into();

    let date_format = format_description::parse(get_date_format(format))
        .expect("timestamp date format is invalid");

    date_time
        .to_offset(config.requested_utc_offset)
        .format(&date_format)
        .expect("timestamp date format could not be applied to the date supplied")
}

fn get_date_format<'a>(format: DateFormat) -> &'a str {
    match format {
        DateFormat::Display => DATE_FORMAT_DISPLAY,
        DateFormat::Timestamp => DATE_FORMAT_TIMESTAMP,
    }
}