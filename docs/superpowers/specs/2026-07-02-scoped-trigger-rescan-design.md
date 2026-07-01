# Scoped subtree rescan for rule trigger files

Date: 2026-07-02

## Summary

Bound the work done when a rule's trigger file (e.g. `Cargo.toml`) is created.
Today, creating any trigger name under a watched directory schedules a
whole-tree `rebuild_watches(root)`. This design replaces that with a rescan
scoped to the trigger file's own directory subtree, so the cost is proportional
to the affected project rather than the entire watched root.

This implements the item deferred in
`2026-07-02-marking-consistency-design.md` (non-goal "Scoped subtree rebuild /
`WatchRegistry::drain_subtree`"), whose concrete revisit signal was: editors
that save a trigger file via write-temp-then-rename emit `IN_MOVED_TO` with the
trigger name, so each save of a watched project's `Cargo.toml` fired a full-tree
rebuild. Inside a large Dropbox tree this exceeds the "low trigger frequency"
assumption and routinely exercised the teardown-before-reseed window. That spec
supersedes its own non-goal here.

This is a performance/robustness change only: no new features, no new external
dependencies, no change to the rule set, CLI, or on-disk format. The overflow
path continues to rescan the whole tree.

## Goals

- Creating a trigger file rescans only that file's directory subtree, not the
  whole watched root.
- The reconciled state is identical to what a whole-tree rescan would produce
  for the affected subtree (a pre-existing sibling that now matches gets marked
  and its subtree stops being watched).
- The teardown-before-reseed hazard window is confined to the affected subtree:
  for any scope below the root, the root watch survives, so discovery failure
  cannot leave the loop blocked forever.
- Unify the overflow rescan and the trigger rescan on one primitive.

## Non-goals (YAGNI)

- Time-based debounce of rescans across event batches. Each batch still triggers
  at most one rescan per distinct scope; that is already bounded to a project
  subtree.
- Ancestor subsumption of scopes within a batch (collapsing `/a/b` into a
  scheduled `/a`). Redundant nested rescans are idempotent; exact deduplication
  via a `HashSet` is enough.
- Un-marking a path that stops matching (e.g. `target` after its `Cargo.toml` is
  deleted). Separate concern, unchanged.
- Non-local trigger effects. Triggers are assumed to affect only their own
  directory subtree (see invariant below).
- Config-file rules, signal handling, cross-platform support.

## Scope invariant

**A rule's trigger is assumed to affect rule verdicts only within the trigger
file's own directory subtree.** This holds for the only trigger-bearing rule,
`RustTargetRule`: its `matches` consults `candidate.path.parent().join(
"Cargo.toml")`, so a created `Cargo.toml` can only change the verdict of a
sibling `target` in the same directory. Rescanning the trigger's parent subtree
therefore fully reconciles it.

A future rule whose trigger has non-local effects would need a wider scope; this
assumption must be documented next to `Rule::triggers()` so the constraint is
visible at the extension point.

## Design

### `WatchRegistry::drain_subtree`

Replace the whole-registry `drain_descriptors` with a prefix-scoped drain:

```rust
/// Drop bookkeeping for every watched path at or under `prefix` (inclusive)
/// and return their descriptors so the caller can remove them from the kernel.
/// Rebuilds a bounded portion of the watch set: a trigger's parent subtree, or
/// the whole tree when `prefix` is the watched root.
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

`Path::starts_with` compares whole components, so `/a/bc` is not treated as a
child of `/a/b` (no false prefix matches), and it returns true for equality, so
`prefix` itself is drained. Because every watched path lives under the
canonicalized root, `drain_subtree(root)` drains everything — exactly what
`drain_descriptors` did — so the old method and its dedicated test are removed.

### `rescan_subtree` (generalizes `rebuild_watches`)

```rust
/// Tear down every watch at or under `scope` and rebuild that portion of the
/// watch set from the current tree. Used after a queue overflow (scope = root),
/// where dropped events mean no descriptor can be trusted, and when a rule's
/// trigger file appears (scope = the trigger's parent), where a pre-existing
/// sibling may have just started matching.
fn rescan_subtree(
    scope: &Path,
    dry_run: bool,
    watcher: &mut Inotify,
    registry: &mut WatchRegistry,
    rules: &RuleEngine,
) -> Result<()> {
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

Teardown-before-reseed hazard: unchanged for `scope == root` (the overflow
path, already accepted). For any `scope` below the root, the root watch is not
drained, so even if `discover_watch_targets` were made fallible and failed, the
loop keeps receiving events and cannot block forever — strictly safer than the
whole-tree-only rebuild.

### Event loop wiring

`event_loop` keeps `needs_rescan: bool` for overflow and adds
`rescan_scopes: HashSet<PathBuf>` for triggers.

On trigger detection, record the scope instead of flagging a whole-tree rescan:

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

The check still runs before the `symlink_metadata` read, so a transient stat
failure on the trigger file still schedules the rescan. Normal handling of the
created entry falls through unchanged.

At batch end, after the `pending_directories` loop:

```rust
if needs_rescan {
    // Overflow: no descriptor is trustworthy, so rebuild the whole tree. This
    // supersedes any recorded scopes (all lie under root).
    rescan_subtree(&root, dry_run, &mut watcher, &mut registry, &rule_engine)?;
} else {
    for scope in &rescan_scopes {
        rescan_subtree(scope, dry_run, &mut watcher, &mut registry, &rule_engine)?;
    }
}
```

`HashSet` deduplicates repeated triggers in the same batch. Nested scopes are
left as-is (idempotent).

## Correctness walk-through

Pre-existing `proj/target` and `proj/target/debug` are watched; no
`Cargo.toml`. User creates `proj/Cargo.toml`:

1. `CREATE Cargo.toml` in `proj` → `is_trigger` true → scope `proj` recorded.
2. Fall-through: `symlink_metadata` + `plan_entry` on the `Cargo.toml` file → no
   rule matches a plain file named `Cargo.toml` → no-op.
3. Batch end, no overflow → `rescan_subtree(proj)`:
   - `drain_subtree(proj)` removes `proj`, `target`, `target/debug`, `src`, …
   - `discover_watch_targets(proj)` re-evaluates: `target` now matches
     `RustTargetRule` → pushed to `matches`, `skip_descendants` prunes
     `target/debug`; `src` re-watched.
   - `apply_discovered_paths` marks `target` ignored and re-adds `proj` + `src`.

Result: `target` marked and its subtree no longer watched — reconciled, with
work bounded to `proj`. An editor's write-temp-then-rename save (`IN_MOVED_TO`
of `Cargo.toml`) triggers the same bounded rescan instead of a whole-tree one.

## Error handling

- `drain_subtree` is infallible bookkeeping.
- `rescan_subtree` reuses `discover_watch_targets` + `apply_discovered_paths`;
  the discovery `Err` branch warns without aborting the loop (discovery is
  currently infallible in practice, so this is defensive).
- `add_watch` and `apply_dropbox_ignore` idempotency is unchanged, so converging
  the overflow and trigger paths on `rescan_subtree` is safe.

## Testing

### `WatchRegistry::drain_subtree`

- Drains `prefix` and all descendants, returns their descriptors, and leaves
  sibling and ancestor entries intact.
- Component-boundary safety: with `prefix = /a/b`, a watched `/a/bc` is not
  drained.
- `drain_subtree(root)` empties the registry and returns every descriptor
  (parity with the removed `drain_descriptors`).

### `rescan_subtree` (real inotify + tempdir, `dry_run = true`)

- Pre-create `proj/target/debug` (no `Cargo.toml`) and seed watches so `target`
  and `target/debug` are watched. Write `proj/Cargo.toml`, call
  `rescan_subtree(proj)`, and assert the resulting registry topology: `target`
  and `target/debug` are no longer watched, while `proj` and its non-matching
  siblings remain watched.
- Scope isolation: seed two sibling project dirs, rescan only one, and assert
  the other's watches are untouched.
- Migrate the existing overflow reconciliation test to `rescan_subtree(root)`;
  it must stay green (parity with the old `rebuild_watches`).

### Whole suite

- `cargo test` stays green; `cargo build` has no new warnings.

## Rollout

Single change set. No config, CLI, or on-disk format changes. Behavior only
becomes cheaper (bounded rescan) and slightly safer (narrower teardown window);
the marked-path set for any given tree is unchanged. README "How it works" note
about the trigger rescan is updated to say the rescan is scoped to the trigger's
directory rather than the whole root.
