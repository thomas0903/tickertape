//! The writer handle.

use std::ffi::{c_char, c_void};
use std::panic::{self, AssertUnwindSafe};

use tickertape::{NewRecord, Writer, WriterConfig};

use crate::args;
use crate::options::tickertape_writer_options;
use crate::status::{FfiError, Handle, guard, tickertape_status, tickertape_status_code as S};

/// A journal writer (an opaque handle). Open with `tickertape_writer_open`, close with `tickertape_writer_close`.
///
/// Only one writer can have a journal open at a time (across processes). A writer handle may move to another thread
/// between calls but must never be used by two threads at once.
#[allow(non_camel_case_types)]
#[derive(Debug)]
pub struct tickertape_writer(Handle<Writer>);

/// One record for `tickertape_writer_append_batch`. The layout is frozen for format version 1.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct tickertape_new_record {
    /// The payload bytes. May be NULL when `payload_len` is 0.
    pub payload: *const u8,
    /// Number of payload bytes.
    pub payload_len: usize,
    /// Caller-supplied timestamp, by convention nanoseconds since the Unix epoch (for example the venue's event
    /// time). 0 means "not set". Never interpreted by the library.
    pub venue_ts_ns: u64,
    /// Caller-supplied timestamp, by convention nanoseconds since the Unix epoch (for example the local receive
    /// time). 0 means "not set". Never interpreted by the library.
    pub local_ts_ns: u64,
    /// Caller-defined event type or schema identifier. Opaque to the library.
    pub schema_id: u32,
    /// Must be 0.
    pub reserved: u32,
}

/// Fills a payload buffer in place for `tickertape_writer_append_with`.
///
/// Called once, on the calling thread, with `buf` pointing to `len` zeroed bytes inside the mapped segment. Write
/// the payload into it and return 0 to commit the record, or return non-zero to abort the append. The callback must
/// not keep `buf` after it returns, must not call any function on the writer that invoked it (that returns
/// `TICKERTAPE_E_BUSY`), and must not unwind or `longjmp` out (C++: catch every exception inside).
#[allow(non_camel_case_types)]
pub type tickertape_fill_fn =
    Option<unsafe extern "C" fn(user_data: *mut c_void, buf: *mut u8, len: usize) -> i32>;

/// The unwind payload that aborts an `append_with` whose callback returned non-zero.
struct FillAborted;

/// Borrows the writer behind a handle pointer.
///
/// # Safety
///
/// `ptr` must be NULL or a live handle from `tickertape_writer_open` that stays open during the borrow.
unsafe fn handle<'a>(ptr: *mut tickertape_writer) -> Result<&'a Handle<Writer>, FfiError> {
    if ptr.is_null() {
        return Err(FfiError::null("writer"));
    }
    // SAFETY: not NULL; the caller guarantees a live handle. Only shared references to the handle are created; the
    // state inside is reached through `Handle::with`.
    Ok(unsafe { &(*ptr).0 })
}

/// Opens the journal in directory `dir` for writing, creating the directory and the first segment if needed, and
/// stores the new handle in `*writer_out`.
///
/// Takes the journal's lock, then recovers the journal (`docs/format.md` → Recovery): it validates everything after
/// the last checkpoint, repairs or quarantines what is invalid, and syncs every repair before returning. `options`
/// may be NULL for the defaults; they are copied. On failure `*writer_out` is set to NULL.
///
/// Errors: `TICKERTAPE_E_LOCKED` if another writer has the journal open; `TICKERTAPE_E_INVALID_CONFIG`;
/// `TICKERTAPE_E_CORRUPT` if the journal is damaged in a way recovery does not repair, or the tail is corrupt and the
/// policy is `TICKERTAPE_RECOVERY_FAIL`; `TICKERTAPE_E_UNSUPPORTED_VERSION`; `TICKERTAPE_E_IO`.
///
/// # Safety
///
/// `dir` must be NULL or a NUL-terminated string. `options` must be NULL or live writer options. `writer_out` must
/// be NULL or valid for writing a pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_open(
    dir: *const c_char,
    options: *const tickertape_writer_options,
    writer_out: *mut *mut tickertape_writer,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        unsafe { args::write(writer_out, std::ptr::null_mut(), "writer_out") }?;
        // SAFETY: forwarded caller contract.
        let dir = unsafe { args::path(dir, "dir") }?;
        let config = if options.is_null() {
            WriterConfig::default()
        } else {
            // SAFETY: not NULL; the caller guarantees live writer options.
            unsafe { &*options }.config.clone()
        };
        let writer = Writer::open(dir, config)?;
        let ptr = Box::into_raw(Box::new(tickertape_writer(Handle::new(writer))));
        // SAFETY: checked above that `writer_out` is writable.
        unsafe { writer_out.write(ptr) };
        Ok(S::TICKERTAPE_OK)
    })
}

/// Closes a writer and frees the handle. NULL is ignored.
///
/// Closing does **not** checkpoint: every appended record stays visible to readers and survives the process exiting,
/// but only records covered by a successful `tickertape_writer_checkpoint` are durable. Returns `TICKERTAPE_E_BUSY`
/// (and closes nothing) if called from a fill callback of the same writer.
///
/// # Safety
///
/// `writer` must be NULL or a live handle from `tickertape_writer_open`. After this returns `TICKERTAPE_OK` the handle
/// is invalid; using or closing it again is undefined behaviour.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_close(
    writer: *mut tickertape_writer,
) -> tickertape_status {
    guard(|| {
        if writer.is_null() {
            return Ok(S::TICKERTAPE_OK);
        }
        // SAFETY: forwarded caller contract.
        if unsafe { handle(writer) }?.is_busy() {
            return Err(FfiError::new(
                S::TICKERTAPE_E_BUSY,
                "cannot close a writer from inside one of its own calls",
            ));
        }
        // SAFETY: a live handle from `Box::into_raw` in `tickertape_writer_open`, not inside a call (checked above);
        // the caller gives up the pointer.
        drop(unsafe { Box::from_raw(writer) });
        Ok(S::TICKERTAPE_OK)
    })
}

/// Appends one record and stores its sequence number in `*seq_out` (which may be NULL).
///
/// When this returns `TICKERTAPE_OK` the record is **visible**: readers in any process on this host can read it, and
/// it survives this process dying. It is **not durable** (it can be lost in a power loss or OS crash) until a later
/// `tickertape_writer_checkpoint` succeeds. If the record does not fit in the current segment, the writer first rolls
/// to a new segment (slower: it creates and syncs a file).
///
/// Errors: `TICKERTAPE_E_INVALID_ARGUMENT` (`payload` NULL with `payload_len` > 0); `TICKERTAPE_E_RECORD_TOO_LARGE`;
/// `TICKERTAPE_E_SEQUENCE_EXHAUSTED`; `TICKERTAPE_E_STORAGE_FULL` or `TICKERTAPE_E_IO` if a roll fails (the journal
/// is unchanged and the call can be retried).
///
/// # Safety
///
/// `writer` must be NULL or a live writer handle. `payload` must point to `payload_len` readable bytes (or be
/// anything, including NULL, when `payload_len` is 0). `seq_out` must be NULL or valid for writing a `uint64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_append(
    writer: *mut tickertape_writer,
    schema_id: u32,
    venue_ts_ns: u64,
    local_ts_ns: u64,
    payload: *const u8,
    payload_len: usize,
    seq_out: *mut u64,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let payload = unsafe { args::bytes(payload, payload_len, "payload") }?;
        let record = NewRecord {
            schema_id,
            venue_ts_ns,
            local_ts_ns,
            payload,
        };
        // SAFETY: forwarded caller contract.
        let seq = unsafe { handle(writer) }?.with(|w| Ok(w.append(&record)?))?;
        // SAFETY: forwarded caller contract.
        unsafe { args::write_optional(seq_out, seq) };
        Ok(S::TICKERTAPE_OK)
    })
}

/// Appends one record whose payload `fill` writes in place, without a copy, and stores its sequence number in
/// `*seq_out` (which may be NULL).
///
/// `fill(user_data, buf, payload_len)` is called once with a zeroed buffer of exactly `payload_len` bytes inside the
/// mapped segment; no reader can see it until `fill` returns 0 and the record is committed. Durability is as for
/// `tickertape_writer_append`: **visible** on return, **durable** only after a later successful checkpoint.
///
/// If `fill` returns non-zero, nothing is committed and this returns `TICKERTAPE_E_ABORTED`: the writer zeroes the
/// buffer and syncs that zeroing to storage before returning (`docs/format.md` → Abandoned writes), so aborting is
/// slow; use it only for exceptional cases. If that sync fails, the writer refuses further appends and checkpoints
/// (reopen the journal to recover).
///
/// Errors: as for `tickertape_writer_append`, plus `TICKERTAPE_E_ABORTED`, and `TICKERTAPE_E_INVALID_ARGUMENT` if
/// `fill` is NULL. `fill` is not called when the append fails before it.
///
/// # Safety
///
/// `writer` must be NULL or a live writer handle. `fill` must be NULL or a function that follows the
/// `tickertape_fill_fn` contract; `user_data` is passed to it unchanged. `seq_out` must be NULL or valid for writing
/// a `uint64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_append_with(
    writer: *mut tickertape_writer,
    schema_id: u32,
    venue_ts_ns: u64,
    local_ts_ns: u64,
    payload_len: usize,
    fill: tickertape_fill_fn,
    user_data: *mut c_void,
    seq_out: *mut u64,
) -> tickertape_status {
    guard(|| {
        let fill = fill.ok_or_else(|| FfiError::null("fill"))?;
        let record = NewRecord {
            schema_id,
            venue_ts_ns,
            local_ts_ns,
            payload: &[],
        };
        // SAFETY: forwarded caller contract.
        let seq = unsafe { handle(writer) }?.with(|w| {
            let appended = panic::catch_unwind(AssertUnwindSafe(|| {
                w.append_with(&record, payload_len, |buf| {
                    // SAFETY: `fill` follows the callback contract (caller guarantee); `buf` is valid for
                    // `buf.len()` writable bytes for the duration of the call.
                    let rc = unsafe { fill(user_data, buf.as_mut_ptr(), buf.len()) };
                    if rc != 0 {
                        // The core treats an unwind out of the closure as an abandoned write: it zeroes and syncs
                        // the bytes, then continues the unwind, which is caught just below.
                        panic::resume_unwind(Box::new(FillAborted));
                    }
                })
            }));
            match appended {
                Ok(result) => Ok(result?),
                Err(payload) if payload.is::<FillAborted>() => Err(FfiError::new(
                    S::TICKERTAPE_E_ABORTED,
                    "the fill callback returned non-zero; nothing was appended",
                )),
                Err(payload) => panic::resume_unwind(payload),
            }
        })?;
        // SAFETY: forwarded caller contract.
        unsafe { args::write_optional(seq_out, seq) };
        Ok(S::TICKERTAPE_OK)
    })
}

/// Appends `count` records in order, stopping at the first failure.
///
/// Every record is checked first (`payload` NULL with `payload_len` > 0, `reserved` not 0): an argument error
/// returns `TICKERTAPE_E_INVALID_ARGUMENT` and appends nothing. Then the records are appended one by one, as with
/// `tickertape_writer_append`. `*appended_out` is always set to the number of records appended, and `*first_seq_out`
/// (which may be NULL) to the sequence number of the first; they are consecutive. On an error, the records before
/// the failing one (index `*appended_out`) **are appended** and stay appended: retry only from the failing record.
///
/// Durability is as for `tickertape_writer_append`: **visible** on return, **durable** only after a later
/// successful checkpoint. Errors: as for `tickertape_writer_append`.
///
/// # Safety
///
/// `writer` must be NULL or a live writer handle. `records` must point to `count` readable
/// `tickertape_new_record`s whose payloads follow the rule for `tickertape_writer_append` (it may be NULL when `count`
/// is 0). `appended_out` must be NULL or valid for writing a `size_t`, and `first_seq_out` NULL or valid for writing
/// a `uint64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_append_batch(
    writer: *mut tickertape_writer,
    records: *const tickertape_new_record,
    count: usize,
    appended_out: *mut usize,
    first_seq_out: *mut u64,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        unsafe { args::write(appended_out, 0, "appended_out") }?;
        if count > 0 && records.is_null() {
            return Err(FfiError::invalid(
                "records must not be NULL when count is not 0",
            ));
        }
        let records: &[tickertape_new_record] = if count == 0 {
            &[]
        } else {
            // SAFETY: not NULL; the caller guarantees `count` readable records.
            unsafe { std::slice::from_raw_parts(records, count) }
        };
        let mut new_records = Vec::with_capacity(records.len());
        for (index, r) in records.iter().enumerate() {
            if r.reserved != 0 {
                return Err(FfiError::invalid(format!(
                    "records[{index}].reserved must be 0"
                )));
            }
            // SAFETY: the caller guarantees each payload is readable for its length.
            let payload =
                unsafe { args::bytes(r.payload, r.payload_len, "payload") }.map_err(|_| {
                    FfiError::invalid(format!(
                        "records[{index}].payload must not be NULL when payload_len is not 0"
                    ))
                })?;
            new_records.push(NewRecord {
                schema_id: r.schema_id,
                venue_ts_ns: r.venue_ts_ns,
                local_ts_ns: r.local_ts_ns,
                payload,
            });
        }
        // SAFETY: forwarded caller contract.
        let (appended, first, result) = unsafe { handle(writer) }?.with(|w| {
            let first = w.next_seq();
            let mut appended = 0usize;
            for record in &new_records {
                if let Err(error) = w.append(record) {
                    return Ok((appended, first, Err(FfiError::from(error))));
                }
                appended += 1;
            }
            Ok((appended, first, Ok(())))
        })?;
        // SAFETY: checked above that `appended_out` is writable; `first_seq_out` per the caller contract.
        unsafe {
            appended_out.write(appended);
            args::write_optional(first_seq_out, first);
        }
        result?;
        Ok(S::TICKERTAPE_OK)
    })
}

/// Makes every record appended so far **durable**, using the platform's sync calls (`docs/crash-contract.md` → What
/// `checkpoint()` does), and blocks until they finish. Configured retention then runs; its failures are reported by
/// `tickertape_writer_last_retention_error` and do not change this result.
///
/// Errors: `TICKERTAPE_E_IO` or `TICKERTAPE_E_STORAGE_FULL` if a sync fails. The writer is then poisoned: every later
/// checkpoint returns `TICKERTAPE_E_DURABILITY_LOST`, because a failed sync can drop data that a retry would not
/// rewrite. Appends keep working. Treat a checkpoint error as an incident: records written before it may not be
/// durable even if a later writer's checkpoint succeeds.
///
/// # Safety
///
/// `writer` must be NULL or a live writer handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_checkpoint(
    writer: *mut tickertape_writer,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        unsafe { handle(writer) }?.with(|w| Ok(w.checkpoint()?))?;
        Ok(S::TICKERTAPE_OK)
    })
}

/// Stores the sequence number the next append will get in `*seq_out`.
///
/// # Safety
///
/// `writer` must be NULL or a live writer handle. `seq_out` must be NULL or valid for writing a `uint64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_next_seq(
    writer: *mut tickertape_writer,
    seq_out: *mut u64,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let seq = unsafe { handle(writer) }?.with(|w| Ok(w.next_seq()))?;
        // SAFETY: forwarded caller contract.
        unsafe { args::write(seq_out, seq, "seq_out") }?;
        Ok(S::TICKERTAPE_OK)
    })
}

/// Reports the most recent retention error since the writer was opened: returns `TICKERTAPE_OK` if there was none,
/// otherwise that error's (negative) status, and sets the thread's last error to it. The writer itself is unaffected:
/// retention errors never fail open, appends or checkpoints, and a later successful run does not clear the error.
///
/// # Safety
///
/// `writer` must be NULL or a live writer handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_last_retention_error(
    writer: *mut tickertape_writer,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        unsafe { handle(writer) }?.with(|w| match w.last_retention_error() {
            None => Ok(S::TICKERTAPE_OK),
            Some(error) => Err(FfiError::from(error)),
        })
    })
}
