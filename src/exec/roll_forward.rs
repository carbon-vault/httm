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

use std::cmp::Ordering;
use std::fs::read_dir;
use std::io::BufRead;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::process::Command as ExecProcess;
use std::process::Stdio;

use hashbrown::HashMap;
use nu_ansi_term::Color::{Blue, Red, Yellow};
use once_cell::sync::OnceCell;
use rayon::prelude::*;
use which::which;

use crate::config::generate::RollForwardConfig;
use crate::data::paths::BasicDirEntryInfo;
use crate::data::paths::PathData;
use crate::library::iter_extensions::HttmIter;
use crate::library::results::{HttmError, HttmResult};
use crate::library::snap_guard::{PrecautionarySnapType, SnapGuard};
use crate::library::utility::{copy_direct, remove_recursive};
use crate::library::utility::{is_metadata_different, user_has_effective_root};
use crate::{GLOBAL_CONFIG, ZFS_SNAPSHOT_DIRECTORY};

static ROLL_FORWARD_PROXIMATE_DATASET: OnceCell<PathBuf> = OnceCell::new();

#[derive(Debug, Clone)]
struct DiffEvent {
    pathdata: PathData,
    diff_type: DiffType,
    time: DiffTime,
}

impl DiffEvent {
    fn new(path_string: &str, diff_type: DiffType, time_str: &str) -> Self {
        Self {
            pathdata: PathData::from(Path::new(path_string)),
            diff_type,
            time: DiffTime::new(time_str).expect("Could not parse a zfs diff time value."),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffTime {
    secs: u64,
    nanos: u64,
}

impl DiffTime {
    fn new(time_str: &str) -> Option<Self> {
        let (secs, nanos) = time_str.split_once('.')?;

        let time = DiffTime {
            secs: secs.parse::<u64>().ok()?,
            nanos: nanos.parse::<u64>().ok()?,
        };

        Some(time)
    }
}

impl std::cmp::Ord for DiffTime {
    #[inline]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let secs_ordering = self.secs.cmp(&other.secs);

        if secs_ordering.is_eq() {
            return self.nanos.cmp(&other.nanos);
        }

        secs_ordering
    }
}

impl std::cmp::PartialOrd for DiffTime {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone)]
enum DiffType {
    Removed,
    Created,
    Modified,
    // zfs diff semantics are: old file name -> new file name
    // old file name will be the key, and new file name will be stored in the value
    Renamed(PathBuf),
}

pub struct RollForward;

impl RollForward {
    pub fn exec(roll_config: &RollForwardConfig) -> HttmResult<()> {
        user_has_effective_root()?;

        let (dataset_name, snap_name) = if let Some(res) =
            roll_config.full_snap_name.split_once('@')
        {
            res
        } else {
            let msg = format!("{} is not a valid data set name.  A valid ZFS snapshot name requires a '@' separating dataset name and snapshot name.", roll_config.full_snap_name);
            return Err(HttmError::new(&msg).into());
        };

        ROLL_FORWARD_PROXIMATE_DATASET
            .get_or_init(|| Self::proximate_dataset_mount(dataset_name).unwrap());

        let snap_guard: SnapGuard =
            SnapGuard::new(dataset_name, PrecautionarySnapType::PreRollForward)?;

        match Self::roll_forward(roll_config, snap_name) {
            Ok(_) => {
                println!("httm roll forward completed successfully.");
            }
            Err(err) => {
                let msg = format!(
                    "httm roll forward failed for the following reason: {}.\n\
                Attempting roll back to precautionary pre-execution snapshot.",
                    err
                );
                eprintln!("{}", msg);

                snap_guard
                    .rollback()
                    .map(|_| println!("Rollback succeeded."))?;

                std::process::exit(1)
            }
        };

        SnapGuard::new(
            dataset_name,
            PrecautionarySnapType::PostRollForward(snap_name.to_owned()),
        )
        .map(|_res| ())
    }

    fn proximate_dataset_mount(dataset_name: &str) -> Option<PathBuf> {
        GLOBAL_CONFIG
            .dataset_collection
            .map_of_datasets
            .iter()
            .find(|(_mount, md)| md.source == PathBuf::from(dataset_name))
            .map(|(mount, _)| mount.to_owned())
    }

    fn exec_diff(full_snapshot_name: &str) -> HttmResult<Child> {
        let zfs_command = which("zfs").map_err(|_err| {
            HttmError::new("'zfs' command not found. Make sure the command 'zfs' is in your path.")
        })?;

        // -H: tab separated, -t: Specify time, -h: Normalize paths (don't use escape codes)
        let process_args = vec!["diff", "-H", "-t", "-h", full_snapshot_name];

        let process_handle = ExecProcess::new(zfs_command)
            .args(&process_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        Ok(process_handle)
    }

    fn ingest(process_handle: &mut Child) -> HttmResult<impl Iterator<Item = DiffEvent> + '_> {
        const IN_BUFFER_SIZE: usize = 65_536;

        if let Some(output) = process_handle.stdout.take() {
            let stdout_buffer = std::io::BufReader::with_capacity(IN_BUFFER_SIZE, output);

            let ret = stdout_buffer
                .lines()
                .map(|line| line.expect("Could not obtain line from string."))
                .filter_map(move |line| {
                    let split_line: Vec<&str> = line.split('\t').collect();

                    let time_str = split_line
                        .first()
                        .expect("Could not obtain a timestamp for diff event.");

                    match split_line.get(1) {
                        Some(event) if event == &"-" => split_line.get(2).map(|path_string| {
                            DiffEvent::new(path_string, DiffType::Removed, time_str)
                        }),
                        Some(event) if event == &"+" => split_line.get(2).map(|path_string| {
                            DiffEvent::new(path_string, DiffType::Created, time_str)
                        }),
                        Some(event) if event == &"M" => split_line.get(2).map(|path_string| {
                            DiffEvent::new(path_string, DiffType::Modified, time_str)
                        }),
                        Some(event) if event == &"R" => split_line.get(2).map(|path_string| {
                            let new_file_name =
                                PathBuf::from(split_line.get(3).expect(
                                    "diff of type rename did not contain a new name value",
                                ));
                            DiffEvent::new(path_string, DiffType::Renamed(new_file_name), time_str)
                        }),
                        _ => panic!("Could not parse diff event."),
                    }
                });

            Ok(ret)
        } else {
            Err(HttmError::new("'zfs diff' reported no changes to dataset").into())
        }
    }

    fn roll_forward(roll_config: &RollForwardConfig, snap_name: &str) -> HttmResult<()> {
        let snap_name_clone = snap_name.to_string();

        let hard_link_handle = std::thread::spawn(|| HardLinkMap::new(snap_name_clone));

        let mut process_handle = Self::exec_diff(&roll_config.full_snap_name)?;

        let stream = Self::ingest(&mut process_handle)?;

        let mut iter_peekable = stream.peekable();

        if iter_peekable.peek().is_none() {
            return Err(HttmError::new("'zfs diff' reported no changes to dataset").into());
        }

        // zfs-diff can return multiple file actions for a single inode, here we dedup
        eprintln!("Building a map of ZFS filesystem events since the specified snapshot:");
        let mut group_map: Vec<(PathBuf, Vec<DiffEvent>)> = iter_peekable
            .map(|event| {
                roll_config.progress_bar.tick();
                event
            })
            .into_group_map_by(|event| event.pathdata.path_buf.clone())
            .into_iter()
            .collect();

        // now sort by number of components, want to build from the bottom up, do less dir creation, etc.
        group_map.sort_by_key(|(key, _values)| key.components().count());

        // into iter and reverse because we want to go smallest first
        group_map
            .into_par_iter()
            .flat_map(|(_key, mut values)| {
                values.sort_by_key(|event| event.time.clone());
                values.pop()
            })
            .map(|event| {
                let proximate_dataset_mount = ROLL_FORWARD_PROXIMATE_DATASET.get().expect("Unable to determine proximate dataset mount, which should be available at this point in execution.");

                let snap_file_path =
                    Self::snap_path(&event.pathdata, snap_name, proximate_dataset_mount)
                        .expect("Could not obtain snap file path for live version.");

                (event, snap_file_path)
            })
            .try_for_each(|(event, snap_file_path)| Self::diff_action(&event, &snap_file_path))?;

        let mut map = hard_link_handle
            .join()
            .map_err(|_err| HttmError::new("Thread panicked!"))??;

        PreserveHardLinks::exec(&mut map)
    }

    fn snap_path(
        event_pathdata: &PathData,
        snap_name: &str,
        proximate_dataset_mount: &Path,
    ) -> Option<PathBuf> {
        event_pathdata
            .relative_path(proximate_dataset_mount)
            .ok()
            .map(|relative_path| {
                let snap_file_path: PathBuf = [
                    proximate_dataset_mount,
                    Path::new(ZFS_SNAPSHOT_DIRECTORY),
                    Path::new(&snap_name),
                    relative_path,
                ]
                .iter()
                .collect();

                snap_file_path
            })
    }

    fn diff_action(event: &DiffEvent, snap_file_path: &Path) -> HttmResult<()> {
        let snap_file = PathData::from(snap_file_path);

        // zfs-diff can return multiple file actions for a single inode
        // since we exclude older file actions, if rename or created is the last action,
        // we should make sure it has the latest data, so a simple rename is not enough
        // this is internal to the fn Self::remove()
        match &event.diff_type {
            DiffType::Removed | DiffType::Modified => {
                Self::copy(&snap_file.path_buf, &event.pathdata.path_buf)
            }
            DiffType::Created => {
                Self::overwrite_or_remove(&snap_file.path_buf, &event.pathdata.path_buf)
            }
            DiffType::Renamed(new_file_name) => {
                Self::overwrite_or_remove(&snap_file.path_buf, new_file_name)
            }
        }
    }

    fn copy(src: &Path, dst: &Path) -> HttmResult<()> {
        if let Err(err) = copy_direct(src, dst, true) {
            eprintln!("Error: {}", err);
            let msg = format!(
                "Could not overwrite {:?} with snapshot file version {:?}",
                dst, src
            );
            return Err(HttmError::new(&msg).into());
        }

        is_metadata_different(src, dst)?;
        eprintln!("{}: {:?} -> {:?}", Blue.paint("Restored "), src, dst);
        Ok(())
    }

    fn overwrite_or_remove(src: &Path, dst: &Path) -> HttmResult<()> {
        // overwrite
        if src.exists() {
            return Self::copy(src, dst);
        }

        // or remove
        match remove_recursive(dst) {
            Ok(_) => {
                if dst.exists() {
                    let msg = format!("File should not exist after deletion {:?}", dst);
                    return Err(HttmError::new(&msg).into());
                }
            }
            Err(err) => {
                eprintln!("Error: {}", err);
                let msg = format!("Could not delete file {:?}", dst);
                return Err(HttmError::new(&msg).into());
            }
        }

        eprintln!("{}: {:?} -> 🗑️", Red.paint("Removed  "), dst);

        Ok(())
    }
}

// key: inode, values: Paths
struct HardLinkMap {
    snap_name: String,
    inner: HashMap<InodeAndNumLinks, Vec<PathBuf>>,
}

#[derive(Eq, PartialEq, Hash)]
struct InodeAndNumLinks {
    ino: u64,
    nlink: u64,
}

impl HardLinkMap {
    fn new(snap_name: String) -> HttmResult<Self> {
        let snap_dataset = Self::snap_dataset(&snap_name).ok_or(HttmError::new(
            "Unable to determine snapshot dataset mount.",
        ))?;

        let constructed = BasicDirEntryInfo {
            path: snap_dataset,
            file_type: None,
        };

        let mut queue: Vec<BasicDirEntryInfo> = vec![constructed];
        let mut inner: HashMap<InodeAndNumLinks, Vec<PathBuf>> = HashMap::new();

        // condition kills iter when user has made a selection
        // pop_back makes this a LIFO queue which is supposedly better for caches
        while let Some(item) = queue.pop() {
            // no errors will be propagated in recursive mode
            // far too likely to run into a dir we don't have permissions to view
            let (vec_dirs, vec_files): (Vec<BasicDirEntryInfo>, Vec<BasicDirEntryInfo>) =
                read_dir(item.path)?
                    .flatten()
                    // checking file_type on dir entries is always preferable
                    // as it is much faster than a metadata call on the path
                    .map(|dir_entry| BasicDirEntryInfo::from(&dir_entry))
                    .partition(|dir_entry| dir_entry.path.is_dir());

            let mut combined = vec_files;
            combined.extend_from_slice(&vec_dirs);
            queue.extend_from_slice(&vec_dirs);

            combined
                .into_iter()
                .map(|entry| {
                    let md = entry
                        .path
                        .metadata()
                        .ok()
                        .expect("Could not obtain metadata");

                    (entry.path, md)
                })
                .filter_map(|(path, md)| {
                    let nlink = md.nlink();

                    // filter files without multiple hard links
                    if nlink <= 1 {
                        return None;
                    }

                    let key = InodeAndNumLinks {
                        ino: md.ino(),
                        nlink,
                    };

                    Some((path, key))
                })
                .for_each(|(path, key)| match inner.get_mut(&key) {
                    Some(values) => values.push(path),
                    None => {
                        let _ = inner.insert_unique_unchecked(key, vec![path]);
                    }
                });
        }

        Ok(Self {
            snap_name: snap_name.to_string(),
            inner,
        })
    }

    fn snap_dataset(snap_name: &str) -> Option<PathBuf> {
        ROLL_FORWARD_PROXIMATE_DATASET
            .get()
            .map(|proximate_dataset_mount| {
                let snap_dataset_mount: PathBuf = [
                    proximate_dataset_mount,
                    Path::new(ZFS_SNAPSHOT_DIRECTORY),
                    Path::new(&snap_name),
                ]
                .iter()
                .collect();

                snap_dataset_mount
            })
    }
}

struct PreserveHardLinks;

impl PreserveHardLinks {
    fn exec(map: &mut HardLinkMap) -> HttmResult<()> {
        let mut none_preserved = true;

        let res = map.inner.iter_mut().try_for_each(|(_key, values)| {
            let live_paths: Vec<PathBuf> = values
                .iter()
                .map(|snap_path| {
                    let live_path = Self::live_path(
                        snap_path,
                        &map.snap_name,
                        ROLL_FORWARD_PROXIMATE_DATASET
                            .get()
                            .expect("Unable to determine proximate dataset mount, which should be available at this point in execution."),
                    ).expect("Could obtain live path for");

                    live_path
                })
                .collect();

            live_paths.iter().filter(|live_path| !live_path.exists()).try_for_each(|live_path| {
                    // I'm not sure this is necessary, but here we don't just find an existing path.
                    // We find the oldest created path and assume this is our "original" path
                    let opt_original = live_paths
                        .iter()
                        .filter(|path| path.exists())
                        .max_by(|a, b| {
                            a.metadata().ok().and_then(|md| md.created().ok()).cmp(&b.metadata().ok().and_then(|md| md.created().ok()))
                        });

                    match opt_original {
                        Some(original) => {
                            none_preserved = false;
                            return Self::hard_link(original, live_path)
                        },
                        None => {
                            return Err(HttmError::new(
                                "Unable to find live path to use as link source.",
                            )
                            .into())
                        }
                    }
            })
        });

        if none_preserved {
            println!("No hard links found which require preservation.");
            return Ok(());
        }

        res
    }

    fn live_path(
        snap_path: &Path,
        snap_name: &str,
        proximate_dataset_mount: &Path,
    ) -> Option<PathBuf> {
        snap_path
            .strip_prefix(proximate_dataset_mount)
            .ok()
            .and_then(|path| path.strip_prefix(ZFS_SNAPSHOT_DIRECTORY).ok())
            .and_then(|path| path.strip_prefix(snap_name).ok())
            .map(|relative_path| {
                [proximate_dataset_mount, relative_path]
                    .into_iter()
                    .collect()
            })
    }

    fn hard_link(original: &Path, link: &Path) -> HttmResult<()> {
        match std::fs::hard_link(original, link) {
            Ok(_) => {
                if !link.exists() {
                    let msg = format!("Target link should exist after linking {:?}", link);
                    return Err(HttmError::new(&msg).into());
                }
            }
            Err(err) => {
                eprintln!("Error: {}", err);
                let msg = format!("Could not link file {:?}", link);
                return Err(HttmError::new(&msg).into());
            }
        }

        eprintln!("{}: {:?} -> {:?}", Yellow.paint("Linked  "), original, link);

        Ok(())
    }
}
