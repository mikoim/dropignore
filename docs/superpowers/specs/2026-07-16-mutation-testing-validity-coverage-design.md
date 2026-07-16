# Prove Test Validity with Containerized Mutation Testing and Close Coverage Gaps

Date: 2026-07-16
Status: Approved

## Goal

Prove that the test suite actually detects bugs — not just that it executes
lines — by running cargo-mutants over the whole crate inside a Podman
container, fixing every gap it finds, and closing the remaining testable
llvm-cov gaps (chiefly the `dropbox.rs` error paths). Ship a permanent,
containerized entry point (`scripts/mutants.sh`) so the check can be repeated
after future rule additions or refactors.

## Background

The suite stands at 74 tests and 95.1% line coverage, but line coverage only
records that code ran under a test, not that any assertion would fail if the
code were wrong. Mutation testing closes that gap: cargo-mutants injects
small plausible bugs ("mutants") and reports every mutant the suite fails to
kill.

A 2026-07-16 feasibility run verified the whole approach inside a
`rust:1.96-slim` container with the repository mounted read-only:

- All 74 tests pass in the container (3.08s). The xattr-guarded tests do
  **not** hit their skip guard — `user.*` xattrs work on the container
  filesystem — and the inotify-driven tests run normally.
- cargo-mutants v27.1.0 completed a full cycle on `src/dropbox.rs`:
  9 mutants, baseline ok, auto-set 20s timeout, 7 caught / 2 missed in 33s,
  results written through `-o` to a writable mount, exit code 2 (= missed
  mutants exist).
- The two survivors are real findings, proving the method's value up front:
  - `&& → ||` in `is_already_ignored` (src/dropbox.rs:32) survives because
    no test covers "attribute present with an unexpected value must be
    rewritten".
  - `+ → *` in the buffer size (src/dropbox.rs:20) is a suspected
    equivalent mutant (a 1-byte and 2-byte read buffer both classify a
    longer stored value as "not marked", via length mismatch vs `ERANGE`);
    to be confirmed during triage and excluded with a rationale if so.
- `src/cli.rs` contains no mutable functions (clap derive only) — expected,
  not a problem.

Isolation matters here: mutated code is untrusted by construction (upstream
docs recommend sandboxing), and this suite writes to the filesystem and sets
xattrs. The user requires that mutation runs happen inside Podman; regular
`cargo test` / `./scripts/check.sh` stay on the host unchanged.

## Non-Goals

- No mutation gate in `check.sh` or the pre-commit hook. Mutation runs cost
  minutes, not seconds; `scripts/mutants.sh` is a manual, occasional gate.
- No `#[mutants::skip]` attributes and no new entries in `Cargo.toml`
  (the attribute would drag in the `mutants` crate; all exclusions live in
  `.cargo/mutants.toml`). cargo-mutants itself exists only inside the
  container image.
- No production code changes, unless a mutant exposes a genuine bug — in
  that case the finding is reported before any fix.
- No CI wiring, sharding, or coverage-threshold automation.
- unmark, rule customization, and multi-root watching stay in the backlog.

## Design

### 1. Container image: `scripts/mutants.Containerfile`

- `FROM docker.io/library/rust:1.96-slim` (matches the host toolchain that
  the crate is developed against).
- `RUN cargo install cargo-mutants --version 27.1.0 --locked` — pinned so
  runs are reproducible; the ~3-minute install cost is paid once and cached
  as an image layer.

### 2. Entry point: `scripts/mutants.sh`

- Builds the image as `localhost/dropignore-mutants` via
  `podman build -f scripts/mutants.Containerfile` (cheap no-op when layers
  are cached), then runs:
  - repository mounted read-only at `/src` — mutated code cannot touch the
    working tree even in principle (cargo-mutants also copies the source,
    so this is defense in depth);
  - a named volume for the cargo registry
    (`dropignore-mutants-cargo:/usr/local/cargo/registry`) so dependencies
    are not re-downloaded every run;
  - results directed to a host-mounted output directory with
    `-o` (`mutants.out/` in the repo root, added to `.gitignore`);
  - extra CLI arguments passed through (`"$@"`), so
    `scripts/mutants.sh -f src/rules.rs` narrows a run.
- Exits with cargo-mutants' own exit code and documents the meaning in a
  comment (0 = clean, 2 = missed mutants, 3 = timeouts, 4 = unviable
  baseline).
- Fails early with a clear message if `podman` is not installed.

### 3. Config: `.cargo/mutants.toml`

Committed, so every developer runs with the same scope:

```toml
# main() and run() wire up the real process (inotify fd, signal handlers,
# blocking loop) and are deliberately outside unit-test scope.
exclude_globs = ["src/main.rs"]
exclude_re = ["app::run"]
```

Survivor triage may append `exclude_re` entries for proven-equivalent
mutants only, each with a comment stating why the mutant is untestable.

### 4. Survivor triage

Run the full sweep, then classify every missed mutant:

- **Real test gap** → add a test TDD-style: confirm it kills the mutant
  (red against mutated logic, green against real code). The known
  `&& → ||` survivor gets a test that pre-sets the xattr to `b"0"` and
  asserts `apply_dropbox_ignore` rewrites it to `b"1"`.
- **Equivalent mutant** (observable behavior identical) → `exclude_re`
  entry with rationale. The known `+ → *` buffer-size survivor is the
  first candidate; its equivalence argument is written down either way.

The loop repeats until a full run reports zero missed mutants.

### 5. Coverage gap closure (independent of mutation results)

- `src/dropbox.rs` setxattr failure path: apply to a path inside a
  just-deleted temp directory and assert the error context.
- `src/dropbox.rs` interior-NUL path: build a `PathBuf` from bytes
  containing `\0` (Unix `OsString`) and assert the NUL error, no
  filesystem needed.
- `src/discovery.rs` (13 uncovered lines): inspect; add tests where the
  branch is reachable in a test, otherwise record why not (expected:
  warn-only branches).
- `run()` / `main.rs` / EINTR arm stay out of scope as previously decided.

### 6. README

New "Verifying the tests (mutation testing)" section: what
`scripts/mutants.sh` does, that it requires Podman, exit-code meaning,
and when to run it (after adding rules or refactoring match/apply logic).

## Implementation order

1. `scripts/mutants.Containerfile` + `scripts/mutants.sh` + `.gitignore`
2. `.cargo/mutants.toml`
3. Full containerized sweep → survivor list
4. Triage: tests for real gaps, justified exclusions for equivalent mutants
5. Coverage gap tests (`dropbox.rs`, `discovery.rs` verdict)
6. README section
7. Final verification: full `scripts/mutants.sh` run exits 0,
   `./scripts/check.sh` green on the host, `cargo llvm-cov` shows no
   regression and improved `dropbox.rs` coverage

## Testing

- The deliverable *is* test verification: success means a clean mutation
  run (every mutant killed or excluded with a written rationale) plus the
  existing quality gate (`fmt --check`, `clippy -D warnings`, `cargo test`)
  staying green on the host.
- Coverage is re-measured with `cargo llvm-cov --summary-only`; line
  coverage must not drop below the current 95.1%, and `dropbox.rs` function
  coverage must rise from 60%.
- New tests follow the house style: `TempDir` fixtures, `Result`-returning
  tests, xattr tests guarded by `test_util::xattr_supported`.
