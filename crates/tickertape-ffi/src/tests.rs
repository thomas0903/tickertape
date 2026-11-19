//! Rust tests that call the exported `extern "C"` functions.
//!
//! Tests whose names start with `pure_` touch no files and run under Miri (`cargo +nightly miri test -p
//! tickertape-ffi --lib pure_`); the rest use real journals in temporary directories.

#![allow(clippy::undocumented_unsafe_blocks)] // Test calls pass valid pointers by construction.

use std::collections::BTreeSet;
use std::ffi::{CStr, CString, c_void};
use std::io;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use tickertape::{CorruptReason, Error};

use super::*;
use crate::status::{FfiError, Handle, guard};

const OK: i32 = tickertape_status_code::TICKERTAPE_OK as i32;
const NONE: i32 = tickertape_status_code::TICKERTAPE_NONE as i32;
const INVALID: i32 = tickertape_status_code::TICKERTAPE_E_INVALID_ARGUMENT as i32;

fn code(c: tickertape_status_code) -> i32 {
    c as i32
}

fn cstr(path: &Path) -> CString {
    CString::new(path.as_os_str().as_encoded_bytes()).unwrap()
}

fn last_message() -> String {
    unsafe { CStr::from_ptr(tickertape_last_error_message()) }
        .to_string_lossy()
        .into_owned()
}

fn name(status: i32) -> String {
    unsafe { CStr::from_ptr(tickertape_status_name(status)) }
        .to_string_lossy()
        .into_owned()
}

/// Expected payload for sequence number `seq`, shared with the C tests' pattern.
fn payload(seq: u64) -> Vec<u8> {
    let len = (seq * 37 % 300) as usize;
    (0..len)
        .map(|i| ((seq as usize + i * 7) & 0xFF) as u8)
        .collect()
}

struct Journal {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    c_dir: CString,
}

impl Journal {
    fn new() -> Journal {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("journal");
        let c_dir = cstr(&dir);
        Journal {
            _tmp: tmp,
            dir,
            c_dir,
        }
    }

    fn writer(&self, segment_size: u64) -> *mut tickertape_writer {
        let options = tickertape_writer_options_new();
        assert_eq!(
            unsafe { tickertape_writer_options_set_segment_size(options, segment_size) },
            OK
        );
        assert_eq!(
            unsafe { tickertape_writer_options_set_prefault(options, 0) },
            OK
        );
        let mut writer = null_mut();
        let status = unsafe { tickertape_writer_open(self.c_dir.as_ptr(), options, &mut writer) };
        unsafe { tickertape_writer_options_free(options) };
        assert_eq!(status, OK, "{}", last_message());
        writer
    }

    fn reader(&self, durable_only: bool, mapped: bool) -> *mut tickertape_reader {
        let options = tickertape_reader_options_new();
        assert_eq!(
            unsafe { tickertape_reader_options_set_durable_only(options, i32::from(durable_only)) },
            OK
        );
        let mut reader = null_mut();
        let status = unsafe {
            if mapped {
                tickertape_reader_open_mapped_unsafe(self.c_dir.as_ptr(), options, &mut reader)
            } else {
                tickertape_reader_open(self.c_dir.as_ptr(), options, &mut reader)
            }
        };
        unsafe { tickertape_reader_options_free(options) };
        assert_eq!(status, OK, "{}", last_message());
        reader
    }
}

fn append(writer: *mut tickertape_writer, seq: u64) {
    let p = payload(seq);
    let mut got = u64::MAX;
    let status = unsafe {
        tickertape_writer_append(
            writer,
            (seq % 5 + 1) as u32,
            1_000_000 + seq,
            2_000_000 + 3 * seq,
            p.as_ptr(),
            p.len(),
            &mut got,
        )
    };
    assert_eq!(status, OK, "{}", last_message());
    assert_eq!(got, seq);
}

fn empty_record() -> tickertape_record {
    tickertape_record {
        seq: u64::MAX,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload: null(),
        payload_len: 0,
        schema_id: 0,
        reserved: 0,
    }
}

fn check_record(record: &tickertape_record, seq: u64) {
    assert_eq!(record.seq, seq);
    assert_eq!(record.schema_id, (seq % 5 + 1) as u32);
    assert_eq!(record.venue_ts_ns, 1_000_000 + seq);
    assert_eq!(record.local_ts_ns, 2_000_000 + 3 * seq);
    assert_eq!(record.reserved, 0);
    let bytes = if record.payload_len == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(record.payload, record.payload_len) }
    };
    assert_eq!(bytes, payload(seq), "payload of {seq}");
}

// ---- Pure tests (no files; also run under Miri) ----

#[test]
fn pure_versions() {
    assert_eq!(tickertape_abi_version(), TICKERTAPE_ABI_VERSION);
    let number = |v: &str| v.parse::<u32>().unwrap();
    assert_eq!(
        TICKERTAPE_VERSION_NUMBER,
        number(env!("CARGO_PKG_VERSION_MAJOR")) * 1_000_000
            + number(env!("CARGO_PKG_VERSION_MINOR")) * 1_000
            + number(env!("CARGO_PKG_VERSION_PATCH"))
    );
    assert_eq!(tickertape_version_number(), TICKERTAPE_VERSION_NUMBER);
    // Before 1.0 the ABI is unstable (version 0); from 1.0 the ABI version is the major version.
    assert_eq!(
        TICKERTAPE_ABI_VERSION,
        if TICKERTAPE_VERSION_MAJOR == 0 {
            0
        } else {
            TICKERTAPE_VERSION_MAJOR
        }
    );
    let version = unsafe { CStr::from_ptr(tickertape_version()) };
    assert_eq!(version.to_str().unwrap(), env!("CARGO_PKG_VERSION"));
    assert_eq!(version.to_str().unwrap(), tickertape::VERSION);
}

#[test]
fn pure_status_and_reason_names() {
    assert_eq!(name(OK), "TICKERTAPE_OK");
    assert_eq!(
        name(code(tickertape_status_code::TICKERTAPE_E_STORAGE_FULL)),
        "TICKERTAPE_E_STORAGE_FULL"
    );
    assert_eq!(name(12345), "TICKERTAPE_UNKNOWN_STATUS");
    let reason = |r: i32| {
        unsafe { CStr::from_ptr(tickertape_corrupt_reason_name(r)) }
            .to_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(
        reason(tickertape_corrupt_reason::TICKERTAPE_CORRUPT_BAD_CHECKSUM as i32),
        "TICKERTAPE_CORRUPT_BAD_CHECKSUM"
    );
    assert_eq!(reason(-7), "TICKERTAPE_CORRUPT_UNKNOWN");
}

/// Variant names of `pub enum <name>` in the core crate's `error.rs`.
fn core_variants(name: &str) -> BTreeSet<String> {
    let source = include_str!("../../tickertape/src/error.rs");
    let start = source
        .find(&format!("pub enum {name} {{"))
        .expect("enum in error.rs");
    let mut variants = BTreeSet::new();
    for line in source[start..].lines().skip(1) {
        if line.starts_with('}') {
            break;
        }
        // Variants are the lines indented by exactly four spaces that start with an identifier.
        let Some(rest) = line.strip_prefix("    ") else {
            continue;
        };
        if rest.starts_with(|c: char| c.is_ascii_uppercase()) {
            let ident: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            variants.insert(ident);
        }
    }
    assert!(!variants.is_empty(), "no variants parsed for {name}");
    variants
}

#[test]
fn pure_every_error_variant_has_a_code() {
    let path = PathBuf::from("/j");
    let io = |kind_or_errno: Option<i32>| match kind_or_errno {
        Some(errno) => io::Error::from_raw_os_error(errno),
        None => io::Error::other("x"),
    };
    // One sample per variant; adding a variant to the core without a row here fails the name check below, and a
    // row whose variant maps to TICKERTAPE_E_UNKNOWN fails the code check.
    let samples: Vec<(&str, Error, tickertape_status_code)> = vec![
        (
            "Io",
            Error::Io {
                op: "op",
                path: path.clone(),
                source: io(Some(libc_eio())),
            },
            tickertape_status_code::TICKERTAPE_E_IO,
        ),
        (
            "Locked",
            Error::Locked {
                dir: path.clone(),
                pid: None,
            },
            tickertape_status_code::TICKERTAPE_E_LOCKED,
        ),
        (
            "Corrupt",
            Error::Corrupt {
                path: path.clone(),
                offset: 64,
                reason: CorruptReason::BadChecksum,
            },
            tickertape_status_code::TICKERTAPE_E_CORRUPT,
        ),
        (
            "UnsupportedVersion",
            Error::UnsupportedVersion {
                path: path.clone(),
                found: 2,
                supported: 1,
            },
            tickertape_status_code::TICKERTAPE_E_UNSUPPORTED_VERSION,
        ),
        (
            "NotFound",
            Error::NotFound { dir: path.clone() },
            tickertape_status_code::TICKERTAPE_E_NOT_FOUND,
        ),
        (
            "SeqUnavailable",
            Error::SeqUnavailable {
                requested: 1,
                first: 5,
            },
            tickertape_status_code::TICKERTAPE_E_SEQ_UNAVAILABLE,
        ),
        (
            "RecordTooLarge",
            Error::RecordTooLarge { len: 10, max: 5 },
            tickertape_status_code::TICKERTAPE_E_RECORD_TOO_LARGE,
        ),
        (
            "DurabilityLost",
            Error::DurabilityLost,
            tickertape_status_code::TICKERTAPE_E_DURABILITY_LOST,
        ),
        (
            "SequenceExhausted",
            Error::SequenceExhausted,
            tickertape_status_code::TICKERTAPE_E_SEQUENCE_EXHAUSTED,
        ),
        (
            "Discarded",
            Error::Discarded { seq: 3 },
            tickertape_status_code::TICKERTAPE_E_DISCARDED,
        ),
        (
            "InvalidConfig",
            Error::InvalidConfig("x".into()),
            tickertape_status_code::TICKERTAPE_E_INVALID_CONFIG,
        ),
        (
            "BeaconInUse",
            Error::BeaconInUse { path: path.clone() },
            tickertape_status_code::TICKERTAPE_E_BEACON_IN_USE,
        ),
    ];
    let names: BTreeSet<String> = samples.iter().map(|(n, _, _)| n.to_string()).collect();
    assert_eq!(
        names,
        core_variants("Error"),
        "every tickertape::Error variant needs a sample here and a code in status.rs"
    );
    for (variant, error, expected) in &samples {
        assert_eq!(status_of(error), *expected, "{variant}");
        assert_ne!(
            status_of(error),
            tickertape_status_code::TICKERTAPE_E_UNKNOWN
        );
    }
    // The storage-full refinement of Io, and the details recorded with each error.
    for errno in [libc_enospc(), libc_edquot()] {
        let full = Error::Io {
            op: "op",
            path: path.clone(),
            source: io(Some(errno)),
        };
        assert_eq!(
            status_of(&full),
            tickertape_status_code::TICKERTAPE_E_STORAGE_FULL
        );
    }
    let status = guard(|| Err(FfiError::from(&samples[0].1)));
    assert_eq!(status, code(tickertape_status_code::TICKERTAPE_E_IO));
    assert_eq!(tickertape_last_error_code(), status);
    assert_eq!(tickertape_last_error_os_code(), libc_eio());
    assert!(last_message().contains("/j"), "{}", last_message());
    let status = guard(|| Err(FfiError::from(&samples[2].1)));
    assert_eq!(status, code(tickertape_status_code::TICKERTAPE_E_CORRUPT));
    assert_eq!(tickertape_last_error_os_code(), 0);
    assert_eq!(
        tickertape_last_error_corrupt_reason(),
        tickertape_corrupt_reason::TICKERTAPE_CORRUPT_BAD_CHECKSUM as i32
    );
    let _ = io(None);
}

#[test]
fn pure_every_corrupt_reason_has_a_code() {
    use tickertape_corrupt_reason as R;
    let samples: Vec<(&str, CorruptReason, R)> = vec![
        (
            "BadMagic",
            CorruptReason::BadMagic,
            R::TICKERTAPE_CORRUPT_BAD_MAGIC,
        ),
        (
            "BadSegmentHeader",
            CorruptReason::BadSegmentHeader("x"),
            R::TICKERTAPE_CORRUPT_BAD_SEGMENT_HEADER,
        ),
        (
            "SizeMismatch",
            CorruptReason::SizeMismatch {
                expected: 1,
                actual: 2,
            },
            R::TICKERTAPE_CORRUPT_SIZE_MISMATCH,
        ),
        (
            "BaseSeqMismatch",
            CorruptReason::BaseSeqMismatch {
                from_name: 1,
                from_header: 2,
            },
            R::TICKERTAPE_CORRUPT_BASE_SEQ_MISMATCH,
        ),
        (
            "BadOffset",
            CorruptReason::BadOffset(3),
            R::TICKERTAPE_CORRUPT_BAD_OFFSET,
        ),
        (
            "SequenceExhausted",
            CorruptReason::SequenceExhausted,
            R::TICKERTAPE_CORRUPT_SEQUENCE_EXHAUSTED,
        ),
        (
            "BadLength",
            CorruptReason::BadLength(3),
            R::TICKERTAPE_CORRUPT_BAD_LENGTH,
        ),
        (
            "BadChecksum",
            CorruptReason::BadChecksum,
            R::TICKERTAPE_CORRUPT_BAD_CHECKSUM,
        ),
        (
            "BadSequence",
            CorruptReason::BadSequence {
                expected: 1,
                found: 2,
            },
            R::TICKERTAPE_CORRUPT_BAD_SEQUENCE,
        ),
        (
            "BadFlags",
            CorruptReason::BadFlags,
            R::TICKERTAPE_CORRUPT_BAD_FLAGS,
        ),
        (
            "BadSeal",
            CorruptReason::BadSeal,
            R::TICKERTAPE_CORRUPT_BAD_SEAL,
        ),
        (
            "BrokenChain",
            CorruptReason::BrokenChain(4),
            R::TICKERTAPE_CORRUPT_BROKEN_CHAIN,
        ),
        (
            "DanglingCheckpoint",
            CorruptReason::DanglingCheckpoint(4),
            R::TICKERTAPE_CORRUPT_DANGLING_CHECKPOINT,
        ),
        (
            "EmptyCommitWord",
            CorruptReason::EmptyCommitWord(4),
            R::TICKERTAPE_CORRUPT_EMPTY_COMMIT_WORD,
        ),
        (
            "UncheckedTail",
            CorruptReason::UncheckedTail(4),
            R::TICKERTAPE_CORRUPT_UNCHECKED_TAIL,
        ),
    ];
    let names: BTreeSet<String> = samples.iter().map(|(n, _, _)| n.to_string()).collect();
    assert_eq!(
        names,
        core_variants("CorruptReason"),
        "every tickertape::CorruptReason needs a sample here and a code in status.rs"
    );
    let mut codes = BTreeSet::new();
    for (variant, reason, expected) in &samples {
        assert_eq!(corrupt_reason_of(reason), *expected, "{variant}");
        assert!(
            codes.insert(*expected as i32),
            "duplicate code for {variant}"
        );
    }
    // The sequence number an empty commit word's evidence carries reaches C in the message, as other reasons'
    // details do.
    for (reason, expected) in [
        (
            CorruptReason::EmptyCommitWord(123_456),
            R::TICKERTAPE_CORRUPT_EMPTY_COMMIT_WORD,
        ),
        (
            CorruptReason::UncheckedTail(123_457),
            R::TICKERTAPE_CORRUPT_UNCHECKED_TAIL,
        ),
    ] {
        let error = Error::Corrupt {
            path: std::path::PathBuf::from("/j/00000000000000000000.tape"),
            offset: 64,
            reason,
        };
        let status = guard(|| Err(FfiError::from(&error)));
        assert_eq!(status, code(tickertape_status_code::TICKERTAPE_E_CORRUPT));
        assert_eq!(tickertape_last_error_corrupt_reason(), expected as i32);
        let seq = match reason {
            CorruptReason::EmptyCommitWord(seq) | CorruptReason::UncheckedTail(seq) => seq,
            _ => unreachable!(),
        };
        assert!(
            last_message().contains(&format!("sequence {seq}")),
            "{}",
            last_message()
        );
        let name =
            unsafe { std::ffi::CStr::from_ptr(tickertape_corrupt_reason_name(expected as i32)) };
        assert_eq!(name.to_str().unwrap(), format!("{expected:?}"));
    }
}

fn libc_eio() -> i32 {
    5
}
fn libc_enospc() -> i32 {
    28
}
fn libc_edquot() -> i32 {
    if cfg!(target_os = "linux") { 122 } else { 69 }
}

#[test]
fn pure_panics_become_status_and_break_the_handle() {
    let status = guard(|| panic!("boom"));
    assert_eq!(status, code(tickertape_status_code::TICKERTAPE_E_PANIC));
    assert!(last_message().contains("boom"), "{}", last_message());

    let handle = Handle::new(5u32);
    let status = guard(|| {
        handle.with(|_| -> Result<(), FfiError> { panic!("inside") })?;
        Ok(tickertape_status_code::TICKERTAPE_OK)
    });
    assert_eq!(status, code(tickertape_status_code::TICKERTAPE_E_PANIC));
    // The handle stays broken.
    let status = guard(|| {
        handle.with(|_| Ok(()))?;
        Ok(tickertape_status_code::TICKERTAPE_OK)
    });
    assert_eq!(status, code(tickertape_status_code::TICKERTAPE_E_PANIC));
    assert!(last_message().contains("broken"), "{}", last_message());
}

#[test]
fn pure_reentrant_calls_are_busy() {
    let handle = Handle::new(0u32);
    let status = guard(|| {
        handle.with(|_| {
            let inner = handle.with(|_| Ok(()));
            assert_eq!(
                inner.unwrap_err().code,
                tickertape_status_code::TICKERTAPE_E_BUSY
            );
            assert!(handle.is_busy());
            Ok(())
        })?;
        Ok(tickertape_status_code::TICKERTAPE_OK)
    });
    assert_eq!(status, OK);
    assert!(!handle.is_busy());
}

#[test]
fn pure_null_arguments_are_rejected() {
    let dir = c"/nonexistent/tickertape-ffi-test";
    let mut seq = 0u64;
    let mut count = 0usize;
    let mut record = empty_record();
    let mut writer = null_mut();
    let mut reader = null_mut();
    unsafe {
        assert_eq!(tickertape_writer_open(null(), null(), &mut writer), INVALID);
        assert!(writer.is_null());
        assert_eq!(
            tickertape_writer_open(c"".as_ptr(), null(), &mut writer),
            INVALID
        );
        assert_eq!(
            tickertape_writer_open(dir.as_ptr(), null(), null_mut()),
            INVALID
        );
        assert_eq!(
            tickertape_writer_append(null_mut(), 0, 0, 0, null(), 0, &mut seq),
            INVALID
        );
        assert!(last_message().contains("writer"), "{}", last_message());
        assert_eq!(tickertape_last_error_code(), INVALID);
        assert_eq!(
            tickertape_writer_append_with(null_mut(), 0, 0, 0, 0, None, null_mut(), null_mut()),
            INVALID
        );
        assert_eq!(
            tickertape_writer_append_batch(null_mut(), null(), 0, &mut count, null_mut()),
            INVALID
        );
        assert_eq!(tickertape_writer_checkpoint(null_mut()), INVALID);
        assert_eq!(tickertape_writer_next_seq(null_mut(), &mut seq), INVALID);
        assert_eq!(tickertape_writer_last_retention_error(null_mut()), INVALID);
        assert_eq!(tickertape_writer_close(null_mut()), OK);

        assert_eq!(tickertape_reader_open(null(), null(), &mut reader), INVALID);
        assert_eq!(
            tickertape_reader_open(dir.as_ptr(), null(), null_mut()),
            INVALID
        );
        assert_eq!(
            tickertape_reader_open_mapped_unsafe(null(), null(), &mut reader),
            INVALID
        );
        assert_eq!(tickertape_reader_seek(null_mut(), 0), INVALID);
        assert_eq!(tickertape_reader_next(null_mut(), 0, &mut record), INVALID);
        assert_eq!(
            tickertape_reader_next_copy(null_mut(), 0, null_mut(), 0, &mut record),
            INVALID
        );
        assert_eq!(
            tickertape_reader_read_batch(null_mut(), 0, &mut record, 1, null_mut(), 0, &mut count),
            INVALID
        );
        assert_eq!(tickertape_reader_position(null_mut(), &mut seq), INVALID);
        assert_eq!(tickertape_reader_close(null_mut()), OK);

        assert_eq!(tickertape_verify(dir.as_ptr(), null_mut()), INVALID);
        assert_eq!(
            tickertape_list_segments(dir.as_ptr(), null_mut(), 0, null_mut()),
            INVALID
        );
        assert_eq!(
            tickertape_list_segments(dir.as_ptr(), null_mut(), 3, &mut count),
            INVALID
        );
        assert_eq!(
            tickertape_read_checkpoint(dir.as_ptr(), null_mut()),
            INVALID
        );
        assert_eq!(
            tickertape_retention_apply(dir.as_ptr(), null(), 1, null_mut()),
            INVALID
        );

        assert_eq!(tickertape_writer_options_set_notify(null_mut(), 1), INVALID);
        assert_eq!(tickertape_reader_options_set_notify(null_mut(), 1), INVALID);
        assert_eq!(tickertape_retention_set_delete(null_mut()), INVALID);
        tickertape_writer_options_free(null_mut());
        tickertape_reader_options_free(null_mut());
        tickertape_retention_free(null_mut());
    }
}

#[test]
fn pure_options_validate_their_values() {
    unsafe {
        let w = tickertape_writer_options_new();
        assert!(!w.is_null());
        assert_eq!(tickertape_writer_options_set_recovery_policy(w, 1), OK);
        assert_eq!((*w).config.recovery, tickertape::RecoveryPolicy::Fail);
        assert_eq!(tickertape_writer_options_set_recovery_policy(w, 7), INVALID);
        assert_eq!(tickertape_writer_options_set_notify(w, 0), OK);
        assert!(!(*w).config.notify);
        assert_eq!(tickertape_writer_options_set_prefault(w, 2), OK);
        assert!((*w).config.prefault);
        assert_eq!(tickertape_writer_options_set_segment_size(w, 65_536), OK);
        assert_eq!((*w).config.segment_size, 65_536);

        let r = tickertape_retention_new();
        assert_eq!(tickertape_retention_set_max_segments(r, 3), OK);
        // No action yet: refusing is the only safe default.
        assert_eq!(tickertape_writer_options_set_retention(w, r), INVALID);
        assert!(last_message().contains("action"), "{}", last_message());
        assert_eq!(tickertape_retention_set_move_to(r, null()), INVALID);
        assert_eq!(
            tickertape_retention_set_move_to(r, c"/archive".as_ptr()),
            OK
        );
        assert_eq!(tickertape_writer_options_set_retention(w, r), OK);
        let (policy, action) = (*w).config.retention.clone().unwrap();
        assert_eq!(policy.max_segments, Some(3));
        assert_eq!(
            action,
            tickertape::retention::Action::MoveTo(PathBuf::from("/archive"))
        );
        assert_eq!(tickertape_writer_options_set_retention(w, null()), OK);
        assert!((*w).config.retention.is_none());
        tickertape_retention_free(r);
        tickertape_writer_options_free(w);

        let o = tickertape_reader_options_new();
        assert_eq!(
            tickertape_reader_options_set_backoff(o, 1, 2, 10, 5),
            INVALID
        );
        assert_eq!(tickertape_reader_options_set_backoff(o, 1, 2, 5, 10), OK);
        assert_eq!((*o).config.backoff.spins, 1);
        assert_eq!((*o).config.backoff.max_sleep.as_nanos(), 10);
        assert_eq!(tickertape_reader_options_set_busy_spin(o), OK);
        assert_eq!((*o).config.backoff, tickertape::Backoff::busy_spin());
        assert_eq!(tickertape_reader_options_set_durable_only(o, 1), OK);
        assert!((*o).config.durable_only);
        tickertape_reader_options_free(o);
    }
}

#[test]
fn pure_report_struct_size_is_checked() {
    let mut report = tickertape_verify_report {
        struct_size: 8,
        ..Default::default()
    };
    let status = unsafe { tickertape_verify(c"/nonexistent".as_ptr(), &mut report) };
    assert_eq!(status, INVALID);
    assert!(last_message().contains("struct_size"), "{}", last_message());
}

// ---- Tests with real journals ----

#[test]
fn write_read_roundtrip_with_both_readers() {
    let j = Journal::new();
    let writer = j.writer(65_536);
    for seq in 0..500 {
        append(writer, seq);
    }
    let mut next = 0;
    assert_eq!(unsafe { tickertape_writer_next_seq(writer, &mut next) }, OK);
    assert_eq!(next, 500);
    for mapped in [false, true] {
        let reader = j.reader(false, mapped);
        let mut record = empty_record();
        for seq in 0..500 {
            assert_eq!(
                unsafe { tickertape_reader_next(reader, 0, &mut record) },
                OK
            );
            check_record(&record, seq);
        }
        let mut untouched = empty_record();
        assert_eq!(
            unsafe { tickertape_reader_next(reader, 0, &mut untouched) },
            NONE
        );
        assert_eq!(untouched.seq, u64::MAX, "NONE leaves the output unchanged");
        // A positive timeout passes without a record.
        assert_eq!(
            unsafe { tickertape_reader_next(reader, 1_000_000, &mut untouched) },
            NONE
        );
        let mut position = 0;
        assert_eq!(
            unsafe { tickertape_reader_position(reader, &mut position) },
            OK
        );
        assert_eq!(position, 500);
        assert_eq!(unsafe { tickertape_reader_seek(reader, 123) }, OK);
        assert_eq!(
            unsafe { tickertape_reader_next(reader, -1, &mut record) },
            OK
        );
        check_record(&record, 123);
        assert_eq!(unsafe { tickertape_reader_close(reader) }, OK);
    }
    // More than one segment was written.
    let mut count = 0;
    assert_eq!(
        unsafe { tickertape_list_segments(j.c_dir.as_ptr(), null_mut(), 0, &mut count) },
        OK
    );
    assert!(count > 1, "{count} segments");
    let mut infos = vec![tickertape_segment_info::default(); count - 1];
    assert_eq!(
        unsafe {
            tickertape_list_segments(
                j.c_dir.as_ptr(),
                infos.as_mut_ptr(),
                infos.len(),
                &mut count,
            )
        },
        code(tickertape_status_code::TICKERTAPE_E_BUFFER_TOO_SMALL)
    );
    infos.push(tickertape_segment_info::default());
    assert_eq!(
        unsafe {
            tickertape_list_segments(
                j.c_dir.as_ptr(),
                infos.as_mut_ptr(),
                infos.len(),
                &mut count,
            )
        },
        OK
    );
    assert_eq!(infos[0].base_seq, 0);
    assert!(infos.windows(2).all(|w| w[0].base_seq < w[1].base_seq));
    assert!(
        infos
            .iter()
            .all(|i| i.segment_size == 65_536 && i.format_version == 1)
    );
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);
}

#[test]
fn checkpoint_durable_only_and_inspection() {
    let j = Journal::new();
    let writer = j.writer(65_536);
    let mut checkpoint = tickertape_checkpoint::default();
    assert_eq!(
        unsafe { tickertape_read_checkpoint(j.c_dir.as_ptr(), &mut checkpoint) },
        NONE
    );
    for seq in 0..10 {
        append(writer, seq);
    }
    let durable = j.reader(true, false);
    let mut record = empty_record();
    assert_eq!(
        unsafe { tickertape_reader_next(durable, 0, &mut record) },
        NONE,
        "no checkpoint yet: nothing is durable"
    );
    assert_eq!(unsafe { tickertape_writer_checkpoint(writer) }, OK);
    for seq in 10..15 {
        append(writer, seq);
    }
    for seq in 0..10 {
        assert_eq!(
            unsafe { tickertape_reader_next(durable, 0, &mut record) },
            OK
        );
        check_record(&record, seq);
    }
    assert_eq!(
        unsafe { tickertape_reader_next(durable, 1_000_000, &mut record) },
        NONE,
        "records 10.. are visible but not durable"
    );
    assert_eq!(
        unsafe { tickertape_read_checkpoint(j.c_dir.as_ptr(), &mut checkpoint) },
        OK
    );
    assert_eq!(checkpoint.next_seq, 10);
    assert_eq!(checkpoint.generation, 1);

    let mut report = tickertape_verify_report {
        struct_size: size_of::<tickertape_verify_report>() as u64,
        ..Default::default()
    };
    assert_eq!(
        unsafe { tickertape_verify(j.c_dir.as_ptr(), &mut report) },
        OK
    );
    assert_eq!(report.records, 15);
    assert_eq!((report.first_seq, report.last_seq), (0, 14));
    assert_eq!(report.problems, 0);
    assert_eq!(
        report.struct_size,
        size_of::<tickertape_verify_report>() as u64
    );
    assert_eq!(unsafe { tickertape_reader_close(durable) }, OK);
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);
}

#[test]
fn copy_out_and_batches_hold_back_records_that_do_not_fit() {
    let j = Journal::new();
    let writer = j.writer(65_536);
    for seq in 0..40 {
        append(writer, seq);
    }
    let reader = j.reader(false, false);
    let mut record = empty_record();
    // Record 0 has an empty payload; record 1 has 37 bytes.
    let mut small = [0u8; 8];
    assert_eq!(
        unsafe { tickertape_reader_next_copy(reader, 0, null_mut(), 0, &mut record) },
        OK
    );
    check_record(&record, 0);
    assert_eq!(
        unsafe { tickertape_reader_next_copy(reader, 0, small.as_mut_ptr(), 8, &mut record) },
        code(tickertape_status_code::TICKERTAPE_E_BUFFER_TOO_SMALL)
    );
    assert_eq!((record.seq, record.payload_len), (1, 37));
    assert!(record.payload.is_null());
    let mut position = 0;
    assert_eq!(
        unsafe { tickertape_reader_position(reader, &mut position) },
        OK
    );
    assert_eq!(position, 1, "the held-back record is next");
    // A borrowed view of the held-back record.
    assert_eq!(
        unsafe { tickertape_reader_next(reader, 0, &mut record) },
        OK
    );
    check_record(&record, 1);

    // Batch: payloads of 2..6 are 74, 111, 148, 185, 222 bytes.
    let mut records = vec![empty_record(); 16];
    let mut buf = vec![0u8; 74 + 111 + 100];
    let mut count = 99;
    let status = unsafe {
        tickertape_reader_read_batch(
            reader,
            0,
            records.as_mut_ptr(),
            records.len(),
            buf.as_mut_ptr(),
            buf.len(),
            &mut count,
        )
    };
    assert_eq!(status, OK);
    assert_eq!(count, 2);
    check_record(&records[0], 2);
    check_record(&records[1], 3);
    assert_eq!(records[1].payload, unsafe { buf.as_ptr().add(74) });
    // Record 4 (148 bytes) was held back; a buffer that is too small for it reports the size needed.
    let status = unsafe {
        tickertape_reader_read_batch(
            reader,
            0,
            records.as_mut_ptr(),
            records.len(),
            buf.as_mut_ptr(),
            100,
            &mut count,
        )
    };
    assert_eq!(
        status,
        code(tickertape_status_code::TICKERTAPE_E_BUFFER_TOO_SMALL)
    );
    assert_eq!((count, records[0].seq, records[0].payload_len), (0, 4, 148));
    // Read everything that is left in one go.
    let mut big = vec![0u8; 64 * 1024];
    let status = unsafe {
        tickertape_reader_read_batch(
            reader,
            0,
            records.as_mut_ptr(),
            records.len(),
            big.as_mut_ptr(),
            big.len(),
            &mut count,
        )
    };
    assert_eq!(status, OK);
    assert_eq!(count, 16);
    for (i, r) in records.iter().enumerate() {
        check_record(r, 4 + i as u64);
    }
    let mut rest = 0;
    let status = unsafe {
        tickertape_reader_read_batch(
            reader,
            0,
            records.as_mut_ptr(),
            records.len(),
            big.as_mut_ptr(),
            big.len(),
            &mut rest,
        )
    };
    assert_eq!((status, rest), (OK, 16));
    check_record(&records[15], 35);
    let status = unsafe {
        tickertape_reader_read_batch(
            reader,
            0,
            records.as_mut_ptr(),
            records.len(),
            big.as_mut_ptr(),
            big.len(),
            &mut rest,
        )
    };
    assert_eq!((status, rest), (OK, 4));
    check_record(&records[3], 39);
    assert_eq!(
        unsafe {
            tickertape_reader_read_batch(
                reader,
                0,
                records.as_mut_ptr(),
                records.len(),
                big.as_mut_ptr(),
                big.len(),
                &mut rest,
            )
        },
        NONE
    );
    assert_eq!(rest, 0);
    assert_eq!(
        unsafe {
            tickertape_reader_read_batch(
                reader,
                0,
                records.as_mut_ptr(),
                0,
                big.as_mut_ptr(),
                big.len(),
                &mut rest,
            )
        },
        INVALID
    );
    assert_eq!(unsafe { tickertape_reader_close(reader) }, OK);
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);
}

#[test]
fn batch_errors_after_records_are_deferred_to_the_next_call() {
    let j = Journal::new();
    let writer = j.writer(65_536);
    let payload = [7u8; 8];
    for _ in 0..5 {
        let status =
            unsafe { tickertape_writer_append(writer, 1, 0, 0, payload.as_ptr(), 8, null_mut()) };
        assert_eq!(status, OK);
    }
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);
    // Each record is 48 bytes from offset 64; flip a payload byte of record 3.
    let segment = j.dir.join(format!("{:020}.tape", 0));
    let mut bytes = std::fs::read(&segment).unwrap();
    bytes[64 + 48 * 3 + 40] ^= 0xFF;
    std::fs::write(&segment, bytes).unwrap();

    let reader = j.reader(false, false);
    let mut records = vec![empty_record(); 10];
    let mut buf = vec![0u8; 1024];
    let mut count = 0;
    let batch = |count: &mut usize, records: &mut Vec<tickertape_record>, buf: &mut Vec<u8>| unsafe {
        tickertape_reader_read_batch(
            reader,
            0,
            records.as_mut_ptr(),
            records.len(),
            buf.as_mut_ptr(),
            buf.len(),
            count,
        )
    };
    assert_eq!(batch(&mut count, &mut records, &mut buf), OK);
    assert_eq!(count, 3, "records before the corruption are returned");
    let corrupt = code(tickertape_status_code::TICKERTAPE_E_CORRUPT);
    assert_eq!(batch(&mut count, &mut records, &mut buf), corrupt);
    assert_eq!(count, 0);
    assert_eq!(
        tickertape_last_error_corrupt_reason(),
        tickertape_corrupt_reason::TICKERTAPE_CORRUPT_BAD_CHECKSUM as i32
    );
    // The cursor did not move: the next call reports the corruption again.
    let mut record = empty_record();
    assert_eq!(
        unsafe { tickertape_reader_next(reader, 0, &mut record) },
        corrupt
    );
    let mut position = 0;
    assert_eq!(
        unsafe { tickertape_reader_position(reader, &mut position) },
        OK
    );
    assert_eq!(position, 3);
    assert_eq!(unsafe { tickertape_reader_close(reader) }, OK);

    let mut report = tickertape_verify_report {
        struct_size: size_of::<tickertape_verify_report>() as u64,
        ..Default::default()
    };
    assert_eq!(
        unsafe { tickertape_verify(j.c_dir.as_ptr(), &mut report) },
        OK
    );
    assert_eq!(report.problems, 1);
    assert_eq!(report.first_problem_base_seq, 0);
    assert_eq!(report.first_problem_offset, 64 + 48 * 3);
    assert_eq!(
        report.first_problem_reason,
        tickertape_corrupt_reason::TICKERTAPE_CORRUPT_BAD_CHECKSUM as i32
    );
}

struct FillState {
    writer: *mut tickertape_writer,
    abort: bool,
    reentrant_status: i32,
    calls: usize,
}

unsafe extern "C-unwind" fn fill(user_data: *mut c_void, buf: *mut u8, len: usize) -> i32 {
    let state = unsafe { &mut *user_data.cast::<FillState>() };
    state.calls += 1;
    let buf = unsafe { std::slice::from_raw_parts_mut(buf, len) };
    assert!(buf.iter().all(|&b| b == 0), "the buffer starts zeroed");
    // The test appends 37-byte payloads: payload(1).
    buf.copy_from_slice(&payload(1));
    let mut seq = 0;
    state.reentrant_status = unsafe { tickertape_writer_next_seq(state.writer, &mut seq) };
    i32::from(state.abort)
}

#[test]
fn append_with_commits_aborts_and_detects_reentrancy() {
    let j = Journal::new();
    let writer = j.writer(65_536);
    let mut state = FillState {
        writer,
        abort: false,
        reentrant_status: OK,
        calls: 0,
    };
    let user = (&raw mut state).cast::<c_void>();
    let mut seq = u64::MAX;
    // 37 bytes: payload(1).
    let status = unsafe {
        tickertape_writer_append_with(
            writer,
            2,
            1_000_001,
            2_000_003,
            37,
            Some(fill),
            user,
            &mut seq,
        )
    };
    assert_eq!(status, OK, "{}", last_message());
    assert_eq!(seq, 0);
    assert_eq!(state.calls, 1);
    assert_eq!(
        state.reentrant_status,
        code(tickertape_status_code::TICKERTAPE_E_BUSY),
        "a callback cannot call back into its writer"
    );
    // The callback reads the state through `user`.
    unsafe { (*user.cast::<FillState>()).abort = true };
    let status =
        unsafe { tickertape_writer_append_with(writer, 2, 0, 0, 37, Some(fill), user, &mut seq) };
    assert_eq!(status, code(tickertape_status_code::TICKERTAPE_E_ABORTED));
    assert_eq!(seq, 0, "seq_out is untouched on failure");
    let mut next = 0;
    assert_eq!(unsafe { tickertape_writer_next_seq(writer, &mut next) }, OK);
    assert_eq!(next, 1, "nothing was committed");
    // The writer still works after an abort.
    append_raw(writer, b"after abort");
    let reader = j.reader(false, false);
    let mut record = empty_record();
    assert_eq!(
        unsafe { tickertape_reader_next(reader, 0, &mut record) },
        OK
    );
    assert_eq!(record.seq, 0);
    assert_eq!(
        unsafe { std::slice::from_raw_parts(record.payload, record.payload_len) },
        payload(1)
    );
    assert_eq!(
        unsafe { tickertape_reader_next(reader, 0, &mut record) },
        OK
    );
    assert_eq!(
        unsafe { std::slice::from_raw_parts(record.payload, record.payload_len) },
        b"after abort"
    );
    assert_eq!(unsafe { tickertape_reader_close(reader) }, OK);
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);
}

fn append_raw(writer: *mut tickertape_writer, bytes: &[u8]) -> u64 {
    let mut seq = 0;
    let status =
        unsafe { tickertape_writer_append(writer, 9, 0, 0, bytes.as_ptr(), bytes.len(), &mut seq) };
    assert_eq!(status, OK, "{}", last_message());
    seq
}

#[test]
fn append_batch_validates_first_and_reports_partial_success() {
    let j = Journal::new();
    let writer = j.writer(65_536);
    let a = b"alpha";
    let mut records = [tickertape_new_record {
        payload: a.as_ptr(),
        payload_len: a.len(),
        venue_ts_ns: 1,
        local_ts_ns: 2,
        schema_id: 3,
        reserved: 0,
    }; 3];
    records[2].reserved = 1;
    let (mut appended, mut first) = (99usize, 99u64);
    let status = unsafe {
        tickertape_writer_append_batch(writer, records.as_ptr(), 3, &mut appended, &mut first)
    };
    assert_eq!(status, INVALID);
    assert_eq!(appended, 0, "argument errors append nothing");
    records[2].reserved = 0;
    records[1].payload = null();
    assert_eq!(
        unsafe {
            tickertape_writer_append_batch(writer, records.as_ptr(), 3, &mut appended, &mut first)
        },
        INVALID
    );
    assert!(last_message().contains("records[1]"), "{}", last_message());
    records[1].payload_len = 0;
    assert_eq!(
        unsafe {
            tickertape_writer_append_batch(writer, records.as_ptr(), 3, &mut appended, &mut first)
        },
        OK
    );
    assert_eq!((appended, first), (3, 0));

    // A record that is too large stops the batch after the records before it.
    let max = tickertape::format::max_payload_len(65_536) as usize;
    let huge = vec![1u8; max + 1];
    records[1] = tickertape_new_record {
        payload: huge.as_ptr(),
        payload_len: huge.len(),
        ..records[0]
    };
    let status = unsafe {
        tickertape_writer_append_batch(writer, records.as_ptr(), 3, &mut appended, &mut first)
    };
    assert_eq!(
        status,
        code(tickertape_status_code::TICKERTAPE_E_RECORD_TOO_LARGE)
    );
    assert_eq!((appended, first), (1, 3));
    let mut next = 0;
    assert_eq!(unsafe { tickertape_writer_next_seq(writer, &mut next) }, OK);
    assert_eq!(next, 4);
    assert_eq!(
        unsafe { tickertape_writer_append_batch(writer, null(), 0, &mut appended, null_mut()) },
        OK
    );
    assert_eq!(appended, 0);
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);
}

#[test]
fn open_errors_map_to_codes() {
    let j = Journal::new();
    let mut reader = null_mut();
    assert_eq!(
        unsafe { tickertape_reader_open(j.c_dir.as_ptr(), null(), &mut reader) },
        code(tickertape_status_code::TICKERTAPE_E_IO)
    );
    std::fs::create_dir_all(&j.dir).unwrap();
    assert_eq!(
        unsafe { tickertape_reader_open(j.c_dir.as_ptr(), null(), &mut reader) },
        code(tickertape_status_code::TICKERTAPE_E_NOT_FOUND)
    );
    assert!(reader.is_null());

    let writer = j.writer(65_536);
    let mut second = null_mut();
    assert_eq!(
        unsafe { tickertape_writer_open(j.c_dir.as_ptr(), null(), &mut second) },
        code(tickertape_status_code::TICKERTAPE_E_LOCKED)
    );
    assert!(second.is_null());
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);

    let options = tickertape_writer_options_new();
    assert_eq!(
        unsafe { tickertape_writer_options_set_segment_size(options, 1000) },
        OK
    );
    assert_eq!(
        unsafe { tickertape_writer_open(j.c_dir.as_ptr(), options, &mut second) },
        code(tickertape_status_code::TICKERTAPE_E_INVALID_CONFIG)
    );
    unsafe { tickertape_writer_options_free(options) };
    assert_eq!(
        name(tickertape_last_error_code()),
        "TICKERTAPE_E_INVALID_CONFIG"
    );
}

#[test]
fn retention_plans_applies_and_readers_see_seq_unavailable() {
    let j = Journal::new();
    let writer = j.writer(65_536);
    for seq in 0..1500 {
        append(writer, seq);
    }
    assert_eq!(unsafe { tickertape_writer_checkpoint(writer) }, OK);
    let retention = tickertape_retention_new();
    assert_eq!(
        unsafe { tickertape_retention_set_max_segments(retention, 1) },
        OK
    );
    let mut report = tickertape_retention_report {
        struct_size: size_of::<tickertape_retention_report>() as u64,
        ..Default::default()
    };
    assert_eq!(
        unsafe { tickertape_retention_apply(j.c_dir.as_ptr(), retention, 0, &mut report) },
        INVALID,
        "applying needs an action"
    );
    assert_eq!(
        unsafe { tickertape_retention_apply(j.c_dir.as_ptr(), retention, 1, &mut report) },
        OK
    );
    let planned = report.removed_segments;
    assert!(planned >= 1, "{report:?}");
    assert_eq!(report.removed_bytes, planned * 65_536);
    let mut count = 0;
    assert_eq!(
        unsafe { tickertape_list_segments(j.c_dir.as_ptr(), null_mut(), 0, &mut count) },
        OK
    );
    let before = count;
    assert_eq!(unsafe { tickertape_retention_set_delete(retention) }, OK);
    assert_eq!(
        unsafe { tickertape_retention_apply(j.c_dir.as_ptr(), retention, 0, &mut report) },
        OK
    );
    assert_eq!(report.removed_segments, planned);
    assert_eq!(
        unsafe { tickertape_list_segments(j.c_dir.as_ptr(), null_mut(), 0, &mut count) },
        OK
    );
    assert_eq!(count as u64, before as u64 - planned);
    assert_eq!(
        unsafe { tickertape_writer_last_retention_error(writer) },
        OK
    );

    let reader = j.reader(false, false);
    assert_eq!(
        unsafe { tickertape_reader_seek(reader, 0) },
        code(tickertape_status_code::TICKERTAPE_E_SEQ_UNAVAILABLE)
    );
    assert_eq!(unsafe { tickertape_reader_close(reader) }, OK);
    unsafe { tickertape_retention_free(retention) };
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);
}

#[test]
fn writer_retention_errors_are_reported_without_failing() {
    let j = Journal::new();
    let retention = tickertape_retention_new();
    assert_eq!(
        unsafe { tickertape_retention_set_max_segments(retention, 1) },
        OK
    );
    let missing = cstr(&j.dir.with_file_name("missing-archive"));
    assert_eq!(
        unsafe { tickertape_retention_set_move_to(retention, missing.as_ptr()) },
        OK
    );
    let options = tickertape_writer_options_new();
    unsafe {
        assert_eq!(
            tickertape_writer_options_set_segment_size(options, 65_536),
            OK
        );
        assert_eq!(
            tickertape_writer_options_set_retention(options, retention),
            OK
        );
        tickertape_retention_free(retention);
    }
    let mut writer = null_mut();
    assert_eq!(
        unsafe { tickertape_writer_open(j.c_dir.as_ptr(), options, &mut writer) },
        OK
    );
    unsafe { tickertape_writer_options_free(options) };
    for seq in 0..1500 {
        append(writer, seq);
    }
    assert_eq!(unsafe { tickertape_writer_checkpoint(writer) }, OK);
    let status = unsafe { tickertape_writer_last_retention_error(writer) };
    assert_eq!(status, code(tickertape_status_code::TICKERTAPE_E_IO));
    assert_eq!(tickertape_last_error_code(), status);
    assert!(
        last_message().contains("missing-archive"),
        "{}",
        last_message()
    );
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);
}

#[test]
fn handles_move_between_threads() {
    struct Send<T>(T);
    unsafe impl<T> std::marker::Send for Send<T> {}
    let j = Journal::new();
    let writer = Send(j.writer(65_536));
    let writer = std::thread::spawn(move || {
        let w = writer;
        append(w.0, 0);
        w
    })
    .join()
    .unwrap();
    append(writer.0, 1);
    let reader = Send(j.reader(false, false));
    let follower = std::thread::spawn(move || {
        let r = reader;
        let mut record = empty_record();
        for seq in 0..3 {
            assert_eq!(
                unsafe { tickertape_reader_next(r.0, 5_000_000_000, &mut record) },
                OK
            );
            check_record(&record, seq);
        }
        assert_eq!(unsafe { tickertape_reader_close(r.0) }, OK);
    });
    append(writer.0, 2);
    follower.join().unwrap();
    assert_eq!(unsafe { tickertape_writer_close(writer.0) }, OK);
}

// ---- Review round 1: regression tests ----

use tickertape_status_code as S;

fn new_record(bytes: &[u8]) -> tickertape_new_record {
    tickertape_new_record {
        payload: bytes.as_ptr(),
        payload_len: bytes.len(),
        venue_ts_ns: 1,
        local_ts_ns: 2,
        schema_id: 3,
        reserved: 0,
    }
}

fn next_seq(writer: *mut tickertape_writer) -> u64 {
    let mut next = u64::MAX;
    assert_eq!(unsafe { tickertape_writer_next_seq(writer, &mut next) }, OK);
    next
}

fn position(reader: *mut tickertape_reader) -> u64 {
    let mut seq = u64::MAX;
    assert_eq!(unsafe { tickertape_reader_position(reader, &mut seq) }, OK);
    seq
}

/// Moves the journal directory away for the duration of `body`, so that `tickertape_reader_seek` fails (listing
/// the segments returns `ENOENT`) while a reader's open segment stays readable.
fn with_journal_moved<R>(j: &Journal, body: impl FnOnce() -> R) -> R {
    let moved = j.dir.with_file_name("moved-away");
    std::fs::rename(&j.dir, &moved).unwrap();
    let result = body();
    std::fs::rename(&moved, &j.dir).unwrap();
    result
}

#[test]
fn failed_seek_keeps_the_held_back_record() {
    let j = Journal::new();
    let writer = j.writer(65_536);
    for seq in 0..4 {
        append(writer, seq);
    }
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);
    let reader = j.reader(false, false);
    let mut record = empty_record();
    let mut small = [0u8; 8];
    let mut big = [0u8; 512];
    // Record 0 is empty; record 1 (37 bytes) does not fit in 8 bytes and is held back.
    assert_eq!(
        unsafe { tickertape_reader_next_copy(reader, 0, big.as_mut_ptr(), big.len(), &mut record) },
        OK
    );
    check_record(&record, 0);
    assert_eq!(
        unsafe {
            tickertape_reader_next_copy(reader, 0, small.as_mut_ptr(), small.len(), &mut record)
        },
        code(S::TICKERTAPE_E_BUFFER_TOO_SMALL)
    );
    let status = with_journal_moved(&j, || unsafe { tickertape_reader_seek(reader, 3) });
    assert_eq!(status, code(S::TICKERTAPE_E_IO), "{}", last_message());
    assert_eq!(
        position(reader),
        1,
        "a failed seek does not move the reader"
    );
    assert_eq!(
        unsafe { tickertape_reader_next_copy(reader, 0, big.as_mut_ptr(), big.len(), &mut record) },
        OK
    );
    check_record(&record, 1);
    // A failed seek with SEQ_UNAVAILABLE behaves the same way: nothing is lost.
    assert_eq!(
        unsafe {
            tickertape_reader_next_copy(reader, 0, small.as_mut_ptr(), small.len(), &mut record)
        },
        code(S::TICKERTAPE_E_BUFFER_TOO_SMALL)
    );
    assert_eq!(record.seq, 2);
    assert_eq!(unsafe { tickertape_reader_close(reader) }, OK);
}

#[test]
fn failed_seek_keeps_the_deferred_error() {
    let j = Journal::new();
    let writer = j.writer(65_536);
    let payload = [7u8; 8];
    for _ in 0..5 {
        let status =
            unsafe { tickertape_writer_append(writer, 1, 0, 0, payload.as_ptr(), 8, null_mut()) };
        assert_eq!(status, OK);
    }
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);
    let segment = j.dir.join(format!("{:020}.tape", 0));
    let flip = |segment: &Path| {
        let mut bytes = std::fs::read(segment).unwrap();
        bytes[64 + 48 * 3 + 40] ^= 0xFF;
        std::fs::write(segment, bytes).unwrap();
    };
    flip(&segment);
    let reader = j.reader(false, false);
    let mut records = [empty_record(); 10];
    let mut buf = [0u8; 1024];
    let mut count = 0;
    let status = unsafe {
        tickertape_reader_read_batch(
            reader,
            0,
            records.as_mut_ptr(),
            records.len(),
            buf.as_mut_ptr(),
            buf.len(),
            &mut count,
        )
    };
    assert_eq!((status, count), (OK, 3));
    let status = with_journal_moved(&j, || unsafe { tickertape_reader_seek(reader, 0) });
    assert_eq!(status, code(S::TICKERTAPE_E_IO), "{}", last_message());
    // Repair the record: the error deferred by the batch is still reported (once), then reading continues.
    flip(&segment);
    let mut record = empty_record();
    assert_eq!(
        unsafe { tickertape_reader_next(reader, 0, &mut record) },
        code(S::TICKERTAPE_E_CORRUPT),
        "the deferred error survives a failed seek"
    );
    assert_eq!(
        unsafe { tickertape_reader_next(reader, 0, &mut record) },
        OK
    );
    assert_eq!(record.seq, 3);
    // A successful seek clears a deferred error.
    flip(&segment);
    assert_eq!(unsafe { tickertape_reader_seek(reader, 0) }, OK);
    let status = unsafe {
        tickertape_reader_read_batch(
            reader,
            0,
            records.as_mut_ptr(),
            records.len(),
            buf.as_mut_ptr(),
            buf.len(),
            &mut count,
        )
    };
    assert_eq!((status, count), (OK, 3));
    flip(&segment);
    assert_eq!(unsafe { tickertape_reader_seek(reader, 0) }, OK);
    assert_eq!(
        unsafe { tickertape_reader_next(reader, 0, &mut record) },
        OK
    );
    assert_eq!(record.seq, 0);
    assert_eq!(unsafe { tickertape_reader_close(reader) }, OK);
}

#[test]
fn pure_append_batch_rejects_a_count_too_large() {
    // Never dereferenced: the count is rejected before the array is touched.
    let records = std::ptr::NonNull::<tickertape_new_record>::dangling().as_ptr();
    let mut appended = 9usize;
    let status = unsafe {
        tickertape_writer_append_batch(
            null_mut(),
            records,
            usize::MAX / 8,
            &mut appended,
            null_mut(),
        )
    };
    assert_eq!(status, INVALID);
    assert!(last_message().contains("count"), "{}", last_message());
    assert_eq!(appended, 0);
}

#[test]
fn pure_append_batch_reports_the_failing_record() {
    let byte = 0u8;
    let mut records = [new_record(b"ok"), new_record(b"ok")];
    records[1].payload = &byte;
    records[1].payload_len = isize::MAX as usize + 1;
    let mut appended = 9usize;
    let status = unsafe {
        tickertape_writer_append_batch(null_mut(), records.as_ptr(), 2, &mut appended, null_mut())
    };
    assert_eq!(status, INVALID);
    let message = last_message();
    assert!(
        message.contains("records[1]") && message.contains("too large"),
        "{message}"
    );
    records[1].payload = null();
    records[1].payload_len = 1;
    let status = unsafe {
        tickertape_writer_append_batch(null_mut(), records.as_ptr(), 2, &mut appended, null_mut())
    };
    assert_eq!(status, INVALID);
    let message = last_message();
    assert!(
        message.contains("records[1].payload") && message.contains("NULL"),
        "{message}"
    );
}

#[test]
fn overlapping_arguments_are_rejected() {
    // Every slot a rejected call could write holds a non-zero sentinel, and each rejected call must leave all of them
    // unchanged: an overlap returns TICKERTAPE_E_INVALID_ARGUMENT before anything is written.
    const WORD: u64 = 0xA5A5_A5A5_A5A5_A5A5;
    let j = Journal::new();
    let writer = j.writer(65_536);
    let alpha = b"alpha";
    // append_batch: an output inside the records array (it would zero a length before validation).
    let mut records = [new_record(alpha), new_record(alpha)];
    let inside = (&raw mut records[0].payload_len).cast::<usize>();
    let mut first_seq = WORD;
    let status = unsafe {
        tickertape_writer_append_batch(writer, records.as_ptr(), 2, inside, &raw mut first_seq)
    };
    assert_eq!(status, INVALID);
    assert!(last_message().contains("overlap"), "{}", last_message());
    assert_eq!(records[0].payload_len, 5, "the input is untouched");
    assert_eq!(first_seq, WORD);
    // An output inside a payload.
    let mut words = [WORD; 4];
    records[1].payload = words.as_ptr().cast();
    records[1].payload_len = size_of_val(&words);
    let mut appended = WORD as usize;
    let status = unsafe {
        tickertape_writer_append_batch(
            writer,
            records.as_ptr(),
            2,
            &mut appended,
            &raw mut words[1],
        )
    };
    assert_eq!(status, INVALID);
    assert_eq!((appended, words), (WORD as usize, [WORD; 4]));
    // The two outputs overlapping each other.
    records[1] = new_record(alpha);
    let mut both = WORD;
    let status = unsafe {
        tickertape_writer_append_batch(
            writer,
            records.as_ptr(),
            2,
            (&raw mut both).cast(),
            &raw mut both,
        )
    };
    assert_eq!(status, INVALID);
    assert_eq!(both, WORD);
    // append: seq_out inside the payload.
    let mut payload_words = [WORD; 2];
    let status = unsafe {
        tickertape_writer_append(
            writer,
            1,
            0,
            0,
            payload_words.as_ptr().cast(),
            16,
            &raw mut payload_words[1],
        )
    };
    assert_eq!(status, INVALID);
    assert_eq!(payload_words, [WORD; 2]);
    assert_eq!(next_seq(writer), 0, "nothing was appended");

    for seq in 0..4 {
        append(writer, seq);
    }
    let reader = j.reader(false, false);
    let sentinel = tickertape_record {
        seq: WORD,
        venue_ts_ns: WORD,
        local_ts_ns: WORD,
        payload: std::ptr::without_provenance(WORD as usize),
        payload_len: WORD as usize,
        schema_id: WORD as u32,
        reserved: WORD as u32,
    };
    let unchanged = |records: &[tickertape_record]| {
        records.iter().all(|r| {
            (
                r.seq,
                r.venue_ts_ns,
                r.local_ts_ns,
                r.payload.addr(),
                r.payload_len,
                r.schema_id,
                r.reserved,
            ) == (
                WORD,
                WORD,
                WORD,
                WORD as usize,
                WORD as usize,
                WORD as u32,
                WORD as u32,
            )
        })
    };
    let mut storage = [sentinel; 4];
    let base = storage.as_mut_ptr();
    // next_copy: record_out inside the copy buffer.
    let status = unsafe {
        tickertape_reader_next_copy(reader, 0, base.cast(), size_of_val(&storage), base.add(1))
    };
    assert_eq!(status, INVALID);
    assert!(unchanged(&storage));
    // read_batch: records inside the buffer (count_out separate), count_out inside the buffer, count_out inside
    // records.
    let mut buf = [WORD; 64];
    let mut count = WORD as usize;
    let status = unsafe {
        tickertape_reader_read_batch(
            reader,
            0,
            buf.as_mut_ptr().cast(),
            2,
            buf.as_mut_ptr().cast(),
            size_of_val(&buf),
            &mut count,
        )
    };
    assert_eq!(status, INVALID);
    assert_eq!(count, WORD as usize, "count_out is not written");
    assert_eq!(buf, [WORD; 64]);
    let status = unsafe {
        tickertape_reader_read_batch(
            reader,
            0,
            base,
            4,
            buf.as_mut_ptr().cast(),
            size_of_val(&buf),
            (&raw mut buf[3]).cast(),
        )
    };
    assert_eq!(status, INVALID);
    assert_eq!(buf, [WORD; 64], "count_out inside buf is not written");
    assert!(unchanged(&storage));
    let status = unsafe {
        tickertape_reader_read_batch(
            reader,
            0,
            base,
            4,
            buf.as_mut_ptr().cast(),
            size_of_val(&buf),
            (&raw mut storage[2].payload_len).cast(),
        )
    };
    assert_eq!(status, INVALID);
    assert!(
        unchanged(&storage),
        "count_out inside records is not written"
    );
    assert_eq!(buf, [WORD; 64]);
    assert_eq!(position(reader), 0, "nothing was read");
    // list_segments: count_out inside the array.
    let filled = tickertape_segment_info {
        base_seq: WORD,
        segment_size: WORD,
        created_unix_ns: WORD,
        format_version: WORD as u32,
        reserved: WORD as u32,
    };
    let mut segments = [filled; 2];
    let status = unsafe {
        tickertape_list_segments(
            j.c_dir.as_ptr(),
            segments.as_mut_ptr(),
            2,
            (&raw mut segments[1].segment_size).cast(),
        )
    };
    assert_eq!(status, INVALID);
    assert!(
        segments.iter().all(|s| (
            s.base_seq,
            s.segment_size,
            s.created_unix_ns,
            s.format_version,
            s.reserved
        ) == (WORD, WORD, WORD, WORD as u32, WORD as u32)),
        "{segments:?}"
    );
    // The other argument errors still set the counts to 0 (documented), once the overlap checks pass.
    let mut count = WORD as usize;
    let status = unsafe {
        tickertape_reader_read_batch(reader, 0, base, 0, buf.as_mut_ptr().cast(), 8, &mut count)
    };
    assert_eq!(status, INVALID);
    assert_eq!(count, 0);
    let mut count = WORD as usize;
    let status = unsafe { tickertape_list_segments(j.c_dir.as_ptr(), null_mut(), 2, &mut count) };
    assert_eq!(status, INVALID);
    assert_eq!(count, 0);
    assert_eq!(unsafe { tickertape_reader_close(reader) }, OK);
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);
}

/// A caller's report struct from a newer header: larger than this library's, with guard bytes after it.
#[repr(C, align(8))]
struct Larger<T> {
    report: T,
    newer_fields: [u8; 32],
}

#[test]
fn report_struct_size_works_in_both_directions() {
    let j = Journal::new();
    let writer = j.writer(65_536);
    for seq in 0..1500 {
        append(writer, seq);
    }
    assert_eq!(unsafe { tickertape_writer_checkpoint(writer) }, OK);
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);

    let verify_size = size_of::<tickertape_verify_report>();
    let mut larger = Larger {
        report: tickertape_verify_report::default(),
        newer_fields: [0xAB; 32],
    };
    larger.report.struct_size = (verify_size + 32) as _;
    assert_eq!(
        unsafe { tickertape_verify(j.c_dir.as_ptr(), &mut larger.report) },
        OK
    );
    assert_eq!(larger.report.records, 1500);
    assert_eq!(
        larger.report.struct_size as usize, verify_size,
        "struct_size says how many bytes the library filled"
    );
    assert!(larger.newer_fields.iter().all(|&b| b == 0xAB));
    // A struct of exactly the minimum (1.0) size.
    let mut minimum = tickertape_verify_report {
        struct_size: verify_size as _,
        ..Default::default()
    };
    assert_eq!(
        unsafe { tickertape_verify(j.c_dir.as_ptr(), &mut minimum) },
        OK
    );
    assert_eq!(minimum.struct_size as usize, verify_size);
    assert_eq!(minimum.records, 1500);
    minimum.struct_size = (verify_size - 1) as _;
    assert_eq!(
        unsafe { tickertape_verify(j.c_dir.as_ptr(), &mut minimum) },
        INVALID
    );

    let retention = tickertape_retention_new();
    assert_eq!(
        unsafe { tickertape_retention_set_max_segments(retention, 1) },
        OK
    );
    let retention_size = size_of::<tickertape_retention_report>();
    let mut larger = Larger {
        report: tickertape_retention_report::default(),
        newer_fields: [0xCD; 32],
    };
    larger.report.struct_size = (retention_size + 32) as _;
    assert_eq!(
        unsafe { tickertape_retention_apply(j.c_dir.as_ptr(), retention, 1, &mut larger.report) },
        OK
    );
    assert!(larger.report.removed_segments >= 1);
    assert_eq!(larger.report.struct_size as usize, retention_size);
    assert!(larger.newer_fields.iter().all(|&b| b == 0xCD));
    let mut minimum = tickertape_retention_report {
        struct_size: retention_size as _,
        ..Default::default()
    };
    assert_eq!(
        unsafe { tickertape_retention_apply(j.c_dir.as_ptr(), retention, 1, &mut minimum) },
        OK
    );
    assert_eq!(minimum.struct_size as usize, retention_size);
    unsafe { tickertape_retention_free(retention) };
}

#[test]
fn pure_unknown_status_is_far_from_the_assigned_codes() {
    assert_eq!(code(S::TICKERTAPE_E_UNKNOWN), -255);
    assert_eq!(name(-255), "TICKERTAPE_E_UNKNOWN");
    assert_eq!(name(-18), "TICKERTAPE_E_BEACON_IN_USE");
    assert_eq!(name(-19), "TICKERTAPE_UNKNOWN_STATUS");
}

#[test]
fn zero_length_payloads_everywhere() {
    let j = Journal::new();
    let writer = j.writer(65_536);
    // append, append_batch and append_with with empty payloads (seq 0, 1, 2).
    assert_eq!(
        unsafe { tickertape_writer_append(writer, 1, 0, 0, null(), 0, null_mut()) },
        OK
    );
    let mut empty = new_record(b"");
    empty.payload = null();
    let mut appended = 0;
    assert_eq!(
        unsafe { tickertape_writer_append_batch(writer, &empty, 1, &mut appended, null_mut()) },
        OK
    );
    assert_eq!(appended, 1);
    let mut state = EmptyFill {
        calls: 0,
        buf_was_null: false,
    };
    let status = unsafe {
        tickertape_writer_append_with(
            writer,
            1,
            0,
            0,
            0,
            Some(empty_fill),
            (&raw mut state).cast(),
            null_mut(),
        )
    };
    assert_eq!(status, OK, "{}", last_message());
    assert_eq!(state.calls, 1, "the callback runs for an empty payload too");
    assert!(state.buf_was_null, "an empty buffer is passed as NULL");
    append_raw(writer, b"not empty"); // seq 3
    assert_eq!(next_seq(writer), 4);

    for mapped in [false, true] {
        let reader = j.reader(false, mapped);
        let mut record = empty_record();
        for seq in 0..3 {
            assert_eq!(
                unsafe { tickertape_reader_next(reader, 0, &mut record) },
                OK
            );
            assert_eq!((record.seq, record.payload_len), (seq, 0));
            assert!(record.payload.is_null(), "zero-length payloads are NULL");
        }
        assert_eq!(unsafe { tickertape_reader_seek(reader, 0) }, OK);
        // next_copy with a NULL buffer of length 0.
        assert_eq!(
            unsafe { tickertape_reader_next_copy(reader, 0, null_mut(), 0, &mut record) },
            OK
        );
        assert!(record.seq == 0 && record.payload.is_null());
        // read_batch with a NULL buffer of length 0 takes the empty records and holds back the next one.
        let mut records = [empty_record(); 8];
        let mut count = 0;
        let status = unsafe {
            tickertape_reader_read_batch(
                reader,
                0,
                records.as_mut_ptr(),
                records.len(),
                null_mut(),
                0,
                &mut count,
            )
        };
        assert_eq!((status, count), (OK, 2));
        assert!(records[..2].iter().all(|r| r.payload.is_null()));
        assert_eq!((records[0].seq, records[1].seq), (1, 2));
        // With a non-NULL buffer, an empty record still gets a NULL payload.
        let mut buf = [0u8; 4];
        assert_eq!(unsafe { tickertape_reader_seek(reader, 2) }, OK);
        let status = unsafe {
            tickertape_reader_read_batch(
                reader,
                0,
                records.as_mut_ptr(),
                records.len(),
                buf.as_mut_ptr(),
                buf.len(),
                &mut count,
            )
        };
        assert_eq!((status, count), (OK, 1));
        assert!(records[0].payload.is_null());
        assert_eq!(unsafe { tickertape_reader_close(reader) }, OK);
    }
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);
}

struct EmptyFill {
    calls: usize,
    buf_was_null: bool,
}

unsafe extern "C-unwind" fn empty_fill(user_data: *mut c_void, buf: *mut u8, len: usize) -> i32 {
    let state = unsafe { &mut *user_data.cast::<EmptyFill>() };
    state.calls += 1;
    state.buf_was_null = buf.is_null() && len == 0;
    0
}

struct CloseFill {
    writer: *mut tickertape_writer,
    close_status: i32,
}

unsafe extern "C-unwind" fn closing_fill(user_data: *mut c_void, buf: *mut u8, len: usize) -> i32 {
    let state = unsafe { &mut *user_data.cast::<CloseFill>() };
    state.close_status = unsafe { tickertape_writer_close(state.writer) };
    unsafe { std::ptr::write_bytes(buf, b'x', len) };
    0
}

#[test]
fn closing_a_writer_from_its_fill_callback_is_busy() {
    let j = Journal::new();
    let writer = j.writer(65_536);
    let mut state = CloseFill {
        writer,
        close_status: OK,
    };
    let mut seq = u64::MAX;
    let status = unsafe {
        tickertape_writer_append_with(
            writer,
            1,
            0,
            0,
            3,
            Some(closing_fill),
            (&raw mut state).cast(),
            &mut seq,
        )
    };
    assert_eq!(status, OK, "{}", last_message());
    assert_eq!(state.close_status, code(S::TICKERTAPE_E_BUSY));
    assert_eq!(seq, 0);
    // The handle is still alive.
    append_raw(writer, b"after");
    assert_eq!(next_seq(writer), 2);
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);
}

unsafe extern "C-unwind" fn panicking_fill(_: *mut c_void, _: *mut u8, _: usize) -> i32 {
    panic!("a bug inside the callback");
}

#[test]
fn a_panic_breaks_the_handle_through_an_exported_function() {
    let j = Journal::new();
    let writer = j.writer(65_536);
    append_raw(writer, b"before");
    let status = unsafe {
        tickertape_writer_append_with(
            writer,
            1,
            0,
            0,
            4,
            Some(panicking_fill),
            null_mut(),
            null_mut(),
        )
    };
    assert_eq!(status, code(S::TICKERTAPE_E_PANIC));
    assert!(
        last_message().contains("a bug inside the callback"),
        "{}",
        last_message()
    );
    let mut seq = 0;
    assert_eq!(
        unsafe { tickertape_writer_next_seq(writer, &mut seq) },
        code(S::TICKERTAPE_E_PANIC)
    );
    assert_eq!(
        unsafe { tickertape_writer_append(writer, 1, 0, 0, null(), 0, null_mut()) },
        code(S::TICKERTAPE_E_PANIC)
    );
    assert!(last_message().contains("broken"), "{}", last_message());
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);
    // Nothing from the abandoned append is visible.
    let reader = j.reader(false, false);
    let mut record = empty_record();
    assert_eq!(
        unsafe { tickertape_reader_next(reader, 0, &mut record) },
        OK
    );
    assert_eq!(
        unsafe { tickertape_reader_next(reader, 0, &mut record) },
        NONE
    );
    assert_eq!(unsafe { tickertape_reader_close(reader) }, OK);
}

#[test]
fn discarded_is_reported_and_the_reader_recovers() {
    let j = Journal::new();
    let writer = j.writer(65_536);
    for seq in 0..1500 {
        append(writer, seq);
    }
    assert_eq!(unsafe { tickertape_writer_checkpoint(writer) }, OK);
    let reader = j.reader(false, false);
    let retention = tickertape_retention_new();
    unsafe {
        assert_eq!(tickertape_retention_set_max_segments(retention, 1), OK);
        assert_eq!(tickertape_retention_set_delete(retention), OK);
        assert_eq!(
            tickertape_retention_apply(j.c_dir.as_ptr(), retention, 0, null_mut()),
            OK
        );
        tickertape_retention_free(retention);
    }
    // The reader still has the first segment open and reads it to its end, then notices the removal.
    let mut record = empty_record();
    let mut read = 0u64;
    let status = loop {
        let status = unsafe { tickertape_reader_next(reader, 0, &mut record) };
        if status != OK {
            break status;
        }
        check_record(&record, read);
        read += 1;
    };
    assert_eq!(
        status,
        code(S::TICKERTAPE_E_DISCARDED),
        "{}",
        last_message()
    );
    assert_eq!(tickertape_last_error_code(), status);
    assert!(read > 0);
    // Seeking to the same position fails: the records there were removed. Continue at the oldest one left.
    assert_eq!(
        unsafe { tickertape_reader_seek(reader, position(reader)) },
        code(S::TICKERTAPE_E_SEQ_UNAVAILABLE)
    );
    let mut segment = tickertape_segment_info::default();
    let mut count = 0;
    assert_eq!(
        unsafe { tickertape_list_segments(j.c_dir.as_ptr(), &mut segment, 1, &mut count) },
        OK
    );
    assert_eq!(count, 1);
    assert_eq!(
        unsafe { tickertape_reader_seek(reader, segment.base_seq) },
        OK
    );
    assert_eq!(
        unsafe { tickertape_reader_next(reader, 0, &mut record) },
        OK
    );
    check_record(&record, segment.base_seq);
    assert_eq!(unsafe { tickertape_reader_close(reader) }, OK);
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);
}

#[test]
fn retention_apply_without_a_report_and_with_a_move() {
    let j = Journal::new();
    let writer = j.writer(65_536);
    for seq in 0..1500 {
        append(writer, seq);
    }
    assert_eq!(unsafe { tickertape_writer_checkpoint(writer) }, OK);
    let mut before = 0;
    assert_eq!(
        unsafe { tickertape_list_segments(j.c_dir.as_ptr(), null_mut(), 0, &mut before) },
        OK
    );
    let archive = j.dir.with_file_name("archive");
    std::fs::create_dir(&archive).unwrap();
    let c_archive = cstr(&archive);
    let retention = tickertape_retention_new();
    unsafe {
        assert_eq!(tickertape_retention_set_max_segments(retention, 1), OK);
        assert_eq!(
            tickertape_retention_set_move_to(retention, c_archive.as_ptr()),
            OK
        );
        // No report.
        assert_eq!(
            tickertape_retention_apply(j.c_dir.as_ptr(), retention, 0, null_mut()),
            OK
        );
    }
    let mut after = 0;
    assert_eq!(
        unsafe { tickertape_list_segments(j.c_dir.as_ptr(), null_mut(), 0, &mut after) },
        OK
    );
    assert_eq!(after, 1);
    let moved = std::fs::read_dir(&archive).unwrap().count();
    assert_eq!(
        moved,
        before - after,
        "the segments were moved, not deleted"
    );
    assert!(archive.join(format!("{:020}.tape", 0)).exists());
    // Nothing is left to move.
    let mut report = tickertape_retention_report {
        struct_size: size_of::<tickertape_retention_report>() as _,
        ..Default::default()
    };
    assert_eq!(
        unsafe { tickertape_retention_apply(j.c_dir.as_ptr(), retention, 0, &mut report) },
        OK
    );
    assert_eq!(report.removed_segments, 0);
    unsafe { tickertape_retention_free(retention) };
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);
}

#[test]
fn mapped_view_stays_valid_across_position_and_a_segment_roll() {
    let j = Journal::new();
    let writer = j.writer(65_536);
    for seq in 0..3 {
        append(writer, seq);
    }
    let reader = j.reader(false, true);
    let mut record = empty_record();
    for _ in 0..2 {
        assert_eq!(
            unsafe { tickertape_reader_next(reader, 0, &mut record) },
            OK
        );
    }
    check_record(&record, 1);
    // The writer rolls to new segments while the view is held; position does not invalidate it.
    let mut segments = 0;
    for seq in 3..1000 {
        append(writer, seq);
    }
    assert_eq!(
        unsafe { tickertape_list_segments(j.c_dir.as_ptr(), null_mut(), 0, &mut segments) },
        OK
    );
    assert!(segments > 1);
    assert_eq!(position(reader), 2);
    check_record(&record, 1);
    assert_eq!(position(reader), 2);
    check_record(&record, 1);
    assert_eq!(unsafe { tickertape_reader_close(reader) }, OK);
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);
}

#[test]
fn pure_append_batch_counts_each_commit_even_if_a_later_append_panics() {
    let bytes = *b"abc";
    let records = [new_record(&bytes), new_record(&bytes), new_record(&bytes)];
    let mut appended = 0usize;
    let mut calls = 0;
    let status = guard(|| {
        unsafe {
            crate::writer::append_records(&records, &mut appended, |_| {
                calls += 1;
                assert!(calls < 3, "the third append fails inside the core");
                Ok(calls)
            })
        }?;
        Ok(tickertape_status_code::TICKERTAPE_OK)
    });
    assert_eq!(status, code(S::TICKERTAPE_E_PANIC));
    assert_eq!(appended, 2, "the two committed records are counted");
}

#[test]
fn pure_struct_size_fills_only_what_a_smaller_caller_has() {
    // A later version of an extensible struct (40 bytes) whose minimum size, its size in the first stable release,
    // is 24 bytes: a caller compiled against that release passes `struct_size` = 24.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Grown {
        struct_size: u64,
        a: u64,
        b: u64,
        added_later: u64,
        added_even_later: u64,
    }
    const MIN: usize = 24;
    const GUARD: u64 = 0xEEEE_EEEE_EEEE_EEEE;
    let value = Grown {
        struct_size: 0,
        a: 1,
        b: 2,
        added_later: 3,
        added_even_later: 4,
    };
    // The caller's struct at the minimum size, followed by guard words in the same (aligned) allocation.
    let mut buffer = [GUARD; 8];
    buffer[0] = MIN as u64;
    let ptr = buffer.as_mut_ptr().cast::<Grown>();
    let filled = unsafe { crate::args::struct_size(ptr, MIN, "report") }.unwrap();
    assert_eq!(filled, MIN, "only the caller's bytes are filled");
    unsafe { crate::args::write_struct(ptr, value, filled) };
    assert_eq!(
        buffer[..3],
        [MIN as u64, 1, 2],
        "struct_size comes back as the minimum"
    );
    assert!(buffer[3..].iter().all(|&word| word == GUARD), "{buffer:x?}");
    // An allocation of exactly the minimum size: under Miri, any write past it is reported as undefined behaviour.
    let mut tight = Box::new([MIN as u64, GUARD, GUARD]);
    let ptr = tight.as_mut_ptr().cast::<Grown>();
    let filled = unsafe { crate::args::struct_size(ptr, MIN, "report") }.unwrap();
    unsafe { crate::args::write_struct(ptr, value, filled) };
    assert_eq!(*tight, [MIN as u64, 1, 2]);
}

#[test]
fn pure_with_ref_keeps_pointers_from_earlier_calls_valid() {
    // State stored inline, as a reader's buffer could be one day: a pointer handed out by one call must survive a
    // read-only call (as a record view survives `tickertape_reader_position`). Under Miri, creating a `&mut` to the
    // state here (`Handle::with`) would invalidate `view`; `Handle::with_ref` does not.
    let handle = Handle::new([7u8; 16]);
    let view = handle.with(|state| Ok(state.as_ptr())).unwrap();
    let first = handle.with_ref(|state| Ok(state[0])).unwrap();
    assert_eq!(first, 7);
    assert_eq!(unsafe { *view.add(3) }, 7);
    // A read-only call from inside another call on the same handle is refused, like any re-entrant call.
    let inner = handle.with(|_| Ok(handle.with_ref(|_| Ok(())).unwrap_err().code));
    assert_eq!(inner.unwrap(), S::TICKERTAPE_E_BUSY);
}

#[cfg(feature = "fault-injection")]
#[test]
fn failed_cleanup_after_an_aborted_fill_is_reported() {
    failed_cleanup_is_reported(5 /* EIO */);
}

#[cfg(feature = "fault-injection")]
#[test]
fn failed_cleanup_on_a_full_volume_is_io_not_storage_full() {
    // `fsync` can fail with ENOSPC on a full APFS volume (docs/crash-contract.md). `TICKERTAPE_E_STORAGE_FULL` means
    // "retry once space is available", which never helps here: the writer refuses appends until it is reopened.
    failed_cleanup_is_reported(28 /* ENOSPC */);
}

#[cfg(feature = "fault-injection")]
fn failed_cleanup_is_reported(errno: i32) {
    use tickertape::fault::{self, Point};
    let j = Journal::new();
    let writer = j.writer(65_536);
    append_raw(writer, b"first");
    let mut state = FillState {
        writer,
        abort: true,
        reentrant_status: OK,
        calls: 0,
    };
    fault::fail_next(Point::AbandonedWriteSync, errno);
    let status = unsafe {
        tickertape_writer_append_with(
            writer,
            1,
            0,
            0,
            37,
            Some(fill),
            (&raw mut state).cast(),
            null_mut(),
        )
    };
    assert_eq!(status, code(S::TICKERTAPE_E_IO), "not a clean abort");
    assert_eq!(tickertape_last_error_os_code(), errno);
    let message = last_message();
    assert!(
        message.contains("fill callback") && message.contains("zeroing"),
        "{message}"
    );
    // Later appends are refused with the same OS error, and say why.
    for _ in 0..3 {
        let status = unsafe { tickertape_writer_append(writer, 1, 0, 0, null(), 0, null_mut()) };
        assert_eq!(status, code(S::TICKERTAPE_E_IO));
        assert_eq!(tickertape_last_error_code(), code(S::TICKERTAPE_E_IO));
        assert_eq!(tickertape_last_error_os_code(), errno);
        assert!(
            last_message().contains("abandoned write"),
            "{}",
            last_message()
        );
    }
    let mut appended = 0;
    let empty = new_record(b"");
    assert_eq!(
        unsafe { tickertape_writer_append_batch(writer, &empty, 1, &mut appended, null_mut()) },
        code(S::TICKERTAPE_E_IO)
    );
    assert_eq!(appended, 0);
    assert_eq!(tickertape_last_error_os_code(), errno);
    // The checkpoint blames the clean-up, not an earlier checkpoint.
    assert_eq!(
        unsafe { tickertape_writer_checkpoint(writer) },
        code(S::TICKERTAPE_E_DURABILITY_LOST)
    );
    let message = last_message();
    assert!(
        message.contains("aborted fill") && !message.contains("earlier checkpoint"),
        "{message}"
    );
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);
    // Reopening recovers.
    let writer = j.writer(65_536);
    assert_eq!(append_raw(writer, b"second"), 1);
    assert_eq!(unsafe { tickertape_writer_checkpoint(writer) }, OK);
    assert_eq!(unsafe { tickertape_writer_close(writer) }, OK);
}
