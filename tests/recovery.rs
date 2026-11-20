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

/// How a test gives up on a record inside its fill closure.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Abandon {
    /// `append_with`'s closure panics.
    Panic,
    /// `try_append_with`'s closure returns `Err`.
    Decline,
    /// `try_append_with`'s closure panics.
    PanicInTry,
}

#[test]
fn panic_in_fill_then_shorter_append() {
    abandoned_fill_then_shorter_append(Abandon::Panic);
}

#[test]
fn declined_fill_then_shorter_append() {
    abandoned_fill_then_shorter_append(Abandon::Decline);
}

#[test]
fn panic_in_fallible_fill_then_shorter_append() {
    abandoned_fill_then_shorter_append(Abandon::PanicInTry);
}

fn abandoned_fill_then_shorter_append(how: Abandon) {
    // C2 / format.md → Abandoned writes: a fill closure writes bytes that form a valid frame for the *next*
    // sequence number exactly where a shorter record's successor would start, then gives up on the record.
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 3);
    let mut r = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    for _ in 0..3 {
        r.next_owned().unwrap().unwrap();
    }
    let (_, path) = segments(dir.path()).pop().unwrap();
    let file_has_planted_frame = || {
        std::fs::read(&path)
            .unwrap()
            .windows(b"evil".len())
            .any(|bytes| bytes == b"evil")
    };

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
    let plant = |buf: &mut [u8]| {
        buf[next_slot_in_payload..next_slot_in_payload + 4]
            .copy_from_slice(&fake_len.to_le_bytes());
        buf[next_slot_in_payload + 4..next_slot_in_payload + 4 + fake_body.len()]
            .copy_from_slice(&fake_body);
        // Positive control for the check below: the planted bytes are in the file (shared mapping).
        assert!(file_has_planted_frame());
    };
    let rec = NewRecord {
        schema_id: 0,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload: &[],
    };
    match how {
        Abandon::Panic => {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                let _ = w.append_with(&rec, 200, |buf| {
                    plant(buf);
                    panic!("fill failed");
                });
            }));
            assert!(result.is_err());
        }
        Abandon::PanicInTry => {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                let _ = w.try_append_with(&rec, 200, |buf| -> Result<(), ()> {
                    plant(buf);
                    panic!("fill failed");
                });
            }));
            assert!(result.is_err());
        }
        Abandon::Decline => {
            let result = w.try_append_with(&rec, 200, |buf| {
                plant(buf);
                Err("declined")
            });
            assert!(matches!(result, Ok(Err("declined"))), "{result:?}");
        }
    }
    assert!(w.abandoned_write_error().is_none());
    assert_eq!(
        w.next_seq(),
        3,
        "the abandoned record used no sequence number"
    );
    assert!(
        !file_has_planted_frame(),
        "the abandoned bytes are zeroed in the file"
    );
    assert!(
        r.try_next().unwrap().is_none(),
        "nothing committed by the abandoned append"
    );

    // The next append reuses the space: the reader finds it right after record 2.
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
fn fallible_fill_is_not_called_when_the_append_fails_first() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 2);
    let max = format::max_payload_len(SMALL_SEGMENT) as usize;
    let rec = NewRecord {
        schema_id: 1,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload: &[],
    };
    let result = w.try_append_with(&rec, max + 1, |_| -> Result<(), ()> {
        panic!("fill must not be called")
    });
    assert!(
        matches!(result, Err(Error::RecordTooLarge { .. })),
        "{result:?}"
    );
    // A committed fallible fill gets the next sequence number, like `append_with`.
    let seq = w
        .try_append_with(&rec, 3, |buf| {
            buf.copy_from_slice(b"abc");
            Ok::<(), ()>(())
        })
        .unwrap()
        .unwrap();
    assert_eq!(seq, 2);
    drop(w);
    let all = read_all(dir.path());
    assert_eq!(all.len(), 3);
    assert_eq!(all[2].payload, b"abc");
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
fn fail_policy_refuses_invalid_successor_header() {
    // Found by the `recover_dir` fuzz target: a sealed segment names a successor whose header is invalid and nothing
    // follows it. `Quarantine` moves the successor away and recreates it; `Fail` must refuse and leave it in place
    // (format.md → Recovery, step 5), not quarantine it.
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 3);
    drop(w);
    let (_, first) = segments(dir.path()).pop().unwrap();
    let (end, _) = end_of_records(&first);
    let raw = open_raw(&first);
    raw.write_all_at(&format::seal_crc(3).to_le_bytes(), end + 4)
        .unwrap();
    raw.write_all_at(&format::SEAL_MARKER.to_le_bytes(), end)
        .unwrap();
    let successor = create_raw_segment(dir.path(), 3, SMALL_SEGMENT);
    open_raw(&successor).write_all_at(b"X", 0).unwrap(); // damages the magic
    let before = std::fs::read(&successor).unwrap();

    match Writer::open(dir.path(), small_config().recovery(RecoveryPolicy::Fail)) {
        Err(Error::Corrupt { .. }) => {}
        other => panic!("expected a corruption error, got {other:?}"),
    }
    assert_eq!(std::fs::read(&successor).unwrap(), before);
    assert!(!dir.path().join("quarantine").exists());

    let w = Writer::open(dir.path(), small_config()).unwrap();
    assert!(
        matches!(
            &w.recovery_report().actions[..],
            [
                RecoveryAction::QuarantinedSegment { .. },
                RecoveryAction::CompletedRoll { .. }
            ]
        ),
        "{:?}",
        w.recovery_report()
    );
    assert_eq!(w.next_seq(), 3);
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

#[test]
fn records_iterator_yields_committed_records_and_stops() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 700);
    let mut r = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let first: Vec<_> = r
        .records()
        .take(10)
        .collect::<tickertape::Result<_>>()
        .unwrap();
    assert_eq!(
        first.iter().map(|x| x.seq).collect::<Vec<_>>(),
        (0..10).collect::<Vec<_>>()
    );
    let rest: Vec<_> = r.records().collect::<tickertape::Result<_>>().unwrap();
    assert_eq!(rest.len(), 690);
    rest.iter().for_each(check_record);
    assert_eq!(
        r.records().count(),
        0,
        "ends at the current end without waiting"
    );
    append_n(&mut w, 5);
    assert_eq!(
        r.records().map(|x| x.unwrap().seq).collect::<Vec<_>>(),
        (700..705).collect::<Vec<_>>()
    );
}

/// A journal whose newest segment was linked by the last of `rolls` rolls after the checkpoint and holds about 100
/// records: returns the newest segment's base and path, and the previous segment's path.
fn rolled_after_checkpoint(
    dir: &std::path::Path,
    rolls: usize,
) -> (u64, std::path::PathBuf, std::path::PathBuf) {
    let mut w = Writer::open(dir, small_config()).unwrap();
    append_n(&mut w, 600);
    w.checkpoint().unwrap();
    for _ in 0..rolls {
        let before = segments(dir).len();
        while segments(dir).len() == before {
            append_n(&mut w, 1);
        }
    }
    append_n(&mut w, 100);
    drop(w);
    let segs = segments(dir);
    let (base, newest) = segs[segs.len() - 1].clone();
    let previous = segs[segs.len() - 2].1.clone();
    assert!(
        end_of_records(&newest).0 > 8192,
        "the newest segment holds more than two pages of records"
    );
    (base, newest, previous)
}

fn zero_range(path: &std::path::Path, from: u64, len: u64) {
    open_raw(path)
        .write_all_at(&vec![0u8; len as usize], from)
        .unwrap();
}

fn has_restored(w: &Writer, segment: &std::path::Path) -> bool {
    w.recovery_report()
        .actions
        .iter()
        .any(|a| matches!(a, RecoveryAction::RestoredHeader { segment: s } if s == segment))
}

/// A roll does not sync the new segment (ADR 0038): after an OS crash or power loss before the next checkpoint, the
/// new segment's name can be on disk while none of its bytes are. Recovery writes its header again and continues as
/// if the header had been there, under both policies: the records before it are intact and nothing is quarantined.
/// With the old segment's seal on disk, and without it (the scan then reaches the new segment at the seal's place).
#[test]
fn unwritten_newest_segment_is_restored_under_both_policies() {
    for seal_on_disk in [true, false] {
        for policy in [RecoveryPolicy::Fail, RecoveryPolicy::Quarantine] {
            let dir = tempfile::tempdir().unwrap();
            let (base, newest, previous) = rolled_after_checkpoint(dir.path(), 1);
            let len = std::fs::metadata(&newest).unwrap().len();
            zero_range(&newest, 0, len);
            if !seal_on_disk {
                let (end, _) = end_of_records(&previous);
                zero_range(&previous, end, format::SEAL_LEN as u64);
            }
            let mut w = Writer::open(dir.path(), small_config().recovery(policy)).unwrap();
            let case = format!("seal on disk {seal_on_disk}, {policy:?}");
            assert!(
                has_restored(&w, &newest),
                "{case}: {:?}",
                w.recovery_report()
            );
            assert_eq!(
                w.recovery_report()
                    .actions
                    .iter()
                    .any(|a| matches!(a, RecoveryAction::Sealed { .. })),
                !seal_on_disk,
                "{case}: {:?}",
                w.recovery_report()
            );
            assert_eq!(w.next_seq(), base, "{case}");
            assert!(!dir.path().join("quarantine").exists(), "{case}");
            append_n(&mut w, 10);
            w.checkpoint().unwrap();
            drop(w);
            let w =
                Writer::open(dir.path(), small_config().recovery(RecoveryPolicy::Fail)).unwrap();
            assert!(
                w.recovery_report().actions.is_empty(),
                "{case}: {:?}",
                w.recovery_report()
            );
            assert_sequence(&read_all(dir.path()), base + 10);
        }
    }
}

/// The new segment's file is empty (its preallocated size did not reach the disk either): it holds only zeros, so
/// recovery replaces it by a new empty segment of the configured size, under both policies.
#[test]
fn empty_newest_segment_file_is_replaced() {
    for policy in [RecoveryPolicy::Fail, RecoveryPolicy::Quarantine] {
        let dir = tempfile::tempdir().unwrap();
        let (base, newest, _) = rolled_after_checkpoint(dir.path(), 1);
        open_raw(&newest).set_len(0).unwrap();
        let mut w = Writer::open(dir.path(), small_config().recovery(policy)).unwrap();
        assert!(
            has_restored(&w, &newest),
            "{policy:?}: {:?}",
            w.recovery_report()
        );
        assert_eq!(w.next_seq(), base);
        assert_eq!(std::fs::metadata(&newest).unwrap().len(), SMALL_SEGMENT);
        assert!(!dir.path().join("quarantine").exists());
        append_n(&mut w, 5);
        drop(w);
        assert_sequence(&read_all(dir.path()), base + 5);
    }
}

/// Later pages of the new segment reached the disk, its first page (header and first records) did not: once the
/// header is restored this is the state a durable header with a lost first page gives, and it is handled the same
/// way (C8, C11 (2)): evidence of committed records after an empty commit word, so `Fail` refuses at the first
/// record slot, and `Quarantine` preserves the records in `quarantine/`. The refused open changes no file, the
/// header included (C11: under `Fail` a restoration is made only when the open succeeds).
#[test]
fn unwritten_first_page_with_later_records_is_a_committed_tail() {
    let dir = tempfile::tempdir().unwrap();
    let (base, newest, _) = rolled_after_checkpoint(dir.path(), 1);
    zero_range(&newest, 0, 4096);
    let before = snapshot(dir.path());
    match Writer::open(dir.path(), small_config().recovery(RecoveryPolicy::Fail)) {
        Err(Error::Corrupt { path, offset, .. }) => {
            assert_eq!(
                (path, offset),
                (newest.clone(), format::SEGMENT_HEADER_LEN as u64)
            );
        }
        other => panic!("expected a corruption error, got {other:?}"),
    }
    assert!(
        snapshot(dir.path()) == before,
        "the refused open changed a file"
    );
    assert!(!dir.path().join("quarantine").exists());

    let w = Writer::open(dir.path(), small_config()).unwrap();
    assert!(
        w.recovery_report().actions.iter().any(|a| matches!(
            a,
            RecoveryAction::RepairedTail { segment, offset: 64, .. } if *segment == newest
        )),
        "{:?}",
        w.recovery_report()
    );
    assert_eq!(w.next_seq(), base);
    assert_sequence(&read_all(dir.path()), base);
}

/// Two segments linked after the checkpoint whose bytes never reached the disk: the first is restored and is the end
/// of the chain, so the second is disconnected and handled as a disconnected segment with a header is (C8).
#[test]
fn two_unwritten_segments_restore_the_first_and_disconnect_the_second() {
    let dir = tempfile::tempdir().unwrap();
    let (base, newest, previous) = rolled_after_checkpoint(dir.path(), 2);
    let previous_base = segments(dir.path()).iter().rev().nth(1).unwrap().0;
    for path in [&previous, &newest] {
        let len = std::fs::metadata(path).unwrap().len();
        zero_range(path, 0, len);
    }
    // `Fail` would restore the first header, then refuses at the disconnected segment: the restoration is held back
    // and never made, so the refused open changes no file (C11).
    let before = snapshot(dir.path());
    match Writer::open(dir.path(), small_config().recovery(RecoveryPolicy::Fail)) {
        Err(Error::Corrupt {
            path,
            reason: CorruptReason::BrokenChain(b),
            ..
        }) => assert_eq!((path, b), (newest.clone(), base)),
        other => panic!("expected BrokenChain, got {other:?}"),
    }
    assert!(
        snapshot(dir.path()) == before,
        "the refused open changed a file"
    );
    let w = Writer::open(dir.path(), small_config()).unwrap();
    assert!(has_restored(&w, &previous), "{:?}", w.recovery_report());
    assert!(
        w.recovery_report().actions.iter().any(|a| matches!(
            a,
            RecoveryAction::QuarantinedSegment { segment, reason: CorruptReason::BrokenChain(b), .. }
                if *segment == newest && *b == base
        )),
        "{:?}",
        w.recovery_report()
    );
    assert_eq!(w.next_seq(), previous_base);
}

/// The regular files of `dir` (not `writer.lock`) and their contents.
fn snapshot(dir: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file() && p.file_name().unwrap() != "writer.lock")
        .map(|p| {
            let bytes = std::fs::read(&p).unwrap();
            (p, bytes)
        })
        .collect();
    files.sort();
    files
}

/// A copy of the regular files of `from` in a new directory.
fn copy_journal(from: &std::path::Path) -> tempfile::TempDir {
    let to = tempfile::tempdir().unwrap();
    for (path, bytes) in snapshot(from) {
        std::fs::write(to.path().join(path.file_name().unwrap()), bytes).unwrap();
    }
    to
}

/// A segment file of invalid length holding only zeros, with a later segment after it, is replaced by preparing the
/// new segment under a temporary name and renaming it over the old file (C8): a failed allocation, or a crash after
/// the copy is synced and before the rename, leaves the old file in place, and the next open recovers exactly as an
/// uninterrupted one does (it does not find a missing middle segment).
#[test]
fn interrupted_replacement_of_an_unwritten_segment_leaves_no_gap() {
    use tickertape::fault::{self, Point};
    for (point, errno) in [(Point::Preallocate, 28), (Point::RestoreBeforeRename, 5)] {
        let dir = tempfile::tempdir().unwrap();
        let (_, newest, previous) = rolled_after_checkpoint(dir.path(), 2);
        let previous_base = segments(dir.path()).iter().rev().nth(1).unwrap().0;
        open_raw(&previous).set_len(0).unwrap();
        open_raw(&previous).set_len(100).unwrap();
        let reference = copy_journal(dir.path());
        let expected = Writer::open(reference.path(), small_config()).unwrap();
        assert!(has_restored(
            &expected,
            &reference.path().join(previous.file_name().unwrap())
        ));

        fault::fail_next(point, errno);
        let failed = Writer::open(dir.path(), small_config());
        fault::clear();
        assert!(failed.is_err(), "{point:?}: {failed:?}");
        assert!(previous.exists() && newest.exists(), "{point:?}");
        let w = Writer::open(dir.path(), small_config()).unwrap();
        assert!(
            has_restored(&w, &previous),
            "{point:?}: {:?}",
            w.recovery_report()
        );
        assert_eq!(w.next_seq(), previous_base, "{point:?}");
        assert_eq!(w.next_seq(), expected.next_seq(), "{point:?}");
        let names = |d: &std::path::Path| {
            segments(d)
                .into_iter()
                .map(|(base, _)| base)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(dir.path()), names(reference.path()), "{point:?}");
        assert_eq!(
            std::fs::metadata(&previous).unwrap().len(),
            SMALL_SEGMENT,
            "{point:?}"
        );
        drop(w);
        assert_eq!(read_all(dir.path()).len(), read_all(reference.path()).len());
    }
}

/// Under `Fail`, a successor whose header never reached the disk is restored only once nothing in the scan step can
/// still refuse (C11): in front of it, a segment whose seal's commit word is empty with non-zero bytes beyond the
/// seal makes `Fail` refuse (`BadSeal`), and the refused open changes no file, byte for byte, whether the successor
/// would have had its header written again or would have been replaced.
#[test]
fn fail_refusing_before_an_unwritten_successor_changes_no_file() {
    for empty_file in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (_, newest, previous) = rolled_after_checkpoint(dir.path(), 1);
        if empty_file {
            open_raw(&newest).set_len(0).unwrap();
        } else {
            let len = std::fs::metadata(&newest).unwrap().len();
            zero_range(&newest, 0, len);
        }
        let (end, _) = end_of_records(&previous);
        zero_range(&previous, end, format::COMMIT_WORD_LEN as u64);
        open_raw(&previous)
            .write_all_at(&[0xAB; 8], end + format::SEAL_LEN as u64 + 16)
            .unwrap();
        let before = snapshot(dir.path());
        match Writer::open(dir.path(), small_config().recovery(RecoveryPolicy::Fail)) {
            Err(Error::Corrupt {
                path,
                offset,
                reason: CorruptReason::BadSeal,
            }) => assert_eq!((path, offset), (previous.clone(), end)),
            other => panic!("empty file {empty_file}: expected BadSeal, got {other:?}"),
        }
        assert!(
            snapshot(dir.path()) == before,
            "empty file {empty_file}: a file changed"
        );
    }
}

/// The state the fuzzer found (`fuzz/seeds/recover_dir/unwritten_successor_then_disconnected_fail`): a segment whose
/// records end at an empty commit word (its seal never reached the disk), the successor at the seal's place whose
/// header never reached the disk (an empty file, or a file of zeros), and a later segment, which is disconnected once
/// the restored successor ends the chain. `Fail` would seal the first segment and restore the successor, then refuses
/// at the disconnected segment (`BrokenChain`): both changes are held back and never made, so no file changes, byte
/// for byte, and no temporary file is left (C11). `Quarantine` restores, seals and quarantines the later segment.
#[test]
fn fail_refusing_after_an_unwritten_successor_changes_no_file() {
    for empty_file in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let (newest_base, newest, previous) = rolled_after_checkpoint(dir.path(), 2);
        let segs = segments(dir.path());
        let previous_base = segs[segs.len() - 2].0;
        let first = segs[segs.len() - 3].1.clone();
        let (end, _) = end_of_records(&first);
        zero_range(&first, end, format::SEAL_LEN as u64);
        if empty_file {
            open_raw(&previous).set_len(0).unwrap();
        } else {
            let len = std::fs::metadata(&previous).unwrap().len();
            zero_range(&previous, 0, len);
        }
        let before = snapshot(dir.path());
        match Writer::open(dir.path(), small_config().recovery(RecoveryPolicy::Fail)) {
            Err(Error::Corrupt {
                path,
                offset: 0,
                reason: CorruptReason::BrokenChain(b),
            }) => assert_eq!((path, b), (newest.clone(), newest_base)),
            other => panic!("empty file {empty_file}: expected BrokenChain, got {other:?}"),
        }
        assert!(
            snapshot(dir.path()) == before,
            "empty file {empty_file}: the refused open changed a file"
        );

        let w = Writer::open(dir.path(), small_config()).unwrap();
        let report = w.recovery_report();
        assert!(has_restored(&w, &previous), "{empty_file}: {report:?}");
        assert!(
            report
                .actions
                .iter()
                .any(|a| matches!(a, RecoveryAction::Sealed { segment } if *segment == first)),
            "{empty_file}: {report:?}"
        );
        assert!(
            report.actions.iter().any(|a| matches!(
                a,
                RecoveryAction::QuarantinedSegment { segment, .. } if *segment == newest
            )),
            "{empty_file}: {report:?}"
        );
        assert_eq!(w.next_seq(), previous_base, "{empty_file}");
    }
}

/// Under `Fail`, the changes held back after a restoration are made in the order the scan decided them, once the open
/// can no longer refuse (C11). An interruption between two of them (here: after the successor's restoration, before
/// the seal in front of it) leaves a journal that the next open recovers to the same end, by making the rest.
#[test]
fn interrupted_held_changes_are_finished_by_the_next_open() {
    use tickertape::fault::{self, Point};
    for empty_file in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (base, newest, previous) = rolled_after_checkpoint(dir.path(), 1);
        if empty_file {
            open_raw(&newest).set_len(0).unwrap();
        } else {
            let len = std::fs::metadata(&newest).unwrap().len();
            zero_range(&newest, 0, len);
        }
        let (end, _) = end_of_records(&previous);
        zero_range(&previous, end, format::SEAL_LEN as u64);
        let fail = || small_config().recovery(RecoveryPolicy::Fail);

        fault::fail_next(Point::BetweenHeldChanges, 5);
        let failed = Writer::open(dir.path(), fail());
        fault::clear();
        assert!(failed.is_err(), "empty file {empty_file}: {failed:?}");
        let image = std::fs::read(&newest).unwrap();
        assert!(
            format::decode_segment_header(image[..64].try_into().unwrap()).is_ok(),
            "empty file {empty_file}: the restoration, decided first, was made"
        );
        assert_eq!(
            end_of_records(&previous).0,
            end,
            "empty file {empty_file}: the seal was not written"
        );

        let mut w = Writer::open(dir.path(), fail()).unwrap();
        let report = w.recovery_report();
        assert!(!has_restored(&w, &newest), "{empty_file}: {report:?}");
        assert!(
            report
                .actions
                .iter()
                .any(|a| matches!(a, RecoveryAction::Sealed { segment } if *segment == previous)),
            "{empty_file}: {report:?}"
        );
        assert_eq!(w.next_seq(), base, "{empty_file}");
        append_n(&mut w, 5);
        drop(w);
        assert_sequence(&read_all(dir.path()), base + 5);
    }
}
