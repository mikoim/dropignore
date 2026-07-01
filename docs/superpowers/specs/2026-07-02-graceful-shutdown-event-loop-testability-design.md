# Graceful shutdown and event-loop integration testability

Date: 2026-07-02

## Summary

Make the inotify event loop interruptible so the process can shut down cleanly
on `SIGINT`/`SIGTERM`, and refactor the loop so its real event-handling path can
be exercised by an integration test.

Today `event_loop` blocks forever in `read_events_blocking` and owns its
`WatchRegistry` privately. Two consequences follow: a supervisor's `SIGTERM`
kills the process abruptly (exit code 130/143, no final log line, no clean exit
code for systemd), and the loop's wiring — the seam between "an inotify event
arrived" and "a watch was registered / a path was marked" — has no automated
coverage. Only the pure helpers (`plan_entry`, `rescan_subtree`,
`discover_watch_targets`, `WatchRegistry`) are tested.

This design replaces the single blocking read with a poll-with-timeout loop
gated on an `AtomicBool` shutdown flag set by a signal handler, extracts the
per-batch processing into a `drain_events` seam, and has `event_loop` return its
final `WatchRegistry`. That unlocks a deterministic integration test that drives
real inotify events through the real code path in `dry_run` mode and asserts on
the returned registry.

Scope: robustness plus test coverage. One new external dependency
(`signal-hook`). No change to the rule set, the CLI surface, the on-disk format,
or the marking semantics.

## Goals

- `SIGINT` and `SIGTERM` cause the event loop to stop and the process to exit
  `0`, with a final log line, within a bounded latency (≈500 ms).
- The shutdown mechanism does not depend on the `inotify` crate's internal
  `EINTR`-retry behavior for correctness (the poll timeout guarantees the flag
  is observed regardless).
- The real event-handling path (inotify event → discover → plan → watch
  registration / skip) is covered by an integration test.
- The integration test is deterministic and does not depend on the filesystem
  supporting `user.*` xattrs (achieved via `dry_run`, asserting on the returned
  `WatchRegistry`).
- The production ownership model is unchanged: the loop still owns its
  `WatchRegistry` directly; no `Arc<Mutex<…>>` wrapping and no per-access locking
  is introduced for the sake of testing.

## Non-goals

- Config-file-driven rules, parallel initial scan, a `--once` scan-only mode,
  additional `--exclude` filters — deferred (see "Deferred").
- Asserting real `setxattr` application end to end (option (b)): rejected because
  `user.*` xattrs are unsupported on some tmpfs/CI mounts, making the test flaky.
- Dependency-injecting the applier via a trait (option (c)): rejected as a larger
  refactor (generifying the loop) than the coverage goal warrants.
- macOS/Windows portability via the `notify` crate: the Dropbox ignore mechanism
  is itself OS-specific, so this is out of scope.
- Any change to what is marked or which directories are watched.

## Design

### Shutdown signaling

Add `signal-hook = "0.3"` to `Cargo.toml`.

In `run()`, before entering the loop:

- Create `let shutdown = Arc::new(AtomicBool::new(false));`.
- Register it for both signals via
  `signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown))?`
  and the same for `SIGTERM`. `flag::register` installs a handler whose only
  action is an atomic store — async-signal-safe by construction.
- Pass `Arc::clone(&shutdown)` into `event_loop`.

`register` returns a `SigId` guard-less handle; the registration lives for the
process lifetime, which is what we want.

### Interruptible loop

After `Inotify::init()`, set the inotify file descriptor non-blocking with
`libc::fcntl(fd, F_SETFL, flags | O_NONBLOCK)` (fd obtained via
`AsRawFd::as_raw_fd`). `libc` is already a dependency.

Replace the `loop { read_events_blocking(...) ... }` body with:

```text
loop {
    if shutdown.load(Ordering::Relaxed) {
        info!("Received shutdown signal, stopping watcher");
        break;
    }

    // libc::poll on the single inotify fd with a bounded timeout.
    let ready = poll_inotify(fd, POLL_TIMEOUT_MS)?; // 500 ms
    match ready {
        PollResult::Interrupted => continue, // EINTR: re-check flag next lap
        PollResult::TimedOut     => continue, // idle re-check
        PollResult::Readable     => drain_events(&mut watcher, &mut registry,
                                                  &rules, &root, dry_run)?,
    }
}
Ok(registry)
```

`poll_inotify` wraps a single `libc::poll` call over a one-element `pollfd`
array (`events = POLLIN`). Return-value handling:

- `-1` with `errno == EINTR` → `Interrupted` (a signal arrived mid-poll; loop
  head re-checks the flag). Any other `-1` → propagate as an error.
- `0` → `TimedOut`.
- `> 0` → `Readable`.

`POLL_TIMEOUT_MS = 500` bounds worst-case shutdown latency. Correctness does not
rely on `EINTR` breaking the wait: even if the signal handler used `SA_RESTART`,
the timeout still forces the flag check within 500 ms.

`event_loop`'s signature becomes `-> Result<WatchRegistry>`. `run()` calls it,
discards the returned registry (`let _ = event_loop(...)?;` or binds and drops),
and returns `Ok(())`, yielding a clean exit code `0`.

### `drain_events` seam

Extract the current inner `for event in events { … }` body — together with the
post-loop `pending_directories` seeding, the `needs_rescan` whole-tree rescan,
and the `rescan_scopes` scoped rescans — into:

```rust
fn drain_events(
    watcher: &mut Inotify,
    registry: &mut WatchRegistry,
    rules: &RuleEngine,
    root: &Path,
    dry_run: bool,
) -> Result<()>
```

It reads with the non-blocking `read_events` (not `read_events_blocking`) and
processes every currently-available event, returning once `read_events` reports
`ErrorKind::WouldBlock` (no more queued events). All existing per-event logic —
overflow → `needs_rescan`, `DELETE_SELF`/`MOVE_SELF` → `remove_by_descriptor`,
`CREATE`/`MOVED_TO` filtering, trigger detection → `rescan_scopes`, metadata
read, `plan_entry`, apply, and `pending_directories` — moves verbatim into this
function. The buffer is a local `[0u8; 4096]` as today.

The overflow-supersedes-scopes rule (whole-tree rescan wins over recorded
scopes) is preserved inside `drain_events`.

`event_loop` retains only the poll/flag wrapper and calls `drain_events` when the
fd is readable. Registry ownership stays with `event_loop`; `drain_events` takes
`&mut`.

### Integration tests (`dry_run`, assert on `WatchRegistry`)

Both live in `app.rs`'s `#[cfg(test)]` module.

**Test 1 — event path is deterministic, thread-free.** Create a temp tree, build
a `RuleEngine` (e.g. `NodeModulesRule`), init `Inotify`, run the initial
`discover_watch_targets` + `apply_discovered_paths` (dry-run) to seed watches.
Then create `node_modules/` and a plain nested dir `a/b` under a watched
directory. Call `drain_events` in a bounded retry loop (a few iterations with a
short sleep between, capped by a deadline) until the registry reflects the
creations. Assert:

- the plain directory is watched (`registry.contains_path(a)` /`a/b`),
- `node_modules` is not watched (skip-descendants honored),

exercising the full inotify → discover → plan → register path without threads,
signals, or xattr writes.

**Test 2 — shutdown returns.** Spawn `event_loop` on a thread with a shared
`Arc<AtomicBool>` shutdown flag (dry-run, over a temp root). Create a file under
the root to generate at least one event, then store `true` into the flag. Join
the thread with a timeout guard and assert it returned `Ok`. Optionally assert
the returned `WatchRegistry` contains the root. This covers the poll/flag
wrapper and the registry return value.

Existing tests (`plan_entry_*`, `rescan_subtree_*`, `apply_all_*`,
`rescan_is_idempotent`, etc.) are unaffected.

## Error handling

- A `poll` failure other than `EINTR` propagates out of `event_loop` as an
  `anyhow::Error` (unchanged philosophy: unexpected syscall failures are fatal to
  the loop).
- `read_events` returning `WouldBlock` is the normal drain terminator, not an
  error.
- Signal registration failure in `run()` propagates via `?` before the loop
  starts.
- `fcntl` failure while setting `O_NONBLOCK` propagates via `?`.
- Per-event handling keeps its existing continue-past-failure behavior; no change
  to how `apply_dropbox_ignore` or `discover_watch_targets` failures are logged.

## Testing

- `cargo test` — existing suite plus the two new integration tests.
- `cargo clippy --all-targets` — must stay warning-free.
- Manual: run against a real directory, press Ctrl-C, confirm the "Received
  shutdown signal" line is logged and the process exits `0`; repeat with
  `kill -TERM <pid>`.

## Deferred

Recorded here so the revisit signal is explicit:

- **`--once` scan-only mode** — run initial discovery + marking, then exit
  without entering the loop. Revisit when cron/one-shot usage is wanted.
- **Config-file-driven rules** (`serde`/`toml`) — revisit when users need to add
  directory-name rules without recompiling.
- **Parallel initial scan** (`rayon`/`jwalk`) — revisit if startup latency on a
  large Dropbox tree becomes a measured problem.
- **Real-xattr and DI-applier test variants** — revisit if a regression escapes
  the `dry_run` registry-level assertions.
