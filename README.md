# dropignore

CLI tool that watches a directory with inotify and marks matching paths with Dropbox's `user.com.dropbox.ignored` extended attribute. Designed for performance, maintainability, and easy rule expansion.

## Features
- Recursive watch starting at a user-specified root, using dynamic inotify registrations.
- Rule-based matching (currently: `node_modules`, pnpm `.pnpm-store`, Cargo `target` with adjacent `Cargo.toml`, Python virtualenvs `venv`/`.venv`, and `*.egg-info`).
- Skips descending into ignored subtrees to avoid unnecessary watches.
- Dry-run mode logs intended actions without calling `setxattr`.
- Detailed logging via `env_logger`.

## Usage
```bash
cargo run -- --dry-run /home/foo/Dropbox  # inspect what would be ignored
cargo run -- /home/foo/Dropbox            # apply Dropbox ignore attribute
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
4. Skips symlinks (matching the initial walk) and, if the inotify event queue overflows, re-scans from the root so dropped events cannot leave paths unmarked. Creating a rule's dependency file (e.g. a `Cargo.toml` next to an existing `target`) triggers a re-scan of just that file's directory subtree, reconciling order-dependent rules without a restart or a whole-tree walk.

## Testing
```bash
cargo test
```

## Extending rules
Add a new type implementing the `Rule` trait in `src/rules.rs`, return the desired `MatchAction`, and register it in the `RuleEngine::new` call in `src/app.rs`. The existing rules serve as templates.
