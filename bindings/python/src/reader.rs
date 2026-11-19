//! `tickertape.Reader`, `tickertape.Record` and `tickertape.RecordBatch`.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pyo3::exceptions::{PyIndexError, PyStopIteration};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyTuple};
use tickertape::{ReaderConfig, Record as CoreRecord};

use crate::config::{Backoff, Timeout};
use crate::errors;
use crate::writer::lock;

/// Longest uninterrupted wait without the GIL. Between slices the binding checks for signals, so Ctrl-C
/// (`KeyboardInterrupt`) interrupts a blocking read within about this long.
const WAIT_SLICE: Duration = Duration::from_millis(50);

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

    fn from_core(py: Python<'_>, r: &CoreRecord<'_>) -> Self {
        Record::new(
            py,
            r.seq,
            r.schema_id,
            r.venue_ts_ns,
            r.local_ts_ns,
            r.payload,
        )
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

    fn __eq__(&self, py: Python<'_>, other: &Bound<'_, PyAny>) -> bool {
        other.cast::<Record>().is_ok_and(|o| {
            let o = o.get();
            o.seq == self.seq
                && o.schema_id == self.schema_id
                && o.venue_ts_ns == self.venue_ts_ns
                && o.local_ts_ns == self.local_ts_ns
                && o.payload.bind(py).as_bytes() == self.payload.bind(py).as_bytes()
        })
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
#[derive(Debug, Default)]
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
        let mut c = Columns {
            seq: Vec::with_capacity(n * 8),
            schema_id: Vec::with_capacity(n * 4),
            venue_ts_ns: Vec::with_capacity(n * 8),
            local_ts_ns: Vec::with_capacity(n * 8),
            offsets: Vec::with_capacity((n + 1) * 8),
            ..Columns::default()
        };
        c.offsets.extend_from_slice(&0i64.to_le_bytes());
        c
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
        // A journal's payloads total far less than i64::MAX bytes per batch.
        self.offsets
            .extend_from_slice(&(self.data.len() as i64).to_le_bytes());
    }

    fn u64_at(col: &[u8], i: usize) -> u64 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&col[i * 8..i * 8 + 8]);
        u64::from_le_bytes(b)
    }

    fn offset_at(&self, i: usize) -> usize {
        Columns::u64_at(&self.offsets, i) as usize
    }

    fn record(&self, py: Python<'_>, i: usize) -> Record {
        let mut s = [0u8; 4];
        s.copy_from_slice(&self.schema_id[i * 4..i * 4 + 4]);
        Record::new(
            py,
            Columns::u64_at(&self.seq, i),
            u32::from_le_bytes(s),
            Columns::u64_at(&self.venue_ts_ns, i),
            Columns::u64_at(&self.local_ts_ns, i),
            &self.data[self.offset_at(i)..self.offset_at(i + 1)],
        )
    }
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
    fn from_columns(py: Python<'_>, c: &Columns) -> Self {
        RecordBatch {
            len: c.len,
            seq: PyBytes::new(py, &c.seq).unbind(),
            schema_id: PyBytes::new(py, &c.schema_id).unbind(),
            venue_ts_ns: PyBytes::new(py, &c.venue_ts_ns).unbind(),
            local_ts_ns: PyBytes::new(py, &c.local_ts_ns).unbind(),
            payload_offsets: PyBytes::new(py, &c.offsets).unbind(),
            payload_data: PyBytes::new(py, &c.data).unbind(),
        }
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
    fn __getitem__(&self, py: Python<'_>, index: isize) -> PyResult<Record> {
        let i = if index < 0 {
            index + self.len as isize
        } else {
            index
        };
        if i < 0 || i as usize >= self.len {
            return Err(PyIndexError::new_err("RecordBatch index out of range"));
        }
        let i = i as usize;
        let u64_at = |b: &Py<PyBytes>| Columns::u64_at(b.bind(py).as_bytes(), i);
        let offsets = self.payload_offsets.bind(py).as_bytes();
        let (start, end) = (
            Columns::u64_at(offsets, i) as usize,
            Columns::u64_at(offsets, i + 1) as usize,
        );
        let mut s = [0u8; 4];
        s.copy_from_slice(&self.schema_id.bind(py).as_bytes()[i * 4..i * 4 + 4]);
        Ok(Record::new(
            py,
            u64_at(&self.seq),
            u32::from_le_bytes(s),
            u64_at(&self.venue_ts_ns),
            u64_at(&self.local_ts_ns),
            &self.payload_data.bind(py).as_bytes()[start..end],
        ))
    }

    /// The records as a list of `Record` objects (copies each payload into its own `bytes`).
    fn records(&self, py: Python<'_>) -> PyResult<Vec<Record>> {
        (0..self.len as isize)
            .map(|i| self.__getitem__(py, i))
            .collect()
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

#[derive(Debug)]
struct State {
    reader: tickertape::Reader,
    /// An error met after a batch call had already collected records. It is raised by the next read, so the
    /// records before it are not lost and the error is not silently dropped.
    pending: Option<tickertape::Error>,
}

impl State {
    fn take_pending(&mut self, py: Python<'_>) -> PyResult<()> {
        match self.pending.take() {
            Some(e) => Err(errors::to_py(py, &e)),
            None => Ok(()),
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

/// Waits for the next record in slices of at most `WAIT_SLICE` with the GIL released, checking for signals between
/// slices. A `KeyboardInterrupt` (or another exception from a signal handler) consumes no record.
fn wait_next(
    py: Python<'_>,
    reader: &mut tickertape::Reader,
    timeout: Option<Duration>,
    columns: &mut Columns,
) -> PyResult<bool> {
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        let slice = deadline.map_or(WAIT_SLICE, |d| {
            WAIT_SLICE.min(d.saturating_duration_since(Instant::now()))
        });
        let found = py
            .detach(|| match reader.next_timeout(Some(slice)) {
                Ok(Some(r)) => {
                    columns.push(&r);
                    Ok(true)
                }
                Ok(None) => Ok(false),
                Err(e) => Err(e),
            })
            .map_err(|e| errors::to_py(py, &e))?;
        if found {
            return Ok(true);
        }
        py.check_signals()?;
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(false);
        }
    }
}

/// An independent cursor over a journal: replay from any sequence number, or follow the writer live.
///
/// Uses the core crate's safe `pread` reader (ADR 0006): memory-safe whatever other processes do to the files.
/// Never takes locks, never writes, never slows the writer down. Every record is validated before it is returned.
#[pyclass(frozen, module = "tickertape")]
#[derive(Debug)]
pub struct Reader {
    inner: Mutex<Option<State>>,
    path: PathBuf,
    durable_only: bool,
}

impl Reader {
    fn with<R>(&self, py: Python<'_>, f: impl FnOnce(&mut State) -> PyResult<R>) -> PyResult<R> {
        let mut guard = lock(py, &self.inner);
        match guard.as_mut() {
            Some(state) => f(state),
            None => Err(errors::closed("reader")),
        }
    }

    /// Collects up to `max` records: waits up to `wait` for the first one (`None`: forever), then takes what is
    /// already committed.
    fn collect(&self, py: Python<'_>, max: usize, wait: Option<Duration>) -> PyResult<Columns> {
        self.with(py, |state| {
            state.take_pending(py)?;
            let mut columns = Columns::with_capacity(max.min(4096));
            if max == 0 {
                return Ok(columns);
            }
            let reader = &mut state.reader;
            let mut failure = py.detach(|| drain(reader, &mut columns, max));
            if columns.len == 0
                && failure.is_none()
                && wait != Some(Duration::ZERO)
                && wait_next(py, reader, wait, &mut columns)?
            {
                failure = py.detach(|| drain(reader, &mut columns, max));
            }
            match failure {
                Some(e) if columns.len == 0 => Err(errors::to_py(py, &e)),
                Some(e) => {
                    state.pending = Some(e);
                    Ok(columns)
                }
                None => Ok(columns),
            }
        })
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
        Ok(Reader {
            inner: Mutex::new(Some(State {
                reader,
                pending: None,
            })),
            path,
            durable_only,
        })
    }

    /// Positions the reader so that the next record returned is `seq` (which may not be written yet). Also the way
    /// to continue after `DiscardedError`. Clears an error held back by a batch call.
    fn seek(&self, py: Python<'_>, seq: u64) -> PyResult<()> {
        self.with(py, |state| {
            state.pending = None;
            let reader = &mut state.reader;
            py.detach(|| reader.seek(seq))
                .map_err(|e| errors::to_py(py, &e))
        })
    }

    /// Returns the next committed record, or `None` if there is none yet. Never waits.
    fn try_next(&self, py: Python<'_>) -> PyResult<Option<Record>> {
        self.with(py, |state| {
            state.take_pending(py)?;
            match state.reader.try_next() {
                Ok(Some(r)) => Ok(Some(Record::from_core(py, &r))),
                Ok(None) => Ok(None),
                Err(e) => Err(errors::to_py(py, &e)),
            }
        })
    }

    /// Returns the next record, waiting up to `timeout` seconds for one (`None`: wait forever). Returns `None` if
    /// the timeout passes. Waits with the GIL released; Ctrl-C raises `KeyboardInterrupt` and consumes nothing.
    #[pyo3(signature = (timeout=Timeout(None)))]
    fn next(&self, py: Python<'_>, timeout: Timeout) -> PyResult<Option<Record>> {
        let Timeout(timeout) = timeout;
        self.with(py, |state| {
            state.take_pending(py)?;
            match state.reader.try_next() {
                Ok(Some(r)) => return Ok(Some(Record::from_core(py, &r))),
                Ok(None) => {}
                Err(e) => return Err(errors::to_py(py, &e)),
            }
            if timeout == Some(Duration::ZERO) {
                return Ok(None);
            }
            let mut columns = Columns::with_capacity(1);
            Ok(wait_next(py, &mut state.reader, timeout, &mut columns)?
                .then(|| columns.record(py, 0)))
        })
    }

    /// Up to `max_records` records as a list. Waits up to `timeout` seconds for the first one (default 0: don't
    /// wait; `None`: wait forever), then returns what is already committed without waiting for more. Reads
    /// with the GIL released. If an error occurs after some records were read, those are returned and the error
    /// is raised by the next read.
    #[pyo3(signature = (max_records, *, timeout=Timeout(Some(Duration::ZERO))))]
    fn read_many(
        &self,
        py: Python<'_>,
        max_records: usize,
        timeout: Timeout,
    ) -> PyResult<Py<PyList>> {
        let columns = self.collect(py, max_records, timeout.0)?;
        let list = PyList::empty(py);
        for i in 0..columns.len {
            list.append(columns.record(py, i))?;
        }
        Ok(list.unbind())
    }

    /// Like `read_many`, but returns the records as a columnar `RecordBatch`.
    #[pyo3(signature = (max_records, *, timeout=Timeout(Some(Duration::ZERO))))]
    fn read_batch(
        &self,
        py: Python<'_>,
        max_records: usize,
        timeout: Timeout,
    ) -> PyResult<RecordBatch> {
        let columns = self.collect(py, max_records, timeout.0)?;
        Ok(RecordBatch::from_columns(py, &columns))
    }

    /// The sequence number of the next record this reader will return.
    #[getter]
    fn position(&self, py: Python<'_>) -> PyResult<u64> {
        self.with(py, |state| Ok(state.reader.position()))
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

    /// True after `close()`.
    #[getter]
    fn closed(&self, py: Python<'_>) -> bool {
        lock(py, &self.inner).is_none()
    }

    /// Closes the segment files. Calling it again does nothing.
    fn close(&self, py: Python<'_>) {
        let state = lock(py, &self.inner).take();
        drop(state);
    }

    fn __enter__(slf: Bound<'_, Self>) -> PyResult<Bound<'_, Self>> {
        if slf.get().closed(slf.py()) {
            return Err(errors::closed("reader"));
        }
        Ok(slf)
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&self, py: Python<'_>, _args: &Bound<'_, PyTuple>) {
        self.close(py);
    }

    /// Iterating yields the records committed so far and stops at the first position without one (it doesn't
    /// wait). Iterating again later continues from there. Use `next(timeout=...)` or `read_many` to follow live.
    fn __iter__(slf: Bound<'_, Self>) -> Bound<'_, Self> {
        slf
    }

    fn __next__(&self, py: Python<'_>) -> PyResult<Record> {
        self.try_next(py)?
            .ok_or_else(|| PyStopIteration::new_err(()))
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        let state = match lock(py, &self.inner).as_ref() {
            Some(s) => format!("position={}", s.reader.position()),
            None => "closed".to_string(),
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
