use log::{debug, info};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

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

/// Action taken when a rule matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MatchAction {
    pub(crate) set_dropbox_ignore: bool,
    pub(crate) skip_descendants: bool,
}

impl MatchAction {
    pub(crate) const IGNORE_AND_SKIP: Self = Self {
        set_dropbox_ignore: true,
        skip_descendants: true,
    };

    /// Skip descending (and watching) without touching the Dropbox attribute.
    pub(crate) const SKIP_ONLY: Self = Self {
        set_dropbox_ignore: false,
        skip_descendants: true,
    };
}

/// Rule match containing metadata useful for logging and control flow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuleMatch {
    pub(crate) name: &'static str,
    pub(crate) action: MatchAction,
}

impl RuleMatch {
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
}

/// Trait implemented by all matching rules to keep the system extensible.
pub(crate) trait Rule: Send + Sync {
    /// Human readable name used for logging.
    fn name(&self) -> &'static str;
    /// Returns true if the candidate satisfies this rule.
    fn matches(&self, candidate: &Candidate<'_>) -> bool;
    /// Behavior to apply when the rule matches.
    fn action(&self) -> MatchAction;
    /// Filenames whose creation may change this rule's verdict for a sibling.
    /// Creating any of these under a watched directory schedules a rescan of
    /// that directory's subtree.
    ///
    /// Scope invariant: a trigger is assumed to affect verdicts only within the
    /// trigger file's own directory subtree, so a scoped rescan fully
    /// reconciles it. `MarkedBuildDirRule` satisfies this — it consults a sibling
    /// marker file. A rule whose trigger has non-local effects would need a
    /// wider rescan scope than the event loop currently uses.
    fn triggers(&self) -> &'static [&'static str] {
        &[]
    }
}

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
        name.to_str()
            .is_some_and(|name| self.triggers.contains(name))
    }

    /// Returns the first matching rule. The ordering in `rules` defines priority.
    pub(crate) fn evaluate(&self, candidate: &Candidate<'_>) -> Option<RuleMatch> {
        for rule in &self.rules {
            if rule.matches(candidate) {
                return Some(RuleMatch {
                    name: rule.name(),
                    action: rule.action(),
                });
            }
        }
        None
    }
}

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

    pub(crate) const MAVEN_TARGET: Self = Self {
        name: "Maven target directory",
        dir: "target",
        markers: &["pom.xml"],
    };

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

/// JavaScript framework build output and tool cache directories matched by
/// exact name. Each is a framework- or tool-owned reproducible cache that
/// never holds user source.
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

/// Rule matching directories whose exact name is in a fixed list. This is the
/// common "tool-owned artifact directory" shape; every instance marks the
/// match and skips its descendants. Adding a directory to an existing
/// instance's list is a one-line change; a new category is a new constant.
pub(crate) struct ArtifactDirsRule {
    name: &'static str,
    dirs: &'static [&'static str],
    action: MatchAction,
}

impl ArtifactDirsRule {
    pub(crate) const NODE_MODULES: Self = Self {
        name: "node_modules directory",
        dirs: &["node_modules"],
        action: MatchAction::IGNORE_AND_SKIP,
    };
    pub(crate) const PNPM_STORE: Self = Self {
        name: "pnpm store directory",
        dirs: &[".pnpm-store"],
        action: MatchAction::IGNORE_AND_SKIP,
    };
    pub(crate) const PYTHON_CACHES: Self = Self {
        name: "Python build/cache artifact",
        dirs: PYTHON_ARTIFACT_DIRS,
        action: MatchAction::IGNORE_AND_SKIP,
    };
    pub(crate) const JS_BUILD: Self = Self {
        name: "JavaScript build/cache directory",
        dirs: JS_ARTIFACT_DIRS,
        action: MatchAction::IGNORE_AND_SKIP,
    };
    /// Project-local Gradle cache. The guarded `build` output lives in
    /// `MarkedBuildDirRule::GRADLE_BUILD`; `.gradle` is unconditional because
    /// a directory with this exact name is Gradle-owned in practice and
    /// marking is non-destructive (sync exclusion only).
    pub(crate) const JVM_CACHES: Self = Self {
        name: "Gradle cache directory",
        dirs: &[".gradle"],
        action: MatchAction::IGNORE_AND_SKIP,
    };
    /// IaC tool caches: `.terraform` holds provider/module downloads
    /// recreated by `terraform init`; `.terragrunt-cache` is Terragrunt's
    /// working copy.
    pub(crate) const IAC_CACHES: Self = Self {
        name: "IaC cache directory",
        dirs: &[".terraform", ".terragrunt-cache"],
        action: MatchAction::IGNORE_AND_SKIP,
    };
    /// Dev-environment state dirs: `.direnv` is direnv's layout/cache dir,
    /// `.devenv` is devenv's local state; both are regenerated on demand.
    pub(crate) const DEV_ENV_DIRS: Self = Self {
        name: "development environment directory",
        dirs: &[".direnv", ".devenv"],
        action: MatchAction::IGNORE_AND_SKIP,
    };

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
        self.action
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
            .is_some_and(|name| name.as_encoded_bytes().ends_with(b".egg-info"))
    }

    fn action(&self) -> MatchAction {
        MatchAction::IGNORE_AND_SKIP
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, Result};
    use tempfile::TempDir;

    #[test]
    fn node_modules_rule_matches_directory_name() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let target = temp.path().join("node_modules");
        fs::create_dir(&target)?;

        let metadata = fs::metadata(&target)?;
        let candidate = Candidate {
            path: &target,
            file_type: metadata.file_type(),
        };
        let engine = RuleEngine::new(vec![Box::new(ArtifactDirsRule::NODE_MODULES)]);

        let result = engine
            .evaluate(&candidate)
            .expect("rule should match node_modules");

        assert_eq!(result.name, "node_modules directory");
        assert!(result.action.set_dropbox_ignore);
        assert!(result.action.skip_descendants);
        Ok(())
    }

    #[test]
    fn pnpm_store_rule_matches_directory_name() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let target = temp.path().join(".pnpm-store");
        fs::create_dir(&target)?;

        let metadata = fs::metadata(&target)?;
        let candidate = Candidate {
            path: &target,
            file_type: metadata.file_type(),
        };
        let engine = RuleEngine::new(vec![Box::new(ArtifactDirsRule::PNPM_STORE)]);

        let result = engine
            .evaluate(&candidate)
            .expect("rule should match .pnpm-store");

        assert_eq!(result.name, "pnpm store directory");
        assert!(result.action.set_dropbox_ignore);
        assert!(result.action.skip_descendants);
        Ok(())
    }

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

    #[test]
    fn python_artifact_rule_matches_env_and_metadata() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let venv_dir = temp.path().join(".venv");
        let egg_info_dir = temp.path().join("package.egg-info");

        fs::create_dir(&venv_dir)?;
        fs::create_dir(&egg_info_dir)?;

        let engine = RuleEngine::new(vec![
            Box::new(ArtifactDirsRule::PYTHON_CACHES),
            Box::new(EggInfoRule),
        ]);

        let venv_meta = fs::metadata(&venv_dir)?;
        let venv_candidate = Candidate {
            path: &venv_dir,
            file_type: venv_meta.file_type(),
        };
        assert!(
            engine.evaluate(&venv_candidate).is_some(),
            ".venv should match"
        );

        let egg_meta = fs::metadata(&egg_info_dir)?;
        let egg_candidate = Candidate {
            path: &egg_info_dir,
            file_type: egg_meta.file_type(),
        };
        assert!(
            engine.evaluate(&egg_candidate).is_some(),
            ".egg-info should match"
        );
        Ok(())
    }

    #[test]
    fn cargo_target_rule_declares_cargo_toml_trigger() {
        assert_eq!(MarkedBuildDirRule::CARGO_TARGET.triggers(), &["Cargo.toml"]);
    }

    #[test]
    fn rule_engine_recognizes_cargo_toml_trigger() {
        let engine = RuleEngine::new(vec![Box::new(MarkedBuildDirRule::CARGO_TARGET)]);
        assert!(engine.is_trigger(OsStr::new("Cargo.toml")));
        assert!(!engine.is_trigger(OsStr::new("package.json")));
    }

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

    #[test]
    fn rule_engine_without_target_rule_has_no_triggers() {
        let engine = RuleEngine::new(vec![Box::new(ArtifactDirsRule::NODE_MODULES)]);
        assert!(!engine.is_trigger(OsStr::new("Cargo.toml")));
    }

    #[test]
    fn python_artifact_rule_matches_tool_caches() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let engine = RuleEngine::new(vec![Box::new(ArtifactDirsRule::PYTHON_CACHES)]);

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
            assert!(
                result.action.skip_descendants,
                "{name} must skip descendants"
            );
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
        let engine = RuleEngine::new(vec![Box::new(ArtifactDirsRule::PYTHON_CACHES)]);
        assert!(engine.evaluate(&candidate).is_none(), "src must not match");
        Ok(())
    }

    #[test]
    fn js_build_artifacts_rule_matches_framework_dirs() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let engine = RuleEngine::new(vec![Box::new(ArtifactDirsRule::JS_BUILD)]);

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
            assert!(
                result.action.skip_descendants,
                "{name} must skip descendants"
            );
        }
        Ok(())
    }

    #[test]
    fn artifact_dirs_rule_instances_match_their_directories() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let cases: &[(&ArtifactDirsRule, &str, &str)] = &[
            (
                &ArtifactDirsRule::NODE_MODULES,
                "node_modules",
                "node_modules directory",
            ),
            (
                &ArtifactDirsRule::PNPM_STORE,
                ".pnpm-store",
                "pnpm store directory",
            ),
            (
                &ArtifactDirsRule::PYTHON_CACHES,
                "__pycache__",
                "Python build/cache artifact",
            ),
            (
                &ArtifactDirsRule::JS_BUILD,
                ".next",
                "JavaScript build/cache directory",
            ),
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
    fn artifact_dirs_rule_ignores_file_named_like_dir() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let file = temp.path().join("node_modules");
        fs::write(&file, b"")?;
        let meta = fs::metadata(&file)?;
        let candidate = Candidate {
            path: &file,
            file_type: meta.file_type(),
        };
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
            let candidate = Candidate {
                path,
                file_type: meta.file_type(),
            };
            assert!(
                EggInfoRule.matches(&candidate),
                "{} should match",
                path.display()
            );
        }
        assert_eq!(EggInfoRule.action(), MatchAction::IGNORE_AND_SKIP);
        Ok(())
    }

    #[test]
    fn egg_info_rule_matches_non_utf8_prefixed_name() -> Result<()> {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let temp = TempDir::new().context("Failed to create temp dir")?;
        let name = OsStr::from_bytes(b"\xffpkg.egg-info");
        let file = temp.path().join(name);
        fs::write(&file, b"")?;

        let meta = fs::metadata(&file)?;
        let candidate = Candidate {
            path: &file,
            file_type: meta.file_type(),
        };
        assert!(
            EggInfoRule.matches(&candidate),
            "an .egg-info suffix must match even when the name has non-UTF-8 bytes"
        );
        Ok(())
    }

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

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn vcs_dirs_rule_skips_all_vcs_dirs_without_marking() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        assert_eq!(
            ArtifactDirsRule::VCS_DIRS.name(),
            "version control directory"
        );
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
}
