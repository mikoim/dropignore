use crate::cli::CliArgs;
use crate::discovery::{DiscoveredPaths, discover_watch_targets};
use crate::dropbox::apply_dropbox_ignore;
use crate::rules::{
    Candidate, NodeModulesRule, PnpmStoreRule, PythonBuildArtifactsRule, RuleEngine, RustTargetRule,
};
use crate::watch::{WatchRegistry, add_watch};
use anyhow::{Context, Result};
use inotify::{EventMask, Inotify};
use log::{debug, info, warn};
use std::fs;
use std::path::{Path, PathBuf};

/// Entrypoint that wires argument parsing, rule initialization, and the inotify loop.
pub(crate) fn run(args: CliArgs) -> Result<()> {
    let root = args
        .root
        .canonicalize()
        .with_context(|| format!("Failed to resolve path: {}", args.root.display()))?;
    ensure_directory(&root)?;

    let rule_engine = RuleEngine::new(vec![
        Box::new(NodeModulesRule),
        Box::new(PnpmStoreRule),
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
                    // Continue past a failure here too; errors are already logged
                    // by apply_dropbox_ignore.
                    apply_all(std::slice::from_ref(&full_path), |path| {
                        apply_dropbox_ignore(path, dry_run)
                    });
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

/// Apply `apply` to every path, continuing past individual failures, and
/// return how many failed. Callees are expected to log their own errors
/// (see `apply_dropbox_ignore`), so this helper stays silent.
fn apply_all<F>(paths: &[PathBuf], mut apply: F) -> usize
where
    F: FnMut(&Path) -> Result<()>,
{
    let mut failures = 0;
    for path in paths {
        if apply(path).is_err() {
            failures += 1;
        }
    }
    failures
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

fn apply_discovered_paths(
    discovered: DiscoveredPaths,
    dry_run: bool,
    watcher: &mut Inotify,
    registry: &mut WatchRegistry,
) -> Result<()> {
    // Apply to every match, continuing past individual failures so one bad
    // path cannot terminate the watcher.
    apply_all(&discovered.matches, |path| apply_dropbox_ignore(path, dry_run));

    for directory in discovered.watchers {
        add_watch(watcher, registry, &directory)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn apply_all_visits_every_path_despite_failures() {
        let paths = vec![
            PathBuf::from("a"),
            PathBuf::from("b"),
            PathBuf::from("c"),
        ];
        let mut seen = Vec::new();
        let failures = apply_all(&paths, |p| {
            seen.push(p.to_path_buf());
            if p == Path::new("b") {
                anyhow::bail!("boom");
            }
            Ok(())
        });

        assert_eq!(failures, 1, "the single failing path should be counted");
        assert_eq!(seen, paths, "every path must be visited even after a failure");
    }
}
