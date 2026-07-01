# Watcher Robustness (Symlink + Overflow) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the inotify event loop treat symlinks consistently with discovery (skip them) and recover from inotify queue overflow by re-scanning the watch root.

**Architecture:** All changes are confined to `src/app.rs`, with two tiny support additions in `src/rules.rs` (a `Candidate::is_symlink` accessor) and `src/watch.rs` (a `WatchRegistry::watched_count` accessor for tests). The per-entry decision is extracted from the event loop into a pure helper (`plan_entry`) so it can be unit-tested without a live inotify instance. Overflow recovery reuses the existing idempotent `discover_watch_targets` + `apply_discovered_paths` path.

**Tech Stack:** Rust (edition 2024), `inotify` 0.10, `libc`, `anyhow`, `log`, `tempfile` (dev).

## Global Constraints

- No new dependencies (spec: "No new dependencies are introduced").
- Keep `cargo clippy --all-targets` warning-free.
- Preserve all existing tests.
- Follow existing patterns: `pub(crate)` visibility, module-local `#[cfg(test)]` tests, `log` macros for diagnostics.
- Symlinks are skipped wholesale in the event loop (no rule evaluation, no attribute, no watch, no recursion) — mirroring `src/discovery.rs`.
- Overflow recovery is idempotent and collapses multiple `Q_OVERFLOW` events in one batch into a single re-scan.

---

## File Structure

- `src/rules.rs` — add `Candidate::is_symlink()` accessor (one method). No behavior change to rules.
- `src/watch.rs` — add `WatchRegistry::watched_count()` accessor (returns `by_path.len()`) to support the idempotency test.
- `src/app.rs` — extract `plan_entry` decision helper; switch the event loop to `fs::symlink_metadata`; add `Q_OVERFLOW` detection and a batch-collapsed re-scan; thread `root: PathBuf` into `event_loop`. Add unit tests for both.

---

## Task 1: Symlink consistency in the event loop (A-1)

**Files:**
- Modify: `src/rules.rs` (add `Candidate::is_symlink`, near `is_dir` at lines 12-20)
- Modify: `src/app.rs` (add `EntryAction` + `plan_entry`; rewrite the decision block at lines 90-123; switch `fs::metadata` → `fs::symlink_metadata`)
- Test: `src/app.rs` (`#[cfg(test)]` module)

**Interfaces:**
- Produces:
  - `Candidate::is_symlink(&self) -> bool` in `src/rules.rs`.
  - `struct EntryAction { apply_ignore: bool, watch_dir: bool }` (module-private in `src/app.rs`).
  - `fn plan_entry(candidate: &Candidate<'_>, rules: &RuleEngine) -> EntryAction` in `src/app.rs`.
- Consumes: existing `Candidate`, `RuleEngine`, `MatchAction` from `src/rules.rs`.

**Behavior of `plan_entry`:**
- If `candidate.is_symlink()` → `EntryAction { apply_ignore: false, watch_dir: false }`.
- Otherwise evaluate rules: `apply_ignore = action.set_dropbox_ignore`; `watch_dir = candidate.is_dir() && !action.skip_descendants` (with `skip_descendants` treated as `false` when no rule matches). This reproduces the current loop logic exactly (a matched `skip_descendants` directory is applied-but-not-watched; a matched non-directory is applied; an unmatched directory is watched).

- [ ] **Step 1: Add the `is_symlink` accessor to `Candidate`**

In `src/rules.rs`, inside `impl Candidate<'_>` (after `is_dir`, around line 15):

```rust
    pub(crate) fn is_symlink(&self) -> bool {
        self.metadata.file_type().is_symlink()
    }
```

- [ ] **Step 2: Write the failing test for `plan_entry`**

In `src/app.rs`, extend the `#[cfg(test)] mod tests` block with:

```rust
    use crate::rules::{Candidate, NodeModulesRule};
    use std::os::unix::fs::symlink;
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
```

Note: `use anyhow::Result;` may be needed in the test module — add it alongside the other test `use` lines if not already present.

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --lib plan_entry`
Expected: FAIL to compile with "cannot find function `plan_entry`" / "cannot find type `EntryAction`".

- [ ] **Step 4: Add `EntryAction` and `plan_entry`**

In `src/app.rs`, above `event_loop` (after the `run` function), add:

```rust
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
```

- [ ] **Step 5: Switch the event loop to `symlink_metadata` and use `plan_entry`**

In `src/app.rs`, replace the metadata read at line 90 (`let metadata = match fs::metadata(&full_path) {`) with `let metadata = match fs::symlink_metadata(&full_path) {`.

Then replace the decision block (current lines 101-123, from `let candidate = Candidate {` through the `if candidate.is_dir() { pending_directories.push(full_path); }`) with:

```rust
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
```

- [ ] **Step 6: Update the module `use` list if needed**

Ensure `src/app.rs` still imports `Candidate` (it already imports it from `crate::rules`). `EventMask` and other imports are unchanged in this task.

- [ ] **Step 7: Run tests and clippy**

Run: `cargo test --lib && cargo clippy --all-targets`
Expected: PASS (all tests, including the three new ones), no clippy warnings.

- [ ] **Step 8: Commit**

```bash
git add src/rules.rs src/app.rs
git commit -m "fix(watch): skip symlinks in the event loop like discovery

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: Queue-overflow recovery (A-2)

**Files:**
- Modify: `src/watch.rs` (add `WatchRegistry::watched_count`)
- Modify: `src/app.rs` (thread `root` into `event_loop`; add `Q_OVERFLOW` branch + batch-collapsed re-scan)
- Test: `src/app.rs` (`#[cfg(test)]` module)

**Interfaces:**
- Consumes: `discover_watch_targets` (`src/discovery.rs`), `apply_discovered_paths` (`src/app.rs`), `add_watch` (`src/watch.rs`), `EventMask::Q_OVERFLOW` (`inotify`).
- Produces:
  - `WatchRegistry::watched_count(&self) -> usize` in `src/watch.rs`.
  - `event_loop` gains a leading `root: PathBuf` parameter: `fn event_loop(root: PathBuf, dry_run: bool, watcher: Inotify, registry: WatchRegistry, rule_engine: RuleEngine) -> Result<()>`.

- [ ] **Step 1: Add `watched_count` to `WatchRegistry`**

In `src/watch.rs`, inside `impl WatchRegistry` (after `contains_path`, around line 90):

```rust
    /// Number of distinct paths currently watched. Used to assert re-scan
    /// idempotency in tests.
    pub(crate) fn watched_count(&self) -> usize {
        self.by_path.len()
    }
```

- [ ] **Step 2: Write the failing idempotency test**

In `src/app.rs` `#[cfg(test)] mod tests`, add:

```rust
    use crate::discovery::discover_watch_targets;
    use crate::watch::WatchRegistry;
    use inotify::Inotify;

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
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --lib rescan_is_idempotent`
Expected: FAIL to compile with "no method named `watched_count`" until Step 1 is in place; if Step 1 compiled, the test should PASS already (it validates existing idempotent behavior). If it does pass here, that is acceptable — this test guards the property the overflow branch relies on. Proceed regardless.

- [ ] **Step 4: Thread `root` into `event_loop`**

In `src/app.rs`, change the `run` call site (currently `event_loop(args.dry_run, watcher, registry, rule_engine)` at line 37) to:

```rust
    event_loop(root, args.dry_run, watcher, registry, rule_engine)
```

`root` is the canonicalized `PathBuf` already bound at the top of `run`; it is moved in here and not used afterward.

Change the `event_loop` signature (line 41) to:

```rust
fn event_loop(
    root: PathBuf,
    dry_run: bool,
    mut watcher: Inotify,
    mut registry: WatchRegistry,
    rule_engine: RuleEngine,
) -> Result<()> {
```

- [ ] **Step 5: Add the overflow branch and re-scan**

In `src/app.rs` `event_loop`, add a re-scan flag next to `pending_directories` (after line 56):

```rust
        let mut needs_rescan = false;
```

At the very start of the `for event in events {` body (before the `DELETE_SELF`/`MOVE_SELF` check at line 61), add:

```rust
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
```

After the `pending_directories` processing loop (after line 141, before the closing brace of the outer `loop`), add:

```rust
        if needs_rescan {
            match discover_watch_targets(&root, &rule_engine) {
                Ok(discovered) => {
                    apply_discovered_paths(discovered, dry_run, &mut watcher, &mut registry)?;
                }
                Err(err) => {
                    warn!("Rescan after overflow failed for {}: {err}", root.display());
                }
            }
        }
```

- [ ] **Step 6: Run tests and clippy**

Run: `cargo test --lib && cargo clippy --all-targets`
Expected: PASS (all tests), no clippy warnings.

- [ ] **Step 7: Manual sanity check (dry-run still works)**

Run: `cargo run -- --dry-run .`
Expected: logs `Watching <cwd>`; then Ctrl-C to exit. No panic, no error. (This confirms the new `event_loop` signature and root threading are wired correctly.)

- [ ] **Step 8: Commit**

```bash
git add src/watch.rs src/app.rs
git commit -m "fix(watch): rescan from root on inotify queue overflow

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Documentation note

**Files:**
- Modify: `README.md` (the "How it works" section)

**Interfaces:** none.

- [ ] **Step 1: Document the new behavior**

In `README.md`, under "How it works", append a fourth list item after the existing step 3:

```markdown
4. Skips symlinks (matching the initial walk) and, if the inotify event queue overflows, re-scans from the root so dropped events cannot leave paths unmarked.
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs(readme): note symlink skip and overflow rescan

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- A-1 (symlink consistency) → Task 1 (`symlink_metadata` + `plan_entry` skip). ✓
- A-2 (overflow recovery) → Task 2 (`Q_OVERFLOW` branch, `root` threading, idempotent re-scan). ✓
- Testability (extracted helper, idempotency assertion) → Task 1 Step 4 (`plan_entry`), Task 2 Step 2 (`rescan_is_idempotent`). ✓
- "No new dependencies" → no `Cargo.toml` changes in any task. ✓
- Optional README note → Task 3. ✓

**Placeholder scan:** No TBD/TODO/vague steps; every code step shows complete code and exact commands. ✓

**Type consistency:** `plan_entry`/`EntryAction` field names (`apply_ignore`, `watch_dir`) used consistently between definition (Task 1 Step 4) and tests (Task 1 Step 2). `event_loop` signature updated at both call site and definition (Task 2 Step 4). `watched_count` defined (Task 2 Step 1) before use (Task 2 Step 2). ✓
