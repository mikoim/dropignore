use crate::cli::CliArgs;
use crate::discovery::{DiscoveredPaths, discover_matches, discover_watch_targets};
use crate::dropbox::apply_dropbox_ignore;
use crate::rules::{ArtifactDirsRule, Candidate, EggInfoRule, MarkedBuildDirRule, RuleEngine};
use crate::watch::{WatchRegistry, add_watch};
use anyhow::{Context, Result};
use inotify::{EventMask, Inotify};
use log::{debug, info, warn};
use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
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
        Box::new(ArtifactDirsRule::NODE_MODULES),
        Box::new(ArtifactDirsRule::PNPM_STORE),
        Box::new(MarkedBuildDirRule::CARGO_TARGET),
        Box::new(MarkedBuildDirRule::MAVEN_TARGET),
        Box::new(MarkedBuildDirRule::GRADLE_BUILD),
        Box::new(ArtifactDirsRule::PYTHON_CACHES),
        Box::new(EggInfoRule),
        Box::new(ArtifactDirsRule::JS_BUILD),
        Box::new(ArtifactDirsRule::JVM_CACHES),
        Box::new(ArtifactDirsRule::IAC_CACHES),
        Box::new(ArtifactDirsRule::DEV_ENV_DIRS),
    ]);

    if args.scan_once {
        return scan_once(&root, &rule_engine, |path| {
            apply_dropbox_ignore(path, args.dry_run)
        });
    }

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

/// Walk the tree once, apply `apply` to every rule match, and return. Watcher
/// collection is skipped entirely (`discover_matches`): nothing is registered
/// with inotify. Fails when at least one matched path could not be marked, so
/// cron/systemd sees a non-zero exit code.
fn scan_once<F>(root: &Path, rules: &RuleEngine, apply: F) -> Result<()>
where
    F: FnMut(&Path) -> Result<()>,
{
    let matches = discover_matches(root, rules)?;
    let total = matches.len();
    let failures = apply_all(&matches, apply);
    if failures > 0 {
        anyhow::bail!("Failed to mark {failures} of {total} matched path(s)");
    }
    info!(
        "Scan complete: {total} matched path(s) under {}",
        root.display()
    );
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
        return EntryAction {
            apply_ignore: false,
            watch_dir: false,
        };
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
    // 64 KiB drains large event bursts in fewer reads; still a single
    // stack frame's allocation.
    let mut buffer = [0u8; 65536];
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

        // A moved directory keeps its kernel watch, and MOVE_SELF fires only
        // for the moved directory itself, so its descendants would keep stale
        // registry entries and live kernel watches. Schedule a scoped rescan
        // of the old path: the drain sweeps the whole stale subtree, and the
        // reseed is a no-op when the path is gone (moved out of tree) or
        // re-registers it when the descriptor was reused for a live path.
        // An unknown descriptor means a prior batch's rescan already drained it.
        // If the root itself moved, every watch is about to go stale and the
        // canonicalized root path is no longer valid; fail so a supervisor
        // restarts the process instead of it idling with no watches.
        if event.mask.contains(EventMask::MOVE_SELF) {
            if let Some(path) = registry.path_for(&event.wd) {
                if path.as_path() == root {
                    anyhow::bail!("Watched root {} was moved or renamed", root.display());
                }
                rescan_scopes.insert(path.clone());
            }
            continue;
        }

        // Deletion needs no rescan: the kernel auto-removes the watch, and a
        // recursive delete fires DELETE_SELF for every directory, so dropping
        // this one entry leaves nothing stale behind. A deleted root gets the
        // same fail-fast treatment as a moved one.
        if event.mask.contains(EventMask::DELETE_SELF) {
            if let Some(path) = registry.path_for(&event.wd)
                && path.as_path() == root
            {
                anyhow::bail!("Watched root {} was deleted", root.display());
            }
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

        // The overflow may have swallowed the root's own MOVE_SELF or
        // DELETE_SELF; if the rescan could not re-watch the root, the tree is
        // gone and idling with zero watches would hide the failure.
        if !registry.contains_path(root) {
            anyhow::bail!(
                "Watched root {} disappeared during overflow rescan",
                root.display()
            );
        }
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
    let failures = apply_all(&discovered.matches, |path| {
        apply_dropbox_ignore(path, dry_run)
    });
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
    use crate::discovery::discover_watch_targets;
    use crate::test_util::xattr_supported;
    use crate::watch::{WatchRegistry, watch_mask};
    use anyhow::Result;
    use inotify::Inotify;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use tempfile::TempDir;

    fn engine() -> RuleEngine {
        RuleEngine::new(vec![Box::new(ArtifactDirsRule::NODE_MODULES)])
    }

    #[test]
    fn plan_entry_skips_symlink_to_directory() -> Result<()> {
        let temp = TempDir::new()?;
        let real_dir = temp.path().join("real");
        let link = temp.path().join("link");
        fs::create_dir(&real_dir)?;
        symlink(&real_dir, &link)?;

        let metadata = fs::symlink_metadata(&link)?;
        let candidate = Candidate {
            path: &link,
            file_type: metadata.file_type(),
        };
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
        let candidate = Candidate {
            path: &dir,
            file_type: metadata.file_type(),
        };
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
        let candidate = Candidate {
            path: &dir,
            file_type: metadata.file_type(),
        };
        let action = plan_entry(&candidate, &engine());

        assert!(action.apply_ignore, "node_modules must be marked");
        assert!(!action.watch_dir, "ignored directory must not be watched");
        Ok(())
    }

    #[test]
    fn apply_all_visits_every_path_despite_failures() {
        let paths = vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")];
        let mut seen = Vec::new();
        let failures = apply_all(&paths, |p| {
            seen.push(p.to_path_buf());
            if p == Path::new("b") {
                anyhow::bail!("boom");
            }
            Ok(())
        });

        assert_eq!(failures, 1, "the single failing path should be counted");
        assert_eq!(
            seen, paths,
            "every path must be visited even after a failure"
        );
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

        assert_eq!(
            after_first, after_second,
            "re-scan must not add duplicate watches"
        );
        assert!(
            after_first >= 3,
            "root + a + a/b should be watched, node_modules skipped"
        );
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

        assert!(
            !registry.contains_path(&ghost),
            "stale entry must be pruned"
        );
        assert!(
            registry.contains_path(temp.path()),
            "root must be re-watched"
        );
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
        let temp = TempDir::new()?;
        let proj = temp.path().join("proj");
        let target = proj.join("target");
        let target_debug = target.join("debug");
        let src = proj.join("src");
        fs::create_dir_all(&target_debug)?;
        fs::create_dir(&src)?;

        let rules = RuleEngine::new(vec![Box::new(MarkedBuildDirRule::CARGO_TARGET)]);
        let mut watcher = Inotify::init()?;
        let mut registry = WatchRegistry::default();

        // No Cargo.toml yet: target does not match, so it and its subtree are
        // watched.
        let discovered = discover_watch_targets(&proj, &rules)?;
        apply_discovered_paths(discovered, true, &mut watcher, &mut registry)?;
        assert!(
            registry.contains_path(&target),
            "target watched pre-trigger"
        );
        assert!(
            registry.contains_path(&target_debug),
            "target/debug watched pre-trigger"
        );

        // Trigger appears; a scoped rescan must now skip target's subtree.
        fs::write(proj.join("Cargo.toml"), b"[package]\nname=\"demo\"")?;
        rescan_subtree(&proj, true, &mut watcher, &mut registry, &rules)?;

        assert!(
            !registry.contains_path(&target),
            "matched target must not be watched"
        );
        assert!(
            !registry.contains_path(&target_debug),
            "target subtree must be pruned"
        );
        assert!(registry.contains_path(&proj), "project dir stays watched");
        assert!(registry.contains_path(&src), "sibling src stays watched");
        Ok(())
    }

    #[test]
    fn rescan_subtree_reconciles_gradle_build_after_marker() -> Result<()> {
        let temp = TempDir::new()?;
        let proj = temp.path().join("proj");
        let build = proj.join("build");
        let build_classes = build.join("classes");
        fs::create_dir_all(&build_classes)?;

        let rules = RuleEngine::new(vec![Box::new(MarkedBuildDirRule::GRADLE_BUILD)]);
        let mut watcher = Inotify::init()?;
        let mut registry = WatchRegistry::default();

        // No Gradle script yet: build does not match, so it is watched.
        let discovered = discover_watch_targets(&proj, &rules)?;
        apply_discovered_paths(discovered, true, &mut watcher, &mut registry)?;
        assert!(registry.contains_path(&build), "build watched pre-marker");

        // Marker appears; a scoped rescan must now skip build's subtree.
        fs::write(proj.join("build.gradle.kts"), b"")?;
        rescan_subtree(&proj, true, &mut watcher, &mut registry, &rules)?;

        assert!(
            !registry.contains_path(&build),
            "matched build must not be watched"
        );
        assert!(
            !registry.contains_path(&build_classes),
            "build subtree must be pruned"
        );
        assert!(registry.contains_path(&proj), "project dir stays watched");
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

        let rules = engine(); // ArtifactDirsRule::NODE_MODULES only
        let mut watcher = Inotify::init()?;
        let mut registry = WatchRegistry::default();
        let discovered = discover_watch_targets(temp.path(), &rules)?;
        apply_discovered_paths(discovered, true, &mut watcher, &mut registry)?;
        assert!(registry.contains_path(&b_inner));

        rescan_subtree(&proj_a, true, &mut watcher, &mut registry, &rules)?;

        assert!(
            registry.contains_path(&a_inner),
            "in-scope descendant re-added"
        );
        assert!(
            registry.contains_path(&proj_b),
            "out-of-scope project untouched"
        );
        assert!(
            registry.contains_path(&b_inner),
            "out-of-scope descendant untouched"
        );
        Ok(())
    }

    #[test]
    fn drain_events_registers_new_dir_and_skips_ignored() -> Result<()> {
        use std::thread::sleep;
        use std::time::{Duration, Instant};

        let temp = TempDir::new()?;
        let root = temp.path().to_path_buf();
        let rules = engine(); // ArtifactDirsRule::NODE_MODULES only
        let mut watcher = Inotify::init()?;
        let mut registry = WatchRegistry::default();

        // Seed watches for the existing (empty) tree so `root` is watched and can
        // deliver CREATE events for children made below.
        let initial = discover_watch_targets(&root, &rules)?;
        apply_discovered_paths(initial, true, &mut watcher, &mut registry)?;
        assert!(
            registry.contains_path(&root),
            "root must be watched after seeding"
        );

        // Create entries *after* watching so they arrive through the event path.
        // node_modules is created first so its CREATE event is drained no later
        // than plain's, making the "not watched" assertion below meaningful
        // rather than trivially true because it was never evaluated.
        let plain = root.join("plain");
        let nm = root.join("node_modules");
        fs::create_dir(&nm)?;
        fs::create_dir(&plain)?;

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
    fn drain_events_prunes_subtree_moved_out_of_tree() -> Result<()> {
        use std::thread::sleep;
        use std::time::{Duration, Instant};

        let temp = TempDir::new()?;
        let outside = TempDir::new()?;
        let root = temp.path().to_path_buf();
        let a = root.join("a");
        let a_x = a.join("x");
        fs::create_dir_all(&a_x)?;

        let rules = engine();
        let mut watcher = Inotify::init()?;
        let mut registry = WatchRegistry::default();
        let initial = discover_watch_targets(&root, &rules)?;
        apply_discovered_paths(initial, true, &mut watcher, &mut registry)?;
        assert!(registry.contains_path(&a_x), "a/x watched before the move");

        fs::rename(&a, outside.path().join("a"))?;

        // Drain until the stale entries disappear or the deadline passes.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && (registry.contains_path(&a) || registry.contains_path(&a_x))
        {
            drain_events(&mut watcher, &mut registry, &rules, &root, true)?;
            sleep(Duration::from_millis(20));
        }

        assert!(!registry.contains_path(&a), "moved dir must be pruned");
        assert!(
            !registry.contains_path(&a_x),
            "descendant of moved dir must be pruned"
        );
        let fresh = discover_watch_targets(&root, &rules)?;
        assert_eq!(
            registry.watched_count(),
            fresh.watchers.len(),
            "registry must match a fresh discovery of the root"
        );
        Ok(())
    }

    #[test]
    fn drain_events_reconciles_in_tree_rename() -> Result<()> {
        use std::thread::sleep;
        use std::time::{Duration, Instant};

        let temp = TempDir::new()?;
        let root = temp.path().to_path_buf();
        let a = root.join("a");
        let a_x = a.join("x");
        fs::create_dir_all(&a_x)?;

        let rules = engine();
        let mut watcher = Inotify::init()?;
        let mut registry = WatchRegistry::default();
        let initial = discover_watch_targets(&root, &rules)?;
        apply_discovered_paths(initial, true, &mut watcher, &mut registry)?;
        assert!(
            registry.contains_path(&a_x),
            "a/x watched before the rename"
        );

        let b = root.join("b");
        let b_x = b.join("x");
        fs::rename(&a, &b)?;

        // Drain until the new paths are watched and the old ones are gone.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && (!registry.contains_path(&b_x) || registry.contains_path(&a_x))
        {
            drain_events(&mut watcher, &mut registry, &rules, &root, true)?;
            sleep(Duration::from_millis(20));
        }

        assert!(registry.contains_path(&b), "renamed dir must be watched");
        assert!(
            registry.contains_path(&b_x),
            "descendant must follow the rename"
        );
        assert!(!registry.contains_path(&a), "old path must be pruned");
        assert!(
            !registry.contains_path(&a_x),
            "old descendant must be pruned"
        );
        let fresh = discover_watch_targets(&root, &rules)?;
        assert_eq!(
            registry.watched_count(),
            fresh.watchers.len(),
            "registry must match a fresh discovery of the root"
        );
        Ok(())
    }

    #[test]
    fn drain_events_errors_when_root_is_moved() -> Result<()> {
        use std::thread::sleep;
        use std::time::{Duration, Instant};

        let temp = TempDir::new()?;
        let root = temp.path().join("root");
        fs::create_dir(&root)?;

        let rules = engine();
        let mut watcher = Inotify::init()?;
        let mut registry = WatchRegistry::default();
        let initial = discover_watch_targets(&root, &rules)?;
        apply_discovered_paths(initial, true, &mut watcher, &mut registry)?;

        fs::rename(&root, temp.path().join("elsewhere"))?;

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut outcome = Ok(());
        while Instant::now() < deadline && outcome.is_ok() {
            outcome = drain_events(&mut watcher, &mut registry, &rules, &root, true);
            sleep(Duration::from_millis(20));
        }

        let err = outcome.expect_err("root move must surface an error");
        assert!(
            err.to_string().contains("was moved or renamed"),
            "got: {err}"
        );
        Ok(())
    }

    #[test]
    fn drain_events_errors_when_root_is_deleted() -> Result<()> {
        use std::thread::sleep;
        use std::time::{Duration, Instant};

        let temp = TempDir::new()?;
        let root = temp.path().join("root");
        fs::create_dir(&root)?;

        let rules = engine();
        let mut watcher = Inotify::init()?;
        let mut registry = WatchRegistry::default();
        let initial = discover_watch_targets(&root, &rules)?;
        apply_discovered_paths(initial, true, &mut watcher, &mut registry)?;

        fs::remove_dir_all(&root)?;

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut outcome = Ok(());
        while Instant::now() < deadline && outcome.is_ok() {
            outcome = drain_events(&mut watcher, &mut registry, &rules, &root, true);
            sleep(Duration::from_millis(20));
        }

        let err = outcome.expect_err("root deletion must surface an error");
        assert!(err.to_string().contains("was deleted"), "got: {err}");
        Ok(())
    }

    #[test]
    fn event_loop_stops_when_shutdown_flag_is_set() -> Result<()> {
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
        let handle = thread::spawn(move || {
            event_loop(root_for_thread, true, watcher, registry, rules, flag)
        });

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
        let rules = RuleEngine::new(vec![Box::new(EggInfoRule)]);
        let action = plan_entry(&candidate, &rules);

        assert!(
            action.apply_ignore,
            "matched *.egg-info file must be marked"
        );
        assert!(!action.watch_dir, "a non-directory must not be watched");
        Ok(())
    }

    #[test]
    fn ensure_directory_rejects_non_directory() -> Result<()> {
        let temp = TempDir::new()?;
        let file = temp.path().join("plain");
        fs::write(&file, b"")?;
        ensure_directory(temp.path())?;

        let err = ensure_directory(&file).expect_err("a regular file must be rejected");
        assert!(err.to_string().contains("is not a directory"), "got: {err}");
        Ok(())
    }

    #[test]
    fn drain_events_removes_registry_entry_for_deleted_subdir() -> Result<()> {
        use std::thread::sleep;
        use std::time::{Duration, Instant};

        let temp = TempDir::new()?;
        let root = temp.path().to_path_buf();
        let a = root.join("a");
        fs::create_dir(&a)?;

        let rules = engine();
        let mut watcher = Inotify::init()?;
        let mut registry = WatchRegistry::default();
        let initial = discover_watch_targets(&root, &rules)?;
        apply_discovered_paths(initial, true, &mut watcher, &mut registry)?;
        assert!(registry.contains_path(&a), "a watched before deletion");

        fs::remove_dir(&a)?;

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && registry.contains_path(&a) {
            drain_events(&mut watcher, &mut registry, &rules, &root, true)?;
            sleep(Duration::from_millis(20));
        }

        assert!(
            !registry.contains_path(&a),
            "deleted subdir must leave the registry via DELETE_SELF"
        );
        assert!(registry.contains_path(&root), "root stays watched");
        Ok(())
    }

    #[test]
    fn drain_events_rescans_when_trigger_file_appears() -> Result<()> {
        use std::thread::sleep;
        use std::time::{Duration, Instant};

        let temp = TempDir::new()?;
        let root = temp.path().to_path_buf();
        let proj = root.join("proj");
        let target = proj.join("target");
        fs::create_dir_all(&target)?;

        let rules = RuleEngine::new(vec![Box::new(MarkedBuildDirRule::CARGO_TARGET)]);
        let mut watcher = Inotify::init()?;
        let mut registry = WatchRegistry::default();
        let initial = discover_watch_targets(&root, &rules)?;
        apply_discovered_paths(initial, true, &mut watcher, &mut registry)?;
        assert!(
            registry.contains_path(&target),
            "target watched while no Cargo.toml exists"
        );

        fs::write(proj.join("Cargo.toml"), b"[package]\nname=\"demo\"")?;

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && registry.contains_path(&target) {
            drain_events(&mut watcher, &mut registry, &rules, &root, true)?;
            sleep(Duration::from_millis(20));
        }

        assert!(
            !registry.contains_path(&target),
            "trigger CREATE event must un-watch target via scoped rescan"
        );
        assert!(registry.contains_path(&proj), "project dir stays watched");
        assert!(registry.contains_path(&root), "root stays watched");
        Ok(())
    }

    #[test]
    fn event_loop_processes_events_before_shutdown() -> Result<()> {
        use std::thread;
        use std::time::Duration;

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
        let handle = thread::spawn(move || {
            event_loop(root_for_thread, true, watcher, registry, rules, flag)
        });

        // Let the loop reach its poll wait, then create a directory it must pick up.
        thread::sleep(Duration::from_millis(50));
        let newdir = root.join("newdir");
        fs::create_dir(&newdir)?;

        // Give the loop one full poll window to drain the event, then stop it.
        thread::sleep(Duration::from_millis(600));
        shutdown.store(true, Ordering::Relaxed);

        let registry = handle.join().expect("event loop thread panicked")?;
        assert!(
            registry.contains_path(&newdir),
            "event_loop must watch a directory created while it runs"
        );
        Ok(())
    }

    #[test]
    fn drain_events_recovers_from_queue_overflow() -> Result<()> {
        use std::thread::sleep;
        use std::time::{Duration, Instant};

        let max: usize = fs::read_to_string("/proc/sys/fs/inotify/max_queued_events")?
            .trim()
            .parse()?;
        if max > 65536 {
            eprintln!("skipping: max_queued_events={max} is too large to overflow in a test");
            return Ok(());
        }

        let temp = TempDir::new()?;
        let root = temp.path().to_path_buf();
        let rules = engine();
        let mut watcher = Inotify::init()?;
        let mut registry = WatchRegistry::default();
        let initial = discover_watch_targets(&root, &rules)?;
        apply_discovered_paths(initial, true, &mut watcher, &mut registry)?;

        // Fill the kernel queue without draining so it genuinely overflows.
        for i in 0..(max + 100) {
            fs::write(root.join(format!("f{i}")), b"")?;
        }

        // Created while events are being dropped: their CREATE events are lost,
        // so only the overflow rescan can discover them.
        let late_dir = root.join("late_dir");
        let nm = root.join("node_modules");
        fs::create_dir(&late_dir)?;
        fs::create_dir(&nm)?;

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !registry.contains_path(&late_dir) {
            drain_events(&mut watcher, &mut registry, &rules, &root, true)?;
            sleep(Duration::from_millis(20));
        }

        assert!(
            registry.contains_path(&late_dir),
            "overflow rescan must find a directory whose CREATE was dropped"
        );
        assert!(
            !registry.contains_path(&nm),
            "overflow rescan must skip matched node_modules"
        );
        let fresh = discover_watch_targets(&root, &rules)?;
        assert_eq!(
            registry.watched_count(),
            fresh.watchers.len(),
            "registry must match a fresh discovery after overflow recovery"
        );
        Ok(())
    }

    #[test]
    fn drain_events_errors_when_root_vanishes_during_overflow() -> Result<()> {
        use std::thread::sleep;
        use std::time::{Duration, Instant};

        let max: usize = fs::read_to_string("/proc/sys/fs/inotify/max_queued_events")?
            .trim()
            .parse()?;
        if max > 65536 {
            eprintln!("skipping: max_queued_events={max} is too large to overflow in a test");
            return Ok(());
        }

        let temp = TempDir::new()?;
        let root = temp.path().join("root");
        fs::create_dir(&root)?;

        let rules = engine();
        let mut watcher = Inotify::init()?;
        let mut registry = WatchRegistry::default();
        let initial = discover_watch_targets(&root, &rules)?;
        apply_discovered_paths(initial, true, &mut watcher, &mut registry)?;

        // Fill the kernel queue without draining so it genuinely overflows.
        for i in 0..(max + 100) {
            fs::write(root.join(format!("f{i}")), b"")?;
        }

        // The queue is still full, so the root's DELETE_SELF is dropped too;
        // only the overflow rescan can notice the root is gone.
        fs::remove_dir_all(&root)?;

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut outcome = Ok(());
        while Instant::now() < deadline && outcome.is_ok() {
            outcome = drain_events(&mut watcher, &mut registry, &rules, &root, true);
            sleep(Duration::from_millis(20));
        }

        let err = outcome.expect_err("vanished root must surface an error after overflow");
        assert!(
            err.to_string()
                .contains("disappeared during overflow rescan"),
            "got: {err}"
        );
        Ok(())
    }

    #[test]
    fn scan_once_visits_matches_only() -> Result<()> {
        let temp = TempDir::new()?;
        let ignored = temp.path().join("node_modules");
        let plain = temp.path().join("src");
        fs::create_dir(&ignored)?;
        fs::create_dir(&plain)?;

        let mut visited = Vec::new();
        scan_once(temp.path(), &engine(), |path| {
            visited.push(path.to_path_buf());
            Ok(())
        })?;

        assert_eq!(visited, vec![ignored], "only the matched path is applied");
        Ok(())
    }

    #[test]
    fn scan_once_fails_when_any_apply_fails() -> Result<()> {
        let temp = TempDir::new()?;
        fs::create_dir(temp.path().join("node_modules"))?;

        let err = scan_once(temp.path(), &engine(), |_| anyhow::bail!("boom"))
            .expect_err("a failed apply must fail the scan");

        assert!(
            err.to_string().contains("1 of 1"),
            "message must carry failure/total counts, got: {err}"
        );
        Ok(())
    }

    #[test]
    fn scan_once_marks_matches_with_real_xattr() -> Result<()> {
        let temp = TempDir::new()?;
        let ignored = temp.path().join("node_modules");
        fs::create_dir(&ignored)?;
        if !xattr_supported(temp.path()) {
            eprintln!("skipping: filesystem lacks user.* xattr support");
            return Ok(());
        }

        scan_once(temp.path(), &engine(), |path| {
            apply_dropbox_ignore(path, false)
        })?;

        let c_path = CString::new(ignored.as_os_str().as_bytes())?;
        let c_name = CString::new("user.com.dropbox.ignored")?;
        // One byte larger than the expected value so a longer stored value
        // yields a length mismatch instead of a truncated false positive.
        let mut value = [0u8; 2];
        // SAFETY: pointers are valid for the duration of the call and the
        // size matches the buffer.
        let len = unsafe {
            libc::getxattr(
                c_path.as_ptr(),
                c_name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        assert_eq!(len, 1, "attribute must be exactly one byte");
        assert_eq!(&value[..1], b"1", "attribute value must be \"1\"");
        Ok(())
    }
}
