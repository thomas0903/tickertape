//! The writer handle.

use std::ffi::{c_char, c_void};

use tickertape::{CheckpointProgress, Error, NewRecord, Writer, WriterConfig};

use crate::args::{self, Region};
use crate::options::tickertape_writer_options;
use crate::status::{FfiError, Handle, guard, tickertape_status, tickertape_status_code as S};

/// A journal writer (an opaque handle). Open with `tickertape_writer_open`, close with `tickertape_writer_close`.
///
/// Only one writer can have a journal open at a time (across processes). A writer handle may move to another thread
/// between calls but must never be used by two threads at once.
///
/// Background checkpoints (`tickertape_writer_request_checkpoint`) run on a thread the library starts and owns
/// (`tickertape-checkpoint`); the program never calls into it, and the rule above still applies to every call on the
/// handle. While one runs, every writer function may be called (from the one thread using the handle): appends
/// continue at once; `tickertape_writer_request_checkpoint` queues another request or joins the queued one;
/// `tickertape_writer_checkpoint_progress`, `tickertape_writer_next_seq`, `tickertape_writer_get_metrics` and
/// `tickertape_writer_last_retention_error` return without waiting; `tickertape_writer_checkpoint` and
/// `tickertape_writer_close` wait for it to finish.
#[allow(non_camel_case_types)]
#[derive(Debug)]
pub struct tickertape_writer(Handle<Writer>);

/// Where a writer's checkpoints stand, an extensible struct filled by `tickertape_writer_checkpoint_progress`.
///
/// Set `struct_size` to `sizeof(tickertape_checkpoint_progress)` before the call. The library rejects a `struct_size`
/// below 56 with `TICKERTAPE_E_INVALID_ARGUMENT`, writes `min(struct_size, the library's own size)` bytes, and stores
/// the number of bytes it filled back into `struct_size`. Later versions only append fields.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct tickertape_checkpoint_progress {
    /// In: the size of the caller's struct. Out: the number of bytes the library filled.
    pub struct_size: u64,
    /// 1 if the `durable_*` fields hold a checkpoint marker; 0 if the journal has none yet (they are then 0).
    pub has_durable: u32,
    /// 1 while a requested background checkpoint has not finished (`pending_next_seq` is then its frontier); 0 when
    /// every requested checkpoint has finished (`pending_next_seq` is then 0).
    pub has_pending: u32,
    /// The generation of the checkpoint marker as this writer last made it durable, or read it at open.
    pub durable_generation: u64,
    /// That marker's `next_seq`: every record with a smaller sequence number is durable.
    pub durable_next_seq: u64,
    /// The segment of that marker's position.
    pub durable_segment_base_seq: u64,
    /// The offset of that marker's position.
    pub durable_offset: u64,
    /// The sequence number the newest requested background checkpoint covers up to (the value
    /// `tickertape_writer_request_checkpoint` returned for it), while it has not finished.
    pub pending_next_seq: u64,
}

/// The smallest accepted `struct_size` of `tickertape_checkpoint_progress`: its size when it was added. (A function,
/// not a constant, so that cbindgen skips it.)
const fn checkpoint_progress_min_size() -> usize {
    56
}

// The layout (ADR 0028 → Versioning); tests/c/test_api.c asserts the same values with offsetof and sizeof. The
// struct has no padding, which `args::write_struct` relies on.
#[allow(dead_code)]
fn checkpoint_progress_layout_is_fixed() {
    // Inside a function so that cbindgen does not parse it; the constant is evaluated all the same.
    const _: () = {
        use std::mem::{align_of, offset_of, size_of};
        type P = tickertape_checkpoint_progress;
        assert!(size_of::<P>() == checkpoint_progress_min_size() && align_of::<P>() == 8);
        assert!(offset_of!(P, has_durable) == 8 && offset_of!(P, has_pending) == 12);
        assert!(offset_of!(P, durable_generation) == 16 && offset_of!(P, durable_offset) == 40);
        assert!(offset_of!(P, pending_next_seq) == 48);
    };
}

impl From<CheckpointProgress> for tickertape_checkpoint_progress {
    fn from(progress: CheckpointProgress) -> Self {
        let durable = progress.durable;
        tickertape_checkpoint_progress {
            struct_size: 0,
            has_durable: u32::from(durable.is_some()),
            has_pending: u32::from(progress.pending.is_some()),
            durable_generation: durable.map_or(0, |slot| slot.generation),
            durable_next_seq: durable.map_or(0, |slot| slot.next_seq),
            durable_segment_base_seq: durable.map_or(0, |slot| slot.segment_base_seq),
            durable_offset: durable.map_or(0, |slot| slot.offset),
            pending_next_seq: progress.pending.unwrap_or(0),
        }
    }
}

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
pub(crate) unsafe fn writer_handle<'a>(
    ptr: *mut tickertape_writer,
) -> Result<&'a Handle<Writer>, FfiError> {
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

/// Closes a writer and frees the handle, as `Writer::close` does in Rust. NULL is ignored.
///
/// Closing does **not** checkpoint: every appended record stays visible to readers and survives the process exiting,
/// but only records covered by a successful checkpoint are durable. If background checkpoints
/// (`tickertape_writer_request_checkpoint`) are queued or running, it first waits for them to finish (it does not
/// cancel them), then reports how this writer's checkpoints ended:
///
/// - `TICKERTAPE_OK` if none failed;
/// - the error of a background checkpoint, or of early write-back (Linux, `tickertape_writer_options_set_early_writeback`),
///   that failed and that no earlier call returned: `TICKERTAPE_E_IO` or `TICKERTAPE_E_STORAGE_FULL`, with the OS
///   error. The write-back helper is stopped first, so an error from a call it was making is included;
/// - `TICKERTAPE_E_DURABILITY_LOST` if the writer was already poisoned: a checkpoint failed and an earlier call
///   returned its error, or the clean-up of an aborted fill failed (the message says which).
///
/// The handle is freed in all three cases. Returns `TICKERTAPE_E_BUSY` (and closes nothing; the handle stays valid)
/// if called from a fill callback of the same writer.
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
        if unsafe { writer_handle(writer) }?.is_busy() {
            return Err(FfiError::new(
                S::TICKERTAPE_E_BUSY,
                "cannot close a writer from inside one of its own calls",
            ));
        }
        // SAFETY: a live handle from `Box::into_raw` in `tickertape_writer_open`, not inside a call (checked above);
        // the caller gives up the pointer.
        let writer = unsafe { Box::from_raw(writer) }.0.into_inner();
        let abandoned = writer.abandoned_write_error().map(ToString::to_string);
        match writer.close() {
            Ok(()) => Ok(S::TICKERTAPE_OK),
            Err(Error::DurabilityLost) if abandoned.is_some() => {
                Err(FfiError::from(&Error::DurabilityLost).with_message(format!(
                    "durability can no longer be guaranteed for this writer: the clean-up after an aborted fill \
                     callback failed ({}); the writer is closed",
                    abandoned.unwrap_or_default()
                )))
            }
            Err(e) => Err(FfiError::from(e)),
        }
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
        let seq = unsafe { writer_handle(writer) }?
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
        let seq = unsafe { writer_handle(writer) }?.with(|w| {
            // A non-zero return declines the record through the core's `try_append_with`, which zeroes and syncs the
            // bytes and returns: aborting needs no unwinding. A Rust panic out of `fill` is cleaned up by the core and
            // resumed, then caught by `Handle::with` (the handle becomes broken; `TICKERTAPE_E_PANIC`). A foreign
            // exception out of `fill` is unsupported (ADR 0028): with current Rust the core's `catch_unwind` ends the
            // process.
            let mut declined = false;
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
                    _ => {
                        declined = true;
                        Err(())
                    }
                }
            });
            match appended {
                Ok(Ok(seq)) => Ok(seq),
                Ok(Err(())) => Err(FfiError::new(
                    S::TICKERTAPE_E_ABORTED,
                    "the fill callback returned non-zero; nothing was appended",
                )),
                // `fill` declined and the clean-up failed: the core returns the error of a refused append; report
                // the recorded cause (the same OS error) with a message that says what happened.
                Err(e) => Err(match w.abandoned_write_error() {
                    Some(cause) if declined => FfiError::from(cause).with_message(format!(
                        "the fill callback returned non-zero, and zeroing the abandoned bytes durably failed: \
                         {cause}; the writer refuses further appends and checkpoints: close it and reopen the \
                         journal"
                    )),
                    _ => writer_error(w, e),
                }),
            }
        })?;
        // SAFETY: forwarded caller contract.
        unsafe { args::write_optional(seq_out, seq) };
        Ok(S::TICKERTAPE_OK)
    })
}

/// Hands `records` to `append_batch` (one call to `Writer::append_batch`) as an iterator of `NewRecord`s and stores
/// the number appended in `*appended_out`. The iterator stores, before each record it hands out, the number handed
/// out so far: the core appends a record before it takes the next, so that is the number committed if
/// `append_batch` unwinds. When `append_batch` returns, the count it reports is stored.
///
/// # Safety
///
/// Every record must have been checked by `tickertape_writer_append_batch` (payload not NULL when its length is not
/// 0, length within `isize::MAX`), and its payload must be readable and unmodified during the call. `appended_out`
/// must be valid for writing a `usize` and must not overlap `records` or any payload.
pub(crate) unsafe fn append_records<'r>(
    records: &'r [tickertape_new_record],
    appended_out: *mut usize,
    append_batch: impl FnOnce(&mut Taken<'r>) -> (usize, Result<(), FfiError>),
) -> Result<(), FfiError> {
    let mut taken = Taken {
        records: records.iter(),
        taken: 0,
        appended_out,
    };
    let (appended, result) = append_batch(&mut taken);
    // SAFETY: valid for writes and disjoint from every input (the function's contract).
    unsafe { appended_out.write(appended) };
    result
}

/// The records of a batch, as `append_records` hands them to the core.
pub(crate) struct Taken<'r> {
    records: std::slice::Iter<'r, tickertape_new_record>,
    taken: usize,
    /// Valid for writes and disjoint from every input (the contract of `append_records`).
    appended_out: *mut usize,
}

impl<'r> Iterator for Taken<'r> {
    type Item = NewRecord<'r>;

    fn next(&mut self) -> Option<NewRecord<'r>> {
        // SAFETY: see the field.
        unsafe { self.appended_out.write(self.taken) };
        let r = self.records.next()?;
        self.taken += 1;
        // SAFETY: every record was checked before (the contract of `append_records`), so this cannot fail.
        let payload = unsafe { args::bytes(r.payload, r.payload_len, "payload") }
            .expect("payload checked before the batch");
        Some(NewRecord {
            schema_id: r.schema_id,
            venue_ts_ns: r.venue_ts_ns,
            local_ts_ns: r.local_ts_ns,
            payload,
        })
    }
}

/// Appends `count` records in order, stopping at the first failure.
///
/// Every record is checked first (`payload` NULL with `payload_len` > 0, a length too large, `reserved` not 0): an
/// argument error returns `TICKERTAPE_E_INVALID_ARGUMENT`, naming the record's index in the message, and appends
/// nothing. Then the records are appended in order in one batch (ADR 0041): each record is committed, and visible to
/// readers, as it is written, as with `tickertape_writer_append`. Instead of one wake-up after each record, a batch
/// that appends at least one record wakes blocked readers once (when wake-ups are enabled) after the last record
/// appended, plus once at each roll inside the batch. `*first_seq_out` (which may be NULL) is set to the sequence
/// number the first record gets, and `*appended_out` to the number of records appended; the appended records are
/// consecutive.
/// On an error, the records before the failing one (index `*appended_out`) **are appended** and stay appended: retry
/// only from the failing record.
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
        unsafe { writer_handle(writer) }?.with(|w| {
            // SAFETY: `first_seq_out` is NULL or writable (caller guarantee) and overlaps no input (checked above).
            unsafe { args::write_optional(first_seq_out, w.next_seq()) };
            // SAFETY: every record was checked above, and `appended_out` overlaps no input.
            unsafe {
                append_records(records, appended_out, |taken| match w.append_batch(taken) {
                    Ok(_) => (records.len(), Ok(())),
                    Err(e) => (e.appended, Err(writer_error(w, e.error))),
                })
            }
        })?;
        Ok(S::TICKERTAPE_OK)
    })
}

/// Makes every record appended so far **durable**, using the platform's sync calls (`docs/crash-contract.md` → What
/// `checkpoint()` does), and blocks until they finish. Configured retention then runs; its failures are reported by
/// `tickertape_writer_last_retention_error` and do not change this result. If background checkpoints
/// (`tickertape_writer_request_checkpoint`) are queued or running, it first waits for them, then checkpoints on the
/// calling thread as without them.
///
/// Errors: `TICKERTAPE_E_IO` or `TICKERTAPE_E_STORAGE_FULL` if a sync fails. The writer is then poisoned: every later
/// checkpoint returns `TICKERTAPE_E_DURABILITY_LOST`, because a failed sync can drop data that a retry would not
/// rewrite. Appends keep working. Treat a checkpoint error as an incident: records written before it may not be
/// durable even if a later writer's checkpoint succeeds. If a background checkpoint or early write-back failed and no
/// call has returned its error yet, this returns that error (and the writer is poisoned).
/// `TICKERTAPE_E_DURABILITY_LOST` is also returned after a failed clean-up of an aborted fill (the message says which
/// happened).
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
        unsafe { writer_handle(writer) }?
            .with(|w| w.checkpoint().map_err(|e| writer_error(w, e)))?;
        Ok(S::TICKERTAPE_OK)
    })
}

/// Starts a checkpoint of every record appended so far on the writer's checkpoint thread, and returns without waiting
/// for its sync calls (ADR 0038). Stores in `*next_seq_out` (which may be NULL) the sequence number it covers up to:
/// once it has finished, every record with a smaller sequence number is durable, exactly as after
/// `tickertape_writer_checkpoint`. Records appended after this call are not covered. Appends continue while it runs.
///
/// Observe completion with `tickertape_writer_checkpoint_progress`; `tickertape_writer_checkpoint` waits for it before
/// checkpointing itself, and `tickertape_writer_close` waits for it and reports how it ended. The first call starts
/// the thread that runs them (`tickertape-checkpoint`, placed as `tickertape_writer_options_set_helper_cpus` says). One
/// checkpoint runs at a time: a request made while one runs waits for it, and requests made meanwhile join the waiting
/// one, which then covers up to the newest of them (one marker for all of them). Configured retention runs on the
/// checkpoint thread after each successful background checkpoint; what it did reaches
/// `tickertape_writer_last_retention_error` at the writer's next call to this function,
/// `tickertape_writer_checkpoint_progress`, `tickertape_writer_checkpoint` or `tickertape_writer_close`
/// (`tickertape_writer_get_metrics` counts its failures at once).
///
/// Cost on the calling thread: one lock and a few small moves; no system call (except for starting the thread at the
/// first call). See `tickertape_writer` for which calls may be made while a checkpoint runs.
///
/// Errors (nothing is requested): the error of an earlier background checkpoint that failed, or of early write-back
/// (Linux), if no call has returned it yet: `TICKERTAPE_E_IO` or `TICKERTAPE_E_STORAGE_FULL`, returned once, after
/// which the writer is poisoned as after a failed `tickertape_writer_checkpoint`; `TICKERTAPE_E_DURABILITY_LOST` if
/// the writer is poisoned; `TICKERTAPE_E_IO` if the checkpoint thread cannot be started (the writer is not poisoned);
/// `TICKERTAPE_E_SEQUENCE_EXHAUSTED` if checkpoint generations are exhausted.
///
/// # Safety
///
/// `writer` must be NULL or a live writer handle. `next_seq_out` must be NULL or valid for writing a `uint64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_request_checkpoint(
    writer: *mut tickertape_writer,
    next_seq_out: *mut u64,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let seq = unsafe { writer_handle(writer) }?
            .with(|w| w.request_checkpoint().map_err(|e| writer_error(w, e)))?;
        // SAFETY: forwarded caller contract.
        unsafe { args::write_optional(next_seq_out, seq) };
        Ok(S::TICKERTAPE_OK)
    })
}

/// Reports where this writer's checkpoints stand in `*progress`: the marker it last made durable (or read at open),
/// and the frontier of a background checkpoint still in flight. Collects what the checkpoint thread has finished.
///
/// Cost: one atomic load when the checkpoint thread has finished nothing since the last call; otherwise one lock. No
/// system calls.
///
/// Errors: `TICKERTAPE_E_INVALID_ARGUMENT` if `progress` is NULL or `progress->struct_size` is below the minimum (see
/// `tickertape_checkpoint_progress`); the error of a background checkpoint or of early write-back (Linux) that failed,
/// the first time it is reported (`TICKERTAPE_E_IO` or `TICKERTAPE_E_STORAGE_FULL`; the writer is then poisoned);
/// `TICKERTAPE_E_DURABILITY_LOST` whenever the writer is poisoned after that, also after a failed
/// `tickertape_writer_checkpoint` or a failed clean-up of an aborted fill. A poisoned writer reports no progress
/// (nothing it does is durable any more): on an error `*progress` is not written.
///
/// # Safety
///
/// `writer` must be NULL or a live writer handle. `progress` must be NULL or point to a caller-owned struct whose
/// first `progress->struct_size` bytes are writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_checkpoint_progress(
    writer: *mut tickertape_writer,
    progress: *mut tickertape_checkpoint_progress,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let filled =
            unsafe { args::struct_size(progress, checkpoint_progress_min_size(), "progress") }?;
        // SAFETY: forwarded caller contract.
        let value = unsafe { writer_handle(writer) }?
            .with(|w| w.checkpoint_progress().map_err(|e| writer_error(w, e)))?;
        // SAFETY: `filled` comes from `struct_size` for this type; the caller guarantees the bytes are writable.
        unsafe { args::write_struct(progress, value.into(), filled) };
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
        let seq = unsafe { writer_handle(writer) }?.with_ref(|w| Ok(w.next_seq()))?;
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
        unsafe { writer_handle(writer) }?.with_ref(|w| match w.last_retention_error() {
            None => Ok(S::TICKERTAPE_OK),
            Some(error) => Err(FfiError::from(error)),
        })
    })
}
