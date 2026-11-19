//! `tickertape.Writer`.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

use pyo3::buffer::PyBuffer;
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::pybacked::PyBackedBytes;
use pyo3::sync::MutexExt;
use pyo3::types::{PyDict, PyList, PyRange};
use tickertape::{NewRecord, RecoveryAction, WriterConfig};

use crate::config::{self, Retention};
use crate::errors;

/// Locks a mutex without holding the GIL while waiting (no deadlock with a thread that holds the lock and needs
/// the GIL), ignoring poisoning: every guarded value stays consistent between calls into the core crate.
pub fn lock<'a, T>(py: Python<'_>, mutex: &'a Mutex<T>) -> MutexGuard<'a, T> {
    mutex
        .lock_py_attached(py)
        .unwrap_or_else(PoisonError::into_inner)
}

/// A payload extracted from Python: `bytes` are borrowed without copying (they are immutable); other buffers
/// (`bytearray`, `memoryview`, NumPy arrays, ...) are copied once, because they may change while the GIL is released.
enum Payload {
    Shared(PyBackedBytes),
    Copied(Vec<u8>),
}

impl Payload {
    fn extract(obj: &Bound<'_, PyAny>) -> PyResult<Payload> {
        if let Ok(bytes) = obj.extract::<PyBackedBytes>() {
            return Ok(Payload::Shared(bytes));
        }
        match PyBuffer::<u8>::get(obj) {
            Ok(buffer) => Ok(Payload::Copied(buffer.to_vec(obj.py())?)),
            Err(_) => Err(PyTypeError::new_err(format!(
                "payload must be bytes or a bytes-like object (buffer of unsigned bytes), not {}",
                obj.get_type().name()?
            ))),
        }
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            Payload::Shared(b) => b,
            Payload::Copied(v) => v,
        }
    }
}

/// A metadata argument of `append_many`: one value for every record, or one per record.
enum Column<T> {
    All(T),
    Each(Vec<T>),
}

impl<T: Copy + for<'a, 'py> FromPyObject<'a, 'py>> Column<T> {
    fn extract(obj: &Bound<'_, PyAny>, name: &str, len: usize) -> PyResult<Column<T>> {
        if let Ok(value) = obj.extract::<T>() {
            return Ok(Column::All(value));
        }
        if obj.is_instance_of::<pyo3::types::PyInt>() {
            // An int out of range: report the overflow, not a type error.
            return obj.extract::<T>().map(Column::All).map_err(Into::into);
        }
        let mut values = Vec::with_capacity(len);
        for item in obj.try_iter().map_err(|_| {
            PyTypeError::new_err(format!("{name} must be an int or a sequence of ints"))
        })? {
            values.push(item?.extract::<T>().map_err(Into::into)?);
        }
        if values.len() != len {
            return Err(PyValueError::new_err(format!(
                "{name} has {} values for {len} payloads",
                values.len()
            )));
        }
        Ok(Column::Each(values))
    }

    fn get(&self, i: usize) -> T {
        match self {
            Column::All(v) => *v,
            Column::Each(values) => values[i],
        }
    }
}

/// The single writer of a journal directory. Takes the journal's lock; a second writer gets `LockedError`.
#[pyclass(frozen, module = "tickertape")]
#[derive(Debug)]
pub struct Writer {
    inner: Mutex<Option<tickertape::Writer>>,
    path: PathBuf,
}

impl Writer {
    fn with<R>(
        &self,
        py: Python<'_>,
        f: impl FnOnce(&mut tickertape::Writer) -> PyResult<R>,
    ) -> PyResult<R> {
        let mut guard = lock(py, &self.inner);
        match guard.as_mut() {
            Some(writer) => f(writer),
            None => Err(errors::closed("writer")),
        }
    }
}

fn recovery_report(py: Python<'_>, report: &tickertape::RecoveryReport) -> PyResult<Py<PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("next_seq", report.next_seq)?;
    dict.set_item("scanned_from_seq", report.scanned_from_seq)?;
    dict.set_item("had_checkpoint", report.had_checkpoint)?;
    let actions = PyList::empty(py);
    for action in &report.actions {
        let a = PyDict::new(py);
        match action {
            RecoveryAction::Created => a.set_item("kind", "created")?,
            RecoveryAction::ZeroedUncommitted {
                segment,
                offset,
                len,
            } => {
                a.set_item("kind", "zeroed_uncommitted")?;
                a.set_item("segment", segment)?;
                a.set_item("offset", offset)?;
                a.set_item("len", len)?;
            }
            RecoveryAction::RepairedTail {
                segment,
                offset,
                len,
                quarantine_file,
                reason,
            } => {
                a.set_item("kind", "repaired_tail")?;
                a.set_item("segment", segment)?;
                a.set_item("offset", offset)?;
                a.set_item("len", len)?;
                a.set_item("quarantine_file", quarantine_file)?;
                a.set_item("reason", errors::reason_fields(py, reason)?.0)?;
            }
            RecoveryAction::QuarantinedSegment {
                segment,
                quarantine_file,
                reason,
            } => {
                a.set_item("kind", "quarantined_segment")?;
                a.set_item("segment", segment)?;
                a.set_item("quarantine_file", quarantine_file)?;
                a.set_item("reason", errors::reason_fields(py, reason)?.0)?;
            }
            RecoveryAction::Sealed { segment } => {
                a.set_item("kind", "sealed")?;
                a.set_item("segment", segment)?;
            }
            RecoveryAction::CompletedRoll { segment } => {
                a.set_item("kind", "completed_roll")?;
                a.set_item("segment", segment)?;
            }
            RecoveryAction::RemovedTemp { file } => {
                a.set_item("kind", "removed_temp")?;
                a.set_item("file", file)?;
            }
            other => {
                a.set_item("kind", "unknown")?;
                a.set_item("description", format!("{other:?}"))?;
            }
        }
        actions.append(a)?;
    }
    dict.set_item("actions", actions)?;
    Ok(dict.unbind())
}

#[pymethods]
impl Writer {
    /// Opens (creating if needed) the journal in `path` for writing and recovers it. Releases the GIL meanwhile.
    #[new]
    #[pyo3(signature = (
        path, *, segment_size=WriterConfig::DEFAULT_SEGMENT_SIZE, recovery="quarantine", notify=true, prefault=true,
        retention=None
    ))]
    fn new(
        py: Python<'_>,
        path: PathBuf,
        segment_size: u64,
        recovery: &str,
        notify: bool,
        prefault: bool,
        retention: Option<PyRef<'_, Retention>>,
    ) -> PyResult<Self> {
        let mut config = WriterConfig::default()
            .segment_size(segment_size)
            .recovery(config::recovery(recovery)?)
            .notify(notify)
            .prefault(prefault);
        if let Some(r) = retention {
            config = config.retention(r.policy.clone(), r.action.clone());
        }
        let open_path = path.clone();
        let writer = py
            .detach(move || tickertape::Writer::open(open_path, config))
            .map_err(|e| errors::to_py(py, &e))?;
        Ok(Writer {
            inner: Mutex::new(Some(writer)),
            path,
        })
    }

    /// Appends one record and returns its sequence number. The record is visible to readers (and survives this
    /// process dying) when this returns; it is durable only after a later `checkpoint()`.
    #[pyo3(signature = (payload, *, schema_id=0, venue_ts_ns=0, local_ts_ns=0))]
    fn append(
        &self,
        py: Python<'_>,
        payload: &Bound<'_, PyAny>,
        schema_id: u32,
        venue_ts_ns: u64,
        local_ts_ns: u64,
    ) -> PyResult<u64> {
        let payload = Payload::extract(payload)?;
        self.with(py, |w| {
            w.append(&NewRecord {
                schema_id,
                venue_ts_ns,
                local_ts_ns,
                payload: payload.as_slice(),
            })
            .map_err(|e| errors::to_py(py, &e))
        })
    }

    /// Appends a batch with one call across the language boundary and returns the range of assigned sequence
    /// numbers. All arguments are checked before anything is appended. If an append fails part-way, raises
    /// `BatchAppendError`: records `[0, appended)` are committed (with sequence numbers starting at `first_seq`),
    /// the rest are not, and the underlying error is its `__cause__`.
    #[pyo3(signature = (payloads, *, schema_id=None, venue_ts_ns=None, local_ts_ns=None))]
    fn append_many<'py>(
        &self,
        py: Python<'py>,
        payloads: &Bound<'py, PyAny>,
        schema_id: Option<&Bound<'py, PyAny>>,
        venue_ts_ns: Option<&Bound<'py, PyAny>>,
        local_ts_ns: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyRange>> {
        if payloads.is_instance_of::<pyo3::types::PyBytes>() {
            return Err(PyTypeError::new_err(
                "append_many takes an iterable of payloads, not a single bytes object",
            ));
        }
        let mut items = Vec::new();
        for item in payloads.try_iter()? {
            items.push(Payload::extract(&item?)?);
        }
        let n = items.len();
        let column = |obj: Option<&Bound<'py, PyAny>>, name| match obj {
            None => Ok(Column::All(0)),
            Some(o) => Column::<u64>::extract(o, name, n),
        };
        let schema_ids = match schema_id {
            None => Column::All(0),
            Some(o) => Column::<u32>::extract(o, "schema_id", n)?,
        };
        let venue = column(venue_ts_ns, "venue_ts_ns")?;
        let local = column(local_ts_ns, "local_ts_ns")?;

        let mut guard = lock(py, &self.inner);
        let writer = guard.as_mut().ok_or_else(|| errors::closed("writer"))?;
        let first = writer.next_seq();
        let (appended, failure) = py.detach(|| {
            for (i, payload) in items.iter().enumerate() {
                let record = NewRecord {
                    schema_id: schema_ids.get(i),
                    venue_ts_ns: venue.get(i),
                    local_ts_ns: local.get(i),
                    payload: payload.as_slice(),
                };
                if let Err(e) = writer.append(&record) {
                    return (i, Some(e));
                }
            }
            (items.len(), None)
        });
        drop(guard);
        if let Some(err) = failure {
            return Err(errors::batch_append(
                py,
                &err,
                appended,
                (appended > 0).then_some(first),
            ));
        }
        let end = first + appended as u64;
        PyRange::new(py, first as isize, end as isize).or_else(|_| {
            // Beyond isize: build the range from Python ints.
            py.import("builtins")?
                .getattr("range")?
                .call1((first, end))?
                .cast_into::<PyRange>()
                .map_err(Into::into)
        })
    }

    /// Makes every record appended so far durable (see `docs/crash-contract.md`). Blocks until the sync calls
    /// finish, with the GIL released. After a failure the writer is poisoned: later checkpoints raise
    /// `DurabilityLostError`, appends keep working.
    fn checkpoint(&self, py: Python<'_>) -> PyResult<()> {
        let mut guard = lock(py, &self.inner);
        let writer = guard.as_mut().ok_or_else(|| errors::closed("writer"))?;
        py.detach(|| writer.checkpoint())
            .map_err(|e| errors::to_py(py, &e))
    }

    /// The sequence number the next append will get.
    #[getter]
    fn next_seq(&self, py: Python<'_>) -> PyResult<u64> {
        self.with(py, |w| Ok(w.next_seq()))
    }

    /// The journal directory.
    #[getter]
    fn path(&self) -> PathBuf {
        self.path.clone()
    }

    /// True after `close()`.
    #[getter]
    fn closed(&self, py: Python<'_>) -> bool {
        lock(py, &self.inner).is_none()
    }

    /// What recovery did when this writer opened the journal, as a dict.
    #[getter]
    fn recovery_report(&self, py: Python<'_>) -> PyResult<Py<PyDict>> {
        self.with(py, |w| recovery_report(py, w.recovery_report()))
    }

    /// Cumulative retention removals since open, as a dict.
    #[getter]
    fn retention_report(&self, py: Python<'_>) -> PyResult<Py<PyDict>> {
        self.with(py, |w| {
            let r = w.retention_report();
            let dict = PyDict::new(py);
            dict.set_item("removed_segments", r.removed_segments)?;
            dict.set_item("removed_bytes", r.removed_bytes)?;
            dict.set_item("already_removed_segments", r.already_removed_segments)?;
            Ok(dict.unbind())
        })
    }

    /// The most recent retention error since open, as an exception instance (not raised), or `None`.
    #[getter]
    fn last_retention_error(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        self.with(py, |w| {
            Ok(w.last_retention_error()
                .map(|e| errors::to_py(py, e).into_value(py).into_any()))
        })
    }

    /// Releases the lock and closes the journal files, without a checkpoint: records appended since the last
    /// checkpoint stay visible but are not made durable. Calling it again does nothing.
    fn close(&self, py: Python<'_>) {
        let writer = lock(py, &self.inner).take();
        // Dropping joins helper threads; don't hold the GIL meanwhile.
        py.detach(move || drop(writer));
    }

    fn __enter__(slf: Bound<'_, Self>) -> PyResult<Bound<'_, Self>> {
        if slf.get().closed(slf.py()) {
            return Err(errors::closed("writer"));
        }
        Ok(slf)
    }

    /// Closes the writer (see `close()`); does not checkpoint and does not suppress exceptions.
    #[pyo3(signature = (*_args))]
    fn __exit__(&self, py: Python<'_>, _args: &Bound<'_, pyo3::types::PyTuple>) {
        self.close(py);
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        let state = match lock(py, &self.inner).as_ref() {
            Some(w) => format!("next_seq={}", w.next_seq()),
            None => "closed".to_string(),
        };
        format!(
            "<tickertape.Writer {:?} {state}>",
            self.path.display().to_string()
        )
    }
}
