# Scoped Trigger Rescan Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound a rule trigger-file rescan to the trigger file's own directory subtree instead of rebuilding the entire watched root.

**Architecture:** Add a prefix-scoped `WatchRegistry::drain_subtree`, generalize the whole-tree `rebuild_watches` into `rescan_subtree(scope)` built on it, and wire the event loop to record each trigger's parent directory as a rescan scope. The queue-overflow path keeps rescanning the whole tree by passing the root as the scope, so `drain_subtree(root)` reproduces the old behavior exactly.

**Tech Stack:** Rust (edition 2024), `inotify` crate, `anyhow`, `log`; tests use `tempfile` and a real `Inotify` handle.

## Global Constraints

- Rust edition 2024; no new external dependencies.
- No change to the rule set, CLI surface, or on-disk format; marked-path set for any given tree is unchanged.
- All new items stay `pub(crate)` / module-private, matching the crate's visibility.
- Keep the rule abstraction clean: no rule-specific filenames in the event loop (reuse the existing `RuleEngine::is_trigger`).
- `cargo test` stays green and `cargo build` emits no new warnings after every task.
- Scope invariant: a trigger affects verdicts only within the trigger file's own directory subtree (satisfied by `RustTargetRule`, which consults a sibling `Cargo.toml`).

---

### Task 1: `WatchRegistry::drain_subtree`

Add the prefix-scoped drain. `drain_descriptors` is left in place for now (still used by `rebuild_watches`) and removed in Task 2, so the crate keeps compiling.

**Files:**
- Modify: `src/watch.rs` (add method to the `impl WatchRegistry` block near `drain_descriptors`, add tests in the `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: existing `WatchRegistry` fields `by_path: HashMap<PathBuf, WatchDescriptor>`, `by_descriptor: HashMap<WatchDescriptor, PathBuf>`.
- Produces: `pub(crate) fn drain_subtree(&mut self, prefix: &Path) -> Vec<WatchDescriptor>` — removes every watched path at or under `prefix` (inclusive) and returns their descriptors.

- [ ] **Step 1: Write the failing tests**

Add these three tests inside `mod tests` in `src/watch.rs` (a `use std::collections::HashSet;` goes at the top of the test that needs it):

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib watch::tests::drain_subtree 2>&1 | tail -20`
Expected: compile error `no method named `drain_subtree` found`.

- [ ] **Step 3: Implement `drain_subtree`**

Add to the `impl WatchRegistry` block in `src/watch.rs`, directly above `drain_descriptors`:

```rust
    /// Drop bookkeeping for every watched path at or under `prefix` (inclusive)
    /// and return their descriptors so the caller can remove them from the
    /// kernel. Rebuilds a bounded portion of the watch set: a trigger's parent
    /// subtree, or the whole tree when `prefix` is the watched root.
    ///
    /// `Path::starts_with` compares whole components, so `/a/bc` is not a child
    /// of `/a/b`; it also returns true for equality, so `prefix` itself drains.
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib watch::tests 2>&1 | tail -20`
Expected: all `watch::tests` pass (existing + 3 new), 0 failed.

- [ ] **Step 5: Commit**

```bash
git add src/watch.rs
git commit -m "feat(watch): add WatchRegistry::drain_subtree for scoped rebuilds"
```

---

### Task 2: Generalize `rebuild_watches` into `rescan_subtree`

Replace the whole-tree `rebuild_watches` with a scope-parameterized `rescan_subtree`, point the overflow path at it (`scope = root`), remove the now-unused `drain_descriptors` and its test, and add behavior + isolation tests.

**Files:**
- Modify: `src/app.rs` (replace `rebuild_watches`; update its overflow call site; add/rename tests)
- Modify: `src/watch.rs` (delete `drain_descriptors` and its test `drain_descriptors_empties_registry_and_returns_all`)

**Interfaces:**
- Consumes: `WatchRegistry::drain_subtree` (Task 1), existing `discover_watch_targets`, `apply_discovered_paths`.
- Produces: `fn rescan_subtree(scope: &Path, dry_run: bool, watcher: &mut Inotify, registry: &mut WatchRegistry, rules: &RuleEngine) -> Result<()>` — the sole rescan primitive, used by both overflow (Task 2) and trigger (Task 3) paths.

- [ ] **Step 1: Write the failing / migrated tests**

In `src/app.rs` `mod tests`, replace the existing `rebuild_watches_reconciles_stale_entries` test's single call `rebuild_watches(temp.path(), true, &mut watcher, &mut registry, &rules)?;` with `rescan_subtree(temp.path(), true, &mut watcher, &mut registry, &rules)?;` (leave the rest of that test unchanged), and add these two tests:

```rust
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
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib app::tests 2>&1 | tail -20`
Expected: compile error `cannot find function `rescan_subtree``.

- [ ] **Step 3: Replace `rebuild_watches` with `rescan_subtree`**

In `src/app.rs`, replace the entire `rebuild_watches` function (the `/// Tear down every held watch ...` doc comment through its closing brace) with:

```rust
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
```

- [ ] **Step 4: Point the overflow call site at `rescan_subtree`**

In `src/app.rs` `event_loop`, change the batch-end block

```rust
        if needs_rescan {
            rebuild_watches(&root, dry_run, &mut watcher, &mut registry, &rule_engine)?;
        }
```

to

```rust
        if needs_rescan {
            rescan_subtree(&root, dry_run, &mut watcher, &mut registry, &rule_engine)?;
        }
```

- [ ] **Step 5: Remove the now-unused `drain_descriptors`**

In `src/watch.rs`, delete the `drain_descriptors` method (its `/// Drop all bookkeeping ...` doc comment through its closing brace) and delete the test `drain_descriptors_empties_registry_and_returns_all` (whole `#[test] fn ... { ... }`). Its parity coverage now lives in `drain_subtree_with_root_prefix_empties_registry` (Task 1).

- [ ] **Step 6: Run the full suite to verify it passes**

Run: `cargo test 2>&1 | tail -20`
Expected: all tests pass, 0 failed. Also run `cargo build 2>&1 | tail -5` — no warnings (no dead-code warning for `drain_descriptors`).

- [ ] **Step 7: Commit**

```bash
git add src/app.rs src/watch.rs
git commit -m "refactor(app): generalize rebuild_watches into scoped rescan_subtree"
```

---

### Task 3: Scope trigger rescans in the event loop

Record each trigger file's parent directory as a rescan scope instead of flagging a whole-tree rescan; dispatch one `rescan_subtree` per distinct scope at batch end. Overflow still supersedes with a whole-tree rescan.

**Files:**
- Modify: `src/app.rs` (`event_loop`: add `rescan_scopes`, change the trigger branch and the batch-end dispatch)

**Interfaces:**
- Consumes: `rescan_subtree` (Task 2), `RuleEngine::is_trigger` (existing), `registry.path_for` (existing).
- Produces: no new public interface; internal control-flow change only.

**Note on testing:** `event_loop` is an infinite blocking loop with no unit-testable seam, so this task adds no new test. The scoped-rescan behavior it invokes is already covered by `rescan_subtree_reconciles_newly_matched_sibling` and `rescan_subtree_leaves_out_of_scope_watches_intact` (Task 2), and trigger detection by the existing `RuleEngine::is_trigger` tests. Verification is the full suite staying green plus a warning-free build.

- [ ] **Step 1: Add the `HashSet` import**

In `src/app.rs`, add `use std::collections::HashSet;` alongside the existing `use std::path::{Path, PathBuf};` (a new line right after it).

- [ ] **Step 2: Declare the per-batch scope set**

In `event_loop`, immediately after `let mut needs_rescan = false;`, add:

```rust
        // Distinct subtrees to rescan because a rule trigger file appeared in
        // them. Deduplicated so repeated triggers in one batch rescan once.
        let mut rescan_scopes: HashSet<PathBuf> = HashSet::new();
```

- [ ] **Step 3: Record the scope instead of a whole-tree flag**

Replace the trigger branch

```rust
            if rule_engine.is_trigger(name) {
                info!(
                    "Trigger file {} created; rescanning {} to reconcile dependent rules",
                    parent_dir.join(name).display(),
                    root.display()
                );
                needs_rescan = true;
            }
```

with

```rust
            if rule_engine.is_trigger(name) {
                info!(
                    "Trigger file {} created; rescanning {} to reconcile dependent rules",
                    parent_dir.join(name).display(),
                    parent_dir.display()
                );
                rescan_scopes.insert(parent_dir.to_path_buf());
            }
```

- [ ] **Step 4: Dispatch scoped rescans at batch end**

Replace the batch-end block

```rust
        if needs_rescan {
            rescan_subtree(&root, dry_run, &mut watcher, &mut registry, &rule_engine)?;
        }
```

with

```rust
        if needs_rescan {
            // Overflow dropped events: no descriptor is trustworthy, so rebuild
            // the whole tree. This supersedes any recorded scopes (all under root).
            rescan_subtree(&root, dry_run, &mut watcher, &mut registry, &rule_engine)?;
        } else {
            for scope in &rescan_scopes {
                rescan_subtree(scope, dry_run, &mut watcher, &mut registry, &rule_engine)?;
            }
        }
```

- [ ] **Step 5: Verify the full suite and a clean build**

Run: `cargo test 2>&1 | tail -20`
Expected: all tests pass, 0 failed.
Run: `cargo build 2>&1 | tail -5`
Expected: no warnings (in particular, `needs_rescan` is still read and `rescan_scopes` is used).

- [ ] **Step 6: Commit**

```bash
git add src/app.rs
git commit -m "feat(app): scope trigger rescans to the trigger's directory"
```

---

### Task 4: Document the scoped rescan and its invariant

Record the scope invariant at the rule extension point and update the README's runtime description.

**Files:**
- Modify: `src/rules.rs` (`Rule::triggers` doc comment)
- Modify: `README.md` ("How it works" item 4)

**Interfaces:** none (documentation only).

- [ ] **Step 1: Expand the `triggers` doc comment**

In `src/rules.rs`, replace the doc comment on the `triggers` trait method

```rust
    /// Filenames whose creation may change this rule's verdict for a sibling.
    /// Creating any of these under a watched directory schedules a rescan.
    fn triggers(&self) -> &'static [&'static str] {
        &[]
    }
```

with

```rust
    /// Filenames whose creation may change this rule's verdict for a sibling.
    /// Creating any of these under a watched directory schedules a rescan of
    /// that directory's subtree.
    ///
    /// Scope invariant: a trigger is assumed to affect verdicts only within the
    /// trigger file's own directory subtree, so a scoped rescan fully
    /// reconciles it. `RustTargetRule` satisfies this — it consults a sibling
    /// `Cargo.toml`. A rule whose trigger has non-local effects would need a
    /// wider rescan scope than the event loop currently uses.
    fn triggers(&self) -> &'static [&'static str] {
        &[]
    }
```

- [ ] **Step 2: Update the README runtime description**

In `README.md`, in the "How it works" section, replace the sentence

```
Creating a rule's dependency file (e.g. a `Cargo.toml` next to an existing `target`) also triggers a re-scan so order-dependent rules are reconciled without a restart.
```

with

```
Creating a rule's dependency file (e.g. a `Cargo.toml` next to an existing `target`) triggers a re-scan of just that file's directory subtree, reconciling order-dependent rules without a restart or a whole-tree walk.
```

- [ ] **Step 3: Verify docs build and tests still pass**

Run: `cargo test --doc 2>&1 | tail -5` (doc comment is not a doctest, but confirms nothing broke) and `cargo build 2>&1 | tail -5`
Expected: success, no warnings.

- [ ] **Step 4: Commit**

```bash
git add src/rules.rs README.md
git commit -m "docs: describe scoped trigger rescan and its scope invariant"
```

---

## Self-Review

**Spec coverage:**
- `drain_subtree` (spec §Design) → Task 1.
- `rescan_subtree` generalization + overflow parity + `drain_descriptors` removal (spec §Design, §rescan_subtree) → Task 2.
- Event-loop scope recording + batch-end dispatch + `HashSet` dedup + overflow supersession (spec §Event loop wiring) → Task 3.
- Scope invariant documentation + README update (spec §Scope invariant, §Rollout) → Task 4.
- Testing plan (spec §Testing): `drain_subtree` boundary/parity tests → Task 1; `rescan_subtree` reconciliation + scope isolation + overflow migration → Task 2; whole-suite green → every task.
- Non-goals (debounce, ancestor subsumption, un-marking, non-local triggers) are not implemented — correct.

**Placeholder scan:** No TBD/TODO; every code step shows complete code and exact commands. Task 3's "no new test" is justified explicitly (untestable infinite loop) with the covering tests named.

**Type consistency:** `rescan_subtree(scope: &Path, dry_run: bool, watcher: &mut Inotify, registry: &mut WatchRegistry, rules: &RuleEngine)` is used identically in Task 2 (definition, overflow call site, tests) and Task 3 (scoped call site). `drain_subtree(&mut self, prefix: &Path) -> Vec<WatchDescriptor>` is used identically in Task 1 (definition/tests) and Task 2 (inside `rescan_subtree`). `rescan_scopes: HashSet<PathBuf>` with `.insert(parent_dir.to_path_buf())` matches its declaration.
