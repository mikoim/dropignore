# MOVE_SELF Subtree Cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop `MOVE_SELF` from leaking descendant watches/registry entries, and fail fast when the watched root itself is moved or deleted.

**Architecture:** All changes live in `drain_events` (`src/app.rs`). The combined `DELETE_SELF || MOVE_SELF` branch is split: `MOVE_SELF` now schedules the descriptor's path into the existing `rescan_scopes` set (post-loop, `rescan_subtree` drains the stale subtree, removes its kernel watches, and reseeds from disk); `DELETE_SELF` keeps `remove_by_descriptor`. Both branches gain a guard that `anyhow::bail!`s when the affected descriptor is the watched root, propagating a non-zero exit through `event_loop` → `run`.

**Tech Stack:** Rust (edition 2024), `inotify` 0.10, `anyhow`, `tempfile` (dev). Spec: `docs/superpowers/specs/2026-07-02-move-self-subtree-cleanup-design.md`.

## Global Constraints

- No new dependencies (project rule: keep dependencies minimal).
- All commands run from the repo root `/home/dev/src/dropignore`.
- Before every commit: `cargo fmt` and `cargo clippy --all-targets -- -D warnings` must pass, `cargo test` must be green.
- Commit messages: conventional commits, ending with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- Tests follow the existing `src/app.rs` test style: `TempDir` + real `Inotify` + dry-run (`true`), event-wait loops with a 5-second deadline and 20 ms sleeps (see `drain_events_registers_new_dir_and_skips_ignored`).

---

### Task 1: MOVE_SELF schedules a scoped rescan

**Files:**
- Modify: `src/app.rs:200-207` (the `DELETE_SELF || MOVE_SELF` branch inside `drain_events`)
- Test: `src/app.rs` (tests module at the bottom of the same file)

**Interfaces:**
- Consumes: existing `rescan_scopes: HashSet<PathBuf>` local in `drain_events`, `WatchRegistry::path_for(&WatchDescriptor) -> Option<&PathBuf>`, `rescan_subtree` (already called post-loop for every scope).
- Produces: `drain_events` behavior — after a watched directory is moved (in-tree or out-of-tree), no registry entry remains under its old path and its subtree's kernel watches are removed. Task 2 edits the two branches this task creates (`MOVE_SELF` branch and `DELETE_SELF` branch), so keep them as two separate `if` blocks in this order.

- [ ] **Step 1: Write the two failing tests**

Add to the `tests` module in `src/app.rs` (after `drain_events_registers_new_dir_and_skips_ignored`):

```rust
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
        assert!(registry.contains_path(&a_x), "a/x watched before the rename");

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
        assert!(registry.contains_path(&b_x), "descendant must follow the rename");
        assert!(!registry.contains_path(&a), "old path must be pruned");
        assert!(!registry.contains_path(&a_x), "old descendant must be pruned");
        let fresh = discover_watch_targets(&root, &rules)?;
        assert_eq!(
            registry.watched_count(),
            fresh.watchers.len(),
            "registry must match a fresh discovery of the root"
        );
        Ok(())
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test drain_events_prunes_subtree_moved_out_of_tree drain_events_reconciles_in_tree_rename`

Expected: both FAIL on the descendant assertions (`descendant of moved dir must be pruned` / `old descendant must be pruned`), because today `MOVE_SELF` removes only the moved directory's own entry. Each failing test takes ~5 s (deadline loop).

- [ ] **Step 3: Split the MOVE_SELF/DELETE_SELF branch**

In `drain_events` (`src/app.rs`), replace:

```rust
        // Remove bookkeeping for directories that disappeared or were moved,
        // so stale mappings can't resolve later events to the wrong path.
        if event.mask.contains(EventMask::DELETE_SELF) || event.mask.contains(EventMask::MOVE_SELF)
        {
            registry.remove_by_descriptor(&event.wd);
            continue;
        }
```

with:

```rust
        // A moved directory keeps its kernel watch, and MOVE_SELF fires only
        // for the moved directory itself, so its descendants would keep stale
        // registry entries and live kernel watches. Schedule a scoped rescan
        // of the old path: the drain sweeps the whole stale subtree, and the
        // reseed is a no-op when the path is gone (moved out of tree) or
        // re-registers it when the descriptor was reused for a live path.
        // An unknown descriptor means an earlier scope already drained it.
        if event.mask.contains(EventMask::MOVE_SELF) {
            if let Some(path) = registry.path_for(&event.wd) {
                rescan_scopes.insert(path.clone());
            }
            continue;
        }

        // Deletion needs no rescan: the kernel auto-removes the watch, and a
        // recursive delete fires DELETE_SELF for every directory, so dropping
        // this one entry leaves nothing stale behind.
        if event.mask.contains(EventMask::DELETE_SELF) {
            registry.remove_by_descriptor(&event.wd);
            continue;
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test drain_events_prunes_subtree_moved_out_of_tree drain_events_reconciles_in_tree_rename`
Expected: both PASS.

Run: `cargo test`
Expected: all tests PASS (in particular the existing `drain_events_registers_new_dir_and_skips_ignored` and `rescan_subtree_*` tests must stay green).

- [ ] **Step 5: Format, lint, commit**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
git add src/app.rs
git commit -m "fix(watch): reconcile subtree bookkeeping on MOVE_SELF via scoped rescan

MOVE_SELF fires only for the moved directory itself, so descendants kept
stale registry entries and live kernel watches after an out-of-tree move.
Reuse the trigger-file rescan machinery: drain the old subtree and reseed
from disk, which also self-heals a descriptor reused across batches.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: Fail fast when the watched root is moved or deleted

**Files:**
- Modify: `src/app.rs` (the two branches created in Task 1 inside `drain_events`)
- Test: `src/app.rs` (tests module at the bottom of the same file)

**Interfaces:**
- Consumes: the separate `MOVE_SELF` and `DELETE_SELF` branches produced by Task 1; `drain_events`'s existing `root: &Path` parameter.
- Produces: `drain_events` returns `Err` when the root's own descriptor receives `MOVE_SELF` (message contains `was moved or renamed`) or `DELETE_SELF` (message contains `was deleted`). The error propagates through `event_loop` → `run` → non-zero process exit; no signature changes.

- [ ] **Step 1: Write the two failing tests**

Add to the `tests` module in `src/app.rs`:

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test drain_events_errors_when_root_is_moved drain_events_errors_when_root_is_deleted`

Expected: both FAIL at `expect_err` (today `drain_events` silently drops the root's bookkeeping and returns `Ok`). Each failing test takes ~5 s (deadline loop).

- [ ] **Step 3: Add the root guards**

In `drain_events` (`src/app.rs`), replace the two branches from Task 1 with:

```rust
        // A moved directory keeps its kernel watch, and MOVE_SELF fires only
        // for the moved directory itself, so its descendants would keep stale
        // registry entries and live kernel watches. Schedule a scoped rescan
        // of the old path: the drain sweeps the whole stale subtree, and the
        // reseed is a no-op when the path is gone (moved out of tree) or
        // re-registers it when the descriptor was reused for a live path.
        // An unknown descriptor means an earlier scope already drained it.
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
            if let Some(path) = registry.path_for(&event.wd) {
                if path.as_path() == root {
                    anyhow::bail!("Watched root {} was deleted", root.display());
                }
            }
            registry.remove_by_descriptor(&event.wd);
            continue;
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test drain_events_errors_when_root_is_moved drain_events_errors_when_root_is_deleted`
Expected: both PASS.

Run: `cargo test`
Expected: all tests PASS (Task 1's move tests and the pre-existing suite must stay green — non-root `MOVE_SELF`/`DELETE_SELF` behavior is unchanged).

- [ ] **Step 5: Format, lint, commit**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
git add src/app.rs
git commit -m "feat(watch): fail fast when the watched root is moved or deleted

Previously the watcher silently dropped the root's bookkeeping and idled
forever with no watches, looking alive to a supervisor. Bail out with a
descriptive error instead so the process exits non-zero and restarting
becomes the supervisor's decision.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```
