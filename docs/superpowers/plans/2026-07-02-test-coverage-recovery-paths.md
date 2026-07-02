# Recovery-Path Test Coverage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add six tests covering the untested recovery paths in `src/app.rs` (queue-overflow rescan, trigger-file wiring, non-root DELETE_SELF, `event_loop` Readable branch, `ensure_directory`), raising `app.rs` line coverage from 85% to roughly 95%.

**Architecture:** Test-only change. All tests go into the existing `mod tests` at the bottom of `src/app.rs` and follow the established style there: `TempDir` + a real `Inotify` instance + deadline-bounded `drain_events` loops in dry-run mode. Production code is not modified.

**Tech Stack:** Rust 2024, `cargo test`, dev-dependency `tempfile` (already present), `cargo llvm-cov` for verification.

## Global Constraints

- **Test-only:** no file outside `src/app.rs`'s `#[cfg(test)] mod tests` may change.
- **No new dependencies** (dev or otherwise).
- **Follow existing test style:** deadline-bounded drain loops (5 s normally, 10 s for the two overflow tests), `sleep(Duration::from_millis(20))` between drains, dry-run (`true`) everywhere, assertion messages on every `assert!`.
- **Overflow guard:** if `/proc/sys/fs/inotify/max_queued_events` exceeds `65536`, skip with `eprintln!` and `return Ok(())`, mirroring the `xattr_supported` skip pattern in `src/dropbox.rs`.
- The spec is `docs/superpowers/specs/2026-07-02-test-coverage-recovery-paths-design.md`; consult it if a behavior question comes up.
- Work happens on the already-checked-out branch `test-coverage-recovery-paths`.

### Background every task needs

The test module (`src/app.rs`, `mod tests` starting around line 424) already has `use super::*;`, `TempDir`, `Inotify`, `WatchRegistry`, and `discover_watch_targets` in scope, plus a helper:

```rust
fn engine() -> RuleEngine {
    RuleEngine::new(vec![Box::new(ArtifactDirsRule::NODE_MODULES)])
}
```

The standard seeding preamble used by existing `drain_events` tests is:

```rust
let temp = TempDir::new()?;
let root = temp.path().to_path_buf();
let rules = engine();
let mut watcher = Inotify::init()?;
let mut registry = WatchRegistry::default();
let initial = discover_watch_targets(&root, &rules)?;
apply_discovered_paths(initial, true, &mut watcher, &mut registry)?;
```

New tests are appended inside `mod tests`, before the closing brace of the module.

---

### Task 1: `ensure_directory` rejection test

**Files:**
- Test: `src/app.rs` (append inside `mod tests`)

**Interfaces:**
- Consumes: `ensure_directory(path: &Path) -> Result<()>` (`src/app.rs:356`), private but visible via `use super::*;`.
- Produces: nothing consumed by later tasks.

- [ ] **Step 1: Write the test**

```rust
#[test]
fn ensure_directory_rejects_non_directory() -> Result<()> {
    let temp = TempDir::new()?;
    let file = temp.path().join("plain");
    fs::write(&file, b"")?;

    let err = ensure_directory(&file).expect_err("a regular file must be rejected");
    assert!(
        err.to_string().contains("is not a directory"),
        "got: {err}"
    );
    Ok(())
}
```

- [ ] **Step 2: Run the test, expect PASS**

Run: `cargo test ensure_directory_rejects_non_directory`
Expected: `test result: ok. 1 passed`

- [ ] **Step 3: Run the full suite**

Run: `cargo test`
Expected: 55 passed, 0 failed.

- [ ] **Step 4: Commit**

```bash
git add src/app.rs
git commit -m "test(app): cover ensure_directory non-directory rejection"
```

---

### Task 2: non-root DELETE_SELF bookkeeping test

**Files:**
- Test: `src/app.rs` (append inside `mod tests`)

**Interfaces:**
- Consumes: `drain_events`, `apply_discovered_paths`, `engine()` from the test module.
- Produces: nothing consumed by later tasks.

Covers `src/app.rs:231` (`registry.remove_by_descriptor` on a non-root DELETE_SELF). Note the watch mask has no `DELETE`, so removing `a` produces exactly one event: `a`'s own `DELETE_SELF`.

- [ ] **Step 1: Write the test**

```rust
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
```

- [ ] **Step 2: Run the test, expect PASS**

Run: `cargo test drain_events_removes_registry_entry_for_deleted_subdir`
Expected: `test result: ok. 1 passed`

- [ ] **Step 3: Run the full suite**

Run: `cargo test`
Expected: 56 passed, 0 failed.

- [ ] **Step 4: Commit**

```bash
git add src/app.rs
git commit -m "test(app): cover non-root DELETE_SELF registry removal"
```

---

### Task 3: trigger-file wiring end-to-end test

**Files:**
- Test: `src/app.rs` (append inside `mod tests`)

**Interfaces:**
- Consumes: `drain_events`, `apply_discovered_paths`, `MarkedBuildDirRule::CARGO_TARGET`, `RuleEngine` (all in scope via `use super::*;`).
- Produces: nothing consumed by later tasks.

Covers `src/app.rs:260-267`: a `Cargo.toml` CREATE event must make `rule_engine.is_trigger` schedule a scoped rescan that un-watches a pre-existing sibling `target/`. The existing `rescan_subtree_reconciles_newly_matched_sibling` test calls `rescan_subtree` directly; this one drives the same outcome through the event path.

- [ ] **Step 1: Write the test**

```rust
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
```

- [ ] **Step 2: Run the test, expect PASS**

Run: `cargo test drain_events_rescans_when_trigger_file_appears`
Expected: `test result: ok. 1 passed`

- [ ] **Step 3: Run the full suite**

Run: `cargo test`
Expected: 57 passed, 0 failed.

- [ ] **Step 4: Commit**

```bash
git add src/app.rs
git commit -m "test(app): cover trigger-file rescan wiring through the event path"
```

---

### Task 4: `event_loop` Readable-branch test

**Files:**
- Test: `src/app.rs` (append inside `mod tests`)

**Interfaces:**
- Consumes: `event_loop(root, dry_run, watcher, registry, rule_engine, shutdown) -> Result<WatchRegistry>` (`src/app.rs:134`), `Arc<AtomicBool>` shutdown flag.
- Produces: nothing consumed by later tasks.

Covers `src/app.rs:151` (the `PollResult::Readable` arm). Same shape as the existing `event_loop_stops_when_shutdown_flag_is_set`, but a directory is created while the loop runs, and the returned registry must contain it. Timing: after the fd becomes readable, `poll` returns immediately, so a 600 ms wait (> one 500 ms poll window) before setting the flag is comfortably safe.

- [ ] **Step 1: Write the test**

```rust
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
    let handle =
        thread::spawn(move || event_loop(root_for_thread, true, watcher, registry, rules, flag));

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
```

- [ ] **Step 2: Run the test, expect PASS**

Run: `cargo test event_loop_processes_events_before_shutdown`
Expected: `test result: ok. 1 passed`

- [ ] **Step 3: Run the full suite**

Run: `cargo test`
Expected: 58 passed, 0 failed.

- [ ] **Step 4: Commit**

```bash
git add src/app.rs
git commit -m "test(app): cover event_loop processing events via the Readable branch"
```

---

### Task 5: queue-overflow recovery test

**Files:**
- Test: `src/app.rs` (append inside `mod tests`)

**Interfaces:**
- Consumes: `drain_events`, `apply_discovered_paths`, `engine()`.
- Produces: the overflow-forcing pattern (proc read + guard + `max + 100` file creates) that Task 6 repeats.

Covers `src/app.rs:192-198` and `316-322`. Mechanism: with the root watched and nothing draining, each `fs::write` of a new file queues one CREATE event. Past `max_queued_events` the kernel drops events and appends a single `IN_Q_OVERFLOW`. Directories created after that point are invisible to the event stream, so only the overflow's whole-tree rescan can find them. The `> 65536` guard keeps the file count practical; on this machine the tunable is 16384.

- [ ] **Step 1: Write the test**

```rust
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
```

- [ ] **Step 2: Run the test, expect PASS**

Run: `cargo test drain_events_recovers_from_queue_overflow`
Expected: `test result: ok. 1 passed` (takes roughly 1–3 s: ~16.5k file creates plus draining ~16k events).

- [ ] **Step 3: Run the full suite**

Run: `cargo test`
Expected: 59 passed, 0 failed.

- [ ] **Step 4: Commit**

```bash
git add src/app.rs
git commit -m "test(app): cover queue-overflow whole-tree rescan recovery"
```

---

### Task 6: overflow root-vanish test and coverage verification

**Files:**
- Test: `src/app.rs` (append inside `mod tests`)

**Interfaces:**
- Consumes: the overflow-forcing pattern from Task 5 (repeated here in full — do not factor into a helper unless both tests are already merged and the duplication bothers a reviewer; two call sites is below the DRY threshold used in this test module).
- Produces: nothing; final task.

Covers `src/app.rs:324-329` (root missing after an overflow rescan → bail). The root lives one level below the TempDir (matching `drain_events_errors_when_root_is_deleted`) so `remove_dir_all(&root)` leaves the TempDir itself intact. The watch mask has no `DELETE`, so deleting the files adds no events; the root's own `DELETE_SELF` is dropped because the queue is still full. Draining therefore hits `Q_OVERFLOW`, rescans, cannot re-watch the vanished root, and must error. The early drained CREATE events reference deleted files, which also exercises the metadata-failure warn path (`src/app.rs:272-277`).

- [ ] **Step 1: Write the test**

```rust
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
        err.to_string().contains("disappeared during overflow rescan"),
        "got: {err}"
    );
    Ok(())
}
```

- [ ] **Step 2: Run the test, expect PASS**

Run: `cargo test drain_events_errors_when_root_vanishes_during_overflow`
Expected: `test result: ok. 1 passed` (roughly 1–3 s).

- [ ] **Step 3: Run the full suite and clippy**

Run: `cargo test && cargo clippy --all-targets`
Expected: 60 passed, 0 failed; no clippy warnings.

- [ ] **Step 4: Verify coverage improvement**

Run: `cargo llvm-cov --summary-only`
Expected: `app.rs` line coverage at roughly 95% (was 85.34%); total at roughly 95% (was 92.01%). The remaining uncovered lines should be only `run()` (`src/app.rs:18-58`), `poll_inotify` error arms, and warn-only branches — all declared out of scope by the spec.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "test(app): cover root vanishing during overflow rescan"
```
