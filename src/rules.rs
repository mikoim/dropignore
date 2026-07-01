use log::info;
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
    pub(crate) fn log_matched(&self, path: &Path) {
        info!("Matched rule '{}' for {}", self.name, path.display());
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
    /// reconciles it. `RustTargetRule` satisfies this — it consults a sibling
    /// `Cargo.toml`. A rule whose trigger has non-local effects would need a
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
        name.to_str().is_some_and(|name| self.triggers.contains(name))
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

/// Rule that matches the Rust build output directory `target` when a `Cargo.toml`
/// exists in the same parent directory. This ensures we only ignore build artifacts
/// for actual Cargo projects.
pub(crate) struct RustTargetRule;

impl Rule for RustTargetRule {
    fn name(&self) -> &'static str {
        "Cargo target directory"
    }

    fn matches(&self, candidate: &Candidate<'_>) -> bool {
        // Check the directory name first to avoid unnecessary filesystem operations.
        if !candidate.is_dir_named("target") {
            return false;
        }

        // Ensure the parent contains a Cargo.toml so we only target Rust projects.
        let Some(parent) = candidate.path.parent() else {
            return false;
        };

        let cargo_toml = parent.join("Cargo.toml");
        cargo_toml.exists()
    }

    fn action(&self) -> MatchAction {
        MatchAction::IGNORE_AND_SKIP
    }

    fn triggers(&self) -> &'static [&'static str] {
        &["Cargo.toml"]
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
/// exact name. Each is reproducible and never holds user source. `.turbo` is
/// Turborepo's local cache (verified against its docs).
const JS_ARTIFACT_DIRS: &[&str] = &[".next", ".nuxt", ".turbo", ".parcel-cache"];

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
    fn rust_target_rule_requires_cargo_toml_in_parent() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let project_root = temp.path();
        let cargo_toml = project_root.join("Cargo.toml");
        fs::write(&cargo_toml, b"[package]\nname=\"demo\"")?;

        let target_dir = project_root.join("target");
        fs::create_dir(&target_dir)?;

        let metadata = fs::metadata(&target_dir)?;
        let candidate = Candidate {
            path: &target_dir,
            file_type: metadata.file_type(),
        };
        let engine = RuleEngine::new(vec![
            Box::new(RustTargetRule),
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
        let engine = RuleEngine::new(vec![Box::new(ArtifactDirsRule::PYTHON_CACHES)]);
        assert!(engine.evaluate(&candidate).is_none(), "src must not match");
        Ok(())
    }

    #[test]
    fn js_build_artifacts_rule_matches_framework_dirs() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let engine = RuleEngine::new(vec![Box::new(ArtifactDirsRule::JS_BUILD)]);

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
}
