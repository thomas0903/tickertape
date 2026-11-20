//! `tickertape.Writer`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};

use pyo3::buffer::PyBuffer;
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::pybacked::PyBackedBytes;
use pyo3::sync::MutexExt;
use pyo3::types::{
    PyByteArray, PyBytes, PyInt, PyIterator, PyList, PyMemoryView, PyRange, PyString, PyTuple,
    PyType,
};
use tickertape::{NewRecord, WriterConfig};

use crate::config::{self, Retention};
use crate::errors;
use crate::owner::Owner;
use crate::reports;

/// In a child created by `fork`: takes the value out of a handle's mutex without waiting. `Err(())` if the mutex is
/// locked: it was locked when the process forked (another thread was inside a call), and stays locked here because
/// that thread exists only in the parent. The value may then be half-updated, so it must never be used or dropped:
/// `close()` leaves it in place, and the handle's `Drop` forgets it (see `forget_inherited`).
pub fn take_inherited<T>(mutex: &Mutex<Option<T>>) -> Result<Option<T>, ()> {
    match mutex.try_lock() {
        Ok(mut guard) => Ok(guard.take()),
        Err(TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner().take()),
        Err(TryLockError::WouldBlock) => Err(()),
    }
}

/// In a handle's `Drop`, in a child created by `fork`: takes the value out of the mutex if the mutex can be taken,
/// and otherwise (it was locked at the fork, see `take_inherited`) forgets the value without running its destructor,
/// leaking what it holds. In practice a handle that was busy at the fork is never dropped in the child at all (the
/// frame of the call that held the mutex keeps a reference to it); this keeps a half-updated value from being dropped
/// if it ever is.
pub fn forget_inherited<T>(mutex: &mut Mutex<Option<T>>) -> Option<T> {
    match take_inherited(mutex) {
        Ok(value) => value,
        Err(()) => {
            let inner = mutex.get_mut().unwrap_or_else(PoisonError::into_inner);
            std::mem::forget(inner.take());
            None
        }
    }
}

/// Locks a mutex without holding the GIL while waiting (no deadlock with a thread that holds the lock and needs
/// the GIL), ignoring poisoning: every guarded value stays consistent between calls into the core crate.
pub fn lock<'a, T>(py: Python<'_>, mutex: &'a Mutex<T>) -> MutexGuard<'a, T> {
    mutex
        .lock_py_attached(py)
        .unwrap_or_else(PoisonError::into_inner)
}

/// A payload extracted from Python: `bytes` are borrowed without copying (they are immutable); other buffers of
/// unsigned bytes (`bytearray`, `memoryview`, `array('B')`, `uint8` NumPy arrays, ...) are copied once, because they
/// may change while the GIL is released. Buffers of other item types (`array('b')`, `int8` arrays, ...) are refused.
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
    /// Reads one value, or exactly `len` values from an iterable. Stops at the first value beyond `len`, so an
    /// endless iterator fails instead of filling memory.
    fn extract(obj: &Bound<'_, PyAny>, name: &str, len: usize) -> PyResult<Column<T>> {
        if let Ok(value) = obj.extract::<T>() {
            return Ok(Column::All(value));
        }
        if obj.is_instance_of::<PyInt>() {
            // An int out of range: report the overflow, not a type error.
            return obj.extract::<T>().map(Column::All).map_err(Into::into);
        }
        let mut values = Vec::with_capacity(len);
        for item in iterate(obj, name)? {
            // The iterator's own exception comes first (it may be KeyboardInterrupt).
            let item = item?;
            if values.len() == len {
                return Err(PyValueError::new_err(format!(
                    "{name} has more values than the {len} payloads"
                )));
            }
            values.push(item.extract::<T>().map_err(Into::into)?);
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

/// `iter(obj)` for a metadata argument. An exception raised by the object's own `__iter__` propagates unchanged (it
/// may be `KeyboardInterrupt`); if the interpreter cannot iterate the object at all, a `TypeError` naming the argument.
///
/// A `list` or `tuple` (exactly those types) is iterated without any lookup: their iterators cannot fail to be made.
/// For any other type, which of the two happened is decided before `iter()` is called, because the object's
/// `__iter__` may change its class before it raises. `iter()` decides as follows: if the type's iterator slot
/// (`tp_iter`) is filled, it calls it; otherwise, if the object passes the sequence check (`PySequence_Check`: a
/// sequence item slot, as a class's `__getitem__` fills it but a C type's mapping-only `__getitem__`, such as
/// `re.Match`'s, does not), it returns an index iterator; otherwise it refuses with `TypeError`. The interpreter fills
/// `tp_iter` from an `__iter__` in the dictionaries of the type and its bases, and `__iter__ = None` fills it with a
/// slot that refuses with `TypeError`. So with an `__iter__` that is not `None`, `iter()` runs it and whatever it
/// raises propagates. Without one, or with `__iter__ = None`, `iter()` runs no Python code of the object, and a
/// `TypeError` it raises is the interpreter's refusal, replaced by the binding's; anything else (`MemoryError`)
/// propagates.
///
/// The decision takes one attribute lookup on the type (`getattr(type(obj), "__iter__", None)`), which allocates
/// nothing for an ordinary class. Only if that finds no `__iter__`, or `None`, are the dictionaries along the MRO read
/// as well (`iter_slot`), because a metaclass can hide the name from the lookup while the slot still runs it; that
/// happens for objects that are not iterable, where the call fails anyway.
fn iterate<'py>(obj: &Bound<'py, PyAny>, name: &str) -> PyResult<Bound<'py, PyIterator>> {
    if obj.is_exact_instance_of::<PyList>() || obj.is_exact_instance_of::<PyTuple>() {
        return obj.try_iter();
    }
    let ty = obj.get_type();
    let own_iter = match ty.getattr_opt(intern!(obj.py(), "__iter__"))? {
        Some(iter) => !iter.is_none(),
        None => false,
    };
    if own_iter || iter_slot(&ty)? == Some(true) {
        return obj.try_iter();
    }
    obj.try_iter().map_err(|error| {
        if error.is_instance_of::<PyTypeError>(obj.py()) {
            PyTypeError::new_err(format!("{name} must be an int or a sequence of ints"))
        } else {
            error
        }
    })
}

/// The `__iter__` that fills `ty`'s iterator slot: `None` if no class in its MRO defines one (the slot is empty),
/// `Some(false)` if the first one is `None` (the slot refuses), `Some(true)` otherwise. Reads the dictionaries along
/// the MRO through `type`'s own descriptors, calling nothing on the object or its classes: an attribute lookup on the
/// type would go through its metaclass, which can hide or invent names.
fn iter_slot(ty: &Bound<'_, PyType>) -> PyResult<Option<bool>> {
    let type_dict = ty.py().get_type::<PyType>().getattr("__dict__")?;
    let mro = type_dict
        .get_item("__mro__")?
        .call_method1("__get__", (ty,))?;
    let dict_of = type_dict.get_item("__dict__")?;
    for base in mro.try_iter()? {
        let dict = dict_of.call_method1("__get__", (base?,))?;
        if dict.contains("__iter__")? {
            return Ok(Some(!dict.get_item("__iter__")?.is_none()));
        }
    }
    Ok(None)
}

/// Refuses a `payloads` argument that is itself one payload: `str` or any bytes-like object (iterating it would
/// yield ints or characters, and an empty one would silently append nothing).
fn check_payloads_container(payloads: &Bound<'_, PyAny>) -> PyResult<()> {
    if payloads.is_instance_of::<PyList>() || payloads.is_instance_of::<PyTuple>() {
        return Ok(());
    }
    let single = payloads.is_instance_of::<PyString>()
        || payloads.is_instance_of::<PyBytes>()
        || payloads.is_instance_of::<PyByteArray>()
        || payloads.is_instance_of::<PyMemoryView>()
        || payloads
            .py()
            .import("builtins")?
            .getattr("memoryview")?
            .call1((payloads,))
            .is_ok();
    if single {
        return Err(PyTypeError::new_err(format!(
            "append_many takes an iterable of payloads, not a single {} (use append for one payload, or pass \
             iter(...) to append the rows of an array)",
            payloads.get_type().name()?
        )));
    }
    Ok(())
}

/// The single writer of a journal directory. Takes the journal's lock; a second writer gets `LockedError`.
///
/// Locking: the core writer sits in a mutex, taken without the GIL and never held while Python code runs (errors and
/// reports are converted to Python objects after it is released). `closed` is a separate flag, readable without the
/// lock. A handle used in another process than the one that opened it (a child created by `fork`) raises
/// `UsedAfterForkError` and never touches the shared journal state there. Closing or dropping it there releases only
/// that process's copies of the descriptors and mappings (`tickertape::Writer::release_after_fork`), unless another
/// thread was inside a call at the fork: then nothing is released in that process (see `take_inherited`).
#[pyclass(frozen, module = "tickertape")]
#[derive(Debug)]
pub struct Writer {
    inner: Mutex<Option<tickertape::Writer>>,
    closed: AtomicBool,
    owner: Owner,
    path: PathBuf,
}

impl Writer {
    /// Raises `UsedAfterForkError` in a forked child, `ValueError` after `close()`. Called before consuming any
    /// argument.
    fn check(&self, py: Python<'_>) -> PyResult<()> {
        self.owner.check(py, "Writer")?;
        if self.closed.load(Ordering::Acquire) {
            return Err(errors::closed("writer"));
        }
        Ok(())
    }

    /// The way out of every method that returns a result (`Owner::settle`): builds the exception if there is one,
    /// then checks the owner, as the last thing the method does.
    fn settle<T>(&self, py: Python<'_>, body: impl FnOnce() -> PyResult<T>) -> PyResult<T> {
        self.owner.settle(py, "Writer", body)
    }

    /// Runs `f` on the open core writer, with the mutex held and the GIL held. `f` must not run Python code.
    fn with<R>(&self, py: Python<'_>, f: impl FnOnce(&mut tickertape::Writer) -> R) -> PyResult<R> {
        self.check(py)?;
        let mut guard = lock(py, &self.inner);
        match guard.as_mut() {
            Some(writer) => Ok(f(writer)),
            None => Err(errors::closed("writer")),
        }
    }

    /// Like `with`, but with the GIL released while `f` runs.
    fn with_detached<R: Send>(
        &self,
        py: Python<'_>,
        f: impl FnOnce(&mut tickertape::Writer) -> R + Send,
    ) -> PyResult<R> {
        self.check(py)?;
        py.detach(|| {
            let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            guard.as_mut().map(f)
        })
        .ok_or_else(|| errors::closed("writer"))
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if self.owner.is_foreign() {
            // A copy made by fork(): its helper threads don't exist here, and the lock, the files and the wake-up
            // region belong to the parent. Release only this process's descriptors and mappings, and nothing at all
            // if another thread was inside a call at the fork (the writer may be half-updated).
            if let Some(writer) = forget_inherited(&mut self.inner) {
                writer.release_after_fork();
            }
        }
    }
}

/// Converts a core result after the writer's lock has been released.
fn convert<T>(py: Python<'_>, result: Result<T, tickertape::Error>) -> PyResult<T> {
    result.map_err(|e| errors::to_py(py, &e))
}

#[pymethods]
impl Writer {
    /// Opens (creating if needed) the journal in `path` for writing and recovers it. Releases the GIL meanwhile.
    #[new]
    #[pyo3(
        signature = (
            path, *, segment_size=WriterConfig::DEFAULT_SEGMENT_SIZE, recovery="quarantine", notify=true,
            prefault=true, retention=None
        ),
        // Spelled out so that `inspect.signature` shows the default size (DEFAULT_SEGMENT_SIZE, checked by a test).
        text_signature = "(path, *, segment_size=134217728, recovery='quarantine', notify=True, prefault=True, \
                          retention=None)"
    )]
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
        let writer = convert(
            py,
            py.detach(move || tickertape::Writer::open(open_path, config)),
        )?;
        Ok(Writer {
            inner: Mutex::new(Some(writer)),
            closed: AtomicBool::new(false),
            owner: Owner::current(),
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
        self.settle(py, || {
            self.check(py)?;
            let payload = Payload::extract(payload)?;
            let result = self.with(py, |w| {
                w.append(&NewRecord {
                    schema_id,
                    venue_ts_ns,
                    local_ts_ns,
                    payload: payload.as_slice(),
                })
            })?;
            convert(py, result)
        })
    }

    /// Appends a batch with one call across the language boundary and returns the range of assigned sequence
    /// numbers. All arguments are converted and checked before anything is appended. If an append fails part-way,
    /// raises `BatchAppendError`: records `[0, appended)` are committed (with sequence numbers starting at
    /// `first_seq`), the rest are not, and the underlying error is its `__cause__`.
    #[pyo3(signature = (payloads, *, schema_id=None, venue_ts_ns=None, local_ts_ns=None))]
    fn append_many<'py>(
        &self,
        py: Python<'py>,
        payloads: &Bound<'py, PyAny>,
        schema_id: Option<&Bound<'py, PyAny>>,
        venue_ts_ns: Option<&Bound<'py, PyAny>>,
        local_ts_ns: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyRange>> {
        self.settle(py, || {
            self.check(py)?;
            check_payloads_container(payloads)?;
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

            let (first, appended, failure) = self.with_detached(py, |writer| {
                let first = writer.next_seq();
                for (i, payload) in items.iter().enumerate() {
                    let record = NewRecord {
                        schema_id: schema_ids.get(i),
                        venue_ts_ns: venue.get(i),
                        local_ts_ns: local.get(i),
                        payload: payload.as_slice(),
                    };
                    if let Err(e) = writer.append(&record) {
                        return (first, i, Some(e));
                    }
                }
                (first, items.len(), None)
            })?;
            if let Some(err) = failure {
                return Err(errors::batch_append(
                    py,
                    &err,
                    appended,
                    (appended > 0).then_some(first),
                ));
            }
            // The writer assigned `appended` consecutive numbers from `first`, so this cannot overflow.
            let end = u64::try_from(appended)
                .ok()
                .and_then(|n| first.checked_add(n))
                .ok_or_else(|| PyValueError::new_err("sequence number range overflows"))?;
            match (isize::try_from(first), isize::try_from(end)) {
                (Ok(start), Ok(stop)) => Ok(PyRange::new(py, start, stop)?),
                // Beyond isize: build the range from Python ints.
                _ => Ok(py
                    .import("builtins")?
                    .getattr("range")?
                    .call1((first, end))?
                    .cast_into::<PyRange>()?),
            }
        })
    }

    /// Makes every record appended so far durable (see `docs/crash-contract.md`). Blocks until the sync calls
    /// finish, with the GIL released. After a failure the writer is poisoned: later checkpoints raise
    /// `DurabilityLostError`, appends keep working. Records appended before a failed checkpoint may not be durable
    /// even if a writer opened later in the same boot checkpoints successfully.
    fn checkpoint(&self, py: Python<'_>) -> PyResult<()> {
        self.settle(py, || {
            let result = self.with_detached(py, tickertape::Writer::checkpoint)?;
            convert(py, result)
        })
    }

    /// The sequence number the next append will get.
    #[getter]
    fn next_seq(&self, py: Python<'_>) -> PyResult<u64> {
        self.settle(py, || self.with(py, |w| w.next_seq()))
    }

    /// The journal directory.
    #[getter]
    fn path(&self) -> PathBuf {
        self.path.clone()
    }

    /// True after `close()`. Never blocks.
    #[getter]
    fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// What recovery did when this writer opened the journal, as a `RecoveryReport`.
    #[getter]
    fn recovery_report<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.settle(py, || {
            let report = self.with(py, |w| w.recovery_report().clone())?;
            reports::recovery_report(py, &report)
        })
    }

    /// What only the writer knows, as a `WriterMetrics` (ADR 0032): its head, the last checkpoint by the monotonic
    /// clock, uncheckpointed records and bytes, whether it is poisoned, failed retention runs. No system calls.
    fn metrics<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.settle(py, || {
            let metrics = self.with(py, |w| w.metrics())?;
            crate::status::writer_metrics(py, &metrics)
        })
    }

    /// Cumulative retention removals since open, as a `RetentionReport`.
    #[getter]
    fn retention_report<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.settle(py, || {
            let report = self.with(py, |w| w.retention_report().clone())?;
            reports::retention_report(py, &report)
        })
    }

    /// The most recent retention error since open, as an exception instance (not raised), or `None`.
    #[getter]
    fn last_retention_error(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        self.settle(py, || {
            let spec = self.with(py, |w| w.last_retention_error().map(errors::describe))?;
            Ok(spec.map(|s| s.into_py(py).into_value(py).into_any()))
        })
    }

    /// Releases the lock and closes the journal files, without a checkpoint: records appended since the last
    /// checkpoint stay visible but are not made durable. Calling it again does nothing. In a child created by
    /// `fork`, it closes only this process's copies of the descriptors and mappings: the parent keeps the lock, and
    /// nothing is synced, joined or removed; if another thread was using the writer at the fork, it releases nothing
    /// there.
    fn close(&self, py: Python<'_>) {
        self.closed.store(true, Ordering::Release);
        if self.owner.is_foreign() {
            // A writer that was busy in another thread at the fork stays in place (see `take_inherited`).
            if let Ok(Some(writer)) = take_inherited(&self.inner) {
                writer.release_after_fork();
            }
            return;
        }
        let writer = lock(py, &self.inner).take();
        // Dropping joins helper threads; don't hold the GIL meanwhile.
        py.detach(move || drop(writer));
    }

    fn __enter__(slf: Bound<'_, Self>) -> PyResult<Bound<'_, Self>> {
        let (py, writer) = (slf.py(), slf.clone());
        writer.get().settle(py, move || {
            slf.get().check(py)?;
            Ok(slf)
        })
    }

    /// Test only: pretends this handle was opened by process `pid` (to simulate process-id reuse after `fork`).
    fn _set_owner_pid_for_testing(&self, pid: u32) {
        self.owner.set_pid_for_testing(pid);
    }

    /// Test only (feature `test-hooks`): locks this handle's mutex and never unlocks it, as if a thread had been
    /// inside a call when the process forked. In this process every later call that needs the lock then waits
    /// forever.
    #[cfg(feature = "test-hooks")]
    fn _lock_forever_for_testing(&self, py: Python<'_>) {
        std::mem::forget(lock(py, &self.inner));
    }

    /// Closes the writer (see `close()`); does not checkpoint and does not suppress exceptions.
    #[pyo3(signature = (*_args))]
    fn __exit__(&self, py: Python<'_>, _args: &Bound<'_, PyTuple>) {
        self.close(py);
    }

    /// Never blocks: shows `next_seq` only when no other call holds the writer.
    fn __repr__(&self) -> String {
        let state = if self.owner.is_foreign() {
            format!("opened in process {}", self.owner.pid())
        } else if self.closed() {
            "closed".to_string()
        } else {
            match self.inner.try_lock() {
                Ok(guard) => guard.as_ref().map_or_else(
                    || "closed".to_string(),
                    |w| format!("next_seq={}", w.next_seq()),
                ),
                Err(_) => "busy".to_string(),
            }
        };
        format!(
            "<tickertape.Writer {:?} {state}>",
            self.path.display().to_string()
        )
    }
}
