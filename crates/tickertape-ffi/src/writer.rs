//! The writer handle.

use std::ffi::{c_char, c_void};

use tickertape::{Error, NewRecord, Writer, WriterConfig};

use crate::args::{self, Region};
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

// The frozen layout (ADR 0028 → Versioning); tests/c/test_api.c asserts the same values with offsetof and sizeof.
#[cfg(target_pointer_width = "64")]
#[allow(dead_code)]
fn new_record_layout_is_frozen() {
    // Inside a function so that cbindgen does not parse it; the constant is evaluated all the same.
    const _: () = {
        use std::mem::{align_of, offset_of, size_of};
        assert!(
            size_of::<tickertape_new_record>() == 40 && align_of::<tickertape_new_record>() == 8
        );
        assert!(offset_of!(tickertape_new_record, payload) == 0);
        assert!(offset_of!(tickertape_new_record, payload_len) == 8);
        assert!(offset_of!(tickertape_new_record, venue_ts_ns) == 16);
        assert!(offset_of!(tickertape_new_record, local_ts_ns) == 24);
        assert!(offset_of!(tickertape_new_record, schema_id) == 32);
        assert!(offset_of!(tickertape_new_record, reserved) == 36);
    };
}

/// Fills a payload buffer in place for `tickertape_writer_append_with`.
///
/// Called once, on the calling thread, with `buf` pointing to `len` zeroed bytes inside the mapped segment (`buf` is
/// NULL when `len` is 0). Write the payload into it and return 0 to commit the record, or return non-zero to abort
/// the append. The callback must not keep `buf` after it returns, and must not call any function on the writer that
/// invoked it (that returns `TICKERTAPE_E_BUSY`; closing it from here is refused the same way).
///
/// The callback must return normally: catch every exception inside it and return non-zero instead. An exception
/// that escapes it is unsupported: Rust leaves it unspecified whether such a foreign exception terminates the process
/// (it did on the platforms tested) or is caught as an opaque error, with the exception lost. Leaving it with
/// `longjmp` is undefined behaviour.
#[allow(non_camel_case_types)]
pub type tickertape_fill_fn =
    Option<unsafe extern "C-unwind" fn(user_data: *mut c_void, buf: *mut u8, len: usize) -> i32>;

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
    // state inside is reached through `Handle::with` and `Handle::with_ref`.
    Ok(unsafe { &(*ptr).0 })
}

/// Converts an error from an append or checkpoint, saying what happened if an aborted fill's clean-up failed
/// earlier (the core reports that as `DurabilityLost` on checkpoints, which otherwise means a failed checkpoint).
fn writer_error(writer: &Writer, error: Error) -> FfiError {
    match (&error, writer.abandoned_write_error()) {
        (Error::DurabilityLost, Some(cause)) => FfiError::from(&error).with_message(format!(
            "durability can no longer be guaranteed for this writer: the clean-up after an aborted fill callback \
             failed ({cause}); close the writer and reopen the journal"
        )),
        _ => FfiError::from(error),
    }
}

/// Opens the journal in directory `dir` for writing, creating the directory and the first segment if needed, and
/// stores the new handle in `*writer_out`.
///
/// Takes the journal's lock, then recovers the journal (`docs/format.md` → Recovery): it validates everything after
/// the last checkpoint, repairs or quarantines what is invalid, and syncs every repair before returning. `options`
/// may be NULL for the defaults; they are copied. On failure `*writer_out` is set to NULL. If memory for the handle
/// cannot be allocated, the process aborts (there is no out-of-memory status).
///
/// Errors: `TICKERTAPE_E_LOCKED` if another writer has the journal open; `TICKERTAPE_E_INVALID_CONFIG`;
/// `TICKERTAPE_E_CORRUPT` if the journal is damaged in a way recovery does not repair, or the tail is corrupt and the
/// policy is `TICKERTAPE_RECOVERY_FAIL`; `TICKERTAPE_E_UNSUPPORTED_VERSION`; `TICKERTAPE_E_IO`.
///
/// # Safety
///
/// `dir` must be NULL or a NUL-terminated string. `options` must be NULL or live writer options. `writer_out` must
/// be NULL or valid for writing a pointer, and must not overlap `dir`. Inputs must stay unmodified until the call
/// returns.
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
/// (and closes nothing; the handle stays valid) if called from a fill callback of the same writer.
///
/// # Safety
///
/// `writer` must be NULL or a live handle from `tickertape_writer_open`. After this returns any status other than
/// `TICKERTAPE_E_BUSY` (including `TICKERTAPE_E_PANIC`), the handle is freed: using or closing it again is undefined
/// behaviour, so never retry a close that failed with another status.
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
/// Errors: `TICKERTAPE_E_INVALID_ARGUMENT` (`payload` NULL with `payload_len` > 0, or `seq_out` overlapping the
/// payload); `TICKERTAPE_E_RECORD_TOO_LARGE`; `TICKERTAPE_E_SEQUENCE_EXHAUSTED`; `TICKERTAPE_E_STORAGE_FULL` or
/// `TICKERTAPE_E_IO` if a roll fails (the journal is unchanged and the call can be retried); `TICKERTAPE_E_IO`, whatever
/// the OS error, for every append after a failed clean-up of an aborted fill (see `tickertape_writer_append_with`;
/// retrying does not help).
///
/// # Safety
///
/// `writer` must be NULL or a live writer handle. `payload` must point to `payload_len` readable bytes (or be
/// anything, including NULL, when `payload_len` is 0) that stay unmodified until the call returns. `seq_out` must be
/// NULL or valid for writing a `uint64_t`, and must not overlap the payload.
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
        args::disjoint(&[
            Region::of(payload.as_ptr(), payload.len(), "payload"),
            Region::of(seq_out, 1, "seq_out"),
        ])?;
        let record = NewRecord {
            schema_id,
            venue_ts_ns,
            local_ts_ns,
            payload,
        };
        // SAFETY: forwarded caller contract.
        let seq = unsafe { handle(writer) }?
            .with(|w| w.append(&record).map_err(|e| writer_error(w, e)))?;
        // SAFETY: forwarded caller contract.
        unsafe { args::write_optional(seq_out, seq) };
        Ok(S::TICKERTAPE_OK)
    })
}

/// Appends one record whose payload `fill` writes in place, without a copy, and stores its sequence number in
/// `*seq_out` (which may be NULL).
///
/// `fill(user_data, buf, payload_len)` is called once with a zeroed buffer of exactly `payload_len` bytes inside the
/// mapped segment (`buf` NULL when `payload_len` is 0); no reader can see it until `fill` returns 0 and the record is
/// committed. Durability is as for `tickertape_writer_append`: **visible** on return, **durable** only after a later
/// successful checkpoint.
///
/// If `fill` returns non-zero, nothing is committed and this returns `TICKERTAPE_E_ABORTED`: the writer zeroes the
/// buffer and syncs that zeroing to storage before returning (`docs/format.md` → Abandoned writes), so aborting is
/// slow; use it only for exceptional cases. Aborting does not unwind inside the library.
///
/// If that sync fails, this call returns `TICKERTAPE_E_IO` instead of `TICKERTAPE_E_ABORTED`, with the OS error in
/// `tickertape_last_error_os_code` and a message saying the clean-up failed. The code is `TICKERTAPE_E_IO` even when
/// the OS error is `ENOSPC` or `EDQUOT`, because freeing space does not help. Treat it as a storage incident, like a
/// failed checkpoint: the writer refuses every further append (`TICKERTAPE_E_IO`, the same OS error, a message that
/// says why) and checkpoint (`TICKERTAPE_E_DURABILITY_LOST`), and the durability of everything appended since the
/// last successful checkpoint is unknown. Closing the writer and reopening the journal gives a writer that accepts
/// appends again, but no guarantee about that tail across a power loss: the zeroing may have reached only the page
/// cache, and recovery cannot tell whether the disk still holds the stale bytes (`docs/crash-contract.md` → Errors
/// are sticky).
///
/// Errors: as for `tickertape_writer_append`, plus `TICKERTAPE_E_ABORTED`, and `TICKERTAPE_E_INVALID_ARGUMENT` if
/// `fill` is NULL. `fill` is not called when the append fails before it.
///
/// # Safety
///
/// `writer` must be NULL or a live writer handle. `fill` must be NULL or a function that follows the
/// `tickertape_fill_fn` contract; `user_data` is passed to it unchanged and never dereferenced by the library.
/// `seq_out` must be NULL or valid for writing a `uint64_t`; it is written after `fill` returns.
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
            // A non-zero return declines the record through the core's `try_append_with`, which zeroes and syncs the
            // bytes and returns: aborting needs no unwinding. A panic or foreign exception out of `fill` still
            // unwinds, to the guard around this call.
            let appended = w.try_append_with(&record, payload_len, |buf| {
                let ptr = if buf.is_empty() {
                    std::ptr::null_mut()
                } else {
                    buf.as_mut_ptr()
                };
                // SAFETY: `fill` follows the callback contract (caller guarantee); `ptr` is NULL with length 0 or
                // valid for `buf.len()` writable bytes for the duration of the call.
                match unsafe { fill(user_data, ptr, buf.len()) } {
                    0 => Ok(()),
                    _ => Err(()),
                }
            });
            match appended {
                Ok(Ok(seq)) => Ok(seq),
                // The writer refuses appends once a clean-up failed, before calling `fill`, so an error recorded
                // now comes from this call.
                Ok(Err(())) => Err(match w.abandoned_write_error() {
                    Some(cause) => FfiError::from(cause).with_message(format!(
                        "the fill callback returned non-zero, and zeroing the abandoned bytes durably failed: \
                         {cause}; the writer refuses further appends and checkpoints: close it and reopen the \
                         journal"
                    )),
                    None => FfiError::new(
                        S::TICKERTAPE_E_ABORTED,
                        "the fill callback returned non-zero; nothing was appended",
                    ),
                }),
                Err(e) => Err(writer_error(w, e)),
            }
        })?;
        // SAFETY: forwarded caller contract.
        unsafe { args::write_optional(seq_out, seq) };
        Ok(S::TICKERTAPE_OK)
    })
}

/// Appends `records` in order with `append`, storing the number committed in `*appended_out` after each one, so that
/// it is right whenever this returns or unwinds.
///
/// # Safety
///
/// Every record must have been checked by `tickertape_writer_append_batch` (payload not NULL when its length is not
/// 0, length within `isize::MAX`), and its payload must be readable and unmodified during the call. `appended_out`
/// must be valid for writing a `usize` and must not overlap `records` or any payload.
pub(crate) unsafe fn append_records(
    records: &[tickertape_new_record],
    appended_out: *mut usize,
    mut append: impl FnMut(&NewRecord<'_>) -> Result<u64, FfiError>,
) -> Result<(), FfiError> {
    for (index, r) in records.iter().enumerate() {
        // SAFETY: checked before (see the function's contract); cannot fail here.
        let payload = unsafe { args::bytes(r.payload, r.payload_len, "payload") }?;
        append(&NewRecord {
            schema_id: r.schema_id,
            venue_ts_ns: r.venue_ts_ns,
            local_ts_ns: r.local_ts_ns,
            payload,
        })?;
        // SAFETY: valid for writes and disjoint from every input (the function's contract).
        unsafe { appended_out.write(index + 1) };
    }
    Ok(())
}

/// Appends `count` records in order, stopping at the first failure.
///
/// Every record is checked first (`payload` NULL with `payload_len` > 0, a length too large, `reserved` not 0): an
/// argument error returns `TICKERTAPE_E_INVALID_ARGUMENT`, naming the record's index in the message, and appends
/// nothing. Then the records are appended one by one, as with `tickertape_writer_append`. `*first_seq_out` (which may
/// be NULL) is set to the sequence number the first record gets, and `*appended_out` to the number of records
/// appended so far, updated as each one commits; the appended records are consecutive. On an error, the records
/// before the failing one (index `*appended_out`) **are appended** and stay appended: retry only from the failing
/// record.
///
/// If `appended_out` or `first_seq_out` overlaps another argument (each other, the `records` array or a payload),
/// returns `TICKERTAPE_E_INVALID_ARGUMENT` and writes nothing. Otherwise `*appended_out` is always set (0 on an
/// argument error).
///
/// Durability is as for `tickertape_writer_append`: **visible** on return, **durable** only after a later
/// successful checkpoint. Errors: as for `tickertape_writer_append`, and `TICKERTAPE_E_INVALID_ARGUMENT` if `count`
/// records do not fit in memory.
///
/// # Safety
///
/// `writer` must be NULL or a live writer handle. `records` must point to `count` readable
/// `tickertape_new_record`s whose payloads follow the rule for `tickertape_writer_append` (it may be NULL when `count`
/// is 0); the records and their payloads must stay unmodified until the call returns. `appended_out` must be NULL or
/// valid for writing a `size_t`, and `first_seq_out` NULL or valid for writing a `uint64_t`; neither may overlap the
/// records, their payloads or each other.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_append_batch(
    writer: *mut tickertape_writer,
    records: *const tickertape_new_record,
    count: usize,
    appended_out: *mut usize,
    first_seq_out: *mut u64,
) -> tickertape_status {
    guard(|| {
        if appended_out.is_null() {
            return Err(FfiError::null("appended_out"));
        }
        let outputs = [
            Region::of(appended_out, 1, "appended_out"),
            Region::of(first_seq_out, 1, "first_seq_out"),
        ];
        // SAFETY: forwarded caller contract; this checks the pointer and the size in bytes without reading.
        let records = unsafe { args::slice(records, count, "records", "count") };
        let records = match records {
            Ok(records) => records,
            Err(error) => {
                args::disjoint(&outputs)?;
                // SAFETY: not NULL; the caller guarantees it is writable; it overlaps no output (checked), and
                // the records are not read.
                unsafe { appended_out.write(0) };
                return Err(error);
            }
        };
        // Overlaps first, before anything is written: an output inside an input would change it mid-call.
        let array = Region::of(records.as_ptr(), records.len(), "records");
        args::disjoint(&[outputs[0], outputs[1], array])?;
        for (index, r) in records.iter().enumerate() {
            if r.payload_len > isize::MAX as usize {
                continue; // rejected by the validation below
            }
            let payload = Region::of(r.payload, r.payload_len, "payload");
            if let Some(output) = outputs.iter().find(|o| o.overlaps(&payload)) {
                return Err(FfiError::invalid(format!(
                    "records[{index}].payload and {} must not overlap",
                    output.name()
                )));
            }
        }
        // SAFETY: not NULL; the caller guarantees it is writable; it overlaps no input (checked above).
        unsafe { appended_out.write(0) };
        for (index, r) in records.iter().enumerate() {
            if r.reserved != 0 {
                return Err(FfiError::invalid(format!(
                    "records[{index}].reserved must be 0"
                )));
            }
            // SAFETY: the caller guarantees each payload is readable for its length.
            unsafe { args::bytes(r.payload, r.payload_len, "payload") }.map_err(|e| {
                let message = format!("records[{index}].{}", e.message());
                e.with_message(message)
            })?;
        }
        // SAFETY: forwarded caller contract.
        unsafe { handle(writer) }?.with(|w| {
            // SAFETY: `first_seq_out` is NULL or writable (caller guarantee) and overlaps no input (checked above).
            unsafe { args::write_optional(first_seq_out, w.next_seq()) };
            // SAFETY: every record was checked above, and `appended_out` overlaps no input.
            unsafe {
                append_records(records, appended_out, |record| {
                    w.append(record).map_err(|e| writer_error(w, e))
                })
            }
        })?;
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
/// durable even if a later writer's checkpoint succeeds. `TICKERTAPE_E_DURABILITY_LOST` is also returned after a
/// failed clean-up of an aborted fill (the message says which happened).
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
        unsafe { handle(writer) }?.with(|w| w.checkpoint().map_err(|e| writer_error(w, e)))?;
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
        let seq = unsafe { handle(writer) }?.with_ref(|w| Ok(w.next_seq()))?;
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
        unsafe { handle(writer) }?.with_ref(|w| match w.last_retention_error() {
            None => Ok(S::TICKERTAPE_OK),
            Some(error) => Err(FfiError::from(error)),
        })
    })
}
