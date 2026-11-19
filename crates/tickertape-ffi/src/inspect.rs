//! Read-only inspection (`verify`, segment list, checkpoint marker) and stand-alone retention.

use std::ffi::c_char;

use tickertape::{format, inspect, retention};

use crate::args;
use crate::options::tickertape_retention;
use crate::status::{
    FfiError, corrupt_reason_of, guard, tickertape_corrupt_reason, tickertape_status,
    tickertape_status_code as S,
};

/// One segment file, as listed by `tickertape_list_segments`. The layout is frozen for format version 1.
///
/// The file is `<dir>/<base_seq as 20 zero-padded decimal digits>.tape`.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct tickertape_segment_info {
    /// Sequence number of the segment's first record.
    pub base_seq: u64,
    /// File size in bytes.
    pub segment_size: u64,
    /// Creation time from the segment header, nanoseconds since the Unix epoch, by the writer's wall clock.
    pub created_unix_ns: u64,
    /// Format version from the segment header.
    pub format_version: u32,
    /// Always 0.
    pub reserved: u32,
}

/// The current checkpoint marker (`docs/format.md` → Checkpoint marker). The layout is frozen for format version 1.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct tickertape_checkpoint {
    /// Incremented on every checkpoint.
    pub generation: u64,
    /// Every record with a smaller sequence number was durable when the marker was written.
    pub next_seq: u64,
    /// The segment that contains the position of `next_seq`.
    pub segment_base_seq: u64,
    /// Offset of `next_seq`'s record (or of a seal) in that segment.
    pub offset: u64,
}

/// A summary of `tickertape_verify`. Set `struct_size` to `sizeof(tickertape_verify_report)` before the call; later
/// versions may append fields and write only those that fit in `struct_size`.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct tickertape_verify_report {
    /// Set by the caller to `sizeof(tickertape_verify_report)`; left unchanged.
    pub struct_size: usize,
    /// Number of segments checked.
    pub segments: u64,
    /// Number of valid records found.
    pub records: u64,
    /// Sequence number of the first valid record (valid only if `records` > 0).
    pub first_seq: u64,
    /// Sequence number of the last valid record (valid only if `records` > 0).
    pub last_seq: u64,
    /// Number of problems found. 0 means the journal is intact.
    pub problems: u64,
    /// Base sequence number of the segment with the first problem, parsed from its file name, or `UINT64_MAX` if the
    /// name does not parse (valid only if `problems` > 0).
    pub first_problem_base_seq: u64,
    /// Byte offset of the first problem in that segment (valid only if `problems` > 0).
    pub first_problem_offset: u64,
    /// The first problem's `tickertape_corrupt_reason` (`TICKERTAPE_CORRUPT_NONE` if there is none).
    pub first_problem_reason: i32,
    /// Always 0.
    pub reserved: u32,
}

/// What `tickertape_retention_apply` removed (or, with `dry_run`, would remove). Set `struct_size` to
/// `sizeof(tickertape_retention_report)` before the call; later versions may append fields and write only those that
/// fit in `struct_size`.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct tickertape_retention_report {
    /// Set by the caller to `sizeof(tickertape_retention_report)`; left unchanged.
    pub struct_size: usize,
    /// Segment files removed by this call (with `dry_run`: planned for removal).
    pub removed_segments: u64,
    /// Sum of the full sizes of those files.
    pub removed_bytes: u64,
    /// Planned files that were already gone (removed concurrently by someone else). Always 0 with `dry_run`.
    pub already_removed_segments: u64,
}

/// Writes `value` into a caller's report struct, keeping the caller's `struct_size`.
///
/// # Safety
///
/// `ptr` must be valid for writing `size` bytes, where `size` is the struct's `struct_size` field, already checked
/// to be at least `size_of::<T>()`.
unsafe fn write_report<T>(ptr: *mut T, value: T) {
    // This version's structs are complete at `size_of::<T>()`; larger `struct_size` values come from newer headers.
    // SAFETY: the caller guarantees `ptr` is valid for writing at least `size_of::<T>()` bytes and aligned for `T`.
    unsafe { ptr.write(value) };
}

/// Checks every segment in directory `dir`: every header, record and seal, and the chain between segments, and writes
/// a summary to `*report`. Takes no lock and changes nothing, so it is safe while a writer is active (the newest
/// segment may grow during the scan).
///
/// Corruption is reported in `report->problems` with `TICKERTAPE_OK`, not as an error. Uncommitted bytes after the
/// end of the newest segment are not a problem. Errors: `TICKERTAPE_E_NOT_FOUND`, `TICKERTAPE_E_IO`,
/// `TICKERTAPE_E_INVALID_ARGUMENT` if `report->struct_size` is too small.
///
/// # Safety
///
/// `dir` must be NULL or a NUL-terminated string. `report` must be NULL or point to a writable
/// `tickertape_verify_report` whose `struct_size` is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_verify(
    dir: *const c_char,
    report: *mut tickertape_verify_report,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let struct_size = unsafe { args::struct_size(report, "report") }?;
        // SAFETY: forwarded caller contract.
        let dir = unsafe { args::path(dir, "dir") }?;
        let verified = inspect::verify(dir)?;
        let mut out = tickertape_verify_report {
            struct_size,
            segments: verified.segments.len() as u64,
            records: verified.records,
            first_seq: verified.first_seq.unwrap_or(0),
            last_seq: verified.last_seq.unwrap_or(0),
            problems: verified.problems.len() as u64,
            first_problem_reason: tickertape_corrupt_reason::TICKERTAPE_CORRUPT_NONE as i32,
            ..tickertape_verify_report::default()
        };
        if let Some(problem) = verified.problems.first() {
            out.first_problem_base_seq = problem
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(format::parse_segment_file_name)
                .unwrap_or(u64::MAX);
            out.first_problem_offset = problem.offset;
            out.first_problem_reason = corrupt_reason_of(&problem.reason) as i32;
        }
        // SAFETY: `report` is valid for `struct_size >= size_of` bytes (checked by `args::struct_size`).
        unsafe { write_report(report, out) };
        Ok(S::TICKERTAPE_OK)
    })
}

/// Lists the segments in directory `dir`, oldest first, into `segments` (room for `capacity` entries), and stores the
/// number of segments in `*count_out`.
///
/// Call with `segments` NULL and `capacity` 0 to learn the count (returns `TICKERTAPE_OK`). If `capacity` is smaller
/// than the count, returns `TICKERTAPE_E_BUFFER_TOO_SMALL` with `*count_out` set and nothing written. The list can
/// change between calls while a writer is active. Errors: `TICKERTAPE_E_NOT_FOUND` if there are no segments;
/// `TICKERTAPE_E_CORRUPT` or `TICKERTAPE_E_UNSUPPORTED_VERSION` for an invalid header; `TICKERTAPE_E_IO`.
///
/// # Safety
///
/// `dir` must be NULL or a NUL-terminated string. `segments` must be valid for writing `capacity` entries (or NULL
/// with `capacity` 0). `count_out` must be NULL or valid for writing a `size_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_list_segments(
    dir: *const c_char,
    segments: *mut tickertape_segment_info,
    capacity: usize,
    count_out: *mut usize,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        unsafe { args::write(count_out, 0, "count_out") }?;
        if segments.is_null() && capacity > 0 {
            return Err(FfiError::invalid(
                "segments must not be NULL when capacity is not 0",
            ));
        }
        // SAFETY: forwarded caller contract.
        let dir = unsafe { args::path(dir, "dir") }?;
        let list = inspect::list_segments(dir)?;
        // SAFETY: checked at the start that `count_out` is writable.
        unsafe { count_out.write(list.len()) };
        if segments.is_null() {
            return Ok(S::TICKERTAPE_OK);
        }
        if capacity < list.len() {
            return Err(FfiError::new(
                S::TICKERTAPE_E_BUFFER_TOO_SMALL,
                format!("{} segments; room for {capacity}", list.len()),
            ));
        }
        for (index, info) in list.iter().enumerate() {
            let entry = tickertape_segment_info {
                base_seq: info.base_seq,
                segment_size: info.segment_size,
                created_unix_ns: info.created_unix_ns,
                format_version: u32::from(info.format_version),
                reserved: 0,
            };
            // SAFETY: `index < list.len() <= capacity`, and `segments` is valid for `capacity` entries.
            unsafe { segments.add(index).write(entry) };
        }
        Ok(S::TICKERTAPE_OK)
    })
}

/// Reads the current checkpoint marker of the journal in directory `dir` into `*checkpoint_out`, without taking a
/// lock or changing anything. Returns `TICKERTAPE_NONE` (leaving `*checkpoint_out` unchanged) if the marker file is
/// missing or neither slot is valid. This observes the marker; it does not validate the position it describes.
/// Errors: `TICKERTAPE_E_IO` if the marker exists but cannot be read.
///
/// # Safety
///
/// `dir` must be NULL or a NUL-terminated string. `checkpoint_out` must be NULL or valid for writing a
/// `tickertape_checkpoint`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_read_checkpoint(
    dir: *const c_char,
    checkpoint_out: *mut tickertape_checkpoint,
) -> tickertape_status {
    guard(|| {
        if checkpoint_out.is_null() {
            return Err(FfiError::null("checkpoint_out"));
        }
        // SAFETY: forwarded caller contract.
        let dir = unsafe { args::path(dir, "dir") }?;
        let Some(slot) = inspect::read_checkpoint(dir)? else {
            return Ok(S::TICKERTAPE_NONE);
        };
        let out = tickertape_checkpoint {
            generation: slot.generation,
            next_seq: slot.next_seq,
            segment_base_seq: slot.segment_base_seq,
            offset: slot.offset,
        };
        // SAFETY: checked above that it is not NULL; the caller guarantees it is writable.
        unsafe { checkpoint_out.write(out) };
        Ok(S::TICKERTAPE_OK)
    })
}

/// Applies retention to the journal in directory `dir` once (ADR 0016) and writes what it did to `*report` (which
/// may be NULL). With `dry_run` non-zero, only plans: nothing changes and the report says what would be removed.
///
/// Only a sealed prefix strictly before the checkpoint segment is eligible, removed oldest first; without a valid
/// checkpoint marker nothing is. Takes no writer lock and is safe while a writer is active. Stops at the first
/// error; removals before it stay in effect (and are not counted in a report, which is written only on success).
/// Errors: `TICKERTAPE_E_INVALID_ARGUMENT` if `retention` has no action (not needed with `dry_run`);
/// `TICKERTAPE_E_NOT_FOUND`; `TICKERTAPE_E_CORRUPT`; `TICKERTAPE_E_IO` (including an existing destination file).
///
/// # Safety
///
/// `dir` must be NULL or a NUL-terminated string. `retention` must be NULL or live retention settings. `report` must
/// be NULL or point to a writable `tickertape_retention_report` whose `struct_size` is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_retention_apply(
    dir: *const c_char,
    retention: *const tickertape_retention,
    dry_run: i32,
    report: *mut tickertape_retention_report,
) -> tickertape_status {
    guard(|| {
        let struct_size = if report.is_null() {
            0
        } else {
            // SAFETY: forwarded caller contract.
            unsafe { args::struct_size(report, "report") }?
        };
        // SAFETY: forwarded caller contract.
        let dir = unsafe { args::path(dir, "dir") }?;
        if retention.is_null() {
            return Err(FfiError::null("retention"));
        }
        // SAFETY: not NULL; the caller guarantees live retention settings.
        let settings = unsafe { &*retention };
        let action = if dry_run == 0 {
            Some(settings.action()?)
        } else {
            None
        };
        let plan = retention::plan(dir, &settings.policy)?;
        let out = if let Some(action) = action {
            let applied = retention::apply(dir, &plan, action)?;
            tickertape_retention_report {
                struct_size,
                removed_segments: applied.removed_segments,
                removed_bytes: applied.removed_bytes,
                already_removed_segments: applied.already_removed_segments,
            }
        } else {
            tickertape_retention_report {
                struct_size,
                removed_segments: plan.segments().len() as u64,
                removed_bytes: plan
                    .segments()
                    .iter()
                    .fold(0u64, |sum, s| sum.saturating_add(s.size_bytes)),
                already_removed_segments: 0,
            }
        };
        if !report.is_null() {
            // SAFETY: `report` is valid for `struct_size >= size_of` bytes (checked by `args::struct_size`).
            unsafe { write_report(report, out) };
        }
        Ok(S::TICKERTAPE_OK)
    })
}
