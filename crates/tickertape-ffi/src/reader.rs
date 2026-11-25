//! The reader handle.

use std::ffi::c_char;
use std::path::Path;

use tickertape::{OwnedRecord, Reader, ReaderConfig, Record};

use crate::args::{self, Region};
use crate::options::tickertape_reader_options;
use crate::status::{FfiError, Handle, guard, tickertape_status, tickertape_status_code as S};

/// A journal reader (an opaque handle). Open with `tickertape_reader_open` or
/// `tickertape_reader_open_mapped_unsafe`, close with `tickertape_reader_close`.
///
/// Readers never take locks, never write segment files and never slow the writer down; any number can run at once,
/// in any process on the host. Every record is validated (length, CRC32C, sequence number) before it is returned. A
/// reader handle may move to another thread between calls but must never be used by two threads at once.
#[allow(non_camel_case_types)]
#[derive(Debug)]
pub struct tickertape_reader(Handle<ReaderState>);

/// A committed, validated record. The layout is frozen for format version 1.
///
/// Returned by `tickertape_reader_next` as a **borrowed view**: `payload` points into the reader's buffer or, for a
/// mapped reader, into the memory-mapped segment, and stays valid only until the next call that takes the same
/// reader (other than `tickertape_reader_position` and `tickertape_reader_get_metrics`) or until the reader is closed.
/// Copy what you need to keep. Returned
/// by `tickertape_reader_next_copy` and `tickertape_reader_read_batch` with `payload` pointing into the caller's
/// buffer. When `payload_len` is 0, `payload` is NULL.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct tickertape_record {
    /// Sequence number: the record's position in the journal, starting at 0, without gaps.
    pub seq: u64,
    /// As given when the record was appended.
    pub venue_ts_ns: u64,
    /// As given when the record was appended.
    pub local_ts_ns: u64,
    /// The payload bytes (see the struct's documentation for how long they stay valid). No alignment is guaranteed.
    pub payload: *const u8,
    /// Number of payload bytes.
    pub payload_len: usize,
    /// As given when the record was appended.
    pub schema_id: u32,
    /// Always 0.
    pub reserved: u32,
}

// The frozen layout (ADR 0028 → Versioning); tests/c/test_api.c asserts the same values with offsetof and sizeof.
#[cfg(target_pointer_width = "64")]
#[allow(dead_code)]
fn record_layout_is_frozen() {
    // Inside a function so that cbindgen does not parse it; the constant is evaluated all the same.
    const _: () = {
        use std::mem::{align_of, offset_of, size_of};
        assert!(size_of::<tickertape_record>() == 48 && align_of::<tickertape_record>() == 8);
        assert!(offset_of!(tickertape_record, seq) == 0);
        assert!(offset_of!(tickertape_record, venue_ts_ns) == 8);
        assert!(offset_of!(tickertape_record, local_ts_ns) == 16);
        assert!(offset_of!(tickertape_record, payload) == 24);
        assert!(offset_of!(tickertape_record, payload_len) == 32);
        assert!(offset_of!(tickertape_record, schema_id) == 40);
        assert!(offset_of!(tickertape_record, reserved) == 44);
    };
}

impl tickertape_record {
    /// The record with `payload` as its payload pointer, or NULL if the payload is empty.
    fn new(record: &Record<'_>, payload: *const u8) -> tickertape_record {
        tickertape_record {
            seq: record.seq,
            venue_ts_ns: record.venue_ts_ns,
            local_ts_ns: record.local_ts_ns,
            payload: if record.payload.is_empty() {
                std::ptr::null()
            } else {
                payload
            },
            payload_len: record.payload.len(),
            schema_id: record.schema_id,
            reserved: 0,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ReaderState {
    reader: Reader,
    /// A record already taken from the reader but not yet handed out (it did not fit a caller's buffer).
    pending: Option<OwnedRecord>,
    /// The pending record after `tickertape_reader_next` handed it out as a view; kept until the next call.
    returned: Option<OwnedRecord>,
    /// An error that stopped a batch after some records; reported by the next call.
    deferred: Option<FfiError>,
}

/// Where the next record comes from.
enum Next<'a> {
    Pending(OwnedRecord),
    Read(Record<'a>),
}

impl Next<'_> {
    fn record(&self) -> Record<'_> {
        match self {
            Next::Pending(owned) => owned.as_record(),
            Next::Read(record) => *record,
        }
    }

    fn into_owned(self) -> OwnedRecord {
        match self {
            Next::Pending(owned) => owned,
            Next::Read(record) => record.to_owned(),
        }
    }
}

impl ReaderState {
    /// The core reader's cursor, and the sequence number of the record held back for the next read, if any (the
    /// core reader has already moved past it).
    pub(crate) fn cursor(&self) -> (tickertape::status::Cursor, Option<u64>) {
        (self.reader.cursor(), self.pending.as_ref().map(|p| p.seq))
    }

    /// Starts a call that may return records: invalidates the previous view and reports a deferred error.
    fn begin(&mut self) -> Result<(), FfiError> {
        self.returned = None;
        self.deferred.take().map_or(Ok(()), Err)
    }
}

/// The pending record, or the next one from the reader (waiting up to `timeout_ns`). Takes the fields separately so
/// that callers can update `pending` while the returned record borrows `reader`.
fn next_record<'a>(
    reader: &'a mut Reader,
    pending: &mut Option<OwnedRecord>,
    timeout_ns: i64,
) -> Result<Option<Next<'a>>, FfiError> {
    if let Some(owned) = pending.take() {
        return Ok(Some(Next::Pending(owned)));
    }
    let record = match args::timeout(timeout_ns) {
        Some(t) if t.is_zero() => reader.try_next()?,
        timeout => reader.next_timeout(timeout)?,
    };
    Ok(record.map(Next::Read))
}

/// Borrows the reader behind a handle pointer.
///
/// # Safety
///
/// `ptr` must be NULL or a live handle from a reader open function that stays open during the borrow.
pub(crate) unsafe fn reader_handle<'a>(
    ptr: *mut tickertape_reader,
) -> Result<&'a Handle<ReaderState>, FfiError> {
    if ptr.is_null() {
        return Err(FfiError::null("reader"));
    }
    // SAFETY: not NULL; the caller guarantees a live handle. Only shared references to the handle are created; the
    // state inside is reached through `Handle::with` and `Handle::with_ref`.
    Ok(unsafe { &(*ptr).0 })
}

/// Shared implementation of both open functions.
///
/// # Safety
///
/// As for `tickertape_reader_open`; if `mapped`, also the precondition of `tickertape_reader_open_mapped_unsafe`.
unsafe fn open(
    dir: *const c_char,
    options: *const tickertape_reader_options,
    reader_out: *mut *mut tickertape_reader,
    mapped: bool,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        unsafe { args::write(reader_out, std::ptr::null_mut(), "reader_out") }?;
        // SAFETY: forwarded caller contract.
        let dir: &Path = unsafe { args::path(dir, "dir") }?;
        let config = if options.is_null() {
            ReaderConfig::default()
        } else {
            // SAFETY: not NULL; the caller guarantees live reader options.
            unsafe { &*options }.config.clone()
        };
        let reader = if mapped {
            // SAFETY: the caller of `tickertape_reader_open_mapped_unsafe` guarantees that only a tickertape writer
            // modifies the journal's segment files while the reader exists.
            unsafe { Reader::open_mapped(dir, config) }?
        } else {
            Reader::open(dir, config)?
        };
        let state = ReaderState {
            reader,
            pending: None,
            returned: None,
            deferred: None,
        };
        let ptr = Box::into_raw(Box::new(tickertape_reader(Handle::new(state))));
        // SAFETY: checked above that `reader_out` is writable.
        unsafe { reader_out.write(ptr) };
        Ok(S::TICKERTAPE_OK)
    })
}

/// Opens a reader on the journal in directory `dir`, positioned at the first record of the oldest segment, and stores
/// the new handle in `*reader_out` (NULL on failure). `options` may be NULL for the defaults; they are copied. If
/// memory for the handle cannot be allocated, the process aborts (there is no out-of-memory status).
///
/// This is the safe reader: it reads the files with `pread` into a buffer it owns and validates the copy, so it is
/// memory-safe whatever other processes do to the files, and never receives `SIGBUS`. At worst it reports corruption.
///
/// Errors: `TICKERTAPE_E_IO` with `ENOENT` (`tickertape_last_error_os_code`) if `dir` does not exist, which is what
/// a reader that starts before the writer sees, since the writer creates the directory; `TICKERTAPE_E_NOT_FOUND` if
/// `dir` exists but has no segment yet (the writer is creating it). A loop that waits for the writer should retry on
/// both and fail on anything else. `TICKERTAPE_E_CORRUPT` or `TICKERTAPE_E_UNSUPPORTED_VERSION` if the oldest
/// segment's header is invalid; `TICKERTAPE_E_IO` for other OS errors.
///
/// # Safety
///
/// `dir` must be NULL or a NUL-terminated string. `options` must be NULL or live reader options. `reader_out` must
/// be NULL or valid for writing a pointer, and must not overlap `dir`. Inputs must stay unmodified until the call
/// returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_reader_open(
    dir: *const c_char,
    options: *const tickertape_reader_options,
    reader_out: *mut *mut tickertape_reader,
) -> tickertape_status {
    // SAFETY: forwarded caller contract.
    unsafe { open(dir, options, reader_out, false) }
}

/// Like `tickertape_reader_open`, but records returned by `tickertape_reader_next` point directly into the
/// memory-mapped segment, without a copy.
///
/// # Safety
///
/// **Precondition: while the returned reader exists, no process other than a tickertape writer may modify the
/// journal's segment files**: no writing into them, truncating them, or replacing their contents through other
/// tools. A tickertape writer never modifies bytes after committing them, including during crash recovery (it repairs
/// by replacing files), so the precondition holds in normal operation. If it is broken, returned payloads can change
/// under the caller, and if a mapped file is truncated the process receives `SIGBUS` and dies. Use
/// `tickertape_reader_open` when you cannot rule this out (for example, for journals on shared or untrusted storage).
/// With `tickertape_reader_options_set_seek_index` enabled, the precondition also covers the journal's `seek-index/`
/// directory and its `checkpoint` file, and an index builder (`tickertape index`) counts as a writer.
///
/// The pointer rules of `tickertape_reader_open` also apply.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_reader_open_mapped_unsafe(
    dir: *const c_char,
    options: *const tickertape_reader_options,
    reader_out: *mut *mut tickertape_reader,
) -> tickertape_status {
    // SAFETY: forwarded caller contract, including the mapped-reader precondition.
    unsafe { open(dir, options, reader_out, true) }
}

/// Closes a reader and frees the handle, invalidating any record view it returned. NULL is ignored. Returns
/// `TICKERTAPE_E_BUSY` (and closes nothing; the handle stays valid) if called from inside a call on the same reader.
///
/// # Safety
///
/// `reader` must be NULL or a live reader handle. After this returns any status other than `TICKERTAPE_E_BUSY`
/// (including `TICKERTAPE_E_PANIC`), the handle is freed: using or closing it again is undefined behaviour, so never
/// retry a close that failed with another status.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_reader_close(
    reader: *mut tickertape_reader,
) -> tickertape_status {
    guard(|| {
        if reader.is_null() {
            return Ok(S::TICKERTAPE_OK);
        }
        // SAFETY: forwarded caller contract.
        if unsafe { reader_handle(reader) }?.is_busy() {
            return Err(FfiError::new(
                S::TICKERTAPE_E_BUSY,
                "cannot close a reader from inside one of its own calls",
            ));
        }
        // SAFETY: a live handle from `Box::into_raw` in `open`, not inside a call (checked above); the caller gives up
        // the pointer.
        drop(unsafe { Box::from_raw(reader) });
        Ok(S::TICKERTAPE_OK)
    })
}

/// Positions the reader so that the next record returned is `seq`.
///
/// If `seq` has not been written yet, the reader waits for it: reads return `TICKERTAPE_NONE` until the writer gets
/// there, and records before `seq` are skipped. Every record on the way is validated (by the reads that follow), so
/// the reader only lands on positions along the journal's current chain, except as below. The way starts at the segment's first record
/// or, for a forward seek within the reader's segment, at the reader's position if the reader can tell that it is on
/// the current chain (from 1.1). Records before that position are then not validated again, and if storage damage
/// destroyed the checkpoint marker, or set it back to an older one, together with covered records, the position can be
/// off the current chain unnoticed (`docs/crash-contract.md`, "Readers after a repair"); a seek to the last record the
/// reader passed, or an earlier one, scans the segment, and so does a new reader. With the seek-index option
/// (`tickertape_reader_options_set_seek_index`, from 1.1), the way may start at an indexed record before `seq`. Records
/// before it are not read, and crash contract C15 says where the landing place can be off the chain. Any call invalidates the record view
/// returned by `tickertape_reader_next`. A successful seek also discards a record held back by
/// `tickertape_reader_next_copy` or `tickertape_reader_read_batch`, and an error deferred by
/// `tickertape_reader_read_batch`; a failed seek leaves the reader exactly as it was (same position, held-back
/// record and deferred error).
///
/// Errors: `TICKERTAPE_E_SEQ_UNAVAILABLE` if `seq` is older than the oldest segment (retention removed it; seek to
/// the oldest segment's `base_seq` from `tickertape_list_segments` to continue there); `TICKERTAPE_E_NOT_FOUND`;
/// `TICKERTAPE_E_CORRUPT` or `TICKERTAPE_E_IO` while locating it.
///
/// # Safety
///
/// `reader` must be NULL or a live reader handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_reader_seek(
    reader: *mut tickertape_reader,
    seq: u64,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        unsafe { reader_handle(reader) }?.with(|state| {
            state.returned = None;
            // The core leaves its cursor unchanged when a seek fails; keep what belongs to that cursor too.
            state.reader.seek(seq)?;
            state.pending = None;
            state.deferred = None;
            Ok(())
        })?;
        Ok(S::TICKERTAPE_OK)
    })
}

/// Positions the reader at the end of the journal, without reading the segments before it: the next record returned
/// is the first one this call did not find committed (with durable-only, the first one it did not find covered by a
/// checkpoint). Added in 1.1.
///
/// Under a live writer, "the end" is an observation made during the call: every record committed before the call is
/// behind the reader, every record committed after it returns is ahead of it, and one committed during the call can
/// be on either side. No record is skipped or repeated from there on. The call reads at most the newest segment, from
/// the checkpoint's position if the checkpoint is in it, and any segments the writer rolls to meanwhile, validating
/// every record it reads; damage in records before that is not noticed. If the writer keeps committing as fast as the
/// reader validates, the call runs until it catches up; it cannot be interrupted. With durable-only and no valid
/// checkpoint, the reader is placed at the first record of the oldest segment.
///
/// Any call invalidates the record view returned by `tickertape_reader_next`. On success this also discards a record
/// held back by `tickertape_reader_next_copy` or `tickertape_reader_read_batch`, and an error deferred by
/// `tickertape_reader_read_batch`; on failure the reader is left exactly as it was (same position, held-back record
/// and deferred error).
///
/// Errors: `TICKERTAPE_E_NOT_FOUND`; `TICKERTAPE_E_CORRUPT`, `TICKERTAPE_E_UNSUPPORTED_VERSION` or `TICKERTAPE_E_IO`
/// while locating it; `TICKERTAPE_E_DISCARDED` if recovery or retention replaced or removed the segment on each of 8
/// attempts in a row (its message names where the call was reading, not the reader's position).
///
/// # Safety
///
/// `reader` must be NULL or a live reader handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_reader_seek_to_end(
    reader: *mut tickertape_reader,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        unsafe { reader_handle(reader) }?.with(|state| {
            state.returned = None;
            // As in `tickertape_reader_seek`: a failure leaves the core's cursor unchanged.
            state.reader.seek_to_end()?;
            state.pending = None;
            state.deferred = None;
            Ok(())
        })?;
        Ok(S::TICKERTAPE_OK)
    })
}

/// Returns the next record as a borrowed view in `*record_out`.
///
/// `timeout_ns` 0 (`TICKERTAPE_NO_WAIT`) does not wait; a positive value waits up to that many nanoseconds, using
/// the configured back-off and wake-ups; a negative value (`TICKERTAPE_WAIT_FOREVER`) waits until a record arrives.
/// The timeout bounds only the wait for a record to become available: argument checks, catching up after a seek
/// (validating the records skipped on the way) and file I/O are not interrupted, so a call can take longer. A call
/// waiting with `TICKERTAPE_WAIT_FOREVER` cannot be interrupted, not even by closing the reader from another thread
/// (which is undefined behaviour): to make a wait cancellable, loop with a finite timeout and check for
/// cancellation between calls. Returns `TICKERTAPE_OK` with a record, or `TICKERTAPE_NONE` (leaving `*record_out`
/// unchanged) if no record is committed yet or the timeout passed. Follows seals into the next segment
/// transparently.
///
/// Without durable-only, the reader returns records as soon as they are **visible**, including records that a later
/// power loss can erase (the writer then reuses their sequence numbers). With durable-only, it returns only records
/// covered by a checkpoint (**durable**) and treats the rest as not written yet.
///
/// The view's `payload` is valid until the next call that takes this reader (other than
/// `tickertape_reader_position` and `tickertape_reader_get_metrics`) or until the reader is closed. It is NULL when
/// `payload_len` is 0.
///
/// Errors: `TICKERTAPE_E_CORRUPT` if the bytes at the cursor fail validation (the cursor does not move, so calling
/// again retries the same position, which may succeed after a writer recovery); `TICKERTAPE_E_DISCARDED` if writer
/// recovery or retention replaced or removed the segment at the cursor (call `tickertape_reader_seek`);
/// `TICKERTAPE_E_IO`; or an error deferred by `tickertape_reader_read_batch`.
///
/// # Safety
///
/// `reader` must be NULL or a live reader handle. `record_out` must be NULL or valid for writing a
/// `tickertape_record` (caller-owned storage; only the returned payload pointer borrows from the reader).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_reader_next(
    reader: *mut tickertape_reader,
    timeout_ns: i64,
    record_out: *mut tickertape_record,
) -> tickertape_status {
    guard(|| {
        if record_out.is_null() {
            return Err(FfiError::null("record_out"));
        }
        // SAFETY: forwarded caller contract.
        unsafe { reader_handle(reader) }?.with(|state| {
            state.begin()?;
            let view = match next_record(&mut state.reader, &mut state.pending, timeout_ns)? {
                None => return Ok(S::TICKERTAPE_NONE),
                Some(Next::Read(record)) => {
                    tickertape_record::new(&record, record.payload.as_ptr())
                }
                Some(Next::Pending(owned)) => {
                    let view = tickertape_record::new(&owned.as_record(), owned.payload.as_ptr());
                    // Moving the record does not move its heap buffer, so the view stays valid until the next call.
                    state.returned = Some(owned);
                    view
                }
            };
            // SAFETY: checked above that `record_out` is not NULL; the caller guarantees it is writable.
            unsafe { record_out.write(view) };
            Ok(S::TICKERTAPE_OK)
        })
    })
}

/// Like `tickertape_reader_next`, but copies the payload into the caller's buffer `buf` of `buf_len` bytes; the
/// returned record's `payload` points to `buf`.
///
/// `payload` is NULL when `payload_len` is 0.
///
/// If the payload does not fit, returns `TICKERTAPE_E_BUFFER_TOO_SMALL` with `*record_out` describing the record
/// (with `payload` NULL and `payload_len` the size needed); the record is held back and returned by the next read
/// call on this reader. Returns `TICKERTAPE_E_INVALID_ARGUMENT` if `record_out` overlaps `buf`.
///
/// # Safety
///
/// `reader` must be NULL or a live reader handle. `buf` must be valid for writing `buf_len` bytes (it may be NULL
/// when `buf_len` is 0). `record_out` must be NULL or valid for writing a `tickertape_record`. Both are caller-owned
/// and must not overlap each other.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_reader_next_copy(
    reader: *mut tickertape_reader,
    timeout_ns: i64,
    buf: *mut u8,
    buf_len: usize,
    record_out: *mut tickertape_record,
) -> tickertape_status {
    guard(|| {
        if record_out.is_null() {
            return Err(FfiError::null("record_out"));
        }
        if buf.is_null() && buf_len > 0 {
            return Err(FfiError::invalid(
                "buf must not be NULL when buf_len is not 0",
            ));
        }
        args::disjoint(&[
            Region::of(buf, buf_len, "buf"),
            Region::of(record_out, 1, "record_out"),
        ])?;
        // SAFETY: forwarded caller contract.
        unsafe { reader_handle(reader) }?.with(|state| {
            state.begin()?;
            let Some(next) = next_record(&mut state.reader, &mut state.pending, timeout_ns)? else {
                return Ok(S::TICKERTAPE_NONE);
            };
            let record = next.record();
            if record.payload.len() > buf_len {
                let described = tickertape_record::new(&record, std::ptr::null());
                state.pending = Some(next.into_owned());
                // SAFETY: checked above that `record_out` is not NULL; the caller guarantees it is writable.
                unsafe { record_out.write(described) };
                return Err(FfiError::new(
                    S::TICKERTAPE_E_BUFFER_TOO_SMALL,
                    format!(
                        "record {} has {} payload bytes; the buffer holds {buf_len}",
                        described.seq, described.payload_len
                    ),
                ));
            }
            // SAFETY: `buf` is valid for `buf_len >= payload.len()` bytes (caller guarantee) and cannot overlap the
            // reader's own buffer or a read-only mapping.
            unsafe { args::copy_to(buf, 0, record.payload) };
            let view = tickertape_record::new(&record, buf);
            // SAFETY: checked above that `record_out` is not NULL; the caller guarantees it is writable.
            unsafe { record_out.write(view) };
            Ok(S::TICKERTAPE_OK)
        })
    })
}

/// Reads up to `max_records` records into `records`, copying their payloads one after another into `buf` (`buf_len`
/// bytes), and stores the number read in `*count_out`. Each record's `payload` points into `buf` (NULL when its
/// `payload_len` is 0). `max_records` must not be 0 (`TICKERTAPE_E_INVALID_ARGUMENT`).
///
/// Waits up to `timeout_ns` (as for `tickertape_reader_next`) for the first record only, then takes only records that
/// are already committed. Stops when `max_records` are read, the next record's payload does not fit in the rest of
/// `buf` (it is held back for the next call), or no further record is available.
///
/// Returns `TICKERTAPE_OK` with `*count_out` >= 1; `TICKERTAPE_NONE` with `*count_out` 0 if no record arrived in
/// time; or `TICKERTAPE_E_BUFFER_TOO_SMALL` with `*count_out` 0 if the first record's payload does not fit in `buf`,
/// with `records[0]` describing it (`payload` NULL, `payload_len` the size needed; the record is held back).
///
/// Partial failure: an error that happens before any record is read is returned directly. An error after one or
/// more records is **not** returned by this call: it returns `TICKERTAPE_OK` with the records read so far, and the
/// next read call on this reader (`next`, `next_copy` or `read_batch`) returns the error. A successful
/// `tickertape_reader_seek` or `tickertape_reader_seek_to_end` discards it instead. The reader does not advance past the failing position, so a
/// persistent error (such as corruption) is returned again by the calls after that. No record handed out is ever
/// lost to an error.
///
/// If any two of `records` (all `max_records` entries), `buf` and `count_out` overlap, returns
/// `TICKERTAPE_E_INVALID_ARGUMENT` and writes nothing. Otherwise `*count_out` is set to 0 first, also on other errors
/// and with `TICKERTAPE_NONE`.
///
/// # Safety
///
/// `reader` must be NULL or a live reader handle. `records` must be valid for writing `max_records`
/// `tickertape_record`s. `buf` must be valid for writing `buf_len` bytes (it may be NULL when `buf_len` is 0).
/// `count_out` must be NULL or valid for writing a `size_t`. All three are caller-owned and must not overlap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_reader_read_batch(
    reader: *mut tickertape_reader,
    timeout_ns: i64,
    records: *mut tickertape_record,
    max_records: usize,
    buf: *mut u8,
    buf_len: usize,
    count_out: *mut usize,
) -> tickertape_status {
    guard(|| {
        if count_out.is_null() {
            return Err(FfiError::null("count_out"));
        }
        let checked = if records.is_null() {
            Err(FfiError::null("records"))
        } else if max_records == 0 {
            Err(FfiError::invalid("max_records must not be 0"))
        } else if buf.is_null() && buf_len > 0 {
            Err(FfiError::invalid(
                "buf must not be NULL when buf_len is not 0",
            ))
        } else {
            Ok(())
        };
        // Overlaps first, before anything is written (a NULL pointer is an empty region).
        args::disjoint(&[
            Region::of(records, max_records, "records"),
            Region::of(buf, buf_len, "buf"),
            Region::of(count_out, 1, "count_out"),
        ])?;
        // SAFETY: not NULL; the caller guarantees it is writable; it overlaps neither `records` nor `buf` (checked).
        unsafe { count_out.write(0) };
        checked?;
        // SAFETY: forwarded caller contract.
        unsafe { reader_handle(reader) }?.with(|state| {
            state.begin()?;
            let mut count = 0usize;
            let mut used = 0usize;
            while count < max_records {
                let timeout = if count == 0 { timeout_ns } else { 0 };
                let next = match next_record(&mut state.reader, &mut state.pending, timeout) {
                    Ok(Some(next)) => next,
                    Ok(None) if count == 0 => return Ok(S::TICKERTAPE_NONE),
                    Ok(None) => break,
                    Err(error) if count == 0 => return Err(error),
                    Err(error) => {
                        state.deferred = Some(error);
                        break;
                    }
                };
                let record = next.record();
                let len = record.payload.len();
                if len > buf_len - used {
                    let described = tickertape_record::new(&record, std::ptr::null());
                    state.pending = Some(next.into_owned());
                    if count > 0 {
                        break;
                    }
                    // SAFETY: `records` is not NULL and holds at least one element (caller guarantee).
                    unsafe { records.write(described) };
                    return Err(FfiError::new(
                        S::TICKERTAPE_E_BUFFER_TOO_SMALL,
                        format!(
                            "record {} has {len} payload bytes; the buffer holds {buf_len}",
                            described.seq
                        ),
                    ));
                }
                // SAFETY: `used + len <= buf_len`, so the destination lies inside the caller's buffer, which cannot
                // overlap the reader's own buffer or a read-only mapping.
                unsafe { args::copy_to(buf, used, record.payload) };
                let payload = if len == 0 {
                    // `tickertape_record::new` stores NULL for an empty payload.
                    std::ptr::null_mut()
                } else {
                    // SAFETY: `used + len <= buf_len` with `len > 0`, so `buf` is not NULL and the result stays
                    // within the caller's buffer.
                    unsafe { buf.add(used) }
                };
                // SAFETY: `count < max_records`, and `records` is valid for `max_records` elements.
                unsafe {
                    records
                        .add(count)
                        .write(tickertape_record::new(&record, payload))
                };
                used += len;
                count += 1;
                // SAFETY: checked at the start that `count_out` is writable.
                unsafe { count_out.write(count) };
            }
            Ok(S::TICKERTAPE_OK)
        })
    })
}

/// Stores the sequence number of the next record this reader will return in `*seq_out`. Does not invalidate a
/// record view.
///
/// # Safety
///
/// `reader` must be NULL or a live reader handle. `seq_out` must be NULL or valid for writing a `uint64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_reader_position(
    reader: *mut tickertape_reader,
    seq_out: *mut u64,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let seq = unsafe { reader_handle(reader) }?.with_ref(|state| {
            Ok(state
                .pending
                .as_ref()
                .map_or_else(|| state.reader.position(), |p| p.seq))
        })?;
        // SAFETY: forwarded caller contract.
        unsafe { args::write(seq_out, seq, "seq_out") }?;
        Ok(S::TICKERTAPE_OK)
    })
}
