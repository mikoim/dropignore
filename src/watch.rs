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

    /// Number of distinct paths currently watched. Used to assert re-scan
    /// idempotency in tests.
    #[allow(dead_code)]
    pub(crate) fn watched_count(&self) -> usize {
        self.by_path.len()
    }

    /// Drop bookkeeping for every watched path at or under `prefix` (inclusive)
    /// and return their descriptors so the caller can remove them from the
    /// kernel. Rebuilds a bounded portion of the watch set: a trigger's parent
    /// subtree, or the whole tree when `prefix` is the watched root.
    ///
    /// `Path::starts_with` compares whole components, so `/a/bc` is not a child
    /// of `/a/b`; it also returns true for equality, so `prefix` itself drains.
    #[allow(dead_code)]
    pub(crate) fn drain_subtree(&mut self, prefix: &Path) -> Vec<WatchDescriptor> {
        let paths: Vec<PathBuf> = self
            .by_path
            .keys()
            .filter(|path| path.starts_with(prefix))
            .cloned()
            .collect();
        let mut descriptors = Vec::with_capacity(paths.len());
        for path in paths {
            if let Some(descriptor) = self.by_path.remove(&path) {
                self.by_descriptor.remove(&descriptor);
                descriptors.push(descriptor);
            }
        }
        descriptors
    }

    /// Drop all bookkeeping and return the descriptors so the caller can
    /// remove them from the kernel. Used by overflow recovery to rebuild the
    /// watch set from scratch.
    pub(crate) fn drain_descriptors(&mut self) -> Vec<WatchDescriptor> {
        self.by_path.clear();
        self.by_descriptor
            .drain()
            .map(|(descriptor, _)| descriptor)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use std::fs;
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

    #[test]
    fn drain_descriptors_empties_registry_and_returns_all() -> Result<()> {
        let temp = TempDir::new()?;
        let dir_a = temp.path().join("a");
        let dir_b = temp.path().join("b");
        fs::create_dir(&dir_a)?;
        fs::create_dir(&dir_b)?;

        let inotify = Inotify::init()?;
        let wd_a = inotify.watches().add(&dir_a, watch_mask())?;
        let wd_b = inotify.watches().add(&dir_b, watch_mask())?;

        let mut registry = WatchRegistry::default();
        registry.insert(dir_a, wd_a);
        registry.insert(dir_b, wd_b);
        assert_eq!(registry.watched_count(), 2);

        let drained = registry.drain_descriptors();
        assert_eq!(drained.len(), 2, "every descriptor must be returned");
        assert_eq!(registry.watched_count(), 0, "registry must be empty after drain");
        Ok(())
    }

    #[test]
    fn drain_subtree_removes_prefix_and_descendants_only() -> Result<()> {
        use std::collections::HashSet;
        let temp = TempDir::new()?;
        let root = temp.path();
        let a = root.join("a");
        let a_b = a.join("b");
        let c = root.join("c");
        fs::create_dir(&a)?;
        fs::create_dir(&a_b)?;
        fs::create_dir(&c)?;

        let inotify = Inotify::init()?;
        let wd_root = inotify.watches().add(root, watch_mask())?;
        let wd_a = inotify.watches().add(&a, watch_mask())?;
        let wd_a_b = inotify.watches().add(&a_b, watch_mask())?;
        let wd_c = inotify.watches().add(&c, watch_mask())?;

        let mut registry = WatchRegistry::default();
        registry.insert(root.to_path_buf(), wd_root);
        registry.insert(a.clone(), wd_a.clone());
        registry.insert(a_b.clone(), wd_a_b.clone());
        registry.insert(c.clone(), wd_c);

        let drained: HashSet<_> = registry.drain_subtree(&a).into_iter().collect();
        let expected: HashSet<_> = [wd_a, wd_a_b].into_iter().collect();
        assert_eq!(drained, expected, "only a and a/b descriptors returned");

        assert!(!registry.contains_path(&a), "a removed");
        assert!(!registry.contains_path(&a_b), "a/b removed");
        assert!(registry.contains_path(root), "root retained");
        assert!(registry.contains_path(&c), "sibling c retained");
        Ok(())
    }

    #[test]
    fn drain_subtree_respects_component_boundaries() -> Result<()> {
        let temp = TempDir::new()?;
        let a = temp.path().join("a");
        let a_b = a.join("b");
        let a_bc = a.join("bc");
        fs::create_dir(&a)?;
        fs::create_dir(&a_b)?;
        fs::create_dir(&a_bc)?;

        let inotify = Inotify::init()?;
        let wd_a_b = inotify.watches().add(&a_b, watch_mask())?;
        let wd_a_bc = inotify.watches().add(&a_bc, watch_mask())?;

        let mut registry = WatchRegistry::default();
        registry.insert(a_b.clone(), wd_a_b);
        registry.insert(a_bc.clone(), wd_a_bc);

        let drained = registry.drain_subtree(&a_b);
        assert_eq!(drained.len(), 1, "only a/b drained");
        assert!(!registry.contains_path(&a_b));
        assert!(
            registry.contains_path(&a_bc),
            "a/bc must NOT be treated as a child of a/b"
        );
        Ok(())
    }

    #[test]
    fn drain_subtree_with_root_prefix_empties_registry() -> Result<()> {
        let temp = TempDir::new()?;
        let dir_a = temp.path().join("a");
        let dir_b = temp.path().join("b");
        fs::create_dir(&dir_a)?;
        fs::create_dir(&dir_b)?;

        let inotify = Inotify::init()?;
        let wd_root = inotify.watches().add(temp.path(), watch_mask())?;
        let wd_a = inotify.watches().add(&dir_a, watch_mask())?;
        let wd_b = inotify.watches().add(&dir_b, watch_mask())?;

        let mut registry = WatchRegistry::default();
        registry.insert(temp.path().to_path_buf(), wd_root);
        registry.insert(dir_a, wd_a);
        registry.insert(dir_b, wd_b);
        assert_eq!(registry.watched_count(), 3);

        let drained = registry.drain_subtree(temp.path());
        assert_eq!(drained.len(), 3, "every descriptor returned");
        assert_eq!(registry.watched_count(), 0, "registry empty after root drain");
        Ok(())
    }
}
