//! Opaque option builders: writer options, reader options and retention settings.

use std::ffi::c_char;
use std::time::Duration;

use tickertape::retention::{Action, Policy};
use tickertape::{Backoff, ReaderConfig, RecoveryPolicy, WriterConfig};

use crate::args;
use crate::status::{FfiError, guard, tickertape_status, tickertape_status_code as S};

/// What a writer does on open when it finds a corrupt committed record at the end of the newest segment
/// (`docs/format.md` → Recovery). Uncommitted partial records are always cleaned up, whatever the policy.
#[allow(non_camel_case_types)]
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum tickertape_recovery_policy {
    /// Copy the invalid bytes to `quarantine/` in the journal directory, then continue after the last valid record.
    /// Nothing is ever deleted. The default.
    TICKERTAPE_RECOVERY_QUARANTINE = 0,
    /// Fail the open with `TICKERTAPE_E_CORRUPT` instead of quarantining or repairing anything.
    TICKERTAPE_RECOVERY_FAIL = 1,
}

/// Special `timeout_ns` values for the reader functions. Any positive value waits up to that many nanoseconds.
#[allow(non_camel_case_types)]
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum tickertape_timeout {
    /// Do not wait: return `TICKERTAPE_NONE` at once if no record is available.
    TICKERTAPE_NO_WAIT = 0,
    /// Wait until a record is available (any negative value does the same).
    TICKERTAPE_WAIT_FOREVER = -1,
}

/// Writer options (an opaque builder). Create with `tickertape_writer_options_new`, free with
/// `tickertape_writer_options_free`. `tickertape_writer_open` copies them.
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Default)]
pub struct tickertape_writer_options {
    pub(crate) config: WriterConfig,
}

/// Reader options (an opaque builder). Create with `tickertape_reader_options_new`, free with
/// `tickertape_reader_options_free`. The open functions copy them.
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Default)]
pub struct tickertape_reader_options {
    pub(crate) config: ReaderConfig,
}

/// Retention settings (an opaque builder, ADR 0016): limits plus what to do with segments that exceed them. Create
/// with `tickertape_retention_new`, free with `tickertape_retention_free`.
///
/// With no limit set, nothing is removed. There is no default action: set one with
/// `tickertape_retention_set_delete` or `tickertape_retention_set_move_to` before using the settings to remove
/// anything. Only a sealed prefix strictly before the checkpoint segment is ever eligible.
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Default)]
pub struct tickertape_retention {
    pub(crate) policy: Policy,
    pub(crate) action: Option<Action>,
}

impl tickertape_retention {
    pub(crate) fn action(&self) -> Result<&Action, FfiError> {
        self.action.as_ref().ok_or_else(|| {
            FfiError::invalid(
                "retention has no action; call tickertape_retention_set_delete or tickertape_retention_set_move_to",
            )
        })
    }
}

/// Borrows the object behind `ptr`.
///
/// # Safety
///
/// `ptr` must be NULL or a live object of type `T` created by this library, not used concurrently, and not freed
/// while the reference lives.
pub(crate) unsafe fn object<'a, T>(ptr: *mut T, name: &str) -> Result<&'a mut T, FfiError> {
    if ptr.is_null() {
        return Err(FfiError::null(name));
    }
    // SAFETY: not NULL; the caller guarantees a live, unshared object created by `Box::into_raw` here.
    Ok(unsafe { &mut *ptr })
}

/// Frees an object created by `Box::into_raw`.
///
/// # Safety
///
/// `ptr` must be NULL or a live object of type `T` created by this library and not freed before.
unsafe fn free<T>(ptr: *mut T) {
    if !ptr.is_null() {
        // Dropping options cannot panic in practice; never unwind into C if it did.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: the caller passes a pointer from `Box::into_raw` that was not freed before.
            drop(unsafe { Box::from_raw(ptr) })
        }));
    }
}

/// Allocates a default object. Allocation failure aborts the process (Rust's allocator never returns NULL to here),
/// and `T::default` cannot panic for these types, so nothing can unwind out of the `extern "C"` functions below.
fn new<T: Default>() -> *mut T {
    Box::into_raw(Box::<T>::default())
}

/// Creates writer options with the defaults: 128 MiB segments, `TICKERTAPE_RECOVERY_QUARANTINE`, notify on,
/// prefault on, no retention. Never returns NULL: if memory cannot be allocated, the process aborts.
#[unsafe(no_mangle)]
pub extern "C" fn tickertape_writer_options_new() -> *mut tickertape_writer_options {
    new()
}

/// Frees writer options. NULL is ignored.
///
/// # Safety
///
/// `options` must be NULL or a pointer returned by `tickertape_writer_options_new` that was not freed before.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_options_free(options: *mut tickertape_writer_options) {
    // SAFETY: forwarded caller contract.
    unsafe { free(options) }
}

/// Sets the size of new segment files in bytes: a multiple of 65,536 from 65,536 to 2^40 (default 128 MiB). It is
/// checked by `tickertape_writer_open` (`TICKERTAPE_E_INVALID_CONFIG`). Existing segments keep their size.
///
/// # Safety
///
/// `options` must be NULL or live writer options, not used by another thread during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_options_set_segment_size(
    options: *mut tickertape_writer_options,
    bytes: u64,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let o = unsafe { object(options, "options") }?;
        o.config.segment_size = bytes;
        Ok(S::TICKERTAPE_OK)
    })
}

/// Sets the recovery policy, a `tickertape_recovery_policy` value. Other values return
/// `TICKERTAPE_E_INVALID_ARGUMENT`.
///
/// # Safety
///
/// `options` must be NULL or live writer options, not used by another thread during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_options_set_recovery_policy(
    options: *mut tickertape_writer_options,
    policy: i32,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let o = unsafe { object(options, "options") }?;
        o.config.recovery = match policy {
            p if p == tickertape_recovery_policy::TICKERTAPE_RECOVERY_QUARANTINE as i32 => {
                RecoveryPolicy::Quarantine
            }
            p if p == tickertape_recovery_policy::TICKERTAPE_RECOVERY_FAIL as i32 => {
                RecoveryPolicy::Fail
            }
            _ => {
                return Err(FfiError::invalid(format!(
                    "unknown recovery policy {policy}"
                )));
            }
        };
        Ok(S::TICKERTAPE_OK)
    })
}

/// Sets whether the writer wakes blocked readers through its shared-memory wake-up region (ADRs 0017 and 0021).
/// Non-zero enables it. Default: on.
///
/// # Safety
///
/// `options` must be NULL or live writer options, not used by another thread during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_options_set_notify(
    options: *mut tickertape_writer_options,
    enabled: i32,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let o = unsafe { object(options, "options") }?;
        o.config.notify = enabled != 0;
        Ok(S::TICKERTAPE_OK)
    })
}

/// Sets whether the writer does segment work (prefaulting pages, and on macOS preparing the next segment) in
/// background threads it starts. Zero gives a writer that starts no threads. Default: on.
///
/// # Safety
///
/// `options` must be NULL or live writer options, not used by another thread during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_options_set_prefault(
    options: *mut tickertape_writer_options,
    enabled: i32,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let o = unsafe { object(options, "options") }?;
        o.config.prefault = enabled != 0;
        Ok(S::TICKERTAPE_OK)
    })
}

/// Makes the writer apply `retention` after open and after every successful checkpoint (a copy is taken; NULL turns
/// retention off). Failures never fail open, appends or checkpoints; see `tickertape_writer_last_retention_error`.
/// The retention settings must have an action, and a move destination must already exist.
///
/// # Safety
///
/// `options` must be NULL or live writer options, and `retention` NULL or live retention settings, neither used by
/// another thread during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_options_set_retention(
    options: *mut tickertape_writer_options,
    retention: *const tickertape_retention,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let o = unsafe { object(options, "options") }?;
        o.config.retention = if retention.is_null() {
            None
        } else {
            // SAFETY: not NULL; the caller guarantees live retention settings.
            let r = unsafe { &*retention };
            Some((r.policy.clone(), r.action()?.clone()))
        };
        Ok(S::TICKERTAPE_OK)
    })
}

/// Creates reader options with the defaults: not durable-only, notify on, default back-off (100 spins, 10 yields,
/// then sleeps from 10 µs doubling to 1 ms). Never returns NULL: if memory cannot be allocated, the process aborts.
#[unsafe(no_mangle)]
pub extern "C" fn tickertape_reader_options_new() -> *mut tickertape_reader_options {
    new()
}

/// Frees reader options. NULL is ignored.
///
/// # Safety
///
/// `options` must be NULL or a pointer returned by `tickertape_reader_options_new` that was not freed before.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_reader_options_free(options: *mut tickertape_reader_options) {
    // SAFETY: forwarded caller contract.
    unsafe { free(options) }
}

/// Sets whether the reader returns only **durable** records: those with a sequence number below the checkpoint
/// marker's `next_seq` (ADR 0010, crash contract C13). Non-zero enables it. Default: off, in which case the reader
/// returns every **visible** record, including records a power loss can still erase.
///
/// A durable-only reader returns `TICKERTAPE_NONE` (or keeps waiting) at a record that no checkpoint covers yet.
/// Without a valid checkpoint marker it returns nothing; if the writer never checkpoints, it never returns a record.
/// Records it returns survive a power loss only under the crash contract's conditions: storage that honours the sync
/// calls, and no failed checkpoint earlier in the same boot (`docs/crash-contract.md`).
///
/// # Safety
///
/// `options` must be NULL or live reader options, not used by another thread during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_reader_options_set_durable_only(
    options: *mut tickertape_reader_options,
    enabled: i32,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let o = unsafe { object(options, "options") }?;
        o.config.durable_only = enabled != 0;
        Ok(S::TICKERTAPE_OK)
    })
}

/// Sets whether a waiting reader blocks on the writer's wake-up region instead of sleeping, so a new record wakes it
/// at once (ADRs 0017 and 0021). Non-zero enables it. Default: on. Readers fall back to sleeping where the region
/// is unavailable.
///
/// # Safety
///
/// `options` must be NULL or live reader options, not used by another thread during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_reader_options_set_notify(
    options: *mut tickertape_reader_options,
    enabled: i32,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let o = unsafe { object(options, "options") }?;
        o.config.notify = enabled != 0;
        Ok(S::TICKERTAPE_OK)
    })
}

/// Sets how a reader waits when no record is available: `spins` busy polls, then `yields` polls that yield the CPU,
/// then sleeps that start at `min_sleep_ns` and double up to `max_sleep_ns`. `min_sleep_ns` must not exceed
/// `max_sleep_ns`.
///
/// # Safety
///
/// `options` must be NULL or live reader options, not used by another thread during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_reader_options_set_backoff(
    options: *mut tickertape_reader_options,
    spins: u32,
    yields: u32,
    min_sleep_ns: u64,
    max_sleep_ns: u64,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let o = unsafe { object(options, "options") }?;
        if min_sleep_ns > max_sleep_ns {
            return Err(FfiError::invalid(
                "min_sleep_ns must not exceed max_sleep_ns",
            ));
        }
        let mut backoff = Backoff::default();
        backoff.spins = spins;
        backoff.yields = yields;
        backoff.min_sleep = Duration::from_nanos(min_sleep_ns);
        backoff.max_sleep = Duration::from_nanos(max_sleep_ns);
        o.config.backoff = backoff;
        Ok(S::TICKERTAPE_OK)
    })
}

/// Makes a waiting reader spin without ever yielding or sleeping: lowest latency, one core at 100 %.
///
/// # Safety
///
/// `options` must be NULL or live reader options, not used by another thread during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_reader_options_set_busy_spin(
    options: *mut tickertape_reader_options,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let o = unsafe { object(options, "options") }?;
        o.config.backoff = Backoff::busy_spin();
        Ok(S::TICKERTAPE_OK)
    })
}

/// Creates retention settings with no limits and no action. Never returns NULL: if memory cannot be allocated, the
/// process aborts.
#[unsafe(no_mangle)]
pub extern "C" fn tickertape_retention_new() -> *mut tickertape_retention {
    new()
}

/// Frees retention settings. NULL is ignored.
///
/// # Safety
///
/// `retention` must be NULL or a pointer returned by `tickertape_retention_new` that was not freed before.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_retention_free(retention: *mut tickertape_retention) {
    // SAFETY: forwarded caller contract.
    unsafe { free(retention) }
}

/// Keeps at most `count` segment files in total. Zero still keeps the checkpoint segment and everything after it.
///
/// # Safety
///
/// `retention` must be NULL or live retention settings, not used by another thread during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_retention_set_max_segments(
    retention: *mut tickertape_retention,
    count: u64,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let r = unsafe { object(retention, "retention") }?;
        let count = usize::try_from(count).unwrap_or(usize::MAX);
        r.policy = r.policy.clone().max_segments(count);
        Ok(S::TICKERTAPE_OK)
    })
}

/// Keeps the total size of segment files at or below `bytes` (file sizes, not occupied bytes).
///
/// # Safety
///
/// `retention` must be NULL or live retention settings, not used by another thread during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_retention_set_max_bytes(
    retention: *mut tickertape_retention,
    bytes: u64,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let r = unsafe { object(retention, "retention") }?;
        r.policy = r.policy.clone().max_bytes(bytes);
        Ok(S::TICKERTAPE_OK)
    })
}

/// Removes segments sealed more than `age_ns` nanoseconds ago (a segment's seal time is its successor's creation
/// time, by the writer's wall clock).
///
/// # Safety
///
/// `retention` must be NULL or live retention settings, not used by another thread during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_retention_set_max_age_ns(
    retention: *mut tickertape_retention,
    age_ns: u64,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let r = unsafe { object(retention, "retention") }?;
        r.policy = r.policy.clone().max_age(Duration::from_nanos(age_ns));
        Ok(S::TICKERTAPE_OK)
    })
}

/// Makes retention **permanently delete** eligible segments.
///
/// # Safety
///
/// `retention` must be NULL or live retention settings, not used by another thread during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_retention_set_delete(
    retention: *mut tickertape_retention,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let r = unsafe { object(retention, "retention") }?;
        r.action = Some(Action::Delete);
        Ok(S::TICKERTAPE_OK)
    })
}

/// Makes retention move eligible segments into the existing directory `dir`, keeping their names. A move never
/// replaces an existing file there (it fails instead). `dir` is copied.
///
/// # Safety
///
/// `retention` must be NULL or live retention settings, not used by another thread during the call. `dir` must be
/// NULL or a NUL-terminated string that stays unmodified until the call returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_retention_set_move_to(
    retention: *mut tickertape_retention,
    dir: *const c_char,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let r = unsafe { object(retention, "retention") }?;
        // SAFETY: the caller guarantees `dir` is NULL or NUL-terminated, and unmodified during the call.
        let dir = unsafe { args::path(dir, "dir") }?;
        r.action = Some(Action::MoveTo(dir.to_path_buf()));
        Ok(S::TICKERTAPE_OK)
    })
}
