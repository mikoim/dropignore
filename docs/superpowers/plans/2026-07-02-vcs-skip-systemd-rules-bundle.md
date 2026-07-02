# VCS Skip, systemd Unit, Rule Expansion & Housekeeping Bundle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop watching VCS internals, add verified rules for Composer/Elixir/Zig/Dart, round out Cargo metadata and the release profile, and ship a systemd user unit.

**Architecture:** All rule work extends the existing `Rule` machinery in `src/rules.rs` (`ArtifactDirsRule` gains a per-instance `MatchAction`; new `MarkedBuildDirRule` constants reuse the marker/trigger system). No changes to the walk (`src/discovery.rs`) or event loop (`src/app.rs::drain_events`) logic are needed — both already honor `skip_descendants` without `set_dropbox_ignore`. The systemd unit and metadata items touch no Rust code.

**Tech Stack:** Rust (edition 2024), inotify, clap, tempfile (tests). Spec: `docs/superpowers/specs/2026-07-02-vcs-skip-systemd-rules-bundle-design.md`.

## Global Constraints

- No new dependencies in `Cargo.toml` (`[dependencies]` and `[dev-dependencies]` stay exactly as they are).
- Every commit must pass `./scripts/check.sh` (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`); the pre-commit hook runs it automatically.
- End every commit message with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- Never push to `origin` (stale remote; this repo uses local-merge workflow).
- Legacy Zig `zig-cache` (undotted) is explicitly out of scope.

---

### Task 1: VCS directory watch skip

**Files:**
- Modify: `src/rules.rs` (MatchAction consts ~line 35, `log_matched` ~line 52, `ArtifactDirsRule` ~lines 212–276, tests at end)
- Modify: `src/app.rs` (`RuleEngine::new` list at lines 25–37; tests ~line 452+)
- Modify: `src/discovery.rs` (tests only, ~line 126+)
- Modify: `README.md` (Features list)

**Interfaces:**
- Consumes: existing `MatchAction`, `ArtifactDirsRule`, `RuleEngine` in `src/rules.rs`.
- Produces: `MatchAction::SKIP_ONLY` (const, `{ set_dropbox_ignore: false, skip_descendants: true }`) and `ArtifactDirsRule::VCS_DIRS` (matches dirs named `.git`, `.hg`, `.svn`, `.jj`, `.bzr`; action `SKIP_ONLY`). `ArtifactDirsRule` gains a private `action: MatchAction` field — Task 2's new constants must set it.

- [ ] **Step 1: Write the failing rule tests**

Append to the `tests` module in `src/rules.rs`:

```rust
    #[test]
    fn vcs_dirs_rule_skips_all_vcs_dirs_without_marking() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        assert_eq!(ArtifactDirsRule::VCS_DIRS.name(), "version control directory");
        for name in [".git", ".hg", ".svn", ".jj", ".bzr"] {
            let dir = temp.path().join(name);
            fs::create_dir(&dir)?;
            let meta = fs::metadata(&dir)?;
            let candidate = Candidate {
                path: &dir,
                file_type: meta.file_type(),
            };
            assert!(
                ArtifactDirsRule::VCS_DIRS.matches(&candidate),
                "{name} should match"
            );
        }
        assert_eq!(ArtifactDirsRule::VCS_DIRS.action(), MatchAction::SKIP_ONLY);
        assert!(
            !MatchAction::SKIP_ONLY.set_dropbox_ignore,
            "skip-only must never mark"
        );
        assert!(
            MatchAction::SKIP_ONLY.skip_descendants,
            "skip-only must skip descendants"
        );
        Ok(())
    }

    #[test]
    fn vcs_dirs_rule_ignores_git_file() -> Result<()> {
        // Submodules and linked worktrees use a .git *file*; it needs no
        // skipping (files have no descendants) and must not match.
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let git_file = temp.path().join(".git");
        fs::write(&git_file, b"gitdir: ../.git/modules/x")?;
        let meta = fs::metadata(&git_file)?;
        let candidate = Candidate {
            path: &git_file,
            file_type: meta.file_type(),
        };
        assert!(
            !ArtifactDirsRule::VCS_DIRS.matches(&candidate),
            "a .git file must not match"
        );
        Ok(())
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test vcs_dirs`
Expected: compile error — `no associated item named 'VCS_DIRS' found` / `no associated item named 'SKIP_ONLY'`.

- [ ] **Step 3: Implement in `src/rules.rs`**

3a. Change the log import (line 1):

```rust
use log::{debug, info};
```

3b. Add `SKIP_ONLY` to the `impl MatchAction` block (after `IGNORE_AND_SKIP`):

```rust
    /// Skip descending (and watching) without touching the Dropbox attribute.
    pub(crate) const SKIP_ONLY: Self = Self {
        set_dropbox_ignore: false,
        skip_descendants: true,
    };
```

3c. Replace `log_matched` so skip-only matches stay out of the info log:

```rust
    /// Log that this match fired for `path`. Kept out of `RuleEngine::evaluate`
    /// so that evaluation stays a pure query and the caller controls logging.
    /// Skip-only matches log at debug: they do not act on Dropbox state, and
    /// at info they would add one line per repository directory.
    pub(crate) fn log_matched(&self, path: &Path) {
        if self.action.set_dropbox_ignore {
            info!("Matched rule '{}' for {}", self.name, path.display());
        } else {
            debug!("Matched rule '{}' for {}", self.name, path.display());
        }
    }
```

3d. Add the `action` field to `ArtifactDirsRule` and return it from `action()`:

```rust
pub(crate) struct ArtifactDirsRule {
    name: &'static str,
    dirs: &'static [&'static str],
    action: MatchAction,
}
```

```rust
    fn action(&self) -> MatchAction {
        self.action
    }
```

3e. Add `action: MatchAction::IGNORE_AND_SKIP,` to all seven existing constants (`NODE_MODULES`, `PNPM_STORE`, `PYTHON_CACHES`, `JS_BUILD`, `JVM_CACHES`, `IAC_CACHES`, `DEV_ENV_DIRS`), e.g.:

```rust
    pub(crate) const NODE_MODULES: Self = Self {
        name: "node_modules directory",
        dirs: &["node_modules"],
        action: MatchAction::IGNORE_AND_SKIP,
    };
```

3f. Add the new constant after `DEV_ENV_DIRS`:

```rust
    /// Version control internals. Never marked (syncing a repository stays
    /// the user's choice) but never descended into either: nothing inside
    /// ever matches a rule, and watching e.g. `.git/objects/*` wastes
    /// thousands of inotify watches and floods the event queue during git
    /// operations.
    pub(crate) const VCS_DIRS: Self = Self {
        name: "version control directory",
        dirs: &[".git", ".hg", ".svn", ".jj", ".bzr"],
        action: MatchAction::SKIP_ONLY,
    };
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test vcs_dirs`
Expected: 2 passed.

- [ ] **Step 5: Write the failing discovery test**

Append to the `tests` module in `src/discovery.rs`:

```rust
    #[test]
    fn discover_watch_targets_skips_vcs_dirs_without_marking() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let git_dir = temp.path().join(".git");
        let git_objects = git_dir.join("objects");
        let nm_in_git = git_dir.join("node_modules");
        let keep_dir = temp.path().join("keep");
        fs::create_dir_all(&git_objects)?;
        fs::create_dir(&nm_in_git)?;
        fs::create_dir(&keep_dir)?;

        let engine = RuleEngine::new(vec![
            Box::new(ArtifactDirsRule::VCS_DIRS),
            Box::new(ArtifactDirsRule::NODE_MODULES),
        ]);
        let discovered = discover_watch_targets(temp.path(), &engine)?;

        assert!(
            discovered.watchers.contains(&keep_dir),
            "plain sibling should be watched"
        );
        assert!(
            !discovered.watchers.contains(&git_dir),
            ".git must not be watched"
        );
        assert!(
            !discovered.watchers.contains(&git_objects),
            ".git contents must not be watched"
        );
        assert!(
            !discovered.matches.contains(&git_dir),
            ".git must not be marked"
        );
        assert!(
            !discovered.matches.contains(&nm_in_git),
            "matches inside a skipped VCS dir must not be marked"
        );
        Ok(())
    }
```

- [ ] **Step 6: Run it — expect pass (no walk changes needed)**

Run: `cargo test discover_watch_targets_skips_vcs`
Expected: PASS — `walk()` already `continue`s on `skip_descendants` before pushing a watcher and only records matches when `set_dropbox_ignore` is true. If this FAILS, the walk logic diverges from the spec; stop and re-read `src/discovery.rs::walk`.

- [ ] **Step 7: Write the failing event-path test**

Append to the `tests` module in `src/app.rs` (helpers `apply_discovered_paths`, `drain_events`, `discover_watch_targets`, `WatchRegistry` are already in scope there):

```rust
    #[test]
    fn drain_events_skips_new_vcs_dir() -> Result<()> {
        use std::thread::sleep;
        use std::time::{Duration, Instant};

        let temp = TempDir::new()?;
        let root = temp.path().to_path_buf();
        let rules = RuleEngine::new(vec![
            Box::new(ArtifactDirsRule::VCS_DIRS),
            Box::new(ArtifactDirsRule::NODE_MODULES),
        ]);
        let mut watcher = Inotify::init()?;
        let mut registry = WatchRegistry::default();

        let initial = discover_watch_targets(&root, &rules)?;
        apply_discovered_paths(initial, true, &mut watcher, &mut registry)?;
        assert!(
            registry.contains_path(&root),
            "root must be watched after seeding"
        );

        // .git is created first so its CREATE event drains no later than
        // plain's, making the "not watched" assertion meaningful.
        let git_dir = root.join(".git");
        let plain = root.join("plain");
        fs::create_dir(&git_dir)?;
        fs::create_dir(&plain)?;

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !registry.contains_path(&plain) {
            drain_events(&mut watcher, &mut registry, &rules, &root, true)?;
            sleep(Duration::from_millis(20));
        }

        assert!(
            registry.contains_path(&plain),
            "a new plain dir must be watched via the event path"
        );
        assert!(
            !registry.contains_path(&git_dir),
            "a new .git dir must be skipped, not watched"
        );
        Ok(())
    }
```

- [ ] **Step 8: Run it — expect pass (plan_entry already honors skip-only)**

Run: `cargo test drain_events_skips_new_vcs_dir`
Expected: PASS — `plan_entry` sets `watch_dir = is_dir && !skip_descendants` and `apply_ignore = set_dropbox_ignore`. If it FAILS, stop and re-read `src/app.rs::plan_entry`.

- [ ] **Step 9: Register the rule in `src/app.rs`**

In `run()`, add `VCS_DIRS` as the first entry of the `RuleEngine::new` vec:

```rust
    let rule_engine = RuleEngine::new(vec![
        Box::new(ArtifactDirsRule::VCS_DIRS),
        Box::new(ArtifactDirsRule::NODE_MODULES),
        // ... existing entries unchanged ...
    ]);
```

- [ ] **Step 10: Update the README Features list**

In `README.md`, after the bullet `- Skips descending into ignored subtrees to avoid unnecessary watches.`, add:

```markdown
- Never watches version control internals (`.git`, `.hg`, `.svn`, `.jj`, `.bzr`): skipped without being marked, so repositories still sync while inotify watches are spared.
```

- [ ] **Step 11: Full gate and commit**

Run: `./scripts/check.sh`
Expected: fmt clean, clippy zero warnings, all tests pass (68 total: 65 existing + 3 new).

```bash
git add src/rules.rs src/app.rs src/discovery.rs README.md
git commit -m "feat(rules): skip watching VCS internals without marking

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: Rule expansion — Composer, Elixir, Zig, Dart

**Files:**
- Modify: `src/rules.rs` (`MarkedBuildDirRule` consts ~line 127; `ArtifactDirsRule` consts ~line 217; tests at end)
- Modify: `src/app.rs` (`RuleEngine::new` list)
- Modify: `README.md` (Features rule list)

**Interfaces:**
- Consumes: `ArtifactDirsRule` with the `action: MatchAction` field from Task 1 (new constants must set `action: MatchAction::IGNORE_AND_SKIP`).
- Produces: `MarkedBuildDirRule::{COMPOSER_VENDOR, MIX_BUILD, MIX_DEPS, ZIG_OUT}` and `ArtifactDirsRule::{ZIG_CACHES, DART_CACHES}`.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `src/rules.rs`:

```rust
    #[test]
    fn marked_build_dir_expansion_rules_require_their_markers() -> Result<()> {
        let cases: &[(&MarkedBuildDirRule, &str, &str, &str)] = &[
            (
                &MarkedBuildDirRule::COMPOSER_VENDOR,
                "vendor",
                "composer.json",
                "Composer vendor directory",
            ),
            (
                &MarkedBuildDirRule::MIX_BUILD,
                "_build",
                "mix.exs",
                "Mix build directory",
            ),
            (
                &MarkedBuildDirRule::MIX_DEPS,
                "deps",
                "mix.exs",
                "Mix deps directory",
            ),
            (
                &MarkedBuildDirRule::ZIG_OUT,
                "zig-out",
                "build.zig",
                "Zig output directory",
            ),
        ];
        for (rule, dir_name, marker, rule_name) in cases {
            assert_eq!(rule.name(), *rule_name);
            assert_eq!(rule.triggers(), &[*marker]);
            assert_eq!(rule.action(), MatchAction::IGNORE_AND_SKIP);

            // Without the marker the directory must not match (e.g. Go's
            // committed vendor/ has no composer.json).
            let bare = TempDir::new().context("Failed to create temp dir")?;
            let dir = bare.path().join(dir_name);
            fs::create_dir(&dir)?;
            let meta = fs::metadata(&dir)?;
            let candidate = Candidate {
                path: &dir,
                file_type: meta.file_type(),
            };
            assert!(
                !rule.matches(&candidate),
                "{dir_name} without {marker} must not match"
            );

            // With the marker it must match.
            let project = TempDir::new().context("Failed to create temp dir")?;
            fs::write(project.path().join(marker), b"")?;
            let dir = project.path().join(dir_name);
            fs::create_dir(&dir)?;
            let meta = fs::metadata(&dir)?;
            let candidate = Candidate {
                path: &dir,
                file_type: meta.file_type(),
            };
            assert!(
                rule.matches(&candidate),
                "{dir_name} with sibling {marker} must match"
            );
        }
        Ok(())
    }

    #[test]
    fn zig_and_dart_cache_rules_match_their_directories() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let cases: &[(&ArtifactDirsRule, &str, &str)] = &[
            (
                &ArtifactDirsRule::ZIG_CACHES,
                ".zig-cache",
                "Zig cache directory",
            ),
            (
                &ArtifactDirsRule::DART_CACHES,
                ".dart_tool",
                "Dart tool directory",
            ),
        ];
        for (rule, dir_name, rule_name) in cases {
            assert_eq!(rule.name(), *rule_name);
            let dir = temp.path().join(dir_name);
            fs::create_dir(&dir)?;
            let meta = fs::metadata(&dir)?;
            let candidate = Candidate {
                path: &dir,
                file_type: meta.file_type(),
            };
            assert!(rule.matches(&candidate), "{dir_name} should match");
            assert_eq!(rule.action(), MatchAction::IGNORE_AND_SKIP);
        }
        Ok(())
    }

    #[test]
    fn rule_engine_recognizes_expansion_triggers() {
        let engine = RuleEngine::new(vec![
            Box::new(MarkedBuildDirRule::COMPOSER_VENDOR),
            Box::new(MarkedBuildDirRule::MIX_BUILD),
            Box::new(MarkedBuildDirRule::MIX_DEPS),
            Box::new(MarkedBuildDirRule::ZIG_OUT),
        ]);
        for trigger in ["composer.json", "mix.exs", "build.zig"] {
            assert!(engine.is_trigger(OsStr::new(trigger)), "{trigger}");
        }
        assert!(!engine.is_trigger(OsStr::new("package.json")));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test expansion`
Expected: compile error — `no associated item named 'COMPOSER_VENDOR'` (and the other new constants).

- [ ] **Step 3: Implement the constants in `src/rules.rs`**

3a. Inside `impl MarkedBuildDirRule`, after `GRADLE_BUILD`:

```rust
    /// Composer installs dependencies into `vendor`; official docs recommend
    /// against committing it and `composer install` regenerates it.
    pub(crate) const COMPOSER_VENDOR: Self = Self {
        name: "Composer vendor directory",
        dir: "vendor",
        markers: &["composer.json"],
    };

    /// Mix compile output; regenerated by `mix compile`.
    pub(crate) const MIX_BUILD: Self = Self {
        name: "Mix build directory",
        dir: "_build",
        markers: &["mix.exs"],
    };

    /// Mix dependency checkouts; regenerated by `mix deps.get`.
    pub(crate) const MIX_DEPS: Self = Self {
        name: "Mix deps directory",
        dir: "deps",
        markers: &["mix.exs"],
    };

    /// Zig install output of `zig build`; `build.zig` is required to build.
    pub(crate) const ZIG_OUT: Self = Self {
        name: "Zig output directory",
        dir: "zig-out",
        markers: &["build.zig"],
    };
```

3b. Inside `impl ArtifactDirsRule`, after `DEV_ENV_DIRS` (before `VCS_DIRS`):

```rust
    /// Zig compiler cache (renamed from undotted `zig-cache` in Zig 0.13,
    /// which is out of scope); regenerated by any `zig build`/`zig test`.
    pub(crate) const ZIG_CACHES: Self = Self {
        name: "Zig cache directory",
        dirs: &[".zig-cache"],
        action: MatchAction::IGNORE_AND_SKIP,
    };
    /// Dart/Flutter tool state created by `dart pub get`; official docs say
    /// to never check it into source control and that deleting it is safe.
    pub(crate) const DART_CACHES: Self = Self {
        name: "Dart tool directory",
        dirs: &[".dart_tool"],
        action: MatchAction::IGNORE_AND_SKIP,
    };
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test expansion && cargo test zig_and_dart`
Expected: 3 new tests pass.

- [ ] **Step 5: Register the rules in `src/app.rs`**

Append to the `RuleEngine::new` vec in `run()` (after `DEV_ENV_DIRS`):

```rust
        Box::new(MarkedBuildDirRule::COMPOSER_VENDOR),
        Box::new(MarkedBuildDirRule::MIX_BUILD),
        Box::new(MarkedBuildDirRule::MIX_DEPS),
        Box::new(MarkedBuildDirRule::ZIG_OUT),
        Box::new(ArtifactDirsRule::ZIG_CACHES),
        Box::new(ArtifactDirsRule::DART_CACHES),
```

- [ ] **Step 6: Update the README rule list**

In `README.md`, replace the Features rule-list bullet (starts with `- Rule-based matching (currently:`) so the parenthetical ends with:

```markdown
- Rule-based matching (currently: `node_modules`, pnpm `.pnpm-store`, Cargo/Maven `target` with an adjacent `Cargo.toml`/`pom.xml`, Gradle `build` with an adjacent Gradle build/settings script, Gradle cache `.gradle`, Python virtualenvs `venv`/`.venv`, `*.egg-info`, Python tool caches `__pycache__`/`.pytest_cache`/`.mypy_cache`/`.ruff_cache`/`.tox`, JS build/cache dirs `.next`/`.nuxt`/`.turbo`/`.parcel-cache`/`.svelte-kit`/`.astro`/`.angular`/`.vite`, IaC caches `.terraform`/`.terragrunt-cache`, dev-environment dirs `.direnv`/`.devenv`, Composer `vendor` with an adjacent `composer.json`, Elixir `_build`/`deps` with an adjacent `mix.exs`, Zig `zig-out` with an adjacent `build.zig`, Zig cache `.zig-cache`, and Dart tool state `.dart_tool`).
```

- [ ] **Step 7: Full gate and commit**

Run: `./scripts/check.sh`
Expected: fmt clean, clippy zero warnings, all tests pass (71 total).

```bash
git add src/rules.rs src/app.rs README.md
git commit -m "feat(rules): add Composer, Elixir, Zig, and Dart artifact rules

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: Cargo metadata & release profile

**Files:**
- Modify: `Cargo.toml`

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces: nothing consumed by other tasks (standalone).

- [ ] **Step 1: Edit `Cargo.toml`**

Add two keys to `[package]` and a new `[profile.release]` section (dependencies unchanged):

```toml
[package]
name = "dropignore"
version = "0.1.0"
edition = "2024"
description = "Watch a directory tree and mark build artifacts with Dropbox's ignore attribute"
readme = "README.md"
license = "MIT"
repository = "https://github.com/mikoim/dropignore"

[profile.release]
strip = true
lto = "thin"
```

- [ ] **Step 2: Verify the release build**

Run: `cargo build --release`
Expected: `Finished 'release' profile` with no warnings; `target/release/dropignore` exists. (This profile is not exercised by `check.sh`, so build it explicitly once.)

- [ ] **Step 3: Full gate and commit**

Run: `./scripts/check.sh`
Expected: all green (no code changes).

```bash
git add Cargo.toml
git commit -m "chore: add package metadata and release profile

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: systemd user unit + README service section

**Files:**
- Create: `contrib/dropignore.service`
- Modify: `README.md` (new section between "Usage" and "How it works")

**Interfaces:**
- Consumes: nothing from other tasks (runs last only so the README feature list is final before its last edit).
- Produces: nothing consumed by other tasks.

- [ ] **Step 1: Create `contrib/dropignore.service`**

Exact approved content:

```ini
[Unit]
Description=Mark build artifacts as Dropbox-ignored
After=default.target

[Service]
ExecStart=%h/.local/bin/dropignore %h/Dropbox
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

- [ ] **Step 2: Verify unit syntax**

Run: `systemd-analyze --user verify contrib/dropignore.service 2>&1 | grep -v "Command dropignore" || true`
Expected: no output other than (possibly) a warning that the ExecStart binary does not exist at `%h/.local/bin/dropignore` — that path is the documented install location, not a repo artifact. If `systemd-analyze` is unavailable, skip this step; the unit is declarative and reviewed by inspection.

- [ ] **Step 3: Add the README section**

Insert after the "### Logging" subsection (before `## How it works`):

````markdown
## Running as a service
Watch mode exits non-zero when the watched root is moved or deleted, so it
is designed to run under a supervisor. A systemd user unit is provided:

```bash
cp contrib/dropignore.service ~/.config/systemd/user/
# Edit ExecStart if your binary or Dropbox directory lives elsewhere.
systemctl --user daemon-reload
systemctl --user enable --now dropignore
```

Follow logs with `journalctl --user -u dropignore -f`. For very large
trees, raise the inotify watch limit
(`/proc/sys/fs/inotify/max_user_watches`).
````

- [ ] **Step 4: Full gate and commit**

Run: `./scripts/check.sh`
Expected: all green (no code changes).

```bash
git add contrib/dropignore.service README.md
git commit -m "docs: ship systemd user unit and service instructions

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```
