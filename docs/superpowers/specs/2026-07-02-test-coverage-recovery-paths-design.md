# Test Coverage: Untested Recovery Paths

**Date:** 2026-07-02
**Status:** Approved for planning

## Problem

Line coverage is 92% overall (`cargo llvm-cov`), but the uncovered lines in
`app.rs` (85%) cluster on the code that matters most when things go wrong —
the recovery paths:

- **Q_OVERFLOW recovery is entirely untested** (`app.rs:192-198`, `316-329`).
  The whole-tree rescan after a kernel queue overflow, and the "root
  disappeared during overflow rescan → bail" guard, have no test. This is the
  most complex recovery logic in the tool.
- **Trigger-file wiring through the event path is untested**
  (`app.rs:260-267`). `rescan_subtree` itself is well tested, but no test
  drives the full chain: a `Cargo.toml` CREATE event arrives →
  `rule_engine.is_trigger` fires → a scoped rescan un-watches a pre-existing
  sibling `target`.
- **Non-root DELETE_SELF bookkeeping removal is untested** (`app.rs:231`).
  Existing deletion tests only cover the root-deleted bail, which returns
  before this line.
- **`event_loop`'s Readable branch is untested** (`app.rs:151`). The shutdown
  test only exercises the timeout path; no test observes the loop actually
  processing an event.
- **`ensure_directory` is untested** (`app.rs:356-363`).

All changes are test-only. Production code is not modified.

## Scope

In scope: six new tests in `app.rs`'s test module, same style as the existing
`drain_events` tests (TempDir + real inotify + deadline-bounded drain loops,
dry-run).

Explicitly out of scope:

- `run()` / `main()` wiring — thin composition of already-tested parts.
- The `setxattr` failure path in `dropbox.rs` — behaves differently under
  root (euid 0 bypasses the permission check), so a test would be a flake
  source.
- Any refactoring of `drain_events` toward synthetic-event unit tests.
  `inotify::WatchDescriptor` cannot be constructed outside the crate, so that
  approach would force an intermediate event representation onto production
  code purely for testing — rejected as contrary to KISS.

## Design

### Overflow tests (2)

Both force a real kernel queue overflow: read
`/proc/sys/fs/inotify/max_queued_events` (16384 by default) and, without
draining, create `max + 100` files inside the watched root so the kernel
appends a genuine `IN_Q_OVERFLOW` event and drops everything after it.

Guard: if the tunable exceeds 65536, creating enough files is not practical
in a unit test; skip with an `eprintln!`, mirroring the existing
`xattr_supported` skip pattern in `dropbox.rs`.

1. **`drain_events_recovers_from_queue_overflow`** — after the queue is full
   (events now being dropped), create `late_dir/` and `node_modules/`; their
   CREATE events are lost. Drain with the standard deadline loop and assert:
   - `late_dir` is watched (the whole-tree rescan found it),
   - `node_modules` is not watched (matched and skipped),
   - the registry exactly matches a fresh `discover_watch_targets` of the
     root.
2. **`drain_events_errors_when_root_vanishes_during_overflow`** — after the
   queue is full, `remove_dir_all` the root; its DELETE_SELF is dropped with
   everything else. Draining must reach the overflow rescan, fail to re-watch
   the root, and return the `disappeared during overflow rescan` error.
   Side effect: the early drained CREATE events reference already-deleted
   files, so this also exercises the "metadata read failed → warn → skip"
   path (`app.rs:272-277`).

### Easy gap-fill tests (4)

3. **`drain_events_rescans_when_trigger_file_appears`** — seed watches for
   `proj/` containing `target/` but no `Cargo.toml` (so `target` is watched),
   then write `proj/Cargo.toml` and drain until `target` is no longer
   watched. Covers the `is_trigger` → `rescan_scopes` wiring end to end.
   Engine: `MarkedBuildDirRule::CARGO_TARGET`.
4. **`drain_events_removes_registry_entry_for_deleted_subdir`** — seed
   `root/a/`, `remove_dir(a)`, drain until `a` leaves the registry. Covers
   the non-root DELETE_SELF branch.
5. **`event_loop_processes_events_before_shutdown`** — same shape as the
   existing shutdown test: spawn `event_loop` on a thread, create a directory
   under the root, give the loop time to process it, set the shutdown flag,
   join, and assert the returned registry contains the new directory. Covers
   the Readable branch.
6. **`ensure_directory_rejects_non_directory`** — call `ensure_directory` on
   a regular file and assert the error message contains
   `is not a directory`.

## Error Handling

Tests follow the existing conventions: deadline-bounded drain loops (5 s; 10 s for the overflow tests) so
missed events fail the assertion rather than hanging, and environment-driven
skips print to stderr instead of failing.

## Verification

- `cargo test` — all tests pass; the two overflow tests add roughly 2–4 s.
- `cargo llvm-cov` — `app.rs` line coverage rises from 85% to roughly 95%;
  total from 92% to roughly 95%.
