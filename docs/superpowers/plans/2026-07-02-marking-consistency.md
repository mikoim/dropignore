# Marking Consistency Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the initial filesystem scan mark the same paths the runtime inotify path would, and reconcile order-dependent rules (Cargo `target`) when their dependency file appears later.

**Architecture:** Two changes to existing modules. (A) `Candidate` carries an owned `FileType` instead of borrowed `Metadata`, and `discover_watch_targets` evaluates non-directory entries so rule-matching files (`*.egg-info`) present at startup are marked. (B) The `Rule` trait gains a defaulted `triggers()` method; `RuleEngine` aggregates trigger filenames and exposes `is_trigger`; `event_loop` reuses its existing end-of-batch `needs_rescan`/`rebuild_watches` path when a trigger file is created.

**Tech Stack:** Rust 2024 edition, `inotify` 0.10, `libc`, `anyhow`, `log`, `tempfile` (dev). No new dependencies.

## Global Constraints

- No new external dependencies (Cargo.toml unchanged).
- No CLI, config, or on-disk format changes — behavior-only improvements.
- Keep rule-specific filenames out of the event loop; dependency knowledge lives in the owning `Rule`.
- All existing tests stay green; `cargo build` produces no new warnings.
- Rules use first-match-wins ordering; `MatchAction::IGNORE_AND_SKIP` = mark + skip descendants.

---

### Task 1: Refactor `Candidate` to hold `FileType`

Pure refactor. `Candidate` currently borrows `&fs::Metadata`, but rules only need entry type + name. Switch to an owned `std::fs::FileType` (`Copy`) so a file `Candidate` can be built from a readdir entry with no extra syscall. Existing tests are the safety net; all construction sites and their tests are updated in lockstep.

**Files:**
- Modify: `src/rules.rs` (struct + impl, ~lines 6-24; test construction sites)
- Modify: `src/discovery.rs` (popped-node construction, ~line 30-33)
- Modify: `src/app.rs` (`event_loop` construction ~line 143-146; test construction sites)

**Interfaces:**
- Produces: `Candidate<'a> { path: &'a Path, file_type: std::fs::FileType }` with `is_dir(&self) -> bool`, `is_symlink(&self) -> bool`, `is_dir_named(&self, &str) -> bool` (signatures unchanged; internals now read `file_type`).

- [ ] **Step 1: Change the `Candidate` struct and accessors**

In `src/rules.rs`, replace the struct and impl (currently lines 6-24):

```rust
/// Representation of a filesystem entry that can be evaluated against rules.
#[derive(Debug)]
pub(crate) struct Candidate<'a> {
    pub(crate) path: &'a Path,
    pub(crate) file_type: fs::FileType,
}

impl Candidate<'_> {
    pub(crate) fn is_dir(&self) -> bool {
        self.file_type.is_dir()
    }

    pub(crate) fn is_symlink(&self) -> bool {
        self.file_type.is_symlink()
    }

    pub(crate) fn is_dir_named(&self, name: &str) -> bool {
        self.is_dir() && self.path.file_name().is_some_and(|entry| entry == name)
    }
}
```

- [ ] **Step 2: Update the `discovery.rs` popped-node construction**

In `src/discovery.rs`, the popped-node `Candidate` (currently lines 30-33) becomes:

```rust
        let candidate = Candidate {
            path: &dir,
            file_type: metadata.file_type(),
        };
```

(`metadata` is the existing `fs::metadata(&dir)` result just above; keep that call — it doubles as an existence check.)

- [ ] **Step 3: Update the `app.rs` event_loop construction**

In `src/app.rs` `event_loop`, the runtime `Candidate` (currently lines 143-146) becomes:

```rust
            let candidate = Candidate {
                path: &full_path,
                file_type: metadata.file_type(),
            };
```

(`metadata` is the existing `fs::symlink_metadata(&full_path)` result; using its `file_type()` preserves symlink detection in `plan_entry`.)

- [ ] **Step 4: Update every `Candidate` construction in tests**

Replace each `metadata: &metadata` (and `metadata: &<name>_meta`) with the matching `file_type: <that metadata>.file_type()`:

- `src/rules.rs` tests: `node_modules_rule_matches_directory_name`, `pnpm_store_rule_matches_directory_name`, `rust_target_rule_requires_cargo_toml_in_parent` — each has one `Candidate { path: &target, metadata: &metadata }` → `Candidate { path: &target, file_type: metadata.file_type() }`.
- `src/rules.rs` test `python_artifact_rule_matches_env_and_metadata` — two sites:
  - `Candidate { path: &venv_dir, metadata: &venv_meta }` → `Candidate { path: &venv_dir, file_type: venv_meta.file_type() }`
  - `Candidate { path: &egg_info_dir, metadata: &egg_meta }` → `Candidate { path: &egg_info_dir, file_type: egg_meta.file_type() }`
- `src/app.rs` tests: `plan_entry_skips_symlink_to_directory`, `plan_entry_watches_plain_directory`, `plan_entry_applies_and_skips_matched_directory`, `plan_entry_applies_matched_non_directory_without_watching` — each `Candidate { path: &X, metadata: &metadata }` → `Candidate { path: &X, file_type: metadata.file_type() }`.

- [ ] **Step 5: Run the full suite to verify the refactor is green**

Run: `cargo test`
Expected: PASS — all existing tests compile and pass unchanged in behavior.

- [ ] **Step 6: Confirm no new warnings**

Run: `cargo build 2>&1 | grep -i warning || echo "no warnings"`
Expected: `no warnings`

- [ ] **Step 7: Commit**

```bash
git add src/rules.rs src/discovery.rs src/app.rs
git commit -m "refactor(rules): make Candidate carry FileType instead of Metadata

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: Discovery marks rule-matching files

Add file evaluation to `discover_watch_targets` so `*.egg-info` files present at startup are marked, matching the runtime path.

**Files:**
- Modify: `src/discovery.rs` (child-entry loop, ~lines 76-87)
- Test: `src/discovery.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `Candidate { path, file_type }` from Task 1; `RuleEngine::evaluate`, `RuleMatch::log_matched`, `MatchAction::set_dropbox_ignore`.
- Produces: no signature change — `DiscoveredPaths.matches` now additionally includes matched non-directory entries.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/discovery.rs` (`PythonBuildArtifactsRule` is already imported there):

```rust
    #[test]
    fn discover_watch_targets_marks_egg_info_files() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let top = temp.path().join("pkg.egg-info");
        let sub = temp.path().join("sub");
        let nested = sub.join("inner.egg-info");
        fs::create_dir(&sub)?;
        fs::write(&top, b"")?;
        fs::write(&nested, b"")?;

        let engine = RuleEngine::new(vec![Box::new(PythonBuildArtifactsRule)]);
        let discovered = discover_watch_targets(temp.path(), &engine)?;

        assert!(
            discovered.matches.contains(&top),
            "top-level *.egg-info file must be marked"
        );
        assert!(
            discovered.matches.contains(&nested),
            "nested *.egg-info file must be marked"
        );
        Ok(())
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test discover_watch_targets_marks_egg_info_files`
Expected: FAIL — the assertions fail because file entries are never evaluated (`matches` does not contain the files).

- [ ] **Step 3: Evaluate non-directory entries in the walk**

In `src/discovery.rs`, replace the tail of the child-entry loop (currently lines 76-87):

```rust
            if file_type.is_symlink() {
                debug!(
                    "Ignoring symlink {} to avoid cycles",
                    entry.path().display()
                );
                continue;
            }

            if file_type.is_dir() {
                stack.push(entry.path());
                continue;
            }

            // Non-directory entry: evaluate now so files like *.egg-info that
            // already exist at startup are marked, mirroring the runtime path
            // in `plan_entry`. Files have no descendants, so `skip_descendants`
            // is irrelevant here.
            let path = entry.path();
            let candidate = Candidate {
                path: &path,
                file_type,
            };
            if let Some(matched) = rules.evaluate(&candidate) {
                matched.log_matched(&path);
                if matched.action.set_dropbox_ignore {
                    discovered.matches.push(path);
                }
            }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test discover_watch_targets_marks_egg_info_files`
Expected: PASS

- [ ] **Step 5: Run the full suite**

Run: `cargo test`
Expected: PASS — existing discovery tests (which assert on directories) are unaffected.

- [ ] **Step 6: Commit**

```bash
git add src/discovery.rs
git commit -m "fix(discovery): mark rule-matching files during initial scan

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: Rules declare trigger filenames

Add a defaulted `triggers()` to the `Rule` trait, have `RustTargetRule` declare `Cargo.toml`, and let `RuleEngine` aggregate triggers and answer `is_trigger`.

**Files:**
- Modify: `src/rules.rs` (imports; `Rule` trait; `RuleEngine` struct/`new`; `RustTargetRule` impl; tests)

**Interfaces:**
- Produces:
  - `Rule::triggers(&self) -> &'static [&'static str]` (default `&[]`)
  - `RuleEngine::is_trigger(&self, name: &std::ffi::OsStr) -> bool`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/rules.rs` (`NodeModulesRule` and `RustTargetRule` are reachable via `use super::*`; add `OsStr` via the import in Step 3):

```rust
    #[test]
    fn rust_target_rule_declares_cargo_toml_trigger() {
        assert_eq!(RustTargetRule.triggers(), &["Cargo.toml"]);
    }

    #[test]
    fn rule_engine_recognizes_cargo_toml_trigger() {
        let engine = RuleEngine::new(vec![Box::new(RustTargetRule)]);
        assert!(engine.is_trigger(OsStr::new("Cargo.toml")));
        assert!(!engine.is_trigger(OsStr::new("package.json")));
    }

    #[test]
    fn rule_engine_without_target_rule_has_no_triggers() {
        let engine = RuleEngine::new(vec![Box::new(NodeModulesRule)]);
        assert!(!engine.is_trigger(OsStr::new("Cargo.toml")));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test triggers`
Expected: FAIL to compile — `triggers`, `is_trigger`, and `OsStr` do not exist yet.

- [ ] **Step 3: Add imports**

At the top of `src/rules.rs`, alongside the existing `use` lines, add:

```rust
use std::collections::HashSet;
use std::ffi::OsStr;
```

- [ ] **Step 4: Add `triggers()` to the `Rule` trait**

In the `Rule` trait (currently lines 56-63), add the defaulted method after `action`:

```rust
    /// Filenames whose creation may change this rule's verdict for a sibling.
    /// Creating any of these under a watched directory schedules a rescan.
    fn triggers(&self) -> &'static [&'static str] {
        &[]
    }
```

- [ ] **Step 5: Declare the trigger on `RustTargetRule`**

In `impl Rule for RustTargetRule`, after the `action` method, add:

```rust
    fn triggers(&self) -> &'static [&'static str] {
        &["Cargo.toml"]
    }
```

- [ ] **Step 6: Aggregate triggers in `RuleEngine`**

Replace the `RuleEngine` struct and `new` (currently lines 66-73):

```rust
/// Simple rule engine that evaluates candidates against registered rules.
pub(crate) struct RuleEngine {
    rules: Vec<Box<dyn Rule>>,
    triggers: HashSet<&'static str>,
}

impl RuleEngine {
    pub(crate) fn new(rules: Vec<Box<dyn Rule>>) -> Self {
        let triggers = rules
            .iter()
            .flat_map(|rule| rule.triggers().iter().copied())
            .collect();
        Self { rules, triggers }
    }

    /// True when `name` is a dependency filename declared by some rule. A
    /// created entry with this name should schedule a rescan so order-dependent
    /// rules (e.g. Cargo `target`) are reconciled. Non-UTF-8 names never match.
    pub(crate) fn is_trigger(&self, name: &OsStr) -> bool {
        name.to_str().is_some_and(|name| self.triggers.contains(name))
    }
```

(Leave the existing `evaluate` method and the closing `}` of the impl block in place, directly after `is_trigger`.)

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test triggers`
Expected: PASS

- [ ] **Step 8: Run the full suite**

Run: `cargo test`
Expected: PASS

- [ ] **Step 9: Commit**

```bash
git add src/rules.rs
git commit -m "feat(rules): let rules declare trigger filenames for rescan

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: Event loop rescans on trigger-file creation

Wire `is_trigger` into `event_loop`: when a created entry's name is a trigger, set the existing `needs_rescan` flag so the batch ends in `rebuild_watches(root)`. The event loop is a blocking I/O loop and is not unit-testable; its correctness rests on the `is_trigger` unit tests from Task 3 plus the existing `rebuild_watches`/rescan tests. This task's verification is a clean build and a green suite.

**Files:**
- Modify: `src/app.rs` (`event_loop`, after the `name` match block ~lines 123-131)

**Interfaces:**
- Consumes: `RuleEngine::is_trigger` (Task 3); the existing local `needs_rescan: bool` and `root: PathBuf`; `log::info` (already imported).

- [ ] **Step 1: Add the trigger check**

In `src/app.rs` `event_loop`, immediately after the `name` match block (currently ending at line 129 with the closing `};`) and before `let full_path = parent_dir.join(name);`, insert:

```rust
            // A dependency file (e.g. Cargo.toml) can flip an order-dependent
            // rule's verdict for a sibling that already exists and is watched.
            // Reuse the overflow rescan path to reconcile the whole tree; the
            // check runs before the metadata read so a transient stat failure on
            // the trigger file still schedules the rescan.
            if rule_engine.is_trigger(name) {
                info!(
                    "Trigger file {} created; rescanning {} to reconcile dependent rules",
                    parent_dir.join(name).display(),
                    root.display()
                );
                needs_rescan = true;
            }
```

- [ ] **Step 2: Build to verify it compiles**

Run: `cargo build`
Expected: builds cleanly (the `&&OsStr` binding from `match &event.name` coerces to the `&OsStr` parameter of `is_trigger`).

- [ ] **Step 3: Confirm no new warnings**

Run: `cargo build 2>&1 | grep -i warning || echo "no warnings"`
Expected: `no warnings`

- [ ] **Step 4: Run the full suite**

Run: `cargo test`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "feat(app): rescan when a rule trigger file is created

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 5: Refresh README

Document the two behavior changes so the README matches the code.

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update the "How it works" list**

In `README.md`, in the "How it works" numbered list, update item 1 (the seed-watches line) to note that files are marked too, and add a clause about trigger-file rescans. Replace item 1:

```markdown
1. Seeds watches for all traversable subdirectories under the root, skipping any directory matched by a rule, and marks any rule-matching file or directory that already exists (e.g. a pre-existing `*.egg-info`).
```

And extend item 4 (the overflow line) with a trailing sentence:

```markdown
4. Skips symlinks (matching the initial walk) and, if the inotify event queue overflows, re-scans from the root so dropped events cannot leave paths unmarked. Creating a rule's dependency file (e.g. a `Cargo.toml` next to an existing `target`) also triggers a re-scan so order-dependent rules are reconciled without a restart.
```

- [ ] **Step 2: Verify the doc reads correctly**

Run: `grep -n "dependency file\|already exists" README.md`
Expected: both new phrases are present.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs(readme): note startup file marking and trigger rescan

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- A — `Candidate` → `FileType`: Task 1. File evaluation in discovery: Task 2. Construction-site updates: Task 1 Step 4. ✓
- B — `Rule::triggers` + `RustTargetRule` declaration + `RuleEngine` aggregation + `is_trigger`: Task 3. Event-loop `needs_rescan` wiring: Task 4. ✓
- Error handling — A adds no fallible ops (Task 2 uses in-hand `file_type`); B reuses `rebuild_watches` and checks the trigger before the metadata read (Task 4 Step 1 comment). ✓
- Testing — discovery file test (Task 2), `is_trigger`/`triggers` tests (Task 3), existing `discover_watch_targets_handles_cargo_target` covers post-`Cargo.toml` marking. ✓
- Docs — README updated (Task 5), beyond the spec but keeps docs truthful (boy-scout). ✓
- Non-goals (B-scoped, config, signals, cross-platform) — no task touches them. ✓

**Placeholder scan:** No TBD/TODO; every code step shows complete code; every command has expected output. ✓

**Type consistency:** `Candidate { path, file_type }` used identically in Tasks 1–2. `triggers(&self) -> &'static [&'static str]` and `is_trigger(&self, &OsStr) -> bool` defined in Task 3 and consumed in Task 4 with matching signatures. `needs_rescan`/`root` referenced in Task 4 exist in the current `event_loop`. ✓
