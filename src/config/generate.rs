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

use std::ops::Index;
use std::path::Path;

use clap::OsValues;
use rayon::prelude::*;

use clap::{crate_name, crate_version, Arg, ArgMatches};
use indicatif::ProgressBar;
use time::UtcOffset;

use crate::config::install_hot_keys::install_hot_keys;
use crate::data::filesystem_info::FilesystemInfo;
use crate::data::paths::PathData;
use crate::library::results::{HttmError, HttmResult};
use crate::library::utility::{read_stdin, HttmIsDir};
use crate::ROOT_DIRECTORY;

#[derive(Debug, Clone)]
pub enum ExecMode {
    Interactive(InteractiveMode),
    NonInteractiveRecursive(indicatif::ProgressBar),
    Display,
    SnapFileMount(String),
    Prune(Option<ListSnapsFilters>),
    MountsForFiles(MountDisplay),
    SnapsForFiles(Option<ListSnapsFilters>),
    NumVersions(NumVersionsMode),
    RollForward(RollForwardConfig),
}

#[derive(Debug, Clone)]
pub struct RollForwardConfig {
    pub full_snap_name: String,
    pub progress_bar: indicatif::ProgressBar,
}

#[derive(Debug, Clone)]
pub enum BulkExclusion {
    NoLive,
    NoSnap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountDisplay {
    Target,
    Source,
    RelativePath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractiveMode {
    Browse,
    Select,
    Restore(RestoreMode),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreSnapGuard {
    Guarded,
    NotGuarded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreMode {
    CopyOnly,
    CopyAndPreserve,
    Overwrite(RestoreSnapGuard),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrintMode {
    FormattedDefault,
    FormattedNotPretty,
    RawNewline,
    RawZero,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeletedMode {
    DepthOfOne,
    All,
    Only,
}

#[derive(Debug, Clone)]
pub enum ListSnapsOfType {
    All,
    UniqueMetadata,
    UniqueContents,
}

#[derive(Debug, Clone)]
pub struct ListSnapsFilters {
    pub select_mode: bool,
    pub omit_num_snaps: usize,
    pub name_filters: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LastSnapMode {
    Any,
    Without,
    DittoOnly,
    NoDittoExclusive,
    NoDittoInclusive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NumVersionsMode {
    AllNumerals,
    AllGraph,
    SingleAll,
    SingleNoSnap,
    SingleWithSnap,
    Multiple,
}

fn parse_args() -> ArgMatches {
    clap::Command::new(crate_name!())
        .about("httm prints the size, date and corresponding locations of available unique versions of files residing on snapshots.  \
        May also be used interactively to select and restore from such versions, and even to snapshot datasets which contain certain files.")
        .version(crate_version!())
        .arg(
            Arg::new("INPUT_FILES")
                .help("in any non-interactive mode, put requested paths here.  If you include no paths as arguments, \
                then httm will pause waiting for input on stdin.  In any interactive mode, \
                this is the directory search path. If no directory is specified, \
                httm will use the current working directory.")
                .takes_value(true)
                .multiple_values(true)
                .value_parser(clap::builder::ValueParser::os_string())
                .display_order(1)
        )
        .arg(
            Arg::new("BROWSE")
                .short('b')
                .short_alias('i')
                .long("browse")
                .visible_alias("interactive")
                .help("interactive browse and search a specified directory to display unique file versions.")
                .display_order(2)
        )
        .arg(
            Arg::new("SELECT")
                .short('s')
                .long("select")
                .help("interactive browse and search a specified directory to display unique file versions.  Continue to another dialog to select a snapshot version to dump to stdout.")
                .conflicts_with("RESTORE")
                .display_order(3)
        )
        .arg(
            Arg::new("RESTORE")
                .short('r')
                .long("restore")
                .takes_value(true)
                .default_missing_value("copy")
                .possible_values(["copy", "copy-and-preserve", "overwrite", "yolo", "guard"])
                .min_values(0)
                .require_equals(true)
                .help("interactive browse and search a specified directory to display unique file versions.  Continue to another dialog to select a snapshot version to restore.  \
                This argument optionally takes a value.  Default behavior/value is a non-destructive \"copy\" to the current working directory with a new name, \
                so as not to overwrite any \"live\" file version.  However, the user may specify \"overwrite\" (or \"yolo\") to restore to the same file location.  Note, \"overwrite\" can be a DESTRUCTIVE operation.  \
                Overwrite mode will attempt to preserve attributes, like the permissions/mode, timestamps, xattrs and ownership of the selected snapshot file version (this is and will likely remain a UNIX only feature).  \
                In order to preserve such attributes in \"copy\" mode, specify the \"copy-and-preserve\" value.  User may also specify \"guard\".  \
                Guard mode has the same semantics as \"overwrite\" but will attempt to take a precautionary snapshot before any overwrite action occurs.  \
                Note: Guard mode is a ZFS only option.")
                .conflicts_with("SELECT")
                .display_order(4)
        )
        .arg(
            Arg::new("DELETED")
                .short('d')
                .long("deleted")
                .takes_value(true)
                .default_missing_value("all")
                .possible_values(["all", "single", "only"])
                .require_equals(true)
                .min_values(0)
                .require_equals(true)
                .help("show deleted files in interactive modes.  In non-interactive modes, do a search for all files deleted from a specified directory. \
                This argument optionally takes a value.  The default behavior/value is \"all\".  \
                If \"only\" is specified, then, in the interactive modes, non-deleted files will be excluded from the search. \
                If \"single\" is specified, then, deleted files behind deleted directories, (that is -- files with a depth greater than one) will be ignored.")
                .display_order(5)
        )
        .arg(
            Arg::new("RECURSIVE")
                .short('R')
                .long("recursive")
                .conflicts_with_all(&["SNAPSHOT"])
                .help("recurse into the selected directory to find more files. Only available in interactive and deleted file modes.")
                .display_order(6)
        )
        .arg(
            Arg::new("ALT_REPLICATED")
                .short('a')
                .long("alt-replicated")
                .help("automatically discover locally replicated datasets and list their snapshots as well.  \
                NOTE: Be certain such replicated datasets are mounted before use.  \
                httm will silently ignore unmounted datasets in the interactive modes.")
                .conflicts_with_all(&["REMOTE_DIR", "LOCAL_DIR"])
                .display_order(7)
        )
        .arg(
            Arg::new("PREVIEW")
                .short('p')
                .long("preview")
                .help("user may specify a command to preview snapshots while in select view.  This argument optionally takes a value specifying the command to be executed.  \
                The default value/command, if no command value specified, is a 'bowie' formatted 'diff'.  \
                User defined commands must specify the snapshot file name \"{snap_file}\" and the live file name \"{live_file}\" within their shell command.")
                .takes_value(true)
                .min_values(0)
                .require_equals(true)
                .default_missing_value("default")
                .display_order(8)
        )
        .arg(
            Arg::new("UNIQUENESS")
                .long("uniqueness")
                .visible_aliases(&["unique"])
                .takes_value(true)
                .default_missing_value("contents")
                .possible_values(["all", "no-filter", "metadata", "contents"])
                .min_values(0)
                .require_equals(true)
                .help("comparing file versions solely on the basis of size and modify time (the default \"metadata\" behavior) may return what appear to be \"false positives\", \
                in the sense that, modify time is not a precise measure of whether a file has actually changed.  A program might overwrite a file with the same contents, \
                or a user can simply update the modify time via 'touch'.  If only this flag is specified, the \"contents\" option compares the actual file contents of file versions, if their sizes match, \
                and overrides the default \"metadata\" behavior.  The \"contents\" option can be expensive, as the file versions need to be read back and compared, and should probably only be used for smaller files.  \
                Given how expensive this operation can be, for larger files or files with many versions, \"contents\" option is not shown in Interactive browse mode, \
                but after a selection is made, can be utilized in Select or Restore modes.  The \"all\" or \"no-filter\" option dumps all snapshot versions, and no attempt is made to determine if the file versions are distinct.")
                .display_order(9)
        )
        .arg(
            Arg::new("EXACT")
                .short('e')
                .long("exact")
                .help("use exact pattern matching for searches in the interactive modes (in contrast to the default fuzzy searching).")
                .display_order(10)
        )
        .arg(
            Arg::new("SNAPSHOT")
                .short('S')
                .long("snap")
                .takes_value(true)
                .min_values(0)
                .require_equals(true)
                .default_missing_value("httmSnapFileMount")
                .visible_aliases(&["snap-file", "snapshot", "snap-file-mount"])
                .help("snapshot a file/s most immediate mount.  \
                This argument optionally takes a value for a snapshot suffix.  The default suffix is 'httmSnapFileMount'.  \
                Note: This is a ZFS only option which requires either superuser or 'zfs allow' privileges.")
                .conflicts_with_all(&["BROWSE", "SELECT", "RESTORE", "ALT_REPLICATED", "REMOTE_DIR", "LOCAL_DIR"])
                .display_order(11)
        )
        .arg(
            Arg::new("LIST_SNAPS")
                .long("list-snaps")
                .aliases(&["snaps-for-file", "ls-snaps", "list-snapshots"])
                .takes_value(true)
                .min_values(0)
                .require_equals(true)
                .multiple_values(false)
                .help("display snapshots names for a file.  This argument optionally takes a value.  \
                By default, this argument will return all available snapshot names.  User may limit type of snapshots returned via the UNIQUENESS flag.  \
                The user may also omit the last \"n\" snapshots from any list.  By appending a comma, this argument also filters those snapshots which contain the specified pattern/s.  \
                A value of \"5,prep_Apt\" would return the snapshot names of only the last 5 (at most) of all snapshot versions which contain \"prep_Apt\".  \
                The value \"native\" will restrict selection to only 'httm' native snapshot suffix values, like \"httmSnapFileMount\" and \"ounceSnapFileMount\".  \
                Note: This is a ZFS only option.")
                .conflicts_with_all(&["BROWSE", "RESTORE"])
                .display_order(12)
        )
        .arg(
            Arg::new("ROLL_FORWARD")
                .long("roll-forward")
                .aliases(&["roll", "spring", "spring-forward"])
                .takes_value(true)
                .min_values(1)
                .require_equals(true)
                .multiple_values(false)
                .help("traditionally 'zfs rollback' is a destructive operation, whereas httm roll-forward is non-destructive.  \
                httm will copy only files and their attributes that have changed since a specified snapshot, from that snapshot, to its live dataset.  \
                httm will also take two precautionary snapshots, one before and one after the copy.  \
                Should the roll forward fail for any reason, httm will roll back to the pre-execution state.  \
                Caveats: This is a ZFS only option which requires super user privileges.")
                .conflicts_with_all(&["BROWSE", "RESTORE", "ALT_REPLICATED", "REMOTE_DIR", "LOCAL_DIR"])
                .display_order(13)
        )
        .arg(
            Arg::new("PRUNE")
                .long("prune")
                .aliases(&["purge"])
                .help("prune all snapshot/s which contain the input file/s on that file's most immediate mount via \"zfs destroy\".  \
                \"zfs destroy\" is a DESTRUCTIVE operation which *does not* only apply to the file in question, but the entire snapshot upon which it resides.  \
                Careless use may cause you to lose snapshot data you care about.  \
                This argument requires and will be filtered according to any values specified at LIST_SNAPS.  \
                User may also enable SELECT mode to make a granular selection of specific snapshots to prune.  \
                Note: This is a ZFS only option.")
                .conflicts_with_all(&["BROWSE", "RESTORE", "ALT_REPLICATED", "REMOTE_DIR", "LOCAL_DIR"])
                .requires("LIST_SNAPS")
                .display_order(13)
        )
        .arg(
            Arg::new("FILE_MOUNT")
                .short('m')
                .long("file-mount")
                .alias("mount-for-file")
                .visible_alias("mount")
                .takes_value(true)
                .default_missing_value("target")
                .possible_values(["source", "target", "directory", "device", "dataset", "relative-path", "relative", "relpath"])
                .min_values(0)
                .require_equals(true)
                .help("display the all mount point/s of all dataset/s which contain/s the input file/s.  \
                This argument optionally takes a value.  Possible values are: \
                \"target\" or \"directory\", return the directory upon which the underlying dataset or device of the mount, \
                \"source\" or \"device\" or \"dataset\", return the underlying dataset/device of the mount, and, \
                \"relative-path\" or \"relative\", return the path relative to the underlying dataset/device of the mount.")
                .conflicts_with_all(&["BROWSE", "SELECT", "RESTORE"])
                .display_order(14)
        )
        .arg(
            Arg::new("LAST_SNAP")
                .short('l')
                .long("last-snap")
                .takes_value(true)
                .default_missing_value("any")
                .possible_values(["any", "ditto", "no-ditto", "no-ditto-exclusive", "no-ditto-inclusive", "none", "without"])
                .min_values(0)
                .require_equals(true)
                .help("automatically select and print the path of last-in-time unique snapshot version for the input file.  \
                This argument optionally takes a value.  Possible values are: \
                \"any\", return the last in time snapshot version, this is the default behavior/value, \
                \"ditto\", return only last snaps which are the same as the live file version, \
                \"no-ditto-exclusive\", return only a last snap which is not the same as the live version (argument \"--no-ditto\" is an alias for this option), \
                \"no-ditto-inclusive\", return a last snap which is not the same as the live version, or should none exist, return the live file, and, \
                \"none\" or \"without\", return the live file only for those files without a last snapshot.")
                .conflicts_with_all(&["NUM_VERSIONS", "SNAPSHOT", "FILE_MOUNT", "ALT_REPLICATED", "REMOTE_DIR", "LOCAL_DIR"])
                .display_order(15)
        )
        .arg(
            Arg::new("RAW")
                .short('n')
                .long("raw")
                .visible_alias("newline")
                .help("display the snapshot locations only, without extraneous information, delimited by a NEWLINE character.")
                .conflicts_with_all(&["ZEROS", "NOT_SO_PRETTY"])
                .display_order(16)
        )
        .arg(
            Arg::new("ZEROS")
                .short('0')
                .long("zero")
                .help("display the snapshot locations only, without extraneous information, delimited by a NULL character.")
                .conflicts_with_all(&["RAW", "NOT_SO_PRETTY"])
                .display_order(17)
        )
        .arg(
            Arg::new("NOT_SO_PRETTY")
                .long("not-so-pretty")
                .visible_aliases(&["tabs", "plain-jane", "not-pretty"])
                .help("display the ordinary output, but tab delimited, without any pretty border lines.")
                .conflicts_with_all(&["RAW", "ZEROS"])
                .display_order(18)
        )
        .arg(
            Arg::new("JSON")
                .long("json")
                .help("display the ordinary output, but as formatted JSON.")
                .conflicts_with_all(&["SELECT", "RESTORE"])
                .display_order(19)
        )
        .arg(
            Arg::new("OMIT_DITTO")
                .long("omit-ditto")
                .help("omit display of the snapshot version which may be identical to the live version (`httm` ordinarily displays all snapshot versions and the live version).")
                .conflicts_with_all(&["NUM_VERSIONS"])
                .display_order(20)
        )
        .arg(
            Arg::new("NO_FILTER")
                .long("no-filter")
                .help("by default, in the interactive modes, httm will filter out files residing upon non-supported datasets (like ext4, tmpfs, procfs, sysfs, or devtmpfs, etc.), and within any \"common\" snapshot paths.  \
                Here, one may select to disable such filtering.  httm, however, will always show the input path, and results from behind any input path when that is the path being searched.")
                .display_order(21)
        )
        .arg(
            Arg::new("FILTER_HIDDEN")
                .long("no-hidden")
                .aliases(&["no-hide", "nohide", "filter-hidden"])
                .help("never show information regarding hidden files and directories (those that start with a \'.\') in the recursive or interactive modes.")
                .display_order(22)
        )
        .arg(
            Arg::new("ONE_FILESYSTEM")
                .long("one-filesystem")
                .aliases(&["same-filesystem", "single-filesystem", "one-fs", "onefs"])
                .requires("RECURSIVE")
                .help("limit recursive search to file and directories on the same filesystem/device as the target directory.")
                .display_order(23)
        )
        .arg(
            Arg::new("NO_TRAVERSE")
                .long("no-traverse")
                .help("in recursive mode, don't traverse symlinks.  Although httm does its best to prevent searching pathologically recursive symlink-ed paths, \
                here, you may disable symlink traversal completely.  NOTE: httm will never traverse symlinks when a requested recursive search is on the root/base directory (\"/\").")
                .display_order(24)
        )
        .arg(
            Arg::new("NO_LIVE")
                .long("no-live")
                .visible_aliases(&["dead", "disco"])
                .help("only display information concerning snapshot versions (display no information regarding live versions of files or directories).")
                .display_order(25)
        )
        .arg(
            Arg::new("NO_SNAP")
                .long("no-snap")
                .visible_aliases(&["undead", "zombie"])
                .help("only display information concerning 'pseudo-live' versions in Display Recursive mode (in --deleted, --recursive, but non-interactive modes).  \
                Useful for finding the \"files that once were\" and displaying only those pseudo-live/zombie files.")
                .conflicts_with_all(&["BROWSE", "SELECT", "RESTORE", "SNAPSHOT", "LAST_SNAP", "NOT_SO_PRETTY"])
                .requires("DELETED")
                .display_order(26)
        )
        .arg(
            Arg::new("MAP_ALIASES")
                .long("map-aliases")
                .visible_aliases(&["aliases"])
                .help("manually map a local directory (eg. \"/Users/<User Name>\") as an alias of a mount point for ZFS or btrfs, \
                such as the local mount point for a backup on a remote share (eg. \"/Volumes/Home\").  \
                This option is useful if you wish to view snapshot versions from within the local directory you back up to your remote share.  \
                This option requires a value.  Such a value is delimited by a colon, ':', and is specified in the form <LOCAL_DIR>:<REMOTE_DIR> \
                (eg. --map-aliases /Users/<User Name>:/Volumes/Home).  Multiple maps may be specified delimited by a comma, ','.  \
                You may also set via the environment variable HTTM_MAP_ALIASES.")
                .use_value_delimiter(true)
                .takes_value(true)
                .value_parser(clap::builder::ValueParser::os_string())
                .display_order(27)
        )
        .arg(
            Arg::new("NUM_VERSIONS")
                .long("num-versions")
                .default_missing_value("all")
                .possible_values(["all", "graph", "single", "single-no-snap", "single-with-snap", "multiple"])
                .min_values(0)
                .require_equals(true)
                .help("detect and display the number of unique versions available (e.g. one, \"1\", \
                version is available if either a snapshot version exists, and is identical to live version, or only a live version exists).  \
                This argument optionally takes a value.  The default value, \"all\", will print the filename and number of versions, \
                \"graph\" will print the filename and a line of characters representing the number of versions, \
                \"single\" will print only filenames which only have one version, \
                (and \"single-no-snap\" will print those without a snap taken, and \"single-with-snap\" will print those with a snap taken), \
                and \"multiple\" will print only filenames which only have multiple versions.")
                .conflicts_with_all(&["LAST_SNAP", "BROWSE", "SELECT", "RESTORE", "RECURSIVE", "SNAPSHOT", "NOT_SO_PRETTY", "NO_LIVE", "NO_SNAP", "OMIT_DITTO", "RAW", "ZEROS"])
                .display_order(28)
        )
        .arg(
            Arg::new("REMOTE_DIR")
                .long("remote-dir")
                .hide(true)
                .visible_aliases(&["remote", "snap-point"])
                .help("DEPRECATED.  Use MAP_ALIASES. Manually specify that mount point for ZFS (directory which contains a \".zfs\" directory) or btrfs-snapper \
                (directory which contains a \".snapshots\" directory), such as the local mount point for a remote share.  You may also set via the HTTM_REMOTE_DIR environment variable.")
                .takes_value(true)
                .value_parser(clap::builder::ValueParser::os_string())
                .display_order(29)
        )
        .arg(
            Arg::new("LOCAL_DIR")
                .long("local-dir")
                .hide(true)
                .visible_alias("local")
                .help("DEPRECATED.  Use MAP_ALIASES.  Used with \"remote-dir\" to determine where the corresponding live root filesystem of the dataset is.  \
                Put more simply, the \"local-dir\" is likely the directory you backup to your \"remote-dir\".  If not set, httm defaults to your current working directory.  \
                You may also set via the environment variable HTTM_LOCAL_DIR.")
                .requires("REMOTE_DIR")
                .takes_value(true)
                .value_parser(clap::builder::ValueParser::os_string())
                .display_order(30)
        )
        .arg(
            Arg::new("UTC")
                .long("utc")
                .help("use UTC for date display and timestamps")
                .display_order(31)
        )
        .arg(
            Arg::new("DEBUG")
                .long("debug")
                .help("print configuration and debugging info")
                .display_order(32)
        )
        .arg(
            Arg::new("ZSH_HOT_KEYS")
                .long("install-zsh-hot-keys")
                .help("install zsh hot keys to the users home directory, and then exit")
                .exclusive(true)
                .display_order(33)
        )
        .get_matches()
}

#[derive(Debug, Clone)]
pub struct Config {
    pub paths: Vec<PathData>,
    pub opt_recursive: bool,
    pub opt_exact: bool,
    pub opt_no_filter: bool,
    pub opt_debug: bool,
    pub opt_no_traverse: bool,
    pub opt_omit_ditto: bool,
    pub opt_no_hidden: bool,
    pub opt_json: bool,
    pub opt_one_filesystem: bool,
    pub uniqueness: ListSnapsOfType,
    pub opt_bulk_exclusion: Option<BulkExclusion>,
    pub opt_last_snap: Option<LastSnapMode>,
    pub opt_preview: Option<String>,
    pub opt_deleted_mode: Option<DeletedMode>,
    pub opt_requested_dir: Option<PathData>,
    pub requested_utc_offset: UtcOffset,
    pub exec_mode: ExecMode,
    pub print_mode: PrintMode,
    pub dataset_collection: FilesystemInfo,
    pub pwd: PathData,
}

impl Config {
    pub fn new() -> HttmResult<Self> {
        let arg_matches = parse_args();
        let config = Config::from_matches(&arg_matches)?;
        if config.opt_debug {
            eprintln!("{config:#?}");
        }
        Ok(config)
    }

    fn from_matches(matches: &ArgMatches) -> HttmResult<Self> {
        if matches.is_present("ZSH_HOT_KEYS") {
            install_hot_keys()?
        }

        let requested_utc_offset = if matches.is_present("UTC") {
            UtcOffset::UTC
        } else {
            // this fn is surprisingly finicky. it needs to be done
            // when program is not multithreaded, etc., so we don't even print an
            // error and we just default to UTC if something fails
            UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC)
        };

        let opt_json = matches.is_present("JSON");

        let mut print_mode = if matches.is_present("ZEROS") {
            PrintMode::RawZero
        } else if matches.is_present("RAW") {
            PrintMode::RawNewline
        } else if matches.is_present("NOT_SO_PRETTY") {
            PrintMode::FormattedNotPretty
        } else {
            PrintMode::FormattedDefault
        };

        let opt_bulk_exclusion = if matches.is_present("NO_LIVE") {
            Some(BulkExclusion::NoLive)
        } else if matches.is_present("NO_SNAP") {
            Some(BulkExclusion::NoSnap)
        } else {
            None
        };

        if let Some(BulkExclusion::NoSnap) = opt_bulk_exclusion {
            if let PrintMode::FormattedNotPretty | PrintMode::FormattedDefault = print_mode {
                return Err(HttmError::new(
                    "NO_SNAP is only available if RAW or ZEROS are specified.",
                )
                .into());
            }
        }

        // force a raw mode if one is not set for no_snap mode
        let opt_one_filesystem = matches.is_present("ONE_FILESYSTEM");
        let opt_recursive = matches.is_present("RECURSIVE");

        let opt_exact = matches.is_present("EXACT");
        let opt_no_filter = matches.is_present("NO_FILTER");
        let opt_debug = matches.is_present("DEBUG");
        let opt_no_hidden = matches.is_present("FILTER_HIDDEN");

        let opt_last_snap = match matches.value_of("LAST_SNAP") {
            Some("" | "any") => Some(LastSnapMode::Any),
            Some("none" | "without") => Some(LastSnapMode::Without),
            Some("ditto") => Some(LastSnapMode::DittoOnly),
            Some("no-ditto-inclusive") => Some(LastSnapMode::NoDittoInclusive),
            Some("no-ditto-exclusive" | "no-ditto") => Some(LastSnapMode::NoDittoExclusive),
            _ => None,
        };

        let opt_num_versions = match matches.value_of("NUM_VERSIONS") {
            Some("" | "all") => Some(NumVersionsMode::AllNumerals),
            Some("graph") => Some(NumVersionsMode::AllGraph),
            Some("single") => Some(NumVersionsMode::SingleAll),
            Some("single-no-snap") => Some(NumVersionsMode::SingleNoSnap),
            Some("single-with-snap") => Some(NumVersionsMode::SingleWithSnap),
            Some("multiple") => Some(NumVersionsMode::Multiple),
            _ => None,
        };

        let opt_mount_display = match matches.value_of("FILE_MOUNT") {
            Some("" | "target" | "directory") => Some(MountDisplay::Target),
            Some("source" | "device" | "dataset") => Some(MountDisplay::Source),
            Some("relative-path" | "relative" | "relpath") => Some(MountDisplay::RelativePath),
            _ => None,
        };

        let opt_preview = match matches.value_of("PREVIEW") {
            Some("" | "default") => Some("default".to_owned()),
            Some(user_defined) => Some(user_defined.to_owned()),
            None => None,
        };

        let mut opt_deleted_mode = match matches.value_of("DELETED") {
            Some("" | "all") => Some(DeletedMode::All),
            Some("single") => Some(DeletedMode::DepthOfOne),
            Some("only") => Some(DeletedMode::Only),
            _ => None,
        };

        let opt_interactive_mode = if matches.is_present("RESTORE") {
            match matches.value_of("RESTORE") {
                Some("guard") => Some(InteractiveMode::Restore(RestoreMode::Overwrite(
                    RestoreSnapGuard::Guarded,
                ))),
                Some("overwrite" | "yolo") => Some(InteractiveMode::Restore(
                    RestoreMode::Overwrite(RestoreSnapGuard::NotGuarded),
                )),
                Some("copy-and-preserve") => {
                    Some(InteractiveMode::Restore(RestoreMode::CopyAndPreserve))
                }
                Some(_) | None => Some(InteractiveMode::Restore(RestoreMode::CopyOnly)),
            }
        } else if matches.is_present("SELECT") {
            Some(InteractiveMode::Select)
        } else if matches.is_present("BROWSE") {
            Some(InteractiveMode::Browse)
        } else {
            None
        };

        let mut uniqueness = match matches.value_of("UNIQUENESS") {
            Some("all" | "no-filter") => ListSnapsOfType::All,
            Some("contents") => ListSnapsOfType::UniqueContents,
            Some("metadata" | _) | None => ListSnapsOfType::UniqueMetadata,
        };

        if opt_no_hidden && !opt_recursive && opt_interactive_mode.is_none() {
            return Err(HttmError::new(
                "FILTER_HIDDEN is only available if either an interactive mode or recursive mode is specified.",
            )
            .into());
        }

        if opt_preview.is_some()
            && matches!(opt_interactive_mode, Some(InteractiveMode::Browse) | None)
        {
            return Err(
                HttmError::new("PREVIEW is only available in Select or Restore modes").into(),
            );
        }

        // if in last snap and select mode we will want to return a raw value,
        // better to have this here.  It's more confusing if we work this logic later, I think.
        if opt_last_snap.is_some() && matches!(opt_interactive_mode, Some(InteractiveMode::Select))
        {
            print_mode = PrintMode::RawNewline
        }

        let opt_snap_file_mount =
            if let Some(requested_snapshot_suffix) = matches.value_of("SNAPSHOT") {
                if requested_snapshot_suffix == "httmSnapFileMount" {
                    Some(requested_snapshot_suffix.to_owned())
                } else if requested_snapshot_suffix.contains(char::is_whitespace) {
                    return Err(HttmError::new(
                        "httm will only accept snapshot suffixes which don't contain whitespace",
                    )
                    .into());
                } else {
                    Some(requested_snapshot_suffix.to_owned())
                }
            } else {
                None
            };

        let opt_snap_mode_filters = if matches.is_present("LIST_SNAPS") {
            // allow selection of snaps to prune in prune mode
            let select_mode = matches!(opt_interactive_mode, Some(InteractiveMode::Select));

            if !matches.is_present("PRUNE") && select_mode {
                eprintln!("Select mode for listed snapshots only available in PRUNE mode.")
            }

            // default to listing all snaps in list snaps mode if unset
            if !matches.is_present("UNIQUENESS") {
                uniqueness = ListSnapsOfType::All;
            }

            if let Some(values) = matches.value_of("LIST_SNAPS") {
                Some(Self::snap_filters(values, select_mode)?)
            } else {
                Some(ListSnapsFilters {
                    select_mode,
                    omit_num_snaps: 0usize,
                    name_filters: None,
                })
            }
        } else {
            None
        };

        let mut exec_mode = if let Some(full_snap_name) = matches.value_of("ROLL_FORWARD") {
            let progress_bar: ProgressBar = indicatif::ProgressBar::new_spinner();
            let roll_config: RollForwardConfig = RollForwardConfig {
                full_snap_name: full_snap_name.to_string(),
                progress_bar,
            };

            ExecMode::RollForward(roll_config)
        } else if let Some(num_versions_mode) = opt_num_versions {
            ExecMode::NumVersions(num_versions_mode)
        } else if let Some(mount_display) = opt_mount_display {
            ExecMode::MountsForFiles(mount_display)
        } else if matches.is_present("PRUNE") {
            ExecMode::Prune(opt_snap_mode_filters)
        } else if opt_snap_mode_filters.is_some() {
            ExecMode::SnapsForFiles(opt_snap_mode_filters)
        } else if let Some(requested_snapshot_suffix) = opt_snap_file_mount {
            ExecMode::SnapFileMount(requested_snapshot_suffix)
        } else if let Some(interactive_mode) = opt_interactive_mode {
            ExecMode::Interactive(interactive_mode)
        } else if opt_deleted_mode.is_some() {
            let progress_bar: ProgressBar = indicatif::ProgressBar::new_spinner();
            ExecMode::NonInteractiveRecursive(progress_bar)
        } else {
            ExecMode::Display
        };

        if opt_recursive {
            if matches!(exec_mode, ExecMode::Display) {
                return Err(HttmError::new("RECURSIVE not available in Display Mode.").into());
            }
        } else if opt_no_filter {
            return Err(HttmError::new(
                "NO_FILTER only available when recursive search is enabled.",
            )
            .into());
        }

        // current working directory will be helpful in a number of places
        let pwd = Self::pwd()?;

        // paths are immediately converted to our PathData struct
        let paths: Vec<PathData> =
            Self::paths(matches.values_of_os("INPUT_FILES"), &exec_mode, &pwd)?;

        // for exec_modes in which we can only take a single directory, process how we handle those here
        let opt_requested_dir: Option<PathData> =
            Self::opt_requested_dir(&mut exec_mode, &mut opt_deleted_mode, &paths, &pwd)?;

        if opt_one_filesystem && opt_requested_dir.is_none() {
            return Err(HttmError::new(
                "ONE_FILESYSTEM requires a requested path for RECURSIVE search",
            )
            .into());
        }

        // doesn't make sense to follow symlinks when you're searching the whole system,
        // so we disable our bespoke "when to traverse symlinks" algo here, or if requested.
        let opt_no_traverse = matches.is_present("NO_TRAVERSE") || {
            if let Some(user_requested_dir) = opt_requested_dir.as_ref() {
                user_requested_dir.path_buf.as_path() == Path::new(ROOT_DIRECTORY)
            } else {
                false
            }
        };

        if !matches!(opt_deleted_mode, None | Some(DeletedMode::All)) && !opt_recursive {
            return Err(HttmError::new(
                "Deleted modes other than \"all\" require recursive mode is enabled.  Quitting.",
            )
            .into());
        }

        let opt_omit_ditto = matches.is_present("OMIT_DITTO");

        // opt_omit_identical doesn't make sense in Display Recursive mode as no live files will exists?
        if opt_omit_ditto && matches!(exec_mode, ExecMode::NonInteractiveRecursive(_)) {
            return Err(HttmError::new(
                "OMIT_DITTO not available when a deleted recursive search is specified.  Quitting.",
            )
            .into());
        }

        if opt_last_snap.is_some() && matches!(exec_mode, ExecMode::NonInteractiveRecursive(_)) {
            return Err(
                HttmError::new("LAST_SNAP is not available in Display Recursive Mode.").into(),
            );
        }

        // obtain a map of datasets, a map of snapshot directories, and possibly a map of
        // alternate filesystems and map of aliases if the user requests
        let dataset_collection = FilesystemInfo::new(
            matches.is_present("ALT_REPLICATED"),
            matches.value_of_os("REMOTE_DIR"),
            matches.value_of_os("LOCAL_DIR"),
            matches.values_of_os("MAP_ALIASES"),
            &pwd,
        )?;

        let config = Config {
            paths,
            opt_bulk_exclusion,
            opt_recursive,
            opt_exact,
            opt_no_filter,
            opt_debug,
            opt_no_traverse,
            opt_omit_ditto,
            opt_no_hidden,
            opt_last_snap,
            opt_preview,
            opt_json,
            opt_one_filesystem,
            uniqueness,
            requested_utc_offset,
            exec_mode,
            print_mode,
            opt_deleted_mode,
            dataset_collection,
            pwd,
            opt_requested_dir,
        };

        Ok(config)
    }

    pub fn pwd() -> HttmResult<PathData> {
        if let Ok(pwd) = std::env::current_dir() {
            Ok(PathData::from(pwd))
        } else {
            Err(HttmError::new(
                "Working directory does not exist or your do not have permissions to access it.",
            )
            .into())
        }
    }

    pub fn paths(
        opt_os_values: Option<OsValues>,
        exec_mode: &ExecMode,
        pwd: &PathData,
    ) -> HttmResult<Vec<PathData>> {
        let mut paths = if let Some(input_files) = opt_os_values {
            input_files
                .par_bridge()
                // canonicalize() on a deleted relative path will not exist,
                // so we have to join with the pwd to make a path that
                // will exist on a snapshot
                .map(PathData::from)
                .collect()
        } else {
            match exec_mode {
                // setting pwd as the path, here, keeps us from waiting on stdin when in certain modes
                //  is more like Interactive and NonInteractiveRecursive in this respect in requiring only one
                // input, and waiting on one input from stdin is pretty silly
                ExecMode::Interactive(_)
                | ExecMode::NonInteractiveRecursive(_)
                | ExecMode::RollForward(_) => {
                    vec![pwd.clone()]
                }
                ExecMode::Display
                | ExecMode::SnapFileMount(_)
                | ExecMode::Prune(_)
                | ExecMode::MountsForFiles(_)
                | ExecMode::SnapsForFiles(_)
                | ExecMode::NumVersions(_) => read_stdin()?,
            }
        };

        // deduplicate pathdata and sort if in display mode --
        // so input of ./.z* and ./.zshrc will only print ./.zshrc once
        paths = if paths.len() > 1 {
            paths.sort_unstable();
            // dedup needs to be sorted/ordered first to work (not like a BTreeMap)
            paths.dedup();

            paths
        } else {
            paths
        };

        Ok(paths)
    }

    pub fn opt_requested_dir(
        exec_mode: &mut ExecMode,
        deleted_mode: &mut Option<DeletedMode>,
        paths: &[PathData],
        pwd: &PathData,
    ) -> HttmResult<Option<PathData>> {
        let res = match exec_mode {
            ExecMode::Interactive(_) | ExecMode::NonInteractiveRecursive(_) => {
                match paths.len() {
                    0 => Some(pwd.clone()),
                    // use our bespoke is_dir fn for determining whether a dir here see pub httm_is_dir
                    // safe to index as we know the paths len is 1
                    1 if paths[0].httm_is_dir() => Some(paths[0].clone()),
                    // handle non-directories
                    1 => {
                        match exec_mode {
                            ExecMode::Interactive(ref interactive_mode) => {
                                match interactive_mode {
                                    InteractiveMode::Browse => {
                                        // doesn't make sense to have a non-dir in these modes
                                        return Err(HttmError::new(
                                                    "Path specified is not a directory, and therefore not suitable for browsing.",
                                                )
                                                .into());
                                    }
                                    InteractiveMode::Restore(_) | InteractiveMode::Select => {
                                        // non-dir file will just cause us to skip the lookup phase
                                        None
                                    }
                                }
                            }
                            // silently disable NonInteractiveRecursive when path given is not a directory
                            // switch to a standard Display mode
                            ExecMode::NonInteractiveRecursive(_) => {
                                *exec_mode = ExecMode::Display;
                                *deleted_mode = None;
                                None
                            }
                            _ => unreachable!(),
                        }
                    }
                    n if n > 1 => return Err(HttmError::new(
                        "May only specify one path in the display recursive or interactive modes.",
                    )
                    .into()),
                    _ => {
                        unreachable!()
                    }
                }
            }

            ExecMode::Display
            | ExecMode::RollForward(_)
            | ExecMode::SnapFileMount(_)
            | ExecMode::Prune(_)
            | ExecMode::MountsForFiles(_)
            | ExecMode::SnapsForFiles(_)
            | ExecMode::NumVersions(_) => {
                // in non-interactive mode / display mode, requested dir is just a file
                // like every other file and pwd must be the requested working dir.
                None
            }
        };
        Ok(res)
    }

    pub fn snap_filters(values: &str, select_mode: bool) -> HttmResult<ListSnapsFilters> {
        let mut raw = values.trim_end().split(',');

        let omit_num_snaps = if let Some(value) = raw.next() {
            if let Ok(number) = value.parse::<usize>() {
                number
            } else {
                return Err(HttmError::new("Invalid max snaps given. Quitting.").into());
            }
        } else {
            0usize
        };

        let rest: Vec<&str> = raw.collect();

        let name_filters = if !rest.is_empty() {
            if rest.len() == 1usize && rest.index(0) == &"none" {
                None
            } else if rest.len() == 1usize && rest.index(0) == &"native" {
                Some(vec![
                    "ounceSnapFileMount".to_owned(),
                    "httmSnapFileMount".to_owned(),
                ])
            } else {
                Some(rest.iter().map(|item| (*item).to_string()).collect())
            }
        } else {
            None
        };

        Ok(ListSnapsFilters {
            select_mode,
            omit_num_snaps,
            name_filters,
        })
    }

    // use an associated function here because we may need this display again elsewhere
    pub fn generate_display_config(&self, paths_selected: &[PathData]) -> Self {
        // generate a config for a preview display only
        Config {
            paths: paths_selected.to_vec(),
            opt_recursive: false,
            opt_exact: false,
            opt_no_filter: false,
            opt_debug: false,
            opt_no_traverse: false,
            opt_no_hidden: false,
            opt_json: false,
            opt_one_filesystem: false,
            opt_bulk_exclusion: None,
            opt_last_snap: None,
            opt_preview: None,
            opt_deleted_mode: None,
            uniqueness: ListSnapsOfType::UniqueMetadata,
            opt_omit_ditto: self.opt_omit_ditto,
            requested_utc_offset: self.requested_utc_offset,
            exec_mode: ExecMode::Display,
            print_mode: PrintMode::FormattedDefault,
            dataset_collection: self.dataset_collection.clone(),
            pwd: self.pwd.clone(),
            opt_requested_dir: self.opt_requested_dir.clone(),
        }
    }
}
