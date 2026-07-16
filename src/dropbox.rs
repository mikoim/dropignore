use anyhow::{Context, Result};
use log::{debug, error, info};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Attribute name that instructs Dropbox to ignore a path.
const DROPBOX_IGNORE_ATTR: &str = "user.com.dropbox.ignored";
/// Attribute value recognized by Dropbox.
const DROPBOX_IGNORE_VALUE: &[u8] = b"1";

/// True when the path already carries the ignore attribute with the expected
/// value. Any read failure (ENODATA, ENOTSUP, ERANGE, …) reads as "not
/// marked", so the caller falls through to setxattr, which reports real
/// errors. Watched paths are never symlinks (walk and event path both skip
/// them), so the symlink-following getxattr is safe here.
fn is_already_ignored(c_path: &CString, c_name: &CString) -> bool {
    // One byte larger than the expected value so a longer stored value yields
    // a length mismatch instead of a truncated false positive.
    let mut value = [0u8; DROPBOX_IGNORE_VALUE.len() + 1];
    // SAFETY: pointers are valid for the duration of the call and the size
    // matches the buffer.
    let len = unsafe {
        libc::getxattr(
            c_path.as_ptr(),
            c_name.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    len == DROPBOX_IGNORE_VALUE.len() as isize
        && &value[..DROPBOX_IGNORE_VALUE.len()] == DROPBOX_IGNORE_VALUE
}

/// Apply the Dropbox ignore attribute to the given path, honoring dry-run
/// mode. Skips the write (debug log only) when the attribute is already set,
/// so rescans do not rewrite or re-announce paths marked earlier.
pub(crate) fn apply_dropbox_ignore(path: &Path, dry_run: bool) -> Result<()> {
    // Construct C strings for the path and attribute name. Path conversion uses
    // raw bytes to support non-UTF8 names on Unix.
    let c_path = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("Path contains interior NUL byte: {}", path.display()))?;
    let c_name =
        CString::new(DROPBOX_IGNORE_ATTR).expect("static attribute name should never contain NUL");

    if is_already_ignored(&c_path, &c_name) {
        debug!("{} is already marked as ignored", path.display());
        return Ok(());
    }

    if dry_run {
        info!("(dry-run) Would mark {} as ignored", path.display());
        return Ok(());
    }

    // SAFETY: Pointers are valid for the duration of the call, sizes are correct,
    // and flags is set to 0 for "create or replace".
    let result = unsafe {
        libc::setxattr(
            c_path.as_ptr(),
            c_name.as_ptr(),
            DROPBOX_IGNORE_VALUE.as_ptr().cast(),
            DROPBOX_IGNORE_VALUE.len(),
            0,
        )
    };

    if result != 0 {
        let err = std::io::Error::last_os_error();
        error!(
            "Failed to set {} on {}: {err}",
            DROPBOX_IGNORE_ATTR,
            path.display()
        );
        return Err(err).with_context(|| format!("setxattr failed for {}", path.display()));
    }

    info!("Marked {} as ignored", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::xattr_supported;
    use anyhow::Result;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn already_marked_path_is_detected_and_skipped() -> Result<()> {
        let temp = TempDir::new()?;
        let file = temp.path().join("artifact");
        fs::write(&file, b"")?;
        if !xattr_supported(temp.path()) {
            eprintln!("skipping: filesystem lacks user.* xattr support");
            return Ok(());
        }

        let c_path = CString::new(file.as_os_str().as_bytes())?;
        let c_name = CString::new(DROPBOX_IGNORE_ATTR)?;

        assert!(
            !is_already_ignored(&c_path, &c_name),
            "fresh file must not read as marked"
        );

        apply_dropbox_ignore(&file, false)?;
        assert!(
            is_already_ignored(&c_path, &c_name),
            "marked file must read as marked"
        );

        // Re-applying (real and dry-run) must succeed as a no-op.
        apply_dropbox_ignore(&file, false)?;
        apply_dropbox_ignore(&file, true)?;
        Ok(())
    }

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

    #[test]
    fn setxattr_failure_is_reported_with_context() {
        // A path that cannot exist: getxattr reads as "not marked", then
        // setxattr fails with ENOENT and must surface as an error.
        let missing = Path::new("/nonexistent-dropignore-test/artifact");
        let err =
            apply_dropbox_ignore(missing, false).expect_err("marking a nonexistent path must fail");
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
}
