//! Operational metrics (ADR 0032): the journal status sample that `tickertape status` prints, the writer's metrics
//! and a reader's lag. The values cross into Python as the named tuples of `tickertape._status`, built here from
//! keyword arguments; unknown values are `None`, durations integer nanoseconds.
//!
//! The core's status enums are `#[non_exhaustive]`; `STATUS_ENUMS` lists every mapped variant with its code.
//! `test_errors.py::test_every_rust_status_enum_variant_is_listed` checks the lists against the core sources, and the
//! unit tests below check the conversion of every listed variant.

use std::path::PathBuf;
use std::time::Duration;

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyDict, PyList};
use tickertape::status::{
    AgeUnknown, BeaconDirState, CheckpointProblem, HeadEnd, LagUnknown, ReaderStatus, Sampler,
    Status,
};

use crate::errors;

/// Every variant of the core's status enums and the code it has in Python (the core's `code()` where it has one, and
/// the CLI's JSON codes for `HeadEnd` and `BeaconDirState`).
pub const STATUS_ENUMS: &[(&str, &[(&str, &str)])] = &[
    (
        "HeadEnd",
        &[
            ("Open", "open"),
            ("Sealed", "sealed"),
            ("Invalid", "invalid"),
        ],
    ),
    (
        "CheckpointProblem",
        &[
            ("MissingSegment", "missing_segment"),
            ("InvalidSegmentHeader", "invalid_segment_header"),
            ("InvalidOffset", "invalid_offset"),
        ],
    ),
    (
        "AgeUnknown",
        &[
            ("NoModificationTime", "no_modification_time"),
            ("InFuture", "modification_time_in_future"),
            ("BeforeSegment", "modification_time_before_segment"),
            ("MarkerResized", "marker_resized"),
            ("Changing", "marker_changing"),
            ("OlderSlotLost", "older_slot_lost"),
        ],
    ),
    (
        "LagUnknown",
        &[
            ("SegmentGone", "segment_gone"),
            ("OtherHistory", "other_history"),
            ("SegmentUnreadable", "segment_unreadable"),
            ("CheckpointUnusable", "checkpoint_unusable"),
            ("AheadOfSample", "ahead_of_sample"),
        ],
    ),
    (
        "BeaconState",
        &[
            ("Live", "live"),
            ("Stale", "stale"),
            ("Uncertain", "uncertain"),
            ("Invalid", "invalid"),
            ("Unreadable", "unreadable"),
            ("Changing", "changing"),
        ],
    ),
    (
        "BeaconDirState",
        &[
            ("Listed", "listed"),
            ("Missing", "missing"),
            ("Unusable", "unusable"),
        ],
    ),
];

static STATUS_MODULE: PyOnceLock<Py<PyModule>> = PyOnceLock::new();

/// Builds the named tuple `name` of `tickertape._status` from keyword arguments.
fn build<'py>(
    py: Python<'py>,
    name: &str,
    fields: &Bound<'py, PyDict>,
) -> PyResult<Bound<'py, PyAny>> {
    let module =
        STATUS_MODULE.get_or_try_init(py, || py.import("tickertape._status").map(Bound::unbind))?;
    module.bind(py).getattr(name)?.call((), Some(fields))
}

fn ns(d: Duration) -> u128 {
    d.as_nanos()
}

pub fn head_end(end: HeadEnd) -> &'static str {
    match end {
        HeadEnd::Open => "open",
        HeadEnd::Sealed => "sealed",
        HeadEnd::Invalid(_) => "invalid",
        // Added to the core after this mapping (the tests named in the module docs fail).
        _ => "unknown",
    }
}

pub fn readers_dir_state(state: BeaconDirState) -> &'static str {
    match state {
        BeaconDirState::Listed => "listed",
        BeaconDirState::Missing => "missing",
        BeaconDirState::Unusable(_) => "unusable",
        _ => "unknown",
    }
}

fn lag_unknown(reason: LagUnknown) -> &'static str {
    reason.code()
}

/// One sample of a journal (an opaque native object; `tickertape.status` returns it inside a `JournalStatus`).
#[pyclass(frozen, module = "tickertape._native")]
#[derive(Debug)]
pub struct StatusSample {
    pub status: Status,
}

/// Takes one read-only sample of the journal in `path` with the GIL released.
#[pyfunction]
#[pyo3(signature = (path, readers_dir=None))]
pub fn sample_status(
    py: Python<'_>,
    path: PathBuf,
    readers_dir: Option<PathBuf>,
) -> PyResult<StatusSample> {
    let status = py
        .detach(|| {
            let mut sampler = Sampler::new(&path);
            if let Some(dir) = &readers_dir {
                sampler = sampler.readers_dir(dir);
            }
            sampler.sample()
        })
        .map_err(|e| errors::to_py(py, &e))?;
    Ok(StatusSample { status })
}

fn lag_fields<'py>(
    py: Python<'py>,
    records: u64,
    bytes: Option<u64>,
) -> PyResult<Bound<'py, PyAny>> {
    let d = PyDict::new(py);
    d.set_item("records", records)?;
    d.set_item("bytes", bytes)?;
    build(py, "Lag", &d)
}

fn beacon<'py>(py: Python<'py>, r: &ReaderStatus) -> PyResult<Bound<'py, PyAny>> {
    let d = PyDict::new(py);
    d.set_item("name", &r.name)?;
    d.set_item("path", &r.path)?;
    d.set_item("state", r.state.code())?;
    let report = match &r.report {
        Some(rep) => {
            let f = PyDict::new(py);
            f.set_item("pid", rep.pid)?;
            f.set_item("next_seq", rep.next_seq)?;
            f.set_item("slot_seq", rep.position.next_seq)?;
            f.set_item("segment_base_seq", rep.position.segment_base_seq)?;
            f.set_item("offset", rep.position.offset)?;
            f.set_item("seek_pending", rep.seek_pending)?;
            f.set_item("durable_only", rep.durable_only)?;
            f.set_item("updated_unix_ns", rep.updated_unix_ns)?;
            Some(build(py, "BeaconReport", &f)?)
        }
        None => None,
    };
    d.set_item("report", report)?;
    d.set_item("update_age_ns", r.update_age.map(ns))?;
    let lag = r
        .lag
        .map(|l| lag_fields(py, l.records, l.bytes))
        .transpose()?;
    d.set_item("lag", lag)?;
    d.set_item("lag_unknown", r.lag_unknown.map(lag_unknown))?;
    d.set_item("oldest_unread_local_ts_ns", r.oldest_unread_local_ts_ns)?;
    d.set_item("unread_age_ns", r.unread_age.map(ns))?;
    build(py, "BeaconStatus", &d)
}

#[pymethods]
impl StatusSample {
    /// The sample's values as keyword arguments for `tickertape._status.JournalStatus` (without the sample itself).
    fn fields<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let s = &self.status;
        let d = PyDict::new(py);
        d.set_item("path", &s.dir)?;
        d.set_item("sampled_unix_ns", s.sampled_unix_ns)?;

        let h = PyDict::new(py);
        h.set_item("next_seq", s.head.position.next_seq)?;
        h.set_item("segment_base_seq", s.head.position.segment_base_seq)?;
        h.set_item("offset", s.head.position.offset)?;
        h.set_item("segment_size", s.head.segment_size)?;
        h.set_item("end", head_end(s.head.end))?;
        let reason = match s.head.end {
            HeadEnd::Invalid(reason) => Some(errors::reason_fields(&reason).0),
            _ => None,
        };
        h.set_item("invalid_reason", reason)?;
        h.set_item("last_local_ts_ns", s.head.last_local_ts_ns)?;
        h.set_item("verified", s.head.verified)?;
        h.set_item("chain_broken", s.head.chain_broken)?;
        d.set_item("head", build(py, "Head", &h)?)?;

        let g = PyDict::new(py);
        g.set_item("segment_count", s.segments.count)?;
        g.set_item("first_seq", s.segments.first_seq)?;
        g.set_item("total_bytes", s.segments.total_bytes)?;
        g.set_item("oldest_created_unix_ns", s.segments.oldest_created_unix_ns)?;
        g.set_item("before_checkpoint", s.segments.before_checkpoint)?;
        d.set_item("segments", build(py, "Segments", &g)?)?;

        let checkpoint = match &s.checkpoint {
            Some(c) => {
                let f = PyDict::new(py);
                f.set_item("generation", c.marker.generation)?;
                f.set_item("next_seq", c.marker.next_seq)?;
                f.set_item("segment_base_seq", c.marker.segment_base_seq)?;
                f.set_item("offset", c.marker.offset)?;
                f.set_item("problem", c.problem.map(CheckpointProblem::code))?;
                f.set_item("modified_unix_ns", c.modified_unix_ns)?;
                f.set_item("estimated_age_ns", c.estimated_age.map(ns))?;
                f.set_item("age_unknown", c.age_unknown.map(AgeUnknown::code))?;
                Some(build(py, "CheckpointStatus", &f)?)
            }
            None => None,
        };
        d.set_item("checkpoint", checkpoint)?;

        let e = PyDict::new(py);
        e.set_item("records", s.exposure.records)?;
        e.set_item("bytes", s.exposure.bytes)?;
        e.set_item("estimated_age_ns", s.exposure.estimated_age.map(ns))?;
        e.set_item("oldest_local_ts_ns", s.exposure.oldest_local_ts_ns)?;
        e.set_item("verified", s.exposure.verified)?;
        d.set_item("exposure", build(py, "Exposure", &e)?)?;

        let st = PyDict::new(py);
        st.set_item("available_bytes", s.storage.available_bytes)?;
        st.set_item("total_bytes", s.storage.total_bytes)?;
        st.set_item("next_segment_size", s.storage.next_segment_size)?;
        st.set_item("spare_segment", s.storage.spare_segment)?;
        st.set_item("estimated_rolls_left", s.storage.estimated_rolls_left)?;
        d.set_item("storage", build(py, "Storage", &st)?)?;

        d.set_item("writer_pid", s.writer.pid)?;
        d.set_item("writer_alive", s.writer.alive)?;
        d.set_item("readers_dir", &s.readers_dir)?;
        d.set_item("readers_dir_state", readers_dir_state(s.readers_dir_state))?;
        let readers = PyList::empty(py);
        for r in &s.readers {
            readers.append(beacon(py, r)?)?;
        }
        d.set_item("readers", readers)?;
        Ok(d)
    }
}

/// `WriterMetrics(...)` from the core's.
pub fn writer_metrics<'py>(
    py: Python<'py>,
    m: &tickertape::status::WriterMetrics,
) -> PyResult<Bound<'py, PyAny>> {
    let d = PyDict::new(py);
    d.set_item("next_seq", m.head.next_seq)?;
    d.set_item("segment_base_seq", m.head.segment_base_seq)?;
    d.set_item("offset", m.head.offset)?;
    d.set_item("segment_size", m.segment_size)?;
    let checkpoint = m
        .checkpoint
        .map(|c| {
            crate::reports::class(py, "Checkpoint")?.call1((
                c.generation,
                c.next_seq,
                c.segment_base_seq,
                c.offset,
            ))
        })
        .transpose()?;
    d.set_item("checkpoint", checkpoint)?;
    d.set_item("since_checkpoint_ns", m.since_checkpoint.map(ns))?;
    d.set_item("uncheckpointed_records", m.uncheckpointed_records)?;
    d.set_item("uncheckpointed_bytes", m.uncheckpointed_bytes)?;
    d.set_item("poisoned", m.poisoned)?;
    d.set_item("retention_failures", m.retention_failures)?;
    build(py, "WriterMetrics", &d)
}

/// `ReaderMetrics(...)`: the core reader's cursor, the records the Python reader holds (read from the journal but not
/// yet returned), and the lag behind `sample`.
pub fn reader_metrics<'py>(
    py: Python<'py>,
    position: u64,
    cursor: tickertape::status::Cursor,
    buffered: usize,
    sample: &Status,
) -> PyResult<Bound<'py, PyAny>> {
    let d = PyDict::new(py);
    d.set_item("next_seq", position)?;
    d.set_item("slot_seq", cursor.position.next_seq)?;
    d.set_item("segment_base_seq", cursor.position.segment_base_seq)?;
    d.set_item("offset", cursor.position.offset)?;
    d.set_item("seek_pending", cursor.seek_pending)?;
    d.set_item("durable_only", cursor.durable_only)?;
    d.set_item("buffered", buffered)?;
    match sample.lag(&cursor) {
        Ok(lag) => {
            // Buffered records were taken from the core reader, which has moved past them; their footprint is not
            // counted, so the byte lag is unknown while any are held.
            let records = lag.records.saturating_add(buffered as u64);
            let bytes = if buffered == 0 { lag.bytes } else { None };
            d.set_item("lag", lag_fields(py, records, bytes)?)?;
            d.set_item("lag_unknown", None::<&str>)?;
        }
        Err(reason) => {
            d.set_item("lag", None::<&str>)?;
            d.set_item("lag_unknown", lag_unknown(reason))?;
        }
    }
    build(py, "ReaderMetrics", &d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use tickertape::CorruptReason;
    use tickertape::status::BeaconState;

    fn listed(name: &str) -> &'static [(&'static str, &'static str)] {
        STATUS_ENUMS.iter().find(|(n, _)| *n == name).unwrap().1
    }

    fn check(name: &str, converted: &[(&str, &str)]) {
        assert_eq!(listed(name), converted, "{name}");
    }

    #[test]
    fn every_listed_status_enum_variant_converts_to_its_code() {
        check(
            "HeadEnd",
            &[
                ("Open", head_end(HeadEnd::Open)),
                ("Sealed", head_end(HeadEnd::Sealed)),
                (
                    "Invalid",
                    head_end(HeadEnd::Invalid(CorruptReason::BadChecksum)),
                ),
            ],
        );
        check(
            "CheckpointProblem",
            &[
                ("MissingSegment", CheckpointProblem::MissingSegment.code()),
                (
                    "InvalidSegmentHeader",
                    CheckpointProblem::InvalidSegmentHeader.code(),
                ),
                ("InvalidOffset", CheckpointProblem::InvalidOffset.code()),
            ],
        );
        check(
            "AgeUnknown",
            &[
                ("NoModificationTime", AgeUnknown::NoModificationTime.code()),
                ("InFuture", AgeUnknown::InFuture.code()),
                ("BeforeSegment", AgeUnknown::BeforeSegment.code()),
                ("MarkerResized", AgeUnknown::MarkerResized.code()),
                ("Changing", AgeUnknown::Changing.code()),
                ("OlderSlotLost", AgeUnknown::OlderSlotLost.code()),
            ],
        );
        check(
            "LagUnknown",
            &[
                ("SegmentGone", LagUnknown::SegmentGone.code()),
                ("OtherHistory", LagUnknown::OtherHistory.code()),
                ("SegmentUnreadable", LagUnknown::SegmentUnreadable.code()),
                ("CheckpointUnusable", LagUnknown::CheckpointUnusable.code()),
                ("AheadOfSample", LagUnknown::AheadOfSample.code()),
            ],
        );
        check(
            "BeaconState",
            &[
                ("Live", BeaconState::Live.code()),
                ("Stale", BeaconState::Stale.code()),
                ("Uncertain", BeaconState::Uncertain.code()),
                ("Invalid", BeaconState::Invalid.code()),
                ("Unreadable", BeaconState::Unreadable.code()),
                ("Changing", BeaconState::Changing.code()),
            ],
        );
        check(
            "BeaconDirState",
            &[
                ("Listed", readers_dir_state(BeaconDirState::Listed)),
                ("Missing", readers_dir_state(BeaconDirState::Missing)),
                (
                    "Unusable",
                    readers_dir_state(BeaconDirState::Unusable(io::ErrorKind::PermissionDenied)),
                ),
            ],
        );
    }
}
