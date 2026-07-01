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
    use anyhow::Result;
    use std::fs;
    use tempfile::TempDir;

    /// True when the filesystem hosting `path` accepts user.* xattrs. Used to
    /// skip (not fail) on filesystems without support.
    fn xattr_supported(path: &Path) -> bool {
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let c_name = CString::new("user.dropignore.probe").unwrap();
        // SAFETY: pointers are valid for the duration of the call; the value
        // is one byte and the length matches.
        let result = unsafe {
            libc::setxattr(c_path.as_ptr(), c_name.as_ptr(), b"1".as_ptr().cast(), 1, 0)
        };
        result == 0
    }

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
}
