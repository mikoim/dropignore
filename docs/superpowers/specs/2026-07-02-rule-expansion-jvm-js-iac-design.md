# Rule Expansion: JVM Build Dirs, JS Frameworks, IaC/Env Caches

Date: 2026-07-02

## Summary

Extend the rule set in two ways:

1. Generalize `RustTargetRule` into a parameterized `MarkedBuildDirRule`
   (directory name + sibling marker files), following the same consolidation
   pattern as `ArtifactDirsRule`. Instantiate it for Cargo `target`, Maven
   `target` (guarded by `pom.xml`), and Gradle `build` (guarded by Gradle
   build/settings scripts).
2. Add new exact-name artifact directories to `ArtifactDirsRule`: JavaScript
   framework build/cache dirs (`.svelte-kit`, `.astro`, `.angular`, `.vite`),
   the project-local Gradle cache (`.gradle`), IaC caches (`.terraform`,
   `.terragrunt-cache`), and dev-environment dirs (`.direnv`, `.devenv`).

All new matches use the existing `MatchAction::IGNORE_AND_SKIP` behavior. No
new runtime state, error paths, or dependencies; the discovery walk, event
loop, and trigger-rescan machinery are reused unchanged.

## Motivation

The 2026-07-02 safe-artifacts expansion deliberately excluded generic names
that need an adjacency guard (`build`, and grouped `.gradle` with them) because
no guarded-rule abstraction existed beyond the Rust-specific `RustTargetRule`.
Generalizing that rule removes the blocker: Maven and Gradle build outputs are
among the largest sync-churn sources in JVM projects and can now be matched
safely. The unconditional additions extend coverage to common JavaScript
framework caches and IaC/dev-environment caches, all tool-owned and
reproducible.

## Design

### 1. Generalize the guarded rule: `MarkedBuildDirRule` (`src/rules.rs`)

Replace `RustTargetRule` with:

```rust
pub(crate) struct MarkedBuildDirRule {
    name: &'static str,                 // logging name
    dir: &'static str,                  // exact directory name to match
    markers: &'static [&'static str],   // any one must exist in the parent
}
```

Associated constants:

| Constant | `dir` | `markers` | `name` |
| --- | --- | --- | --- |
| `CARGO_TARGET` | `target` | `Cargo.toml` | "Cargo target directory" |
| `MAVEN_TARGET` | `target` | `pom.xml` | "Maven target directory" |
| `GRADLE_BUILD` | `build` | `build.gradle`, `build.gradle.kts`, `settings.gradle`, `settings.gradle.kts` | "Gradle build directory" |

- `matches()`: `candidate.is_dir_named(self.dir)` and at least one marker
  exists in the candidate's parent directory (checked in slice order; the name
  check runs first to avoid filesystem calls, as today).
- `action()`: `MatchAction::IGNORE_AND_SKIP`.
- `triggers()`: returns `self.markers`. The existing scoped-rescan machinery
  (`RuleEngine::is_trigger` + `rescan_scopes` in `drain_events`) then handles
  a marker file created after its build directory with no changes. Markers
  are siblings of the matched directory, so the trigger scope invariant
  documented on `Rule::triggers` continues to hold.

`CARGO_TARGET` preserves `RustTargetRule`'s exact behavior; the existing tests
carry over as its regression suite.

### 2. Extend `ArtifactDirsRule` (`src/rules.rs`)

Append to the existing JS list and add three new instances:

- `JS_BUILD` (existing list grows): `.svelte-kit` (SvelteKit output),
  `.astro` (Astro generated types), `.angular` (Angular CLI cache),
  `.vite` (Vite cache when configured at project root).
- `JVM_CACHES` (new): `.gradle` — the project-local Gradle cache. Matched
  unconditionally: a directory with this exact name is Gradle-owned in
  practice, and marking is non-destructive (sync exclusion only). The one
  theoretical exception — a user home directory synced into Dropbox, where
  `~/.gradle` holds user-edited `gradle.properties` — is outside the tool's
  expected use of watching a Dropbox root.
- `IAC_CACHES` (new): `.terraform` (provider/module cache, recreated by
  `terraform init`), `.terragrunt-cache`.
- `DEV_ENV_DIRS` (new): `.direnv`, `.devenv`.

### 3. Registration (`src/app.rs`)

In `RuleEngine::new`, replace `RustTargetRule` with the three
`MarkedBuildDirRule` constants and append the three new `ArtifactDirsRule`
constants (11 rules total). Evaluation is first-match; `CARGO_TARGET` and
`MAVEN_TARGET` both match a `target` directory when both markers exist, but
their actions are identical, so ordering does not affect behavior.

### 4. Documentation (`README.md`)

- Update the Features rule enumeration with the new names.
- In "Extending rules", replace the `RustTargetRule` template reference:
  a new guarded rule is now a `MarkedBuildDirRule` constant; a new exact-name
  rule is an `ArtifactDirsRule` constant.

## Data flow and error handling

No change. Pre-existing matches are marked and skipped by
`discover_watch_targets`; newly created ones by `plan_entry`; marker files
created after the fact are reconciled by the existing trigger-driven
`rescan_subtree`. Every new rule uses `IGNORE_AND_SKIP`, so matched
directories are never watched. No new failure modes.

## Testing (TDD)

- `src/rules.rs`:
  - Port the existing `RustTargetRule` tests to `MarkedBuildDirRule::CARGO_TARGET`
    unchanged in substance (match with `Cargo.toml`, trigger declaration).
  - `MAVEN_TARGET`: `target` matches with a sibling `pom.xml`; does not match
    without it.
  - `GRADLE_BUILD`: `build` matches with each of the four marker names
    (table-driven); does not match with no marker present.
  - Trigger registration: `RuleEngine::is_trigger` recognizes `pom.xml` and
    `build.gradle.kts`; an engine without JVM rules does not.
  - Extend `artifact_dirs_rule_instances_match_their_directories` with the
    three new instances (`.gradle`, `.terraform`, `.direnv`) and a new
    `JS_BUILD` name (`.svelte-kit`).
- `src/app.rs`:
  - Integration: an existing unmatched `build` directory starts matching after
    `build.gradle` is written and `rescan_subtree` runs (mirrors the existing
    `rescan_subtree_reconciles_newly_matched_sibling` Cargo test).
- All existing tests must continue to pass; the `RustTargetRule` ports prove
  behavior equivalence of the generalization.

## Non-goals

- Un-ignore (attribute removal) and config-file/CLI-driven rules.
- Ambiguous unguarded names (`dist`, bare `build` without markers).
- Ecosystems not selected for this round (Zig, Dart, Elixir, Haskell) — the
  same two patterns cover them later.
