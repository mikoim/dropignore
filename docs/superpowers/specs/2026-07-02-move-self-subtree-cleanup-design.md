# Watcher Robustness: MOVE_SELF Subtree Cleanup

**Date:** 2026-07-02
**Status:** Approved for planning

## Problem

When a watched directory is moved, `MOVE_SELF` fires only for that directory's
own watch descriptor. The current handler (`app.rs:203-207`) removes exactly one
registry entry via `remove_by_descriptor`, which leaves two holes:

- **Scenario A — subtree moved out of the watched tree.** `mv root/a /outside/`
  fires `MOVE_SELF` for `a` only. Every descendant (`a/x`, `a/x/y`, …) keeps its
  registry entry under the stale in-tree path *and* its kernel watch stays alive
  (the kernel only auto-removes watches on `DELETE_SELF`). Consequences:
  - Leaked kernel watches count against `max_user_watches` forever.
  - Events on the moved inodes resolve to the stale in-tree paths. Normally the
    metadata read fails and warns, but if the old path is later recreated, events
    from the *old, moved* inode are attributed to the *new* directory — the
    watcher can mark or walk the wrong path.
- **Scenario B — descriptor reuse across event batches.** inotify returns the
  same watch descriptor when a still-watched inode is re-added. For an in-tree
  rename, the `MOVED_TO` rediscovery re-registers descendants under their new
  paths, reusing the old descriptors. If the queued `MOVE_SELF` for the old path
  is read in a *later* batch (buffer boundary), `remove_by_descriptor` deletes
  the freshly re-registered entry: the directory silently stops being tracked
  while its kernel watch stays alive, and later events hit "unknown watch
  descriptor".

A related gap: if the watched **root itself** is moved (or deleted), the watcher
loses its entire watch set and idles forever, looking alive to a supervisor
while doing nothing.

## Scope

In scope:

- Handle `MOVE_SELF` by scheduling a scoped rescan of the descriptor's path,
  reusing the existing `rescan_scopes` → `rescan_subtree` machinery.
- Fail fast (non-zero exit) when the watched root itself is moved or deleted, so
  a supervisor restarts the process instead of it idling with no watches.

Explicitly out of scope:

- Deduplicating nested `rescan_scopes` (parent + child scheduled in one batch);
  rescans are idempotent, so overlap is only redundant work.
- Un-ignore / attribute-clearing, multiple roots, config-file rules.

## Design

All changes live in `drain_events` (`src/app.rs`). The combined
`DELETE_SELF || MOVE_SELF` branch is split.

### MOVE_SELF → scoped rescan

```rust
if event.mask.contains(EventMask::MOVE_SELF) {
    if let Some(path) = registry.path_for(&event.wd) {
        if path == root {
            anyhow::bail!("Watched root {} was moved or renamed", root.display());
        }
        rescan_scopes.insert(path.clone());
    }
    continue;
}
```

`rescan_subtree` already implements exactly the recovery needed: it drains every
registry entry at or under the scope (`drain_subtree`), removes the drained
kernel watches (ignoring EINVAL for already-dead ones), and reseeds from a fresh
walk. Per scenario:

- **Moved out of tree:** the old path no longer exists, so the drain removes all
  stale descendants and their kernel watches; the reseeding walk hits the
  existing "metadata read failed → warn → skip" path and is a no-op.
- **Moved within tree:** the `MOVED_TO` rediscovery (processed via
  `pending_directories`, which runs *before* `rescan_scopes`) registers the new
  paths; descriptor-reuse eviction in `WatchRegistry::insert` already clears the
  matching stale entries, so the rescan only sweeps leftovers.
- **Scenario B (late MOVE_SELF):** the descriptor now maps to the new, live
  path. The rescan drains it and immediately reseeds it from disk — a
  self-healing no-op instead of a silent tracking loss.
- **Unknown descriptor** (subtree already drained by an earlier scope): no path
  to rescan; skip silently.

Ordering note: the kernel queues rename events as `MOVED_FROM`, `MOVED_TO`,
`MOVE_SELF` in FIFO order, and within one batch `pending_directories` runs
before `rescan_scopes`, so a rescan can never tear down registrations that the
same batch's `MOVED_TO` discovery is about to make — it runs after them.

### Root-gone guard

- **MOVE_SELF on the root's descriptor:** bail with an error (shown above). The
  canonicalized root path is no longer valid; restarting is the only correct
  recovery.
- **DELETE_SELF on the root's descriptor:** same bail, symmetric reasoning. For
  non-root paths, `DELETE_SELF` keeps its current handling
  (`remove_by_descriptor`): the kernel auto-removes the watch, and recursive
  deletion fires `DELETE_SELF` for every directory, so nothing lingers.

The `bail!` propagates `drain_events` → `event_loop` → `run` → non-zero exit.

## Error Handling

- Root moved/deleted: process exits non-zero with a message naming the root.
  Today it would idle forever with zero watches; explicit failure hands recovery
  to the supervisor.
- All other failure paths (unreadable directories during reseeding, EINVAL on
  watch removal) already have warn-and-continue semantics in `rescan_subtree`
  and are unchanged.

## Testing

Unit tests in `app.rs`, same style as the existing `drain_events` /
`rescan_subtree` tests (TempDir + real inotify, dry-run):

1. **Out-of-tree move:** watch `root/a/x`, rename `a` into a sibling TempDir,
   drain; assert no registry entry remains under the old paths and
   `watched_count` equals a fresh discovery of the root.
2. **In-tree move:** rename `a` → `b` inside the root, drain; assert `b` and
   `b/x` are watched and `a`/`a/x` entries are gone.
3. **Root moved:** rename the root directory itself, drain; assert
   `drain_events` returns `Err`.
4. **Root deleted:** remove the root directory tree, drain; assert
   `drain_events` returns `Err`.
