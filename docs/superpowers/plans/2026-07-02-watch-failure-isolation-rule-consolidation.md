# Watch-Failure Isolation, Rule Consolidation, and Small Quality Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop transient `add_watch` failures from killing the watcher, make xattr marking idempotent, add `--version`, grow the event buffer, and consolidate the four name-list rules into one data-driven rule type.

**Architecture:** Three disjoint seams. (A) `watch.rs::add_watch` classifies errors so only ENOSPC is fatal; all call sites inherit the fix. (B) `dropbox.rs` gains a `getxattr` pre-check; `cli.rs` gains `#[command(version)]`; `app.rs` gets a 64 KiB read buffer. (C) `rules.rs` replaces `NodeModulesRule`/`PnpmStoreRule`/`PythonBuildArtifactsRule`/`JsBuildArtifactsRule` with `ArtifactDirsRule` (data-driven) plus `EggInfoRule` (suffix match); `RustTargetRule` is untouched.

**Tech Stack:** Rust (edition 2024), inotify 0.10, libc, clap 4.5 (derive), anyhow, log. Tests use `cargo test` with tempfile.

**Spec:** `docs/superpowers/specs/2026-07-02-watch-failure-isolation-rule-consolidation-design.md`

## Global Constraints

- No new external dependencies (Cargo.toml dependency list unchanged).
- No behavior change to *which* paths get marked, except already-marked paths are no longer re-written.
- Log names for the four consolidated rules stay exactly: `node_modules directory`, `pnpm store directory`, `Python build/cache artifact`, `JavaScript build/cache directory`. The egg-info rule's log name becomes `Python egg-info metadata`.
- Rule priority order in `RuleEngine::new` registration is preserved.
- After every task: `cargo test` passes and `cargo clippy --all-targets` reports no warnings.
- Commit messages follow the repo's conventional-commit style (`fix(watch): …`, `feat(cli): …`, etc.) and end with the Claude co-author trailer.

---

### Task 1: Isolate non-ENOSPC `add_watch` failures

**Files:**
- Modify: `src/watch.rs:30-51` (`add_watch`), `src/watch.rs:1-5` (imports)
- Test: `src/watch.rs` (tests module in the same file)

**Interfaces:**
- Consumes: existing `watch_error_context(path, &io::Error) -> String`, `WatchRegistry`, `watch_mask()`.
- Produces: `add_watch(&mut Inotify, &mut WatchRegistry, &Path) -> Result<()>` with a new contract: returns `Err` only when the kernel reports ENOSPC; every other registration failure is logged at `warn!` and returns `Ok(())` without touching the registry. Signature is unchanged; later tasks and existing call sites rely on this contract implicitly.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/watch.rs` (it already has `use super::*;`, which brings `Path`, `Inotify`, and `WatchRegistry` into scope):

```rust
#[test]
fn add_watch_skips_nonexistent_path() -> Result<()> {
    let mut inotify = Inotify::init()?;
    let mut registry = WatchRegistry::default();
    let missing = Path::new("/nonexistent-dropignore-test-path");

    add_watch(&mut inotify, &mut registry, missing)?;

    assert!(
        !registry.contains_path(missing),
        "a path that failed to register must not enter the registry"
    );
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test add_watch_skips_nonexistent_path`
Expected: FAIL — the test returns `Err` because `add_watch` currently propagates the ENOENT error.

- [ ] **Step 3: Write minimal implementation**

In `src/watch.rs`, change the imports line `use log::debug;` to:

```rust
use log::{debug, warn};
```

Replace the body of `add_watch` and update its doc comment:

```rust
/// Register a directory with inotify if it has not already been registered.
///
/// Contract: only ENOSPC (inotify watch limit reached) is fatal. Any other
/// failure — the directory vanished before registration (ENOENT), permission
/// denied (EACCES), a non-directory due to `ONLYDIR` (ENOTDIR), … — is logged
/// at warn and skipped, so one bad path cannot take down the watcher. This
/// mirrors the discovery walk, which warns and skips unreadable directories.
pub(crate) fn add_watch(
    watcher: &mut Inotify,
    registry: &mut WatchRegistry,
    path: &Path,
) -> Result<()> {
    if registry.contains_path(path) {
        return Ok(());
    }

    let descriptor = match watcher.watches().add(path, watch_mask()) {
        Ok(descriptor) => descriptor,
        Err(err) if err.raw_os_error() == Some(libc::ENOSPC) => {
            let context = watch_error_context(path, &err);
            return Err(anyhow::Error::new(err).context(context));
        }
        Err(err) => {
            warn!("{}: {err}; skipping", watch_error_context(path, &err));
            return Ok(());
        }
    };

    registry.insert(path.to_path_buf(), descriptor);
    debug!("Watching {}", path.display());
    Ok(())
}
```

`watch_error_context` is kept unchanged; both of its branches stay live (ENOSPC via the fatal arm, plain message via the warn arm) and both of its existing tests still apply.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test && cargo clippy --all-targets`
Expected: all tests PASS, no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add src/watch.rs
git commit -m "fix(watch): treat only ENOSPC as fatal in add_watch

A directory deleted between discovery and registration (ENOENT) or an
unreadable directory (EACCES) previously unwound the event loop and
killed the process. Log and skip those; keep the watch-limit case fatal
so coverage cannot degrade silently.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: Idempotent xattr marking

**Files:**
- Modify: `src/dropbox.rs` (whole file is 50 lines; add helper + pre-check + tests module)

**Interfaces:**
- Consumes: existing constants `DROPBOX_IGNORE_ATTR`, `DROPBOX_IGNORE_VALUE`.
- Produces: private `fn is_already_ignored(c_path: &CString, c_name: &CString) -> bool`; `apply_dropbox_ignore(&Path, bool) -> Result<()>` keeps its signature but now returns early (debug log, no write) when the attribute already holds `1`, in both real and dry-run mode.

- [ ] **Step 1: Write the failing test**

Add a `tests` module at the bottom of `src/dropbox.rs`. The xattr probe skips the test on filesystems without `user.*` xattr support (e.g. some tmpfs configurations), per the spec.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use std::fs;
    use tempfile::TempDir;

    /// True when the filesystem hosting `path` accepts user.* xattrs. Used to
    /// skip (not fail) on filesystems without support.
    fn xattr_supported(path: &Path) -> bool {
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let c_name = CString::new("user.dropignore.probe").unwrap();
        // SAFETY: pointers are valid for the duration of the call; the value
        // is one byte and the length matches.
        let result = unsafe {
            libc::setxattr(c_path.as_ptr(), c_name.as_ptr(), b"1".as_ptr().cast(), 1, 0)
        };
        result == 0
    }

    #[test]
    fn already_marked_path_is_detected_and_skipped() -> Result<()> {
        let temp = TempDir::new()?;
        let file = temp.path().join("artifact");
        fs::write(&file, b"")?;
        if !xattr_supported(temp.path()) {
            eprintln!("skipping: filesystem lacks user.* xattr support");
            return Ok(());
        }

        let c_path = CString::new(file.as_os_str().as_bytes())?;
        let c_name = CString::new(DROPBOX_IGNORE_ATTR)?;

        assert!(
            !is_already_ignored(&c_path, &c_name),
            "fresh file must not read as marked"
        );

        apply_dropbox_ignore(&file, false)?;
        assert!(
            is_already_ignored(&c_path, &c_name),
            "marked file must read as marked"
        );

        // Re-applying (real and dry-run) must succeed as a no-op.
        apply_dropbox_ignore(&file, false)?;
        apply_dropbox_ignore(&file, true)?;
        Ok(())
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test already_marked_path_is_detected_and_skipped`
Expected: FAIL to compile with "cannot find function `is_already_ignored`".

- [ ] **Step 3: Write minimal implementation**

In `src/dropbox.rs`, change `use log::{error, info};` to:

```rust
use log::{debug, error, info};
```

Add the helper above `apply_dropbox_ignore`:

```rust
/// True when the path already carries the ignore attribute with the expected
/// value. Any read failure (ENODATA, ENOTSUP, ERANGE, …) reads as "not
/// marked", so the caller falls through to setxattr, which reports real
/// errors. Watched paths are never symlinks (walk and event path both skip
/// them), so the symlink-following getxattr is safe here.
fn is_already_ignored(c_path: &CString, c_name: &CString) -> bool {
    // One byte larger than the expected value so a longer stored value yields
    // a length mismatch instead of a truncated false positive.
    let mut value = [0u8; DROPBOX_IGNORE_VALUE.len() + 1];
    // SAFETY: pointers are valid for the duration of the call and the size
    // matches the buffer.
    let len = unsafe {
        libc::getxattr(
            c_path.as_ptr(),
            c_name.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    len == DROPBOX_IGNORE_VALUE.len() as isize
        && &value[..DROPBOX_IGNORE_VALUE.len()] == DROPBOX_IGNORE_VALUE
}
```

Rewrite `apply_dropbox_ignore` so the C strings are built once and the check runs before the dry-run branch (dry-run logs must distinguish "already marked" from "would mark"):

```rust
/// Apply the Dropbox ignore attribute to the given path, honoring dry-run
/// mode. Skips the write (debug log only) when the attribute is already set,
/// so rescans do not rewrite or re-announce paths marked earlier.
pub(crate) fn apply_dropbox_ignore(path: &Path, dry_run: bool) -> Result<()> {
    // Construct C strings for the path and attribute name. Path conversion uses
    // raw bytes to support non-UTF8 names on Unix.
    let c_path = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("Path contains interior NUL byte: {}", path.display()))?;
    let c_name =
        CString::new(DROPBOX_IGNORE_ATTR).expect("static attribute name should never contain NUL");

    if is_already_ignored(&c_path, &c_name) {
        debug!("{} is already marked as ignored", path.display());
        return Ok(());
    }

    if dry_run {
        info!("(dry-run) Would mark {} as ignored", path.display());
        return Ok(());
    }

    // SAFETY: Pointers are valid for the duration of the call, sizes are correct,
    // and flags is set to 0 for "create or replace".
    let result = unsafe {
        libc::setxattr(
            c_path.as_ptr(),
            c_name.as_ptr(),
            DROPBOX_IGNORE_VALUE.as_ptr().cast(),
            DROPBOX_IGNORE_VALUE.len(),
            0,
        )
    };

    if result != 0 {
        let err = std::io::Error::last_os_error();
        error!(
            "Failed to set {} on {}: {err}",
            DROPBOX_IGNORE_ATTR,
            path.display()
        );
        return Err(err).with_context(|| format!("setxattr failed for {}", path.display()));
    }

    info!("Marked {} as ignored", path.display());
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test && cargo clippy --all-targets`
Expected: all tests PASS (or the new test prints the skip line on xattr-less filesystems), no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add src/dropbox.rs
git commit -m "feat(dropbox): skip setxattr when the path is already marked

Rescans revisit every previously marked path; checking the attribute
first avoids rewriting it and demotes the repeat log line to debug.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: `--version` flag

**Files:**
- Modify: `src/cli.rs` (17 lines; add attribute + tests module)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `dropignore --version` prints the version from `Cargo.toml` via clap's `#[command(version)]` (current clap derive API, verified against clap docs).

- [ ] **Step 1: Write the failing test**

Add a `tests` module at the bottom of `src/cli.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn version_flag_reads_cargo_metadata() {
        let cmd = CliArgs::command();
        assert_eq!(
            cmd.get_version(),
            Some(env!("CARGO_PKG_VERSION")),
            "--version must report the Cargo.toml version"
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test version_flag_reads_cargo_metadata`
Expected: FAIL — `get_version()` returns `None` because no version is configured.

- [ ] **Step 3: Write minimal implementation**

In `src/cli.rs`, add `version,` to the `#[command(...)]` attribute:

```rust
#[command(
    name = "dropignore",
    version,
    about = "Watch a directory and tag matching paths with the Dropbox ignore attribute."
)]
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test && cargo clippy --all-targets`
Expected: all tests PASS, no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add src/cli.rs
git commit -m "feat(cli): add --version flag

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: Consolidate name-list rules into `ArtifactDirsRule`

**Files:**
- Modify: `src/rules.rs` (replace `NodeModulesRule`, `PnpmStoreRule`, `PythonBuildArtifactsRule`, `JsBuildArtifactsRule` with `ArtifactDirsRule` + `EggInfoRule`; update tests)
- Modify: `src/app.rs:4-7` (imports), `src/app.rs:28-34` (registration), tests at `src/app.rs:384`, `src/app.rs:391-393`, `src/app.rs:692`
- Modify: `src/discovery.rs:113` (test imports) and test bodies referencing the old types

**Interfaces:**
- Consumes: `Rule` trait, `MatchAction::IGNORE_AND_SKIP`, `Candidate`, constants `PYTHON_ARTIFACT_DIRS` and `JS_ARTIFACT_DIRS` (both already in `rules.rs`).
- Produces:
  - `pub(crate) struct ArtifactDirsRule { name: &'static str, dirs: &'static [&'static str] }` implementing `Rule`, with associated constants `ArtifactDirsRule::NODE_MODULES`, `ArtifactDirsRule::PNPM_STORE`, `ArtifactDirsRule::PYTHON_CACHES`, `ArtifactDirsRule::JS_BUILD`.
  - `pub(crate) struct EggInfoRule;` implementing `Rule` with name `"Python egg-info metadata"`, matching any entry (dir or file) whose file name ends with `.egg-info`.
  - The old four rule types no longer exist; `RustTargetRule` is unchanged.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/rules.rs`:

```rust
#[test]
fn artifact_dirs_rule_instances_match_their_directories() -> Result<()> {
    let temp = TempDir::new().context("Failed to create temp dir")?;
    let cases: &[(&ArtifactDirsRule, &str, &str)] = &[
        (&ArtifactDirsRule::NODE_MODULES, "node_modules", "node_modules directory"),
        (&ArtifactDirsRule::PNPM_STORE, ".pnpm-store", "pnpm store directory"),
        (&ArtifactDirsRule::PYTHON_CACHES, "__pycache__", "Python build/cache artifact"),
        (&ArtifactDirsRule::JS_BUILD, ".next", "JavaScript build/cache directory"),
    ];

    for (rule, dir_name, rule_name) in cases {
        assert_eq!(rule.name(), *rule_name);
        let dir = temp.path().join(dir_name);
        fs::create_dir(&dir)?;
        let meta = fs::metadata(&dir)?;
        let candidate = Candidate { path: &dir, file_type: meta.file_type() };
        assert!(rule.matches(&candidate), "{dir_name} should match");
        assert_eq!(rule.action(), MatchAction::IGNORE_AND_SKIP);
    }
    Ok(())
}

#[test]
fn artifact_dirs_rule_ignores_file_named_like_dir() -> Result<()> {
    let temp = TempDir::new().context("Failed to create temp dir")?;
    let file = temp.path().join("node_modules");
    fs::write(&file, b"")?;
    let meta = fs::metadata(&file)?;
    let candidate = Candidate { path: &file, file_type: meta.file_type() };
    assert!(
        !ArtifactDirsRule::NODE_MODULES.matches(&candidate),
        "a regular file named node_modules must not match"
    );
    Ok(())
}

#[test]
fn egg_info_rule_matches_file_and_directory() -> Result<()> {
    let temp = TempDir::new().context("Failed to create temp dir")?;
    let egg_file = temp.path().join("pkg.egg-info");
    let egg_dir = temp.path().join("other.egg-info");
    fs::write(&egg_file, b"")?;
    fs::create_dir(&egg_dir)?;

    assert_eq!(EggInfoRule.name(), "Python egg-info metadata");
    for path in [&egg_file, &egg_dir] {
        let meta = fs::metadata(path)?;
        let candidate = Candidate { path, file_type: meta.file_type() };
        assert!(EggInfoRule.matches(&candidate), "{} should match", path.display());
    }
    assert_eq!(EggInfoRule.action(), MatchAction::IGNORE_AND_SKIP);
    Ok(())
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test artifact_dirs egg_info_rule 2>&1 | head -20` (or just `cargo test`)
Expected: FAIL to compile with "cannot find type `ArtifactDirsRule`" / "cannot find `EggInfoRule`".

- [ ] **Step 3: Implement the new rule types**

In `src/rules.rs`, add after `RustTargetRule` (keep `PYTHON_ARTIFACT_DIRS` and `JS_ARTIFACT_DIRS` with their existing doc comments):

```rust
/// Rule matching directories whose exact name is in a fixed list. This is the
/// common "tool-owned artifact directory" shape; every instance marks the
/// match and skips its descendants. Adding a directory to an existing
/// instance's list is a one-line change; a new category is a new constant.
pub(crate) struct ArtifactDirsRule {
    name: &'static str,
    dirs: &'static [&'static str],
}

impl ArtifactDirsRule {
    pub(crate) const NODE_MODULES: Self = Self {
        name: "node_modules directory",
        dirs: &["node_modules"],
    };
    pub(crate) const PNPM_STORE: Self = Self {
        name: "pnpm store directory",
        dirs: &[".pnpm-store"],
    };
    pub(crate) const PYTHON_CACHES: Self = Self {
        name: "Python build/cache artifact",
        dirs: PYTHON_ARTIFACT_DIRS,
    };
    pub(crate) const JS_BUILD: Self = Self {
        name: "JavaScript build/cache directory",
        dirs: JS_ARTIFACT_DIRS,
    };
}

impl Rule for ArtifactDirsRule {
    fn name(&self) -> &'static str {
        self.name
    }

    fn matches(&self, candidate: &Candidate<'_>) -> bool {
        if !candidate.is_dir() {
            return false;
        }
        candidate
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| self.dirs.contains(&name))
    }

    fn action(&self) -> MatchAction {
        MatchAction::IGNORE_AND_SKIP
    }
}

/// Rule matching `*.egg-info` metadata by suffix. Unlike the directory-list
/// rules this matches files as well as directories, so it stays a separate
/// type.
pub(crate) struct EggInfoRule;

impl Rule for EggInfoRule {
    fn name(&self) -> &'static str {
        "Python egg-info metadata"
    }

    fn matches(&self, candidate: &Candidate<'_>) -> bool {
        candidate
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".egg-info"))
    }

    fn action(&self) -> MatchAction {
        MatchAction::IGNORE_AND_SKIP
    }
}
```

Delete the four superseded types and their impls: `NodeModulesRule`, `PnpmStoreRule`, `PythonBuildArtifactsRule`, `JsBuildArtifactsRule`. Keep `Candidate::is_dir_named` (still used by `RustTargetRule`).

- [ ] **Step 4: Migrate all references**

`src/app.rs` — imports (top of file):

```rust
use crate::rules::{ArtifactDirsRule, Candidate, EggInfoRule, RuleEngine, RustTargetRule};
```

`src/app.rs` — registration in `run` (order preserved; `EggInfoRule` sits where the old Python rule's egg-info half lived):

```rust
let rule_engine = RuleEngine::new(vec![
    Box::new(ArtifactDirsRule::NODE_MODULES),
    Box::new(ArtifactDirsRule::PNPM_STORE),
    Box::new(RustTargetRule),
    Box::new(ArtifactDirsRule::PYTHON_CACHES),
    Box::new(EggInfoRule),
    Box::new(ArtifactDirsRule::JS_BUILD),
]);
```

`src/app.rs` tests — delete the test-module line `use crate::rules::{Candidate, NodeModulesRule, PythonBuildArtifactsRule};` and the fn-local `use crate::rules::RustTargetRule;` in `rescan_subtree_reconciles_newly_matched_sibling` (every needed name now arrives via `use super::*;` from the updated top-level import, matching the repo's redundant-import cleanup in e3afe0f); the `engine()` helper and the egg-info test change to:

```rust
fn engine() -> RuleEngine {
    RuleEngine::new(vec![Box::new(ArtifactDirsRule::NODE_MODULES)])
}
```

and in `plan_entry_applies_matched_non_directory_without_watching`:

```rust
let rules = RuleEngine::new(vec![Box::new(EggInfoRule)]);
```

`src/discovery.rs` tests — import line becomes:

```rust
use crate::rules::{ArtifactDirsRule, EggInfoRule, RuleEngine, RustTargetRule};
```

and each engine construction maps as follows:
- `Box::new(NodeModulesRule)` → `Box::new(ArtifactDirsRule::NODE_MODULES)`
- `Box::new(PythonBuildArtifactsRule)` in `discover_watch_targets_skips_python_envs` and `discover_watch_targets_skips_pycache_subtree` → `Box::new(ArtifactDirsRule::PYTHON_CACHES)`
- `Box::new(PythonBuildArtifactsRule)` in `discover_watch_targets_marks_egg_info_files` → `Box::new(EggInfoRule)`

`src/rules.rs` tests — migrate the existing tests that construct old types:
- `node_modules_rule_matches_directory_name` → build the engine with `Box::new(ArtifactDirsRule::NODE_MODULES)`.
- `pnpm_store_rule_matches_directory_name` → `Box::new(ArtifactDirsRule::PNPM_STORE)`.
- `rust_target_rule_requires_cargo_toml_in_parent` → second rule becomes `Box::new(ArtifactDirsRule::NODE_MODULES)`.
- `python_artifact_rule_matches_env_and_metadata` → engine with both `Box::new(ArtifactDirsRule::PYTHON_CACHES)` and `Box::new(EggInfoRule)`.
- `rule_engine_without_target_rule_has_no_triggers` → `Box::new(ArtifactDirsRule::NODE_MODULES)`.
- `python_artifact_rule_matches_tool_caches`, `python_artifact_rule_ignores_ordinary_directory` → `Box::new(ArtifactDirsRule::PYTHON_CACHES)`.
- `js_build_artifacts_rule_matches_framework_dirs` → `Box::new(ArtifactDirsRule::JS_BUILD)`; the asserted rule name stays `"JavaScript build/cache directory"`.
- `js_build_artifacts_rule_ignores_file_named_like_dir` → delete (superseded by `artifact_dirs_rule_ignores_file_named_like_dir` from Step 1).

- [ ] **Step 5: Run the full suite**

Run: `cargo test && cargo clippy --all-targets`
Expected: all tests PASS, no clippy warnings, no unused-import warnings.

- [ ] **Step 6: Commit**

```bash
git add src/rules.rs src/app.rs src/discovery.rs
git commit -m "refactor(rules): consolidate name-list rules into ArtifactDirsRule

Four rules shared one predicate (directory whose name is in a fixed
list). Replace them with a data-driven ArtifactDirsRule and split the
*.egg-info suffix match into EggInfoRule. Log names are unchanged except
egg-info, which now logs as 'Python egg-info metadata'. RustTargetRule
stays as the template for conditional rules.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: Grow the event buffer, apply fmt, final verification

**Files:**
- Modify: `src/app.rs:162` (buffer size in `drain_events`)
- Modify: whatever `cargo fmt` touches (known drift: `src/app.rs` import order and a struct literal)
- Modify: `README.md` (Extending-rules paragraph references the removed per-type pattern)

**Interfaces:**
- Consumes: everything from Tasks 1–4.
- Produces: final tree where `cargo fmt --check` is clean.

- [ ] **Step 1: Grow the read buffer**

In `drain_events` (`src/app.rs`), replace:

```rust
    let mut buffer = [0u8; 4096];
```

with:

```rust
    // 64 KiB drains large event bursts in fewer reads; still a single
    // stack frame's allocation.
    let mut buffer = [0u8; 65536];
```

The existing `drain_events_registers_new_dir_and_skips_ignored` test covers the read path; no new test is needed for a capacity change.

- [ ] **Step 2: Update README's rule-extension instructions**

In `README.md`, replace the "Extending rules" paragraph body with:

```markdown
For a new "ignore directories with these exact names" rule, add the name to an
existing `ArtifactDirsRule` list in `src/rules.rs` (or add a new associated
constant) and register it in `RuleEngine::new` in `src/app.rs`. For
conditional rules, implement the `Rule` trait; `RustTargetRule` is the
template.
```

- [ ] **Step 3: Apply formatting**

Run: `cargo fmt`
Then: `git diff --stat` — review that only formatting changed.

- [ ] **Step 4: Full verification**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: all tests PASS, no clippy warnings, fmt check silent.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "chore: grow inotify read buffer to 64 KiB and apply cargo fmt

Also point README's rule-extension guide at ArtifactDirsRule.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```
