# Rule Evaluation Logging & `apply_all` Cleanup — Design

Date: 2026-07-02

## Goal

A focused, behavior-preserving internal cleanup (boy-scout pass) with two
threads:

- **B.** Remove the logging side effect from `RuleEngine::evaluate` so the
  match-evaluation query is pure, and emit the same log from the callers.
- **C.** Stop discarding the failure count returned by `apply_all`; surface it
  as a rollup warning where multiple paths are applied.

No new dependencies. No change to public/observable behavior except one added
rollup `warn` line (see C), which was explicitly requested.

## Background

`RuleEngine::evaluate` (`src/rules.rs`) currently logs
`info!("Matched rule '{}' for {}")` *inside* the match loop. This method is
reached from two places:

- `discovery::discover_watch_targets` — a read-only tree walk, via
  `evaluate_action`.
- `app::plan_entry` — the runtime event loop, via `evaluate_action`.

So a nominally read-only query has an `info!` side effect, the logging concern
is tangled with matching, and the wording lives inside the engine rather than
with the code that acts on the match.

`apply_all` (`src/app.rs`) returns a failure count, but every caller discards
it: the single-path apply in the event loop and the multi-path apply in
`apply_discovered_paths`.

## Design

### B. Pure `evaluate` + caller-side logging

1. `RuleEngine::evaluate` becomes pure: it returns `Option<RuleMatch>` with the
   `info!` call removed.
2. Add a small method to keep the log wording in one place:

   ```rust
   impl RuleMatch {
       pub(crate) fn log_matched(&self, path: &Path) {
           info!("Matched rule '{}' for {}", self.name, path.display());
       }
   }
   ```

   (`log::info` and `std::path::Path` are already in scope in `rules.rs`.)
3. Switch both callers from `evaluate_action` to `evaluate`, logging via
   `log_matched` before using `matched.action`:
   - `discovery.rs`: `if let Some(matched) = rules.evaluate(&candidate) { matched.log_matched(&dir); let action = matched.action; ... }`
   - `app.rs::plan_entry`: same shape, logging `candidate.path`.
4. `RuleEngine::evaluate_action` is now unused → **remove it**.

**Behavior:** the `info!("Matched rule ...")` line fires at exactly the same
points, at the same level, with the same text. Only the *source* of the call
moves from the engine to the callers. The side effect leaves the query.

**Rejected alternative:** inline `info!` at both call sites without a helper —
duplicates the wording; rejected to keep the message DRY.

### C. Use the `apply_all` failure count

1. In `apply_discovered_paths` (`src/app.rs`), capture the return value and emit
   a rollup warning when any path failed:

   ```rust
   let failures = apply_all(&discovered.matches, |p| apply_dropbox_ignore(p, dry_run));
   if failures > 0 {
       warn!(
           "Failed to mark {failures} of {} discovered path(s) as ignored",
           discovered.matches.len()
       );
   }
   ```

   (`log::warn` is already imported in `app.rs`.) Individual failures are still
   logged at `error!` by `apply_dropbox_ignore`; this adds a single summary
   line.
2. The single-path apply in the event loop drops `apply_all` +
   `std::slice::from_ref` + closure in favor of a direct call, since routing one
   path through the multi-path helper is needless indirection:

   ```rust
   if action.apply_ignore {
       // Failure is already logged at error! by apply_dropbox_ignore; the loop
       // continues to the next event regardless.
       let _ = apply_dropbox_ignore(&full_path, dry_run);
   }
   ```

   After this, `apply_all` is used only where it earns its keep (multi-path
   apply) plus its unit test, and its return value is no longer dead.

**Behavior:** the only added observable output is the rollup `warn`, emitted
whenever at least one applied match fails (`failures > 0`).

## Scope / impact

- `src/rules.rs` — purify `evaluate`, add `RuleMatch::log_matched`, remove
  `evaluate_action`.
- `src/discovery.rs` — one call site switched to `evaluate` + `log_matched`.
- `src/app.rs` — `plan_entry` switched to `evaluate` + `log_matched`; two
  `apply` call sites reworked (rollup warn; direct single-path call).

No signature change to `evaluate` or `apply_all`, so existing tests stay green.

## Testing

- Existing unit tests in `rules.rs`, `discovery.rs`, `app.rs`, `watch.rs`
  continue to pass unchanged (they exercise `evaluate`, `apply_all`,
  `plan_entry`, and discovery behavior).
- `cargo test` and `cargo clippy` must be clean; in particular no
  `dead_code`/unused-import warnings after removing `evaluate_action`.
- Optional: a small unit test asserting `apply_all` returns the failure count is
  already present (`apply_all_visits_every_path_despite_failures`); no new test
  is strictly required for this cleanup, but a caller-level assertion is not
  feasible without capturing logs and is out of scope.

## Non-goals

- No change to log levels or reduction of per-match logging on large rescans
  (that would alter observable behavior; deliberately excluded).
- No new rules, config-driven rules, unignore mode, or robustness edge-case work
  (tracked separately as candidates A/D/E).
