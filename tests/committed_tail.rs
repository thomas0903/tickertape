//! Recovery after an empty commit word that hides committed records (ADR 0035, `docs/format.md` → Recovery, crash
//! contract C8 and C11): storage damage that zeroes a commit word after the checkpoint must be treated as corruption
//! (preserved in `quarantine/` and reported, or refused with `RecoveryPolicy::Fail`), never zeroed in place, while
//! every shape a torn append can leave is still zeroed in place with nothing quarantined.

use std::fs;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use proptest::prelude::*;
use tickertape::format::{self, RecordHeader, Slot};
use tickertape::inspect;
use tickertape::{
    CorruptReason, Error, Reader, ReaderConfig, RecoveryAction, RecoveryPolicy, Writer,
    WriterConfig,
};
use tickertape_tests::{append_n, check_record, open_raw, read_all, segments, small_config};

/// Every record slot of segment `path` from offset 64: `(offset, seq, len)`, and where the records end.
fn record_slots(path: &Path) -> (Vec<(u64, u64, u32)>, u64) {
    let image = fs::read(path).unwrap();
    let header = format::decode_segment_header(image[..64].try_into().unwrap()).unwrap();
    let (mut offset, mut seq) = (format::SEGMENT_HEADER_LEN as u64, header.base_seq);
    let mut out = Vec::new();
    while let Ok(Slot::Record {
        len, next_offset, ..
    }) = format::read_slot(&image[..], offset, seq)
    {
        out.push((offset, seq, len));
        offset = next_offset;
        seq += 1;
    }
    (out, offset)
}

/// The segment holding record `seq`, and the record's offset in it.
fn locate(dir: &Path, seq: u64) -> (PathBuf, u64) {
    for (_, path) in segments(dir) {
        if let Some(&(offset, ..)) = record_slots(&path).0.iter().find(|s| s.1 == seq) {
            return (path, offset);
        }
    }
    panic!("record {seq} not found");
}

/// Zeroes the commit word of record `seq` in place, as storage damage would. Returns the segment and offset.
fn zero_commit_word(dir: &Path, seq: u64) -> (PathBuf, u64) {
    let (path, offset) = locate(dir, seq);
    open_raw(&path).write_all_at(&[0; 4], offset).unwrap();
    (path, offset)
}

fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    segments(dir)
        .into_iter()
        .map(|(_, p)| {
            let bytes = fs::read(&p).unwrap();
            (p, bytes)
        })
        .collect()
}

/// The reviewer's journal: records 0 to 99, a checkpoint at 50, all in one 64 KiB segment.
fn hundred_with_checkpoint_at_50(dir: &Path) {
    let mut w = Writer::open(dir, small_config()).unwrap();
    append_n(&mut w, 50);
    w.checkpoint().unwrap();
    append_n(&mut w, 50);
    assert_eq!(segments(dir).len(), 1);
}

/// Checks a `RepairedTail` for a damaged commit word at `offset` of `path`: the reason, the quarantine file (byte for
/// byte the removed tail of `damaged`, up to its last non-zero byte), and the repaired segment (the prefix kept, the
/// rest zero).
fn assert_preserved(action: &RecoveryAction, path: &Path, offset: u64, damaged: &[u8], found: u64) {
    let RecoveryAction::RepairedTail {
        segment,
        offset: at,
        len,
        quarantine_file,
        reason,
    } = action
    else {
        panic!("expected RepairedTail, got {action:?}");
    };
    assert_eq!((segment.as_path(), *at), (path, offset));
    assert_eq!(*reason, CorruptReason::EmptyCommitWord(found));
    assert_eq!(*len, damaged.len() as u64 - offset);
    let kept = fs::read(quarantine_file).unwrap();
    let tail = &damaged[offset as usize..];
    assert!(!kept.is_empty());
    assert_eq!(kept[..], tail[..kept.len()], "quarantined bytes");
    assert!(
        tail[kept.len()..].iter().all(|&b| b == 0),
        "quarantine dropped bytes"
    );
    let repaired = fs::read(path).unwrap();
    assert_eq!(repaired[..offset as usize], damaged[..offset as usize]);
    assert!(repaired[offset as usize..].iter().all(|&b| b == 0));
}

#[test]
fn zeroed_commit_word_after_checkpoint_is_quarantined() {
    // The reproduction: records 0 to 99, checkpoint at 50, the commit word of record 60 zeroed.
    let dir = tempfile::tempdir().unwrap();
    hundred_with_checkpoint_at_50(dir.path());
    let original = fs::read(&segments(dir.path())[0].1).unwrap();
    let (path, offset) = zero_commit_word(dir.path(), 60);
    let damaged = fs::read(&path).unwrap();

    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    let actions = &w.recovery_report().actions;
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert_preserved(&actions[0], &path, offset, &damaged, 61);
    assert_eq!(w.next_seq(), 60);
    assert_eq!(
        record_slots(&path).0.len(),
        60,
        "the repaired segment keeps records 0 to 59"
    );
    // Records 60 to 99 are in the quarantine file, intact: with the lost commit word put back, they validate there.
    let RecoveryAction::RepairedTail {
        quarantine_file, ..
    } = &actions[0]
    else {
        unreachable!()
    };
    let kept = fs::read(quarantine_file).unwrap();
    let mut image = vec![0u8; damaged.len()];
    image[offset as usize..offset as usize + kept.len()].copy_from_slice(&kept);
    image[offset as usize..offset as usize + 4]
        .copy_from_slice(&original[offset as usize..offset as usize + 4]);
    let scan = format::scan_segment(&image[..], offset, 60);
    assert_eq!((scan.records, scan.next_seq), (40, 100));

    append_n(&mut w, 5);
    let all = read_all(dir.path());
    assert_eq!(all.len(), 65);
    all.iter().for_each(check_record);
    drop(w);
    let w = Writer::open(dir.path(), small_config()).unwrap();
    assert!(w.recovery_report().actions.is_empty(), "idempotent");
}

#[test]
fn zeroed_commit_word_at_first_record_after_checkpoint_is_quarantined() {
    let dir = tempfile::tempdir().unwrap();
    hundred_with_checkpoint_at_50(dir.path());
    let checkpoint = inspect::read_checkpoint(dir.path()).unwrap().unwrap();
    let (path, offset) = zero_commit_word(dir.path(), 50);
    assert_eq!(offset, checkpoint.offset);
    let damaged = fs::read(&path).unwrap();
    let w = Writer::open(dir.path(), small_config()).unwrap();
    let actions = &w.recovery_report().actions;
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert_preserved(&actions[0], &path, offset, &damaged, 51);
    assert_eq!(w.next_seq(), 50);
}

#[test]
fn zeroed_commit_word_of_the_newest_record_is_indistinguishable_from_a_torn_append() {
    // The newest record's commit word zeroed, nothing after it: byte for byte the state a writer killed after
    // writing a complete body and before storing its commit word leaves. Recovery cannot tell them apart and handles
    // it as a torn append (ADR 0035, "What cannot be detected"); `verify` cannot report it either.
    let dir = tempfile::tempdir().unwrap();
    hundred_with_checkpoint_at_50(dir.path());
    let (path, offset) = zero_commit_word(dir.path(), 99);
    assert!(inspect::verify(dir.path()).unwrap().problems.is_empty());
    let w = Writer::open(dir.path(), small_config()).unwrap();
    assert!(
        matches!(&w.recovery_report().actions[..], [RecoveryAction::ZeroedUncommitted { segment, offset: at, .. }] if *segment == path && *at == offset + 4),
        "{:?}",
        w.recovery_report()
    );
    assert_eq!(w.next_seq(), 99);
    assert!(!dir.path().join("quarantine").exists());
}

/// A journal of 64 KiB segments with no checkpoint, so recovery scans every segment.
fn multi_segment(dir: &Path) -> Vec<(u64, PathBuf)> {
    let mut w = Writer::open(dir, small_config()).unwrap();
    append_n(&mut w, 1500);
    let segs = segments(dir);
    assert!(segs.len() >= 4, "{segs:?}");
    segs
}

#[test]
fn zeroed_commit_word_in_a_sealed_segment_is_quarantined() {
    // In the middle of a sealed segment, and at its last record (just before the seal).
    for last in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let segs = multi_segment(dir.path());
        let (slots, _) = record_slots(&segs[1].1);
        let (_, seq, _) = if last {
            slots[slots.len() - 1]
        } else {
            slots[slots.len() / 2]
        };
        let (path, offset) = zero_commit_word(dir.path(), seq);
        let damaged = fs::read(&path).unwrap();
        let later: Vec<(PathBuf, Vec<u8>)> = segs[2..]
            .iter()
            .map(|(_, p)| (p.clone(), fs::read(p).unwrap()))
            .collect();

        let w = Writer::open(dir.path(), small_config()).unwrap();
        let actions = &w.recovery_report().actions;
        assert_eq!(actions.len(), 1 + later.len(), "{actions:?}");
        assert_preserved(&actions[0], &path, offset, &damaged, seq + 1);
        for (action, (p, bytes)) in actions[1..].iter().zip(&later) {
            let RecoveryAction::QuarantinedSegment {
                segment,
                quarantine_file,
                ..
            } = action
            else {
                panic!("expected QuarantinedSegment, got {action:?}");
            };
            assert_eq!(segment, p);
            assert!(fs::read(quarantine_file).unwrap() == *bytes);
        }
        assert_eq!(w.next_seq(), seq, "last = {last}");
        let all = read_all(dir.path());
        assert_eq!(all.len() as u64, seq);
        all.iter().for_each(check_record);
    }
}

#[test]
fn fail_policy_refuses_a_zeroed_commit_word_without_changing_a_byte() {
    // Each case builds a journal and returns the record whose commit word gets zeroed.
    type Build = fn(&Path) -> u64;
    let cases: [(&str, Build); 3] = [
        ("after the checkpoint", |d| {
            hundred_with_checkpoint_at_50(d);
            60
        }),
        ("first after the checkpoint", |d| {
            hundred_with_checkpoint_at_50(d);
            50
        }),
        ("sealed segment", |d| {
            let segs = multi_segment(d);
            let (slots, _) = record_slots(&segs[1].1);
            slots[3].1
        }),
    ];
    for (name, build) in cases {
        let dir = tempfile::tempdir().unwrap();
        let seq = build(dir.path());
        let (path, offset) = zero_commit_word(dir.path(), seq);
        let before = snapshot(dir.path());
        let cfg = small_config().recovery(RecoveryPolicy::Fail);
        match Writer::open(dir.path(), cfg) {
            Err(Error::Corrupt {
                path: p,
                offset: at,
                reason: CorruptReason::EmptyCommitWord(found),
            }) => {
                assert_eq!((p, at, found), (path.clone(), offset, seq + 1), "{name}");
            }
            other => panic!("{name}: expected EmptyCommitWord, got {other:?}"),
        }
        assert!(snapshot(dir.path()) == before, "{name}: bytes changed");
        assert!(!dir.path().join("quarantine").exists(), "{name}");
    }
}

#[test]
fn live_readers_never_see_changed_bytes_across_the_repair() {
    let dir = tempfile::tempdir().unwrap();
    hundred_with_checkpoint_at_50(dir.path());
    // A safe reader at the end, a safe reader before the damage, and a zero-copy reader at the end.
    let mut safe_end = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    while safe_end.next_owned().unwrap().is_some() {}
    let mut safe_mid = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    safe_mid.seek(55).unwrap();
    // SAFETY: only tickertape writers modify the segment files while this reader exists, except the test's own
    // zeroing of one commit word, which this reader has already passed and never reads again.
    let mut mapped = unsafe { Reader::open_mapped(dir.path(), ReaderConfig::default()) }.unwrap();
    for _ in 0..99 {
        mapped.try_next().unwrap().unwrap();
    }
    let rec = mapped.try_next().unwrap().unwrap();
    assert_eq!(rec.seq, 99);
    let held: &[u8] = rec.payload;
    let copy = held.to_vec();
    assert!(!copy.is_empty());

    zero_commit_word(dir.path(), 60);
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    // The payload slice the zero-copy reader holds still has the same bytes.
    assert_eq!(held, &copy[..], "a mapped payload changed under the reader");
    assert!(matches!(
        w.recovery_report().actions[..],
        [RecoveryAction::RepairedTail { .. }]
    ));

    // Each reader either keeps returning the bytes it already returned, or reports `Discarded`; then it re-seeks onto
    // the repaired history.
    append_n(&mut w, 3);
    let until_discarded =
        |r: &mut Reader, at: u64, check_old: &dyn Fn(&tickertape::OwnedRecord)| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match r.try_next() {
                    Err(Error::Discarded { seq }) => {
                        assert_eq!(seq, at);
                        break;
                    }
                    Ok(Some(rec)) => check_old(&rec.to_owned()),
                    Ok(None) => {
                        assert!(Instant::now() < deadline, "reader never noticed the repair");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("reader error: {e}"),
                }
            }
            r.seek(at.min(60)).unwrap();
        };
    until_discarded(&mut mapped, 100, &|_| panic!("record past the end"));
    until_discarded(&mut safe_end, 100, &|_| panic!("record past the end"));
    until_discarded(&mut safe_mid, 60, &|r| {
        assert!((55..60).contains(&r.seq));
        check_record(r);
    });
    for r in [&mut mapped, &mut safe_end, &mut safe_mid] {
        for seq in 60..63 {
            let rec = r.next_owned().unwrap().unwrap();
            assert_eq!(rec.seq, seq);
            check_record(&rec);
        }
        assert!(r.try_next().unwrap().is_none());
    }
}

#[test]
fn verify_reports_a_zeroed_commit_word_before_recovery() {
    let dir = tempfile::tempdir().unwrap();
    hundred_with_checkpoint_at_50(dir.path());
    let (path, offset) = zero_commit_word(dir.path(), 60);
    let report = inspect::verify(dir.path()).unwrap();
    assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
    let p = &report.problems[0];
    assert_eq!(
        (&p.path, p.offset, p.reason),
        (&path, offset, CorruptReason::EmptyCommitWord(61))
    );
    assert_eq!(report.last_seq, Some(59));

    let dir = tempfile::tempdir().unwrap();
    let segs = multi_segment(dir.path());
    let seq = record_slots(&segs[1].1).0[5].1;
    let (path, offset) = zero_commit_word(dir.path(), seq);
    let report = inspect::verify(dir.path()).unwrap();
    assert!(
        report.problems.iter().any(|p| p.path == path
            && p.offset == offset
            && p.reason == CorruptReason::EmptyCommitWord(seq + 1)),
        "{:?}",
        report.problems
    );
}

/// The ways a writer killed during an append (or during the clean-up of an abandoned one) can leave the slot after
/// the last record: `(name, bytes at [P, P + n))`. `next` is the sequence number the append had; `stale` is a copy
/// of an earlier committed frame.
fn torn_shapes(next: u64, stale: &[u8]) -> Vec<(String, Vec<u8>)> {
    let payload: Vec<u8> = (0..300u32).map(|i| (i * 7 + 3) as u8).collect();
    let header = RecordHeader {
        seq: next,
        schema_id: 5,
        venue_ts_ns: 11,
        local_ts_ns: 12,
    };
    let len = format::frame_len(payload.len()).unwrap();
    let mut complete = vec![0u8; len as usize];
    format::encode_record_body(&header, &payload, &mut complete[4..]);
    let mut shapes = Vec::new();
    // Header fields written, payload not started (`write_record_fields`).
    let mut fields = vec![0u8; len as usize];
    format::write_record_fields(&header, &mut fields[4..]);
    shapes.push(("header fields only".to_string(), fields.clone()));
    // The fill closure stopped after `k` payload bytes.
    for k in [1, 8, 33, 150, 299, 300] {
        let mut s = fields.clone();
        s[40..40 + k].copy_from_slice(&payload[..k]);
        shapes.push((format!("payload filled to {k}"), s));
    }
    // Complete body including the CRC, commit word never stored.
    shapes.push(("complete body, no commit word".into(), complete.clone()));
    // An abandoned write whose zeroing (`body.fill(0)`) was interrupted after `j` bytes.
    for j in [4, 40, 100, 296] {
        let mut s = complete.clone();
        s[4..4 + j].fill(0);
        shapes.push((format!("abandoned write zeroed to {j}"), s));
    }
    // Payloads that hold frames which validate, but with sequence numbers the position rules out: a stale copy of
    // an earlier record, and a frame carrying the torn record's own sequence number.
    let mut with_stale = complete.clone();
    with_stale[48..48 + stale.len().min(240)].copy_from_slice(&stale[..stale.len().min(240)]);
    shapes.push(("payload holding a stale frame".into(), with_stale));
    let own_len = format::frame_len(8).unwrap();
    let mut own = vec![0u8; own_len as usize];
    format::encode_record_body(&header, &[9; 8], &mut own[4..]);
    own[..4].copy_from_slice(&own_len.to_le_bytes());
    let mut with_own = fields.clone();
    with_own[48..48 + own.len()].copy_from_slice(&own);
    shapes.push((
        "payload holding a frame with its own sequence".into(),
        with_own,
    ));
    shapes
}

#[test]
fn every_torn_append_shape_is_zeroed_in_place_with_nothing_quarantined() {
    let base = tempfile::tempdir().unwrap();
    hundred_with_checkpoint_at_50(base.path());
    let (_, path) = segments(base.path()).pop().unwrap();
    let (slots, end) = record_slots(&path);
    let (stale_at, _, stale_len) = slots[10];
    let image = fs::read(&path).unwrap();
    let stale = image[stale_at as usize..(stale_at + u64::from(stale_len)) as usize].to_vec();
    for (name, bytes) in torn_shapes(100, &stale) {
        let dir = tempfile::tempdir().unwrap();
        for entry in fs::read_dir(base.path()).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
            }
        }
        let (_, seg) = segments(dir.path()).pop().unwrap();
        open_raw(&seg).write_all_at(&bytes, end).unwrap();
        assert!(
            inspect::verify(dir.path()).unwrap().problems.is_empty(),
            "{name}: verify reported a torn append"
        );
        for policy in [RecoveryPolicy::Fail, RecoveryPolicy::Quarantine] {
            let w = Writer::open(dir.path(), small_config().recovery(policy))
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let actions = &w.recovery_report().actions;
            if policy == RecoveryPolicy::Fail {
                assert!(
                    matches!(&actions[..], [RecoveryAction::ZeroedUncommitted { segment, offset, .. }] if *segment == seg && *offset >= end + 4),
                    "{name}: {actions:?}"
                );
            } else {
                assert!(actions.is_empty(), "{name}: second open: {actions:?}");
            }
            assert_eq!(w.next_seq(), 100, "{name}");
        }
        assert!(!dir.path().join("quarantine").exists(), "{name}");
        assert!(
            fs::read(&seg).unwrap()[end as usize..]
                .iter()
                .all(|&b| b == 0)
        );
        assert_eq!(read_all(dir.path()).len(), 100);
    }
}

#[test]
fn torn_payload_holding_the_next_records_frame_is_preserved_not_zeroed() {
    // The accepted false positive (ADR 0035): a torn append whose payload contains a frame of this journal's *next*
    // record, at an 8-byte-aligned offset, with a valid CRC. No rule can tell it from records hidden behind a damaged
    // commit word, so recovery preserves it rather than destroy it. Only a payload built from this journal's own
    // upcoming frames does this (tests/recovery.rs::panic_in_fill_then_shorter_append builds one deliberately).
    let dir = tempfile::tempdir().unwrap();
    hundred_with_checkpoint_at_50(dir.path());
    let (_, seg) = segments(dir.path()).pop().unwrap();
    let (_, end) = record_slots(&seg);
    let next_len = format::frame_len(4).unwrap();
    let mut next = vec![0u8; next_len as usize];
    let header = |seq| RecordHeader {
        seq,
        schema_id: 0,
        venue_ts_ns: 0,
        local_ts_ns: 0,
    };
    format::encode_record_body(&header(101), b"evil", &mut next[4..]);
    next[..4].copy_from_slice(&next_len.to_le_bytes());
    let mut torn = vec![0u8; 200];
    format::write_record_fields(&header(100), &mut torn[4..]);
    torn[48..48 + next.len()].copy_from_slice(&next);
    open_raw(&seg).write_all_at(&torn, end).unwrap();
    let damaged = fs::read(&seg).unwrap();
    let w = Writer::open(dir.path(), small_config()).unwrap();
    let actions = &w.recovery_report().actions;
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert_preserved(&actions[0], &seg, end, &damaged, 101);
    assert_eq!(w.next_seq(), 100);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// Generated journals (varied payload lengths, several 64 KiB segments, an optional checkpoint) with the commit
    /// word of one record after the checkpoint zeroed: recovery keeps every record before it, preserves everything
    /// after it in `quarantine/` (or refuses with `Fail`, changing nothing), and `verify` reports it first. The newest
    /// record alone is indistinguishable from a torn append and is zeroed in place.
    #[test]
    fn one_damaged_commit_word(
        lens in prop::collection::vec(0_usize..2_000, 1..160),
        checkpoint_at in any::<prop::sample::Index>(),
        with_checkpoint in any::<bool>(),
        damaged_at in any::<prop::sample::Index>(),
        fail in any::<bool>(),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let n = lens.len() as u64;
        let ckpt = if with_checkpoint { checkpoint_at.index(lens.len()) as u64 } else { 0 };
        let mut w = Writer::open(dir.path(), small_config()).unwrap();
        for (i, len) in lens.iter().enumerate() {
            if with_checkpoint && i as u64 == ckpt {
                w.checkpoint().unwrap();
            }
            let payload = vec![i as u8; *len];
            w.append(&tickertape::NewRecord { schema_id: 1, venue_ts_ns: 0, local_ts_ns: 0, payload: &payload }).unwrap();
        }
        drop(w);
        let seq = ckpt + damaged_at.index((n - ckpt) as usize) as u64;
        let (path, offset) = zero_commit_word(dir.path(), seq);
        let damaged = fs::read(&path).unwrap();
        let before = snapshot(dir.path());
        let newest = seq == n - 1;
        let problems = inspect::verify(dir.path()).unwrap().problems;
        if newest {
            prop_assert!(problems.is_empty(), "{:?}", problems);
        } else {
            prop_assert!(problems.iter().any(|p| p.path == path && p.offset == offset), "{:?}", problems);
        }
        let policy = if fail { RecoveryPolicy::Fail } else { RecoveryPolicy::Quarantine };
        match Writer::open(dir.path(), small_config().recovery(policy)) {
            Err(Error::Corrupt { path: p, offset: at, reason: CorruptReason::EmptyCommitWord(_) }) => {
                prop_assert!(fail && !newest);
                prop_assert_eq!((p, at), (path.clone(), offset));
                prop_assert!(snapshot(dir.path()) == before, "Fail changed bytes");
            }
            Err(e) => prop_assert!(false, "unexpected error {}", e),
            Ok(w) => {
                prop_assert_eq!(w.next_seq(), seq);
                let actions = &w.recovery_report().actions;
                if newest {
                    prop_assert!(matches!(&actions[..], [RecoveryAction::ZeroedUncommitted { .. }]), "{:?}", actions);
                } else {
                    prop_assert!(!fail, "Fail opened a damaged journal: {:?}", actions);
                    assert_preserved(&actions[0], &path, offset, &damaged, seq + 1);
                    prop_assert!(actions[1..].iter().all(|a| matches!(a, RecoveryAction::QuarantinedSegment { .. })), "{:?}", actions);
                }
                drop(w);
                let all = read_all(dir.path());
                prop_assert_eq!(all.len() as u64, seq);
                for (i, r) in all.iter().enumerate() {
                    prop_assert_eq!(r.payload.len(), lens[i]);
                }
            }
        }
    }
}

/// Recovery time of a 128 MiB segment (ADR 0035, "Cost"): the median of three opens of a fresh copy of each state.
/// Prints, doesn't assert. It writes about 2 GB, so it only runs when asked:
/// `TICKERTAPE_RECOVERY_TIMING=1 cargo test -p tickertape-tests [--release] --test committed_tail -- --ignored
/// --nocapture recovery_time`.
#[test]
#[ignore = "timing: writes several 128 MiB segments; set TICKERTAPE_RECOVERY_TIMING=1"]
fn recovery_time_128_mib() {
    if std::env::var_os("TICKERTAPE_RECOVERY_TIMING").is_none() {
        eprintln!("recovery_time_128_mib: skipped; set TICKERTAPE_RECOVERY_TIMING=1 to run it");
        return;
    }
    let size = WriterConfig::DEFAULT_SEGMENT_SIZE;
    let config = || WriterConfig::default().segment_size(size).prefault(false);
    let payload = vec![0x5a_u8; 1000];
    let fill = |dir: &Path, records: u64| {
        let mut w = Writer::open(dir, config()).unwrap();
        for _ in 0..records {
            w.append(&tickertape::NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload: &payload,
            })
            .unwrap();
        }
    };
    // ~126 MiB of 1,040-byte records: the segment is nearly full.
    let full = (size - 1024 * 1024) / 1040;
    let torn = |dir: &Path| {
        fill(dir, 0);
        let (_, seg) = segments(dir).pop().unwrap();
        let body = vec![0xa5_u8; (format::max_payload_len(size) + 36) as usize];
        open_raw(&seg).write_all_at(&body, 68).unwrap();
    };
    let damaged = |dir: &Path| {
        fill(dir, full);
        zero_commit_word(dir, 1000);
    };
    type Case<'a> = (&'a str, &'a dyn Fn(&Path), RecoveryPolicy);
    let cases: [Case<'_>; 5] = [
        // The whole (never written) tail is read once to find it zero.
        (
            "empty segment, clean close",
            &|d| fill(d, 0),
            RecoveryPolicy::Quarantine,
        ),
        // Every record is validated (no checkpoint).
        (
            "full segment, clean close",
            &|d| fill(d, full),
            RecoveryPolicy::Quarantine,
        ),
        // About 128 MiB of uncommitted body after the end.
        (
            "torn append of the largest record",
            &torn,
            RecoveryPolicy::Quarantine,
        ),
        (
            "commit word of record 1000 zeroed, Fail",
            &damaged,
            RecoveryPolicy::Fail,
        ),
        (
            "commit word of record 1000 zeroed, Quarantine",
            &damaged,
            RecoveryPolicy::Quarantine,
        ),
    ];
    for (name, build, policy) in cases {
        let mut times = Vec::new();
        let mut what = String::new();
        for _ in 0..3 {
            let d = tempfile::tempdir().unwrap();
            build(d.path());
            let t = Instant::now();
            let w = Writer::open(d.path(), config().recovery(policy));
            times.push(t.elapsed().as_secs_f64() * 1e3);
            what = match &w {
                Ok(w) => format!(
                    "{:?}",
                    w.recovery_report()
                        .actions
                        .iter()
                        .map(|a| match a {
                            RecoveryAction::ZeroedUncommitted { len, .. } =>
                                format!("zeroed {len}"),
                            RecoveryAction::RepairedTail { len, .. } => format!("repaired {len}"),
                            other => format!("{other:?}"),
                        })
                        .collect::<Vec<_>>()
                ),
                Err(e) => format!("error: {e}"),
            };
        }
        times.sort_by(f64::total_cmp);
        eprintln!(
            "recovery_time_128_mib: {name}: median {:.0} ms (min {:.0}, max {:.0}) {what}",
            times[1], times[0], times[2]
        );
    }
}
