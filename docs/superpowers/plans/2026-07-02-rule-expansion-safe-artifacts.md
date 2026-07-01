# Safe Reproducible-Artifact Rule Expansion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Mark additional reproducible Python and JavaScript artifact directories with the Dropbox ignore attribute out of the box.

**Architecture:** Additive rule change. Extend the existing `PythonBuildArtifactsRule` with tool-cache directory names and add a new `JsBuildArtifactsRule`, both using the established exact-name `IGNORE_AND_SKIP` pattern. Register the new rule in the engine and update the README. No new runtime state, error paths, or dependencies.

**Tech Stack:** Rust 2024 edition, `inotify`, `libc`, `anyhow`, `clap`, `env_logger`; tests use `tempfile`.

## Global Constraints

- Safe layer only: every added name is a fixed, tool-owned directory that never contains user source. No ambiguous/guarded names (`dist`, `build`, `.gradle`) and no sibling names outside the approved set.
- Python names (folded into `PythonBuildArtifactsRule`): `__pycache__`, `.pytest_cache`, `.mypy_cache`, `.ruff_cache`, `.tox`.
- JavaScript names (new `JsBuildArtifactsRule`): `.next`, `.nuxt`, `.turbo`, `.parcel-cache`.
- All new matches return `MatchAction::IGNORE_AND_SKIP` and declare no `triggers()`.
- Follow the "one rule struct per concept" pattern; keep `node_modules`/`.pnpm-store` in their existing dedicated rules (no churn).
- No new crate dependencies.

---

### Task 1: Extend `PythonBuildArtifactsRule` with tool caches

**Files:**
- Modify: `src/rules.rs` (the `PythonBuildArtifactsRule::matches` impl, ~lines 517-544; add a module-level `const`)
- Test: `src/rules.rs` (`#[cfg(test)] mod tests`), `src/discovery.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: existing `Candidate`, `RuleEngine`, `RuleMatch`, `MatchAction::IGNORE_AND_SKIP`.
- Produces: `PythonBuildArtifactsRule` now additionally matches directories named `__pycache__`, `.pytest_cache`, `.mypy_cache`, `.ruff_cache`, `.tox`. Public surface unchanged (same struct, same `Rule` impl).

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/rules.rs`:

```rust
    #[test]
    fn python_artifact_rule_matches_tool_caches() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let engine = RuleEngine::new(vec![Box::new(PythonBuildArtifactsRule)]);

        for name in [
            "__pycache__",
            ".pytest_cache",
            ".mypy_cache",
            ".ruff_cache",
            ".tox",
        ] {
            let dir = temp.path().join(name);
            fs::create_dir(&dir)?;
            let meta = fs::metadata(&dir)?;
            let candidate = Candidate {
                path: &dir,
                file_type: meta.file_type(),
            };
            let result = engine
                .evaluate(&candidate)
                .unwrap_or_else(|| panic!("{name} should match"));
            assert!(result.action.set_dropbox_ignore, "{name} must be marked");
            assert!(result.action.skip_descendants, "{name} must skip descendants");
        }
        Ok(())
    }

    #[test]
    fn python_artifact_rule_ignores_ordinary_directory() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let dir = temp.path().join("src");
        fs::create_dir(&dir)?;
        let meta = fs::metadata(&dir)?;
        let candidate = Candidate {
            path: &dir,
            file_type: meta.file_type(),
        };
        let engine = RuleEngine::new(vec![Box::new(PythonBuildArtifactsRule)]);
        assert!(engine.evaluate(&candidate).is_none(), "src must not match");
        Ok(())
    }
```

Add to the `tests` module in `src/discovery.rs`:

```rust
    #[test]
    fn discover_watch_targets_skips_pycache_subtree() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let cache = temp.path().join("__pycache__");
        let nested = cache.join("sub");
        fs::create_dir(&cache)?;
        fs::create_dir(&nested)?;

        let engine = RuleEngine::new(vec![Box::new(PythonBuildArtifactsRule)]);
        let discovered = discover_watch_targets(temp.path(), &engine)?;

        assert!(
            discovered.matches.contains(&cache),
            "__pycache__ must be marked"
        );
        assert!(
            !discovered.watchers.contains(&cache),
            "__pycache__ must not be watched"
        );
        assert!(
            !discovered.watchers.contains(&nested),
            "__pycache__ child must not be watched"
        );
        Ok(())
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib python_artifact_rule_matches_tool_caches skips_pycache_subtree`
Expected: FAIL — `__pycache__` etc. do not yet match (`evaluate` returns `None`; the `unwrap_or_else`/`assert` panics, and `matches` does not contain `cache`).

- [ ] **Step 3: Implement the minimal change**

Add this `const` above the `PythonBuildArtifactsRule` struct in `src/rules.rs`:

```rust
/// Reproducible Python environment and tool-cache directories matched by exact
/// name. Each is tool-owned and never contains user source, so it is safe to
/// mark and skip. Verified against Ruff's default `exclude` list.
const PYTHON_ARTIFACT_DIRS: &[&str] = &[
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
];
```

Replace the body of `PythonBuildArtifactsRule::matches` with:

```rust
    fn matches(&self, candidate: &Candidate<'_>) -> bool {
        let Some(file_name) = candidate.path.file_name() else {
            return false;
        };

        let name = file_name.to_string_lossy();

        // Reproducible environment/cache directories matched by exact name.
        if candidate.is_dir() && PYTHON_ARTIFACT_DIRS.contains(&name.as_ref()) {
            return true;
        }

        // egg-info metadata can be a directory or file; match by suffix. This
        // intentionally excludes other transient caches to keep the rule scoped.
        if name.ends_with(".egg-info") {
            return true;
        }

        false
    }
```

- [ ] **Step 4: Run the full test suite to verify it passes**

Run: `cargo test`
Expected: PASS — all tests green, including the two new `rules.rs` tests and the new `discovery.rs` test. The pre-existing `python_artifact_rule_matches_env_and_metadata` still passes (`.venv`/`.egg-info` unchanged).

- [ ] **Step 5: Lint**

Run: `cargo clippy --all-targets`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add src/rules.rs src/discovery.rs
git commit -m "$(cat <<'EOF'
feat(rules): mark Python tool cache directories

Extends PythonBuildArtifactsRule to match __pycache__, .pytest_cache,
.mypy_cache, .ruff_cache, and .tox by exact name.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Add and register `JsBuildArtifactsRule`

**Files:**
- Modify: `src/rules.rs` (add `const` + new struct/impl after `PythonBuildArtifactsRule`, before the `tests` module)
- Modify: `src/app.rs` (import at ~line 966-968; `RuleEngine::new` vector at ~lines 985-990)
- Test: `src/rules.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `Candidate`, `Rule`, `MatchAction::IGNORE_AND_SKIP`, `RuleEngine`.
- Produces: `pub(crate) struct JsBuildArtifactsRule;` implementing `Rule` with `name()` == `"JavaScript build/cache directory"`, matching directories named `.next`, `.nuxt`, `.turbo`, `.parcel-cache`. Registered in `app::run`'s engine.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/rules.rs`:

```rust
    #[test]
    fn js_build_artifacts_rule_matches_framework_dirs() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let engine = RuleEngine::new(vec![Box::new(JsBuildArtifactsRule)]);

        for name in [".next", ".nuxt", ".turbo", ".parcel-cache"] {
            let dir = temp.path().join(name);
            fs::create_dir(&dir)?;
            let meta = fs::metadata(&dir)?;
            let candidate = Candidate {
                path: &dir,
                file_type: meta.file_type(),
            };
            let result = engine
                .evaluate(&candidate)
                .unwrap_or_else(|| panic!("{name} should match"));
            assert_eq!(result.name, "JavaScript build/cache directory");
            assert!(result.action.set_dropbox_ignore, "{name} must be marked");
            assert!(result.action.skip_descendants, "{name} must skip descendants");
        }
        Ok(())
    }

    #[test]
    fn js_build_artifacts_rule_ignores_file_named_like_dir() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let file = temp.path().join(".turbo");
        fs::write(&file, b"")?;
        let meta = fs::metadata(&file)?;
        let candidate = Candidate {
            path: &file,
            file_type: meta.file_type(),
        };
        let engine = RuleEngine::new(vec![Box::new(JsBuildArtifactsRule)]);
        assert!(
            engine.evaluate(&candidate).is_none(),
            "a regular file named .turbo must not match"
        );
        Ok(())
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib js_build_artifacts_rule`
Expected: FAIL to compile — `JsBuildArtifactsRule` is not yet defined (`cannot find value/type JsBuildArtifactsRule`).

- [ ] **Step 3: Implement the rule**

Add to `src/rules.rs`, immediately after the `PythonBuildArtifactsRule` `impl` block and before `#[cfg(test)]`:

```rust
/// JavaScript framework build output and tool cache directories matched by
/// exact name. Each is reproducible and never holds user source. `.turbo` is
/// Turborepo's local cache (verified against its docs).
const JS_ARTIFACT_DIRS: &[&str] = &[".next", ".nuxt", ".turbo", ".parcel-cache"];

/// Rule that matches JavaScript build/cache directories by exact name.
pub(crate) struct JsBuildArtifactsRule;

impl Rule for JsBuildArtifactsRule {
    fn name(&self) -> &'static str {
        "JavaScript build/cache directory"
    }

    fn matches(&self, candidate: &Candidate<'_>) -> bool {
        if !candidate.is_dir() {
            return false;
        }
        candidate
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| JS_ARTIFACT_DIRS.contains(&name))
    }

    fn action(&self) -> MatchAction {
        MatchAction::IGNORE_AND_SKIP
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib js_build_artifacts_rule`
Expected: PASS — both new tests green.

- [ ] **Step 5: Register the rule in the engine**

In `src/app.rs`, update the `use crate::rules::{...}` import to include `JsBuildArtifactsRule`:

```rust
use crate::rules::{
    Candidate, JsBuildArtifactsRule, NodeModulesRule, PnpmStoreRule, PythonBuildArtifactsRule,
    RuleEngine, RustTargetRule,
};
```

Then add `JsBuildArtifactsRule` to the engine vector in `run`:

```rust
    let rule_engine = RuleEngine::new(vec![
        Box::new(NodeModulesRule),
        Box::new(PnpmStoreRule),
        Box::new(RustTargetRule),
        Box::new(PythonBuildArtifactsRule),
        Box::new(JsBuildArtifactsRule),
    ]);
```

- [ ] **Step 6: Verify the whole suite builds and passes**

Run: `cargo test && cargo clippy --all-targets`
Expected: all tests PASS; clippy reports no warnings. (Compilation proves the rule is wired into `app::run`.)

- [ ] **Step 7: Commit**

```bash
git add src/rules.rs src/app.rs
git commit -m "$(cat <<'EOF'
feat(rules): add JavaScript build/cache directory rule

Adds JsBuildArtifactsRule matching .next, .nuxt, .turbo, and
.parcel-cache by exact directory name, and registers it in the engine.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Update the README rule enumeration

**Files:**
- Modify: `README.md` (the Features list line describing current rules, ~line 6)

**Interfaces:**
- Consumes: nothing.
- Produces: user-facing docs listing the newly supported directories. No code impact.

- [ ] **Step 1: Update the Features bullet**

In `README.md`, replace the rule-list bullet:

```markdown
- Rule-based matching (currently: `node_modules`, pnpm `.pnpm-store`, Cargo `target` with adjacent `Cargo.toml`, Python virtualenvs `venv`/`.venv`, and `*.egg-info`).
```

with:

```markdown
- Rule-based matching (currently: `node_modules`, pnpm `.pnpm-store`, Cargo `target` with adjacent `Cargo.toml`, Python virtualenvs `venv`/`.venv`, `*.egg-info`, Python tool caches `__pycache__`/`.pytest_cache`/`.mypy_cache`/`.ruff_cache`/`.tox`, and JS build/cache dirs `.next`/`.nuxt`/`.turbo`/`.parcel-cache`).
```

- [ ] **Step 2: Sanity-check the build is still green**

Run: `cargo test`
Expected: PASS (docs-only change; nothing should regress).

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "$(cat <<'EOF'
docs(readme): list newly supported artifact directories

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review

**Spec coverage:**
- Extend `PythonBuildArtifactsRule` with the five cache names → Task 1. ✓
- New `JsBuildArtifactsRule` with the four JS names → Task 2 (Steps 1-4). ✓
- Register the new rule in `RuleEngine::new` → Task 2 (Step 5). ✓
- README Features update → Task 3. ✓
- Tests: rules unit tests for representative names + negative case → Tasks 1 & 2; discovery `__pycache__` subtree-skip test → Task 1. ✓
- Non-goals (guarded names, config, un-ignore) → none added. ✓

**Placeholder scan:** No TBD/TODO/"handle edge cases"; every code step shows full code and exact commands. ✓

**Type consistency:** `JsBuildArtifactsRule` name string `"JavaScript build/cache directory"` matches between the impl (Task 2 Step 3) and the assertion (Task 2 Step 1). `PYTHON_ARTIFACT_DIRS` / `JS_ARTIFACT_DIRS` referenced only where defined. `RuleEngine::new`, `Candidate`, `MatchAction::IGNORE_AND_SKIP`, `evaluate` all match existing signatures. ✓
