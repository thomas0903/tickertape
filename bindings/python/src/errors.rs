//! Mapping of `tickertape::Error` and `tickertape::CorruptReason` to the Python exception hierarchy.
//!
//! The exception classes are defined in Python (`tickertape/_errors.py`), so that they can inherit from several
//! built-in exceptions (for example `RecordTooLargeError` is also a `ValueError`). This module looks them up on first
//! use and builds instances with keyword fields.
//!
//! Both enums are `#[non_exhaustive]`, so a `match` here needs a catch-all arm and the compiler cannot report a new
//! variant. `ERROR_VARIANTS` and `CORRUPT_REASONS` list every mapped variant by name; the Python test
//! `test_errors.py::test_every_rust_variant_is_mapped` parses `crates/tickertape/src/error.rs` and fails if a
//! variant is missing from these lists.

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyDict, PyType};
use tickertape::{CorruptReason, Error};

/// Every `tickertape::Error` variant and the Python exception class it maps to.
pub const ERROR_VARIANTS: &[(&str, &str)] = &[
    ("Io", "JournalIOError"),
    ("Locked", "LockedError"),
    ("Corrupt", "CorruptJournalError"),
    ("UnsupportedVersion", "UnsupportedVersionError"),
    ("NotFound", "JournalNotFoundError"),
    ("SeqUnavailable", "SeqUnavailableError"),
    ("RecordTooLarge", "RecordTooLargeError"),
    ("DurabilityLost", "DurabilityLostError"),
    ("SequenceExhausted", "SequenceExhaustedError"),
    ("Discarded", "DiscardedError"),
    ("InvalidConfig", "InvalidConfigError"),
];

/// Every `tickertape::CorruptReason` variant and its stable string code (`CorruptJournalError.reason`).
pub const CORRUPT_REASONS: &[(&str, &str)] = &[
    ("BadMagic", "bad_magic"),
    ("BadSegmentHeader", "bad_segment_header"),
    ("SizeMismatch", "size_mismatch"),
    ("BaseSeqMismatch", "base_seq_mismatch"),
    ("BadOffset", "bad_offset"),
    ("SequenceExhausted", "sequence_exhausted"),
    ("BadLength", "bad_length"),
    ("BadChecksum", "bad_checksum"),
    ("BadSequence", "bad_sequence"),
    ("BadFlags", "bad_flags"),
    ("BadSeal", "bad_seal"),
    ("BrokenChain", "broken_chain"),
    ("DanglingCheckpoint", "dangling_checkpoint"),
];

static ERRORS_MODULE: PyOnceLock<Py<PyModule>> = PyOnceLock::new();

/// Looks up an exception class defined in `tickertape._errors`.
pub fn class<'py>(py: Python<'py>, name: &str) -> PyResult<Bound<'py, PyType>> {
    let module =
        ERRORS_MODULE.get_or_try_init(py, || py.import("tickertape._errors").map(Bound::unbind))?;
    Ok(module.bind(py).getattr(name)?.cast_into::<PyType>()?)
}

/// Builds an exception instance `cls(message, **fields)`.
fn build(py: Python<'_>, name: &str, message: String, fields: &Bound<'_, PyDict>) -> PyErr {
    match class(py, name).and_then(|cls| cls.call((message,), Some(fields))) {
        Ok(instance) => PyErr::from_value(instance),
        Err(err) => err,
    }
}

/// The stable code and fields of a corruption reason.
pub fn reason_fields<'py>(
    py: Python<'py>,
    reason: &CorruptReason,
) -> PyResult<(&'static str, Bound<'py, PyDict>)> {
    let details = PyDict::new(py);
    let code = match *reason {
        CorruptReason::BadMagic => "bad_magic",
        CorruptReason::BadSegmentHeader(what) => {
            details.set_item("what", what)?;
            "bad_segment_header"
        }
        CorruptReason::SizeMismatch { expected, actual } => {
            details.set_item("expected", expected)?;
            details.set_item("actual", actual)?;
            "size_mismatch"
        }
        CorruptReason::BaseSeqMismatch {
            from_name,
            from_header,
        } => {
            details.set_item("from_name", from_name)?;
            details.set_item("from_header", from_header)?;
            "base_seq_mismatch"
        }
        CorruptReason::BadOffset(offset) => {
            details.set_item("offset", offset)?;
            "bad_offset"
        }
        CorruptReason::SequenceExhausted => "sequence_exhausted",
        CorruptReason::BadLength(length) => {
            details.set_item("length", length)?;
            "bad_length"
        }
        CorruptReason::BadChecksum => "bad_checksum",
        CorruptReason::BadSequence { expected, found } => {
            details.set_item("expected", expected)?;
            details.set_item("found", found)?;
            "bad_sequence"
        }
        CorruptReason::BadFlags => "bad_flags",
        CorruptReason::BadSeal => "bad_seal",
        CorruptReason::BrokenChain(next_seq) => {
            details.set_item("next_seq", next_seq)?;
            "broken_chain"
        }
        CorruptReason::DanglingCheckpoint(segment_base_seq) => {
            details.set_item("segment_base_seq", segment_base_seq)?;
            "dangling_checkpoint"
        }
        // A variant added to the core crate after this mapping was written. The test named in the module docs
        // fails until it is mapped; until then it is still reported, with this code.
        _ => "unknown",
    };
    Ok((code, details))
}

fn try_to_py(py: Python<'_>, err: &Error) -> PyResult<PyErr> {
    let message = err.to_string();
    let fields = PyDict::new(py);
    let name = match err {
        Error::Io { op, path, source } => {
            fields.set_item("op", *op)?;
            fields.set_item("filename", path)?;
            fields.set_item("errno", source.raw_os_error())?;
            fields.set_item("strerror", source.to_string())?;
            fields.set_item("storage_full", err.is_storage_full())?;
            if err.is_storage_full() {
                "StorageFullError"
            } else {
                "JournalIOError"
            }
        }
        Error::Locked { dir, pid } => {
            fields.set_item("path", dir)?;
            fields.set_item("pid", *pid)?;
            "LockedError"
        }
        Error::Corrupt {
            path,
            offset,
            reason,
        } => {
            let (code, details) = reason_fields(py, reason)?;
            fields.set_item("path", path)?;
            fields.set_item("offset", *offset)?;
            fields.set_item("reason", code)?;
            fields.set_item("details", details)?;
            "CorruptJournalError"
        }
        Error::UnsupportedVersion {
            path,
            found,
            supported,
        } => {
            fields.set_item("path", path)?;
            fields.set_item("found", *found)?;
            fields.set_item("supported", *supported)?;
            "UnsupportedVersionError"
        }
        Error::NotFound { dir } => {
            fields.set_item("path", dir)?;
            "JournalNotFoundError"
        }
        Error::SeqUnavailable { requested, first } => {
            fields.set_item("requested", *requested)?;
            fields.set_item("first", *first)?;
            "SeqUnavailableError"
        }
        Error::RecordTooLarge { len, max } => {
            fields.set_item("length", *len)?;
            fields.set_item("max", *max)?;
            "RecordTooLargeError"
        }
        Error::DurabilityLost => "DurabilityLostError",
        Error::SequenceExhausted => "SequenceExhaustedError",
        Error::Discarded { seq } => {
            fields.set_item("seq", *seq)?;
            "DiscardedError"
        }
        Error::InvalidConfig(_) => "InvalidConfigError",
        // See `reason_fields`: reported as the base class until mapped.
        _ => "TickertapeError",
    };
    Ok(build(py, name, message, &fields))
}

/// Converts a core error into the matching Python exception.
pub fn to_py(py: Python<'_>, err: &Error) -> PyErr {
    try_to_py(py, err).unwrap_or_else(|e| e)
}

/// `BatchAppendError(message, appended=..., first_seq=..., failed_index=...)` caused by `cause`.
pub fn batch_append(
    py: Python<'_>,
    cause: &Error,
    appended: usize,
    first_seq: Option<u64>,
) -> PyErr {
    let inner = || -> PyResult<PyErr> {
        let cause = to_py(py, cause);
        let fields = PyDict::new(py);
        fields.set_item("appended", appended)?;
        fields.set_item("first_seq", first_seq)?;
        fields.set_item("failed_index", appended)?;
        let message = format!(
            "append_many stopped at index {appended} after appending {appended} record(s): {}",
            cause.value(py)
        );
        let err = build(py, "BatchAppendError", message, &fields);
        err.set_cause(py, Some(cause));
        Ok(err)
    };
    inner().unwrap_or_else(|e| e)
}

/// A `ValueError` for use after `close()`.
pub fn closed(what: &str) -> PyErr {
    pyo3::exceptions::PyValueError::new_err(format!("I/O operation on a closed {what}"))
}
