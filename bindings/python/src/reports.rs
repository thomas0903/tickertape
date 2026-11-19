//! Writer reports (`RecoveryReport`, `RecoveryAction`, `RetentionReport`) and segment ends, described in plain Rust
//! (so they can be copied out under a lock) and built as the named tuples of `tickertape._inspect`.
//!
//! `RecoveryAction` and `SegmentEnd` are `#[non_exhaustive]`; `RECOVERY_ACTIONS` and `SEGMENT_ENDS` list every mapped
//! variant. `test_errors.py::test_every_rust_enum_variant_is_listed` checks the lists against the core sources, and
//! the unit tests below check the conversion of every listed variant.

use std::path::PathBuf;

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::PyList;
use tickertape::RecoveryAction;
use tickertape::inspect::SegmentEnd;

use crate::errors;

/// Every `tickertape::RecoveryAction` variant and its `RecoveryAction.kind` code.
pub const RECOVERY_ACTIONS: &[(&str, &str)] = &[
    ("Created", "created"),
    ("ZeroedUncommitted", "zeroed_uncommitted"),
    ("RepairedTail", "repaired_tail"),
    ("QuarantinedSegment", "quarantined_segment"),
    ("Sealed", "sealed"),
    ("CompletedRoll", "completed_roll"),
    ("RemovedTemp", "removed_temp"),
];

/// Every `tickertape::inspect::SegmentEnd` variant and its `SegmentReport.end` code.
pub const SEGMENT_ENDS: &[(&str, &str)] = &[
    ("Sealed", "sealed"),
    ("Open", "open"),
    ("Damaged", "damaged"),
];

/// The `SegmentReport.end` code of a segment end.
pub fn segment_end(end: SegmentEnd) -> &'static str {
    match end {
        SegmentEnd::Sealed => "sealed",
        SegmentEnd::Open => "open",
        SegmentEnd::Damaged => "damaged",
        // Added to the core crate after this mapping was written (the tests named in the module docs fail).
        _ => "unknown",
    }
}

/// The fields of a `RecoveryAction` named tuple.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Action {
    pub kind: &'static str,
    pub segment: Option<PathBuf>,
    pub offset: Option<u64>,
    pub length: Option<u64>,
    pub quarantine_file: Option<PathBuf>,
    pub reason: Option<&'static str>,
    pub file: Option<PathBuf>,
}

pub fn describe_action(action: &RecoveryAction) -> Action {
    match action {
        RecoveryAction::Created => Action {
            kind: "created",
            ..Action::default()
        },
        RecoveryAction::ZeroedUncommitted {
            segment,
            offset,
            len,
        } => Action {
            kind: "zeroed_uncommitted",
            segment: Some(segment.clone()),
            offset: Some(*offset),
            length: Some(*len),
            ..Action::default()
        },
        RecoveryAction::RepairedTail {
            segment,
            offset,
            len,
            quarantine_file,
            reason,
        } => Action {
            kind: "repaired_tail",
            segment: Some(segment.clone()),
            offset: Some(*offset),
            length: Some(*len),
            quarantine_file: Some(quarantine_file.clone()),
            reason: Some(errors::reason_fields(reason).0),
            ..Action::default()
        },
        RecoveryAction::QuarantinedSegment {
            segment,
            quarantine_file,
            reason,
        } => Action {
            kind: "quarantined_segment",
            segment: Some(segment.clone()),
            quarantine_file: Some(quarantine_file.clone()),
            reason: Some(errors::reason_fields(reason).0),
            ..Action::default()
        },
        RecoveryAction::Sealed { segment } => Action {
            kind: "sealed",
            segment: Some(segment.clone()),
            ..Action::default()
        },
        RecoveryAction::CompletedRoll { segment } => Action {
            kind: "completed_roll",
            segment: Some(segment.clone()),
            ..Action::default()
        },
        RecoveryAction::RemovedTemp { file } => Action {
            kind: "removed_temp",
            file: Some(file.clone()),
            ..Action::default()
        },
        _ => Action {
            kind: "unknown",
            ..Action::default()
        },
    }
}

static INSPECT_MODULE: PyOnceLock<Py<PyModule>> = PyOnceLock::new();

/// Looks up a named tuple class defined in `tickertape._inspect`.
pub fn class<'py>(py: Python<'py>, name: &str) -> PyResult<Bound<'py, PyAny>> {
    let module = INSPECT_MODULE
        .get_or_try_init(py, || py.import("tickertape._inspect").map(Bound::unbind))?;
    module.bind(py).getattr(name)
}

/// `RecoveryReport(next_seq, scanned_from_seq, had_checkpoint, actions)`.
pub fn recovery_report<'py>(
    py: Python<'py>,
    report: &tickertape::RecoveryReport,
) -> PyResult<Bound<'py, PyAny>> {
    let action_class = class(py, "RecoveryAction")?;
    let actions = PyList::empty(py);
    for action in &report.actions {
        let a = describe_action(action);
        actions.append(action_class.call1((
            a.kind,
            a.segment,
            a.offset,
            a.length,
            a.quarantine_file,
            a.reason,
            a.file,
        ))?)?;
    }
    class(py, "RecoveryReport")?.call1((
        report.next_seq,
        report.scanned_from_seq,
        report.had_checkpoint,
        actions,
    ))
}

/// `RetentionReport(removed_segments, removed_bytes, already_removed_segments)`.
pub fn retention_report<'py>(
    py: Python<'py>,
    report: &tickertape::retention::Report,
) -> PyResult<Bound<'py, PyAny>> {
    class(py, "RetentionReport")?.call1((
        report.removed_segments,
        report.removed_bytes,
        report.already_removed_segments,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickertape::CorruptReason;

    fn variant<T: std::fmt::Debug>(value: &T) -> String {
        format!("{value:?}")
            .split(|c: char| !c.is_alphanumeric())
            .next()
            .unwrap()
            .to_string()
    }

    #[test]
    fn every_listed_recovery_action_converts_to_its_kind() {
        let p = PathBuf::from("/j/0.tape");
        let q = PathBuf::from("/j/quarantine/x.bin");
        let reason = CorruptReason::BadChecksum;
        let actions = vec![
            RecoveryAction::Created,
            RecoveryAction::ZeroedUncommitted {
                segment: p.clone(),
                offset: 64,
                len: 8,
            },
            RecoveryAction::RepairedTail {
                segment: p.clone(),
                offset: 64,
                len: 8,
                quarantine_file: q.clone(),
                reason,
            },
            RecoveryAction::QuarantinedSegment {
                segment: p.clone(),
                quarantine_file: q.clone(),
                reason,
            },
            RecoveryAction::Sealed { segment: p.clone() },
            RecoveryAction::CompletedRoll { segment: p.clone() },
            RecoveryAction::RemovedTemp { file: p.clone() },
        ];
        let mut seen = Vec::new();
        for action in &actions {
            let name = variant(action);
            let described = describe_action(action);
            let listed = RECOVERY_ACTIONS.iter().find(|(n, _)| *n == name);
            assert_eq!(
                listed.map(|(_, kind)| *kind),
                Some(described.kind),
                "{name}"
            );
            seen.push(name);
        }
        let repaired = describe_action(&actions[2]);
        assert_eq!(
            (repaired.length, repaired.reason, repaired.quarantine_file),
            (Some(8), Some("bad_checksum"), Some(q))
        );
        seen.sort();
        let mut listed: Vec<_> = RECOVERY_ACTIONS
            .iter()
            .map(|(n, _)| n.to_string())
            .collect();
        listed.sort();
        assert_eq!(seen, listed);
    }

    #[test]
    fn every_listed_segment_end_converts_to_its_code() {
        let ends = [SegmentEnd::Sealed, SegmentEnd::Open, SegmentEnd::Damaged];
        assert_eq!(ends.len(), SEGMENT_ENDS.len());
        for end in ends {
            let name = variant(&end);
            let listed = SEGMENT_ENDS.iter().find(|(n, _)| *n == name);
            assert_eq!(listed.map(|(_, code)| *code), Some(segment_end(end)));
        }
    }
}
