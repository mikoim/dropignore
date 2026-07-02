# One-shot Scan Mode (`--scan-once`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `--scan-once` flag that walks the tree once, marks every rule match with the Dropbox ignore attribute, and exits with a failure-reflecting exit code — without initializing inotify or signal handlers.

**Architecture:** `run()` in `src/app.rs` branches right after its shared prefix (canonicalize → `ensure_directory` → `RuleEngine::new`) into a new `scan_once` function that reuses `discover_watch_targets` + `apply_all` and ignores discovered watch targets. Like `apply_all`, `scan_once` takes the application closure as a parameter so its bail path is testable without real xattr failures.

**Tech Stack:** Rust (edition 2024), clap 4.5 derive, anyhow, log; libc only inside tests for xattr verification. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-07-02-scan-once-mode-design.md`

## Global Constraints

- No new dependencies in `Cargo.toml`.
- Verification bar for every task: `cargo test` green, `cargo clippy --all-targets` with zero warnings, `cargo fmt --check` clean.
- The resident watch mode (inotify path) must be byte-for-byte untouched: no behavior or exit-code changes.
- `--scan-once` composes with `--dry-run` (no `conflicts_with`).
- Exit code: 0 when every matched path was marked (or dry-run); non-zero when setup fails or at least one mark fails.
- Code and comments in English, matching the existing style (doc comments state constraints, not narration).
- Work on the existing `scan-once-mode` branch (spec is already committed there). Commit messages follow the `feat(scope):` / `test(scope):` / `docs:` convention seen in `git log`.

**Note on task boundaries:** the CLI field, the `scan_once` function, and the `run()` wiring must land in the same commit — any of them alone is dead code in the bin target and breaks the zero-warning bar. That is why Task 1 is larger than usual.

---

### Task 1: `--scan-once` flag, `scan_once` function, and `run()` wiring

**Files:**
- Modify: `src/cli.rs` (add field to `CliArgs` at `src/cli.rs:11-18`; add test in the existing `tests` module)
- Modify: `src/app.rs` (branch in `run()` after `src/app.rs:37`; new `scan_once` function after `run()`; tests in the existing `tests` module)

**Interfaces:**
- Consumes: `discover_watch_targets(&Path, &RuleEngine) -> Result<DiscoveredPaths>` (`src/discovery.rs`), `apply_all(&[PathBuf], FnMut(&Path) -> Result<()>) -> usize` (`src/app.rs:342`), `apply_dropbox_ignore(&Path, bool) -> Result<()>` (`src/dropbox.rs`).
- Produces: `CliArgs.scan_once: bool`; `fn scan_once<F>(root: &Path, rules: &RuleEngine, apply: F) -> Result<()> where F: FnMut(&Path) -> Result<()>` (private to `app.rs`; Task 2 adds another test against this exact signature).

- [ ] **Step 1: Write the failing CLI test**

Append to the `tests` module in `src/cli.rs`:

```rust
#[test]
fn scan_once_flag_parses_and_defaults_off() {
    let on = CliArgs::parse_from(["dropignore", "--scan-once", "/tmp"]);
    assert!(on.scan_once, "--scan-once must set the flag");
    assert_eq!(on.root, PathBuf::from("/tmp"));

    let off = CliArgs::parse_from(["dropignore", "/tmp"]);
    assert!(!off.scan_once, "flag must default to false");
}
```

- [ ] **Step 2: Write the failing `scan_once` unit tests**

Append to the `tests` module in `src/app.rs` (the `engine()` helper at `src/app.rs:435` already builds a `NODE_MODULES`-only `RuleEngine`):

```rust
#[test]
fn scan_once_visits_matches_only() -> Result<()> {
    let temp = TempDir::new()?;
    let ignored = temp.path().join("node_modules");
    let plain = temp.path().join("src");
    fs::create_dir(&ignored)?;
    fs::create_dir(&plain)?;

    let mut visited = Vec::new();
    scan_once(temp.path(), &engine(), |path| {
        visited.push(path.to_path_buf());
        Ok(())
    })?;

    assert_eq!(visited, vec![ignored], "only the matched path is applied");
    Ok(())
}

#[test]
fn scan_once_fails_when_any_apply_fails() -> Result<()> {
    let temp = TempDir::new()?;
    fs::create_dir(temp.path().join("node_modules"))?;

    let err = scan_once(temp.path(), &engine(), |_| anyhow::bail!("boom"))
        .expect_err("a failed apply must fail the scan");

    assert!(
        err.to_string().contains("1 of 1"),
        "message must carry failure/total counts, got: {err}"
    );
    Ok(())
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test scan_once`
Expected: compilation FAILS — `no field scan_once on CliArgs` and `cannot find function scan_once`.

- [ ] **Step 4: Implement flag, function, and wiring**

In `src/cli.rs`, add after the `dry_run` field (`src/cli.rs:17`):

```rust
    /// Scan the tree once, mark matches, and exit without watching.
    #[arg(long = "scan-once", default_value_t = false)]
    pub(crate) scan_once: bool,
```

In `src/app.rs`, insert the branch in `run()` immediately after the `RuleEngine::new(...)` block ends (`src/app.rs:37`), before the shutdown-flag comment:

```rust
    if args.scan_once {
        return scan_once(&root, &rule_engine, |path| {
            apply_dropbox_ignore(path, args.dry_run)
        });
    }
```

Add the function directly after `run()` (before `struct EntryAction`):

```rust
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

- [ ] **Step 5: Run the new tests to verify they pass**

Run: `cargo test scan_once`
Expected: PASS — `scan_once_flag_parses_and_defaults_off`, `scan_once_visits_matches_only`, `scan_once_fails_when_any_apply_fails`.

- [ ] **Step 6: Run the full verification bar**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: all tests pass, zero clippy warnings, no fmt diff.

- [ ] **Step 7: Commit**

```bash
git add src/cli.rs src/app.rs
git commit -m "feat(app): add --scan-once one-shot scan mode"
```

---

### Task 2: End-to-end xattr test for `scan_once`

**Files:**
- Modify: `src/app.rs` (tests module only)

**Interfaces:**
- Consumes: `scan_once<F>(root: &Path, rules: &RuleEngine, apply: F) -> Result<()>` and `apply_dropbox_ignore(&Path, bool) -> Result<()>` from Task 1.
- Produces: nothing new (test-only).

- [ ] **Step 1: Write the end-to-end test**

Append to the `tests` module in `src/app.rs`. The xattr-support probe mirrors the one in `src/dropbox.rs:91-99` (that helper is private to its own tests module, so it is duplicated here); the skip-not-fail guard mirrors the overflow tests at `src/app.rs:1107`. Add the needed imports at the top of the tests module (alongside the existing `use` lines at `src/app.rs:426-433`):

```rust
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
```

```rust
    /// True when the filesystem hosting `path` accepts user.* xattrs. Used to
    /// skip (not fail) on filesystems without support.
    fn xattr_supported(path: &Path) -> bool {
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let c_name = CString::new("user.dropignore.probe").unwrap();
        // SAFETY: pointers are valid for the duration of the call; the value
        // is one byte and the length matches.
        let result =
            unsafe { libc::setxattr(c_path.as_ptr(), c_name.as_ptr(), b"1".as_ptr().cast(), 1, 0) };
        result == 0
    }

    #[test]
    fn scan_once_marks_matches_with_real_xattr() -> Result<()> {
        let temp = TempDir::new()?;
        let ignored = temp.path().join("node_modules");
        fs::create_dir(&ignored)?;
        if !xattr_supported(temp.path()) {
            eprintln!("skipping: filesystem lacks user.* xattr support");
            return Ok(());
        }

        scan_once(temp.path(), &engine(), |path| {
            apply_dropbox_ignore(path, false)
        })?;

        let c_path = CString::new(ignored.as_os_str().as_bytes())?;
        let c_name = CString::new("user.com.dropbox.ignored")?;
        // One byte larger than the expected value so a longer stored value
        // yields a length mismatch instead of a truncated false positive.
        let mut value = [0u8; 2];
        // SAFETY: pointers are valid for the duration of the call and the
        // size matches the buffer.
        let len = unsafe {
            libc::getxattr(
                c_path.as_ptr(),
                c_name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        assert_eq!(len, 1, "attribute must be exactly one byte");
        assert_eq!(&value[..1], b"1", "attribute value must be \"1\"");
        Ok(())
    }
```

- [ ] **Step 2: Run the test to verify it passes**

Run: `cargo test scan_once_marks_matches_with_real_xattr`
Expected: PASS (or the eprintln skip message on filesystems without `user.*` xattr support — the assertion path must be exercised at least once on a supporting filesystem).

- [ ] **Step 3: Run the full verification bar**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: all tests pass, zero clippy warnings, no fmt diff.

- [ ] **Step 4: Commit**

```bash
git add src/app.rs
git commit -m "test(app): cover scan-once end-to-end xattr marking"
```

---

### Task 3: README documentation

**Files:**
- Modify: `README.md` (Features list at line 5-10, Usage section at lines 12-16)

**Interfaces:**
- Consumes: the `--scan-once` CLI behavior from Task 1 (flag name and exit-code semantics must match exactly).
- Produces: nothing (docs only).

- [ ] **Step 1: Update Features and Usage**

In `README.md`, add one bullet at the end of the Features list (after the `env_logger` bullet):

```markdown
- One-shot scan mode (`--scan-once`) for cron or systemd-timer use: marks existing matches and exits without watching.
```

Replace the Usage code block:

````markdown
## Usage
```bash
cargo run -- --dry-run /home/foo/Dropbox  # inspect what would be ignored
cargo run -- /home/foo/Dropbox            # apply Dropbox ignore attribute
cargo run -- --scan-once /home/foo/Dropbox  # mark existing matches once and exit
```
````

Add a subsection after the Usage code block (before `### Logging`):

````markdown
### One-shot scans
`--scan-once` walks the tree once, marks every match, and exits without
registering any inotify watches. It composes with `--dry-run` to preview.
The process exits non-zero when at least one matched path could not be
marked, so failures surface through cron mail or a systemd `OnFailure=`
unit. Example crontab entry:

```cron
0 * * * * /usr/local/bin/dropignore --scan-once /home/foo/Dropbox
```
````

- [ ] **Step 2: Verify the flag name against the binary**

Run: `cargo run -- --help | grep -A1 scan-once`
Expected: shows `--scan-once` with the help text "Scan the tree once, mark matches, and exit without watching."

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: document --scan-once one-shot scan mode"
```
