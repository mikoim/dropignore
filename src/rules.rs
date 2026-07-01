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
    /// Creating any of these under a watched directory schedules a rescan.
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

/// Rule that matches directories named exactly `node_modules`.
pub(crate) struct NodeModulesRule;

impl Rule for NodeModulesRule {
    fn name(&self) -> &'static str {
        "node_modules directory"
    }

    fn matches(&self, candidate: &Candidate<'_>) -> bool {
        candidate.is_dir_named("node_modules")
    }

    fn action(&self) -> MatchAction {
        MatchAction::IGNORE_AND_SKIP
    }
}

/// Rule that matches pnpm's content-addressable store directory `.pnpm-store`.
pub(crate) struct PnpmStoreRule;

impl Rule for PnpmStoreRule {
    fn name(&self) -> &'static str {
        "pnpm store directory"
    }

    fn matches(&self, candidate: &Candidate<'_>) -> bool {
        candidate.is_dir_named(".pnpm-store")
    }

    fn action(&self) -> MatchAction {
        MatchAction::IGNORE_AND_SKIP
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

/// Rule that captures large, reproducible Python build or cache artifacts typically
/// found in uv-managed projects. These are safe to ignore in Dropbox sync to reduce
/// noise and storage churn.
pub(crate) struct PythonBuildArtifactsRule;

impl Rule for PythonBuildArtifactsRule {
    fn name(&self) -> &'static str {
        "Python build/cache artifact"
    }

    fn matches(&self, candidate: &Candidate<'_>) -> bool {
        let Some(file_name) = candidate.path.file_name() else {
            return false;
        };

        let is_dir = candidate.is_dir();
        let name = file_name.to_string_lossy();

        // Virtual environments (uv default is .venv) and common aliases.
        if is_dir && (name == ".venv" || name == "venv") {
            return true;
        }

        // egg-info metadata can be a directory or file; match by suffix. This
        // intentionally excludes other transient caches to keep the rule scoped.
        if name.ends_with(".egg-info") {
            return true;
        }

        false
    }

    fn action(&self) -> MatchAction {
        // Directories should not be traversed; files have no descendants,
        // so this flag is fine to leave true for both cases.
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
        let engine = RuleEngine::new(vec![Box::new(NodeModulesRule)]);

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
        let engine = RuleEngine::new(vec![Box::new(PnpmStoreRule)]);

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
        let engine = RuleEngine::new(vec![Box::new(RustTargetRule), Box::new(NodeModulesRule)]);

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

        let engine = RuleEngine::new(vec![Box::new(PythonBuildArtifactsRule)]);

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
        let engine = RuleEngine::new(vec![Box::new(NodeModulesRule)]);
        assert!(!engine.is_trigger(OsStr::new("Cargo.toml")));
    }
}
