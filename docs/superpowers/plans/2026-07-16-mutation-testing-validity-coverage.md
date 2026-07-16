# Mutation-Testing Validity & Coverage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove the test suite detects real bugs by running cargo-mutants in a Podman container until zero mutants survive, and close the testable llvm-cov gaps in `dropbox.rs` and `discovery.rs`.

**Architecture:** A pinned container image (`rust:1.96-slim` + cargo-mutants v27.1.0) is built from `scripts/mutants.Containerfile` and driven by `scripts/mutants.sh`, which mounts the repo read-only and collects results in a gitignored `mutants.out/`. Exclusions live only in `.cargo/mutants.toml`, each with a rationale comment. Survivors are fixed with TDD-added tests or excluded as proven-equivalent.

**Tech Stack:** Rust 2024 (rustc 1.96), cargo-mutants 27.1.0, Podman 5.x, cargo-llvm-cov (host, already installed), bash.

**Spec:** `docs/superpowers/specs/2026-07-16-mutation-testing-validity-coverage-design.md`

## Global Constraints

- No new entries in `Cargo.toml` (`[dependencies]` or `[dev-dependencies]`); no `#[mutants::skip]` attributes anywhere.
- Mutation runs happen **only** inside Podman via `scripts/mutants.sh`; plain `cargo test` / `./scripts/check.sh` stay on the host.
- Mutation exclusions live only in `.cargo/mutants.toml`, each preceded by a comment stating why.
- No production code changes unless a mutant exposes a genuine bug — report the finding before fixing.
- Every commit passes the pre-commit gate (`./scripts/check.sh`: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`). Commit messages in English, imperative, `type: subject` style (see `git log`).
- New tests follow house style: `Result<()>`-returning, `TempDir` fixtures, xattr tests guarded by `test_util::xattr_supported`, assertion messages explain the invariant.

## Feasibility facts (verified 2026-07-16, use as reference)

- In-container: all 74 tests pass; xattr works in container `/tmp` (skip guards do NOT trigger); inotify works.
- `cargo mutants -f src/dropbox.rs` in-container: 9 mutants, 7 caught, 2 missed:
  - `src/dropbox.rs:32: replace && with || in is_already_ignored` — real test gap.
  - `src/dropbox.rs:20: replace + with * in is_already_ignored` — suspected equivalent mutant.
- `cargo mutants` exit codes: 0 = clean, 2 = missed mutants, 3 = timeouts, 4 = baseline failing.
- `-o DIR` writes results to `DIR/mutants.out/`; cargo-mutants rotates a previous run to `DIR/mutants.out.old/` (rename must stay inside one mount, hence mounting the parent of the result dir).
- Mutant names in lists/config regexes look like `src/watch.rs:20:5: replace watch_error_context -> String with String::new()` — free functions appear WITHOUT a module prefix, so the spec's shorthand "exclude app::run" must be expressed as a regex on the mutant name (see Task 2).
- `src/cli.rs` has no mutable functions (clap derive only).

## Known uncovered lines (from `cargo llvm-cov --show-missing-lines`, 2026-07-16)

| Location | What it is | Verdict |
|---|---|---|
| `src/dropbox.rs:69-75` | setxattr failure path | testable (Task 4) |
| `src/dropbox.rs:96-97` | a test's own xattr skip guard | not production code, leave |
| `src/discovery.rs:81-85` | symlink skip in `walk` | testable (Task 5) |
| `src/discovery.rs:57-59` | `read_dir` failure warn+continue | testable via 0o000 dir (Task 5) |
| `src/discovery.rs:66-68, 74-76` | per-entry / file_type race errors | untestable deterministically, document (Task 5) |
| `src/discovery.rs:48` | region artifact on closing brace | leave |
| `src/watch.rs:49-50` | ENOSPC arm in `add_watch` | needs real watch exhaustion, document (Task 5) |
| `src/watch.rs:91` | region artifact on closing brace | leave |
| `src/rules.rs:235` | defensive `parent() == None` arm (unreachable: a path with a file name always has a parent) | document (Task 5) |
| `src/rules.rs:781` | assert format arg in a test | leave |
| `src/main.rs`, `app::run` | process wiring | out of scope per spec |

---

### Task 1: Containerized mutation harness

**Files:**
- Create: `scripts/mutants.Containerfile`
- Create: `scripts/mutants.sh` (executable)
- Modify: `.gitignore` (repo root)

**Interfaces:**
- Produces: `./scripts/mutants.sh [cargo-mutants args...]` — later tasks invoke it as `./scripts/mutants.sh --list`, `./scripts/mutants.sh -f src/dropbox.rs`, `./scripts/mutants.sh` (full sweep). Results land in `mutants.out/mutants.out/` (`missed.txt`, `caught.txt`, `outcomes.json`). Exit code = cargo-mutants exit code.

- [ ] **Step 1: Write `scripts/mutants.Containerfile`**

```dockerfile
# Image for containerized mutation testing (see scripts/mutants.sh).
# rustc matches the host toolchain; cargo-mutants is pinned so runs are
# reproducible. Both are baked into a cached layer because
# `cargo install cargo-mutants` costs minutes.
FROM docker.io/library/rust:1.96-slim
RUN cargo install cargo-mutants --version 27.1.0 --locked
```

- [ ] **Step 2: Write `scripts/mutants.sh`**

```bash
#!/usr/bin/env bash
# Mutation testing in an isolated Podman container. Mutated code is
# untrusted by construction, so the repository is mounted read-only and
# all builds/tests run on a container-local copy; only mutants.out/
# (gitignored) receives results.
#
# Slow (minutes for a full sweep) — run manually after adding rules or
# refactoring match/apply logic. Not part of check.sh or pre-commit.
#
# Usage: scripts/mutants.sh [cargo-mutants args...]
#   e.g. scripts/mutants.sh --list
#        scripts/mutants.sh -f src/dropbox.rs
# Exit codes (cargo-mutants): 0 clean, 2 missed mutants, 3 timeouts,
# 4 baseline tests already failing.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

if ! command -v podman >/dev/null; then
    echo "error: podman is required for isolated mutation runs" >&2
    exit 1
fi

image=localhost/dropignore-mutants
podman build --quiet -f scripts/mutants.Containerfile -t "$image" scripts >/dev/null

# mutants.out/ is the mount; results appear in mutants.out/mutants.out/ so
# cargo-mutants can rotate the previous run without crossing the mount.
mkdir -p mutants.out
exec podman run --rm \
    --volume "$PWD":/src:ro \
    --volume "$PWD/mutants.out":/out \
    --volume dropignore-mutants-cargo:/usr/local/cargo/registry \
    --workdir /src \
    "$image" cargo mutants -o /out "$@"
```

- [ ] **Step 3: Make it executable and ignore the output directory**

```bash
chmod +x scripts/mutants.sh
```

Append to `.gitignore` (after the existing `target/` line):

```gitignore
mutants.out/
```

- [ ] **Step 4: Verify the harness end-to-end with `--list`**

Run: `./scripts/mutants.sh --list | head -5` and `./scripts/mutants.sh --list | wc -l`

Expected: first invocation builds the image (~3-5 min, cached afterwards), then prints mutant lines like `src/rules.rs:87:9: replace Rule::triggers -> ...`; the count is >150 (whole crate, no config yet). Must NOT error on the read-only mount.

- [ ] **Step 5: Commit**

```bash
git add scripts/mutants.Containerfile scripts/mutants.sh .gitignore
git commit -m "build: add containerized cargo-mutants harness"
```

---

### Task 2: Mutation scope config

**Files:**
- Create: `.cargo/mutants.toml`

**Interfaces:**
- Consumes: `./scripts/mutants.sh --list` from Task 1.
- Produces: committed `.cargo/mutants.toml` with `exclude_globs` / `exclude_re` lists that ALL later mutation runs pick up automatically. Later tasks append `exclude_re` entries here (rationale comment above each entry).

- [ ] **Step 1: Write `.cargo/mutants.toml`**

```toml
# Mutation-testing scope (cargo-mutants, run via scripts/mutants.sh).
# Every exclusion needs a comment saying why it is out of scope.

# main() only wires env_logger + CLI parsing into run(); run() opens the
# real inotify fd, installs signal handlers, and blocks — deliberately
# outside unit-test scope (see docs/superpowers/specs/2026-07-16-*.md).
exclude_globs = ["src/main.rs"]

exclude_re = [
    # app.rs run(): see above. Mutant names carry no module prefix for
    # free functions, so match on file + function.
    "src/app\\.rs.*(replace|delete).* in run($| )",
    "src/app\\.rs.* replace run ",
]
```

- [ ] **Step 2: Verify the exclusions bite (and nothing else)**

Run: `./scripts/mutants.sh --list > /tmp/mutants-list.txt; grep -c "" /tmp/mutants-list.txt; grep -E "src/main\.rs| run |in run" /tmp/mutants-list.txt || echo NO-RUN-MUTANTS`

Expected: `NO-RUN-MUTANTS`; no `src/main.rs` lines; total count only slightly lower than Task 1's count. Spot-check that unrelated functions (e.g. `rescan_subtree`, `drain_events`) still appear in the list — the `run` regex must not swallow them. Adjust the regex until both conditions hold, then record the final regex in this file.

- [ ] **Step 3: Commit**

```bash
git add .cargo/mutants.toml
git commit -m "build: scope mutation runs to unit-testable code"
```

---

### Task 3: Kill the known dropbox.rs survivors

**Files:**
- Modify: `src/dropbox.rs` (tests module only)
- Modify: `.cargo/mutants.toml` (one equivalent-mutant exclusion)

**Interfaces:**
- Consumes: `./scripts/mutants.sh -f src/dropbox.rs` from Task 1; `.cargo/mutants.toml` from Task 2.
- Produces: test helper `fn read_attr(c_path: &CString, c_name: &CString) -> Vec<u8>` in `dropbox::tests` (reused by Task 4 if convenient); a mutation-clean `src/dropbox.rs`.

- [ ] **Step 1: Reproduce the survivors (red)**

Run: `./scripts/mutants.sh -f src/dropbox.rs; echo "exit=$?"`

Expected: `exit=2` with exactly these two in `mutants.out/mutants.out/missed.txt`:
`replace && with || in is_already_ignored` and `replace + with * in is_already_ignored`.

- [ ] **Step 2: Add a raw-readback helper and the wrong-value test**

In `src/dropbox.rs` `mod tests`, add. IMPORTANT: the final assertion must read the attribute bytes back RAW — asserting via `is_already_ignored` would use the mutated function itself and the `&&→||` mutant would sail through:

```rust
    /// Read the attribute bytes back without going through
    /// `is_already_ignored`, so tests can observe what is actually stored.
    fn read_attr(c_path: &CString, c_name: &CString) -> Vec<u8> {
        let mut buf = [0u8; 16];
        // SAFETY: pointers are valid for the call; size matches the buffer.
        let len = unsafe {
            libc::getxattr(
                c_path.as_ptr(),
                c_name.as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        assert!(len >= 0, "getxattr must succeed for a stored attribute");
        buf[..len as usize].to_vec()
    }

    #[test]
    fn wrong_attribute_value_is_rewritten() -> Result<()> {
        let temp = TempDir::new()?;
        let file = temp.path().join("artifact");
        fs::write(&file, b"")?;
        if !xattr_supported(temp.path()) {
            eprintln!("skipping: filesystem lacks user.* xattr support");
            return Ok(());
        }

        let c_path = CString::new(file.as_os_str().as_bytes())?;
        let c_name = CString::new(DROPBOX_IGNORE_ATTR)?;

        // Pre-set the attribute to a same-length wrong value: the path must
        // NOT count as marked, and apply must rewrite it to "1".
        let wrong = b"0";
        // SAFETY: pointers are valid for the call; sizes are correct.
        let rc = unsafe {
            libc::setxattr(
                c_path.as_ptr(),
                c_name.as_ptr(),
                wrong.as_ptr().cast(),
                wrong.len(),
                0,
            )
        };
        assert_eq!(rc, 0, "test setup: presetting the attribute must succeed");

        apply_dropbox_ignore(&file, false)?;
        assert_eq!(
            read_attr(&c_path, &c_name),
            DROPBOX_IGNORE_VALUE,
            "a wrong stored value must be overwritten with \"1\""
        );

        // A longer stored value must also be rewritten (length mismatch path).
        let long = b"10";
        // SAFETY: as above.
        let rc = unsafe {
            libc::setxattr(
                c_path.as_ptr(),
                c_name.as_ptr(),
                long.as_ptr().cast(),
                long.len(),
                0,
            )
        };
        assert_eq!(rc, 0, "test setup: presetting the long value must succeed");

        apply_dropbox_ignore(&file, false)?;
        assert_eq!(
            read_attr(&c_path, &c_name),
            DROPBOX_IGNORE_VALUE,
            "a longer stored value must be overwritten with \"1\""
        );
        Ok(())
    }
```

- [ ] **Step 3: Run the test on the host**

Run: `cargo test wrong_attribute_value_is_rewritten -- --nocapture`

Expected: PASS, and no `skipping:` line in the output.

- [ ] **Step 4: Exclude the equivalent `+ → *` mutant with its proof**

The mutation shrinks the read buffer from `len()+1 = 2` to `len()*1 = 1` byte. Stored value of length 1: both buffers read that byte and compare it — identical result. Stored value of length ≥ 2: the 2-byte buffer sees a length mismatch (`len == 2 != 1`), the 1-byte buffer gets `ERANGE` (`len == -1`); both return "not marked", so the caller rewrites either way. No observable difference exists — equivalent mutant.

Append to `exclude_re` in `.cargo/mutants.toml`:

```toml
    # Equivalent mutant: shrinking the getxattr buffer from len()+1 to
    # len()*1 byte changes a length-mismatch result into an ERANGE result;
    # both read as "not marked" and trigger the same rewrite. Proven
    # equivalent in the 2026-07-16 triage (see the design spec).
    "src/dropbox\\.rs.*replace \\+ with \\* in is_already_ignored",
```

- [ ] **Step 5: Verify dropbox.rs is mutation-clean (green)**

Run: `./scripts/mutants.sh -f src/dropbox.rs; echo "exit=$?"`

Expected: `exit=0`, `missed.txt` empty. If `&&→||` still survives, the assertion is not raw enough — fix the test, not the exclusion list.

- [ ] **Step 6: Commit**

```bash
git add src/dropbox.rs .cargo/mutants.toml
git commit -m "test: prove wrong xattr values get rewritten; exclude equivalent buffer mutant"
```

---

### Task 4: dropbox.rs coverage — error paths

**Files:**
- Modify: `src/dropbox.rs` (tests module only)

**Interfaces:**
- Consumes: nothing new (plain `cargo test`).
- Produces: coverage for `src/dropbox.rs:69-75` (setxattr failure) and the interior-NUL context closure.

- [ ] **Step 1: Add the two error-path tests**

In `src/dropbox.rs` `mod tests`:

```rust
    #[test]
    fn setxattr_failure_is_reported_with_context() {
        // A path that cannot exist: getxattr reads as "not marked", then
        // setxattr fails with ENOENT and must surface as an error.
        let missing = Path::new("/nonexistent-dropignore-test/artifact");
        let err = apply_dropbox_ignore(missing, false)
            .expect_err("marking a nonexistent path must fail");
        assert!(
            format!("{err:#}").contains("setxattr failed"),
            "error must name the failing call, got: {err:#}"
        );
    }

    #[test]
    fn interior_nul_in_path_is_rejected() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::path::PathBuf;

        let path = PathBuf::from(OsString::from_vec(b"bad\0path".to_vec()));
        // dry_run=true: the NUL check fires before any filesystem access.
        let err = apply_dropbox_ignore(&path, true)
            .expect_err("a path with an interior NUL must be rejected");
        assert!(
            format!("{err:#}").contains("interior NUL"),
            "error must explain the NUL rejection, got: {err:#}"
        );
    }
```

Note: `setxattr_failure_is_reported_with_context` needs no xattr-support guard — it must fail identically everywhere (the target does not exist).

- [ ] **Step 2: Run the tests**

Run: `cargo test dropbox -- --nocapture`

Expected: all dropbox tests PASS (now 4).

- [ ] **Step 3: Confirm the coverage moved**

Run: `cargo llvm-cov --summary-only 2>/dev/null | grep dropbox`

Expected: function coverage for `dropbox.rs` > 60% (target 100%), line coverage > 86.67%. Record the numbers for the final report.

- [ ] **Step 4: Commit**

```bash
git add src/dropbox.rs
git commit -m "test: cover setxattr failure and interior-NUL error paths"
```

---

### Task 5: discovery.rs coverage — symlink and unreadable-dir branches

**Files:**
- Modify: `src/discovery.rs` (tests module only)

**Interfaces:**
- Consumes: existing test-module imports (`ArtifactDirsRule`, `RuleEngine`, `discover_watch_targets`).
- Produces: coverage for `src/discovery.rs:81-85` and `57-59`; a documented verdict on the remaining uncovered arms.

- [ ] **Step 1: Add the symlink-skip test**

In `src/discovery.rs` `mod tests`:

```rust
    #[test]
    fn walk_skips_symlinks_without_evaluating_them() -> Result<()> {
        let temp = TempDir::new().context("Failed to create temp dir")?;
        let real_dir = temp.path().join("real");
        fs::create_dir(&real_dir)?;
        // A symlink whose NAME matches a rule: it must be skipped before
        // rule evaluation, so it is neither marked nor watched.
        let link = temp.path().join("node_modules");
        std::os::unix::fs::symlink(&real_dir, &link)?;

        let engine = RuleEngine::new(vec![Box::new(ArtifactDirsRule::NODE_MODULES)]);
        let discovered = discover_watch_targets(temp.path(), &engine)?;

        assert!(
            discovered.matches.is_empty(),
            "a symlink must never be marked, even with a matching name"
        );
        assert!(
            !discovered.watchers.contains(&link),
            "a symlink must not be watched"
        );
        assert!(
            discovered.watchers.contains(&real_dir),
            "the symlink target reached by its real path is still watched"
        );
        Ok(())
    }
```

- [ ] **Step 2: Add the unreadable-directory test**

```rust
    #[test]
    fn unreadable_directory_is_skipped_and_walk_continues() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().context("Failed to create temp dir")?;
        let locked = temp.path().join("locked");
        let open_dir = temp.path().join("open");
        fs::create_dir(&locked)?;
        fs::create_dir(&open_dir)?;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))?;

        if fs::read_dir(&locked).is_ok() {
            // Running as root (e.g. inside the mutation container):
            // permission bits are not enforced, branch cannot be exercised.
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755))?;
            eprintln!("skipping: read_dir succeeds despite 0o000 (running as root)");
            return Ok(());
        }

        let engine = RuleEngine::new(vec![Box::new(ArtifactDirsRule::NODE_MODULES)]);
        let result = discover_watch_targets(temp.path(), &engine);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755))?;
        let discovered = result?;

        assert!(
            discovered.watchers.contains(&locked),
            "the unreadable directory itself is still a watch target"
        );
        assert!(
            discovered.watchers.contains(&open_dir),
            "the walk must continue past an unreadable directory"
        );
        Ok(())
    }
```

- [ ] **Step 3: Document the deliberately-untested arms**

At the top of `mod tests` in `src/discovery.rs` (right after the `use` lines), add:

```rust
    // Deliberately untested error arms in `walk` (deterministically
    // unreachable from a test): per-entry iteration errors and
    // `entry.file_type()` failures require a directory entry to vanish
    // between readdir and stat — a race we cannot stage reliably.
```

- [ ] **Step 4: Run the tests**

Run: `cargo test discovery -- --nocapture`

Expected: all discovery tests PASS (now 9); no `skipping:` line (host runs as non-root).

- [ ] **Step 5: Confirm the coverage moved**

Run: `cargo llvm-cov --summary-only 2>/dev/null | grep discovery`

Expected: `discovery.rs` line coverage > 93.23% (lines 57-59, 81-85 now covered; only the two racy arms remain). Record the numbers.

- [ ] **Step 6: Commit**

```bash
git add src/discovery.rs
git commit -m "test: cover symlink skip and unreadable-directory recovery in walk"
```

---

### Task 6: Full mutation sweep and survivor triage

**Files:**
- Modify: whichever `src/*.rs` test modules the survivors point at
- Modify: `.cargo/mutants.toml` (equivalent mutants only)

**Interfaces:**
- Consumes: `./scripts/mutants.sh` (Task 1), config (Tasks 2-3).
- Produces: a mutation-clean crate — full sweep exits 0.

- [ ] **Step 1: Run the full sweep**

Run: `./scripts/mutants.sh --no-shuffle; echo "exit=$?"` (do NOT pipe the command before reading `$?` — a pipe would report the last pipe stage's status)

This takes roughly 15-45 minutes (est. 150-250 mutants × a few seconds each; the container caps test time via auto-timeout). Run it in the background and continue only when finished.

Expected: exit 0 (unlikely on the first pass) or exit 2 with `mutants.out/mutants.out/missed.txt` listing survivors. Exit 4 means the baseline failed — stop and investigate before anything else.

- [ ] **Step 2: Classify every survivor**

For each line in `missed.txt`, decide with these rules and keep a running list (used in the final report):

1. **Real test gap** — some input would make the mutated program observably misbehave, and no test feeds it. → Step 3 pattern.
2. **Equivalent mutant** — argue concretely (as in Task 3 Step 4) that no input distinguishes mutant from original. Write the argument down; it becomes the exclusion comment. → Step 4 pattern.
3. **Genuine production bug** — the mutant is *more correct* than the original, or the analysis exposes wrong behavior. → STOP; report to the user before changing production code (Global Constraints).

Known likely survivors and their verdicts (verify, don't assume):
- Mutants in `watch_error_context` / log-message formatting (`String` replacements): messages are asserted by `watch_error_context_*` tests — anything surviving there is a real gap in those assertions.
- Mutants in `add_watch`'s ENOSPC arm (`match guard ... with true/false`): the arm needs real watch exhaustion; if mutants survive there, exclude with that rationale.
- `MarkedBuildDirRule::matches` `parent()` arm (rules.rs:235): the `None` arm is defensively unreachable (a path with a file name always has a parent — even a bare `"target"` has parent `""`); mutants flipping only that arm are equivalent.

- [ ] **Step 3: Pattern for a real gap — add a killing test (TDD)**

For each real gap, in the module's `mod tests`:

1. Write a test that pins the behavior the mutant breaks. It must observe *behavior* (return values, filesystem effects, registry state) — not internal helpers the mutation also rewires (see Task 3 Step 2's raw-readback lesson).
2. Run `cargo test <new_test_name>` — PASS against the real code.
3. Re-run `./scripts/mutants.sh -f src/<file>.rs` — the targeted mutant moves from `missed.txt` to `caught.txt`.

Batch commits per module:

```bash
git add src/<file>.rs
git commit -m "test: kill surviving mutants in <file>"
```

- [ ] **Step 4: Pattern for an equivalent mutant — exclude with proof**

Append to `exclude_re` in `.cargo/mutants.toml` (comment first, regex tight enough to match ONLY that mutant — include file, operator, and function):

```toml
    # <two-to-three-line equivalence argument>
    "src/<file>\\.rs.*replace <op> with <op> in <function>",
```

Verify with `./scripts/mutants.sh --list | grep <function>` that only the intended mutant disappeared.

```bash
git add .cargo/mutants.toml
git commit -m "build: exclude proven-equivalent mutant in <function>"
```

- [ ] **Step 5: Repeat until clean**

Re-run Step 1. Loop Steps 2-4 until: `./scripts/mutants.sh; echo "exit=$?"` prints `exit=0`.

---

### Task 7: README — how to verify the tests

**Files:**
- Modify: `README.md` (insert a subsection at the end of the existing `## Testing` section, before `## Extending rules`)

**Interfaces:**
- Consumes: final `scripts/mutants.sh` behavior from Tasks 1-6.

- [ ] **Step 1: Add the section**

Insert after the pre-commit paragraph of `## Testing` (keep the existing content unchanged):

````markdown
### Verifying the tests (mutation testing)

`cargo test` proves the code passes the tests; mutation testing proves the
tests catch bugs. `scripts/mutants.sh` runs [cargo-mutants] over the crate
inside a Podman container (mutated code is untrusted, so the repository is
mounted read-only and everything executes on a container-local copy):

```bash
./scripts/mutants.sh                  # full sweep (minutes)
./scripts/mutants.sh -f src/rules.rs  # one file
```

Requires Podman. Results land in `mutants.out/` (gitignored); exit code 0
means every mutant was caught, 2 means survivors are listed in
`mutants.out/mutants.out/missed.txt`. Scope and justified exclusions live
in `.cargo/mutants.toml`. Run it after adding rules or refactoring
match/apply logic.

[cargo-mutants]: https://mutants.rs/
````

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: document the containerized mutation-testing gate"
```

---

### Task 8: Final verification and report

**Files:** none (verification only)

- [ ] **Step 1: Full mutation sweep is clean**

Run: `./scripts/mutants.sh; echo "exit=$?"`
Expected: `exit=0`.

- [ ] **Step 2: Host gate is green**

Run: `./scripts/check.sh`
Expected: fmt, clippy (zero warnings), and all tests pass.

- [ ] **Step 3: Coverage did not regress**

Run: `cargo llvm-cov --summary-only`
Expected: TOTAL line coverage ≥ 95.10%; `dropbox.rs` function coverage > 60% and line coverage > 86.67%; `discovery.rs` line coverage > 93.23%.

- [ ] **Step 4: Report**

Summarize for the user: mutants total/caught/excluded (with each exclusion's rationale), tests added per module, before/after coverage table, and any production-code findings (expected: none, but the `&&→||` gap analysis from Task 3 belongs in the story).
