# Watch-failure isolation, rule consolidation, and small quality fixes

Date: 2026-07-02

## Summary

Three improvements found during a codebase review, bundled because each is
small and they touch disjoint seams:

- **A. Watch-failure isolation (robustness bug).** `add_watch` failures
  currently propagate through `apply_discovered_paths` and kill the whole
  process. A directory that vanishes between discovery and registration
  (ENOENT race) or an unreadable directory (EACCES) must not take down a
  long-running watcher.
- **B. Small quality fixes.** Idempotent xattr marking, a `--version` flag,
  a larger inotify read buffer, and applying `cargo fmt`.
- **C. Rule consolidation.** Four of the five rules are the same shape
  ("directory whose name is in a fixed list"); consolidate them into one
  data-driven rule type so future rule additions are a one-line constant
  change.

No new external dependencies. No change to which paths get marked, except
that already-marked paths are no longer re-marked.

## A. Watch-failure isolation

### Problem

`drain_events` and the initial seeding both call
`apply_discovered_paths(...)?`, which calls `add_watch(...)?`. Any
`inotify_add_watch` failure — including a benign ENOENT when a directory is
deleted between the discovery walk and registration — returns `Err`, unwinds
the event loop, and exits the process.

### Design

Change `add_watch`'s contract so that **only ENOSPC is fatal**:

- On failure with `raw_os_error() == ENOSPC`: return `Err` with the existing
  `max_user_watches` hint. Silently degrading coverage is worse than exiting
  with a clear message, so the watch-limit case stays fatal.
- On any other failure (ENOENT race, EACCES, ENOTDIR from `ONLYDIR`, …):
  `warn!` with the path and error, do not touch the registry, and return
  `Ok(())`. This matches the discovery walk, which already warns and skips
  unreadable directories.

Document the contract in the doc comment. All three call sites (initial
seeding, `drain_events`, `rescan_subtree`) are unchanged and inherit the fix.

### Tests

- `add_watch` on a nonexistent path returns `Ok(())` and does not insert into
  the registry (real ENOENT exercise of the skip path).
- ENOSPC cannot be reproduced in a unit test; the existing
  `watch_error_context` tests continue to cover that branch's message.

## B. Small quality fixes

### Idempotent marking

`apply_dropbox_ignore` re-applies the attribute (and logs at `info!`) every
time a rescan revisits an already-marked path. Add a private helper in
`dropbox.rs` that reads the attribute with `getxattr` and returns whether the
value is already `1`:

- Already marked: log at `debug!` and return `Ok(())` without writing.
- Not marked, unreadable attribute (ENODATA, ENOTSUP, …): fall through to
  `setxattr` as today.
- Dry-run performs the same check so its log distinguishes "already marked"
  from "would mark".

Watched paths are never symlinks (both walk and event path skip them), so
plain `getxattr` (which follows symlinks) is safe.

Test: applying twice succeeds and the second call performs no write. When the
test filesystem lacks `user.*` xattr support (ENOTSUP), skip the assertions
with an explanatory log rather than failing.

### `--version` flag

Add `#[command(version)]` to `CliArgs` so clap reads the version from
`Cargo.toml` (verified against current clap docs).

### Larger event buffer

Grow `drain_events`' read buffer from 4 KiB to 64 KiB to reduce read calls
during event bursts. Still a stack allocation in a single frame; no other
change.

### Formatting

Run `cargo fmt` to clear the existing drift in `app.rs` (import order, struct
literal layout).

## C. Rule consolidation

### Problem

`NodeModulesRule`, `PnpmStoreRule`, `PythonBuildArtifactsRule` (directory
half), and `JsBuildArtifactsRule` all implement the same predicate: "is a
directory whose file name is in a fixed list", with `IGNORE_AND_SKIP`. That is
~100 lines of near-duplicate code, and adding a rule means adding a type.

### Design

One data-driven rule type in `rules.rs`:

```rust
pub(crate) struct ArtifactDirsRule {
    name: &'static str,
    dirs: &'static [&'static str],
}
```

- `matches`: candidate is a directory and its file name is in `dirs`.
- `action`: `IGNORE_AND_SKIP`.
- Associated constants define the four instances, keeping today's log names:
  - `NODE_MODULES` — "node_modules directory", `["node_modules"]`
  - `PNPM_STORE` — "pnpm store directory", `[".pnpm-store"]`
  - `PYTHON_CACHES` — "Python build/cache artifact", the current
    `PYTHON_ARTIFACT_DIRS` list
  - `JS_BUILD` — "JavaScript build/cache directory", the current
    `JS_ARTIFACT_DIRS` list

The `*.egg-info` suffix match is a different predicate (suffix, dir-or-file),
so it becomes its own `EggInfoRule` with log name "Python egg-info metadata"
(log-only change). `RustTargetRule` (conditional match + `Cargo.toml` trigger)
is unchanged and remains the template for conditional rules.

Delete the four superseded rule types, update the `RuleEngine::new`
registration in `app.rs`, and update tests that construct the old types.
Rule priority ordering in the registration list is preserved.

### Non-goals (YAGNI)

- No config file or runtime-defined rules; lists stay compiled in.
- No new artifact directories; the rule set's coverage is unchanged.
- No macro-generated rule types.

## Verification

- `cargo test` passes.
- `cargo clippy --all-targets` reports no warnings.
- `cargo fmt --check` is clean.
- Implementation proceeds test-first (TDD).
