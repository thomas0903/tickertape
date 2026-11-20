//! One value of every variant of [`Error`], [`CorruptReason`] and [`RecoveryAction`], for the tests of the C ABI
//! (`crates/tickertape-ffi`) and the Python bindings, which map each variant to a code of their own and cannot build
//! the `#[non_exhaustive]` struct-like variants themselves.
//!
//! Hidden: not covered by the compatibility promise (`docs/compatibility.md` → Rust API). The values are arbitrary
//! and may change.

use std::io;
use std::path::PathBuf;

use crate::{CorruptReason, Error, RecoveryAction};

/// One error of each variant, in declaration order. The I/O error carries `EIO`; the corruption carries
/// [`CorruptReason::BadChecksum`] at offset 64.
pub fn errors() -> Vec<Error> {
    let segment = PathBuf::from("/j/00000000000000000000.tape");
    vec![
        Error::io(
            "open segment",
            &segment,
            io::Error::from_raw_os_error(libc::EIO),
        ),
        Error::Locked {
            dir: "/j".into(),
            pid: Some(42),
        },
        Error::corrupt(&segment, 64, CorruptReason::BadChecksum),
        Error::UnsupportedVersion {
            path: segment.clone(),
            found: 2,
            supported: 1,
        },
        Error::NotFound { dir: "/j".into() },
        Error::SeqUnavailable {
            requested: 1,
            first: 5,
        },
        Error::RecordTooLarge { len: 10, max: 5 },
        Error::DurabilityLost,
        Error::SequenceExhausted,
        Error::Discarded { seq: 3 },
        Error::InvalidConfig("segment_size".into()),
        Error::BeaconInUse {
            path: "/j/readers/a.beacon".into(),
        },
    ]
}

/// One reason of each variant, in declaration order.
pub fn corrupt_reasons() -> Vec<CorruptReason> {
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
        CorruptReason::EmptyCommitWord(11),
        CorruptReason::UncheckedTail(12),
    ]
}

/// One action of each variant, in declaration order. Segments are `/j/00000000000000000000.tape`, quarantine files
/// `/j/quarantine/x.bin`, lengths 8 and reasons [`CorruptReason::BadChecksum`].
pub fn recovery_actions() -> Vec<RecoveryAction> {
    let p = PathBuf::from("/j/00000000000000000000.tape");
    let q = PathBuf::from("/j/quarantine/x.bin");
    let reason = CorruptReason::BadChecksum;
    vec![
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
            quarantine_file: q,
            reason,
        },
        RecoveryAction::Sealed { segment: p.clone() },
        RecoveryAction::CompletedRoll { segment: p.clone() },
        RecoveryAction::RestoredHeader { segment: p.clone() },
        RecoveryAction::RemovedTemp { file: p },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    // Exhaustive matches without a wildcard: a new variant fails to compile here until it gets an index, and the
    // tests below then fail until it gets a sample.
    fn error_index(e: &Error) -> usize {
        match e {
            Error::Io { .. } => 0,
            Error::Locked { .. } => 1,
            Error::Corrupt { .. } => 2,
            Error::UnsupportedVersion { .. } => 3,
            Error::NotFound { .. } => 4,
            Error::SeqUnavailable { .. } => 5,
            Error::RecordTooLarge { .. } => 6,
            Error::DurabilityLost => 7,
            Error::SequenceExhausted => 8,
            Error::Discarded { .. } => 9,
            Error::InvalidConfig(_) => 10,
            Error::BeaconInUse { .. } => 11,
        }
    }

    fn reason_index(r: &CorruptReason) -> usize {
        match r {
            CorruptReason::BadMagic => 0,
            CorruptReason::BadSegmentHeader(_) => 1,
            CorruptReason::SizeMismatch { .. } => 2,
            CorruptReason::BaseSeqMismatch { .. } => 3,
            CorruptReason::BadOffset(_) => 4,
            CorruptReason::SequenceExhausted => 5,
            CorruptReason::BadLength(_) => 6,
            CorruptReason::BadChecksum => 7,
            CorruptReason::BadSequence { .. } => 8,
            CorruptReason::BadFlags => 9,
            CorruptReason::BadSeal => 10,
            CorruptReason::BrokenChain(_) => 11,
            CorruptReason::DanglingCheckpoint(_) => 12,
            CorruptReason::EmptyCommitWord(_) => 13,
            CorruptReason::UncheckedTail(_) => 14,
        }
    }

    fn action_index(a: &RecoveryAction) -> usize {
        match a {
            RecoveryAction::Created => 0,
            RecoveryAction::ZeroedUncommitted { .. } => 1,
            RecoveryAction::RepairedTail { .. } => 2,
            RecoveryAction::QuarantinedSegment { .. } => 3,
            RecoveryAction::Sealed { .. } => 4,
            RecoveryAction::CompletedRoll { .. } => 5,
            RecoveryAction::RestoredHeader { .. } => 6,
            RecoveryAction::RemovedTemp { .. } => 7,
        }
    }

    #[test]
    fn one_sample_of_each_variant_in_order() {
        let e: Vec<usize> = errors().iter().map(error_index).collect();
        assert_eq!(e, (0..12).collect::<Vec<_>>());
        let r: Vec<usize> = corrupt_reasons().iter().map(reason_index).collect();
        assert_eq!(r, (0..15).collect::<Vec<_>>());
        let a: Vec<usize> = recovery_actions().iter().map(action_index).collect();
        assert_eq!(a, (0..8).collect::<Vec<_>>());
    }
}
