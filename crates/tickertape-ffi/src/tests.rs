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
        struct_size: size_of::<tickertape_verify_report>(),
        ..Default::default()
    };
    assert_eq!(
        unsafe { tickertape_verify(j.c_dir.as_ptr(), &mut report) },
        OK
    );
    assert_eq!(report.records, 15);
    assert_eq!((report.first_seq, report.last_seq), (0, 14));
    assert_eq!(report.problems, 0);
    assert_eq!(report.struct_size, size_of::<tickertape_verify_report>());
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
        struct_size: size_of::<tickertape_verify_report>(),
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

unsafe extern "C" fn fill(user_data: *mut c_void, buf: *mut u8, len: usize) -> i32 {
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
        struct_size: size_of::<tickertape_retention_report>(),
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
