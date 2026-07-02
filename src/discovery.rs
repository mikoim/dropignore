use crate::rules::{Candidate, RuleEngine};
use anyhow::Result;
use log::{debug, warn};
use std::fs;
use std::path::{Path, PathBuf};

/// Directories to watch and matching paths discovered during a filesystem walk.
#[derive(Debug, Default)]
pub(crate) struct DiscoveredPaths {
    pub(crate) watchers: Vec<PathBuf>,
    pub(crate) matches: Vec<PathBuf>,
}

/// Walk the directory tree rooted at `start` and gather:
/// - All paths that satisfy a matching rule.
/// - When `collect_watchers` is set, all directories that should be watched
///   (excluding ignored subtrees). `--scan-once` never registers watches, so
///   it skips this collection.
fn walk(start: &Path, rules: &RuleEngine, collect_watchers: bool) -> Result<DiscoveredPaths> {
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
            file_type: metadata.file_type(),
        };

        if let Some(matched) = rules.evaluate(&candidate) {
            matched.log_matched(&dir);
            let action = matched.action;

            if action.set_dropbox_ignore {
                discovered.matches.push(dir.clone());
            }

            if action.skip_descendants {
                // Do not traverse further down this subtree.
                continue;
            }
        }

        if collect_watchers {
            discovered.watchers.push(dir.clone());
        }

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
        }
    }

    Ok(discovered)
}

/// Walk the tree and gather both watch targets and rule matches.
pub(crate) fn discover_watch_targets(start: &Path, rules: &RuleEngine) -> Result<DiscoveredPaths> {
    walk(start, rules, true)
}

/// Walk the tree and return only rule matches, skipping watcher collection.
/// Used by `--scan-once`, which never registers inotify watches.
pub(crate) fn discover_matches(start: &Path, rules: &RuleEngine) -> Result<Vec<PathBuf>> {
    Ok(walk(start, rules, false)?.matches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{ArtifactDirsRule, EggInfoRule, MarkedBuildDirRule, RuleEngine};
    use anyhow::{Context, Result};
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn discover_watch_targets_skips_ignored_subtrees() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let keep_dir = temp.path().join("keep");
        let ignored_dir = temp.path().join("node_modules");
        let ignored_child = ignored_dir.join("deep");

        fs::create_dir(&keep_dir)?;
        fs::create_dir(&ignored_dir)?;
        fs::create_dir(&ignored_child)?;

        let engine = RuleEngine::new(vec![Box::new(ArtifactDirsRule::NODE_MODULES)]);
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

        let engine = RuleEngine::new(vec![
            Box::new(MarkedBuildDirRule::CARGO_TARGET),
            Box::new(ArtifactDirsRule::NODE_MODULES),
        ]);
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

        let engine = RuleEngine::new(vec![Box::new(ArtifactDirsRule::PYTHON_CACHES)]);
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

    #[test]
    fn discover_watch_targets_marks_egg_info_files() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let top = temp.path().join("pkg.egg-info");
        let sub = temp.path().join("sub");
        let nested = sub.join("inner.egg-info");
        fs::create_dir(&sub)?;
        fs::write(&top, b"")?;
        fs::write(&nested, b"")?;

        let engine = RuleEngine::new(vec![Box::new(EggInfoRule)]);
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

    #[test]
    fn discover_watch_targets_skips_pycache_subtree() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let cache = temp.path().join("__pycache__");
        let nested = cache.join("sub");
        fs::create_dir(&cache)?;
        fs::create_dir(&nested)?;

        let engine = RuleEngine::new(vec![Box::new(ArtifactDirsRule::PYTHON_CACHES)]);
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

    #[test]
    fn discover_matches_agrees_with_full_discovery() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        fs::create_dir(temp.path().join("keep"))?;
        fs::create_dir(temp.path().join("node_modules"))?;

        let engine = RuleEngine::new(vec![Box::new(ArtifactDirsRule::NODE_MODULES)]);
        let matches = discover_matches(temp.path(), &engine)?;
        let full = discover_watch_targets(temp.path(), &engine)?;

        assert_eq!(
            matches, full.matches,
            "both discovery entry points must report the same matches"
        );
        Ok(())
    }
}
