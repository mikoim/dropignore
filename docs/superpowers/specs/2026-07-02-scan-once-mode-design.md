# One-shot scan mode (`--scan-once`)

Date: 2026-07-02

## Summary

Add a `--scan-once` CLI flag that walks the tree once, marks every rule match
with the Dropbox ignore attribute, and exits — without initializing inotify,
registering watches, or installing signal handlers. This makes dropignore
usable from cron or a systemd timer where a resident watcher is unwanted, and
it costs nothing from the `max_user_watches` budget.

The resident watch mode is unchanged. `--scan-once` composes with `--dry-run`
to preview what would be marked and exit.

## Goals

- `dropignore --scan-once <DIR>` marks all existing matches and exits.
- No inotify initialization, watch registration, or signal-handler setup on
  the one-shot path; the process relies on default signal behavior.
- Exit code reflects the outcome: 0 when every matched path was marked (or
  dry-run), 1 when setup fails or at least one mark fails. Cron/systemd can
  detect failures from the exit code alone.
- All matched paths are attempted even when some fail, matching the resident
  mode's continue-past-failures behavior; the failure count is reported once
  at the end.

## Non-goals (YAGNI)

- Subcommand restructuring (`dropignore scan` / `dropignore watch`). The
  existing `dropignore <DIR>` invocation must keep working; a boolean flag is
  enough.
- Un-marking paths that no longer match. Separate backlog item, unchanged.
- Multiple roots, rule configuration, machine-readable output (JSON, counts on
  stdout). Logs via `env_logger` remain the only output.
- Changing the resident mode's exit-code semantics for per-path failures.

## Design

### CLI (`cli.rs`)

Add one flag to `CliArgs`:

```rust
/// Scan the tree once, mark matches, and exit without watching.
#[arg(long = "scan-once", default_value_t = false)]
pub(crate) scan_once: bool,
```

No conflict constraints: `--scan-once --dry-run` is valid and useful
(preview-only run).

### One-shot path (`app.rs`)

`run()` keeps its shared prefix — canonicalize the root, `ensure_directory`,
build the `RuleEngine` — then branches before any inotify or signal-handler
setup:

```rust
if args.scan_once {
    return scan_once(&root, args.dry_run, &rule_engine);
}
```

`scan_once` reuses the existing discovery and application primitives. Like
`apply_all`, it takes the application closure as a parameter so the bail path
is testable without real xattr failures; `run()` passes the real
`apply_dropbox_ignore`:

```rust
if args.scan_once {
    return scan_once(&root, &rule_engine, |path| {
        apply_dropbox_ignore(path, args.dry_run)
    });
}

/// Walk the tree once, apply `apply` to every rule match, and return. Watch
/// targets from discovery are ignored: nothing is registered with inotify.
/// Fails when at least one matched path could not be marked, so cron/systemd
/// sees a non-zero exit code.
fn scan_once<F>(root: &Path, rules: &RuleEngine, apply: F) -> Result<()>
where
    F: FnMut(&Path) -> Result<()>,
{
    let discovered = discover_watch_targets(root, rules)?;
    let total = discovered.matches.len();
    let failures = apply_all(&discovered.matches, apply);
    if failures > 0 {
        anyhow::bail!("Failed to mark {failures} of {total} matched path(s)");
    }
    info!(
        "Scan complete: {total} matched path(s) under {}",
        root.display()
    );
    Ok(())
}
```

`discovered.watchers` is intentionally dropped. Per-path errors are already
logged by `apply_dropbox_ignore`; the bail message adds the single rollup,
mirroring the warn in `apply_discovered_paths`.

The resident path (`Inotify::init`, signal handlers, seed + `event_loop`) is
untouched.

## Error handling

- Setup failures (root missing, not a directory, interior NUL) propagate as
  today → non-zero exit.
- Per-path mark failures: every path is still attempted (`apply_all`), each
  failure is logged by `apply_dropbox_ignore`, and `scan_once` returns `Err`
  with the failure/total counts → exit 1.
- Dry-run never writes, so it can only fail on discovery errors (none in
  practice: the walk warns and skips unreadable entries).

## Testing

- `scan_once` on a tempdir containing `node_modules` with a recording closure
  visits exactly the matched path and returns `Ok` — no xattr support needed.
- Bail path: a closure that always fails makes `scan_once` return `Err` whose
  message carries the failure/total counts.
- End-to-end marking with the real `apply_dropbox_ignore` (dry_run = false) on
  a tempdir, guarded by the existing xattr-support probe pattern, skipping on
  filesystems without `user.*` xattrs.
- CLI: `--scan-once` parses into `scan_once: true`; default is `false`.
- `cargo test`, `cargo clippy --all-targets` (zero warnings), and
  `cargo fmt --check` stay green.

## Rollout

Single change set. No new dependencies, no on-disk format changes, no change
to resident-mode behavior. README gains a `--scan-once` usage line and a
cron/systemd-timer example under Usage.
