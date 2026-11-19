//! Checked conversions of C arguments. Every raw-pointer access of the crate outside handle state goes through here.

use std::ffi::{CStr, OsStr, c_char};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::Duration;

use crate::status::FfiError;

/// Borrows a NUL-terminated path argument.
///
/// # Safety
///
/// `ptr` must be NULL or point to a NUL-terminated string that stays valid and unmodified for `'a`.
pub(crate) unsafe fn path<'a>(ptr: *const c_char, name: &str) -> Result<&'a Path, FfiError> {
    if ptr.is_null() {
        return Err(FfiError::null(name));
    }
    // SAFETY: not NULL, and the caller guarantees a NUL-terminated string valid for 'a.
    let bytes = unsafe { CStr::from_ptr(ptr) }.to_bytes();
    if bytes.is_empty() {
        return Err(FfiError::invalid(format!("{name} must not be empty")));
    }
    Ok(Path::new(OsStr::from_bytes(bytes)))
}

/// Borrows `len` bytes at `ptr`. `ptr` may be NULL only when `len` is 0.
///
/// # Safety
///
/// If `len > 0`, `ptr` must be NULL or point to `len` readable bytes that stay valid and are not written for `'a`.
pub(crate) unsafe fn bytes<'a>(
    ptr: *const u8,
    len: usize,
    name: &str,
) -> Result<&'a [u8], FfiError> {
    if len == 0 {
        return Ok(&[]);
    }
    if ptr.is_null() {
        return Err(FfiError::invalid(format!(
            "{name} must not be NULL when its length is not 0"
        )));
    }
    if len > isize::MAX as usize {
        return Err(FfiError::invalid(format!("{name} length is too large")));
    }
    // SAFETY: not NULL, `len` fits in isize, and the caller guarantees `len` readable bytes valid for 'a.
    Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// Stores `value` through an output pointer.
///
/// # Safety
///
/// `ptr` must be NULL or valid for writing a `T` and properly aligned.
pub(crate) unsafe fn write<T>(ptr: *mut T, value: T, name: &str) -> Result<(), FfiError> {
    if ptr.is_null() {
        return Err(FfiError::null(name));
    }
    // SAFETY: not NULL; the caller guarantees it is valid for writes and aligned. `write` does not read or drop the
    // old (possibly uninitialized) contents.
    unsafe { ptr.write(value) };
    Ok(())
}

/// Stores `value` through an output pointer that may be NULL (the value is then discarded).
///
/// # Safety
///
/// `ptr` must be NULL or valid for writing a `T` and properly aligned.
pub(crate) unsafe fn write_optional<T>(ptr: *mut T, value: T) {
    if !ptr.is_null() {
        // SAFETY: not NULL; the caller guarantees it is valid for writes and aligned.
        unsafe { ptr.write(value) };
    }
}

/// Copies `src` to `dst + offset`.
///
/// # Safety
///
/// `dst` must be valid for writing `offset + src.len()` bytes, and must not overlap `src`.
pub(crate) unsafe fn copy_to(dst: *mut u8, offset: usize, src: &[u8]) {
    if src.is_empty() {
        return;
    }
    // SAFETY: the caller guarantees `[dst + offset, dst + offset + src.len())` is writable and disjoint from `src`;
    // `src` is a valid slice.
    unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dst.add(offset), src.len()) };
}

/// Reads the `struct_size` field at the start of a caller's report struct, checking that it covers `T`.
///
/// # Safety
///
/// `ptr` must be NULL or point to a readable, aligned `usize` (the struct's first field).
pub(crate) unsafe fn struct_size<T>(ptr: *const T, name: &str) -> Result<usize, FfiError> {
    if ptr.is_null() {
        return Err(FfiError::null(name));
    }
    // SAFETY: not NULL; the caller guarantees the struct starts with an aligned, readable `size_t`.
    let size = unsafe { ptr.cast::<usize>().read() };
    if size < std::mem::size_of::<T>() {
        return Err(FfiError::invalid(format!(
            "{name}->struct_size is {size}; set it to sizeof the struct ({} in this version)",
            std::mem::size_of::<T>()
        )));
    }
    Ok(size)
}

/// Converts a timeout in nanoseconds: 0 means do not wait (`Some(ZERO)`), negative means wait forever (`None`).
pub(crate) fn timeout(ns: i64) -> Option<Duration> {
    u64::try_from(ns).ok().map(Duration::from_nanos)
}
