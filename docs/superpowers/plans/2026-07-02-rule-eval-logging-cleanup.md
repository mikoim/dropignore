# Rule Evaluation Logging & `apply_all` Cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the logging side effect from `RuleEngine::evaluate` (making it a pure query) and stop discarding the failure count returned by `apply_all`, as a behavior-preserving boy-scout cleanup.

**Architecture:** `evaluate` returns `Option<RuleMatch>` with no logging; a new `RuleMatch::log_matched` method centralizes the wording; the two callers (`discovery`, `app::plan_entry`) log via it and use `matched.action`. The now-unused `evaluate_action` is removed. Separately, `apply_discovered_paths` emits a rollup warning from `apply_all`'s count, and the single-path event-loop apply calls `apply_dropbox_ignore` directly instead of routing one path through `apply_all`.

**Tech Stack:** Rust 2024 edition, `log`/`env_logger`, `inotify`, `libc`, `anyhow`; tests use `tempfile`.

## Global Constraints

- No new dependencies (Cargo.toml unchanged).
- Rust edition 2024.
- Behavior-preserving except one added rollup `warn` line in `apply_discovered_paths`, emitted whenever at least one applied match fails (`failures > 0`). Log level, wording, and firing points of the existing `info!("Matched rule ...")` line must stay identical.
- All existing tests must remain green; `cargo clippy` must be clean (no `dead_code`, no unused imports) after removals.
- Follow existing style: `pub(crate)` visibility, doc comments on items, `info!`/`warn!`/`error!` macros already imported per module.

---

### Task 1: Purify `evaluate` and relocate the match log

**Files:**
- Modify: `src/rules.rs` (add `RuleMatch::log_matched`, remove `info!` from `evaluate`, remove `evaluate_action`)
- Modify: `src/discovery.rs:35` (switch call site to `evaluate` + `log_matched`)
- Modify: `src/app.rs` `plan_entry` (~lines 56-59) (switch call site to `evaluate` + `log_matched`)

**Interfaces:**
- Produces: `RuleEngine::evaluate(&self, &Candidate) -> Option<RuleMatch>` (unchanged signature, now pure/no logging); `RuleMatch::log_matched(&self, path: &Path)` (new, `pub(crate)`).
- Removes: `RuleEngine::evaluate_action(&self, &Candidate) -> Option<MatchAction>`.
- Consumes: existing `RuleMatch { name: &'static str, action: MatchAction }`, `Candidate { path: &Path, metadata: &Metadata }`.

- [ ] **Step 1: Confirm the baseline is green**

Run: `cargo test`
Expected: PASS (all existing tests).

- [ ] **Step 2: Add `RuleMatch::log_matched` and purify `evaluate`**

In `src/rules.rs`, add an `impl` block for `RuleMatch` (place it right after the `RuleMatch` struct definition, before the `Rule` trait). `log::info` and `std::path::Path` are already imported at the top of the file.

```rust
impl RuleMatch {
    /// Log that this match fired for `path`. Kept out of `RuleEngine::evaluate`
    /// so that evaluation stays a pure query and the caller controls logging.
    pub(crate) fn log_matched(&self, path: &Path) {
        info!("Matched rule '{}' for {}", self.name, path.display());
    }
}
```

Then remove the `info!(...)` call from `RuleEngine::evaluate` so it reads:

```rust
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
```

Delete the `evaluate_action` method entirely:

```rust
    // DELETE THIS METHOD:
    // pub(crate) fn evaluate_action(&self, candidate: &Candidate<'_>) -> Option<MatchAction> {
    //     self.evaluate(candidate).map(|matched| matched.action)
    // }
```

- [ ] **Step 3: Update the `discovery.rs` call site**

In `src/discovery.rs`, replace the `evaluate_action` block (currently lines 35-44) with an `evaluate` + `log_matched` block:

```rust
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
```

- [ ] **Step 4: Update the `app.rs` `plan_entry` call site**

In `src/app.rs`, `plan_entry`, replace the `evaluate_action` block (currently lines 54-59) with:

```rust
    let mut apply_ignore = false;
    let mut skip_descendants = false;
    if let Some(matched) = rules.evaluate(candidate) {
        matched.log_matched(candidate.path);
        apply_ignore = matched.action.set_dropbox_ignore;
        skip_descendants = matched.action.skip_descendants;
    }
```

- [ ] **Step 5: Verify it compiles, tests pass, and clippy is clean**

Run: `cargo test`
Expected: PASS (all existing tests, including `plan_entry_*`, `discover_watch_targets_*`, and `rules.rs` tests).

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings — in particular no `dead_code` for a leftover `evaluate_action` and no unused imports.

- [ ] **Step 6: Commit**

```bash
git add src/rules.rs src/discovery.rs src/app.rs
git commit -m "refactor(rules): make evaluate pure and move match log to callers

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: Use the `apply_all` failure count

**Files:**
- Modify: `src/app.rs` `apply_discovered_paths` (~line 219, rollup warn) and the event-loop single-path apply (~lines 149-155)

**Interfaces:**
- Consumes: `apply_all(&[PathBuf], impl FnMut(&Path) -> Result<()>) -> usize` (unchanged), `apply_dropbox_ignore(&Path, bool) -> Result<()>` (unchanged). `log::warn` is already imported in `app.rs`.
- Produces: no new public items; `apply_all`'s return value is now consumed in `apply_discovered_paths`.

- [ ] **Step 1: Add the rollup warning in `apply_discovered_paths`**

In `src/app.rs`, `apply_discovered_paths`, replace the current `apply_all(&discovered.matches, ...)` statement with a version that captures and reports the count:

```rust
    // Apply to every match, continuing past individual failures so one bad
    // path cannot terminate the watcher. Individual errors are logged by
    // apply_dropbox_ignore; this adds a single rollup summary.
    let failures = apply_all(&discovered.matches, |path| apply_dropbox_ignore(path, dry_run));
    if failures > 0 {
        warn!(
            "Failed to mark {failures} of {} discovered path(s) as ignored",
            discovered.matches.len()
        );
    }
```

- [ ] **Step 2: Simplify the single-path apply in the event loop**

In `src/app.rs`, `event_loop`, replace the `action.apply_ignore` block (currently ~lines 149-155, using `apply_all` + `std::slice::from_ref`) with a direct call:

```rust
            if action.apply_ignore {
                // Failure is already logged at error! by apply_dropbox_ignore;
                // the loop continues to the next event regardless.
                let _ = apply_dropbox_ignore(&full_path, dry_run);
            }
```

- [ ] **Step 3: Verify tests pass and clippy is clean**

Run: `cargo test`
Expected: PASS (including `apply_all_visits_every_path_despite_failures`, `rescan_is_idempotent`, `rebuild_watches_reconciles_stale_entries`).

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings — in particular `apply_all` must still be referenced (in `apply_discovered_paths` and its test), so no `dead_code`. If `std::slice` is no longer used anywhere, remove any now-unused import.

- [ ] **Step 4: Commit**

```bash
git add src/app.rs
git commit -m "refactor(app): surface apply_all failure count and simplify single-path apply

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- B (purify `evaluate`, `log_matched`, remove `evaluate_action`, update both callers) → Task 1.
- C (rollup warn from `apply_all` count; simplify single-path apply) → Task 2.
- Non-goals (no log-level change, no new rules/config/unignore) → respected; no tasks touch them.

**Placeholder scan:** No TBD/TODO/"handle edge cases"; every code step shows the actual code.

**Type consistency:** `evaluate` returns `Option<RuleMatch>` throughout; `RuleMatch::log_matched(&self, path: &Path)` used with `&dir` (a `PathBuf`, auto-derefs to `&Path`) in discovery and `candidate.path` (already `&Path`) in `app`; `apply_all` returns `usize` consumed as `failures`. Names match across tasks.
