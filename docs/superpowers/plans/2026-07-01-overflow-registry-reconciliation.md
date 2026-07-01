# Overflow Registry Reconciliation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `Q_OVERFLOW` recovery reconcile the watch registry with reality by rebuilding the watch set from scratch, and add a direct test for the matched-non-directory branch of `plan_entry`.

**Architecture:** Confine changes to the overflow-recovery branch in `src/app.rs` plus one new `WatchRegistry` method in `src/watch.rs`. The normal event-loop path (`add_watch` with its `contains_path` guard) is unchanged. Overflow recovery switches from an additive re-scan to a full rebuild: drop every held watch (kernel + registry), then re-seed from `root` via the existing idempotent `discover_watch_targets` + `apply_discovered_paths`.

**Tech Stack:** Rust (edition 2024), `inotify` 0.10, `libc`, `anyhow`, `log`, `tempfile` (dev).

## Global Constraints

- No new dependencies (spec: "No new dependencies are introduced").
- Keep `cargo clippy --all-targets` warning-free.
- Preserve all existing tests.
- Follow existing patterns: `pub(crate)` visibility, module-local `#[cfg(test)]` tests, `log` macros for diagnostics.
- The normal-path `add_watch` (`src/watch.rs`) and its `contains_path` guard stay unchanged; only overflow recovery changes.
- Correctness relies on standard Linux inotify semantics: `add` on a live inode is idempotent (same descriptor); `add` on a recreated inode yields a fresh descriptor; `remove` of an already-gone watch returns `EINVAL` (ignored).

## File Structure

- `src/watch.rs` — add `WatchRegistry::drain_descriptors()` (empties bookkeeping, returns the descriptors to remove from the kernel). No change to `add_watch`.
- `src/app.rs` — extract `rebuild_watches()`; replace the overflow branch (lines 179-188) with a call to it; add reconciliation and matched-non-directory tests.

---

## Task 1: `WatchRegistry::drain_descriptors` (`src/watch.rs`)

**Files:**
- Modify: `src/watch.rs` (add method inside `impl WatchRegistry`, after `watched_count` around line 97)
- Test: `src/watch.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: existing `WatchRegistry` fields `by_path`, `by_descriptor`; `WatchDescriptor` from `inotify`.
- Produces: `WatchRegistry::drain_descriptors(&mut self) -> Vec<WatchDescriptor>` — clears all bookkeeping and returns every descriptor previously held (order unspecified). Leaves `watched_count() == 0`.

- [ ] **Step 1: Write the failing test**

In `src/watch.rs` `#[cfg(test)] mod tests`, first ensure `use std::fs;` is present at the top of the test module (add it alongside the existing `use` lines if missing), then add:

```rust
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib drain_descriptors_empties_registry_and_returns_all`
Expected: FAIL to compile with "no method named `drain_descriptors`".

- [ ] **Step 3: Add `drain_descriptors`**

In `src/watch.rs`, inside `impl WatchRegistry`, after `watched_count` (around line 97):

```rust
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
```

If the `#[allow(dead_code)]` on `watched_count` was only needed because it had no non-test caller, leave it as-is; `drain_descriptors` gets a real caller in Task 2 so it needs no such attribute. If clippy flags `drain_descriptors` as dead code before Task 2 lands, that is expected and resolved by Task 2 — do not add `#[allow(dead_code)]` to it.

- [ ] **Step 4: Run tests and clippy**

Run: `cargo test --lib && cargo clippy --all-targets`
Expected: PASS (all tests, including the new one). Clippy: warning-free, except a possible transient dead-code note on `drain_descriptors` until Task 2 wires it in — acceptable at this checkpoint.

- [ ] **Step 5: Commit**

```bash
git add src/watch.rs
git commit -m "feat(watch): add WatchRegistry::drain_descriptors for rebuilds

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: Full-rebuild overflow recovery (`src/app.rs`)

**Files:**
- Modify: `src/app.rs` (add `rebuild_watches`; replace overflow branch at lines 179-188)
- Test: `src/app.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `WatchRegistry::drain_descriptors` (Task 1); `discover_watch_targets` (`src/discovery.rs`); `apply_discovered_paths` (`src/app.rs`); `Watches::remove` (`inotify`); `WatchRegistry::insert`, `contains_path`, `watched_count`, and `watch_mask` (`src/watch.rs`, in tests).
- Produces: `fn rebuild_watches(root: &Path, dry_run: bool, watcher: &mut Inotify, registry: &mut WatchRegistry, rules: &RuleEngine) -> Result<()>` (module-private in `src/app.rs`).

**Behavior of `rebuild_watches`:** remove every held watch from the kernel (ignoring `EINVAL` from already-gone inodes) and empty the registry via `drain_descriptors`, then re-seed from `root`. A `discover_watch_targets` failure is logged and swallowed (best-effort recovery); an `apply_discovered_paths` failure propagates.

- [ ] **Step 1: Write the failing test**

In `src/app.rs` `#[cfg(test)] mod tests`, add `use crate::watch::watch_mask;` alongside the existing `use crate::watch::WatchRegistry;` line, then add:

```rust
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib rebuild_watches_reconciles_stale_entries`
Expected: FAIL to compile with "cannot find function `rebuild_watches`".

- [ ] **Step 3: Add `rebuild_watches`**

In `src/app.rs`, after the `apply_discovered_paths` function (around line 233), add:

```rust
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
```

- [ ] **Step 4: Replace the overflow branch with a call to `rebuild_watches`**

In `src/app.rs` `event_loop`, replace the current overflow block (lines 179-188):

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

with:

```rust
        if needs_rescan {
            rebuild_watches(&root, dry_run, &mut watcher, &mut registry, &rule_engine)?;
        }
```

- [ ] **Step 5: Run tests and clippy**

Run: `cargo test --lib && cargo clippy --all-targets`
Expected: PASS (all tests, including the new one). No clippy warnings (the Task 1 dead-code note is now resolved).

- [ ] **Step 6: Manual sanity check (dry-run still works)**

Run: `cargo run -- --dry-run .`
Expected: logs `Watching <cwd>`; Ctrl-C to exit. No panic, no error. Confirms the overflow branch still compiles and wires correctly.

- [ ] **Step 7: Commit**

```bash
git add src/app.rs
git commit -m "fix(watch): rebuild watch set on overflow to reconcile the registry

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Matched-non-directory `plan_entry` test (`src/app.rs`)

**Files:**
- Test: `src/app.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `plan_entry`, `EntryAction` (`src/app.rs`); `Candidate`, `PythonBuildArtifactsRule`, `RuleEngine` (`src/rules.rs`).

- [ ] **Step 1: Write the test**

In `src/app.rs` `#[cfg(test)] mod tests`, extend the existing `use crate::rules::{Candidate, NodeModulesRule};` line to also import `PythonBuildArtifactsRule` (i.e. `use crate::rules::{Candidate, NodeModulesRule, PythonBuildArtifactsRule};`), then add:

```rust
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
```

- [ ] **Step 2: Run the test to verify it passes**

Run: `cargo test --lib plan_entry_applies_matched_non_directory_without_watching`
Expected: PASS. (This is a characterization test for an existing untested branch — a matched non-directory gets the attribute but is not watched. It should pass immediately against the current `plan_entry`.)

- [ ] **Step 3: Run full tests and clippy**

Run: `cargo test --lib && cargo clippy --all-targets`
Expected: PASS, no clippy warnings.

- [ ] **Step 4: Commit**

```bash
git add src/app.rs
git commit -m "test(app): cover plan_entry matched-non-directory branch

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- Overflow recovery reconciles registry (rebuild from scratch) → Task 1 (`drain_descriptors`) + Task 2 (`rebuild_watches` + branch swap). ✓
- Scenario A (deleted) and Scenario B (deleted-then-recreated) → both closed by clear-then-reseed; verified by Task 2's reconciliation test (stale `ghost` pruned; tree fully re-established). ✓
- Matched-non-directory `plan_entry` test → Task 3. ✓
- "No new dependencies" → no `Cargo.toml` changes in any task. ✓
- Normal-path `add_watch` unchanged → only `watch.rs` addition is `drain_descriptors`; `add_watch` untouched. ✓
- README unchanged → no task modifies it. ✓

**Placeholder scan:** No TBD/TODO/vague steps; every code step shows complete code and exact commands. ✓

**Type consistency:** `drain_descriptors(&mut self) -> Vec<WatchDescriptor>` defined in Task 1 and called in Task 2's `rebuild_watches`. `rebuild_watches(root: &Path, dry_run: bool, watcher: &mut Inotify, registry: &mut WatchRegistry, rules: &RuleEngine) -> Result<()>` defined in Task 2 Step 3, called in Task 2 Step 4 (`&root`, `&mut watcher`, `&mut registry`, `&rule_engine`) and in the test (Task 2 Step 1) with matching argument shapes. `EntryAction` fields `apply_ignore`/`watch_dir` used consistently in Task 3. Test imports (`watch_mask`, `PythonBuildArtifactsRule`) added where first used. ✓
