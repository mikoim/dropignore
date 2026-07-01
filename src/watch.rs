use anyhow::Result;
use inotify::{Inotify, WatchDescriptor, WatchMask};
use log::debug;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Compute the inotify mask used for every watched directory.
pub(crate) fn watch_mask() -> WatchMask {
    WatchMask::CREATE
        | WatchMask::MOVED_TO
        | WatchMask::DELETE_SELF
        | WatchMask::MOVE_SELF
        | WatchMask::ONLYDIR
}

/// Build the error context for a failed `add_watch`. When the kernel reports
/// ENOSPC the real cause is usually the inotify watch limit, so point the user
/// at the tunable rather than leaving a bare "No space left on device".
fn watch_error_context(path: &Path, err: &std::io::Error) -> String {
    if err.raw_os_error() == Some(libc::ENOSPC) {
        format!(
            "Failed to add watch for {} (inotify watch limit reached; increase /proc/sys/fs/inotify/max_user_watches)",
            path.display()
        )
    } else {
        format!("Failed to add watch for {}", path.display())
    }
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
        .map_err(|err| {
            let context = watch_error_context(path, &err);
            anyhow::Error::new(err).context(context)
        })?;

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
        // inotify reuses a descriptor when a still-watched inode is renamed and
        // re-added. Drop any prior path this descriptor mapped to so `by_path`
        // cannot retain a stale, orphaned entry.
        let stale_path = self
            .by_descriptor
            .get(&descriptor)
            .filter(|old| *old != &path)
            .cloned();
        if let Some(stale_path) = stale_path {
            self.by_path.remove(&stale_path);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use tempfile::TempDir;

    #[test]
    fn watch_mask_includes_move_and_delete_self() {
        let mask = watch_mask();
        assert!(mask.contains(WatchMask::MOVE_SELF), "MOVE_SELF must be watched");
        assert!(mask.contains(WatchMask::DELETE_SELF), "DELETE_SELF must be watched");
    }

    #[test]
    fn registry_insert_lookup_and_remove() -> Result<()> {
        let temp = TempDir::new()?;
        let inotify = Inotify::init()?;
        let descriptor = inotify.watches().add(temp.path(), watch_mask())?;

        let mut registry = WatchRegistry::default();
        registry.insert(temp.path().to_path_buf(), descriptor.clone());

        assert!(registry.contains_path(temp.path()));
        assert_eq!(registry.path_for(&descriptor), Some(&temp.path().to_path_buf()));

        registry.remove_by_descriptor(&descriptor);
        assert!(!registry.contains_path(temp.path()), "path mapping must be gone");
        assert_eq!(registry.path_for(&descriptor), None, "descriptor mapping must be gone");
        Ok(())
    }

    #[test]
    fn watch_error_context_mentions_limit_on_enospc() {
        let err = std::io::Error::from_raw_os_error(libc::ENOSPC);
        let msg = watch_error_context(Path::new("/some/dir"), &err);
        assert!(msg.contains("max_user_watches"), "got: {msg}");
    }

    #[test]
    fn watch_error_context_is_plain_for_other_errors() {
        let err = std::io::Error::from_raw_os_error(libc::EACCES);
        let msg = watch_error_context(Path::new("/some/dir"), &err);
        assert!(!msg.contains("max_user_watches"), "got: {msg}");
    }

    #[test]
    fn registry_insert_evicts_stale_path_on_descriptor_reuse() -> Result<()> {
        let temp = TempDir::new()?;
        let inotify = Inotify::init()?;
        let descriptor = inotify.watches().add(temp.path(), watch_mask())?;

        let mut registry = WatchRegistry::default();
        let old_path = temp.path().join("old");
        let new_path = temp.path().join("new");

        registry.insert(old_path.clone(), descriptor.clone());
        // Same descriptor reused for a new path (as happens on rename of a
        // still-watched inode): the old inverse mapping must not linger.
        registry.insert(new_path.clone(), descriptor.clone());

        assert!(!registry.contains_path(&old_path), "stale path must be evicted");
        assert!(registry.contains_path(&new_path));
        assert_eq!(registry.path_for(&descriptor), Some(&new_path));
        Ok(())
    }
}
