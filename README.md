# dropignore

CLI tool that watches a directory with inotify and marks matching paths with Dropbox's `user.com.dropbox.ignored` extended attribute. Designed for performance, maintainability, and easy rule expansion.

## Features
- Recursive watch starting at a user-specified root, using dynamic inotify registrations.
- Rule-based matching (currently: `node_modules`, pnpm `.pnpm-store`, Cargo/Maven `target` with an adjacent `Cargo.toml`/`pom.xml`, Gradle `build` with an adjacent Gradle build/settings script, Gradle cache `.gradle`, Python virtualenvs `venv`/`.venv`, `*.egg-info`, Python tool caches `__pycache__`/`.pytest_cache`/`.mypy_cache`/`.ruff_cache`/`.tox`, JS build/cache dirs `.next`/`.nuxt`/`.turbo`/`.parcel-cache`/`.svelte-kit`/`.astro`/`.angular`/`.vite`, IaC caches `.terraform`/`.terragrunt-cache`, and dev-environment dirs `.direnv`/`.devenv`).
- Skips descending into ignored subtrees to avoid unnecessary watches.
- Dry-run mode logs intended actions without calling `setxattr`.
- Detailed logging via `env_logger`.
- One-shot scan mode (`--scan-once`) for cron or systemd-timer use: marks existing matches and exits without watching.

## Usage
```bash
cargo run -- --dry-run /home/foo/Dropbox  # inspect what would be ignored
cargo run -- /home/foo/Dropbox            # apply Dropbox ignore attribute
cargo run -- --scan-once /home/foo/Dropbox  # mark existing matches once and exit
```

### One-shot scans
`--scan-once` walks the tree once, marks every match, and exits without
registering any inotify watches. It composes with `--dry-run` to preview.
The process exits non-zero when at least one matched path could not be
marked, so failures surface through cron mail or a systemd `OnFailure=`
unit. Example crontab entry:

```cron
0 * * * * /usr/local/bin/dropignore --scan-once /home/foo/Dropbox
```

### Logging
Logs default to `info`. Override with `RUST_LOG`, e.g.:
```bash
RUST_LOG=debug cargo run -- --dry-run /home/foo/Dropbox
```

## How it works
1. Seeds watches for all traversable subdirectories under the root, skipping any directory matched by a rule, and marks any rule-matching file or directory that already exists (e.g. a pre-existing `*.egg-info`).
2. Applies `user.com.dropbox.ignored=1` to any matched path (or logs in dry-run).
3. Listens for create/move-in events and processes new paths, adding watches for newly discovered directories unless a rule says to skip descendants.
4. Skips symlinks (matching the initial walk) and, if the inotify event queue overflows, re-scans from the root so dropped events cannot leave paths unmarked. Creating a rule's dependency file (e.g. a `Cargo.toml` next to an existing `target`) triggers a re-scan of just that file's directory subtree, reconciling order-dependent rules without a restart or a whole-tree walk. If the watched root itself is moved or deleted, the process exits with an error so a supervisor can restart it.

## Testing
```bash
cargo test
```

## Extending rules
For a new "ignore directories with these exact names" rule, add the name to an
existing `ArtifactDirsRule` list in `src/rules.rs` (or add a new associated
constant). For a build directory that should only match next to a project
marker file (like Cargo's `target` next to `Cargo.toml`), add a
`MarkedBuildDirRule` constant; its markers automatically become rescan
triggers. Register new constants in `RuleEngine::new` in `src/app.rs`. For
anything else, implement the `Rule` trait directly.
