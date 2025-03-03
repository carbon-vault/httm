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

use std::collections::BTreeMap;
use std::ops::Deref;

use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

use crate::config::generate::MountDisplay;
use crate::config::generate::{ExecMode, PrintMode};
use crate::display_versions::format::NOT_SO_PRETTY_FIXED_WIDTH_PADDING;
use crate::display_versions::format::QUOTATION_MARKS_LEN;
use crate::library::utility::delimiter;
use crate::SnapNameMap;
use crate::VersionsMap;
use crate::{MountsForFiles, GLOBAL_CONFIG};

#[derive(Debug)]
pub struct PrintAsMap {
    inner: BTreeMap<String, Vec<String>>,
}

impl Deref for PrintAsMap {
    type Target = BTreeMap<String, Vec<String>>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl From<BTreeMap<String, Vec<String>>> for PrintAsMap {
    fn from(map: BTreeMap<String, Vec<String>>) -> Self {
        Self { inner: map }
    }
}

impl Serialize for PrintAsMap {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("PrintAsMap", 1)?;

        state.serialize_field("inner", &self)?;
        state.end()
    }
}

impl<'a> From<&MountsForFiles<'a>> for PrintAsMap {
    fn from(mounts_for_files: &MountsForFiles) -> Self {
        let inner = mounts_for_files
            .iter()
            .map(|(key, values)| {
                let res = values
                    .iter()
                    .filter_map(|value| match mounts_for_files.mount_display() {
                        MountDisplay::Target => Some(value.path_buf.to_string_lossy()),
                        MountDisplay::Source => GLOBAL_CONFIG
                            .dataset_collection
                            .map_of_datasets
                            .get(&value.path_buf)
                            .map(|md| md.source.to_string_lossy()),
                        MountDisplay::RelativePath => key
                            .relative_path(value.path_buf.as_path())
                            .ok()
                            .map(|path| path.to_string_lossy()),
                    })
                    .map(|s| s.to_string())
                    .collect();
                (key.path_buf.to_string_lossy().to_string(), res)
            })
            .collect();
        Self { inner }
    }
}

impl From<&VersionsMap> for PrintAsMap {
    fn from(map: &VersionsMap) -> Self {
        let inner = map
            .iter()
            .map(|(key, values)| {
                let res = values
                    .iter()
                    .map(|value| value.path_buf.to_string_lossy().to_string())
                    .collect();
                (key.path_buf.to_string_lossy().to_string(), res)
            })
            .collect();
        Self { inner }
    }
}

impl From<&SnapNameMap> for PrintAsMap {
    fn from(map: &SnapNameMap) -> Self {
        let inner = map
            .iter()
            .map(|(key, value)| (key.path_buf.to_string_lossy().to_string(), value.clone()))
            .collect();
        Self { inner }
    }
}

impl std::string::ToString for PrintAsMap {
    fn to_string(&self) -> String {
        if GLOBAL_CONFIG.opt_json {
            let json_string = self.to_json();

            let res = match &GLOBAL_CONFIG.exec_mode {
                ExecMode::Display | ExecMode::Interactive(_) => {
                    json_string.replace("\"inner\": ", "\"versions\": ")
                }
                ExecMode::MountsForFiles(_) => json_string.replace("\"inner\": ", "\"mounts\": "),
                ExecMode::SnapsForFiles(_) => {
                    json_string.replace("\"inner\": ", "\"snapshot_names\": ")
                }
                ExecMode::NonInteractiveRecursive(_)
                | ExecMode::RollForward(_)
                | ExecMode::NumVersions(_)
                | ExecMode::Prune(_)
                | ExecMode::SnapFileMount(_) => {
                    unreachable!(
                        "JSON print should not be available in the selected {:?} execution mode.",
                        &GLOBAL_CONFIG.exec_mode
                    );
                }
            };

            return res;
        }

        match &GLOBAL_CONFIG.print_mode {
            PrintMode::RawNewline | PrintMode::RawZero => self
                .values()
                .flatten()
                .map(|value| {
                    let delimiter = delimiter();
                    format!("{value}{delimiter}")
                })
                .collect::<String>(),
            PrintMode::FormattedDefault | PrintMode::FormattedNotPretty => self.format(),
        }
    }
}

impl PrintAsMap {
    pub fn map_padding(&self) -> usize {
        self.keys().max_by_key(|key| key.len()).map_or_else(
            || QUOTATION_MARKS_LEN,
            |key| key.len() + QUOTATION_MARKS_LEN,
        )
    }

    pub fn to_json(&self) -> String {
        let res = match GLOBAL_CONFIG.print_mode {
            PrintMode::FormattedNotPretty | PrintMode::RawNewline | PrintMode::RawZero => {
                serde_json::to_string(&self)
            }
            PrintMode::FormattedDefault => serde_json::to_string_pretty(&self),
        };

        match res {
            Ok(s) => {
                let delimiter = delimiter();
                format!("{s}{delimiter}")
            }
            Err(error) => {
                eprintln!("Error: {error}");
                std::process::exit(1)
            }
        }
    }

    pub fn format(&self) -> String {
        let padding = self.map_padding();

        let write_out_buffer = self
            .iter()
            .filter(|(_key, values)| {
                if GLOBAL_CONFIG.opt_last_snap.is_some() {
                    !values.is_empty()
                } else {
                    true
                }
            })
            .map(|(key, values)| {
                let display_path =
                    if matches!(&GLOBAL_CONFIG.print_mode, PrintMode::FormattedNotPretty) {
                        key.clone()
                    } else {
                        format!("\"{key}\"")
                    };

                let values_string: String = values
                    .iter()
                    .enumerate()
                    .map(|(idx, value)| {
                        if matches!(&GLOBAL_CONFIG.print_mode, PrintMode::FormattedNotPretty) {
                            format!("{NOT_SO_PRETTY_FIXED_WIDTH_PADDING}{value}")
                        } else if idx == 0 {
                            format!(
                                "{:<width$} : \"{}\"\n",
                                display_path,
                                value,
                                width = padding
                            )
                        } else {
                            format!("{:<padding$} : \"{value}\"\n", "")
                        }
                    })
                    .collect::<String>();

                if matches!(&GLOBAL_CONFIG.print_mode, PrintMode::FormattedNotPretty) {
                    format!("{display_path}:{values_string}\n")
                } else {
                    values_string
                }
            })
            .collect();

        write_out_buffer
    }
}
