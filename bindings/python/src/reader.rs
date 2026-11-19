//! `tickertape.Reader`, `tickertape.Record` and `tickertape.RecordBatch`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use pyo3::exceptions::{PyIndexError, PyStopIteration, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyInt, PyList, PySlice, PyTuple};
use tickertape::{ReaderConfig, Record as CoreRecord};

use crate::config::{Backoff, Timeout};
use crate::errors;
use crate::owner::Owner;
use crate::writer::{lock, take_inherited};

/// Longest uninterrupted wait without the GIL, and the longest the reader's lock is held by a waiting read. Between
/// slices the binding releases the lock and checks for signals, so Ctrl-C (`KeyboardInterrupt`) interrupts a blocking
/// read, and other threads get the reader, within about this long.
const WAIT_SLICE: Duration = Duration::from_millis(50);

/// How long a waiting read, between two slices, lets other callers queued for the reader's lock go first. The mutex
/// is not fair: without this, the waiting read would usually take it back before a queued caller wakes up.
const HAND_OFF: Duration = Duration::from_millis(10);

/// A length or offset as `i64` (a `Vec` holds at most `isize::MAX` bytes, which fits on every supported target).
fn to_i64(n: usize) -> i64 {
    i64::try_from(n).expect("a Vec length fits in i64")
}

/// An offset written by this crate, back to `usize`.
fn to_usize(n: u64) -> usize {
    usize::try_from(n).expect("an offset into a Vec fits in usize")
}

/// One record, copied out of the journal. Its payload is an ordinary `bytes` object that the record owns, so it
/// stays valid after the reader moves on or is closed.
#[pyclass(frozen, module = "tickertape")]
#[derive(Debug)]
pub struct Record {
    /// Sequence number: the record's position in the journal, from 0, without gaps.
    #[pyo3(get)]
    seq: u64,
    /// Caller-defined schema id.
    #[pyo3(get)]
    schema_id: u32,
    /// Caller-supplied timestamp (nanoseconds by convention), 0 if not set.
    #[pyo3(get)]
    venue_ts_ns: u64,
    /// Caller-supplied timestamp (nanoseconds by convention), 0 if not set.
    #[pyo3(get)]
    local_ts_ns: u64,
    /// The payload bytes.
    #[pyo3(get)]
    payload: Py<PyBytes>,
}

impl Record {
    fn new(
        py: Python<'_>,
        seq: u64,
        schema_id: u32,
        venue: u64,
        local: u64,
        payload: &[u8],
    ) -> Self {
        Record {
            seq,
            schema_id,
            venue_ts_ns: venue,
            local_ts_ns: local,
            payload: PyBytes::new(py, payload).unbind(),
        }
    }
}

#[pymethods]
impl Record {
    /// `(seq, schema_id, venue_ts_ns, local_ts_ns, payload)`, the field order of the pure-Python decoder's `Record`.
    fn as_tuple<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(
            py,
            [
                self.seq.into_pyobject(py)?.into_any(),
                self.schema_id.into_pyobject(py)?.into_any(),
                self.venue_ts_ns.into_pyobject(py)?.into_any(),
                self.local_ts_ns.into_pyobject(py)?.into_any(),
                self.payload.bind(py).clone().into_any(),
            ],
        )
    }

    /// The fields as a dict (for `pandas.DataFrame.from_records` and similar).
    fn as_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("seq", self.seq)?;
        d.set_item("schema_id", self.schema_id)?;
        d.set_item("venue_ts_ns", self.venue_ts_ns)?;
        d.set_item("local_ts_ns", self.local_ts_ns)?;
        d.set_item("payload", self.payload.bind(py))?;
        Ok(d)
    }

    /// Field-wise equality; other types compare as `NotImplemented`.
    fn __eq__(&self, py: Python<'_>, other: PyRef<'_, Self>) -> bool {
        other.seq == self.seq
            && other.schema_id == self.schema_id
            && other.venue_ts_ns == self.venue_ts_ns
            && other.local_ts_ns == self.local_ts_ns
            && other.payload.bind(py).as_bytes() == self.payload.bind(py).as_bytes()
    }

    fn __hash__(&self, py: Python<'_>) -> PyResult<isize> {
        self.as_tuple(py)?.hash()
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        format!(
            "Record(seq={}, schema_id={}, venue_ts_ns={}, local_ts_ns={}, payload=<{} bytes>)",
            self.seq,
            self.schema_id,
            self.venue_ts_ns,
            self.local_ts_ns,
            self.payload.bind(py).as_bytes().len()
        )
    }
}

/// Records collected without the GIL, in columnar form (little-endian, which is every supported target's order).
#[derive(Debug)]
struct Columns {
    len: usize,
    seq: Vec<u8>,
    schema_id: Vec<u8>,
    venue_ts_ns: Vec<u8>,
    local_ts_ns: Vec<u8>,
    /// `len + 1` little-endian `i64` offsets into `data` (Arrow `large_binary` layout).
    offsets: Vec<u8>,
    data: Vec<u8>,
}

impl Columns {
    fn with_capacity(n: usize) -> Self {
        let mut offsets = Vec::with_capacity((n + 1) * 8);
        offsets.extend_from_slice(&0i64.to_le_bytes());
        Columns {
            len: 0,
            seq: Vec::with_capacity(n * 8),
            schema_id: Vec::with_capacity(n * 4),
            venue_ts_ns: Vec::with_capacity(n * 8),
            local_ts_ns: Vec::with_capacity(n * 8),
            offsets,
            data: Vec::new(),
        }
    }

    /// Empties the columns, keeping their capacity (unless it is large).
    fn clear(&mut self) {
        if self.data.capacity() > 1 << 20 || self.seq.capacity() > 1 << 16 {
            *self = Columns::with_capacity(0);
            return;
        }
        self.len = 0;
        self.seq.clear();
        self.schema_id.clear();
        self.venue_ts_ns.clear();
        self.local_ts_ns.clear();
        self.offsets.clear();
        self.offsets.extend_from_slice(&0i64.to_le_bytes());
        self.data.clear();
    }

    /// Room for `n` more records (payload bytes excluded).
    fn reserve(&mut self, n: usize) {
        self.seq.reserve(n * 8);
        self.schema_id.reserve(n * 4);
        self.venue_ts_ns.reserve(n * 8);
        self.local_ts_ns.reserve(n * 8);
        self.offsets.reserve(n * 8);
    }

    fn push(&mut self, r: &CoreRecord<'_>) {
        self.len += 1;
        self.seq.extend_from_slice(&r.seq.to_le_bytes());
        self.schema_id.extend_from_slice(&r.schema_id.to_le_bytes());
        self.venue_ts_ns
            .extend_from_slice(&r.venue_ts_ns.to_le_bytes());
        self.local_ts_ns
            .extend_from_slice(&r.local_ts_ns.to_le_bytes());
        self.data.extend_from_slice(r.payload);
        self.offsets
            .extend_from_slice(&to_i64(self.data.len()).to_le_bytes());
    }

    fn u64_at(col: &[u8], i: usize) -> u64 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&col[i * 8..i * 8 + 8]);
        u64::from_le_bytes(b)
    }

    fn u32_at(col: &[u8], i: usize) -> u32 {
        let mut b = [0u8; 4];
        b.copy_from_slice(&col[i * 4..i * 4 + 4]);
        u32::from_le_bytes(b)
    }

    fn offset_at(&self, i: usize) -> usize {
        to_usize(Columns::u64_at(&self.offsets, i))
    }

    fn payload(&self, i: usize) -> &[u8] {
        &self.data[self.offset_at(i)..self.offset_at(i + 1)]
    }

    fn record(&self, py: Python<'_>, i: usize) -> Record {
        Record::new(
            py,
            Columns::u64_at(&self.seq, i),
            Columns::u32_at(&self.schema_id, i),
            Columns::u64_at(&self.venue_ts_ns, i),
            Columns::u64_at(&self.local_ts_ns, i),
            self.payload(i),
        )
    }

    /// Records `[from, from + n)` as a `RecordBatch` (not yet a Python object). Allocates only `bytes` objects,
    /// which the garbage collector does not track, so no Python code runs.
    fn batch(&self, py: Python<'_>, from: usize, n: usize) -> RecordBatch {
        let (base, end) = (self.offset_at(from), self.offset_at(from + n));
        let offsets = if base == 0 {
            PyBytes::new(py, &self.offsets[from * 8..(from + n + 1) * 8])
        } else {
            // Offsets relative to the first payload of the batch.
            let mut rebased = Vec::with_capacity((n + 1) * 8);
            for i in from..=from + n {
                rebased.extend_from_slice(&to_i64(self.offset_at(i) - base).to_le_bytes());
            }
            PyBytes::new(py, &rebased)
        };
        RecordBatch {
            len: n,
            seq: PyBytes::new(py, &self.seq[from * 8..(from + n) * 8]).unbind(),
            schema_id: PyBytes::new(py, &self.schema_id[from * 4..(from + n) * 4]).unbind(),
            venue_ts_ns: PyBytes::new(py, &self.venue_ts_ns[from * 8..(from + n) * 8]).unbind(),
            local_ts_ns: PyBytes::new(py, &self.local_ts_ns[from * 8..(from + n) * 8]).unbind(),
            payload_offsets: offsets.unbind(),
            payload_data: PyBytes::new(py, &self.data[base..end]).unbind(),
        }
    }
}

/// Records read from the journal but not yet delivered to Python ("staged"), in front of the core reader's cursor.
///
/// A read moves records from the core reader into this buffer and builds its result from a copy, leaving them
/// staged; it removes them (`consume`) only once the Python result object is complete and no Python code can run
/// any more in the call. If a signal handler raises (`KeyboardInterrupt`) or anything else fails before that, the
/// records stay here and the next read returns them. `epoch` changes with every change to the buffer, so the
/// removal can confirm that the front still holds the records the result was built from.
#[derive(Debug)]
struct Buffer {
    columns: Columns,
    /// Records before `start` were already delivered.
    start: usize,
    epoch: u64,
}

impl Buffer {
    fn new() -> Self {
        Buffer {
            columns: Columns::with_capacity(0),
            start: 0,
            epoch: 0,
        }
    }

    fn len(&self) -> usize {
        self.columns.len - self.start
    }

    fn is_empty(&self) -> bool {
        self.start == self.columns.len
    }

    fn first_seq(&self) -> Option<u64> {
        (!self.is_empty()).then(|| Columns::u64_at(&self.columns.seq, self.start))
    }

    /// Empties the buffer, keeping its capacity for the next read.
    fn reset(&mut self) {
        self.columns.clear();
        self.start = 0;
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// Removes `n` delivered records from the front.
    fn consume(&mut self, n: usize) {
        self.start += n;
        self.epoch = self.epoch.wrapping_add(1);
        if self.is_empty() {
            self.reset();
        }
    }

    /// The first staged record, as a `Record` (not yet a Python object). Allocates only the payload's `bytes`, which
    /// the garbage collector does not track, so no Python code runs (this is called with the reader's lock held).
    fn record(&self, py: Python<'_>, i: usize) -> Record {
        self.columns.record(py, self.start + i)
    }

    /// The first `n` staged records as `Record`s; see `record`.
    fn records(&self, py: Python<'_>, n: usize) -> Vec<Record> {
        (0..n).map(|i| self.record(py, i)).collect()
    }

    /// The first `n` staged records as a `RecordBatch`; see `Columns::batch`.
    fn batch(&self, py: Python<'_>, n: usize) -> RecordBatch {
        self.columns.batch(py, self.start, n)
    }
}

/// What a read found staged, under the reader's lock.
enum Staged<T> {
    /// A result built from the first `count` staged records, which stay staged until `State::commit`.
    Records { value: T, epoch: u64, count: usize },
    /// No records were staged, but an error was held back; it has been taken.
    Error(tickertape::Error),
    /// Nothing: another caller took what `fill` found.
    Nothing,
}

/// A batch of records as columns, each an immutable `bytes` object owned by the batch.
///
/// `seq`, `venue_ts_ns` and `local_ts_ns` are little-endian `uint64`, `schema_id` little-endian `uint32`, and
/// payload `i` is `payload_data[payload_offsets[i]:payload_offsets[i + 1]]` with `len + 1` little-endian `int64`
/// offsets (Arrow's `large_binary` layout). `numpy.frombuffer` and `pyarrow.py_buffer` wrap these without copying;
/// the arrays keep the `bytes` alive. `to_numpy()`, `to_arrow()` and `to_pandas()` do that for you; NumPy, pyarrow
/// and pandas are optional and imported only when called.
#[pyclass(frozen, sequence, module = "tickertape")]
#[derive(Debug)]
pub struct RecordBatch {
    len: usize,
    /// `uint64` sequence numbers.
    #[pyo3(get)]
    seq: Py<PyBytes>,
    /// `uint32` schema ids.
    #[pyo3(get)]
    schema_id: Py<PyBytes>,
    /// `uint64` venue timestamps.
    #[pyo3(get)]
    venue_ts_ns: Py<PyBytes>,
    /// `uint64` local timestamps.
    #[pyo3(get)]
    local_ts_ns: Py<PyBytes>,
    /// `len + 1` `int64` payload offsets.
    #[pyo3(get)]
    payload_offsets: Py<PyBytes>,
    /// All payloads, concatenated.
    #[pyo3(get)]
    payload_data: Py<PyBytes>,
}

impl RecordBatch {
    fn record(&self, py: Python<'_>, i: usize) -> Record {
        let offsets = self.payload_offsets.bind(py).as_bytes();
        let (start, end) = (
            to_usize(Columns::u64_at(offsets, i)),
            to_usize(Columns::u64_at(offsets, i + 1)),
        );
        Record::new(
            py,
            Columns::u64_at(self.seq.bind(py).as_bytes(), i),
            Columns::u32_at(self.schema_id.bind(py).as_bytes(), i),
            Columns::u64_at(self.venue_ts_ns.bind(py).as_bytes(), i),
            Columns::u64_at(self.local_ts_ns.bind(py).as_bytes(), i),
            &self.payload_data.bind(py).as_bytes()[start..end],
        )
    }

    fn convert<'py>(slf: &Bound<'py, Self>, function: &str) -> PyResult<Bound<'py, PyAny>> {
        slf.py()
            .import("tickertape._columns")?
            .getattr(function)?
            .call1((slf,))
    }
}

#[pymethods]
impl RecordBatch {
    fn __len__(&self) -> usize {
        self.len
    }

    /// Record `index` (negative indices count from the end), as a `Record` with its own copy of the payload.
    /// Slicing is not supported (use `records()[a:b]`, or slice the arrays from `to_numpy()`). As for a list, other
    /// non-integer indices raise `TypeError` and only an integer out of range raises `IndexError`.
    fn __getitem__(&self, py: Python<'_>, index: &Bound<'_, PyAny>) -> PyResult<Record> {
        if index.is_instance_of::<PySlice>() {
            return Err(PyTypeError::new_err(
                "RecordBatch does not support slicing; use batch.records()[start:stop], or slice the arrays \
                 from batch.to_numpy()",
            ));
        }
        // Like a list: an object without `__index__` is a TypeError, and `__index__`'s own exceptions propagate.
        let index = if index.is_instance_of::<PyInt>() {
            index.clone()
        } else {
            py.import("operator")?.getattr("index")?.call1((index,))?
        };
        // Only an int too large for isize fails here; it is out of range.
        let index: isize = index
            .extract()
            .map_err(|_| PyIndexError::new_err("RecordBatch index out of range"))?;
        let i = if index < 0 {
            isize::try_from(self.len)
                .ok()
                .and_then(|len| index.checked_add(len))
        } else {
            Some(index)
        };
        match i.and_then(|i| usize::try_from(i).ok()) {
            Some(i) if i < self.len => Ok(self.record(py, i)),
            _ => Err(PyIndexError::new_err("RecordBatch index out of range")),
        }
    }

    /// Iterates over the records (each a `Record` with its own copy of the payload).
    fn __iter__(slf: Bound<'_, Self>) -> RecordBatchIterator {
        RecordBatchIterator {
            batch: slf.unbind(),
            next: AtomicU64::new(0),
        }
    }

    /// The records as a list of `Record` objects (copies each payload into its own `bytes`).
    fn records(&self, py: Python<'_>) -> Vec<Record> {
        (0..self.len).map(|i| self.record(py, i)).collect()
    }

    /// A dict of NumPy arrays (needs `numpy`): `seq`, `schema_id`, `venue_ts_ns`, `local_ts_ns` share this batch's
    /// buffers; `payload` is an object array of `bytes`.
    fn to_numpy<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        RecordBatch::convert(slf, "to_numpy")
    }

    /// A `pyarrow.Table` (needs `pyarrow`), with `payload` as `large_binary`. Numeric columns share this batch's
    /// buffers.
    fn to_arrow<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        RecordBatch::convert(slf, "to_arrow")
    }

    /// A `pandas.DataFrame` (needs `pandas` and `numpy`), with `payload` as a column of `bytes`.
    fn to_pandas<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        RecordBatch::convert(slf, "to_pandas")
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        let seqs = self.seq.bind(py).as_bytes();
        let range = if self.len == 0 {
            "empty".to_string()
        } else {
            format!(
                "seq {}..={}",
                Columns::u64_at(seqs, 0),
                Columns::u64_at(seqs, self.len - 1)
            )
        };
        format!(
            "<tickertape.RecordBatch {} records, {range}, {} payload bytes>",
            self.len,
            self.payload_data.bind(py).as_bytes().len()
        )
    }
}

/// The iterator returned by `iter(batch)`.
#[pyclass(frozen, module = "tickertape")]
#[derive(Debug)]
pub struct RecordBatchIterator {
    batch: Py<RecordBatch>,
    next: AtomicU64,
}

#[pymethods]
impl RecordBatchIterator {
    fn __iter__(slf: Bound<'_, Self>) -> Bound<'_, Self> {
        slf
    }

    fn __next__(&self, py: Python<'_>) -> PyResult<Record> {
        let batch = self.batch.get();
        loop {
            let i = self.next.load(Ordering::Acquire);
            let index = usize::try_from(i).ok().filter(|&i| i < batch.len);
            let Some(index) = index else {
                return Err(PyStopIteration::new_err(()));
            };
            if self
                .next
                .compare_exchange(i, i + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(batch.record(py, index));
            }
        }
    }
}

/// Collects up to `max` committed records without waiting. Returns the error that stopped it, if any.
fn drain(
    reader: &mut tickertape::Reader,
    columns: &mut Columns,
    max: usize,
) -> Option<tickertape::Error> {
    while columns.len < max {
        match reader.try_next() {
            Ok(Some(r)) => columns.push(&r),
            Ok(None) => return None,
            Err(e) => return Some(e),
        }
    }
    None
}

#[derive(Debug)]
struct State {
    reader: tickertape::Reader,
    /// Records read from the journal and not yet returned (see `Buffer`).
    buffer: Buffer,
    /// An error met after records were collected. It is raised by the read after those records have been returned,
    /// so neither the records nor the error are lost.
    pending: Option<tickertape::Error>,
}

impl State {
    /// The sequence number of the next record a read will return.
    fn position(&self) -> u64 {
        self.buffer
            .first_seq()
            .unwrap_or_else(|| self.reader.position())
    }

    /// Makes something deliverable: if the buffer holds records or an error is held back, does nothing. Otherwise
    /// moves up to `max` committed records from the core reader into the buffer, waiting up to `slice` for the first
    /// if none is committed. An error goes to `pending`, behind the records read before it. Runs no Python code.
    /// Returns whether there is something to deliver.
    fn fill(&mut self, max: usize, slice: Duration) -> bool {
        if !self.buffer.is_empty() || self.pending.is_some() {
            return true;
        }
        self.buffer.reset();
        let columns = &mut self.buffer.columns;
        columns.reserve(max.min(4096));
        if let Some(e) = drain(&mut self.reader, columns, max) {
            self.pending = Some(e);
            return true;
        }
        if columns.len > 0 {
            return true;
        }
        if slice.is_zero() {
            return false;
        }
        match self.reader.next_timeout(Some(slice)) {
            Ok(Some(r)) => {
                columns.push(&r);
                self.pending = drain(&mut self.reader, columns, max);
                true
            }
            Ok(None) => false,
            Err(e) => {
                self.pending = Some(e);
                true
            }
        }
    }

    /// Builds a result from up to `max` staged records with `build` (which must not run Python code), leaving them
    /// staged; with none staged, takes the held-back error.
    fn stage<T>(&mut self, max: usize, build: impl FnOnce(&Buffer, usize) -> T) -> Staged<T> {
        if !self.buffer.is_empty() {
            let count = max.min(self.buffer.len());
            Staged::Records {
                value: build(&self.buffer, count),
                epoch: self.buffer.epoch,
                count,
            }
        } else if let Some(e) = self.pending.take() {
            Staged::Error(e)
        } else {
            Staged::Nothing
        }
    }

    /// Delivers the records a `Staged::Records` result was built from: removes them if the buffer has not changed
    /// since. Returns false if it has (another caller took or replaced them), and changes nothing then.
    fn commit(&mut self, epoch: u64, count: usize) -> bool {
        if self.buffer.epoch != epoch {
            return false;
        }
        self.buffer.consume(count);
        true
    }
}

/// An independent cursor over a journal: replay from any sequence number, or follow the writer live.
///
/// Uses the core crate's safe `pread` reader (ADR 0006): memory-safe whatever other processes do to the files.
/// Never takes locks on the journal, never writes, never slows the writer down. Every record is validated before it
/// is returned.
///
/// Locking: the core reader sits in a mutex that is held only for bounded steps (one wait slice of at most 50 ms, a
/// batch drain, a seek) and never while Python code runs: signal handlers, exception constructors and conversions
/// to Python objects all run with it released. `closed` and the last known `position` are atomics, readable without
/// the lock. `close()` sets the flag first, so a read waiting in another thread (or interrupted by a signal handler
/// that closes the reader) ends within one slice with `ValueError`.
///
/// In a child created by `fork`, every method raises `UsedAfterForkError`; closing or dropping the copy there closes
/// that process's segment descriptors and unmaps its wake-up region (the core reader has no threads and writes
/// nothing, so dropping it is safe there).
#[pyclass(frozen, module = "tickertape")]
#[derive(Debug)]
pub struct Reader {
    inner: Mutex<Option<State>>,
    closed: AtomicBool,
    /// `State::position` after the last call that changed it.
    position: AtomicU64,
    /// Callers waiting for `inner` (see `HAND_OFF`).
    contenders: AtomicUsize,
    owner: Owner,
    path: PathBuf,
    durable_only: bool,
}

impl Reader {
    /// Raises `UsedAfterForkError` in a forked child, `ValueError` after `close()`.
    fn check(&self, py: Python<'_>) -> PyResult<()> {
        self.owner.check(py, "Reader")?;
        if self.closed.load(Ordering::Acquire) {
            return Err(errors::closed("reader"));
        }
        Ok(())
    }

    /// Runs `f` on the open state with the lock held, then records the position. With `detach`, the GIL is released
    /// meanwhile. `None` if the reader is closed. `f` must not run Python code.
    fn locked<R: Send>(
        &self,
        py: Python<'_>,
        detach: bool,
        f: impl FnOnce(&mut State) -> R + Send,
    ) -> Option<R> {
        if !detach {
            return self.locked_attached(py, f);
        }
        self.contenders.fetch_add(1, Ordering::AcqRel);
        py.detach(|| {
            let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            self.contenders.fetch_sub(1, Ordering::AcqRel);
            guard.as_mut().map(|state| {
                let result = f(state);
                self.position.store(state.position(), Ordering::Release);
                result
            })
        })
    }

    /// Like `locked` with the GIL held throughout. `f` must not run Python code (it may create objects that the
    /// garbage collector does not track).
    fn locked_attached<R>(&self, py: Python<'_>, f: impl FnOnce(&mut State) -> R) -> Option<R> {
        self.contenders.fetch_add(1, Ordering::AcqRel);
        let mut guard = lock(py, &self.inner);
        self.contenders.fetch_sub(1, Ordering::AcqRel);
        guard.as_mut().map(|state| {
            let result = f(state);
            self.position.store(state.position(), Ordering::Release);
            result
        })
    }

    /// Between two wait slices: lets callers queued for the lock take it first, for at most `HAND_OFF`, with the GIL
    /// released (a queued caller may need the GIL to finish taking the lock).
    fn hand_off(&self, py: Python<'_>) {
        if self.contenders.load(Ordering::Acquire) == 0 {
            return;
        }
        py.detach(|| {
            let started = Instant::now();
            while self.contenders.load(Ordering::Acquire) > 0 && started.elapsed() < HAND_OFF {
                std::thread::yield_now();
            }
        });
    }

    /// Reads up to `max` records: waits up to `timeout` for the first (`Timeout::ZERO`: not at all), then takes what
    /// is already committed.
    ///
    /// Each step holds the lock only for one bounded slice. Records are staged in the buffer (see `Buffer`); `stage`
    /// builds the result's parts from them under the lock (it must not run Python code), and `finish` turns those
    /// into the Python result with the lock released. Signals are checked before the result is built and again after
    /// it is complete; the records leave the buffer only after that last check, and only if the buffer still holds
    /// them. If a handler raises (`KeyboardInterrupt`) or building fails, the records stay staged and the next read
    /// returns them. If another thread (or a read in a signal handler) took them meanwhile, this call looks at the
    /// journal again: that can happen only when someone else advanced the cursor, so the loop makes progress.
    fn read<T, U>(
        &self,
        py: Python<'_>,
        max: usize,
        timeout: Timeout,
        stage: impl Fn(&Buffer, usize) -> T,
        finish: impl Fn(T) -> PyResult<U>,
    ) -> PyResult<Option<U>> {
        self.check(py)?;
        if max == 0 {
            return Ok(None);
        }
        let no_wait = timeout == Timeout::ZERO;
        let deadline = timeout.deadline(Instant::now());
        let mut waited = false;
        let closed = |waited: bool| {
            if waited {
                errors::closed_while_waiting()
            } else {
                errors::closed("reader")
            }
        };
        loop {
            if self.closed.load(Ordering::Acquire) {
                return Err(closed(waited));
            }
            if waited {
                self.hand_off(py);
            }
            // Taking one committed record is quicker than releasing the GIL.
            let detach = !no_wait || max > 1;
            let (ready, slice) = self
                .locked(py, detach, |s| {
                    // The remaining wait, measured once the lock is held.
                    let slice = if no_wait {
                        Duration::ZERO
                    } else {
                        deadline.map_or(WAIT_SLICE, |d| {
                            WAIT_SLICE.min(d.saturating_duration_since(Instant::now()))
                        })
                    };
                    (s.fill(max, slice), slice)
                })
                .ok_or_else(|| closed(waited))?;
            waited |= !slice.is_zero();
            if ready {
                // Run signal handlers before building the result (see above).
                py.check_signals()?;
                if self.closed.load(Ordering::Acquire) {
                    return Err(closed(waited));
                }
                let staged = self
                    .locked_attached(py, |s| s.stage(max, &stage))
                    .ok_or_else(|| closed(waited))?;
                let (value, epoch, count) = match staged {
                    Staged::Records {
                        value,
                        epoch,
                        count,
                    } => (value, epoch, count),
                    Staged::Error(e) => return Err(errors::to_py(py, &e)),
                    // Taken by another thread or a signal handler: look at the journal again.
                    Staged::Nothing => continue,
                };
                let result = finish(value)?;
                // The last point in this call where Python code (a signal handler) can run. After it, nothing does
                // until the result has been returned.
                py.check_signals()?;
                if self
                    .locked_attached(py, |s| s.commit(epoch, count))
                    .ok_or_else(|| closed(waited))?
                {
                    return Ok(Some(result));
                }
                // Another caller took or replaced the records while the result was built: start again.
                continue;
            } else if !slice.is_zero() {
                py.check_signals()?;
            }
            if self.closed.load(Ordering::Acquire) {
                return Err(closed(waited));
            }
            if no_wait || deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(None);
            }
        }
    }

    fn read_one(&self, py: Python<'_>, timeout: Timeout) -> PyResult<Option<Py<Record>>> {
        self.read(
            py,
            1,
            timeout,
            |buffer, _| buffer.record(py, 0),
            |record| Py::new(py, record),
        )
    }
}

#[pymethods]
impl Reader {
    /// Opens the journal in `path`, positioned at the first record of the oldest segment.
    #[new]
    #[pyo3(signature = (path, *, durable_only=false, notify=true, backoff=None))]
    fn new(
        py: Python<'_>,
        path: PathBuf,
        durable_only: bool,
        notify: bool,
        backoff: Option<PyRef<'_, Backoff>>,
    ) -> PyResult<Self> {
        let mut config = ReaderConfig::default()
            .durable_only(durable_only)
            .notify(notify);
        if let Some(b) = backoff {
            config = config.backoff(b.inner);
        }
        let open_path = path.clone();
        let reader = py
            .detach(move || tickertape::Reader::open(open_path, config))
            .map_err(|e| errors::to_py(py, &e))?;
        let position = reader.position();
        Ok(Reader {
            inner: Mutex::new(Some(State {
                reader,
                buffer: Buffer::new(),
                pending: None,
            })),
            closed: AtomicBool::new(false),
            position: AtomicU64::new(position),
            contenders: AtomicUsize::new(0),
            owner: Owner::current(),
            path,
            durable_only,
        })
    }

    /// Positions the reader so that the next record returned is `seq` (which may not be written yet). Also the way
    /// to continue after `DiscardedError`. If it succeeds, it also drops records held for an interrupted read and
    /// an error held back by a batch call; if it fails, the reader is unchanged.
    fn seek(&self, py: Python<'_>, seq: u64) -> PyResult<()> {
        self.check(py)?;
        let result = self
            .locked(py, true, |s| {
                let result = s.reader.seek(seq);
                if result.is_ok() {
                    s.buffer.reset();
                    s.pending = None;
                }
                result
            })
            .ok_or_else(|| errors::closed("reader"))?;
        result.map_err(|e| errors::to_py(py, &e))
    }

    /// Returns the next committed record, or `None` if there is none yet. Never waits.
    fn try_next(&self, py: Python<'_>) -> PyResult<Option<Py<Record>>> {
        self.read_one(py, Timeout::ZERO)
    }

    /// Returns the next record, waiting up to `timeout` seconds for one (`None`, the default: wait forever). Returns
    /// `None` if the timeout passes. Waits with the GIL released. Ctrl-C raises `KeyboardInterrupt`; a record that
    /// arrived meanwhile stays staged for the next read.
    #[pyo3(signature = (timeout=None), text_signature = "($self, timeout=None)")]
    fn next(&self, py: Python<'_>, timeout: Option<Timeout>) -> PyResult<Option<Py<Record>>> {
        self.read_one(py, timeout.unwrap_or(Timeout::FOREVER))
    }

    /// Up to `max_records` records as a list. Waits up to `timeout` seconds for the first one (default 0: don't
    /// wait; `None`: wait forever), then returns what is already committed without waiting for more. Reads
    /// with the GIL released. If an error occurs after some records were read, those are returned and the error
    /// is raised by the next read.
    #[pyo3(
        signature = (max_records, *, timeout=Timeout::ZERO),
        text_signature = "($self, max_records, *, timeout=0)"
    )]
    fn read_many(
        &self,
        py: Python<'_>,
        max_records: usize,
        timeout: Timeout,
    ) -> PyResult<Py<PyList>> {
        let list = self.read(
            py,
            max_records,
            timeout,
            |buffer, n| buffer.records(py, n),
            |records| Ok(PyList::new(py, records)?.unbind()),
        )?;
        list.map_or_else(|| Ok(PyList::empty(py).unbind()), Ok)
    }

    /// Like `read_many`, but returns the records as a columnar `RecordBatch`.
    #[pyo3(
        signature = (max_records, *, timeout=Timeout::ZERO),
        text_signature = "($self, max_records, *, timeout=0)"
    )]
    fn read_batch(
        &self,
        py: Python<'_>,
        max_records: usize,
        timeout: Timeout,
    ) -> PyResult<Py<RecordBatch>> {
        let batch = self.read(
            py,
            max_records,
            timeout,
            |buffer, n| buffer.batch(py, n),
            |batch| Py::new(py, batch),
        )?;
        batch.map_or_else(
            || Py::new(py, Columns::with_capacity(0).batch(py, 0, 0)),
            Ok,
        )
    }

    /// The sequence number of the next record this reader will return. Never blocks.
    #[getter]
    fn position(&self, py: Python<'_>) -> PyResult<u64> {
        self.check(py)?;
        Ok(self.position.load(Ordering::Acquire))
    }

    /// Whether this reader returns only checkpointed (durable) records.
    #[getter]
    fn durable_only(&self) -> bool {
        self.durable_only
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

    /// Closes the segment files. Calling it again does nothing. A read waiting in another thread raises
    /// `ValueError` within about 50 ms. In a child created by `fork`, it closes that process's copies of the
    /// descriptors and mappings, and nothing else.
    fn close(&self, py: Python<'_>) {
        self.closed.store(true, Ordering::Release);
        if self.owner.is_foreign() {
            drop(take_inherited(&self.inner));
            return;
        }
        self.contenders.fetch_add(1, Ordering::AcqRel);
        let state = lock(py, &self.inner).take();
        self.contenders.fetch_sub(1, Ordering::AcqRel);
        drop(state);
    }

    fn __enter__(slf: Bound<'_, Self>) -> PyResult<Bound<'_, Self>> {
        slf.get().check(slf.py())?;
        Ok(slf)
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&self, py: Python<'_>, _args: &Bound<'_, PyTuple>) {
        self.close(py);
    }

    /// Test only: pretends this handle was opened by process `pid` (to simulate process-id reuse after `fork`).
    fn _set_owner_pid_for_testing(&self, pid: u32) {
        self.owner.set_pid_for_testing(pid);
    }

    /// A new iterator over the records committed so far: it stops at the first position without one (it doesn't
    /// wait) and then stays exhausted. It reads from this reader's cursor, so iterating again later (with a new
    /// iterator) continues from there. Use `next(timeout=...)` or `read_many` to follow live.
    fn __iter__(slf: Bound<'_, Self>) -> PyResult<ReaderIterator> {
        slf.get().check(slf.py())?;
        Ok(ReaderIterator {
            reader: slf.unbind(),
            exhausted: AtomicBool::new(false),
        })
    }

    /// Never blocks.
    fn __repr__(&self) -> String {
        let state = if self.owner.is_foreign() {
            format!("opened in process {}", self.owner.pid())
        } else if self.closed() {
            "closed".to_string()
        } else {
            format!("position={}", self.position.load(Ordering::Acquire))
        };
        let durable = if self.durable_only {
            " durable_only"
        } else {
            ""
        };
        format!(
            "<tickertape.Reader {:?} {state}{durable}>",
            self.path.display().to_string()
        )
    }
}

/// The iterator returned by `iter(reader)`: drains the committed records from the reader's cursor, then stays
/// exhausted.
#[pyclass(frozen, module = "tickertape")]
#[derive(Debug)]
pub struct ReaderIterator {
    reader: Py<Reader>,
    exhausted: AtomicBool,
}

#[pymethods]
impl ReaderIterator {
    fn __iter__(slf: Bound<'_, Self>) -> Bound<'_, Self> {
        slf
    }

    fn __next__(&self, py: Python<'_>) -> PyResult<Py<Record>> {
        if self.exhausted.load(Ordering::Acquire) {
            return Err(PyStopIteration::new_err(()));
        }
        match self.reader.get().read_one(py, Timeout::ZERO)? {
            Some(record) => Ok(record),
            None => {
                self.exhausted.store(true, Ordering::Release);
                Err(PyStopIteration::new_err(()))
            }
        }
    }
}
