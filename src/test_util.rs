//! Test-only helpers shared across module test suites.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// True when the filesystem hosting `path` accepts user.* xattrs. Used to
/// skip (not fail) on filesystems without support.
pub(crate) fn xattr_supported(path: &Path) -> bool {
    let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let c_name = CString::new("user.dropignore.probe").unwrap();
    // SAFETY: pointers are valid for the duration of the call; the value
    // is one byte and the length matches.
    let result =
        unsafe { libc::setxattr(c_path.as_ptr(), c_name.as_ptr(), b"1".as_ptr().cast(), 1, 0) };
    result == 0
}
