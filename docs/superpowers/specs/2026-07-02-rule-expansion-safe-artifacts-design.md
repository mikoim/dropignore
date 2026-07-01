# Rule Expansion: Safe Reproducible-Artifact Directories

Date: 2026-07-02

## Summary

Extend the rule set with additional directory names that are unambiguously
reproducible build or cache artifacts, so common Python and JavaScript project
debris is marked with the Dropbox ignore attribute out of the box. All new
names are matched by exact directory name and use the existing
`MatchAction::IGNORE_AND_SKIP` behavior (mark the directory, do not descend).

This is a scoped, additive change: no new runtime state, error paths, or
dependencies. It reuses the existing discovery walk and event-loop code
unchanged.

## Motivation

The tool currently covers `node_modules`, `.pnpm-store`, Cargo `target`
(guarded by an adjacent `Cargo.toml`), Python virtualenvs (`.venv`/`venv`), and
`*.egg-info`. Several equally reproducible directories are left un-marked,
notably Python tool caches and JavaScript framework build/cache directories.
Marking them reduces Dropbox sync churn with no risk of hiding source, because
every added name is a well-known, tool-owned artifact directory.

## Scope

### In scope: safe layer only

Names included are limited to fixed, tool-owned directory names that never
contain user source. Directory-name conventions were verified against upstream
docs (Ruff's default `exclude` list cross-checks the Python names; Turborepo
docs confirm `.turbo`).

New Python cache directories (folded into the existing
`PythonBuildArtifactsRule`):

- `__pycache__`
- `.pytest_cache`
- `.mypy_cache`
- `.ruff_cache`
- `.tox`

New JavaScript build/cache directories (new `JsBuildArtifactsRule`):

- `.next`
- `.nuxt`
- `.turbo`
- `.parcel-cache`

### Out of scope (YAGNI)

- Generic, ambiguous names that can hold source and would need an adjacency
  guard: `dist`, `build`, `.gradle`.
- Sibling names not in the approved set (e.g. `.nox`, `.svelte-kit`,
  `.ipynb_checkpoints`). Easy to add later using the same pattern.
- Config-file-driven rules and un-ignore (attribute removal). Tracked
  separately; not part of this change.

## Design

Follows the established "one rule struct per concept" pattern documented in the
README's "Extending rules" section.

### 1. Extend `PythonBuildArtifactsRule` (`src/rules.rs`)

Add the five Python cache directory names to the existing rule, which already
carries the name "Python build/cache artifact" and already matches `.venv`,
`venv`, and `*.egg-info`. Extract the directory-name set into a module-level
`const` slice for readability, and match a directory whose name is in that set.
`*.egg-info` suffix matching (file or directory) is unchanged.

### 2. Add `JsBuildArtifactsRule` (`src/rules.rs`)

New struct modeled on `NodeModulesRule`. Matches a directory whose name is in a
`const` slice of the four JavaScript names. Returns `MatchAction::IGNORE_AND_SKIP`.
Declares no `triggers()` (name-only match, no dependency file).
`node_modules` and `.pnpm-store` keep their existing dedicated rules to avoid
churn.

### 3. Register the new rule (`src/app.rs`)

Add `JsBuildArtifactsRule` to the `RuleEngine::new` vector. All new names are
disjoint from existing ones, so evaluation order is irrelevant to correctness.

### 4. Update documentation (`README.md`)

Update the Features rule enumeration to reflect the added Python caches and the
new JavaScript build/cache directories.

## Data flow and error handling

No change. The initial scan (`discover_watch_targets`) marks pre-existing
matching directories and skips their subtrees; the event loop (`plan_entry`)
marks newly created ones. Because every new name uses `IGNORE_AND_SKIP`, matched
directories are never watched, so their descendants add no watches. No new
failure modes are introduced.

## Testing (TDD)

- `src/rules.rs`: assert each new name matches with `set_dropbox_ignore` and
  `skip_descendants` true — cover representative Python names (`__pycache__`,
  `.pytest_cache`, `.tox`) and JS names (`.next`, `.turbo`, `.parcel-cache`).
  Negative case: an ordinary directory (e.g. `src`) does not match.
- `src/discovery.rs`: a `__pycache__` directory is added to `matches` and its
  descendants are not added to `watchers` (mirrors the existing
  `discover_watch_targets_skips_ignored_subtrees` test).
- Existing tests remain unchanged and must continue to pass.

## Non-goals

Attribute removal, config-driven rules, and guarded generic names are explicitly
excluded from this change.
