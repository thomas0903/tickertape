//! Opaque option builders: writer options, reader options and retention settings.

use std::ffi::c_char;
use std::time::Duration;

use tickertape::retention::{Action, Policy};
use tickertape::{Backoff, HelperCpus, ReaderConfig, RecoveryPolicy, WriterConfig};

use crate::args;
use crate::status::{FfiError, guard, tickertape_status, tickertape_status_code as S};

/// What a writer does on open when it finds corruption after the checkpoint (`docs/format.md` → Recovery;
/// `docs/crash-contract.md` C1, C8 and C11). After a process crash in an otherwise intact journal, an uncommitted
/// partial record is usually zeroed in place under either policy; the exception, a torn record that recovery cannot
/// tell from damage, is in C1.
#[allow(non_camel_case_types)]
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum tickertape_recovery_policy {
    /// Copy the invalid bytes to `quarantine/` in the journal directory (a disconnected segment is moved there),
    /// then continue after the last valid record. No segment and no repaired tail is deleted; bytes that recovery
    /// zeroes in place are not kept (C8). The default.
    TICKERTAPE_RECOVERY_QUARANTINE = 0,
    /// Fail the open with `TICKERTAPE_E_CORRUPT`: the bytes at the corruption it reports, and everything after them
    /// in that segment, are left untouched and no file is moved. The open is not read-only: C11 lists what it may
    /// still change.
    TICKERTAPE_RECOVERY_FAIL = 1,
}

/// Where the writer's helper threads run, for `tickertape_writer_options_set_helper_cpus` (`HelperCpus` in Rust,
/// ADR 0038 → Helper threads and CPU affinity). The helper threads are the checkpoint thread
/// (`tickertape-checkpoint`), the prefault and write-back helpers (`tickertape-prefault`, `tickertape-writeback`) and
/// the spare-segment thread (`tickertape-spare`). **Linux:** each helper sets its affinity when it starts, and the
/// kernel intersects the mask with the CPUs the process's cpuset allows. **macOS:** no effect.
#[allow(non_camel_case_types)]
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum tickertape_helper_cpus {
    /// Every CPU the process's cpuset allows, except the CPUs the kernel lists as isolated
    /// (`/sys/devices/system/cpu/isolated`), unless that would leave none. Not limited by a mask set with `taskset` or
    /// `sched_setaffinity` on the process or the calling thread (such a mask belongs to each thread). The default.
    TICKERTAPE_HELPER_CPUS_PROCESS = 0,
    /// Keep the mask inherited from the thread that creates the helper (the writer's thread).
    TICKERTAPE_HELPER_CPUS_INHERITED = 1,
    /// The CPUs listed (numbers as the kernel counts them, below 8,192), intersected with the process's cpuset. If the
    /// kernel refuses the mask (no listed CPU is allowed, or the list is empty), the helper uses
    /// `TICKERTAPE_HELPER_CPUS_PROCESS` instead.
    TICKERTAPE_HELPER_CPUS_LIST = 2,
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
/// prefault on, early write-back on, helper threads on `TICKERTAPE_HELPER_CPUS_PROCESS`, no retention. Never returns NULL: if memory cannot be allocated, the process aborts.
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

/// Sets whether the writer does segment work (prefaulting pages) in background threads it starts. Zero gives a writer
/// that starts no threads, except the checkpoint thread that `tickertape_writer_request_checkpoint` starts; it also turns
/// off early write-back (`tickertape_writer_options_set_early_writeback`) and the spare segment
/// (`tickertape_writer_options_set_spare_segment`). Default: on.
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

/// Sets whether the writer starts the kernel's write-back of the pages it has finished with as it goes, so that a
/// checkpoint finds little left to write (ADR 0038 → Early write-back). Non-zero enables it. Default: on.
///
/// **Linux:** applies only with prefault on (`tickertape_writer_options_set_prefault`), the switch for the writer's
/// helper threads. A helper thread (`tickertape-writeback`) calls `sync_file_range(SYNC_FILE_RANGE_WRITE)` each time
/// the write position has moved 1 MiB, up to the 128 KiB boundary below it (not into the page-cache folio the writer is
/// still filling; ADR 0058, from 1.1), and over the rest of a segment when the writer rolls past it. A checkpoint may
/// therefore find up to 128 KiB more to write. That call only starts writing: what is durable when is unchanged, and
/// only a checkpoint promises it (`docs/crash-contract.md`). If the call fails with an I/O error, no later checkpoint
/// of this writer succeeds: the error is returned once, by the next checkpoint, `tickertape_writer_request_checkpoint`,
/// `tickertape_writer_checkpoint_progress` or `tickertape_writer_close`, and `TICKERTAPE_E_DURABILITY_LOST` after that.
/// If the filesystem does not support the call, the helper stops. **macOS:** no effect.
///
/// # Safety
///
/// `options` must be NULL or live writer options, not used by another thread during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_options_set_early_writeback(
    options: *mut tickertape_writer_options,
    enabled: i32,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let o = unsafe { object(options, "options") }?;
        o.config.early_writeback = enabled != 0;
        Ok(S::TICKERTAPE_OK)
    })
}

/// Sets whether the writer prepares the next segment in a background thread (`tickertape-spare`) once the active one is
/// half full, so that a roll writes its header and links it instead of creating a file (ADR 0022; on Linux ADR 0049).
/// Non-zero enables it. Default: on on macOS, **off on Linux**. Applies only with prefault on
/// (`tickertape_writer_options_set_prefault`).
///
/// It reserves one more segment's worth of disk space from the time the active segment is half full, and holds one more
/// file descriptor and segment mapping. **Linux:** the thread lives as long as the writer; a roll that finds the
/// segment not ready creates one itself, and waits for the prepared one only if that fails for lack of space; closing
/// the writer waits for one being prepared. Whether it helps depends on the load and the disk: measure first (ADR
/// 0049). Added in 1.1.
///
/// # Safety
///
/// `options` must be NULL or live writer options, not used by another thread during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_options_set_spare_segment(
    options: *mut tickertape_writer_options,
    enabled: i32,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let o = unsafe { object(options, "options") }?;
        o.config.spare_segment = enabled != 0;
        Ok(S::TICKERTAPE_OK)
    })
}

/// Sets where the writer's helper threads run: `placement` is a `tickertape_helper_cpus` value. With
/// `TICKERTAPE_HELPER_CPUS_LIST`, `cpus` points to `cpu_count` CPU numbers (copied; `cpus` may be NULL when
/// `cpu_count` is 0); with the other values, `cpu_count` must be 0. Default: `TICKERTAPE_HELPER_CPUS_PROCESS`.
///
/// Errors: `TICKERTAPE_E_INVALID_ARGUMENT` for an unknown `placement`, CPUs given with a placement other than
/// `TICKERTAPE_HELPER_CPUS_LIST`, `cpus` NULL with `cpu_count` > 0, or a `cpu_count` too large. The options are then
/// unchanged.
///
/// # Safety
///
/// `options` must be NULL or live writer options, not used by another thread during the call. `cpus` must be NULL or
/// point to `cpu_count` readable `uint32_t` values that stay unmodified until the call returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_options_set_helper_cpus(
    options: *mut tickertape_writer_options,
    placement: i32,
    cpus: *const u32,
    cpu_count: usize,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let o = unsafe { object(options, "options") }?;
        let list = tickertape_helper_cpus::TICKERTAPE_HELPER_CPUS_LIST as i32;
        if placement != list && cpu_count != 0 {
            return Err(FfiError::invalid(
                "cpus are given only with TICKERTAPE_HELPER_CPUS_LIST; cpu_count must be 0",
            ));
        }
        o.config.helper_cpus = match placement {
            p if p == tickertape_helper_cpus::TICKERTAPE_HELPER_CPUS_PROCESS as i32 => {
                HelperCpus::Process
            }
            p if p == tickertape_helper_cpus::TICKERTAPE_HELPER_CPUS_INHERITED as i32 => {
                HelperCpus::Inherited
            }
            p if p == list => {
                // SAFETY: forwarded caller contract.
                let cpus = unsafe { args::slice(cpus, cpu_count, "cpus", "cpu_count") }?;
                HelperCpus::List(cpus.iter().map(|&cpu| cpu as usize).collect())
            }
            _ => {
                return Err(FfiError::invalid(format!(
                    "unknown helper CPU placement {placement}"
                )));
            }
        };
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
/// While it waits for a checkpoint it is woken by checkpoints, not by appends (ADR 0053; beside a 1.0.0 writer it polls
/// the marker at the maximum sleep instead).
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

/// Sets whether `tickertape_reader_seek` may start at a record named by a seek index, one of the optional files that
/// `tickertape index` writes next to the journal (ADR 0045, crash contract C15). Non-zero enables it. Default: off.
///
/// A seek into a sealed segment that has an index then validates the record it lands on and every record from there,
/// instead of every record from the segment's start. Records before that are not read, so damage there no longer stops
/// the reader. Without an index, or if anything about it is in doubt, the seek does what it does without the option,
/// and it never fails because of an index. A durable-only reader and a mapped one
/// (`tickertape_reader_open_mapped_unsafe`) use an index only for a segment before the one the checkpoint marker
/// names. Do not enable this where someone who may not write the segments can write `seek-index/`: a forged index can
/// make the reader return bytes from inside a payload that form a valid record. Added in 1.1.
///
/// # Safety
///
/// `options` must be NULL or live reader options, not used by another thread during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_reader_options_set_seek_index(
    options: *mut tickertape_reader_options,
    enabled: i32,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let o = unsafe { object(options, "options") }?;
        o.config.seek_index = enabled != 0;
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
