//! Recovery tests: simulated crash states and corruption, checked against `docs/format.md` → Recovery and the
//! crash contract (C2, C4, C8, C11).

use std::os::unix::fs::FileExt;
use std::panic::{self, AssertUnwindSafe};

use tickertape::format::{self, RecordHeader};
use tickertape::{
    CorruptReason, Error, NewRecord, Reader, ReaderConfig, RecoveryAction, RecoveryPolicy, Writer,
    WriterConfig,
};
use tickertape_tests::{
    SMALL_SEGMENT, append_n, check_record, create_raw_segment, end_of_records, open_raw, read_all,
    segments, small_config,
};

fn assert_sequence(records: &[tickertape::OwnedRecord], n: u64) {
    assert_eq!(records.len() as u64, n, "record count");
    for (i, r) in records.iter().enumerate() {
        assert_eq!(r.seq, i as u64);
        check_record(r);
    }
}

#[test]
fn roundtrip_across_rolls_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    assert_eq!(w.recovery_report().actions, vec![RecoveryAction::Created]);
    append_n(&mut w, 1500);
    assert!(
        segments(dir.path()).len() > 3,
        "small segments must have rolled"
    );
    drop(w);

    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    assert!(
        w.recovery_report().actions.is_empty(),
        "{:?}",
        w.recovery_report()
    );
    assert_eq!(w.next_seq(), 1500);
    append_n(&mut w, 10);
    assert_sequence(&read_all(dir.path()), 1510);

    // The zero-copy reader sees the same.
    // SAFETY: only the tickertape writer in this test modifies the journal.
    let mut r = unsafe { Reader::open_mapped(dir.path(), ReaderConfig::default()) }.unwrap();
    let mut n = 0;
    while let Some(rec) = r.try_next().unwrap() {
        check_record(&rec.to_owned());
        assert_eq!(rec.seq, n);
        n += 1;
    }
    assert_eq!(n, 1510);
}

#[test]
fn live_tail_and_seek_in_process() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    let mut r = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    assert!(r.try_next().unwrap().is_none());
    for round in 0..50u64 {
        append_n(&mut w, 20);
        for i in 0..20 {
            let rec = r
                .next_owned()
                .unwrap()
                .expect("record visible after append");
            assert_eq!(rec.seq, round * 20 + i);
            check_record(&rec);
        }
        assert!(r.try_next().unwrap().is_none());
    }
    r.seek(777).unwrap();
    assert_eq!(r.position(), 777);
    assert_eq!(r.next_owned().unwrap().unwrap().seq, 777);
    // Seeking ahead of the writer waits for the record.
    r.seek(1005).unwrap();
    assert!(r.try_next().unwrap().is_none());
    append_n(&mut w, 10);
    assert_eq!(r.next_owned().unwrap().unwrap().seq, 1005);
    assert!(matches!(r.seek(0), Ok(())));
}

#[test]
fn record_size_limits() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    let max = format::max_payload_len(SMALL_SEGMENT) as usize;
    let big = vec![7u8; max];
    let rec = NewRecord {
        schema_id: 1,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload: &big,
    };
    // Exactly the maximum fits in an empty segment.
    assert_eq!(w.append(&rec).unwrap(), 0);
    // One more byte never fits.
    let too_big = vec![7u8; max + 1];
    assert!(matches!(
        w.append(&NewRecord {
            payload: &too_big,
            ..rec
        }),
        Err(Error::RecordTooLarge { .. })
    ));
    // The maximum again forces a roll.
    assert_eq!(w.append(&rec).unwrap(), 1);
    assert_eq!(segments(dir.path()).len(), 2);
    let all = read_all(dir.path());
    assert_eq!(all.len(), 2);
    assert!(all.iter().all(|r| r.payload == big));
}

#[test]
fn torn_final_record() {
    // C2: a body written but never committed (commit word 0), as a writer killed mid-append leaves it.
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 25);
    drop(w);
    let (_, path) = segments(dir.path()).pop().unwrap();
    let (end, next) = end_of_records(&path);
    assert_eq!(next, 25);
    open_raw(&path).write_all_at(&[0xAB; 100], end + 4).unwrap();

    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    assert!(
        matches!(w.recovery_report().actions[..], [RecoveryAction::ZeroedUncommitted { offset, len: 100, .. }] if offset == end + 4),
        "{:?}",
        w.recovery_report()
    );
    assert_eq!(w.next_seq(), 25);
    append_n(&mut w, 5);
    assert_sequence(&read_all(dir.path()), 30);
}

#[test]
fn panic_in_fill_then_shorter_append() {
    // C2 / format.md → Abandoned writes: a fill closure writes bytes that form a valid frame for the *next*
    // sequence number exactly where a shorter record's successor would start, then panics.
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 3);
    let mut r = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    for _ in 0..3 {
        r.next_owned().unwrap().unwrap();
    }

    let short_payload = vec![1u8; 8]; // frame 48 bytes, so the next slot starts 48 bytes later
    let next_slot_in_payload = 48 - format::RECORD_HEADER_LEN;
    let fake_len = format::frame_len(4).unwrap();
    let mut fake_body = vec![0u8; fake_len as usize - 4];
    format::encode_record_body(
        &RecordHeader {
            seq: 4,
            schema_id: 99,
            venue_ts_ns: 0,
            local_ts_ns: 0,
        },
        b"evil",
        &mut fake_body,
    );
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let rec = NewRecord {
            schema_id: 0,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: &[],
        };
        let _ = w.append_with(&rec, 200, |buf| {
            buf[next_slot_in_payload..next_slot_in_payload + 4]
                .copy_from_slice(&fake_len.to_le_bytes());
            buf[next_slot_in_payload + 4..next_slot_in_payload + 4 + fake_body.len()]
                .copy_from_slice(&fake_body);
            panic!("fill failed");
        });
    }));
    assert!(result.is_err());
    assert!(
        r.try_next().unwrap().is_none(),
        "nothing committed by the panicking append"
    );

    let seq = w
        .append(&NewRecord {
            schema_id: 5,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: &short_payload,
        })
        .unwrap();
    assert_eq!(seq, 3);
    assert_eq!(r.next_owned().unwrap().unwrap().payload, short_payload);
    assert!(
        r.try_next().unwrap().is_none(),
        "the stale frame must not be readable"
    );
    drop(w);
    assert_eq!(read_all(dir.path()).len(), 4);
}

#[test]
fn malformed_checksum_is_quarantined_and_repaired() {
    // C11: a committed record with a bad CRC (power loss or storage corruption).
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 10);
    drop(w);
    let (_, path) = segments(dir.path()).pop().unwrap();
    let image = std::fs::read(&path).unwrap();
    // Find record 7 and flip a payload byte.
    let mut off = format::SEGMENT_HEADER_LEN as u64;
    for _ in 0..7 {
        let len = u32::from_le_bytes(image[off as usize..off as usize + 4].try_into().unwrap());
        off += format::align8(u64::from(len));
    }
    let flip_at = off + format::RECORD_HEADER_LEN as u64;
    let byte = image[flip_at as usize] ^ 0xFF;
    open_raw(&path).write_all_at(&[byte], flip_at).unwrap();
    let corrupted = std::fs::read(&path).unwrap();

    // Readers stop at the corrupt record with an error and don't advance.
    let mut r = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    for _ in 0..7 {
        r.next_owned().unwrap().unwrap();
    }
    assert!(matches!(
        r.try_next(),
        Err(Error::Corrupt {
            reason: CorruptReason::BadChecksum,
            ..
        })
    ));
    assert_eq!(r.position(), 7);

    // Policy Fail refuses and changes nothing.
    let cfg = small_config().recovery(RecoveryPolicy::Fail);
    assert!(
        matches!(Writer::open(dir.path(), cfg), Err(Error::Corrupt { offset, .. }) if offset == off)
    );
    assert_eq!(std::fs::read(&path).unwrap()[flip_at as usize], byte);

    // Policy Quarantine preserves the tail and replaces the segment.
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    let actions = &w.recovery_report().actions;
    let Some(RecoveryAction::RepairedTail {
        offset,
        quarantine_file,
        reason,
        ..
    }) = actions.first()
    else {
        panic!("expected RepairedTail, got {actions:?}");
    };
    assert_eq!(*offset, off);
    assert_eq!(*reason, CorruptReason::BadChecksum);
    let preserved = std::fs::read(quarantine_file).unwrap();
    assert!(!preserved.is_empty());
    assert_eq!(
        preserved[..],
        corrupted[off as usize..off as usize + preserved.len()]
    );
    assert_eq!(w.next_seq(), 7);

    // The old reader notices the replacement instead of reading the new history at its old offset.
    assert!(matches!(r.try_next(), Err(Error::Discarded { seq: 7 })));
    append_n(&mut w, 3);
    r.seek(7).unwrap();
    let rec = r.next_owned().unwrap().unwrap();
    assert_eq!(rec.seq, 7);
    check_record(&rec);
    assert_sequence(&read_all(dir.path()), 10);
}

#[test]
fn malformed_length_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 4);
    drop(w);
    let (_, path) = segments(dir.path()).pop().unwrap();
    let (end, _) = end_of_records(&path);
    // A commit word pointing past the end of the record area.
    open_raw(&path)
        .write_all_at(&u32::MAX.wrapping_sub(1).to_le_bytes(), end)
        .unwrap();
    let mut r = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    for _ in 0..4 {
        r.next_owned().unwrap().unwrap();
    }
    assert!(matches!(
        r.try_next(),
        Err(Error::Corrupt {
            reason: CorruptReason::BadLength(_),
            ..
        })
    ));
    let w = Writer::open(dir.path(), small_config()).unwrap();
    assert!(matches!(
        w.recovery_report().actions[..],
        [RecoveryAction::RepairedTail { .. }]
    ));
    assert_eq!(w.next_seq(), 4);
}

#[test]
fn crash_between_link_and_seal() {
    // C4: the next segment was published but the old one never sealed.
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 12);
    drop(w);
    create_raw_segment(dir.path(), 12, SMALL_SEGMENT);
    let mut r = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    for _ in 0..12 {
        r.next_owned().unwrap().unwrap();
    }
    assert!(
        r.try_next().unwrap().is_none(),
        "reader waits at the unsealed end"
    );

    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    assert!(
        w.recovery_report()
            .actions
            .iter()
            .any(|a| matches!(a, RecoveryAction::Sealed { .. })),
        "{:?}",
        w.recovery_report()
    );
    append_n(&mut w, 3);
    // The waiting reader follows the new seal.
    for i in 0..3 {
        assert_eq!(r.next_owned().unwrap().unwrap().seq, 12 + i);
    }
    assert_sequence(&read_all(dir.path()), 15);
}

#[test]
fn checkpoint_marker_bounds_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 600);
    w.checkpoint().unwrap();
    append_n(&mut w, 5);
    drop(w);
    let w = Writer::open(dir.path(), small_config()).unwrap();
    let report = w.recovery_report();
    assert!(report.had_checkpoint);
    assert_eq!(report.scanned_from_seq, 600);
    assert_eq!(report.next_seq, 605);
}

#[test]
fn dangling_checkpoint_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 600);
    w.checkpoint().unwrap();
    drop(w);
    let (_, newest) = segments(dir.path()).pop().unwrap();
    std::fs::remove_file(&newest).unwrap();
    for policy in [RecoveryPolicy::Quarantine, RecoveryPolicy::Fail] {
        match Writer::open(dir.path(), small_config().recovery(policy)) {
            Err(Error::Corrupt {
                reason: CorruptReason::DanglingCheckpoint(_),
                ..
            }) => {}
            other => panic!("expected DanglingCheckpoint, got {other:?}"),
        }
    }
}

#[test]
fn disconnected_segment_is_quarantined() {
    // C8: a segment that isn't on the valid chain (e.g. survived a power loss that lost its predecessor's tail).
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 8);
    drop(w);
    let stray = create_raw_segment(dir.path(), 500, SMALL_SEGMENT);
    assert!(Writer::open(dir.path(), small_config().recovery(RecoveryPolicy::Fail)).is_err());
    let w = Writer::open(dir.path(), small_config()).unwrap();
    assert!(
        matches!(&w.recovery_report().actions[..], [RecoveryAction::QuarantinedSegment { segment, .. }] if *segment == stray),
        "{:?}",
        w.recovery_report()
    );
    assert!(!stray.exists());
    assert_eq!(w.next_seq(), 8);
}

#[test]
fn missing_middle_segment_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 1500);
    drop(w);
    let segs = segments(dir.path());
    assert!(segs.len() >= 3);
    std::fs::remove_file(&segs[1].1).unwrap();
    // No checkpoint: recovery scans from the oldest segment and finds a seal pointing to a missing segment while
    // later segments exist.
    match Writer::open(dir.path(), small_config()) {
        Err(Error::Corrupt {
            reason: CorruptReason::BrokenChain(_),
            ..
        }) => {}
        other => panic!("expected BrokenChain, got {other:?}"),
    }
}

#[test]
fn invalid_config_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        Writer::open(dir.path(), WriterConfig::default().segment_size(1000)),
        Err(Error::InvalidConfig(_))
    ));
}

#[test]
fn verify_reports_intact_and_corrupt_journals() {
    use tickertape::inspect::{self, SegmentEnd};
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 1200);
    let report = inspect::verify(dir.path()).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    assert_eq!(report.records, 1200);
    assert_eq!((report.first_seq, report.last_seq), (Some(0), Some(1199)));
    assert!(report.segments.len() > 2);
    let (last, rest) = report.segments.split_last().unwrap();
    assert!(rest.iter().all(|s| s.end == SegmentEnd::Sealed));
    assert_eq!(last.end, SegmentEnd::Open);
    assert_eq!(
        inspect::list_segments(dir.path()).unwrap().len(),
        report.segments.len()
    );
    drop(w);

    // Corrupt a record in the middle segment.
    let segs = segments(dir.path());
    let (_, middle) = &segs[1];
    let flip = format::SEGMENT_HEADER_LEN as u64 + 20; // inside the first record's header fields
    let f = open_raw(middle);
    let mut b = [0u8];
    f.read_exact_at(&mut b, flip).unwrap();
    f.write_all_at(&[b[0] ^ 1], flip).unwrap();
    let report = inspect::verify(dir.path()).unwrap();
    assert!(
        report
            .problems
            .iter()
            .any(|p| p.path == *middle && p.offset == 64),
        "{:?}",
        report.problems
    );
}

#[test]
fn self_referential_seal_is_rejected() {
    // Review finding: a seal at the first slot names its own segment; following it used to loop forever.
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 3);
    drop(w);
    let path = create_raw_segment(dir.path(), 3, SMALL_SEGMENT);
    // Seal segment 0 properly so readers move into segment 3, then put a seal for 3 at segment 3's first slot.
    let (_, first) = segments(dir.path()).into_iter().next().unwrap();
    let (end, _) = end_of_records(&first);
    let raw = open_raw(&first);
    raw.write_all_at(&format::seal_crc(3).to_le_bytes(), end + 4)
        .unwrap();
    raw.write_all_at(&format::SEAL_MARKER.to_le_bytes(), end)
        .unwrap();
    let raw = open_raw(&path);
    raw.write_all_at(&format::seal_crc(3).to_le_bytes(), 68)
        .unwrap();
    raw.write_all_at(&format::SEAL_MARKER.to_le_bytes(), 64)
        .unwrap();

    let mut r = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    for _ in 0..3 {
        r.next_owned().unwrap().unwrap();
    }
    assert!(matches!(
        r.try_next(),
        Err(Error::Corrupt {
            reason: CorruptReason::BadSeal,
            offset: 64,
            ..
        })
    ));
    let w = Writer::open(dir.path(), small_config()).unwrap();
    assert_eq!(w.next_seq(), 3);
    assert!(
        w.recovery_report()
            .actions
            .iter()
            .any(|a| matches!(a, RecoveryAction::RepairedTail { .. }))
    );
}

#[test]
fn exhausted_sequence_number_is_rejected_by_readers() {
    // Review finding: a checksummed record with seq u64::MAX overflowed the reader's counter.
    let dir = tempfile::tempdir().unwrap();
    let path = create_raw_segment(dir.path(), u64::MAX, SMALL_SEGMENT);
    let len = format::frame_len(0).unwrap();
    let mut body = vec![0u8; len as usize - 4];
    format::encode_record_body(
        &RecordHeader {
            seq: u64::MAX,
            schema_id: 0,
            venue_ts_ns: 0,
            local_ts_ns: 0,
        },
        &[],
        &mut body,
    );
    let raw = open_raw(&path);
    raw.write_all_at(&body, 68).unwrap();
    raw.write_all_at(&len.to_le_bytes(), 64).unwrap();
    let mut r = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    assert!(matches!(
        r.try_next(),
        Err(Error::Corrupt {
            reason: CorruptReason::SequenceExhausted,
            ..
        })
    ));
}
