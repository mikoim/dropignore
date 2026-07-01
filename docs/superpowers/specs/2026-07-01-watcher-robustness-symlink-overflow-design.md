# Watcher Robustness: Symlink Consistency & Queue Overflow Recovery

**Date:** 2026-07-01
**Status:** Approved for planning

## Problem

The inotify event loop in `src/app.rs` has two robustness defects that let the
watched tree drift out of sync with reality:

1. **Symlink handling is inconsistent between discovery and the event loop.**
   `src/discovery.rs` inspects entries with `entry.file_type()` (which does *not*
   follow symlinks) and skips symlinks to avoid cycles. The runtime event loop in
   `src/app.rs` instead calls `fs::metadata` (which *does* follow symlinks), so a
   symlink pointing at a directory is treated as a real directory. When such a
   symlink is created or moved into a watched directory at runtime, the loop
   recurses through it via `discover_watch_targets`, registering watches on the
   link target — potentially **outside the watched tree** (e.g. `$HOME`) or in a
   cycle. This is a latent bug, not just a style inconsistency.

2. **inotify queue overflow (`Q_OVERFLOW`) is unhandled.** When many events are
   produced faster than the loop drains them (e.g. a large `npm install`), the
   kernel drops events and emits a single `Q_OVERFLOW` event with an invalid
   watch descriptor. The current loop only branches on
   `CREATE`/`MOVED_TO`/`DELETE_SELF`/`MOVE_SELF`, so the overflow is silently
   ignored. Dropped events mean newly created `node_modules` (etc.) never get the
   Dropbox ignore attribute, leaking into sync without any signal.

## Scope

In scope:

- **A-1:** Make the event loop treat symlinks consistently with discovery — skip
  them.
- **A-2:** Detect `Q_OVERFLOW` and recover by re-scanning from the watch root.

Explicitly out of scope:

- Initial-seed race between walking the tree and registering watches (A-3).
- Un-ignore / clear capability, multiple roots (B).
- Config-driven rules or additional built-in rules (C).

No new dependencies are introduced.

## Design

### Guiding approach

Keep the existing structure intact (event loop in `app.rs`, tree walk in
`discovery.rs`, watch bookkeeping in `watch.rs`). Concentrate changes in
`app.rs`. Rely on existing idempotency to make re-scanning safe:

- `add_watch` (`src/watch.rs`) already guards with `registry.contains_path` and
  skips already-registered paths.
- `apply_dropbox_ignore` (`src/dropbox.rs`) uses `setxattr` with `flags = 0`
  (create-or-replace), so re-applying an attribute is a no-op in effect.

### A-1: Symlink consistency

- Replace `fs::metadata(&full_path)` in the event loop with
  `fs::symlink_metadata(&full_path)`, so the metadata describes the entry itself
  rather than a symlink's target.
- If the resulting metadata reports a symlink, emit a `debug!` log (mirroring
  `discovery.rs`'s "Ignoring symlink … to avoid cycles" message) and skip the
  entry entirely: no rule evaluation, no attribute application, no watch, no
  recursion.
- Rationale: matches are keyed on directory names and are meant for real
  directories, not links. Skipping symlinks wholesale is the simplest behavior
  that aligns the loop with discovery and closes the tree-escape/cycle path.

### A-2: Queue overflow recovery

- Thread the canonicalized `root: PathBuf` from `run` into `event_loop` so the
  loop has a re-scan origin. (Today `event_loop` receives the watcher, registry,
  and rule engine but not the root.)
- At the top of per-event handling, check
  `event.mask.contains(EventMask::Q_OVERFLOW)`. On overflow the descriptor and
  name are invalid, so this branch must run before descriptor lookup. Set a
  `needs_rescan` flag and `warn!` that events were dropped. Multiple overflow
  events in one batch collapse to a single flag (one re-scan per batch).
- After the event iterator is dropped (same point where `pending_directories`
  are processed, so the mutable borrow of the watcher is available again), if
  `needs_rescan` is set, run `discover_watch_targets(&root, &rule_engine)` and
  feed the result to the existing `apply_discovered_paths`. This re-registers any
  missing watches and re-applies the attribute to any matches, both idempotently.

### Testability

Extract the per-entry decision (given a path + its own metadata, should it be
skipped as a symlink / evaluated / recursed) into a small helper so it can be
unit-tested without a live inotify instance. The overflow path's safety is
covered by asserting the re-scan primitive is idempotent rather than trying to
provoke a real kernel overflow (which is environment-dependent and flaky).

## Testing

- **A-1:** Create a directory and a symlink pointing to it; assert the event-loop
  decision helper classifies the symlink as skip (not as a directory to watch or
  a candidate to evaluate).
- **A-2:** Assert `discover_watch_targets` + `apply_discovered_paths` are
  idempotent — running twice over the same tree does not double-register watches
  (the registry holds a single entry per path) and does not error.
- Preserve all existing tests; keep `cargo clippy --all-targets` warning-free.

## Files touched

- `src/app.rs`: symlink skip via `symlink_metadata`, `Q_OVERFLOW` branch,
  `root` parameter on `event_loop`, batch-collapsed re-scan, extracted decision
  helper, new unit tests.
- Possibly a one-line note in `README.md` if overflow-recovery behavior is worth
  documenting; otherwise unchanged.
