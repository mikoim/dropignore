use crate::cli::CliArgs;
use crate::discovery::{DiscoveredPaths, discover_watch_targets};
use crate::dropbox::apply_dropbox_ignore;
use crate::rules::{
    Candidate, NodeModulesRule, PnpmStoreRule, PythonBuildArtifactsRule, RuleEngine, RustTargetRule,
};
use crate::watch::{WatchRegistry, add_watch};
use anyhow::{Context, Result};
use inotify::{EventMask, Inotify};
use log::{debug, info, warn};
use std::fs;
use std::path::{Path, PathBuf};

/// Entrypoint that wires argument parsing, rule initialization, and the inotify loop.
pub(crate) fn run(args: CliArgs) -> Result<()> {
    let root = args
        .root
        .canonicalize()
        .with_context(|| format!("Failed to resolve path: {}", args.root.display()))?;
    ensure_directory(&root)?;

    let rule_engine = RuleEngine::new(vec![
        Box::new(NodeModulesRule),
        Box::new(PnpmStoreRule),
        Box::new(RustTargetRule),
        Box::new(PythonBuildArtifactsRule),
    ]);
    let mut watcher = Inotify::init().context("Failed to initialize inotify")?;
    let mut registry = WatchRegistry::default();

    // Seed watches for existing directories and apply the attribute to any
    // pre-existing matches to keep the tree in a consistent state.
    let initial = discover_watch_targets(&root, &rule_engine)?;
    apply_discovered_paths(initial, args.dry_run, &mut watcher, &mut registry)?;

    info!("Watching {}", root.display());
    event_loop(root, args.dry_run, watcher, registry, rule_engine)
}

/// Outcome of evaluating a single filesystem entry seen at runtime.
struct EntryAction {
    apply_ignore: bool,
    watch_dir: bool,
}

/// Decide what to do with one entry, given metadata describing the entry
/// itself (not a symlink target). Symlinks are skipped wholesale to mirror
/// `discover_watch_targets` and avoid escaping the watched tree.
fn plan_entry(candidate: &Candidate<'_>, rules: &RuleEngine) -> EntryAction {
    if candidate.is_symlink() {
        return EntryAction { apply_ignore: false, watch_dir: false };
    }

    let mut apply_ignore = false;
    let mut skip_descendants = false;
    if let Some(action) = rules.evaluate_action(candidate) {
        apply_ignore = action.set_dropbox_ignore;
        skip_descendants = action.skip_descendants;
    }

    EntryAction {
        apply_ignore,
        watch_dir: candidate.is_dir() && !skip_descendants,
    }
}

/// Main blocking loop that reads inotify events and reacts to creations/moves.
fn event_loop(
    root: PathBuf,
    dry_run: bool,
    mut watcher: Inotify,
    mut registry: WatchRegistry,
    rule_engine: RuleEngine,
) -> Result<()> {
    let mut buffer = [0u8; 4096];

    loop {
        let events = watcher
            .read_events_blocking(&mut buffer)
            .context("Failed to read inotify events")?;

        // Collect new directories to process after the borrow from `events` ends,
        // which keeps the borrow checker happy while allowing new watches to be added.
        let mut pending_directories: Vec<PathBuf> = Vec::new();
        let mut needs_rescan = false;

        for event in events {
            // A queue overflow means the kernel dropped events; the descriptor
            // and name are invalid, so handle it before any lookup. Collapse
            // multiple overflows in one batch into a single re-scan.
            if event.mask.contains(EventMask::Q_OVERFLOW) {
                warn!(
                    "inotify queue overflowed; events were dropped, will rescan {}",
                    root.display()
                );
                needs_rescan = true;
                continue;
            }

            // Remove bookkeeping for directories that disappeared or were moved,
            // so stale mappings can't resolve later events to the wrong path.
            if event.mask.contains(EventMask::DELETE_SELF)
                || event.mask.contains(EventMask::MOVE_SELF)
            {
                registry.remove_by_descriptor(&event.wd);
                continue;
            }

            if !(event.mask.contains(EventMask::CREATE) || event.mask.contains(EventMask::MOVED_TO))
            {
                continue;
            }

            let parent_dir = match registry.path_for(&event.wd) {
                Some(path) => path,
                None => {
                    warn!("Received event for unknown watch descriptor {:?}", event.wd);
                    continue;
                }
            };

            let name = match &event.name {
                Some(name) => name,
                None => {
                    debug!("Ignored event without a name in {}", parent_dir.display());
                    continue;
                }
            };

            let full_path = parent_dir.join(name);
            let metadata = match fs::symlink_metadata(&full_path) {
                Ok(m) => m,
                Err(err) => {
                    warn!(
                        "Skipping {} because metadata could not be read: {err}",
                        full_path.display()
                    );
                    continue;
                }
            };

            let candidate = Candidate {
                path: &full_path,
                metadata: &metadata,
            };

            let action = plan_entry(&candidate, &rule_engine);

            if action.apply_ignore {
                // Continue past a failure here too; errors are already logged
                // by apply_dropbox_ignore.
                apply_all(std::slice::from_ref(&full_path), |path| {
                    apply_dropbox_ignore(path, dry_run)
                });
            }

            if action.watch_dir {
                pending_directories.push(full_path);
            }
        }

        // Process newly discovered directories once the event iterator is dropped so
        // inotify can be borrowed mutably again.
        for directory in pending_directories {
            let discovered = match discover_watch_targets(&directory, &rule_engine) {
                Ok(d) => d,
                Err(err) => {
                    warn!(
                        "Failed to walk {} for watch seeding: {err}",
                        directory.display()
                    );
                    continue;
                }
            };

            apply_discovered_paths(discovered, dry_run, &mut watcher, &mut registry)?;
        }

        if needs_rescan {
            rebuild_watches(&root, dry_run, &mut watcher, &mut registry, &rule_engine)?;
        }
    }
}

/// Apply `apply` to every path, continuing past individual failures, and
/// return how many failed. Callees are expected to log their own errors
/// (see `apply_dropbox_ignore`), so this helper stays silent.
fn apply_all<F>(paths: &[PathBuf], mut apply: F) -> usize
where
    F: FnMut(&Path) -> Result<()>,
{
    let mut failures = 0;
    for path in paths {
        if apply(path).is_err() {
            failures += 1;
        }
    }
    failures
}

/// Ensure the path exists and is a directory.
fn ensure_directory(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("Failed to read metadata for {}", path.display()))?;
    if !metadata.is_dir() {
        anyhow::bail!("{} is not a directory", path.display());
    }
    Ok(())
}

fn apply_discovered_paths(
    discovered: DiscoveredPaths,
    dry_run: bool,
    watcher: &mut Inotify,
    registry: &mut WatchRegistry,
) -> Result<()> {
    // Apply to every match, continuing past individual failures so one bad
    // path cannot terminate the watcher.
    apply_all(&discovered.matches, |path| apply_dropbox_ignore(path, dry_run));

    for directory in discovered.watchers {
        add_watch(watcher, registry, &directory)?;
    }

    Ok(())
}

/// Tear down every held watch and rebuild the watch set from the current tree.
/// Used after a queue overflow, when dropped events mean no existing descriptor
/// can be trusted (a directory may have been deleted, or deleted and recreated
/// under the same name as a fresh inode).
fn rebuild_watches(
    root: &Path,
    dry_run: bool,
    watcher: &mut Inotify,
    registry: &mut WatchRegistry,
    rules: &RuleEngine,
) -> Result<()> {
    for descriptor in registry.drain_descriptors() {
        // EINVAL means the kernel already dropped this watch (inode gone);
        // nothing else is actionable, so ignore the result.
        let _ = watcher.watches().remove(descriptor);
    }
    match discover_watch_targets(root, rules) {
        Ok(discovered) => apply_discovered_paths(discovered, dry_run, watcher, registry),
        Err(err) => {
            warn!("Rescan after overflow failed for {}: {err}", root.display());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use crate::discovery::discover_watch_targets;
    use crate::rules::{Candidate, NodeModulesRule, PythonBuildArtifactsRule};
    use crate::watch::{WatchRegistry, watch_mask};
    use inotify::Inotify;
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use tempfile::TempDir;

    fn engine() -> RuleEngine {
        RuleEngine::new(vec![Box::new(NodeModulesRule)])
    }

    #[test]
    fn plan_entry_skips_symlink_to_directory() -> Result<()> {
        let temp = TempDir::new()?;
        let real_dir = temp.path().join("real");
        let link = temp.path().join("link");
        fs::create_dir(&real_dir)?;
        symlink(&real_dir, &link)?;

        let metadata = fs::symlink_metadata(&link)?;
        let candidate = Candidate { path: &link, metadata: &metadata };
        let action = plan_entry(&candidate, &engine());

        assert!(!action.apply_ignore, "symlink must not be marked");
        assert!(!action.watch_dir, "symlink target must not be watched");
        Ok(())
    }

    #[test]
    fn plan_entry_watches_plain_directory() -> Result<()> {
        let temp = TempDir::new()?;
        let dir = temp.path().join("plain");
        fs::create_dir(&dir)?;

        let metadata = fs::symlink_metadata(&dir)?;
        let candidate = Candidate { path: &dir, metadata: &metadata };
        let action = plan_entry(&candidate, &engine());

        assert!(!action.apply_ignore);
        assert!(action.watch_dir, "unmatched directory should be watched");
        Ok(())
    }

    #[test]
    fn plan_entry_applies_and_skips_matched_directory() -> Result<()> {
        let temp = TempDir::new()?;
        let dir = temp.path().join("node_modules");
        fs::create_dir(&dir)?;

        let metadata = fs::symlink_metadata(&dir)?;
        let candidate = Candidate { path: &dir, metadata: &metadata };
        let action = plan_entry(&candidate, &engine());

        assert!(action.apply_ignore, "node_modules must be marked");
        assert!(!action.watch_dir, "ignored directory must not be watched");
        Ok(())
    }

    #[test]
    fn apply_all_visits_every_path_despite_failures() {
        let paths = vec![
            PathBuf::from("a"),
            PathBuf::from("b"),
            PathBuf::from("c"),
        ];
        let mut seen = Vec::new();
        let failures = apply_all(&paths, |p| {
            seen.push(p.to_path_buf());
            if p == Path::new("b") {
                anyhow::bail!("boom");
            }
            Ok(())
        });

        assert_eq!(failures, 1, "the single failing path should be counted");
        assert_eq!(seen, paths, "every path must be visited even after a failure");
    }

    #[test]
    fn rescan_is_idempotent() -> Result<()> {
        let temp = TempDir::new()?;
        fs::create_dir(temp.path().join("a"))?;
        fs::create_dir(temp.path().join("a").join("b"))?;
        fs::create_dir(temp.path().join("node_modules"))?;

        let rules = engine();
        let mut watcher = Inotify::init()?;
        let mut registry = WatchRegistry::default();

        let first = discover_watch_targets(temp.path(), &rules)?;
        apply_discovered_paths(first, true, &mut watcher, &mut registry)?;
        let after_first = registry.watched_count();

        // Re-scanning the same tree (as overflow recovery does) must not
        // register duplicate watches.
        let second = discover_watch_targets(temp.path(), &rules)?;
        apply_discovered_paths(second, true, &mut watcher, &mut registry)?;
        let after_second = registry.watched_count();

        assert_eq!(after_first, after_second, "re-scan must not add duplicate watches");
        assert!(after_first >= 3, "root + a + a/b should be watched, node_modules skipped");
        Ok(())
    }

    #[test]
    fn rebuild_watches_reconciles_stale_entries() -> Result<()> {
        let temp = TempDir::new()?;
        fs::create_dir(temp.path().join("a"))?;
        fs::create_dir(temp.path().join("a").join("b"))?;

        let rules = engine();
        let mut watcher = Inotify::init()?;
        let mut registry = WatchRegistry::default();

        // Seed watches from the real tree.
        let discovered = discover_watch_targets(temp.path(), &rules)?;
        apply_discovered_paths(discovered, true, &mut watcher, &mut registry)?;

        // Inject a stale entry for a path that no longer exists, modelling a
        // deleted (or deleted-then-recreated) intermediate whose old
        // bookkeeping lingers. Use a descriptor from a separate directory so it
        // does not collide with any tree descriptor, and so drain's kernel
        // removal has a valid descriptor to call.
        let other = TempDir::new()?;
        let ghost_wd = watcher.watches().add(other.path(), watch_mask())?;
        let ghost = temp.path().join("ghost");
        registry.insert(ghost.clone(), ghost_wd);
        assert!(registry.contains_path(&ghost), "stale entry seeded");

        rebuild_watches(temp.path(), true, &mut watcher, &mut registry, &rules)?;

        assert!(!registry.contains_path(&ghost), "stale entry must be pruned");
        assert!(registry.contains_path(temp.path()), "root must be re-watched");
        assert!(
            registry.contains_path(&temp.path().join("a")),
            "a must be re-watched"
        );
        assert!(
            registry.contains_path(&temp.path().join("a").join("b")),
            "a/b must be re-watched"
        );

        let fresh = discover_watch_targets(temp.path(), &rules)?;
        assert_eq!(
            registry.watched_count(),
            fresh.watchers.len(),
            "registry must match a fresh discovery exactly"
        );
        Ok(())
    }

    #[test]
    fn plan_entry_applies_matched_non_directory_without_watching() -> Result<()> {
        let temp = TempDir::new()?;
        let egg = temp.path().join("pkg.egg-info");
        fs::write(&egg, b"")?;

        let metadata = fs::symlink_metadata(&egg)?;
        let candidate = Candidate {
            path: &egg,
            metadata: &metadata,
        };
        let rules = RuleEngine::new(vec![Box::new(PythonBuildArtifactsRule)]);
        let action = plan_entry(&candidate, &rules);

        assert!(action.apply_ignore, "matched *.egg-info file must be marked");
        assert!(!action.watch_dir, "a non-directory must not be watched");
        Ok(())
    }
}
