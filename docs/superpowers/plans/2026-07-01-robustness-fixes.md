# Robustness Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the `dropignore` inotify daemon resilient to individual failures and correct across directory renames, and fix a documentation mismatch.

**Architecture:** Four independent, small changes to the existing Rust CLI: (1) continue past individual `setxattr` failures instead of exiting, (2) drop stale watch-registry entries when a watched directory is renamed/moved, (3) surface a clearer error when the inotify watch limit is hit, (4) correct the README's "Extending rules" section. Each change adds a small, pure, unit-testable seam.

**Tech Stack:** Rust (edition 2024), `inotify`, `libc`, `anyhow`, `log`, `tempfile` (dev).

## Global Constraints

- Rust edition 2024.
- No new dependencies. Only use crates already in `Cargo.toml`: `anyhow`, `clap`, `env_logger`, `inotify`, `libc`, `log`, and dev-dependency `tempfile`.
- Follow the "minimal external dependencies" and KISS philosophy — no restructuring beyond the tasks below.
- Every task must leave `cargo test` and `cargo clippy --all-targets` passing.
- Commit after each task.

---

### Task 1: Continue past individual setxattr failures (#1)

**Files:**
- Modify: `src/app.rs` (add `apply_all` helper; rewire both `apply_dropbox_ignore` call sites; add `#[cfg(test)] mod tests`)
- Test: `src/app.rs` (inline `mod tests`)

**Interfaces:**
- Produces: `fn apply_all<F>(paths: &[PathBuf], apply: F) -> usize where F: FnMut(&Path) -> Result<()>` — applies `apply` to every path, continuing past `Err`, and returns the count of failures. Callees log their own errors (`apply_dropbox_ignore` already logs `error!`), so `apply_all` does not log.
- Consumes: existing `apply_dropbox_ignore(path: &Path, dry_run: bool) -> Result<()>` from `src/dropbox.rs` (unchanged).

- [ ] **Step 1: Write the failing test**

Add to the bottom of `src/app.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn apply_all_visits_every_path_despite_failures() {
        let paths = vec![
            PathBuf::from("a"),
            PathBuf::from("b"),
            PathBuf::from("c"),
        ];
        let mut seen = Vec::new();
        let failures = apply_all(&paths, |p| {
            seen.push(p.to_path_buf());
            if p == Path::new("b") {
                anyhow::bail!("boom");
            }
            Ok(())
        });

        assert_eq!(failures, 1, "the single failing path should be counted");
        assert_eq!(seen, paths, "every path must be visited even after a failure");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib apply_all_visits_every_path_despite_failures`
Expected: FAIL to compile with "cannot find function `apply_all`".

- [ ] **Step 3: Add the `apply_all` helper**

Add this function to `src/app.rs` (place it just above `fn ensure_directory`):

```rust
/// Apply `apply` to every path, continuing past individual failures, and
/// return how many failed. Callees are expected to log their own errors
/// (see `apply_dropbox_ignore`), so this helper stays silent.
fn apply_all<F>(paths: &[PathBuf], mut apply: F) -> usize
where
    F: FnMut(&Path) -> Result<()>,
{
    let mut failures = 0;
    for path in paths {
        if apply(path).is_err() {
            failures += 1;
        }
    }
    failures
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib apply_all_visits_every_path_despite_failures`
Expected: PASS.

- [ ] **Step 5: Rewire the discovery apply loop**

In `src/app.rs`, inside `apply_discovered_paths`, replace:

```rust
    for matched in discovered.matches {
        apply_dropbox_ignore(&matched, dry_run)?;
    }
```

with:

```rust
    // Apply to every match, continuing past individual failures so one bad
    // path cannot terminate the watcher.
    apply_all(&discovered.matches, |path| apply_dropbox_ignore(path, dry_run));
```

- [ ] **Step 6: Rewire the event-loop apply site**

In `src/app.rs`, inside `event_loop`, replace:

```rust
                if action.set_dropbox_ignore {
                    apply_dropbox_ignore(&full_path, dry_run)?;
                }
```

with:

```rust
                if action.set_dropbox_ignore {
                    // Continue past a failure here too; errors are already logged
                    // by apply_dropbox_ignore.
                    apply_all(std::slice::from_ref(&full_path), |path| {
                        apply_dropbox_ignore(path, dry_run)
                    });
                }
```

- [ ] **Step 7: Run the full suite and clippy**

Run: `cargo test && cargo clippy --all-targets`
Expected: PASS, no warnings.

- [ ] **Step 8: Commit**

```bash
git add src/app.rs
git commit -m "fix(core): continue past individual setxattr failures

A single setxattr error no longer terminates the watcher; matches are
applied via apply_all, which counts failures and continues.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: Clean stale registry entries on rename/move (#2)

**Files:**
- Modify: `src/watch.rs` (add `WatchMask::MOVE_SELF` to `watch_mask`; add `#[cfg(test)] mod tests`)
- Modify: `src/app.rs:60` (handle `MOVE_SELF` alongside `DELETE_SELF`)
- Test: `src/watch.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `watch_mask() -> WatchMask`, `WatchRegistry` (`insert`, `remove_by_descriptor`, `path_for`, `contains_path`) from `src/watch.rs`; `EventMask` from the `inotify` crate.
- Produces: `watch_mask()` now includes `WatchMask::MOVE_SELF`; the event loop removes a registry entry on either `DELETE_SELF` or `MOVE_SELF`.

- [ ] **Step 1: Write the failing tests**

Add to the bottom of `src/watch.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use tempfile::TempDir;

    #[test]
    fn watch_mask_includes_move_and_delete_self() {
        let mask = watch_mask();
        assert!(mask.contains(WatchMask::MOVE_SELF), "MOVE_SELF must be watched");
        assert!(mask.contains(WatchMask::DELETE_SELF), "DELETE_SELF must be watched");
    }

    #[test]
    fn registry_insert_lookup_and_remove() -> Result<()> {
        let temp = TempDir::new()?;
        let mut inotify = Inotify::init()?;
        let descriptor = inotify.watches().add(temp.path(), watch_mask())?;

        let mut registry = WatchRegistry::default();
        registry.insert(temp.path().to_path_buf(), descriptor.clone());

        assert!(registry.contains_path(temp.path()));
        assert_eq!(registry.path_for(&descriptor), Some(&temp.path().to_path_buf()));

        registry.remove_by_descriptor(&descriptor);
        assert!(!registry.contains_path(temp.path()), "path mapping must be gone");
        assert_eq!(registry.path_for(&descriptor), None, "descriptor mapping must be gone");
        Ok(())
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib watch_mask_includes_move_and_delete_self`
Expected: FAIL — assertion `MOVE_SELF must be watched` fails (mask lacks `MOVE_SELF`).

(The `registry_insert_lookup_and_remove` test should already pass — it documents existing behavior the fix relies on.)

- [ ] **Step 3: Add `MOVE_SELF` to the watch mask**

In `src/watch.rs`, replace:

```rust
pub(crate) fn watch_mask() -> WatchMask {
    WatchMask::CREATE | WatchMask::MOVED_TO | WatchMask::DELETE_SELF | WatchMask::ONLYDIR
}
```

with:

```rust
pub(crate) fn watch_mask() -> WatchMask {
    WatchMask::CREATE
        | WatchMask::MOVED_TO
        | WatchMask::DELETE_SELF
        | WatchMask::MOVE_SELF
        | WatchMask::ONLYDIR
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS (both new tests green).

- [ ] **Step 5: Handle `MOVE_SELF` in the event loop**

In `src/app.rs`, inside `event_loop`, replace:

```rust
            // Remove bookkeeping for directories that disappeared to avoid stale mappings.
            if event.mask.contains(EventMask::DELETE_SELF) {
                registry.remove_by_descriptor(&event.wd);
                continue;
            }
```

with:

```rust
            // Remove bookkeeping for directories that disappeared or were moved,
            // so stale mappings can't resolve later events to the wrong path.
            if event.mask.contains(EventMask::DELETE_SELF)
                || event.mask.contains(EventMask::MOVE_SELF)
            {
                registry.remove_by_descriptor(&event.wd);
                continue;
            }
```

- [ ] **Step 6: Run the full suite and clippy**

Run: `cargo test && cargo clippy --all-targets`
Expected: PASS, no warnings.

- [ ] **Step 7: Commit**

```bash
git add src/watch.rs src/app.rs
git commit -m "fix(watch): drop stale registry entries on rename/move

Add MOVE_SELF to the watch mask and clean up the registry when a watched
directory is renamed or moved, preventing later events from resolving to a
stale path (which could tag the wrong path). Adds WatchRegistry unit tests.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: Clearer error on inotify watch limit (#3)

**Files:**
- Modify: `src/watch.rs` (add `watch_error_context` helper; rewire `add_watch`; adjust imports; extend `mod tests`)
- Test: `src/watch.rs` (inline `mod tests`)

**Interfaces:**
- Produces: `fn watch_error_context(path: &Path, err: &std::io::Error) -> String` — returns a context string that mentions `max_user_watches` when `err.raw_os_error() == Some(libc::ENOSPC)`, and a plain message otherwise.
- Consumes: `libc::ENOSPC` (dependency already present), `std::io::Error`.

- [ ] **Step 1: Write the failing tests**

Add these two tests inside the existing `#[cfg(test)] mod tests` block in `src/watch.rs` (added in Task 2):

```rust
    #[test]
    fn watch_error_context_mentions_limit_on_enospc() {
        let err = std::io::Error::from_raw_os_error(libc::ENOSPC);
        let msg = watch_error_context(Path::new("/some/dir"), &err);
        assert!(msg.contains("max_user_watches"), "got: {msg}");
    }

    #[test]
    fn watch_error_context_is_plain_for_other_errors() {
        let err = std::io::Error::from_raw_os_error(libc::EACCES);
        let msg = watch_error_context(Path::new("/some/dir"), &err);
        assert!(!msg.contains("max_user_watches"), "got: {msg}");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib watch_error_context`
Expected: FAIL to compile with "cannot find function `watch_error_context`".

- [ ] **Step 3: Add the helper**

In `src/watch.rs`, add this function just above `fn add_watch`:

```rust
/// Build the error context for a failed `add_watch`. When the kernel reports
/// ENOSPC the real cause is usually the inotify watch limit, so point the user
/// at the tunable rather than leaving a bare "No space left on device".
fn watch_error_context(path: &Path, err: &std::io::Error) -> String {
    if err.raw_os_error() == Some(libc::ENOSPC) {
        format!(
            "Failed to add watch for {} (inotify watch limit reached; increase /proc/sys/fs/inotify/max_user_watches)",
            path.display()
        )
    } else {
        format!("Failed to add watch for {}", path.display())
    }
}
```

- [ ] **Step 4: Rewire `add_watch` to use the helper**

In `src/watch.rs`, replace:

```rust
    let descriptor = watcher
        .watches()
        .add(path, watch_mask())
        .with_context(|| format!("Failed to add watch for {}", path.display()))?;
```

with:

```rust
    let descriptor = watcher
        .watches()
        .add(path, watch_mask())
        .map_err(|err| {
            let context = watch_error_context(path, &err);
            anyhow::Error::new(err).context(context)
        })?;
```

- [ ] **Step 5: Fix the now-unused `Context` import**

`with_context` was the only user of the `Context` trait in this file. In `src/watch.rs`, change the import:

```rust
use anyhow::{Context, Result};
```

to:

```rust
use anyhow::Result;
```

- [ ] **Step 6: Run tests and clippy to verify they pass**

Run: `cargo test --lib watch_error_context && cargo clippy --all-targets`
Expected: PASS, no warnings (no "unused import: Context").

- [ ] **Step 7: Run the full suite**

Run: `cargo test && cargo clippy --all-targets`
Expected: PASS, no warnings.

- [ ] **Step 8: Commit**

```bash
git add src/watch.rs
git commit -m "fix(watch): explain inotify watch limit on ENOSPC

When add_watch fails with ENOSPC, the error now points at
/proc/sys/fs/inotify/max_user_watches instead of a bare OS message.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: Fix README "Extending rules" section (#4)

**Files:**
- Modify: `README.md:35`

**Interfaces:** None (documentation only).

- [ ] **Step 1: Correct the rule-location instructions**

In `README.md`, replace:

```markdown
Add a new type implementing the `Rule` trait in `src/main.rs`, return the desired `MatchAction`, and register it in `RuleEngine::new`. The existing rules serve as templates.
```

with:

```markdown
Add a new type implementing the `Rule` trait in `src/rules.rs`, return the desired `MatchAction`, and register it in the `RuleEngine::new` call in `src/app.rs`. The existing rules serve as templates.
```

- [ ] **Step 2: Verify the referenced paths exist**

Run: `test -f src/rules.rs && test -f src/app.rs && grep -n "RuleEngine::new" src/app.rs`
Expected: prints the `RuleEngine::new(vec![` line in `src/app.rs` (confirming the doc now matches reality).

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs(readme): fix rule-location paths in Extending rules

Rules live in src/rules.rs and are registered via the RuleEngine::new call
in src/app.rs, not src/main.rs.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- #1 (continue on setxattr failure) → Task 1. ✓
- #2 (clean stale registry on rename/move; add `MOVE_SELF`) → Task 2. ✓
- #3 (clearer ENOSPC error) → Task 3. ✓
- #4 (README rule-location fix) → Task 4. ✓
- Testing plan: `WatchRegistry` unit tests → Task 2; `apply_all` continuation seam → Task 1; ENOSPC message pure helper → Task 3. ✓
- Success criteria (cargo test + clippy pass) → asserted at the end of every task. ✓

**Placeholder scan:** No TBD/TODO/"handle edge cases"/"similar to Task N". All code shown in full. ✓

**Type consistency:** `apply_all(&[PathBuf], FnMut(&Path) -> Result<()>) -> usize` used identically in Task 1 Steps 3/5/6. `watch_mask()`, `WatchRegistry::{insert,remove_by_descriptor,path_for,contains_path}` match `src/watch.rs`. `watch_error_context(&Path, &std::io::Error) -> String` defined in Task 3 Step 3 and called in Steps 1/4. `EventMask::{DELETE_SELF,MOVE_SELF}` and `WatchMask::{MOVE_SELF,DELETE_SELF}` are valid `inotify` crate variants. ✓
