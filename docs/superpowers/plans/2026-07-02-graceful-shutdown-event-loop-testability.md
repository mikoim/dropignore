# Graceful Shutdown and Event-Loop Testability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the inotify event loop interruptible so the process exits cleanly on SIGINT/SIGTERM, and refactor it so its real event-handling path is covered by an integration test.

**Architecture:** Split the loop's single blocking read into (1) a `poll_inotify` wait with a bounded timeout and (2) a `drain_events` seam that processes one buffer of events via the already-non-blocking `read_events`. `event_loop` gates on an `AtomicBool` shutdown flag set by `signal-hook`, and returns its final `WatchRegistry` so tests can assert on it without threads or xattr writes.

**Tech Stack:** Rust 2024, `inotify` 0.10, `libc` 0.2 (`poll`), `signal-hook` 0.3, `anyhow`, `log`, `tempfile` (dev).

## Global Constraints

- Rust edition: `2024` (do not change `Cargo.toml`'s `edition`).
- Exactly one new external dependency is permitted: `signal-hook = "0.3"`. Add no others.
- `libc` is already a dependency; use it for `poll` (no `nix`).
- The inotify fd is **already non-blocking** (the crate calls `inotify_init1(IN_CLOEXEC | IN_NONBLOCK)`), so `read_events` returns `ErrorKind::WouldBlock` when empty. Do **not** add any `fcntl(O_NONBLOCK)` call.
- Poll timeout constant: `const POLL_TIMEOUT_MS: i32 = 500;` — this bounds worst-case shutdown latency.
- No change to the rule set, CLI surface, on-disk format, or marking semantics.
- Preserve the existing "overflow whole-tree rescan supersedes recorded scopes" behavior.
- `cargo test` and `cargo clippy --all-targets` must both pass with no warnings after every task.
- End every commit message with the trailer:
  `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`

---

## File Structure

- Modify `src/app.rs` — extract `drain_events`, add `poll_inotify` + `PollResult`, rewrite `event_loop`, add the shutdown flag, add two integration tests. This is the only source file with logic changes.
- Modify `src/main.rs` — no change expected; `app::run` keeps its signature.
- Modify `Cargo.toml` — add `signal-hook = "0.3"`.

Both tasks touch `src/app.rs`. Task 1 is a pure refactor (no signal handling) validated by the event-path integration test. Task 2 layers shutdown on top, validated by the shutdown integration test. A reviewer can accept Task 1 (seam + poll) while rejecting Task 2 (signals), so they are split.

---

### Task 1: Extract `drain_events` seam and drive the event loop with `poll`

**Files:**
- Modify: `src/app.rs` (rewrite `event_loop` at `src/app.rs:71-208`; add `poll_inotify`, `PollResult`, `drain_events`; add one test in the `#[cfg(test)]` module)

**Interfaces:**
- Consumes: `discover_watch_targets`, `apply_discovered_paths`, `rescan_subtree`, `plan_entry`, `apply_dropbox_ignore`, `WatchRegistry` (all already in `src/app.rs` / imported there).
- Produces:
  - `fn drain_events(watcher: &mut Inotify, registry: &mut WatchRegistry, rules: &RuleEngine, root: &Path, dry_run: bool) -> Result<()>` — reads at most one buffer of events (non-blocking) and applies all existing per-event handling, directory seeding, and rescans; returns `Ok(())` on `WouldBlock`.
  - `enum PollResult { Readable, TimedOut, Interrupted }`
  - `fn poll_inotify(fd: RawFd, timeout_ms: i32) -> Result<PollResult>`
  - `event_loop(root: PathBuf, dry_run: bool, watcher: Inotify, registry: WatchRegistry, rule_engine: RuleEngine) -> Result<()>` — signature unchanged; body now poll-driven and still infinite (no shutdown yet).

- [ ] **Step 1: Write the failing integration test**

Add to the `#[cfg(test)] mod tests` block in `src/app.rs`:

```rust
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib drain_events_registers_new_dir_and_skips_ignored 2>&1 | tail -20`
Expected: FAIL — compile error `cannot find function `drain_events` in this scope`.

- [ ] **Step 3: Add imports and the `poll_inotify` helper**

At the top of `src/app.rs`, add to the existing `use` block (near the other `std` imports at `src/app.rs:12-14`):

```rust
use std::io::ErrorKind;
use std::os::unix::io::{AsRawFd, RawFd};
```

Add, just above `fn event_loop` (`src/app.rs:71`):

```rust
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
```

- [ ] **Step 4: Extract `drain_events` and rewrite `event_loop`**

Replace the entire current `event_loop` function (`src/app.rs:71-208`) with the following two functions. The per-event logic is moved verbatim from the old loop body; only the read mechanism and the surrounding wrapper change.

```rust
/// Main blocking loop that waits for inotify events and reacts to
/// creations/moves. Runs until the process is killed (shutdown gating is added
/// in a later change).
fn event_loop(
    root: PathBuf,
    dry_run: bool,
    mut watcher: Inotify,
    mut registry: WatchRegistry,
    rule_engine: RuleEngine,
) -> Result<()> {
    let fd = watcher.as_raw_fd();
    loop {
        match poll_inotify(fd, POLL_TIMEOUT_MS)? {
            PollResult::Interrupted | PollResult::TimedOut => continue,
            PollResult::Readable => {
                drain_events(&mut watcher, &mut registry, &rule_engine, &root, dry_run)?
            }
        }
    }
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
```

- [ ] **Step 5: Run the new test to verify it passes**

Run: `cargo test --lib drain_events_registers_new_dir_and_skips_ignored 2>&1 | tail -20`
Expected: PASS (`test result: ok. 1 passed`).

- [ ] **Step 6: Run the full suite and clippy**

Run: `cargo test 2>&1 | tail -5 && cargo clippy --all-targets 2>&1 | tail -5`
Expected: all tests pass (34 total); clippy prints no warnings.

- [ ] **Step 7: Commit**

```bash
git add src/app.rs
git commit -m "refactor(app): extract drain_events seam and poll the inotify fd

Split the single blocking read into a bounded poll_inotify wait and a
non-blocking drain_events seam, covered by an event-path integration test.
No behavior change yet; the loop still runs until killed.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: Add graceful shutdown via a signal-set flag

**Files:**
- Modify: `Cargo.toml` (add `signal-hook = "0.3"`)
- Modify: `src/app.rs` (add `shutdown: Arc<AtomicBool>` param to `event_loop`, return `WatchRegistry`, register signals in `run`; add one test)

**Interfaces:**
- Consumes: `event_loop` and `drain_events`/`poll_inotify` from Task 1; `WatchRegistry` from `crate::watch`.
- Produces:
  - `event_loop(root: PathBuf, dry_run: bool, watcher: Inotify, registry: WatchRegistry, rule_engine: RuleEngine, shutdown: Arc<AtomicBool>) -> Result<WatchRegistry>` — checks the flag at the loop head, breaks and returns the registry when set.
  - `run` registers `SIGINT` and `SIGTERM` to set the flag before entering the loop.

- [ ] **Step 1: Write the failing shutdown test**

Add to the `#[cfg(test)] mod tests` block in `src/app.rs`:

```rust
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib event_loop_stops_when_shutdown_flag_is_set 2>&1 | tail -20`
Expected: FAIL — compile error: `event_loop` takes 5 arguments but 6 were supplied.

- [ ] **Step 3: Add the dependency**

Edit `Cargo.toml`, in the `[dependencies]` table (keep the list alphabetically ordered — insert after `log`):

```toml
signal-hook = "0.3"
```

- [ ] **Step 4: Add the shutdown parameter to `event_loop`**

Add imports to the `use` block at the top of `src/app.rs`:

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
```

Change the `event_loop` signature and loop head (from Task 1's version) to:

```rust
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
```

- [ ] **Step 5: Register signals in `run` and pass the flag**

In `run` (`src/app.rs:17-41`), add signal registration after the `RuleEngine::new(...)` block and before `Inotify::init()`, and update the final `event_loop` call. The `use crate::...`/`anyhow` imports already include `Context`.

Add near the top-of-function logic (after `let rule_engine = ...;`):

```rust
    // A signal handler flips this flag; the event loop polls it and exits
    // cleanly so a supervisor's SIGTERM yields exit code 0 rather than 143.
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown))
        .context("Failed to register SIGINT handler")?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown))
        .context("Failed to register SIGTERM handler")?;
```

Change the final line of `run` from:

```rust
    event_loop(root, args.dry_run, watcher, registry, rule_engine)
```

to:

```rust
    let _final_registry = event_loop(root, args.dry_run, watcher, registry, rule_engine, shutdown)?;
    Ok(())
```

- [ ] **Step 6: Run the shutdown test to verify it passes**

Run: `cargo test --lib event_loop_stops_when_shutdown_flag_is_set 2>&1 | tail -20`
Expected: PASS (`test result: ok. 1 passed`).

- [ ] **Step 7: Run the full suite and clippy**

Run: `cargo test 2>&1 | tail -5 && cargo clippy --all-targets 2>&1 | tail -5`
Expected: all tests pass (35 total); clippy prints no warnings.

- [ ] **Step 8: Manual smoke check (optional but recommended)**

Run in one shell: `cargo run -- --dry-run .` then press `Ctrl-C`.
Expected: a `Received shutdown signal, stopping watcher` log line, then the process exits `0` (`echo $status` in fish shows `0`).

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml Cargo.lock src/app.rs
git commit -m "feat(app): shut down cleanly on SIGINT/SIGTERM

Register a signal-set AtomicBool that the polled event loop checks each
lap; event_loop now returns its final WatchRegistry so shutdown is
covered by an integration test and the process exits 0 under a supervisor.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- Graceful SIGINT/SIGTERM shutdown, exit 0, final log line, ≤500 ms latency → Task 2 (Steps 4–5, `POLL_TIMEOUT_MS`, `info!` line).
- Shutdown independent of the crate's EINTR-retry behavior → Task 1 `poll_inotify` timeout + Task 2 flag check (poll timeout forces the check).
- Real event path covered by integration test → Task 1 Step 1.
- Deterministic, xattr-independent test via `dry_run` + returned registry → Task 1 (dry-run `true`, registry assertions) and Task 2 (returned registry).
- Ownership unchanged, no `Arc<Mutex>` → confirmed: registry stays owned by `event_loop`, passed by `&mut` to `drain_events`.
- One new dependency (`signal-hook`) → Task 2 Step 3; Global Constraints forbid others.
- Preserve overflow-supersedes-scopes → carried verbatim into `drain_events`.
- Non-goals (`--once`, config rules, parallel scan, real-xattr/DI tests, portability) → not present in any task.

**Placeholder scan:** No TBD/TODO; every code step shows complete code; commands have expected output. Clean.

**Type consistency:** `drain_events`, `poll_inotify`, `PollResult`, and `event_loop` signatures are identical everywhere they appear (Task 1 defines the 5-arg `event_loop`; Task 2 extends it to 6 args returning `WatchRegistry`, and updates both the `run` call site and the test call site). `POLL_TIMEOUT_MS: i32` matches `libc::poll`'s `c_int` timeout and `RawFd` matches `pollfd.fd`.
