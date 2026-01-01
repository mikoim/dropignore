use anyhow::{Context, Result};
use log::{error, info};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Attribute name that instructs Dropbox to ignore a path.
const DROPBOX_IGNORE_ATTR: &str = "user.com.dropbox.ignored";
/// Attribute value recognized by Dropbox.
const DROPBOX_IGNORE_VALUE: &[u8] = b"1";

/// Apply the Dropbox ignore attribute to the given path, honoring dry-run mode.
pub(crate) fn apply_dropbox_ignore(path: &Path, dry_run: bool) -> Result<()> {
    if dry_run {
        info!("(dry-run) Would mark {} as ignored", path.display());
        return Ok(());
    }

    // Construct C strings for the path and attribute name. Path conversion uses
    // raw bytes to support non-UTF8 names on Unix.
    let c_path = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("Path contains interior NUL byte: {}", path.display()))?;
    let c_name =
        CString::new(DROPBOX_IGNORE_ATTR).expect("static attribute name should never contain NUL");

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
