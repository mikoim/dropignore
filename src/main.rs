//! CLI tool that watches for newly created paths under a user supplied root and marks
//! matching entries with Dropbox's `com.dropbox.ignored` extended attribute.
//! The implementation focuses on:
//! - Efficient recursive monitoring using inotify with a dynamic watch set.
//! - A pluggable rule system so new matching conditions can be added easily.
//! - A dry-run mode for safe inspection without mutating the filesystem.

use anyhow::{Context, Result};
use clap::Parser;
use env_logger::Env;
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Attribute name that instructs Dropbox to ignore a path.
const DROPBOX_IGNORE_ATTR: &str = "com.dropbox.ignored";
/// Attribute value recognized by Dropbox.
const DROPBOX_IGNORE_VALUE: &[u8] = b"1";

/// Compute the inotify mask used for every watched directory.
fn watch_mask() -> WatchMask {
    WatchMask::CREATE | WatchMask::MOVED_TO | WatchMask::DELETE_SELF | WatchMask::ONLYDIR
}

/// Command line arguments handled by clap.
#[derive(Debug, Parser)]
#[command(
    name = "dropignore",
    about = "Watch a directory and tag matching paths with the Dropbox ignore attribute."
)]
struct CliArgs {
    /// Root directory to watch recursively.
    #[arg(value_name = "DIRECTORY")]
    root: PathBuf,
    /// Skip calling setxattr and only log intended actions.
    #[arg(short = 'n', long = "dry-run", default_value_t = false)]
    dry_run: bool,
}

fn main() -> Result<()> {
    let args = CliArgs::parse();
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();
    run(args)
}

/// Entrypoint that wires argument parsing, rule initialization, and the inotify loop.
fn run(args: CliArgs) -> Result<()> {
    let root = args
        .root
        .canonicalize()
        .with_context(|| format!("Failed to resolve path: {}", args.root.display()))?;
    ensure_directory(&root)?;

    let rule_engine = RuleEngine::new(vec![
        Box::new(NodeModulesRule),
        Box::new(RustTargetRule),
        Box::new(PythonBuildArtifactsRule),
    ]);
    let mut watcher = Inotify::init().context("Failed to initialize inotify")?;
    let mut registry = WatchRegistry::default();

    // Seed watches for existing directories and apply the attribute to any
    // pre-existing matches to keep the tree in a consistent state.
    let initial = discover_watch_targets(&root, &rule_engine)?;
    apply_discovered_paths(initial, args.dry_run, &mut watcher, &mut registry)?;

    info!("Watching {}", root.display());
    event_loop(args.dry_run, watcher, registry, rule_engine)
}

/// Main blocking loop that reads inotify events and reacts to creations/moves.
fn event_loop(
    dry_run: bool,
    mut watcher: Inotify,
    mut registry: WatchRegistry,
    rule_engine: RuleEngine,
) -> Result<()> {
    let mut buffer = [0u8; 4096];

    loop {
        let events = watcher
            .read_events_blocking(&mut buffer)
            .context("Failed to read inotify events")?;

        // Collect new directories to process after the borrow from `events` ends,
        // which keeps the borrow checker happy while allowing new watches to be added.
        let mut pending_directories: Vec<PathBuf> = Vec::new();

        for event in events {
            // Remove bookkeeping for directories that disappeared to avoid stale mappings.
            if event.mask.contains(EventMask::DELETE_SELF) {
                registry.remove_by_descriptor(&event.wd);
                continue;
            }

            if !(event.mask.contains(EventMask::CREATE) || event.mask.contains(EventMask::MOVED_TO))
            {
                continue;
            }

            let parent_dir = match registry.path_for(&event.wd) {
                Some(path) => path,
                None => {
                    warn!("Received event for unknown watch descriptor {:?}", event.wd);
                    continue;
                }
            };

            let name = match &event.name {
                Some(name) => name,
                None => {
                    debug!("Ignored event without a name in {}", parent_dir.display());
                    continue;
                }
            };

            let full_path = parent_dir.join(name);
            let metadata = match fs::metadata(&full_path) {
                Ok(m) => m,
                Err(err) => {
                    warn!(
                        "Skipping {} because metadata could not be read: {err}",
                        full_path.display()
                    );
                    continue;
                }
            };

            let candidate = Candidate {
                path: &full_path,
                metadata: &metadata,
            };

            if let Some(action) = rule_engine.evaluate_action(&candidate) {
                if action.set_dropbox_ignore {
                    apply_dropbox_ignore(&full_path, dry_run)?;
                }

                if action.skip_descendants && candidate.is_dir() {
                    // Do not watch inside directories that we decided to ignore.
                    continue;
                }
            }

            if candidate.is_dir() {
                pending_directories.push(full_path);
            }
        }

        // Process newly discovered directories once the event iterator is dropped so
        // inotify can be borrowed mutably again.
        for directory in pending_directories {
            let discovered = match discover_watch_targets(&directory, &rule_engine) {
                Ok(d) => d,
                Err(err) => {
                    warn!(
                        "Failed to walk {} for watch seeding: {err}",
                        directory.display()
                    );
                    continue;
                }
            };

            apply_discovered_paths(discovered, dry_run, &mut watcher, &mut registry)?;
        }
    }
}

/// Ensure the path exists and is a directory.
fn ensure_directory(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("Failed to read metadata for {}", path.display()))?;
    if !metadata.is_dir() {
        anyhow::bail!("{} is not a directory", path.display());
    }
    Ok(())
}

/// Register a directory with inotify if it has not already been registered.
fn add_watch(watcher: &mut Inotify, registry: &mut WatchRegistry, path: &Path) -> Result<()> {
    if registry.contains_path(path) {
        return Ok(());
    }

    let descriptor = watcher
        .watches()
        .add(path, watch_mask())
        .with_context(|| format!("Failed to add watch for {}", path.display()))?;

    registry.insert(path.to_path_buf(), descriptor);
    debug!("Watching {}", path.display());
    Ok(())
}

/// Apply the Dropbox ignore attribute to the given path, honoring dry-run mode.
fn apply_dropbox_ignore(path: &Path, dry_run: bool) -> Result<()> {
    if dry_run {
        info!("(dry-run) Would mark {} as ignored", path.display());
        return Ok(());
    }

    // Construct C strings for the path and attribute name. Path conversion uses
    // raw bytes to support non-UTF8 names on Unix.
    let c_path = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("Path contains interior NUL byte: {}", path.display()))?;
    let c_name =
        CString::new(DROPBOX_IGNORE_ATTR).expect("static attribute name should never contain NUL");

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

fn apply_discovered_paths(
    discovered: DiscoveredPaths,
    dry_run: bool,
    watcher: &mut Inotify,
    registry: &mut WatchRegistry,
) -> Result<()> {
    for matched in discovered.matches {
        apply_dropbox_ignore(&matched, dry_run)?;
    }

    for directory in discovered.watchers {
        add_watch(watcher, registry, &directory)?;
    }

    Ok(())
}

/// Representation of a filesystem entry that can be evaluated against rules.
#[derive(Debug)]
struct Candidate<'a> {
    path: &'a Path,
    metadata: &'a fs::Metadata,
}

impl Candidate<'_> {
    fn is_dir(&self) -> bool {
        self.metadata.is_dir()
    }

    fn is_dir_named(&self, name: &str) -> bool {
        self.is_dir() && self.path.file_name().is_some_and(|entry| entry == name)
    }
}

/// Action taken when a rule matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MatchAction {
    set_dropbox_ignore: bool,
    skip_descendants: bool,
}

impl MatchAction {
    const IGNORE_AND_SKIP: Self = Self {
        set_dropbox_ignore: true,
        skip_descendants: true,
    };
}

/// Rule match containing metadata useful for logging and control flow.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RuleMatch {
    name: &'static str,
    action: MatchAction,
}

/// Trait implemented by all matching rules to keep the system extensible.
trait Rule: Send + Sync {
    /// Human readable name used for logging.
    fn name(&self) -> &'static str;
    /// Returns true if the candidate satisfies this rule.
    fn matches(&self, candidate: &Candidate<'_>) -> bool;
    /// Behavior to apply when the rule matches.
    fn action(&self) -> MatchAction;
}

/// Simple rule engine that evaluates candidates against registered rules.
struct RuleEngine {
    rules: Vec<Box<dyn Rule>>,
}

impl RuleEngine {
    fn new(rules: Vec<Box<dyn Rule>>) -> Self {
        Self { rules }
    }

    /// Returns the first matching rule. The ordering in `rules` defines priority.
    fn evaluate(&self, candidate: &Candidate<'_>) -> Option<RuleMatch> {
        for rule in &self.rules {
            if rule.matches(candidate) {
                let matched = RuleMatch {
                    name: rule.name(),
                    action: rule.action(),
                };
                info!(
                    "Matched rule '{}' for {}",
                    matched.name,
                    candidate.path.display()
                );
                return Some(matched);
            }
        }
        None
    }

    fn evaluate_action(&self, candidate: &Candidate<'_>) -> Option<MatchAction> {
        self.evaluate(candidate).map(|matched| matched.action)
    }
}

/// Rule that matches directories named exactly `node_modules`.
struct NodeModulesRule;

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

/// Rule that matches the Rust build output directory `target` when a `Cargo.toml`
/// exists in the same parent directory. This ensures we only ignore build artifacts
/// for actual Cargo projects.
struct RustTargetRule;

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
}

/// Rule that captures large, reproducible Python build or cache artifacts typically
/// found in uv-managed projects. These are safe to ignore in Dropbox sync to reduce
/// noise and storage churn.
struct PythonBuildArtifactsRule;

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

/// Directories to watch and matching paths discovered during a filesystem walk.
#[derive(Debug, Default)]
struct DiscoveredPaths {
    watchers: Vec<PathBuf>,
    matches: Vec<PathBuf>,
}

/// Walk the directory tree rooted at `start` and gather:
/// - All directories that should be watched (excluding ignored subtrees).
/// - All paths that satisfy a matching rule.
fn discover_watch_targets(start: &Path, rules: &RuleEngine) -> Result<DiscoveredPaths> {
    let mut discovered = DiscoveredPaths::default();
    let mut stack = vec![start.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let metadata = match fs::metadata(&dir) {
            Ok(meta) => meta,
            Err(err) => {
                warn!("Skipping {}: {err}", dir.display());
                continue;
            }
        };

        let candidate = Candidate {
            path: &dir,
            metadata: &metadata,
        };

        if let Some(action) = rules.evaluate_action(&candidate) {
            if action.set_dropbox_ignore {
                discovered.matches.push(dir.clone());
            }

            if action.skip_descendants {
                // Do not traverse further down this subtree.
                continue;
            }
        }

        discovered.watchers.push(dir.clone());

        let read_dir = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(err) => {
                warn!("Failed to read {}: {err}", dir.display());
                continue;
            }
        };

        for entry in read_dir {
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    warn!("Error iterating inside {}: {err}", dir.display());
                    continue;
                }
            };

            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(err) => {
                    warn!("Error reading file type under {}: {err}", dir.display());
                    continue;
                }
            };

            if file_type.is_symlink() {
                debug!(
                    "Ignoring symlink {} to avoid cycles",
                    entry.path().display()
                );
                continue;
            }

            if file_type.is_dir() {
                stack.push(entry.path());
            }
        }
    }

    Ok(discovered)
}

/// Bookkeeping for inotify watch descriptors and their corresponding paths.
#[derive(Default)]
struct WatchRegistry {
    by_descriptor: HashMap<WatchDescriptor, PathBuf>,
    by_path: HashMap<PathBuf, WatchDescriptor>,
}

impl WatchRegistry {
    fn insert(&mut self, path: PathBuf, descriptor: WatchDescriptor) {
        self.by_path.insert(path.clone(), descriptor.clone());
        self.by_descriptor.insert(descriptor, path);
    }

    fn remove_by_descriptor(&mut self, descriptor: &WatchDescriptor) {
        if let Some(path) = self.by_descriptor.remove(descriptor) {
            debug!("Removing watch for {}", path.display());
            self.by_path.remove(&path);
        }
    }

    fn path_for(&self, descriptor: &WatchDescriptor) -> Option<&PathBuf> {
        self.by_descriptor.get(descriptor)
    }

    fn contains_path(&self, path: &Path) -> bool {
        self.by_path.contains_key(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn node_modules_rule_matches_directory_name() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let target = temp.path().join("node_modules");
        fs::create_dir(&target)?;

        let metadata = fs::metadata(&target)?;
        let candidate = Candidate {
            path: &target,
            metadata: &metadata,
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
            metadata: &metadata,
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
            metadata: &venv_meta,
        };
        assert!(
            engine.evaluate(&venv_candidate).is_some(),
            ".venv should match"
        );

        let egg_meta = fs::metadata(&egg_info_dir)?;
        let egg_candidate = Candidate {
            path: &egg_info_dir,
            metadata: &egg_meta,
        };
        assert!(
            engine.evaluate(&egg_candidate).is_some(),
            ".egg-info should match"
        );
        Ok(())
    }

    #[test]
    fn discover_watch_targets_skips_ignored_subtrees() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let keep_dir = temp.path().join("keep");
        let ignored_dir = temp.path().join("node_modules");
        let ignored_child = ignored_dir.join("deep");

        fs::create_dir(&keep_dir)?;
        fs::create_dir(&ignored_dir)?;
        fs::create_dir(&ignored_child)?;

        let engine = RuleEngine::new(vec![Box::new(NodeModulesRule)]);
        let discovered = discover_watch_targets(temp.path(), &engine)?;

        assert!(
            discovered.watchers.contains(&temp.path().to_path_buf()),
            "root should be watched"
        );
        assert!(
            discovered.watchers.contains(&keep_dir),
            "non matching directory should be watched"
        );
        assert!(
            !discovered.watchers.contains(&ignored_dir),
            "ignored directory should not be watched"
        );
        assert!(
            !discovered.watchers.contains(&ignored_child),
            "child of ignored directory should not be watched"
        );
        assert!(
            discovered.matches.contains(&ignored_dir),
            "ignored directory should be marked for attribute application"
        );
        Ok(())
    }

    #[test]
    fn discover_watch_targets_handles_cargo_target() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let cargo_root = temp.path();
        fs::write(cargo_root.join("Cargo.toml"), b"[package]\nname=\"demo\"")?;
        let target_dir = cargo_root.join("target");
        let nested_dir = target_dir.join("debug");
        fs::create_dir(&target_dir)?;
        fs::create_dir(&nested_dir)?;

        let engine = RuleEngine::new(vec![Box::new(RustTargetRule), Box::new(NodeModulesRule)]);
        let discovered = discover_watch_targets(cargo_root, &engine)?;

        assert!(
            discovered.matches.contains(&target_dir),
            "target should be marked for attribute application"
        );
        assert!(
            !discovered.watchers.contains(&target_dir),
            "target directory should not be watched"
        );
        assert!(
            !discovered.watchers.contains(&nested_dir),
            "children of target directory should not be watched"
        );
        Ok(())
    }

    #[test]
    fn discover_watch_targets_skips_python_envs() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let root = temp.path();
        let venv_dir = root.join(".venv");
        fs::create_dir(&venv_dir)?;

        let engine = RuleEngine::new(vec![Box::new(PythonBuildArtifactsRule)]);
        let discovered = discover_watch_targets(root, &engine)?;

        assert!(
            discovered.matches.contains(&venv_dir),
            ".venv should be marked"
        );
        assert!(
            !discovered.watchers.contains(&venv_dir),
            ".venv should not be watched"
        );
        Ok(())
    }
}
