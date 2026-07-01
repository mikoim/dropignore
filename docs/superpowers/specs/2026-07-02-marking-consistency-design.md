# Marking consistency between initial scan and runtime

Date: 2026-07-02

## Summary

Close two gaps where the initial filesystem scan (`discover_watch_targets`) and
the runtime inotify path (`plan_entry` / `event_loop`) disagree about which paths
get the Dropbox ignore attribute. This is a correctness/consistency change only:
no new features, no new external dependencies, no change to the rule set itself.

The two gaps:

- **A. Initial scan never evaluates files.** The discovery walk pushes only
  directories onto its work stack, so the only `Candidate`s it evaluates are the
  root and subdirectories. `PythonBuildArtifactsRule` matches `*.egg-info` as a
  *file* too, and the runtime path (`plan_entry`) handles files, but a
  `*.egg-info` file that already exists at startup is never marked until it is
  recreated at runtime.

- **B. Order-dependent rules are not re-evaluated.** `RustTargetRule` matches a
  `target` directory only when a sibling `Cargo.toml` exists. If `target/`
  exists before `Cargo.toml`, `target` is watched (not ignored). When
  `Cargo.toml` is later created, the pre-existing sibling `target` is never
  re-evaluated and stays unmarked until a restart or an overflow rescan.

## Goals

- The set of paths marked by the initial scan matches what the runtime path
  would mark for the same tree.
- Creation of a rule's dependency file (e.g. `Cargo.toml`) causes the dependent
  sibling to be reconciled without a restart.
- Keep the rule abstraction clean: no rule-specific filenames leak into the
  event loop.

## Non-goals (YAGNI)

- Scoped subtree rebuild / `WatchRegistry::drain_subtree` (B-scoped). Deferred
  until measurement shows the whole-tree rescan is too costly. Concrete signal
  to revisit: editors that save a trigger file via write-temp-then-rename emit
  `IN_MOVED_TO` with the trigger name, so each save of a watched project's
  `Cargo.toml` fires a full-tree `rebuild_watches`. For an actively edited
  project inside a large Dropbox tree this exceeds the "low trigger frequency"
  assumption and routinely exercises the teardown-before-reseed window, which a
  scoped rebuild would confine to the affected subtree. (Plain `MODIFY` churn
  does not trigger — the check is under the `CREATE || MOVED_TO` guard.)
- Config-file-driven rules.
- Signal handling / systemd integration.
- Cross-platform (macOS) support.

## A. Initial scan marks rule-matching files

### Candidate holds a `FileType`

`Candidate` currently borrows `&fs::Metadata`, but the rules only ever need the
entry type and its name (`is_dir`, `is_symlink`, `is_dir_named`). Replace the
borrowed metadata with an owned `std::fs::FileType` (which is `Copy`):

```rust
pub(crate) struct Candidate<'a> {
    pub(crate) path: &'a Path,
    pub(crate) file_type: std::fs::FileType,
}

impl Candidate<'_> {
    pub(crate) fn is_dir(&self) -> bool { self.file_type.is_dir() }
    pub(crate) fn is_symlink(&self) -> bool { self.file_type.is_symlink() }
    // is_dir_named unchanged (uses is_dir() + path.file_name())
}
```

Rationale: the readdir iterator already yields a `FileType` per entry via
`entry.file_type()` (backed by `d_type`, no extra `stat` in the common case), so
a file `Candidate` can be built with no additional syscall. Building it from
`entry.metadata()` instead would add an `lstat` per file entry on file-heavy
trees; rejected for that cost.

### Evaluate files during the walk

In `discover_watch_targets`, the child-entry loop currently skips symlinks and
pushes directories onto the stack. Add evaluation for the remaining case
(non-symlink, non-directory entries):

```rust
if file_type.is_symlink() {
    debug!("Ignoring symlink {} to avoid cycles", entry.path().display());
    continue;
}

if file_type.is_dir() {
    stack.push(entry.path());
    continue; // directories are evaluated when popped
}

// Non-directory entry: evaluate now so files like *.egg-info that already
// exist at startup are marked, mirroring the runtime path in plan_entry.
let path = entry.path();
let candidate = Candidate { path: &path, file_type };
if let Some(matched) = rules.evaluate(&candidate) {
    matched.log_matched(&path);
    if matched.action.set_dropbox_ignore {
        discovered.matches.push(path);
    }
    // Files have no descendants, so skip_descendants is irrelevant.
}
```

Directories keep being evaluated on pop (so `skip_descendants` still prunes the
stack). The popped-node `Candidate` is built from the existing
`fs::metadata(&dir)` call via `.file_type()`; popped nodes are never symlinks
(symlinks are skipped before being pushed), so following-vs-not is immaterial.

### Construction sites to update

Every `Candidate { path, metadata }` becomes `Candidate { path, file_type }`:

- `discovery.rs`: the popped-node site (pass `metadata.file_type()`) and the new
  file-evaluation site (pass the entry's `file_type`).
- `app.rs` `event_loop`: pass `metadata.file_type()` from the existing
  `symlink_metadata` result (this preserves the symlink detection in
  `plan_entry`).
- Test modules in `rules.rs` and `app.rs`: pass `metadata.file_type()`.

## B. Re-evaluate order-dependent rules on trigger-file creation (B-full)

### Rules declare trigger filenames

Add a defaulted method to the `Rule` trait so dependency knowledge stays inside
the rule that owns it:

```rust
pub(crate) trait Rule: Send + Sync {
    // ... existing methods ...

    /// Filenames whose creation may change this rule's verdict for a sibling.
    /// Creation of any of these under a watched directory schedules a rescan.
    fn triggers(&self) -> &'static [&'static str] { &[] }
}
```

`RustTargetRule` returns `&["Cargo.toml"]`; all other rules use the default
empty slice.

### RuleEngine aggregates triggers

```rust
pub(crate) struct RuleEngine {
    rules: Vec<Box<dyn Rule>>,
    triggers: std::collections::HashSet<&'static str>,
}

impl RuleEngine {
    pub(crate) fn new(rules: Vec<Box<dyn Rule>>) -> Self {
        let triggers = rules
            .iter()
            .flat_map(|rule| rule.triggers().iter().copied())
            .collect();
        Self { rules, triggers }
    }

    /// True when `name` is a dependency filename declared by some rule.
    pub(crate) fn is_trigger(&self, name: &std::ffi::OsStr) -> bool {
        name.to_str().is_some_and(|name| self.triggers.contains(name))
    }
}
```

Trigger names are static ASCII, so non-UTF-8 event names simply never match.

### Event loop schedules a rescan on triggers

`event_loop` already collapses inotify overflow into a single end-of-batch
`needs_rescan` that runs `rebuild_watches(root)`. Reuse that exact path: when a
created entry's name is a trigger, set `needs_rescan = true` and log the reason.

In the CREATE/MOVED_TO handling, right after `name` is resolved and before the
`symlink_metadata` read:

```rust
if rule_engine.is_trigger(name) {
    info!(
        "Trigger file {} created; rescanning {} to reconcile dependent rules",
        parent_dir.join(name).display(),
        root.display()
    );
    needs_rescan = true;
}
```

The check runs before the metadata read so a transient `symlink_metadata`
failure on the trigger file still schedules the rescan. Normal handling of the
created entry then proceeds (falls through); the whole-tree `rebuild_watches`
at batch end supersedes any redundant per-entry work in that batch. Because
`rebuild_watches` and `add_watch` are idempotent, converging the trigger and
overflow paths on the same flag is safe.

## Error handling

- **A** introduces no new fallible operations: the entry's `file_type` is
  already in hand from the readdir iteration; a rule match only logs and pushes.
- **B** reuses `rebuild_watches`, which already tears down every watch, re-runs
  `discover_watch_targets`, and warns (without aborting the loop) if discovery
  fails. The documented teardown-before-reseed hazard is unchanged and already
  accepted for the overflow path.

## Testing

### A

- `discover_watch_targets` marks a top-level `pkg.egg-info` *file* (create the
  file, assert it appears in `matches`). This fails against the current code.
- A `*.egg-info` file nested inside a watched subdirectory is also marked.
- `Candidate` construction sites in `rules.rs` / `app.rs` tests updated to the
  `file_type` shape; all existing rule and `plan_entry` assertions stay green.

### B

- `RuleEngine::is_trigger` returns `true` for `"Cargo.toml"`, `false` for an
  unrelated name, and `false` for an engine built without `RustTargetRule`.
- `RustTargetRule::triggers()` returns `["Cargo.toml"]`; a trigger-free rule
  (e.g. `NodeModulesRule`) returns an empty slice.
- The existing `discover_watch_targets_handles_cargo_target` already covers the
  post-`Cargo.toml` marking that a rescan produces; the event-loop wiring
  (`is_trigger` → `needs_rescan`) is a single thin branch validated by the
  `is_trigger` unit tests.

### Whole suite

- `cargo test` stays green; `cargo build` has no new warnings.

## Rollout

Single change set. No config, CLI, or on-disk format changes; behavior only
becomes more complete (more paths marked, faster reconciliation). Existing users
see no interface change.
