//! Status codes, the per-thread last error, and the panic guard every entry point runs in.

use std::cell::{Cell, RefCell, UnsafeCell};
use std::ffi::{CString, c_char};
use std::panic::{self, AssertUnwindSafe};

use tickertape::{CorruptReason, Error};

/// The return type of every fallible function: a value of `tickertape_status_code`.
///
/// `TICKERTAPE_OK` (0) and `TICKERTAPE_NONE` (1) are successes; negative values are errors. Later versions may add
/// error codes: treat any unknown negative value as an error.
#[allow(non_camel_case_types)]
pub type tickertape_status = i32;

/// Status codes returned as `tickertape_status`.
#[allow(non_camel_case_types)]
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum tickertape_status_code {
    /// Success.
    TICKERTAPE_OK = 0,
    /// Success, but there is nothing to return: no record is committed at the reader's position yet, the timeout
    /// passed, or (durable-only readers) the next record is not covered by a checkpoint yet; or there is no valid
    /// checkpoint marker. Output arguments are left unchanged, except where a function says otherwise
    /// (`tickertape_reader_read_batch` sets `*count_out` to 0). Not an error; the last error is not set.
    TICKERTAPE_NONE = 1,
    /// A required pointer was NULL, a value was out of range, or a reserved field was not zero.
    TICKERTAPE_E_INVALID_ARGUMENT = -1,
    /// An operating-system call failed. `tickertape_last_error_os_code` returns its `errno`.
    TICKERTAPE_E_IO = -2,
    /// An operating-system call failed because the filesystem is full (`ENOSPC`, `EDQUOT`). An append that fails
    /// this way leaves the journal consistent and can be retried once space is available. A failed clean-up of an
    /// aborted fill, and every append the writer refuses after it, is `TICKERTAPE_E_IO` instead, whatever the OS
    /// error: freeing space does not make that writer accept appends again.
    TICKERTAPE_E_STORAGE_FULL = -3,
    /// Another writer holds the journal's lock.
    TICKERTAPE_E_LOCKED = -4,
    /// Bytes in the journal failed validation. `tickertape_last_error_corrupt_reason` says why. A reader's cursor
    /// does not move, so calling again retries the same position.
    TICKERTAPE_E_CORRUPT = -5,
    /// A segment was written by an incompatible format version.
    TICKERTAPE_E_UNSUPPORTED_VERSION = -6,
    /// The directory exists but contains no journal segments (for example, the writer has not created the first one
    /// yet). A directory that does not exist is `TICKERTAPE_E_IO` with `ENOENT` instead.
    TICKERTAPE_E_NOT_FOUND = -7,
    /// The requested sequence number is older than the oldest segment in the journal.
    TICKERTAPE_E_SEQ_UNAVAILABLE = -8,
    /// The payload does not fit in one segment.
    TICKERTAPE_E_RECORD_TOO_LARGE = -9,
    /// An earlier checkpoint failed, or the clean-up after an aborted fill callback failed, so this writer can no
    /// longer guarantee durability. After a failed checkpoint appends still work. See `docs/crash-contract.md` →
    /// Errors are sticky.
    TICKERTAPE_E_DURABILITY_LOST = -10,
    /// Sequence numbers (or checkpoint generations) are exhausted.
    TICKERTAPE_E_SEQUENCE_EXHAUSTED = -11,
    /// Writer recovery or retention replaced or removed the segment at the reader's position. Records from here on
    /// may belong to a discarded history: call `tickertape_reader_seek` to continue on the current chain.
    TICKERTAPE_E_DISCARDED = -12,
    /// A configuration value is out of range (for example a segment size that is not a multiple of 64 KiB).
    TICKERTAPE_E_INVALID_CONFIG = -13,
    /// A caller-provided buffer or array is too small. The function says what it reports about the size needed.
    TICKERTAPE_E_BUFFER_TOO_SMALL = -14,
    /// A fill callback returned non-zero; nothing was appended and the writer cleaned up. (If the clean-up fails, the
    /// call returns `TICKERTAPE_E_IO` instead; see `tickertape_writer_append_with`.)
    TICKERTAPE_E_ABORTED = -15,
    /// The handle is already inside a call on this thread, for example when a fill callback calls back into the
    /// writer that invoked it. Nothing was done.
    TICKERTAPE_E_BUSY = -16,
    /// An internal error (a bug in tickertape) was caught. The handle involved is broken: later calls on it return
    /// this code; close it.
    TICKERTAPE_E_PANIC = -17,
    /// An error this version of the C ABI has no specific code for. Its value stays far from the assigned codes,
    /// which later versions extend downwards from -17.
    TICKERTAPE_E_UNKNOWN = -255,
}

/// Why journal bytes failed validation, returned by `tickertape_last_error_corrupt_reason` and in verify reports.
/// See `docs/format.md` for the rules.
#[allow(non_camel_case_types)]
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum tickertape_corrupt_reason {
    /// No corruption (the last error was not `TICKERTAPE_E_CORRUPT`).
    TICKERTAPE_CORRUPT_NONE = 0,
    /// The segment header's magic bytes are wrong.
    TICKERTAPE_CORRUPT_BAD_MAGIC = 1,
    /// The segment header failed its checksum or one of its fields is out of range.
    TICKERTAPE_CORRUPT_BAD_SEGMENT_HEADER = 2,
    /// The segment file is shorter or longer than its header says.
    TICKERTAPE_CORRUPT_SIZE_MISMATCH = 3,
    /// The file name and the header's base sequence number disagree.
    TICKERTAPE_CORRUPT_BASE_SEQ_MISMATCH = 4,
    /// A slot is misaligned, before the record area, or outside the segment.
    TICKERTAPE_CORRUPT_BAD_OFFSET = 5,
    /// A record has no representable next sequence number.
    TICKERTAPE_CORRUPT_SEQUENCE_EXHAUSTED = 6,
    /// A record's length is out of range.
    TICKERTAPE_CORRUPT_BAD_LENGTH = 7,
    /// A record's CRC32C does not match.
    TICKERTAPE_CORRUPT_BAD_CHECKSUM = 8,
    /// A record has an unexpected sequence number.
    TICKERTAPE_CORRUPT_BAD_SEQUENCE = 9,
    /// A reserved field is not zero.
    TICKERTAPE_CORRUPT_BAD_FLAGS = 10,
    /// A seal is invalid (its checksum does not match the expected next sequence number, or it lies where a seal
    /// cannot), or writer recovery found non-zero bytes beyond the place of a missing seal in front of a successor
    /// segment that opens (`docs/crash-contract.md` C11).
    TICKERTAPE_CORRUPT_BAD_SEAL = 11,
    /// A sealed segment's successor is missing while later segments exist, or does not start where the seal says.
    TICKERTAPE_CORRUPT_BROKEN_CHAIN = 12,
    /// The checkpoint marker points to a missing segment or an impossible offset.
    TICKERTAPE_CORRUPT_DANGLING_CHECKPOINT = 13,
    /// A commit word reads 0, and recovery or verify found evidence of committed records after it (usually storage
    /// damage; a torn payload can also cause it: `docs/crash-contract.md` C1, C11; ADR 0035). The error message
    /// names the sequence number the evidence carries: a later valid record's, the next sequence number of a seal
    /// that ends the segment, or the base of a later segment.
    TICKERTAPE_CORRUPT_EMPTY_COMMIT_WORD = 14,
    /// A commit word reads 0, and the bytes after it were too costly to check completely (the search for committed
    /// records stopped at its work bound; ADR 0035). Inconclusive, but treated like
    /// `TICKERTAPE_CORRUPT_EMPTY_COMMIT_WORD`: the writer preserves the bytes or refuses. The error message names a
    /// sequence number: the one in the header of the candidate record where the search stopped, or the expected one
    /// plus one when it stopped at a final seal.
    TICKERTAPE_CORRUPT_UNCHECKED_TAIL = 15,
    /// A reason this version of the C ABI has no specific code for.
    TICKERTAPE_CORRUPT_UNKNOWN = 255,
}

use tickertape_corrupt_reason as R;
use tickertape_status_code as S;

impl From<S> for tickertape_status {
    fn from(code: S) -> tickertape_status {
        code as tickertape_status
    }
}

/// An error on its way to C: the status code plus what `tickertape_last_error_*` report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FfiError {
    pub(crate) code: S,
    message: String,
    os_code: i32,
    corrupt_reason: R,
}

impl FfiError {
    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    /// The same error with another message (the OS error and corruption reason are kept).
    pub(crate) fn with_message(mut self, message: impl Into<String>) -> FfiError {
        self.message = message.into();
        self
    }

    pub(crate) fn new(code: S, message: impl Into<String>) -> FfiError {
        FfiError {
            code,
            message: message.into(),
            os_code: 0,
            corrupt_reason: R::TICKERTAPE_CORRUPT_NONE,
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> FfiError {
        FfiError::new(S::TICKERTAPE_E_INVALID_ARGUMENT, message)
    }

    pub(crate) fn null(name: &str) -> FfiError {
        FfiError::invalid(format!("{name} must not be NULL"))
    }

    fn panic(payload: &(dyn std::any::Any + Send)) -> FfiError {
        let detail = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("unknown cause");
        FfiError::new(
            S::TICKERTAPE_E_PANIC,
            format!("internal error (a bug in tickertape, please report it): {detail}"),
        )
    }
}

/// The status code for a core error.
pub(crate) fn status_of(error: &Error) -> S {
    match error {
        Error::Io { .. } if error.is_storage_full() => S::TICKERTAPE_E_STORAGE_FULL,
        Error::Io { .. } => S::TICKERTAPE_E_IO,
        Error::Locked { .. } => S::TICKERTAPE_E_LOCKED,
        Error::Corrupt { .. } => S::TICKERTAPE_E_CORRUPT,
        Error::UnsupportedVersion { .. } => S::TICKERTAPE_E_UNSUPPORTED_VERSION,
        Error::NotFound { .. } => S::TICKERTAPE_E_NOT_FOUND,
        Error::SeqUnavailable { .. } => S::TICKERTAPE_E_SEQ_UNAVAILABLE,
        Error::RecordTooLarge { .. } => S::TICKERTAPE_E_RECORD_TOO_LARGE,
        Error::DurabilityLost => S::TICKERTAPE_E_DURABILITY_LOST,
        Error::SequenceExhausted => S::TICKERTAPE_E_SEQUENCE_EXHAUSTED,
        Error::Discarded { .. } => S::TICKERTAPE_E_DISCARDED,
        Error::InvalidConfig(_) => S::TICKERTAPE_E_INVALID_CONFIG,
        _ => S::TICKERTAPE_E_UNKNOWN,
    }
}

/// The code for a corruption reason.
pub(crate) fn corrupt_reason_of(reason: &CorruptReason) -> R {
    match reason {
        CorruptReason::BadMagic => R::TICKERTAPE_CORRUPT_BAD_MAGIC,
        CorruptReason::BadSegmentHeader(_) => R::TICKERTAPE_CORRUPT_BAD_SEGMENT_HEADER,
        CorruptReason::SizeMismatch { .. } => R::TICKERTAPE_CORRUPT_SIZE_MISMATCH,
        CorruptReason::BaseSeqMismatch { .. } => R::TICKERTAPE_CORRUPT_BASE_SEQ_MISMATCH,
        CorruptReason::BadOffset(_) => R::TICKERTAPE_CORRUPT_BAD_OFFSET,
        CorruptReason::SequenceExhausted => R::TICKERTAPE_CORRUPT_SEQUENCE_EXHAUSTED,
        CorruptReason::BadLength(_) => R::TICKERTAPE_CORRUPT_BAD_LENGTH,
        CorruptReason::BadChecksum => R::TICKERTAPE_CORRUPT_BAD_CHECKSUM,
        CorruptReason::BadSequence { .. } => R::TICKERTAPE_CORRUPT_BAD_SEQUENCE,
        CorruptReason::BadFlags => R::TICKERTAPE_CORRUPT_BAD_FLAGS,
        CorruptReason::BadSeal => R::TICKERTAPE_CORRUPT_BAD_SEAL,
        CorruptReason::BrokenChain(_) => R::TICKERTAPE_CORRUPT_BROKEN_CHAIN,
        CorruptReason::DanglingCheckpoint(_) => R::TICKERTAPE_CORRUPT_DANGLING_CHECKPOINT,
        CorruptReason::EmptyCommitWord(_) => R::TICKERTAPE_CORRUPT_EMPTY_COMMIT_WORD,
        CorruptReason::UncheckedTail(_) => R::TICKERTAPE_CORRUPT_UNCHECKED_TAIL,
        _ => R::TICKERTAPE_CORRUPT_UNKNOWN,
    }
}

/// The `errno` behind an I/O error, or 0. Looks one level inside a wrapping error: the core wraps the OS error of a
/// failed clean-up after an abandoned write (and of the appends refused after it) so that it never counts as
/// storage-full, and `tickertape_last_error_os_code` still reports it.
fn os_code_of(source: &std::io::Error) -> i32 {
    source
        .raw_os_error()
        .or_else(|| {
            source
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<std::io::Error>())
                .and_then(std::io::Error::raw_os_error)
        })
        .unwrap_or(0)
}

impl From<&Error> for FfiError {
    fn from(error: &Error) -> FfiError {
        let mut out = FfiError::new(status_of(error), error.to_string());
        match error {
            Error::Io { source, .. } => out.os_code = os_code_of(source),
            Error::Corrupt { reason, .. } => out.corrupt_reason = corrupt_reason_of(reason),
            _ => {}
        }
        out
    }
}

impl From<Error> for FfiError {
    fn from(error: Error) -> FfiError {
        FfiError::from(&error)
    }
}

/// What `tickertape_last_error_*` report on this thread.
struct LastError {
    code: S,
    message: CString,
    os_code: i32,
    corrupt_reason: R,
}

thread_local! {
    static LAST_ERROR: RefCell<LastError> = RefCell::new(LastError {
        code: S::TICKERTAPE_OK,
        message: CString::default(),
        os_code: 0,
        corrupt_reason: R::TICKERTAPE_CORRUPT_NONE,
    });
}

fn set_last_error(error: FfiError) {
    // Messages come from paths and OS errors, which contain no NUL bytes; replace any anyway.
    let message = CString::new(error.message.replace('\0', "?")).unwrap_or_default();
    // `try_with` fails only while this thread's locals are being destroyed; the code is still returned.
    let _ = LAST_ERROR.try_with(|last| {
        if let Ok(mut last) = last.try_borrow_mut() {
            *last = LastError {
                code: error.code,
                message,
                os_code: error.os_code,
                corrupt_reason: error.corrupt_reason,
            };
        }
    });
}

/// Runs an entry point: catches panics, records a failure as the thread's last error, and returns the status.
pub(crate) fn guard(body: impl FnOnce() -> Result<S, FfiError>) -> tickertape_status {
    let result =
        panic::catch_unwind(AssertUnwindSafe(body)).unwrap_or_else(|p| Err(FfiError::panic(&*p)));
    match result {
        Ok(code) => code.into(),
        Err(error) => {
            let code = error.code;
            set_last_error(error);
            code.into()
        }
    }
}

/// The state behind a writer or reader handle, with the checks that every call on it makes.
#[derive(Debug)]
pub(crate) struct Handle<T> {
    /// Set while a call on this handle runs, to detect re-entrant calls (from a callback) on the same thread.
    busy: Cell<bool>,
    /// Set when a panic unwound through a call on this handle; its state may be inconsistent.
    broken: Cell<bool>,
    inner: UnsafeCell<T>,
}

impl<T> Handle<T> {
    pub(crate) fn new(inner: T) -> Handle<T> {
        Handle {
            busy: Cell::new(false),
            broken: Cell::new(false),
            inner: UnsafeCell::new(inner),
        }
    }

    /// Runs `body` with exclusive access to the state, unless the handle is busy or broken.
    pub(crate) fn with<R>(
        &self,
        body: impl FnOnce(&mut T) -> Result<R, FfiError>,
    ) -> Result<R, FfiError> {
        if self.broken.get() {
            return Err(FfiError::new(
                S::TICKERTAPE_E_PANIC,
                "this handle is broken by an earlier internal error; close it",
            ));
        }
        if self.busy.replace(true) {
            return Err(FfiError::new(
                S::TICKERTAPE_E_BUSY,
                "this handle is already inside a call on this thread",
            ));
        }
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: `busy` was false and is now true, so no other `with` call on this thread holds a reference to
            // the state, and the C ABI forbids concurrent calls on one handle from several threads. The reference
            // does not outlive this closure.
            body(unsafe { &mut *self.inner.get() })
        }));
        self.busy.set(false);
        result.unwrap_or_else(|p| {
            self.broken.set(true);
            Err(FfiError::panic(&*p))
        })
    }

    /// Runs `body` with shared access to the state, unless the handle is busy or broken. For read-only calls, which
    /// must not invalidate pointers that earlier calls handed out (`tickertape_reader_position` promises not to
    /// invalidate a borrowed record view).
    pub(crate) fn with_ref<R>(
        &self,
        body: impl FnOnce(&T) -> Result<R, FfiError>,
    ) -> Result<R, FfiError> {
        if self.broken.get() {
            return Err(FfiError::new(
                S::TICKERTAPE_E_PANIC,
                "this handle is broken by an earlier internal error; close it",
            ));
        }
        if self.busy.replace(true) {
            return Err(FfiError::new(
                S::TICKERTAPE_E_BUSY,
                "this handle is already inside a call on this thread",
            ));
        }
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: `busy` was false and is now true, so no `with` call on this thread holds a mutable reference to
            // the state, and the C ABI forbids concurrent calls on one handle from several threads. Only a shared
            // reference is created, so raw pointers derived from earlier calls stay valid; it does not outlive this
            // closure.
            body(unsafe { &*self.inner.get() })
        }));
        self.busy.set(false);
        result.unwrap_or_else(|p| {
            self.broken.set(true);
            Err(FfiError::panic(&*p))
        })
    }

    /// Whether a call on this handle is running on this thread.
    pub(crate) fn is_busy(&self) -> bool {
        self.busy.get()
    }
}

/// Returns the code of the last error on the calling thread, or `TICKERTAPE_OK` if no call on this thread has
/// failed yet.
///
/// Every call that returns a negative status sets the last error; other calls leave it unchanged (like `errno`).
/// The state is per **operating-system thread**: read it on the same OS thread, right after the failing call. In a
/// runtime that moves its tasks between OS threads (Go goroutines, Java virtual threads), make the failing call and
/// the reads of the code, message, OS error and corruption reason in one foreign call sequence that cannot move,
/// for example a C shim that captures all four, or Go's `runtime.LockOSThread`.
#[unsafe(no_mangle)]
pub extern "C" fn tickertape_last_error_code() -> tickertape_status {
    LAST_ERROR
        .try_with(|last| last.try_borrow().map_or(S::TICKERTAPE_OK, |l| l.code))
        .unwrap_or(S::TICKERTAPE_OK)
        .into()
}

/// Returns a description of the last error on the calling thread, or an empty string. Never NULL.
///
/// The string is owned by the library and stays valid until the next call on this thread that returns a negative
/// status, or until the thread exits. Do not free it. It is always valid UTF-8: a file name that is not valid UTF-8
/// appears with its invalid bytes replaced by U+FFFD, so do not parse paths out of it. Per OS thread, as for
/// `tickertape_last_error_code`.
#[unsafe(no_mangle)]
pub extern "C" fn tickertape_last_error_message() -> *const c_char {
    static EMPTY: &[u8] = b"\0";
    LAST_ERROR
        .try_with(|last| {
            last.try_borrow()
                .map_or(EMPTY.as_ptr().cast(), |l| l.message.as_ptr())
        })
        .unwrap_or(EMPTY.as_ptr().cast())
}

/// Returns the operating system's error number (`errno`) for the last error on the calling thread if it was
/// `TICKERTAPE_E_IO` or `TICKERTAPE_E_STORAGE_FULL`, and 0 otherwise.
#[unsafe(no_mangle)]
pub extern "C" fn tickertape_last_error_os_code() -> i32 {
    LAST_ERROR
        .try_with(|last| last.try_borrow().map_or(0, |l| l.os_code))
        .unwrap_or(0)
}

/// Returns the corruption reason (a `tickertape_corrupt_reason`) for the last error on the calling thread if it was
/// `TICKERTAPE_E_CORRUPT`, and `TICKERTAPE_CORRUPT_NONE` otherwise.
#[unsafe(no_mangle)]
pub extern "C" fn tickertape_last_error_corrupt_reason() -> i32 {
    LAST_ERROR
        .try_with(|last| {
            last.try_borrow()
                .map_or(R::TICKERTAPE_CORRUPT_NONE, |l| l.corrupt_reason)
        })
        .unwrap_or(R::TICKERTAPE_CORRUPT_NONE) as i32
}

macro_rules! names {
    ($value:expr, $ty:ident, $unknown:literal, [$($variant:ident),* $(,)?]) => {{
        const NAMES: &[($ty, &str)] = &[$(($ty::$variant, concat!(stringify!($variant), "\0"))),*];
        NAMES
            .iter()
            .find(|(code, _)| *code as i32 == $value)
            .map_or(concat!($unknown, "\0"), |(_, name)| *name)
            .as_ptr()
            .cast()
    }};
}

/// Returns the name of a status code, such as `"TICKERTAPE_E_IO"`, or `"TICKERTAPE_UNKNOWN_STATUS"`. The string is
/// static; never NULL.
#[unsafe(no_mangle)]
pub extern "C" fn tickertape_status_name(status: tickertape_status) -> *const c_char {
    names!(
        status,
        S,
        "TICKERTAPE_UNKNOWN_STATUS",
        [
            TICKERTAPE_OK,
            TICKERTAPE_NONE,
            TICKERTAPE_E_INVALID_ARGUMENT,
            TICKERTAPE_E_IO,
            TICKERTAPE_E_STORAGE_FULL,
            TICKERTAPE_E_LOCKED,
            TICKERTAPE_E_CORRUPT,
            TICKERTAPE_E_UNSUPPORTED_VERSION,
            TICKERTAPE_E_NOT_FOUND,
            TICKERTAPE_E_SEQ_UNAVAILABLE,
            TICKERTAPE_E_RECORD_TOO_LARGE,
            TICKERTAPE_E_DURABILITY_LOST,
            TICKERTAPE_E_SEQUENCE_EXHAUSTED,
            TICKERTAPE_E_DISCARDED,
            TICKERTAPE_E_INVALID_CONFIG,
            TICKERTAPE_E_BUFFER_TOO_SMALL,
            TICKERTAPE_E_ABORTED,
            TICKERTAPE_E_BUSY,
            TICKERTAPE_E_PANIC,
            TICKERTAPE_E_UNKNOWN,
        ]
    )
}

/// Returns the name of a corruption reason, such as `"TICKERTAPE_CORRUPT_BAD_CHECKSUM"`, or
/// `"TICKERTAPE_CORRUPT_UNKNOWN"`. The string is static; never NULL.
#[unsafe(no_mangle)]
pub extern "C" fn tickertape_corrupt_reason_name(reason: i32) -> *const c_char {
    names!(
        reason,
        R,
        "TICKERTAPE_CORRUPT_UNKNOWN",
        [
            TICKERTAPE_CORRUPT_NONE,
            TICKERTAPE_CORRUPT_BAD_MAGIC,
            TICKERTAPE_CORRUPT_BAD_SEGMENT_HEADER,
            TICKERTAPE_CORRUPT_SIZE_MISMATCH,
            TICKERTAPE_CORRUPT_BASE_SEQ_MISMATCH,
            TICKERTAPE_CORRUPT_BAD_OFFSET,
            TICKERTAPE_CORRUPT_SEQUENCE_EXHAUSTED,
            TICKERTAPE_CORRUPT_BAD_LENGTH,
            TICKERTAPE_CORRUPT_BAD_CHECKSUM,
            TICKERTAPE_CORRUPT_BAD_SEQUENCE,
            TICKERTAPE_CORRUPT_BAD_FLAGS,
            TICKERTAPE_CORRUPT_BAD_SEAL,
            TICKERTAPE_CORRUPT_BROKEN_CHAIN,
            TICKERTAPE_CORRUPT_DANGLING_CHECKPOINT,
            TICKERTAPE_CORRUPT_EMPTY_COMMIT_WORD,
            TICKERTAPE_CORRUPT_UNCHECKED_TAIL,
            TICKERTAPE_CORRUPT_UNKNOWN,
        ]
    )
}
