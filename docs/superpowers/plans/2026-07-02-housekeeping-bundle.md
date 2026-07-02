# Housekeeping Bundle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Clear four maintenance items: refresh `Cargo.lock`, deduplicate the `xattr_supported` test helper, add a matches-only discovery path for `--scan-once`, and add a local verification gate (check script + pre-commit hook).

**Architecture:** Four independent, small changes to an existing healthy Rust CLI (`dropignore`). No public behavior changes; item 4 adds repo tooling only. Spec: `docs/superpowers/specs/2026-07-02-housekeeping-bundle-design.md`.

**Tech Stack:** Rust 2024 edition, cargo, plain POSIX shell + git hooks (no new dependencies).

## Global Constraints

- Do not add any dependency to `Cargo.toml` (dev or otherwise); do not change version ranges in `Cargo.toml`.
- Verification standard for every task: `cargo test` all green, `cargo clippy --all-targets` zero warnings, `cargo fmt --check` no diff.
- This repo never pushes to `origin` (it is stale). Work on a local branch; it will be merged into local `main` with `--no-ff` at the end.
- If the branch/worktree was created from `origin/main`, immediately run `git reset --hard main` (origin is ~63 commits behind local main).
- Commit messages follow the existing conventional style (`chore:`, `refactor(test):`, `perf(discovery):`, `docs:` …).

---

### Task 1: Refresh Cargo.lock

**Files:**
- Modify: `Cargo.lock` (via `cargo update` only — never edit by hand)

**Interfaces:**
- Consumes: nothing
- Produces: nothing later tasks rely on (independent)

- [ ] **Step 1: Refresh the lockfile**

Run: `cargo update`
Expected: output lists `Updating <crate> vX -> vY` lines (e.g. `clap v4.5.53 -> v4.6.1`) and exits 0. `Cargo.toml` must remain untouched (`git diff --stat` shows only `Cargo.lock`).

- [ ] **Step 2: Verify the full suite against the new lockfile**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: `test result: ok. 64 passed; 0 failed`, clippy finishes with no warnings, fmt exits 0 silently.

- [ ] **Step 3: Commit**

```bash
git add Cargo.lock
git commit -m "chore(deps): refresh Cargo.lock to latest compatible releases"
```

---

### Task 2: Share the xattr_supported test helper

**Files:**
- Create: `src/test_util.rs`
- Modify: `src/main.rs` (module declaration)
- Modify: `src/dropbox.rs:89-99` (remove local helper)
- Modify: `src/app.rs:1262-1272` (remove local helper)

**Interfaces:**
- Consumes: nothing
- Produces: `pub(crate) fn xattr_supported(path: &Path) -> bool` in `crate::test_util` (test builds only) — Task 1/3/4 do not use it; only the two existing test modules do.

Note: this is a pure test refactor — the "failing test" phase is the compile error after deleting the local helpers; the existing 64 tests are the safety net.

- [ ] **Step 1: Create the shared helper module**

Create `src/test_util.rs` with exactly:

```rust
//! Test-only helpers shared across module test suites.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// True when the filesystem hosting `path` accepts user.* xattrs. Used to
/// skip (not fail) on filesystems without support.
pub(crate) fn xattr_supported(path: &Path) -> bool {
    let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let c_name = CString::new("user.dropignore.probe").unwrap();
    // SAFETY: pointers are valid for the duration of the call; the value
    // is one byte and the length matches.
    let result =
        unsafe { libc::setxattr(c_path.as_ptr(), c_name.as_ptr(), b"1".as_ptr().cast(), 1, 0) };
    result == 0
}
```

- [ ] **Step 2: Register the module in main.rs**

In `src/main.rs`, the module list currently reads:

```rust
mod app;
mod cli;
mod discovery;
mod dropbox;
mod rules;
mod watch;
```

Change it to:

```rust
mod app;
mod cli;
mod discovery;
mod dropbox;
mod rules;
#[cfg(test)]
mod test_util;
mod watch;
```

- [ ] **Step 3: Remove the duplicate in dropbox.rs**

In `src/dropbox.rs` inside `mod tests`, delete this block (lines 89–99):

```rust
    /// True when the filesystem hosting `path` accepts user.* xattrs. Used to
    /// skip (not fail) on filesystems without support.
    fn xattr_supported(path: &Path) -> bool {
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let c_name = CString::new("user.dropignore.probe").unwrap();
        // SAFETY: pointers are valid for the duration of the call; the value
        // is one byte and the length matches.
        let result =
            unsafe { libc::setxattr(c_path.as_ptr(), c_name.as_ptr(), b"1".as_ptr().cast(), 1, 0) };
        result == 0
    }
```

and add the import to the test module's `use` block (after `use super::*;`):

```rust
    use crate::test_util::xattr_supported;
```

(`CString` etc. stay imported via `use super::*;` — the remaining test still uses them.)

- [ ] **Step 4: Remove the duplicate in app.rs**

In `src/app.rs` inside `mod tests`, delete the identical block (lines 1262–1272, the doc comment plus `fn xattr_supported`), and add to the test module's `use` block (after `use crate::discovery::discover_watch_targets;` around line 454):

```rust
    use crate::test_util::xattr_supported;
```

(`std::ffi::CString` and `std::os::unix::ffi::OsStrExt` imports stay — `scan_once_marks_matches_with_real_xattr` still uses them directly.)

- [ ] **Step 5: Verify**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: `test result: ok. 64 passed; 0 failed`; zero clippy warnings (in particular no `unused_imports`); fmt clean.

- [ ] **Step 6: Commit**

```bash
git add src/test_util.rs src/main.rs src/dropbox.rs src/app.rs
git commit -m "refactor(test): share xattr_supported probe via test_util"
```

---

### Task 3: Matches-only discovery for --scan-once

**Files:**
- Modify: `src/discovery.rs:14-108` (extract `walk`, add `discover_matches`, add test)
- Modify: `src/app.rs:2` (import) and `src/app.rs:66-85` (`scan_once`)

**Interfaces:**
- Consumes: nothing from earlier tasks
- Produces: `pub(crate) fn discover_matches(start: &Path, rules: &RuleEngine) -> Result<Vec<PathBuf>>` in `crate::discovery`. `discover_watch_targets(start: &Path, rules: &RuleEngine) -> Result<DiscoveredPaths>` keeps its exact signature.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/discovery.rs`:

```rust
    #[test]
    fn discover_matches_agrees_with_full_discovery() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        fs::create_dir(temp.path().join("keep"))?;
        fs::create_dir(temp.path().join("node_modules"))?;

        let engine = RuleEngine::new(vec![Box::new(ArtifactDirsRule::NODE_MODULES)]);
        let matches = discover_matches(temp.path(), &engine)?;
        let full = discover_watch_targets(temp.path(), &engine)?;

        assert_eq!(
            matches, full.matches,
            "both discovery entry points must report the same matches"
        );
        Ok(())
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test discover_matches_agrees_with_full_discovery`
Expected: compile error `cannot find function \`discover_matches\` in this scope`.

- [ ] **Step 3: Extract walk and add discover_matches**

In `src/discovery.rs`, replace the header of the walk function

```rust
/// Walk the directory tree rooted at `start` and gather:
/// - All directories that should be watched (excluding ignored subtrees).
/// - All paths that satisfy a matching rule.
pub(crate) fn discover_watch_targets(start: &Path, rules: &RuleEngine) -> Result<DiscoveredPaths> {
```

with

```rust
/// Walk the directory tree rooted at `start` and gather:
/// - All paths that satisfy a matching rule.
/// - When `collect_watchers` is set, all directories that should be watched
///   (excluding ignored subtrees). `--scan-once` never registers watches, so
///   it skips this collection.
fn walk(start: &Path, rules: &RuleEngine, collect_watchers: bool) -> Result<DiscoveredPaths> {
```

Inside the loop, wrap the watcher push (currently `discovered.watchers.push(dir.clone());`) as:

```rust
        if collect_watchers {
            discovered.watchers.push(dir.clone());
        }
```

Then add the two public entry points directly below `walk`:

```rust
/// Walk the tree and gather both watch targets and rule matches.
pub(crate) fn discover_watch_targets(start: &Path, rules: &RuleEngine) -> Result<DiscoveredPaths> {
    walk(start, rules, true)
}

/// Walk the tree and return only rule matches, skipping watcher collection.
/// Used by `--scan-once`, which never registers inotify watches.
pub(crate) fn discover_matches(start: &Path, rules: &RuleEngine) -> Result<Vec<PathBuf>> {
    Ok(walk(start, rules, false)?.matches)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test discover_matches_agrees_with_full_discovery`
Expected: `test result: ok. 1 passed`.

- [ ] **Step 5: Switch scan_once to discover_matches**

In `src/app.rs` line 2, change

```rust
use crate::discovery::{DiscoveredPaths, discover_watch_targets};
```

to

```rust
use crate::discovery::{DiscoveredPaths, discover_matches, discover_watch_targets};
```

Replace the `scan_once` function (currently lines 66–85):

```rust
/// Walk the tree once, apply `apply` to every rule match, and return. Watch
/// targets from discovery are ignored: nothing is registered with inotify.
/// Fails when at least one matched path could not be marked, so cron/systemd
/// sees a non-zero exit code.
fn scan_once<F>(root: &Path, rules: &RuleEngine, apply: F) -> Result<()>
where
    F: FnMut(&Path) -> Result<()>,
{
    let discovered = discover_watch_targets(root, rules)?;
    let total = discovered.matches.len();
    let failures = apply_all(&discovered.matches, apply);
```

with

```rust
/// Walk the tree once, apply `apply` to every rule match, and return. Watcher
/// collection is skipped entirely (`discover_matches`): nothing is registered
/// with inotify. Fails when at least one matched path could not be marked, so
/// cron/systemd sees a non-zero exit code.
fn scan_once<F>(root: &Path, rules: &RuleEngine, apply: F) -> Result<()>
where
    F: FnMut(&Path) -> Result<()>,
{
    let matches = discover_matches(root, rules)?;
    let total = matches.len();
    let failures = apply_all(&matches, apply);
```

The remainder of the function (`if failures > 0 { … }` through `Ok(())`) is unchanged.

- [ ] **Step 6: Verify the full suite**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: `test result: ok. 65 passed; 0 failed`; zero clippy warnings; fmt clean.

- [ ] **Step 7: Commit**

```bash
git add src/discovery.rs src/app.rs
git commit -m "perf(discovery): skip watcher collection in scan-once"
```

---

### Task 4: Local verification gate (check script + pre-commit hook)

**Files:**
- Create: `scripts/check.sh` (executable)
- Create: `.githooks/pre-commit` (executable)
- Modify: `README.md:43-46` (Testing section)

**Interfaces:**
- Consumes: nothing (runs the standard cargo commands)
- Produces: `scripts/check.sh` as the repo's canonical verification entry point.

- [ ] **Step 1: Create the check script**

Create `scripts/check.sh` with exactly:

```bash
#!/usr/bin/env bash
# Full local verification gate: formatting, lints (zero warnings), tests.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Then make it executable: `chmod +x scripts/check.sh`

- [ ] **Step 2: Create the pre-commit hook**

Create `.githooks/pre-commit` with exactly:

```bash
#!/usr/bin/env bash
exec "$(git rev-parse --show-toplevel)/scripts/check.sh"
```

Then make it executable: `chmod +x .githooks/pre-commit`

- [ ] **Step 3: Run the script directly to verify it works**

Run: `./scripts/check.sh`
Expected: fmt silent, clippy finishes with no warnings, `test result: ok. 65 passed; 0 failed`, exit code 0.

- [ ] **Step 4: Document activation in README**

In `README.md`, the Testing section currently reads:

````markdown
## Testing
```bash
cargo test
```
````

Replace it with:

````markdown
## Testing
```bash
cargo test          # tests only
./scripts/check.sh  # full gate: fmt --check, clippy -D warnings, tests
```

Enable the gate as a pre-commit hook (opt-in, once per clone):
```bash
git config core.hooksPath .githooks
```
Bypass in emergencies with `git commit --no-verify`.
````

- [ ] **Step 5: Activate the hook and commit through it**

```bash
git config core.hooksPath .githooks
git add scripts/check.sh .githooks/pre-commit README.md
git commit -m "chore: add local verification gate (check script + pre-commit hook)"
```

Expected: the commit output is preceded by the full check run (fmt, clippy, 65 tests) — that run is the end-to-end verification of the hook itself. The commit succeeds.
