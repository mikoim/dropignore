use crate::cli::CliArgs;
use crate::discovery::{DiscoveredPaths, discover_watch_targets};
use crate::dropbox::apply_dropbox_ignore;
use crate::rules::{
    Candidate, JsBuildArtifactsRule, NodeModulesRule, PnpmStoreRule, PythonBuildArtifactsRule,
    RuleEngine, RustTargetRule,
};
use crate::watch::{WatchRegistry, add_watch};
use anyhow::{Context, Result};
use inotify::{EventMask, Inotify};
use log::{debug, info, warn};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
        Box::new(JsBuildArtifactsRule),
    ]);

    // A signal handler flips this flag; the event loop polls it and exits
    // cleanly so a supervisor's SIGTERM yields exit code 0 rather than 143.
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown))
        .context("Failed to register SIGINT handler")?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown))
        .context("Failed to register SIGTERM handler")?;

    let mut watcher = Inotify::init().context("Failed to initialize inotify")?;
    let mut registry = WatchRegistry::default();

    // Seed watches for existing directories and apply the attribute to any
    // pre-existing matches to keep the tree in a consistent state.
    let initial = discover_watch_targets(&root, &rule_engine)?;
    apply_discovered_paths(initial, args.dry_run, &mut watcher, &mut registry)?;

    info!("Watching {}", root.display());
    let _final_registry = event_loop(root, args.dry_run, watcher, registry, rule_engine, shutdown)?;
    Ok(())
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
    if let Some(matched) = rules.evaluate(candidate) {
        matched.log_matched(candidate.path);
        apply_ignore = matched.action.set_dropbox_ignore;
        skip_descendants = matched.action.skip_descendants;
    }

    EntryAction {
        apply_ignore,
        watch_dir: candidate.is_dir() && !skip_descendants,
    }
}

/// Worst-case latency between a shutdown request and the loop noticing it.
const POLL_TIMEOUT_MS: i32 = 500;

/// Result of waiting on the inotify fd.
enum PollResult {
    /// The fd has events queued and is ready to read.
    Readable,
    /// The poll timed out; no events arrived within the window.
    TimedOut,
    /// A signal interrupted the wait (EINTR); the caller should re-check its
    /// shutdown flag and poll again.
    Interrupted,
}

/// Wait up to `timeout_ms` for the inotify fd to become readable. The timeout
/// guarantees the caller regains control periodically, so shutdown never depends
/// on a signal interrupting the wait.
fn poll_inotify(fd: RawFd, timeout_ms: i32) -> Result<PollResult> {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pollfd` is a single valid, fully initialized `pollfd` that lives
    // for the whole call; we pass a count of 1 to match.
    let ret = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
    match ret {
        -1 => {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                Ok(PollResult::Interrupted)
            } else {
                Err(anyhow::Error::new(err).context("poll on inotify fd failed"))
            }
        }
        0 => Ok(PollResult::TimedOut),
        _ => Ok(PollResult::Readable),
    }
}

/// Main blocking loop that waits for inotify events and reacts to
/// creations/moves. Runs until `shutdown` is flipped by a signal handler,
/// then returns the final registry (useful for tests; `run` discards it).
fn event_loop(
    root: PathBuf,
    dry_run: bool,
    mut watcher: Inotify,
    mut registry: WatchRegistry,
    rule_engine: RuleEngine,
    shutdown: Arc<AtomicBool>,
) -> Result<WatchRegistry> {
    let fd = watcher.as_raw_fd();
    loop {
        if shutdown.load(Ordering::Relaxed) {
            info!("Received shutdown signal, stopping watcher");
            break;
        }
        match poll_inotify(fd, POLL_TIMEOUT_MS)? {
            PollResult::Interrupted | PollResult::TimedOut => continue,
            PollResult::Readable => {
                drain_events(&mut watcher, &mut registry, &rule_engine, &root, dry_run)?
            }
        }
    }
    Ok(registry)
}

/// Read one buffer's worth of inotify events (non-blocking) and apply all
/// per-event handling: mark matches, seed watches for new directories, and run
/// scoped or whole-tree rescans. Returns `Ok(())` when no events are queued.
fn drain_events(
    watcher: &mut Inotify,
    registry: &mut WatchRegistry,
    rule_engine: &RuleEngine,
    root: &Path,
    dry_run: bool,
) -> Result<()> {
    let mut buffer = [0u8; 4096];
    let events = match watcher.read_events(&mut buffer) {
        Ok(events) => events,
        // The fd is non-blocking; an empty queue is the normal terminator.
        Err(err) if err.kind() == ErrorKind::WouldBlock => return Ok(()),
        Err(err) => {
            return Err(anyhow::Error::new(err).context("Failed to read inotify events"));
        }
    };

    // Collect new directories to process after the borrow from `events` ends,
    // which keeps the borrow checker happy while allowing new watches to be added.
    let mut pending_directories: Vec<PathBuf> = Vec::new();
    let mut needs_rescan = false;
    // Distinct subtrees to rescan because a rule trigger file appeared in
    // them. Deduplicated so repeated triggers in one batch rescan once.
    let mut rescan_scopes: HashSet<PathBuf> = HashSet::new();

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

        if !(event.mask.contains(EventMask::CREATE) || event.mask.contains(EventMask::MOVED_TO)) {
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

        // A dependency file (e.g. Cargo.toml) can flip an order-dependent
        // rule's verdict for a sibling that already exists and is watched.
        // Reuse the overflow rescan path to reconcile the whole tree; the
        // check runs before the metadata read so a transient stat failure on
        // the trigger file still schedules the rescan.
        if rule_engine.is_trigger(name) {
            info!(
                "Trigger file {} created; rescanning {} to reconcile dependent rules",
                parent_dir.join(name).display(),
                parent_dir.display()
            );
            rescan_scopes.insert(parent_dir.to_path_buf());
        }

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
            file_type: metadata.file_type(),
        };

        let action = plan_entry(&candidate, rule_engine);

        if action.apply_ignore {
            // Failure is already logged at error! by apply_dropbox_ignore;
            // the loop continues to the next event regardless.
            let _ = apply_dropbox_ignore(&full_path, dry_run);
        }

        if action.watch_dir {
            pending_directories.push(full_path);
        }
    }

    // Process newly discovered directories once the event iterator is dropped so
    // inotify can be borrowed mutably again.
    for directory in pending_directories {
        let discovered = match discover_watch_targets(&directory, rule_engine) {
            Ok(d) => d,
            Err(err) => {
                warn!(
                    "Failed to walk {} for watch seeding: {err}",
                    directory.display()
                );
                continue;
            }
        };

        apply_discovered_paths(discovered, dry_run, watcher, registry)?;
    }

    if needs_rescan {
        // Overflow dropped events: no descriptor is trustworthy, so rebuild
        // the whole tree. This supersedes any recorded scopes (all under root).
        rescan_subtree(root, dry_run, watcher, registry, rule_engine)?;
    } else {
        for scope in &rescan_scopes {
            rescan_subtree(scope, dry_run, watcher, registry, rule_engine)?;
        }
    }

    Ok(())
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
    // path cannot terminate the watcher. Individual errors are logged by
    // apply_dropbox_ignore; this adds a single rollup summary.
    let failures = apply_all(&discovered.matches, |path| apply_dropbox_ignore(path, dry_run));
    if failures > 0 {
        warn!(
            "Failed to mark {failures} of {} discovered path(s) as ignored",
            discovered.matches.len()
        );
    }

    for directory in discovered.watchers {
        add_watch(watcher, registry, &directory)?;
    }

    Ok(())
}

/// Tear down every watch at or under `scope` and rebuild that portion of the
/// watch set from the current tree. Used after a queue overflow (`scope` = the
/// root), where dropped events mean no descriptor can be trusted, and when a
/// rule's trigger file appears (`scope` = the trigger's parent), where a
/// pre-existing sibling may have just started matching.
fn rescan_subtree(
    scope: &Path,
    dry_run: bool,
    watcher: &mut Inotify,
    registry: &mut WatchRegistry,
    rules: &RuleEngine,
) -> Result<()> {
    // Ordering hazard: we tear down watches *before* reseeding. For `scope` =
    // root this is the same window the overflow path has always accepted; for
    // any scope below the root the root watch survives, so a discovery failure
    // cannot leave the loop blocked forever in `read_events_blocking`. This is
    // safe today because `discover_watch_targets` never returns `Err` (it logs
    // and skips per-entry failures); if that changes, the `Err` branch would
    // need to reseed defensively rather than just warn.
    for descriptor in registry.drain_subtree(scope) {
        // EINVAL means the kernel already dropped this watch (inode gone);
        // nothing else is actionable, so ignore the result.
        let _ = watcher.watches().remove(descriptor);
    }
    match discover_watch_targets(scope, rules) {
        Ok(discovered) => apply_discovered_paths(discovered, dry_run, watcher, registry),
        Err(err) => {
            warn!("Rescan of {} failed: {err}", scope.display());
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
        let candidate = Candidate { path: &link, file_type: metadata.file_type() };
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
        let candidate = Candidate { path: &dir, file_type: metadata.file_type() };
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
        let candidate = Candidate { path: &dir, file_type: metadata.file_type() };
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

        rescan_subtree(temp.path(), true, &mut watcher, &mut registry, &rules)?;

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
    fn rescan_subtree_reconciles_newly_matched_sibling() -> Result<()> {
        use crate::rules::RustTargetRule;
        let temp = TempDir::new()?;
        let proj = temp.path().join("proj");
        let target = proj.join("target");
        let target_debug = target.join("debug");
        let src = proj.join("src");
        fs::create_dir_all(&target_debug)?;
        fs::create_dir(&src)?;

        let rules = RuleEngine::new(vec![Box::new(RustTargetRule)]);
        let mut watcher = Inotify::init()?;
        let mut registry = WatchRegistry::default();

        // No Cargo.toml yet: target does not match, so it and its subtree are
        // watched.
        let discovered = discover_watch_targets(&proj, &rules)?;
        apply_discovered_paths(discovered, true, &mut watcher, &mut registry)?;
        assert!(registry.contains_path(&target), "target watched pre-trigger");
        assert!(
            registry.contains_path(&target_debug),
            "target/debug watched pre-trigger"
        );

        // Trigger appears; a scoped rescan must now skip target's subtree.
        fs::write(proj.join("Cargo.toml"), b"[package]\nname=\"demo\"")?;
        rescan_subtree(&proj, true, &mut watcher, &mut registry, &rules)?;

        assert!(!registry.contains_path(&target), "matched target must not be watched");
        assert!(
            !registry.contains_path(&target_debug),
            "target subtree must be pruned"
        );
        assert!(registry.contains_path(&proj), "project dir stays watched");
        assert!(registry.contains_path(&src), "sibling src stays watched");
        Ok(())
    }

    #[test]
    fn rescan_subtree_leaves_out_of_scope_watches_intact() -> Result<()> {
        let temp = TempDir::new()?;
        let proj_a = temp.path().join("a");
        let proj_b = temp.path().join("b");
        let a_inner = proj_a.join("inner");
        let b_inner = proj_b.join("inner");
        fs::create_dir_all(&a_inner)?;
        fs::create_dir_all(&b_inner)?;

        let rules = engine(); // NodeModulesRule only
        let mut watcher = Inotify::init()?;
        let mut registry = WatchRegistry::default();
        let discovered = discover_watch_targets(temp.path(), &rules)?;
        apply_discovered_paths(discovered, true, &mut watcher, &mut registry)?;
        assert!(registry.contains_path(&b_inner));

        rescan_subtree(&proj_a, true, &mut watcher, &mut registry, &rules)?;

        assert!(registry.contains_path(&a_inner), "in-scope descendant re-added");
        assert!(registry.contains_path(&proj_b), "out-of-scope project untouched");
        assert!(registry.contains_path(&b_inner), "out-of-scope descendant untouched");
        Ok(())
    }

    #[test]
    fn drain_events_registers_new_dir_and_skips_ignored() -> Result<()> {
        use std::thread::sleep;
        use std::time::{Duration, Instant};

        let temp = TempDir::new()?;
        let root = temp.path().to_path_buf();
        let rules = engine(); // NodeModulesRule only
        let mut watcher = Inotify::init()?;
        let mut registry = WatchRegistry::default();

        // Seed watches for the existing (empty) tree so `root` is watched and can
        // deliver CREATE events for children made below.
        let initial = discover_watch_targets(&root, &rules)?;
        apply_discovered_paths(initial, true, &mut watcher, &mut registry)?;
        assert!(registry.contains_path(&root), "root must be watched after seeding");

        // Create entries *after* watching so they arrive through the event path.
        let plain = root.join("plain");
        let nm = root.join("node_modules");
        fs::create_dir(&plain)?;
        fs::create_dir(&nm)?;

        // Drain until the registry reflects the creations or the deadline passes.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !registry.contains_path(&plain) {
            drain_events(&mut watcher, &mut registry, &rules, &root, true)?;
            sleep(Duration::from_millis(20));
        }

        assert!(
            registry.contains_path(&plain),
            "a new plain dir must be watched via the event path"
        );
        assert!(
            !registry.contains_path(&nm),
            "node_modules must be skipped (matched), not watched"
        );
        Ok(())
    }

    #[test]
    fn event_loop_stops_when_shutdown_flag_is_set() -> Result<()> {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        use std::time::{Duration, Instant};

        let temp = TempDir::new()?;
        let root = temp.path().to_path_buf();
        let rules = engine();
        let mut watcher = Inotify::init()?;
        let mut registry = WatchRegistry::default();
        let initial = discover_watch_targets(&root, &rules)?;
        apply_discovered_paths(initial, true, &mut watcher, &mut registry)?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        let root_for_thread = root.clone();
        let handle =
            thread::spawn(move || event_loop(root_for_thread, true, watcher, registry, rules, flag));

        // Let the loop reach its poll wait, then request shutdown.
        thread::sleep(Duration::from_millis(50));
        shutdown.store(true, Ordering::Relaxed);

        // The loop must return within a bounded time (poll timeout is 500 ms).
        let start = Instant::now();
        let returned = handle.join().expect("event loop thread panicked");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "shutdown must be prompt"
        );
        let registry = returned?;
        assert!(
            registry.contains_path(&root),
            "returned registry must retain the root watch"
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
            file_type: metadata.file_type(),
        };
        let rules = RuleEngine::new(vec![Box::new(PythonBuildArtifactsRule)]);
        let action = plan_entry(&candidate, &rules);

        assert!(action.apply_ignore, "matched *.egg-info file must be marked");
        assert!(!action.watch_dir, "a non-directory must not be watched");
        Ok(())
    }
}
