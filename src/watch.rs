use anyhow::{Context, Result};
use inotify::{Inotify, WatchDescriptor, WatchMask};
use log::debug;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Compute the inotify mask used for every watched directory.
pub(crate) fn watch_mask() -> WatchMask {
    WatchMask::CREATE | WatchMask::MOVED_TO | WatchMask::DELETE_SELF | WatchMask::ONLYDIR
}

/// Register a directory with inotify if it has not already been registered.
pub(crate) fn add_watch(
    watcher: &mut Inotify,
    registry: &mut WatchRegistry,
    path: &Path,
) -> Result<()> {
    if registry.contains_path(path) {
        return Ok(());
    }

    let descriptor = watcher
        .watches()
        .add(path, watch_mask())
        .with_context(|| format!("Failed to add watch for {}", path.display()))?;

    registry.insert(path.to_path_buf(), descriptor);
    debug!("Watching {}", path.display());
    Ok(())
}

/// Bookkeeping for inotify watch descriptors and their corresponding paths.
#[derive(Default)]
pub(crate) struct WatchRegistry {
    by_descriptor: HashMap<WatchDescriptor, PathBuf>,
    by_path: HashMap<PathBuf, WatchDescriptor>,
}

impl WatchRegistry {
    pub(crate) fn insert(&mut self, path: PathBuf, descriptor: WatchDescriptor) {
        self.by_path.insert(path.clone(), descriptor.clone());
        self.by_descriptor.insert(descriptor, path);
    }

    pub(crate) fn remove_by_descriptor(&mut self, descriptor: &WatchDescriptor) {
        if let Some(path) = self.by_descriptor.remove(descriptor) {
            debug!("Removing watch for {}", path.display());
            self.by_path.remove(&path);
        }
    }

    pub(crate) fn path_for(&self, descriptor: &WatchDescriptor) -> Option<&PathBuf> {
        self.by_descriptor.get(descriptor)
    }

    pub(crate) fn contains_path(&self, path: &Path) -> bool {
        self.by_path.contains_key(path)
    }
}
