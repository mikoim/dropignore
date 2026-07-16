# default_rules() Extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the production ruleset out of the untested `run()` into a pure `default_rules()` in `src/rules.rs`, guarded by a table-driven test.

**Architecture:** Extract the 18-entry `Vec<Box<dyn Rule>>` currently inline in `app.rs::run()` into a named function next to the rule definitions, then point `run()` at it. Add a behavioral test over `RuleEngine::new(default_rules())` so registration completeness, action wiring, and VCS-first ordering are covered — the production set is otherwise never exercised because `run()` is out of test scope.

**Tech Stack:** Rust (edition 2024), `tempfile` for test fixtures, existing `RuleEngine`/`Rule` types. No new dependencies.

## Global Constraints

- No change to matching behavior, rule order, or the set of rules — pure refactor plus tests; the produced engine is byte-for-byte equivalent to today's.
- No new dependencies; no public-API change (all items stay `pub(crate)`).
- Verification gate for every commit: `./scripts/check.sh` (`cargo fmt --check`, `cargo clippy --all-targets` with zero warnings, `cargo test`). The pre-commit hook runs it; `git commit` fails if the gate fails.
- The added test touches only `TempDir`-scoped rule evaluation — no inotify, xattr, or `/proc` interaction.
- The production ruleset has exactly 18 entries, in this order: `VCS_DIRS`, `NODE_MODULES`, `PNPM_STORE`, `CARGO_TARGET`, `MAVEN_TARGET`, `GRADLE_BUILD`, `PYTHON_CACHES`, `EggInfoRule`, `JS_BUILD`, `JVM_CACHES`, `IAC_CACHES`, `DEV_ENV_DIRS`, `COMPOSER_VENDOR`, `MIX_BUILD`, `MIX_DEPS`, `ZIG_OUT`, `ZIG_CACHES`, `DART_CACHES`.

---

### Task 1: Extract `default_rules()` and switch `run()` to it

**Files:**
- Modify: `src/rules.rs` (add `default_rules()` after `RuleEngine`'s `impl` block; add `use` for the rule types it references)
- Modify: `src/app.rs:25-44` (replace the inline `vec![...]`), `src/app.rs:4` (trim now-unused imports)

**Interfaces:**
- Consumes: existing `Rule` trait, `RuleEngine::new`, and the rule constants `ArtifactDirsRule::{VCS_DIRS, NODE_MODULES, PNPM_STORE, PYTHON_CACHES, JS_BUILD, JVM_CACHES, IAC_CACHES, DEV_ENV_DIRS, ZIG_CACHES, DART_CACHES}`, `MarkedBuildDirRule::{CARGO_TARGET, MAVEN_TARGET, GRADLE_BUILD, COMPOSER_VENDOR, MIX_BUILD, MIX_DEPS, ZIG_OUT}`, `EggInfoRule` — all already defined in `src/rules.rs`.
- Produces: `pub(crate) fn default_rules() -> Vec<Box<dyn Rule>>` in `src/rules.rs`.

- [ ] **Step 1: Add `default_rules()` to `src/rules.rs`**

Add this function immediately after the closing `}` of the `impl RuleEngine` block (around line 126, before the `MarkedBuildDirRule` doc comment):

```rust
/// The production ruleset, in priority order. `evaluate` returns the first
/// match, so `VCS_DIRS` stays first: a version-control directory must resolve
/// to skip-only before any marking rule can see it. This is the single
/// registration site — adding a rule means adding one line here (and its
/// constant above). Kept next to the rule definitions and out of the untested
/// `run()` so a table-driven test can exercise the real set.
pub(crate) fn default_rules() -> Vec<Box<dyn Rule>> {
    vec![
        Box::new(ArtifactDirsRule::VCS_DIRS),
        Box::new(ArtifactDirsRule::NODE_MODULES),
        Box::new(ArtifactDirsRule::PNPM_STORE),
        Box::new(MarkedBuildDirRule::CARGO_TARGET),
        Box::new(MarkedBuildDirRule::MAVEN_TARGET),
        Box::new(MarkedBuildDirRule::GRADLE_BUILD),
        Box::new(ArtifactDirsRule::PYTHON_CACHES),
        Box::new(EggInfoRule),
        Box::new(ArtifactDirsRule::JS_BUILD),
        Box::new(ArtifactDirsRule::JVM_CACHES),
        Box::new(ArtifactDirsRule::IAC_CACHES),
        Box::new(ArtifactDirsRule::DEV_ENV_DIRS),
        Box::new(MarkedBuildDirRule::COMPOSER_VENDOR),
        Box::new(MarkedBuildDirRule::MIX_BUILD),
        Box::new(MarkedBuildDirRule::MIX_DEPS),
        Box::new(MarkedBuildDirRule::ZIG_OUT),
        Box::new(ArtifactDirsRule::ZIG_CACHES),
        Box::new(ArtifactDirsRule::DART_CACHES),
    ]
}
```

The types `ArtifactDirsRule`, `MarkedBuildDirRule`, and `EggInfoRule` are all defined later in the same file, so no new `use` is needed — they are in scope module-wide.

- [ ] **Step 2: Switch `run()` in `src/app.rs` to `default_rules()`**

Replace the inline construction at `src/app.rs:25-44`:

```rust
    let rule_engine = RuleEngine::new(vec![
        Box::new(ArtifactDirsRule::VCS_DIRS),
        Box::new(ArtifactDirsRule::NODE_MODULES),
        Box::new(ArtifactDirsRule::PNPM_STORE),
        Box::new(MarkedBuildDirRule::CARGO_TARGET),
        Box::new(MarkedBuildDirRule::MAVEN_TARGET),
        Box::new(MarkedBuildDirRule::GRADLE_BUILD),
        Box::new(ArtifactDirsRule::PYTHON_CACHES),
        Box::new(EggInfoRule),
        Box::new(ArtifactDirsRule::JS_BUILD),
        Box::new(ArtifactDirsRule::JVM_CACHES),
        Box::new(ArtifactDirsRule::IAC_CACHES),
        Box::new(ArtifactDirsRule::DEV_ENV_DIRS),
        Box::new(MarkedBuildDirRule::COMPOSER_VENDOR),
        Box::new(MarkedBuildDirRule::MIX_BUILD),
        Box::new(MarkedBuildDirRule::MIX_DEPS),
        Box::new(MarkedBuildDirRule::ZIG_OUT),
        Box::new(ArtifactDirsRule::ZIG_CACHES),
        Box::new(ArtifactDirsRule::DART_CACHES),
    ]);
```

with:

```rust
    let rule_engine = RuleEngine::new(default_rules());
```

- [ ] **Step 3: Fix imports in `src/app.rs:4`**

The current import is:

```rust
use crate::rules::{ArtifactDirsRule, Candidate, EggInfoRule, MarkedBuildDirRule, RuleEngine};
```

`run()` no longer names the individual rule constants, but the `#[cfg(test)]` module in `app.rs` still uses `ArtifactDirsRule`, `EggInfoRule`, and `MarkedBuildDirRule` (e.g. `app.rs:473`, `1002`, `1069`). Those test references resolve through `use super::*;`, so the production imports must remain available to the module. Change the line to add `default_rules` and keep the rest:

```rust
use crate::rules::{
    ArtifactDirsRule, Candidate, EggInfoRule, MarkedBuildDirRule, RuleEngine, default_rules,
};
```

Do not remove `ArtifactDirsRule`, `EggInfoRule`, or `MarkedBuildDirRule`: clippy would then fail the tests with unresolved names. `clippy --all-targets` (in the gate) compiles the test target, so any genuinely unused import surfaces as a zero-warnings failure and tells you exactly which to drop.

- [ ] **Step 4: Run the gate to verify the refactor is behavior-preserving**

Run: `./scripts/check.sh`
Expected: PASS — `cargo fmt --check` clean, `cargo clippy --all-targets` zero warnings, all 72 existing tests pass. No test asserts on `default_rules()` yet; this step proves the extraction compiles and the existing suite is still green.

- [ ] **Step 5: Commit**

```bash
git add src/rules.rs src/app.rs
git commit -m "refactor: extract production ruleset into default_rules()

Move the 18-entry Vec<Box<dyn Rule>> out of the untested run() into a
pure default_rules() in src/rules.rs, the single rule-registration site.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Table-driven test over `default_rules()`

**Files:**
- Modify: `src/rules.rs` — add tests inside the existing `#[cfg(test)] mod tests` block (append before its closing `}` at line ~1028)
- Test: `src/rules.rs` (same file; rule tests already live here)

**Interfaces:**
- Consumes: `default_rules()` from Task 1; existing `RuleEngine::new`, `Candidate`, `MatchAction::{IGNORE_AND_SKIP, SKIP_ONLY}`, and `tempfile::TempDir` (already imported in the test module at `src/rules.rs:376-379`).
- Produces: nothing consumed downstream (leaf tests).

- [ ] **Step 1: Write the failing tests**

Append these two tests inside `mod tests`, before its closing brace. The `tempfile::TempDir`, `anyhow::{Context, Result}`, `fs`, and `super::*` imports the module already has cover everything used here.

```rust
    #[test]
    fn default_rules_has_expected_count() {
        // Tripwire: an accidental duplicate or dropped rule changes this
        // count. Bump it deliberately when adding a rule.
        assert_eq!(
            default_rules().len(),
            18,
            "the production ruleset must have exactly 18 rules"
        );
    }

    #[test]
    fn default_rules_engine_matches_representative_fixtures() -> Result<()> {
        let engine = RuleEngine::new(default_rules());

        // Each fixture lives in its own TempDir so sibling markers never leak
        // (e.g. a stray Cargo.toml making an unrelated `target` match).
        // (dir_name, create_marker, is_file, expected_action)
        // expected_action = None means "must not match".
        struct Case {
            entry: &'static str,
            marker: Option<&'static str>,
            is_file: bool,
            expected: Option<MatchAction>,
        }
        let cases = [
            Case {
                entry: "node_modules",
                marker: None,
                is_file: false,
                expected: Some(MatchAction::IGNORE_AND_SKIP),
            },
            Case {
                entry: "target",
                marker: Some("Cargo.toml"),
                is_file: false,
                expected: Some(MatchAction::IGNORE_AND_SKIP),
            },
            Case {
                entry: "target",
                marker: None,
                is_file: false,
                expected: None,
            },
            Case {
                entry: ".venv",
                marker: None,
                is_file: false,
                expected: Some(MatchAction::IGNORE_AND_SKIP),
            },
            Case {
                entry: "pkg.egg-info",
                marker: None,
                is_file: true,
                expected: Some(MatchAction::IGNORE_AND_SKIP),
            },
            Case {
                entry: ".git",
                marker: None,
                is_file: false,
                expected: Some(MatchAction::SKIP_ONLY),
            },
            Case {
                entry: "src",
                marker: None,
                is_file: false,
                expected: None,
            },
        ];

        for case in cases {
            let temp = TempDir::new().context("Failed to create temp dir")?;
            if let Some(marker) = case.marker {
                fs::write(temp.path().join(marker), b"")?;
            }
            let path = temp.path().join(case.entry);
            if case.is_file {
                fs::write(&path, b"")?;
            } else {
                fs::create_dir(&path)?;
            }

            let meta = fs::symlink_metadata(&path)?;
            let candidate = Candidate {
                path: &path,
                file_type: meta.file_type(),
            };
            let result = engine.evaluate(&candidate);

            match case.expected {
                Some(action) => {
                    let matched = result.unwrap_or_else(|| {
                        panic!("{} must match a rule", case.entry)
                    });
                    assert_eq!(
                        matched.action, action,
                        "{} must resolve to {:?}",
                        case.entry, action
                    );
                }
                None => assert!(
                    result.is_none(),
                    "{} must not match any rule",
                    case.entry
                ),
            }
        }
        Ok(())
    }
```

- [ ] **Step 2: Run the tests to verify they compile and pass**

Run: `cargo test --lib default_rules`
Expected: both `default_rules_has_expected_count` and `default_rules_engine_matches_representative_fixtures` PASS. (They pass immediately rather than fail-first: `default_rules()` already exists from Task 1 and the assertions encode current behavior. The tests are regression guards, not new behavior. If either fails, the extraction in Task 1 diverged from the original list or order — fix Task 1, not the test.)

- [ ] **Step 3: Run the full gate**

Run: `./scripts/check.sh`
Expected: PASS — fmt clean, clippy zero warnings, 74 tests pass (72 existing + 2 new).

- [ ] **Step 4: Commit**

```bash
git add src/rules.rs
git commit -m "test: cover the production ruleset via default_rules()

Table-driven test asserting registration completeness, action wiring, and
VCS-first ordering over RuleEngine::new(default_rules()), plus a length
tripwire. Closes the coverage gap left by run() being out of test scope.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Update the README registration pointer

**Files:**
- Modify: `README.md:71-78` (the "Extending rules" section)

**Interfaces:**
- Consumes: nothing.
- Produces: nothing (docs only).

- [ ] **Step 1: Update the "Extending rules" text**

In `README.md`, the section currently ends:

```
constant; its markers automatically become rescan triggers. Register new
constants in `RuleEngine::new` in `src/app.rs`. For anything else, implement
the `Rule` trait directly.
```

Replace `Register new constants in `RuleEngine::new` in `src/app.rs`.` with:

```
Register new constants in `default_rules` in `src/rules.rs`.
```

so the full tail reads:

```
constant; its markers automatically become rescan triggers. Register new
constants in `default_rules` in `src/rules.rs`. For anything else, implement
the `Rule` trait directly.
```

- [ ] **Step 2: Verify the pointer is accurate**

Run: `grep -n "default_rules" README.md src/rules.rs`
Expected: README references `default_rules`, and `src/rules.rs` defines `pub(crate) fn default_rules`. No lingering `RuleEngine::new` in `src/app.rs` reference in the README (confirm with `grep -n "RuleEngine::new. in .src/app.rs." README.md` returning nothing).

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: point rule registration at default_rules() in rules.rs

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- Design §1 (`default_rules()` in rules.rs) → Task 1 Step 1. ✓
- Design §2 (`run()` uses it, imports trimmed) → Task 1 Steps 2–3. ✓
- Design §3 (table-driven test + `len() == 18` tripwire) → Task 2 Steps 1. ✓
- Design §4 (README update) → Task 3. ✓
- Non-goals (no behavior/order change, no introspection API, no new deps) → enforced by Global Constraints and the behavior-preserving gate in Task 1 Step 4. ✓
- Testing section (gate, temp-only test) → Task 2 Step 3 and Global Constraints. ✓

**Placeholder scan:** No TBD/TODO/"handle edge cases"/vague steps; every code step shows complete code. ✓

**Type consistency:** `default_rules() -> Vec<Box<dyn Rule>>` is named identically in Task 1 (produced), Task 1 Step 3 (imported), and Task 2 (consumed). Rule constant names match those verified present in `src/rules.rs`. `MatchAction::{IGNORE_AND_SKIP, SKIP_ONLY}` and `Candidate` field names (`path`, `file_type`) match the existing definitions. Count `18` is consistent between the Global Constraints list, Task 1's vector, and Task 2's tripwire. ✓
