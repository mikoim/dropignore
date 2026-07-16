# Extract the Production Ruleset into a Testable `default_rules()`

Date: 2026-07-16
Status: Approved

## Goal

Move the production ruleset out of the untested `run()` into a pure function
`default_rules()` in `src/rules.rs`, and add a table-driven test over the
resulting engine so a dropped registration, a mis-wired action, or a broken
ordering assumption is caught by the test suite instead of shipping.

## Background

The 2026-07-16 codebase survey found the tool healthy and thoroughly tested,
but surfaced one maintainability and coverage gap:

- The production ruleset — 18 `Box::new(...)` entries — is defined inline in
  `RuleEngine::new(vec![...])` inside `app.rs::run()`. `run()` is
  deliberately outside test scope (it opens a real inotify fd and blocks), so
  **no test exercises the production set**. Nothing guarantees that every
  rule is registered, that each maps to the intended `MatchAction`, or that
  `VCS_DIRS` stays ahead of rules that could otherwise mark a repository.
- Adding a rule requires editing two files (`src/rules.rs` for the constant,
  `src/app.rs` for registration). The README's "Extending rules" section
  documents this friction explicitly.

Extracting a named constructor gives the production set a single home next to
the rules it lists and a directly testable value.

## Non-Goals

- Any change to matching behavior, rule order, or the set of rules. This is a
  pure refactor plus tests; the produced engine is identical to today's.
- Introspection API on `RuleEngine` (e.g. iterating registered rule names).
  A behavioral table over representative fixtures covers the same ground
  without widening the public surface.
- unmark, rule customization (`--ignore-dir`), and multi-root watching
  (remain in the backlog).

## Design

### 1. `default_rules()` in `src/rules.rs`

- New `pub(crate) fn default_rules() -> Vec<Box<dyn Rule>>` holding the exact
  18-entry vector currently in `app.rs::run()`, in the same order.
- `RuleEngine::new(rules)` stays generic so tests keep building subset
  engines. The production path composes as `RuleEngine::new(default_rules())`.
  A `Default` impl was rejected: `new(rules)` is the single construction path
  today and a free function reads more clearly than a `Default` that
  constructs the full engine.

### 2. `app.rs::run()` uses it

- Replace the inline `RuleEngine::new(vec![...])` with
  `RuleEngine::new(default_rules())`.
- Trim `run()`'s now-unused rule-type imports. `app.rs` tests still reference
  `ArtifactDirsRule`, `EggInfoRule`, and `MarkedBuildDirRule`, so those stay
  imported where the tests need them; only imports that become unused after
  the edit are removed (clippy `-D warnings` enforces this).

### 3. Table-driven test in `src/rules.rs`

Build `RuleEngine::new(default_rules())` once, then assert one representative
fixture per rule category (each under its own `TempDir`):

| Fixture | Expected |
|---|---|
| `node_modules/` dir | match, `IGNORE_AND_SKIP` |
| `target/` + sibling `Cargo.toml` | match (`IGNORE_AND_SKIP`) |
| `target/` with no marker | no match |
| `.venv/` dir | match, `IGNORE_AND_SKIP` |
| `pkg.egg-info` file | match, `IGNORE_AND_SKIP` |
| `.git/` dir | match, `SKIP_ONLY` (never marked) |
| a plain `src/` dir | no match |

- These assertions catch a **dropped registration** (the rule stops
  matching), a **wrong action** (e.g. `.git` marked instead of skip-only),
  and the **VCS-first ordering intent** (`.git` resolves to `SKIP_ONLY`).
- Add a cheap tripwire: `assert_eq!(default_rules().len(), 18)` so an
  accidental duplicate or removal is flagged with a clear message. The
  constant is updated deliberately whenever a rule is added.

### 4. README update

The "Extending rules" section currently reads "Register new constants in
`RuleEngine::new` in `src/app.rs`." Update it to point at `default_rules()`
in `src/rules.rs`, since that is the new registration site.

## Implementation order

1 (`default_rules()`) → 2 (`run()` switch) → 3 (test) → 4 (README).
`./scripts/check.sh` gates every step; the switch in step 2 must keep the
existing suite green before the new test is added.

## Testing

- New table-driven test over `default_rules()` (fixtures per the table
  above) plus the `len() == 18` tripwire, both in `src/rules.rs`.
- Whole change: `./scripts/check.sh` — `cargo fmt --check`,
  `cargo clippy --all-targets` (zero warnings), `cargo test`. No new
  dependencies, no public-API change, no inotify/xattr/`/proc` interaction in
  the added test (temp-dir rule evaluation only).
