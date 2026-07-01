# Watcher Robustness: Overflow Registry Reconciliation

**Date:** 2026-07-01
**Status:** Approved for planning

## Problem

The `Q_OVERFLOW` recovery added in the previous branch (`app.rs:179-188`) re-scans
the watch root via `discover_watch_targets` → `apply_discovered_paths`, but
`add_watch` (`watch.rs:36`) early-returns on `registry.contains_path`. That guard
leaves a correctness hole after an overflow, when the kernel has dropped events
we can no longer replay:

- **Scenario A — deleted intermediate directory.** A watched directory
  disappeared. The walk no longer finds it, so its stale `by_path` entry lingers
  in the registry (bookkeeping leak) and its kernel watch also leaks against
  `max_user_watches`.
- **Scenario B — deleted then recreated under the same name.** A watched
  intermediate directory was removed and recreated with a fresh inode. The walk
  *does* rediscover the path, but `contains_path` is still `true` from the old
  entry, so `add_watch` skips it. The new inode is never watched, and any
  `node_modules` (etc.) created underneath it silently leaks into sync.

Pruning "paths not rediscovered by the walk" fixes only Scenario A. It cannot fix
Scenario B, because the path *is* rediscovered — the stale entry is what shields
it. Closing Scenario B requires re-establishing the watch on the new inode.

## Scope

In scope:

- Make `Q_OVERFLOW` recovery reconcile the watch registry with reality so both
  scenarios are handled, by rebuilding the watch set from scratch.
- Add a direct unit test for the matched-non-directory branch of `plan_entry`
  (e.g. an `*.egg-info` file), which was previously untested.

Explicitly out of scope (unchanged from the prior branch):

- Initial-seed race between walking the tree and registering watches (A-3).
- Un-ignore / clear capability, multiple roots (B).
- Config-driven rules or additional built-in rules (C).

No new dependencies are introduced.

## Design

### Guiding approach

Confine changes to the overflow-recovery branch in `src/app.rs` plus one new
`WatchRegistry` method in `src/watch.rs`. The normal event-loop path —
`add_watch` with its `contains_path` guard — is left **unchanged**, preserving
its behavior and cost. Only overflow recovery changes: instead of an additive
re-scan, it performs a **full rebuild** — drop every watch we hold (kernel and
registry), then re-seed from `root` using the existing idempotent
`discover_watch_targets` + `apply_discovered_paths`.

### Why full rebuild is correct

After an overflow the kernel has dropped events, so no held descriptor can be
trusted. Rebuilding from the current tree state resolves every scenario:

- **Scenario A:** a deleted directory is absent from the fresh walk, so it is not
  re-added; its old watch was already removed from the kernel and its registry
  entry was cleared during teardown. No leak.
- **Scenario B:** clearing the registry first means the recreated path has
  `contains_path == false`, so `add_watch` unconditionally registers a watch on
  the new inode. The old descriptor was already auto-removed by the kernel (our
  explicit `remove` returns `EINVAL`, which we ignore).

This relies on standard Linux inotify semantics: `add` on a live inode is
idempotent (same descriptor), `add` on a recreated inode yields a fresh
descriptor, and `remove` of an already-gone watch returns `EINVAL`.

### `WatchRegistry::drain_descriptors` (`src/watch.rs`)

A single primitive supports the rebuild:

```rust
/// Drop all bookkeeping and return the descriptors so the caller can remove
/// them from the kernel. Used by overflow recovery to rebuild the watch set
/// from scratch.
pub(crate) fn drain_descriptors(&mut self) -> Vec<WatchDescriptor> {
    self.by_path.clear();
    self.by_descriptor
        .drain()
        .map(|(descriptor, _)| descriptor)
        .collect()
}
```

Returning an owned `Vec` avoids holding a borrow of the registry during the
kernel-removal loop (no borrow-checker gymnastics; the allocation only happens on
the rare overflow path). It leaves the registry empty. The existing
`watched_count` accessor is retained for the idempotency test.

### `rebuild_watches` helper (`src/app.rs`)

Following the precedent of extracting `plan_entry` for testability, the rebuild
is extracted into a helper so its reconciliation behavior can be tested without
provoking a real kernel overflow:

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

The overflow branch (`app.rs:179-188`) collapses to:

```rust
if needs_rescan {
    rebuild_watches(&root, dry_run, &mut watcher, &mut registry, &rule_engine)?;
}
```

Error handling mirrors the current block: a `discover_watch_targets` failure is
logged and swallowed (recovery is best-effort), while an
`apply_discovered_paths` failure (e.g. `ENOSPC` from `add_watch`) propagates so
the watcher fails loudly rather than running blind.

## Testing

- **`drain_descriptors` (`src/watch.rs`):** register two real watches, assert
  `drain_descriptors()` returns two descriptors and leaves `watched_count() == 0`.
- **Rebuild reconciliation / Scenario B (`src/app.rs`):** seed a real tree
  (`a`, `a/b`) via discover + apply, then `insert` a stale entry for a
  nonexistent path (`ghost`) using a live descriptor — modelling a
  deleted-then-recreated intermediate whose old entry lingers. After
  `rebuild_watches`, assert `ghost` is gone, `a`/`a/b`/root are present, and the
  count equals a fresh `discover_watch_targets`. This verifies the
  clear-then-reseed invariant that closes Scenario B, deterministically and
  without driving live inotify events.
- **Matched-non-directory branch (`src/app.rs`):** create an `*.egg-info` **file**
  (not a directory), evaluate `plan_entry` with a `PythonBuildArtifactsRule`
  engine, and assert `apply_ignore == true` and `watch_dir == false` (a matched
  non-directory gets the attribute but is not watched).
- Preserve all existing tests; keep `cargo clippy --all-targets` warning-free.

## Files touched

- `src/watch.rs`: add `drain_descriptors`; add its unit test.
- `src/app.rs`: extract `rebuild_watches`; replace the overflow branch with a
  call to it; add the reconciliation test and the matched-non-directory
  `plan_entry` test.
- `README.md`: unchanged — the existing overflow-rescan note remains accurate.
