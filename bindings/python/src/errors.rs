//! Mapping of `tickertape::Error` and `tickertape::CorruptReason` to the Python exception hierarchy.
//!
//! The exception classes are defined in Python (`tickertape/_errors.py`), so that they can inherit from several
//! built-in exceptions (for example `RecordTooLargeError` is also a `ValueError`). Conversion happens in two steps:
//! [`describe`] turns a core error into an [`ErrorSpec`] (class name, message and fields) in plain Rust, and
//! [`ErrorSpec::into_py`] builds the Python exception. The first step needs no Python, so a caller can run it while
//! holding a lock and run the second (which executes Python code) after releasing it; the unit tests below check it
//! for an instance of every variant.
//!
//! Both enums are `#[non_exhaustive]`, so a `match` here needs a catch-all arm and the compiler cannot report a new
//! variant. `ERROR_VARIANTS` and `CORRUPT_REASONS` list every mapped variant by name. The Python test
//! `test_errors.py::test_every_rust_variant_is_listed` parses `crates/tickertape/src/error.rs` and fails if a variant is
//! missing from these lists, and the unit tests below fail if a listed variant is not converted as the list says.

use std::path::PathBuf;

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

/// A field value of an exception, owned by Rust until the exception is built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    None,
    Bool(bool),
    U64(u64),
    I64(i64),
    Str(String),
    Path(PathBuf),
    Dict(Fields),
}

/// Named fields, in order.
pub type Fields = Vec<(&'static str, Value)>;

impl From<u64> for Value {
    fn from(v: u64) -> Self {
        Value::U64(v)
    }
}

impl From<usize> for Value {
    fn from(v: usize) -> Self {
        // A usize is at most 64 bits on every supported target.
        u64::try_from(v).map_or(Value::None, Value::U64)
    }
}

impl From<u32> for Value {
    fn from(v: u32) -> Self {
        Value::U64(v.into())
    }
}

impl From<u16> for Value {
    fn from(v: u16) -> Self {
        Value::U64(v.into())
    }
}

impl From<&std::path::Path> for Value {
    fn from(v: &std::path::Path) -> Self {
        Value::Path(v.to_path_buf())
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::None, Into::into)
    }
}

impl Value {
    pub fn to_py<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        Ok(match self {
            Value::None => py.None().into_bound(py),
            Value::Bool(b) => b.into_pyobject(py)?.to_owned().into_any(),
            Value::U64(v) => v.into_pyobject(py)?.into_any(),
            Value::I64(v) => v.into_pyobject(py)?.into_any(),
            Value::Str(s) => s.into_pyobject(py)?.into_any(),
            Value::Path(p) => p.into_pyobject(py)?.into_any(),
            Value::Dict(fields) => fields_to_dict(py, fields)?.into_any(),
        })
    }
}

pub fn fields_to_dict<'py>(py: Python<'py>, fields: &Fields) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    for (name, value) in fields {
        dict.set_item(name, value.to_py(py)?)?;
    }
    Ok(dict)
}

/// A Python exception described in plain Rust: `class(message, **fields)` from `tickertape._errors`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorSpec {
    pub class: &'static str,
    pub message: String,
    pub fields: Fields,
}

impl ErrorSpec {
    /// Builds the exception. Runs Python code: call it without holding a lock that Python code could need.
    pub fn into_py(self, py: Python<'_>) -> PyErr {
        let build = || -> PyResult<PyErr> {
            let fields = fields_to_dict(py, &self.fields)?;
            let instance = class(py, self.class)?.call((self.message.as_str(),), Some(&fields))?;
            Ok(PyErr::from_value(instance))
        };
        build().unwrap_or_else(|e| e)
    }
}

static ERRORS_MODULE: PyOnceLock<Py<PyModule>> = PyOnceLock::new();

/// Looks up an exception class defined in `tickertape._errors`.
pub fn class<'py>(py: Python<'py>, name: &str) -> PyResult<Bound<'py, PyType>> {
    let module =
        ERRORS_MODULE.get_or_try_init(py, || py.import("tickertape._errors").map(Bound::unbind))?;
    Ok(module.bind(py).getattr(name)?.cast_into::<PyType>()?)
}

/// The stable code and fields of a corruption reason.
pub fn reason_fields(reason: &CorruptReason) -> (&'static str, Fields) {
    let mut d: Fields = Vec::new();
    let code = match *reason {
        CorruptReason::BadMagic => "bad_magic",
        CorruptReason::BadSegmentHeader(what) => {
            d.push(("what", Value::Str(what.to_string())));
            "bad_segment_header"
        }
        CorruptReason::SizeMismatch { expected, actual } => {
            d.push(("expected", expected.into()));
            d.push(("actual", actual.into()));
            "size_mismatch"
        }
        CorruptReason::BaseSeqMismatch {
            from_name,
            from_header,
        } => {
            d.push(("from_name", from_name.into()));
            d.push(("from_header", from_header.into()));
            "base_seq_mismatch"
        }
        CorruptReason::BadOffset(offset) => {
            d.push(("offset", offset.into()));
            "bad_offset"
        }
        CorruptReason::SequenceExhausted => "sequence_exhausted",
        CorruptReason::BadLength(length) => {
            d.push(("length", length.into()));
            "bad_length"
        }
        CorruptReason::BadChecksum => "bad_checksum",
        CorruptReason::BadSequence { expected, found } => {
            d.push(("expected", expected.into()));
            d.push(("found", found.into()));
            "bad_sequence"
        }
        CorruptReason::BadFlags => "bad_flags",
        CorruptReason::BadSeal => "bad_seal",
        CorruptReason::BrokenChain(next_seq) => {
            d.push(("next_seq", next_seq.into()));
            "broken_chain"
        }
        CorruptReason::DanglingCheckpoint(segment_base_seq) => {
            d.push(("segment_base_seq", segment_base_seq.into()));
            "dangling_checkpoint"
        }
        // A variant added to the core crate after this mapping was written. The tests named in the module docs
        // fail until it is mapped; until then it is still reported, with this code.
        _ => "unknown",
    };
    (code, d)
}

/// The OS message of an I/O error, without Rust's " (os error N)" suffix (the same text as `os.strerror(errno)`).
fn strerror(source: &std::io::Error) -> String {
    let text = source.to_string();
    match source.raw_os_error() {
        Some(code) => text
            .strip_suffix(&format!(" (os error {code})"))
            .map_or_else(|| text.clone(), str::to_string),
        None => text,
    }
}

/// Describes a core error as the Python exception it maps to.
pub fn describe(err: &Error) -> ErrorSpec {
    let mut f: Fields = Vec::new();
    let class = match err {
        Error::Io { op, path, source } => {
            f.push(("op", Value::Str((*op).to_string())));
            f.push(("filename", path.as_path().into()));
            f.push((
                "errno",
                source
                    .raw_os_error()
                    .map_or(Value::None, |e| Value::I64(e.into())),
            ));
            f.push(("strerror", Value::Str(strerror(source))));
            f.push(("storage_full", Value::Bool(err.is_storage_full())));
            if err.is_storage_full() {
                "StorageFullError"
            } else {
                "JournalIOError"
            }
        }
        Error::Locked { dir, pid } => {
            f.push(("path", dir.as_path().into()));
            f.push(("pid", (*pid).into()));
            "LockedError"
        }
        Error::Corrupt {
            path,
            offset,
            reason,
        } => {
            let (code, details) = reason_fields(reason);
            f.push(("path", path.as_path().into()));
            f.push(("offset", (*offset).into()));
            f.push(("reason", Value::Str(code.to_string())));
            f.push(("details", Value::Dict(details)));
            "CorruptJournalError"
        }
        Error::UnsupportedVersion {
            path,
            found,
            supported,
        } => {
            f.push(("path", path.as_path().into()));
            f.push(("found", (*found).into()));
            f.push(("supported", (*supported).into()));
            "UnsupportedVersionError"
        }
        Error::NotFound { dir } => {
            f.push(("path", dir.as_path().into()));
            "JournalNotFoundError"
        }
        Error::SeqUnavailable { requested, first } => {
            f.push(("requested", (*requested).into()));
            f.push(("first", (*first).into()));
            "SeqUnavailableError"
        }
        Error::RecordTooLarge { len, max } => {
            f.push(("length", (*len).into()));
            f.push(("max", (*max).into()));
            "RecordTooLargeError"
        }
        Error::DurabilityLost => "DurabilityLostError",
        Error::SequenceExhausted => "SequenceExhaustedError",
        Error::Discarded { seq } => {
            f.push(("seq", (*seq).into()));
            "DiscardedError"
        }
        Error::InvalidConfig(_) => "InvalidConfigError",
        // See `reason_fields`: reported as the base class until mapped.
        _ => "TickertapeError",
    };
    ErrorSpec {
        class,
        message: err.to_string(),
        fields: f,
    }
}

/// Converts a core error into the matching Python exception. Runs Python code (see [`ErrorSpec::into_py`]).
pub fn to_py(py: Python<'_>, err: &Error) -> PyErr {
    describe(err).into_py(py)
}

/// `BatchAppendError(message, appended=..., first_seq=..., failed_index=...)` caused by `cause`.
pub fn batch_append(
    py: Python<'_>,
    cause: &Error,
    appended: usize,
    first_seq: Option<u64>,
) -> PyErr {
    let cause_spec = describe(cause);
    let message = format!(
        "append_many stopped at index {appended} after appending {appended} record(s): {}",
        cause_spec.message
    );
    let spec = ErrorSpec {
        class: "BatchAppendError",
        message,
        fields: vec![
            ("appended", appended.into()),
            ("first_seq", first_seq.into()),
            ("failed_index", appended.into()),
        ],
    };
    let err = spec.into_py(py);
    err.set_cause(py, Some(cause_spec.into_py(py)));
    err
}

/// `UsedAfterForkError`: a handle opened in process `owner_pid` was used in process `pid` (a child made by `fork`).
pub fn used_after_fork(what: &str, owner_pid: u32, pid: u32) -> ErrorSpec {
    ErrorSpec {
        class: "UsedAfterForkError",
        message: format!(
            "this {what} was opened in process {owner_pid}{reused} and cannot be used in process {pid}, a child \
             created by fork(); open a new {what} in the child (or use the 'spawn' start method of multiprocessing)",
            reused = if owner_pid == pid {
                " (before a fork; the process id was reused)"
            } else {
                ""
            }
        ),
        fields: vec![("owner_pid", owner_pid.into()), ("pid", pid.into())],
    }
}

/// A `ValueError` for use after `close()`.
pub fn closed(what: &str) -> PyErr {
    pyo3::exceptions::PyValueError::new_err(format!("I/O operation on a closed {what}"))
}

/// A `ValueError` for a read that was waiting when another thread (or a signal handler) closed the reader.
pub fn closed_while_waiting() -> PyErr {
    pyo3::exceptions::PyValueError::new_err("the reader was closed while this read was waiting")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io;

    /// The variant name of a value, from its derived `Debug` output.
    fn variant<T: std::fmt::Debug>(value: &T) -> String {
        format!("{value:?}")
            .split(|c: char| !c.is_alphanumeric())
            .next()
            .unwrap()
            .to_string()
    }

    fn one_of_each_reason() -> Vec<CorruptReason> {
        vec![
            CorruptReason::BadMagic,
            CorruptReason::BadSegmentHeader("header_len"),
            CorruptReason::SizeMismatch {
                expected: 1,
                actual: 2,
            },
            CorruptReason::BaseSeqMismatch {
                from_name: 1,
                from_header: 2,
            },
            CorruptReason::BadOffset(8),
            CorruptReason::SequenceExhausted,
            CorruptReason::BadLength(3),
            CorruptReason::BadChecksum,
            CorruptReason::BadSequence {
                expected: 5,
                found: 7,
            },
            CorruptReason::BadFlags,
            CorruptReason::BadSeal,
            CorruptReason::BrokenChain(9),
            CorruptReason::DanglingCheckpoint(10),
        ]
    }

    fn one_of_each_error() -> Vec<Error> {
        vec![
            Error::Io {
                op: "open segment",
                path: "/j/0.tape".into(),
                source: io::Error::from_raw_os_error(2),
            },
            Error::Locked {
                dir: "/j".into(),
                pid: Some(42),
            },
            Error::Corrupt {
                path: "/j/0.tape".into(),
                offset: 64,
                reason: CorruptReason::BadChecksum,
            },
            Error::UnsupportedVersion {
                path: "/j/0.tape".into(),
                found: 2,
                supported: 1,
            },
            Error::NotFound { dir: "/j".into() },
            Error::SeqUnavailable {
                requested: 0,
                first: 5,
            },
            Error::RecordTooLarge { len: 10, max: 5 },
            Error::DurabilityLost,
            Error::SequenceExhausted,
            Error::Discarded { seq: 3 },
            Error::InvalidConfig("segment_size".into()),
        ]
    }

    #[test]
    fn every_listed_error_variant_converts_to_its_class() {
        let listed: BTreeMap<&str, &str> = ERROR_VARIANTS.iter().copied().collect();
        let mut seen = BTreeMap::new();
        for err in one_of_each_error() {
            let name = variant(&err);
            let spec = describe(&err);
            assert_eq!(
                listed.get(name.as_str()),
                Some(&spec.class),
                "{name} converts to {}, not the listed class",
                spec.class
            );
            assert_eq!(spec.message, err.to_string());
            seen.insert(name, spec.class);
        }
        assert_eq!(
            seen.keys().map(String::as_str).collect::<Vec<_>>(),
            listed.keys().copied().collect::<Vec<_>>(),
            "the test needs one instance of every listed variant"
        );
    }

    #[test]
    fn every_listed_corrupt_reason_converts_to_its_code() {
        let listed: BTreeMap<&str, &str> = CORRUPT_REASONS.iter().copied().collect();
        let mut seen = Vec::new();
        for reason in one_of_each_reason() {
            let name = variant(&reason);
            let (code, _) = reason_fields(&reason);
            assert_eq!(listed.get(name.as_str()), Some(&code), "{name}");
            // The code also reaches CorruptJournalError.reason.
            let spec = describe(&Error::Corrupt {
                path: "/p".into(),
                offset: 0,
                reason,
            });
            assert!(
                spec.fields
                    .contains(&("reason", Value::Str(code.to_string())))
            );
            seen.push(name);
        }
        seen.sort();
        assert_eq!(seen, listed.keys().copied().collect::<Vec<_>>());
    }

    #[test]
    fn io_errors_carry_the_os_message() {
        let spec = describe(&one_of_each_error().remove(0));
        let strerror = spec
            .fields
            .iter()
            .find(|(name, _)| *name == "strerror")
            .map(|(_, v)| v.clone());
        let Some(Value::Str(text)) = strerror else {
            panic!("no strerror: {spec:?}")
        };
        assert!(!text.contains("os error"), "{text}");
        assert!(spec.fields.contains(&("errno", Value::I64(2))));
    }
}
