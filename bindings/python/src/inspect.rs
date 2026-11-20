//! Read-only inspection: `verify`, `list_segments`, `read_checkpoint`. The Python layer (`tickertape/_inspect.py`)
//! turns the tuples returned here into named tuples.

use std::path::PathBuf;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyTuple};
use tickertape::inspect::{self, SegmentInfo};

use crate::{errors, reports};

fn segment_info<'py>(py: Python<'py>, s: &SegmentInfo) -> PyResult<Bound<'py, PyTuple>> {
    (
        &s.path,
        s.base_seq,
        s.segment_size,
        s.format_version,
        s.created_unix_ns,
    )
        .into_pyobject(py)
}

/// `[(path, base_seq, segment_size, format_version, created_unix_ns), ...]`, oldest first.
#[pyfunction]
pub fn list_segments(py: Python<'_>, path: PathBuf) -> PyResult<Py<PyList>> {
    let segments = py
        .detach(|| inspect::list_segments(&path))
        .map_err(|e| errors::to_py(py, &e))?;
    let list = PyList::empty(py);
    for s in &segments {
        list.append(segment_info(py, s)?)?;
    }
    Ok(list.unbind())
}

/// `(generation, next_seq, segment_base_seq, offset)` or `None`.
#[pyfunction]
pub fn read_checkpoint(py: Python<'_>, path: PathBuf) -> PyResult<Option<(u64, u64, u64, u64)>> {
    let slot = py
        .detach(|| inspect::read_checkpoint(&path))
        .map_err(|e| errors::to_py(py, &e))?;
    Ok(slot.map(|c| (c.generation, c.next_seq, c.segment_base_seq, c.offset)))
}

/// A dict with `records`, `first_seq`, `last_seq`, `segments` (tuples) and `problems` (tuples).
#[pyfunction]
pub fn verify(py: Python<'_>, path: PathBuf) -> PyResult<Py<PyDict>> {
    let report = py
        .detach(|| inspect::verify(&path))
        .map_err(|e| errors::to_py(py, &e))?;
    let dict = PyDict::new(py);
    dict.set_item("records", report.records)?;
    dict.set_item("first_seq", report.first_seq)?;
    dict.set_item("last_seq", report.last_seq)?;
    let segments = PyList::empty(py);
    for s in &report.segments {
        let info = s.info.as_ref().map(|i| segment_info(py, i)).transpose()?;
        let end = reports::segment_end(s.end);
        segments.append((&s.path, info, s.records, s.end_seq, s.used_bytes, end))?;
    }
    dict.set_item("segments", segments)?;
    let problems = PyList::empty(py);
    for p in &report.problems {
        let (code, details) = errors::reason_fields(&p.reason);
        let details = errors::fields_to_dict(py, &details)?;
        problems.append((&p.path, p.offset, code, details, p.reason.to_string()))?;
    }
    dict.set_item("problems", problems)?;
    Ok(dict.unbind())
}
