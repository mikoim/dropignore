# Rule Expansion: JVM Build Dirs, JS Frameworks, IaC/Env Caches — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Generalize the Cargo-specific guarded rule into `MarkedBuildDirRule` (dir name + sibling marker files) and expand rule coverage to Maven/Gradle build dirs, new JS framework caches, and IaC/dev-environment caches.

**Architecture:** `src/rules.rs` holds a trait-based rule system. `RustTargetRule` (matches `target` when a sibling `Cargo.toml` exists) becomes a parameterized `MarkedBuildDirRule` with associated constants, mirroring the existing `ArtifactDirsRule` pattern. Marker files double as `triggers()`, which the existing scoped-rescan machinery in `src/app.rs` consumes unchanged. Unconditional names are appended to `ArtifactDirsRule` lists. Spec: `docs/superpowers/specs/2026-07-02-rule-expansion-jvm-js-iac-design.md`.

**Tech Stack:** Rust (edition 2024), inotify, tempfile (dev). No new dependencies.

## Global Constraints

- No new dependencies in `Cargo.toml`.
- Every new rule uses `MatchAction::IGNORE_AND_SKIP` (mark, never descend).
- Marker checks run only after the cheap directory-name check (no needless filesystem calls).
- Run `cargo fmt` before every commit; `cargo test` must pass at every commit.
- Rule log names are `&'static str` and human-readable (they appear in `info!` logs).

---

### Task 1: Generalize `RustTargetRule` into `MarkedBuildDirRule`

**Files:**
- Modify: `src/rules.rs` (replace `RustTargetRule` at lines 116–148; update tests at lines 292–319, 357–366)
- Modify: `src/app.rs` (import line 4, registration line 28, test at line 568)
- Modify: `src/discovery.rs` (test import line 113, test at lines 165–168)

**Interfaces:**
- Consumes: existing `Rule` trait, `Candidate`, `MatchAction::IGNORE_AND_SKIP` (all in `src/rules.rs`, unchanged).
- Produces: `pub(crate) struct MarkedBuildDirRule { name, dir, markers }` with `pub(crate) const CARGO_TARGET: Self`. Later tasks add `MAVEN_TARGET` and `GRADLE_BUILD` constants to the same `impl` block. `RustTargetRule` is deleted.

- [ ] **Step 1: Rewrite the Cargo-target tests against the new type (failing)**

In `src/rules.rs` tests, replace `rust_target_rule_requires_cargo_toml_in_parent` (lines 291–319) with these two tests (delete the old test):

```rust
#[test]
fn cargo_target_rule_requires_cargo_toml_in_parent() -> Result<()> {
    let temp = TempDir::new().context("Failed to create temp dir")?;
    let project_root = temp.path();
    fs::write(project_root.join("Cargo.toml"), b"[package]\nname=\"demo\"")?;

    let target_dir = project_root.join("target");
    fs::create_dir(&target_dir)?;

    let metadata = fs::metadata(&target_dir)?;
    let candidate = Candidate {
        path: &target_dir,
        file_type: metadata.file_type(),
    };
    let engine = RuleEngine::new(vec![
        Box::new(MarkedBuildDirRule::CARGO_TARGET),
        Box::new(ArtifactDirsRule::NODE_MODULES),
    ]);

    let result = engine
        .evaluate(&candidate)
        .expect("rule should match Cargo target");

    assert_eq!(result.name, "Cargo target directory");
    assert!(result.action.set_dropbox_ignore);
    assert!(result.action.skip_descendants);
    Ok(())
}

#[test]
fn cargo_target_rule_ignores_target_without_cargo_toml() -> Result<()> {
    let temp = TempDir::new().context("Failed to create temp dir")?;
    let target_dir = temp.path().join("target");
    fs::create_dir(&target_dir)?;

    let metadata = fs::metadata(&target_dir)?;
    let candidate = Candidate {
        path: &target_dir,
        file_type: metadata.file_type(),
    };
    assert!(
        !MarkedBuildDirRule::CARGO_TARGET.matches(&candidate),
        "target without a sibling Cargo.toml must not match"
    );
    Ok(())
}
```

Replace `rust_target_rule_declares_cargo_toml_trigger` (lines 357–360) with:

```rust
#[test]
fn cargo_target_rule_declares_cargo_toml_trigger() {
    assert_eq!(MarkedBuildDirRule::CARGO_TARGET.triggers(), &["Cargo.toml"]);
}
```

In `rule_engine_recognizes_cargo_toml_trigger` (line 364), replace `Box::new(RustTargetRule)` with `Box::new(MarkedBuildDirRule::CARGO_TARGET)`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test`
Expected: compile error — `cannot find ... MarkedBuildDirRule` in `rules::tests`.

- [ ] **Step 3: Implement `MarkedBuildDirRule`, delete `RustTargetRule`, update references**

In `src/rules.rs`, replace the whole `RustTargetRule` block (struct doc comment through the end of its `impl Rule`, lines 116–148) with:

```rust
/// Rule matching a build output directory only when a marker file exists in
/// the same parent directory, so generic names like `target` or `build` are
/// ignored only inside real projects. The markers double as `triggers()`:
/// creating one schedules a scoped rescan that reconciles a pre-existing
/// build directory (see `Rule::triggers`).
pub(crate) struct MarkedBuildDirRule {
    name: &'static str,
    dir: &'static str,
    markers: &'static [&'static str],
}

impl MarkedBuildDirRule {
    pub(crate) const CARGO_TARGET: Self = Self {
        name: "Cargo target directory",
        dir: "target",
        markers: &["Cargo.toml"],
    };
}

impl Rule for MarkedBuildDirRule {
    fn name(&self) -> &'static str {
        self.name
    }

    fn matches(&self, candidate: &Candidate<'_>) -> bool {
        // Check the directory name first to avoid unnecessary filesystem operations.
        if !candidate.is_dir_named(self.dir) {
            return false;
        }

        // Ensure the parent contains a marker file so we only match real projects.
        let Some(parent) = candidate.path.parent() else {
            return false;
        };
        self.markers
            .iter()
            .any(|marker| parent.join(marker).exists())
    }

    fn action(&self) -> MatchAction {
        MatchAction::IGNORE_AND_SKIP
    }

    fn triggers(&self) -> &'static [&'static str] {
        self.markers
    }
}
```

In `src/app.rs`:
- Line 4: `use crate::rules::{ArtifactDirsRule, Candidate, EggInfoRule, MarkedBuildDirRule, RuleEngine};`
- Line 28 (inside `RuleEngine::new` vec): replace `Box::new(RustTargetRule),` with `Box::new(MarkedBuildDirRule::CARGO_TARGET),`
- Line 568 (test `rescan_subtree_reconciles_newly_matched_sibling`): replace `RuleEngine::new(vec![Box::new(RustTargetRule)])` with `RuleEngine::new(vec![Box::new(MarkedBuildDirRule::CARGO_TARGET)])`

In `src/discovery.rs`:
- Line 113 (test import): `use crate::rules::{ArtifactDirsRule, EggInfoRule, MarkedBuildDirRule, RuleEngine};`
- Line 166 (test `discover_watch_targets_handles_cargo_target`): replace `Box::new(RustTargetRule),` with `Box::new(MarkedBuildDirRule::CARGO_TARGET),`

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: all tests PASS (the ported Cargo tests prove behavior equivalence).

- [ ] **Step 5: Format and commit**

```bash
cargo fmt
git add src/rules.rs src/app.rs src/discovery.rs
git commit -m "refactor(rules): generalize RustTargetRule into MarkedBuildDirRule"
```

---

### Task 2: Add `MAVEN_TARGET`

**Files:**
- Modify: `src/rules.rs` (add constant to `impl MarkedBuildDirRule`; add tests)
- Modify: `src/app.rs` (register the constant)

**Interfaces:**
- Consumes: `MarkedBuildDirRule` from Task 1 (fields `name`, `dir`, `markers`; `Rule` impl already generic).
- Produces: `MarkedBuildDirRule::MAVEN_TARGET` (`dir: "target"`, `markers: &["pom.xml"]`, name `"Maven target directory"`), registered in `RuleEngine::new`.

- [ ] **Step 1: Write the failing tests**

Add to `src/rules.rs` tests, next to the Cargo-target tests:

```rust
#[test]
fn maven_target_rule_requires_pom_xml_in_parent() -> Result<()> {
    let temp = TempDir::new().context("Failed to create temp dir")?;
    fs::write(temp.path().join("pom.xml"), b"<project/>")?;
    let target_dir = temp.path().join("target");
    fs::create_dir(&target_dir)?;

    let metadata = fs::metadata(&target_dir)?;
    let candidate = Candidate {
        path: &target_dir,
        file_type: metadata.file_type(),
    };
    assert_eq!(
        MarkedBuildDirRule::MAVEN_TARGET.name(),
        "Maven target directory"
    );
    assert!(
        MarkedBuildDirRule::MAVEN_TARGET.matches(&candidate),
        "target with a sibling pom.xml must match"
    );
    assert_eq!(
        MarkedBuildDirRule::MAVEN_TARGET.action(),
        MatchAction::IGNORE_AND_SKIP
    );
    Ok(())
}

#[test]
fn maven_target_rule_ignores_target_without_pom_xml() -> Result<()> {
    let temp = TempDir::new().context("Failed to create temp dir")?;
    let target_dir = temp.path().join("target");
    fs::create_dir(&target_dir)?;

    let metadata = fs::metadata(&target_dir)?;
    let candidate = Candidate {
        path: &target_dir,
        file_type: metadata.file_type(),
    };
    assert!(
        !MarkedBuildDirRule::MAVEN_TARGET.matches(&candidate),
        "target without a sibling pom.xml must not match"
    );
    Ok(())
}

#[test]
fn rule_engine_recognizes_pom_xml_trigger() {
    let engine = RuleEngine::new(vec![Box::new(MarkedBuildDirRule::MAVEN_TARGET)]);
    assert!(engine.is_trigger(OsStr::new("pom.xml")));
    assert!(!engine.is_trigger(OsStr::new("Cargo.toml")));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test maven`
Expected: compile error — no associated item `MAVEN_TARGET` found.

- [ ] **Step 3: Implement the constant and register it**

In `src/rules.rs`, inside `impl MarkedBuildDirRule`, after `CARGO_TARGET`:

```rust
    pub(crate) const MAVEN_TARGET: Self = Self {
        name: "Maven target directory",
        dir: "target",
        markers: &["pom.xml"],
    };
```

In `src/app.rs` `RuleEngine::new` vec, after `CARGO_TARGET`:

```rust
        Box::new(MarkedBuildDirRule::MAVEN_TARGET),
```

(`CARGO_TARGET` and `MAVEN_TARGET` both match `target` when both markers exist; actions are identical, so first-match ordering is irrelevant.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: all tests PASS.

- [ ] **Step 5: Format and commit**

```bash
cargo fmt
git add src/rules.rs src/app.rs
git commit -m "feat(rules): ignore Maven target directories guarded by pom.xml"
```

---

### Task 3: Add `GRADLE_BUILD` with rescan integration test

**Files:**
- Modify: `src/rules.rs` (add constant; add tests)
- Modify: `src/app.rs` (register the constant; add integration test)

**Interfaces:**
- Consumes: `MarkedBuildDirRule` from Task 1; test helpers already in `src/app.rs` tests (`discover_watch_targets`, `apply_discovered_paths`, `rescan_subtree`, `WatchRegistry`).
- Produces: `MarkedBuildDirRule::GRADLE_BUILD` (`dir: "build"`, `markers: &["build.gradle", "build.gradle.kts", "settings.gradle", "settings.gradle.kts"]`, name `"Gradle build directory"`), registered in `RuleEngine::new`.

- [ ] **Step 1: Write the failing rule tests**

Add to `src/rules.rs` tests:

```rust
#[test]
fn gradle_build_rule_matches_with_each_marker() -> Result<()> {
    for marker in [
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
        "settings.gradle.kts",
    ] {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        fs::write(temp.path().join(marker), b"")?;
        let build_dir = temp.path().join("build");
        fs::create_dir(&build_dir)?;

        let metadata = fs::metadata(&build_dir)?;
        let candidate = Candidate {
            path: &build_dir,
            file_type: metadata.file_type(),
        };
        assert!(
            MarkedBuildDirRule::GRADLE_BUILD.matches(&candidate),
            "build with sibling {marker} must match"
        );
    }
    assert_eq!(
        MarkedBuildDirRule::GRADLE_BUILD.name(),
        "Gradle build directory"
    );
    assert_eq!(
        MarkedBuildDirRule::GRADLE_BUILD.action(),
        MatchAction::IGNORE_AND_SKIP
    );
    Ok(())
}

#[test]
fn gradle_build_rule_ignores_build_without_marker() -> Result<()> {
    let temp = TempDir::new().context("Failed to create temp dir")?;
    let build_dir = temp.path().join("build");
    fs::create_dir(&build_dir)?;

    let metadata = fs::metadata(&build_dir)?;
    let candidate = Candidate {
        path: &build_dir,
        file_type: metadata.file_type(),
    };
    assert!(
        !MarkedBuildDirRule::GRADLE_BUILD.matches(&candidate),
        "build without a Gradle script sibling must not match"
    );
    Ok(())
}

#[test]
fn rule_engine_recognizes_gradle_triggers() {
    let engine = RuleEngine::new(vec![Box::new(MarkedBuildDirRule::GRADLE_BUILD)]);
    for trigger in [
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
        "settings.gradle.kts",
    ] {
        assert!(engine.is_trigger(OsStr::new(trigger)), "{trigger}");
    }
    assert!(!engine.is_trigger(OsStr::new("pom.xml")));
}
```

- [ ] **Step 2: Write the failing integration test**

Add to `src/app.rs` tests, after `rescan_subtree_reconciles_newly_matched_sibling`:

```rust
#[test]
fn rescan_subtree_reconciles_gradle_build_after_marker() -> Result<()> {
    let temp = TempDir::new()?;
    let proj = temp.path().join("proj");
    let build = proj.join("build");
    let build_classes = build.join("classes");
    fs::create_dir_all(&build_classes)?;

    let rules = RuleEngine::new(vec![Box::new(MarkedBuildDirRule::GRADLE_BUILD)]);
    let mut watcher = Inotify::init()?;
    let mut registry = WatchRegistry::default();

    // No Gradle script yet: build does not match, so it is watched.
    let discovered = discover_watch_targets(&proj, &rules)?;
    apply_discovered_paths(discovered, true, &mut watcher, &mut registry)?;
    assert!(registry.contains_path(&build), "build watched pre-marker");

    // Marker appears; a scoped rescan must now skip build's subtree.
    fs::write(proj.join("build.gradle.kts"), b"")?;
    rescan_subtree(&proj, true, &mut watcher, &mut registry, &rules)?;

    assert!(
        !registry.contains_path(&build),
        "matched build must not be watched"
    );
    assert!(
        !registry.contains_path(&build_classes),
        "build subtree must be pruned"
    );
    assert!(registry.contains_path(&proj), "project dir stays watched");
    Ok(())
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test gradle`
Expected: compile error — no associated item `GRADLE_BUILD` found.

- [ ] **Step 4: Implement the constant and register it**

In `src/rules.rs`, inside `impl MarkedBuildDirRule`, after `MAVEN_TARGET`:

```rust
    pub(crate) const GRADLE_BUILD: Self = Self {
        name: "Gradle build directory",
        dir: "build",
        markers: &[
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ],
    };
```

In `src/app.rs` `RuleEngine::new` vec, after `MAVEN_TARGET`:

```rust
        Box::new(MarkedBuildDirRule::GRADLE_BUILD),
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test`
Expected: all tests PASS, including `rescan_subtree_reconciles_gradle_build_after_marker`.

- [ ] **Step 6: Format and commit**

```bash
cargo fmt
git add src/rules.rs src/app.rs
git commit -m "feat(rules): ignore Gradle build directories guarded by Gradle scripts"
```

---

### Task 4: Extend `ArtifactDirsRule` lists (JS, Gradle cache, IaC, dev-env)

**Files:**
- Modify: `src/rules.rs` (extend `JS_ARTIFACT_DIRS` const at line 166; add three `ArtifactDirsRule` constants; extend tests)
- Modify: `src/app.rs` (register three constants)

**Interfaces:**
- Consumes: existing `ArtifactDirsRule { name, dirs }` and its `Rule` impl (unchanged).
- Produces: `ArtifactDirsRule::JVM_CACHES`, `ArtifactDirsRule::IAC_CACHES`, `ArtifactDirsRule::DEV_ENV_DIRS`; `JS_ARTIFACT_DIRS` grows to eight names.

- [ ] **Step 1: Extend the tests (failing)**

In `src/rules.rs`, update the loop in `js_build_artifacts_rule_matches_framework_dirs` to:

```rust
        for name in [
            ".next",
            ".nuxt",
            ".turbo",
            ".parcel-cache",
            ".svelte-kit",
            ".astro",
            ".angular",
            ".vite",
        ] {
```

In `artifact_dirs_rule_instances_match_their_directories`, append to the `cases` slice:

```rust
            (
                &ArtifactDirsRule::JVM_CACHES,
                ".gradle",
                "Gradle cache directory",
            ),
            (
                &ArtifactDirsRule::IAC_CACHES,
                ".terraform",
                "IaC cache directory",
            ),
            (
                &ArtifactDirsRule::DEV_ENV_DIRS,
                ".direnv",
                "development environment directory",
            ),
```

Add a coverage test for the names not exercised by the table test:

```rust
#[test]
fn iac_and_env_rules_match_all_listed_dirs() -> Result<()> {
    let temp = TempDir::new().context("Failed to create temp dir")?;
    let cases: &[(&ArtifactDirsRule, &str)] = &[
        (&ArtifactDirsRule::IAC_CACHES, ".terragrunt-cache"),
        (&ArtifactDirsRule::DEV_ENV_DIRS, ".devenv"),
    ];
    for (rule, name) in cases {
        let dir = temp.path().join(name);
        fs::create_dir(&dir)?;
        let meta = fs::metadata(&dir)?;
        let candidate = Candidate {
            path: &dir,
            file_type: meta.file_type(),
        };
        assert!(rule.matches(&candidate), "{name} should match");
    }
    Ok(())
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test artifact`
Expected: compile error — no associated items `JVM_CACHES` / `IAC_CACHES` / `DEV_ENV_DIRS`.

- [ ] **Step 3: Implement the list changes and register**

In `src/rules.rs`, replace the `JS_ARTIFACT_DIRS` const (line 166) with:

```rust
/// JavaScript framework build output and tool cache directories matched by
/// exact name. Each is reproducible and never holds user source. `.turbo` is
/// Turborepo's local cache (verified against its docs).
const JS_ARTIFACT_DIRS: &[&str] = &[
    ".next",
    ".nuxt",
    ".turbo",
    ".parcel-cache",
    ".svelte-kit",
    ".astro",
    ".angular",
    ".vite",
];
```

In `impl ArtifactDirsRule`, after `JS_BUILD`:

```rust
    /// Project-local Gradle cache. The guarded `build` output lives in
    /// `MarkedBuildDirRule::GRADLE_BUILD`; `.gradle` is unconditional because
    /// a directory with this exact name is Gradle-owned in practice and
    /// marking is non-destructive (sync exclusion only).
    pub(crate) const JVM_CACHES: Self = Self {
        name: "Gradle cache directory",
        dirs: &[".gradle"],
    };
    /// IaC tool caches: `.terraform` holds provider/module downloads
    /// recreated by `terraform init`; `.terragrunt-cache` is Terragrunt's
    /// working copy.
    pub(crate) const IAC_CACHES: Self = Self {
        name: "IaC cache directory",
        dirs: &[".terraform", ".terragrunt-cache"],
    };
    /// Dev-environment state dirs owned by direnv/devenv.
    pub(crate) const DEV_ENV_DIRS: Self = Self {
        name: "development environment directory",
        dirs: &[".direnv", ".devenv"],
    };
```

In `src/app.rs` `RuleEngine::new` vec, after `Box::new(ArtifactDirsRule::JS_BUILD),`:

```rust
        Box::new(ArtifactDirsRule::JVM_CACHES),
        Box::new(ArtifactDirsRule::IAC_CACHES),
        Box::new(ArtifactDirsRule::DEV_ENV_DIRS),
```

The final registration list (11 rules):

```rust
    let rule_engine = RuleEngine::new(vec![
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
    ]);
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: all tests PASS.

- [ ] **Step 5: Format and commit**

```bash
cargo fmt
git add src/rules.rs src/app.rs
git commit -m "feat(rules): add JS framework, Gradle cache, IaC, and dev-env directories"
```

---

### Task 5: Update README

**Files:**
- Modify: `README.md` (Features bullet at line 7; "Extending rules" section at lines 35–40)

**Interfaces:**
- Consumes: the final rule set from Tasks 1–4.
- Produces: documentation only.

- [ ] **Step 1: Update the Features rule enumeration**

Replace the line-7 bullet with:

```markdown
- Rule-based matching (currently: `node_modules`, pnpm `.pnpm-store`, Cargo/Maven `target` with an adjacent `Cargo.toml`/`pom.xml`, Gradle `build` with an adjacent Gradle build/settings script, Gradle cache `.gradle`, Python virtualenvs `venv`/`.venv`, `*.egg-info`, Python tool caches `__pycache__`/`.pytest_cache`/`.mypy_cache`/`.ruff_cache`/`.tox`, JS build/cache dirs `.next`/`.nuxt`/`.turbo`/`.parcel-cache`/`.svelte-kit`/`.astro`/`.angular`/`.vite`, IaC caches `.terraform`/`.terragrunt-cache`, and dev-environment dirs `.direnv`/`.devenv`).
```

- [ ] **Step 2: Update "Extending rules"**

Replace the section body (lines 36–40) with:

```markdown
For a new "ignore directories with these exact names" rule, add the name to an
existing `ArtifactDirsRule` list in `src/rules.rs` (or add a new associated
constant). For a build directory that should only match next to a project
marker file (like Cargo's `target` next to `Cargo.toml`), add a
`MarkedBuildDirRule` constant; its markers automatically become rescan
triggers. Register new constants in `RuleEngine::new` in `src/app.rs`. For
anything else, implement the `Rule` trait directly.
```

- [ ] **Step 3: Verify tests still pass**

Run: `cargo test`
Expected: all tests PASS (no code change; sanity check before commit).

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs: update rule list and extension guide for new rules"
```
