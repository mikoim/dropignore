# Housekeeping Bundle Design

Date: 2026-07-02
Status: Approved

## Goal

Clear four small maintenance items in one pass: refresh dependencies, add a
local verification gate (script + pre-commit hook), deduplicate the
`xattr_supported` test helper, and stop `--scan-once` from building a
watchers list it never uses.

## Background

The 2026-07-02 codebase survey found the project healthy (64 tests passing,
zero clippy warnings, ~94% coverage) but left these known items unaddressed:

- `Cargo.lock` lags current compatible releases (e.g. clap 4.5.53 → 4.6.1).
- No automated local verification; `origin` is stale and never pushed, so
  GitHub Actions cannot serve as CI.
- `xattr_supported` is duplicated verbatim in `src/dropbox.rs` and
  `src/app.rs` test modules.
- `discover_watch_targets` always collects a `watchers` Vec; `scan_once`
  discards it (flagged as a non-blocking Minor in the scan-once review).

## Non-Goals

- Raising minimum versions in `Cargo.toml` (ranges like `"4.5"` already
  admit newer minors; no new API is used).
- Adding external tooling (`just`, `pre-commit` framework, `xattr` crate).
- Callback- or iterator-based discovery rewrite.
- unmark, rule configuration, and multi-root watching (remain in the
  backlog).

## Design

### 1. Dependency refresh

Run `cargo update` to refresh `Cargo.lock` only; `Cargo.toml` is untouched.
clap's policy reserves breaking changes for major releases and cargo
resolves against the current toolchain's MSRV, so the risk is low. Verify
with the full check suite afterwards.

### 2. Local verification gate

- `scripts/check.sh` — `set -euo pipefail`; runs in order:
  1. `cargo fmt --check`
  2. `cargo clippy --all-targets -- -D warnings`
  3. `cargo test`
- `.githooks/pre-commit` — thin hook that only invokes `scripts/check.sh`.
- Activation is opt-in per clone: `git config core.hooksPath .githooks`.
- README Testing section documents activation and the
  `git commit --no-verify` escape hatch.
- Both files are executable and committed to the repository.

### 3. Shared `xattr_supported` test helper

- New `src/test_util.rs`, registered in `main.rs` as
  `#[cfg(test)] mod test_util;`.
- Holds `pub(crate) fn xattr_supported(path: &Path) -> bool` (the existing
  probe implementation, moved verbatim).
- `src/dropbox.rs` and `src/app.rs` test modules drop their local copies
  and import the shared one.
- Test-only: the release binary is unaffected.

### 4. Discovery without watcher collection for `--scan-once`

- Extract the traversal body of `discover_watch_targets` into a private
  `walk(start, rules, collect_watchers: bool)`.
- `discover_watch_targets` keeps its signature and becomes a thin wrapper
  over `walk(…, true)`; all existing call sites are unchanged.
- New `discover_matches(start, rules) -> Result<Vec<PathBuf>>` wraps
  `walk(…, false)` and returns only the matches.
- `app::scan_once` switches to `discover_matches`; it is the sole caller.
- New unit test asserts `discover_matches` returns the same matches as
  `discover_watch_targets` on a tree containing both matching and
  non-matching directories. (Skipping watcher collection is an internal
  efficiency property, not observable through the return value.)

## Implementation order

1 (deps) → 3 (test helper) → 4 (discovery) → 2 (verification gate). The
items are independent; doing the gate last lets the finished `check.sh`
serve as the final end-to-end verification.

## Testing

- Every step keeps the existing suite green: `cargo test`,
  `cargo clippy --all-targets` with zero warnings, `cargo fmt --check`.
- Item 4 adds the `discover_matches` unit test described above.
- Item 2 is exercised by making a commit with the hook active.
