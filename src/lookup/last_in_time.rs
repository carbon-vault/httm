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

use std::{collections::BTreeMap, ops::Deref, path::PathBuf, time::SystemTime};

use crate::config::generate::Config;
use crate::data::paths::PathData;
use crate::lookup::versions::{MostProximateAndOptAlts, RelativePathAndSnapMounts};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastInTimeSet {
    inner: Vec<PathBuf>,
}

impl Deref for LastInTimeSet {
    type Target = Vec<PathBuf>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl LastInTimeSet {
    // this is very similar to DisplayMap, but of course returns only last in time
    // it is also missing parallel iter functions, to make the searches more responsive
    // by leaving parallel search for the interactive views
    pub fn new(config: &Config, path_set: &[PathData]) -> Self {
        // create vec of all local and replicated backups at once
        let snaps_selected_for_search = config
            .dataset_collection
            .snaps_selected_for_search
            .get_value();

        let inner: Vec<PathBuf> = path_set
            .iter()
            .filter_map(|pathdata| {
                snaps_selected_for_search
                    .iter()
                    .flat_map(|dataset_type| {
                        MostProximateAndOptAlts::new(config, pathdata, dataset_type)
                    })
                    .flat_map(|dataset_for_search| {
                        dataset_for_search.get_search_bundles(config, pathdata)
                    })
                    .flatten()
                    .flat_map(|search_bundle| Self::get_last_version(&search_bundle))
                    .filter(|pathdata| pathdata.metadata.is_some())
                    .max_by_key(|pathdata| pathdata.metadata.unwrap().modify_time)
                    .map(|pathdata| pathdata.path_buf)
            })
            .collect();

        Self { inner }
    }

    fn get_last_version(search_bundle: &RelativePathAndSnapMounts) -> Option<PathData> {
        // get the DirEntry for our snapshot path which will have all our possible
        // snapshots, like so: .zfs/snapshots/<some snap name>/
        //
        // BTreeMap will then remove duplicates with the same system modify time and size/file len
        let unique_versions: BTreeMap<(SystemTime, u64), PathData> = search_bundle
            .snap_mounts
            .iter()
            .map(|path| path.join(&search_bundle.relative_path))
            .map(|joined_path| PathData::from(joined_path.as_path()))
            .filter_map(|pathdata| {
                pathdata
                    .metadata
                    .map(|metadata| ((metadata.modify_time, metadata.size), pathdata))
            })
            .collect();

        let sorted_versions: Vec<PathData> = unique_versions.into_values().collect();

        let res: Option<PathData> = sorted_versions.last().cloned();

        res
    }
}
