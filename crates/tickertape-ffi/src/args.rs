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

/// Borrows `count` values of type `T` at `ptr`. `ptr` may be NULL only when `count` is 0. Nothing is read: this
/// only checks the pointer and that the array's size in bytes fits in `isize::MAX`.
///
/// # Safety
///
/// If `count > 0`, `ptr` must be NULL or point to `count` initialised values that stay valid and are not written for
/// `'a`.
pub(crate) unsafe fn slice<'a, T>(
    ptr: *const T,
    count: usize,
    name: &str,
    count_name: &str,
) -> Result<&'a [T], FfiError> {
    if count == 0 {
        return Ok(&[]);
    }
    if ptr.is_null() {
        return Err(FfiError::invalid(format!(
            "{name} must not be NULL when {count_name} is not 0"
        )));
    }
    if count
        .checked_mul(size_of::<T>())
        .is_none_or(|bytes| bytes > isize::MAX as usize)
    {
        return Err(FfiError::invalid(format!("{count_name} is too large")));
    }
    if !ptr.is_aligned() {
        return Err(FfiError::invalid(format!("{name} is not aligned")));
    }
    // SAFETY: not NULL, aligned, the size in bytes fits in isize, and the caller guarantees `count` initialised values
    // valid and unmodified for 'a.
    Ok(unsafe { std::slice::from_raw_parts(ptr, count) })
}

/// A range of caller memory that one call reads or writes, for the overlap checks (ADR 0028 → Ownership).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Region<'n> {
    start: usize,
    len: usize,
    name: &'n str,
}

impl<'n> Region<'n> {
    /// `count` values of type `T` at `ptr`; empty if `ptr` is NULL. Only the address is used.
    pub(crate) fn of<T>(ptr: *const T, count: usize, name: &'n str) -> Region<'n> {
        let len = if ptr.is_null() {
            0
        } else {
            count.saturating_mul(size_of::<T>())
        };
        Region {
            start: ptr.addr(),
            len,
            name,
        }
    }

    pub(crate) fn name(&self) -> &'n str {
        self.name
    }

    pub(crate) fn overlaps(&self, other: &Region<'_>) -> bool {
        self.len > 0
            && other.len > 0
            && self.start < other.start.saturating_add(other.len)
            && other.start < self.start.saturating_add(self.len)
    }
}

/// Fails with `TICKERTAPE_E_INVALID_ARGUMENT` if any two of `regions` overlap.
pub(crate) fn disjoint(regions: &[Region<'_>]) -> Result<(), FfiError> {
    for (index, a) in regions.iter().enumerate() {
        if let Some(b) = regions[index + 1..].iter().find(|b| a.overlaps(b)) {
            return Err(FfiError::invalid(format!(
                "{} and {} must not overlap",
                a.name, b.name
            )));
        }
    }
    Ok(())
}

/// Reads the `uint64_t struct_size` field at the start of a caller's extensible struct and checks it against the
/// struct's minimum size (its size in the first stable release). Returns how many bytes the library fills:
/// `min(struct_size, size_of::<T>())`. See ADR 0028 → Versioning.
///
/// # Safety
///
/// `ptr` must be NULL or point to a readable `uint64_t` (the struct's first field).
pub(crate) unsafe fn struct_size<T>(
    ptr: *const T,
    min_size: usize,
    name: &str,
) -> Result<usize, FfiError> {
    if ptr.is_null() {
        return Err(FfiError::null(name));
    }
    if !ptr.is_aligned() {
        return Err(FfiError::invalid(format!("{name} is not aligned")));
    }
    // SAFETY: not NULL and aligned (a `T` starts with its `uint64_t struct_size`, so `T`'s alignment covers it); the
    // caller guarantees the field is readable.
    let size = unsafe { ptr.cast::<u64>().read() };
    if size < min_size as u64 {
        return Err(FfiError::invalid(format!(
            "{name}->struct_size is {size}; set it to sizeof the struct (at least {min_size})"
        )));
    }
    Ok(usize::try_from(size).map_or(size_of::<T>(), |size| size.min(size_of::<T>())))
}

/// Writes the first `filled` bytes of `value` to a caller's extensible struct, then stores `filled` in its
/// `struct_size` field so that the caller can tell which fields were written.
///
/// # Safety
///
/// `ptr` must be valid for writing `filled` bytes and aligned for `T`. `filled` must come from [`struct_size`] for
/// the same `T`, and `T` must start with a `u64 struct_size` field and have no padding bytes.
pub(crate) unsafe fn write_struct<T: Copy>(ptr: *mut T, value: T, filled: usize) {
    debug_assert!((size_of::<u64>()..=size_of::<T>()).contains(&filled));
    // SAFETY: `value` is a live local of `filled <= size_of::<T>()` bytes without padding; the caller guarantees
    // `ptr` is valid for writing `filled` bytes, and a caller-owned struct cannot overlap a local.
    unsafe {
        std::ptr::copy_nonoverlapping((&raw const value).cast::<u8>(), ptr.cast::<u8>(), filled)
    };
    // SAFETY: `filled >= 8` bytes are writable, and the struct starts with an aligned `u64`.
    unsafe { ptr.cast::<u64>().write(filled as u64) };
}

/// Converts a timeout in nanoseconds: 0 means do not wait (`Some(ZERO)`), negative means wait forever (`None`).
pub(crate) fn timeout(ns: i64) -> Option<Duration> {
    u64::try_from(ns).ok().map(Duration::from_nanos)
}
