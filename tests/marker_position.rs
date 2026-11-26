//! Recovery and a checkpoint marker whose position the records of its segment do not reach (ADR 0056, issue #87):
//! `docs/format.md` → Recovery, step 2; crash contract C11 and "Missing segments".
//!
//! The marker vouches for every record below its `next_seq`. When the segment it names ends before the marker's
//! position (segments restored from a backup older than the marker), or the record right before that position is
//! damaged, or `next_seq` disagrees with that record, the writer refuses to open under every policy and changes
//! nothing. Every state that a working writer and a crash leave must still open; the sweeps at the end check that for
//! every position a writer can give a marker and for every cut or torn slot after the marker.

use std::fs;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

use tickertape::fault;
use tickertape::format::{self, CheckpointSlot};
use tickertape::{CorruptReason, Error, NewRecord, RecoveryAction, RecoveryPolicy, Writer};
use tickertape_tests::{
    SMALL_SEGMENT, append_n, check_record, locate_record, open_raw, read_all, segments,
    small_config,
};

const POLICIES: [RecoveryPolicy; 2] = [RecoveryPolicy::Quarantine, RecoveryPolicy::Fail];

/// The segments and the marker file of `dir`, with their contents: what a refused open must leave unchanged.
fn journal_files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.is_file()
                && (p.extension().is_some_and(|e| e == "tape")
                    || p.file_name().unwrap() == "checkpoint")
        })
        .map(|p| {
            let bytes = fs::read(&p).unwrap();
            (p, bytes)
        })
        .collect();
    files.sort();
    files
}

/// A new directory holding a copy of the segments and the marker file of `from`.
fn copy_journal(from: &Path) -> tempfile::TempDir {
    let to = tempfile::tempdir().unwrap();
    for (path, bytes) in journal_files(from) {
        fs::write(to.path().join(path.file_name().unwrap()), bytes).unwrap();
    }
    to
}

/// Replaces the segments of `dir` by those of `backup`, keeping `dir`'s marker: the operator mistake of issue #87.
fn restore_segments_only(dir: &Path, backup: &Path) {
    for (_, path) in segments(dir) {
        fs::remove_file(path).unwrap();
    }
    for (_, path) in segments(backup) {
        fs::copy(&path, dir.join(path.file_name().unwrap())).unwrap();
    }
}

fn marker(dir: &Path) -> CheckpointSlot {
    tickertape::inspect::read_checkpoint(dir)
        .unwrap()
        .expect("a current checkpoint")
}

/// Writes `slot` into its slot of the marker file, as a writer's checkpoint would.
fn write_slot(dir: &Path, slot: &CheckpointSlot) {
    let at = format::CHECKPOINT_SLOT_OFFSETS[(slot.generation % 2) as usize] as u64;
    open_raw(&dir.join("checkpoint"))
        .write_all_at(&format::encode_checkpoint_slot(slot), at)
        .unwrap();
}

/// Writes a slot with the next generation, naming `segment_base_seq`, `offset` and `next_seq`.
fn forge(dir: &Path, segment_base_seq: u64, offset: u64, next_seq: u64) {
    let generation = tickertape::inspect::read_checkpoint(dir)
        .unwrap()
        .map_or(1, |c| c.generation + 1);
    write_slot(
        dir,
        &CheckpointSlot {
            generation,
            next_seq,
            segment_base_seq,
            offset,
        },
    );
}

/// Damages the slot of the current checkpoint (a bit of its CRC), so that the other slot becomes current.
fn tear_current_slot(dir: &Path) {
    let c = marker(dir);
    let at = format::CHECKPOINT_SLOT_OFFSETS[(c.generation % 2) as usize] as u64 + 60;
    let file = open_raw(&dir.join("checkpoint"));
    let mut byte = [0u8; 1];
    file.read_exact_at(&mut byte, at).unwrap();
    file.write_all_at(&[byte[0] ^ 1], at).unwrap();
}

/// Opens `dir` under each policy, expects the ADR 0056 refusal at `segment` and `offset`, and checks that the open
/// changed neither a segment nor the marker.
fn assert_refused(dir: &Path, segment: &Path, offset: u64, base: u64) {
    let before = journal_files(dir);
    for policy in POLICIES {
        match Writer::open(dir, small_config().recovery(policy)) {
            Err(Error::Corrupt {
                path,
                offset: at,
                reason: CorruptReason::DanglingCheckpoint(b),
                ..
            }) => {
                assert_eq!(
                    (path.as_path(), at, b),
                    (segment, offset, base),
                    "{policy:?}"
                );
            }
            Err(other) => panic!("{policy:?}: expected DanglingCheckpoint, got {other}"),
            Ok(w) => panic!(
                "{policy:?}: the open succeeded at {}: {:?}",
                w.next_seq(),
                w.recovery_report()
            ),
        }
        assert_eq!(journal_files(dir), before, "{policy:?} changed the journal");
    }
}

/// Opens `dir` under `policy` and returns the writer, or panics with the error.
fn opens(dir: &Path, policy: RecoveryPolicy) -> Writer {
    Writer::open(dir, small_config().recovery(policy))
        .unwrap_or_else(|e| panic!("{policy:?}: the open was refused: {e}"))
}

fn assert_records(dir: &Path, n: u64) {
    let records = read_all(dir);
    assert_eq!(records.len() as u64, n);
    for (i, r) in records.iter().enumerate() {
        assert_eq!(r.seq, i as u64);
        check_record(r);
    }
}

/// Issue #87: the segments restored from a backup, the newer marker kept. The writer refuses, under both policies,
/// and changes nothing; restoring the marker that belongs to the segments, or moving the marker aside, lets it open
/// at the end of the restored records.
#[test]
fn restored_segments_with_a_newer_marker_are_refused() {
    // In the first segment, and in a later one (the restored journal ends in the marker's segment either way).
    for (backed_up, more) in [(100, 150), (450, 100)] {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut w, backed_up);
        w.checkpoint().unwrap();
        drop(w);
        let backup = copy_journal(dir.path());
        let mut w = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut w, more);
        w.checkpoint().unwrap();
        drop(w);
        let newer = marker(dir.path());
        assert_eq!(newer.next_seq, backed_up + more);
        assert_eq!(
            segments(dir.path()).len(),
            segments(backup.path()).len(),
            "the marker's segment is in the backup"
        );

        restore_segments_only(dir.path(), backup.path());
        let segment = dir
            .path()
            .join(format::segment_file_name(newer.segment_base_seq));
        assert_refused(dir.path(), &segment, newer.offset, newer.segment_base_seq);
        // `verify` names where the restored records end, below what the marker covers (ADR 0050).
        let report = tickertape::inspect::verify(dir.path()).unwrap();
        assert_eq!(report.last_seq, Some(backed_up - 1));
        assert!(
            report.problems.iter().any(|p| p.path == segment
                && p.reason == CorruptReason::EmptyCommitWord(backed_up + more)),
            "{:?}",
            report.problems
        );

        // Remedy 1: restore the marker that belongs to the segments.
        let one = copy_journal(dir.path());
        fs::copy(
            backup.path().join("checkpoint"),
            one.path().join("checkpoint"),
        )
        .unwrap();
        for policy in POLICIES {
            let w = opens(one.path(), policy);
            assert_eq!(w.next_seq(), backed_up);
            assert!(w.recovery_report().actions.is_empty());
        }
        assert_records(one.path(), backed_up);

        // Remedy 2: move the marker aside; the writer validates from the oldest segment.
        let two = copy_journal(dir.path());
        fs::rename(
            two.path().join("checkpoint"),
            two.path().join("checkpoint.newer"),
        )
        .unwrap();
        for policy in POLICIES {
            let w = opens(two.path(), policy);
            assert_eq!(w.next_seq(), backed_up);
            assert!(!w.recovery_report().had_checkpoint);
        }
        assert_records(two.path(), backed_up);
    }
}

/// Storage damage to the last record the marker covers (its commit word zeroed, or a byte of it changed) used to be
/// trusted, and the writer appended after the marker with the hole below it. Now the open is refused. Damage to an
/// earlier covered record is still not looked at (recovery starts at the marker): those journals open as before.
#[test]
fn damage_to_the_last_covered_record_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 600);
    w.checkpoint().unwrap();
    append_n(&mut w, 20);
    drop(w);
    let c = marker(dir.path());
    assert_eq!(c.next_seq, 600);
    let (path, offset) = locate_record(dir.path(), 599);
    assert_eq!(
        path,
        dir.path()
            .join(format::segment_file_name(c.segment_base_seq))
    );

    type Damage = fn(&Path, u64);
    let damages: [(&str, Damage); 3] = [
        ("commit word zeroed", |p, at| {
            open_raw(p).write_all_at(&[0; 4], at).unwrap()
        }),
        ("checksum changed", |p, at| {
            open_raw(p).write_all_at(&[0xff], at + 5).unwrap()
        }),
        ("whole record zeroed", |p, at| {
            open_raw(p).write_all_at(&[0; 40], at).unwrap()
        }),
    ];
    for (name, damage) in damages {
        let copy = copy_journal(dir.path());
        let seg = copy.path().join(path.file_name().unwrap());
        damage(&seg, offset);
        eprintln!("{name}");
        assert_refused(copy.path(), &seg, c.offset, c.segment_base_seq);
    }

    // Earlier covered records: in the marker's segment, and in the segment before it. Unchanged from 1.0.0.
    let first_of_segment = c.segment_base_seq;
    for seq in [598, first_of_segment, first_of_segment - 1, 10] {
        let copy = copy_journal(dir.path());
        let (p, at) = locate_record(copy.path(), seq);
        open_raw(&p).write_all_at(&[0; 4], at).unwrap();
        for policy in POLICIES {
            let w = opens(copy.path(), policy);
            assert_eq!(w.next_seq(), 620, "damage to record {seq}");
            assert_eq!(w.recovery_report().scanned_from_seq, 600);
        }
    }
}

/// A slot whose `next_seq` disagrees with the record before its offset, or that names offset 64 with a `next_seq`
/// other than the segment's base, is refused. 1.0.0 opened the first kind and gave the writer a sequence number the
/// journal already used, or quarantined the records after the marker as out of sequence.
#[test]
fn a_marker_disagreeing_with_the_record_before_it_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 450);
    w.checkpoint().unwrap();
    append_n(&mut w, 10);
    drop(w);
    let c = marker(dir.path());
    let seg = dir
        .path()
        .join(format::segment_file_name(c.segment_base_seq));
    let (_, inside) = locate_record(dir.path(), 445);
    let cases = [
        ("next_seq one too high", c.offset, c.next_seq + 1),
        ("next_seq one too low", c.offset, c.next_seq - 1),
        (
            "offset 64, next_seq above the base",
            64,
            c.segment_base_seq + 1,
        ),
        ("an offset inside a record", inside + 8, 446),
        ("the start of record 445 with next_seq 450", inside, 450),
    ];
    for (name, offset, next_seq) in cases {
        let copy = copy_journal(dir.path());
        forge(copy.path(), c.segment_base_seq, offset, next_seq);
        eprintln!("{name}");
        let seg = copy.path().join(seg.file_name().unwrap());
        assert_refused(copy.path(), &seg, offset, c.segment_base_seq);
    }
    // The marker set back to the start of its segment, or to an earlier record boundary, is consistent and opens.
    let (_, at_440) = locate_record(dir.path(), 440);
    for (offset, next_seq) in [(64, c.segment_base_seq), (at_440, 440)] {
        let copy = copy_journal(dir.path());
        forge(copy.path(), c.segment_base_seq, offset, next_seq);
        for policy in POLICIES {
            let w = opens(copy.path(), policy);
            assert_eq!(w.next_seq(), 460);
            assert_eq!(w.recovery_report().scanned_from_seq, next_seq);
        }
    }
}

/// Both slots. A torn newer slot makes the older current, whose records were durable before it: it opens. A newer
/// slot that its segment's records do not reach is refused even when the older slot would be consistent: recovery does
/// not fall back to it (ADR 0056, option 3). A newer slot that is consistent opens whatever the older says.
#[test]
fn both_slots() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 100);
    w.checkpoint().unwrap();
    append_n(&mut w, 50);
    drop(w);
    let backup = copy_journal(dir.path());
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 50);
    w.checkpoint().unwrap();
    drop(w);
    let (older, newer) = (marker(backup.path()), marker(dir.path()));
    assert_eq!(
        (older.next_seq, newer.next_seq, newer.generation),
        (100, 200, 2)
    );

    // The newer slot torn: the older one is current and consistent with the full journal.
    let torn = copy_journal(dir.path());
    tear_current_slot(torn.path());
    assert_eq!(marker(torn.path()), older);
    for policy in POLICIES {
        let w = opens(torn.path(), policy);
        assert_eq!(w.recovery_report().scanned_from_seq, 100);
        assert_eq!(w.next_seq(), 200);
    }

    // Segments restored from between the two checkpoints: the newer slot is beyond the records, the older is not.
    restore_segments_only(dir.path(), backup.path());
    let seg = dir
        .path()
        .join(format::segment_file_name(newer.segment_base_seq));
    assert_refused(dir.path(), &seg, newer.offset, newer.segment_base_seq);
    // With the newer slot torn as well, the older one is current, and the journal opens at the restored end.
    tear_current_slot(dir.path());
    for policy in POLICIES {
        let w = opens(dir.path(), policy);
        assert_eq!(w.recovery_report().scanned_from_seq, 100);
        assert_eq!(w.next_seq(), 150);
    }

    // A consistent newer slot with an older slot that the records do not reach: only the current slot counts.
    let other = copy_journal(backup.path());
    forge(other.path(), 0, 64 + 100 * 8 * 100, 999); // generation 2, slot 0: nonsense
    let nonsense = marker(other.path());
    let (_, at_120) = locate_record(other.path(), 120);
    forge(other.path(), 0, at_120, 120); // generation 3, slot 1
    assert_eq!(marker(other.path()).generation, nonsense.generation + 1);
    for policy in POLICIES {
        let w = opens(other.path(), policy);
        assert_eq!(w.recovery_report().scanned_from_seq, 120);
        assert_eq!(w.next_seq(), 150);
    }
}

/// Appends records of 56-byte frames until the first segment's record area is full: 1,169 of them reach offset
/// 65,528 (`SMALL_SEGMENT - 8`), where the next append rolls.
fn fill_first_segment(dir: &Path, w: &mut Writer) -> u64 {
    const FULL: u64 = (SMALL_SEGMENT - 64 - 8) / 56;
    while w.next_seq() < FULL {
        w.append(&NewRecord {
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: &[0x5a; 16],
        })
        .unwrap();
    }
    assert_eq!(segments(dir).len(), 1);
    FULL
}

/// The states around a roll, each a state a writer and a crash leave, with the marker at the old segment's end
/// (a checkpoint right before the roll) or at offset 64 of the new segment: all open under both policies.
#[test]
fn markers_around_a_roll_open() {
    let base = tempfile::tempdir().unwrap();
    let mut w = Writer::open(base.path(), small_config()).unwrap();
    let full = fill_first_segment(base.path(), &mut w);
    w.checkpoint().unwrap();
    drop(w);
    let c = marker(base.path());
    assert_eq!(
        (c.segment_base_seq, c.offset, c.next_seq),
        (0, SMALL_SEGMENT - 8, full)
    );

    // The writer stopped before the roll.
    let before_roll = copy_journal(base.path());
    for policy in POLICIES {
        assert_eq!(opens(before_roll.path(), policy).next_seq(), full);
    }

    // The roll completed: successor linked, seal written, one record in the successor.
    let rolled = copy_journal(base.path());
    let mut w = Writer::open(rolled.path(), small_config()).unwrap();
    append_n(&mut w, 1);
    drop(w);
    assert_eq!(segments(rolled.path()).len(), 2);
    let first = segments(rolled.path())[0].1.clone();
    let seal_at = c.offset;
    for policy in POLICIES {
        assert_eq!(opens(rolled.path(), policy).next_seq(), full + 1);
    }

    // Crash between linking the successor and sealing: the seal is missing.
    let unsealed = copy_journal(rolled.path());
    open_raw(&unsealed.path().join(first.file_name().unwrap()))
        .write_all_at(&[0; 8], seal_at)
        .unwrap();
    for policy in POLICIES {
        let copy = copy_journal(unsealed.path());
        let w = opens(copy.path(), policy);
        assert_eq!(w.next_seq(), full + 1);
        assert!(
            w.recovery_report()
                .actions
                .iter()
                .any(|a| matches!(a, RecoveryAction::Sealed { .. }))
        );
    }

    // Power loss that kept the seal and lost the successor: recovery creates it again; a checkpoint right after
    // that open names offset 64 of the new segment, and the next open accepts it.
    let lost = copy_journal(rolled.path());
    fs::remove_file(&segments(lost.path())[1].1).unwrap();
    for policy in POLICIES {
        let copy = copy_journal(lost.path());
        let mut w = opens(copy.path(), policy);
        assert_eq!(w.next_seq(), full);
        assert!(
            w.recovery_report()
                .actions
                .iter()
                .any(|a| matches!(a, RecoveryAction::CompletedRoll { .. }))
        );
        w.checkpoint().unwrap();
        drop(w);
        let at_new = marker(copy.path());
        assert_eq!(
            (at_new.segment_base_seq, at_new.offset, at_new.next_seq),
            (full, 64, full)
        );
        for p in POLICIES {
            assert_eq!(opens(copy.path(), p).next_seq(), full);
        }
    }

    // Neither the seal nor the successor reached the disk.
    let neither = copy_journal(unsealed.path());
    fs::remove_file(&segments(neither.path())[1].1).unwrap();
    for policy in POLICIES {
        assert_eq!(opens(neither.path(), policy).next_seq(), full);
    }
}

/// A repair by replacement copies the records before the marker into a new file and renames it over the segment. The
/// copy is on the drive (the platform barrier: `F_FULLFSYNC` on macOS) before the rename: otherwise a power loss right
/// after the rename could keep the new name and lose the copied bytes, and the next open would refuse the marker
/// although nothing outside the contract happened. The open is stopped right before the rename, and the syncs it made
/// until then must include a barrier on the copy.
#[test]
fn a_repair_by_replacement_is_on_the_drive_before_its_rename() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 450);
    w.checkpoint().unwrap();
    append_n(&mut w, 20);
    drop(w);
    let c = marker(dir.path());
    tickertape_tests::corrupt_by_replacement(dir.path(), 455);
    assert_eq!(
        locate_record(dir.path(), 454).0,
        dir.path()
            .join(format::segment_file_name(c.segment_base_seq))
    );

    fault::record_syncs();
    fault::fail_next(fault::Point::RepairBeforeRename, 5 /* EIO */);
    let refused = Writer::open(dir.path(), small_config());
    let syncs = fault::take_syncs();
    fault::clear();
    assert!(matches!(refused, Err(Error::Io { .. })), "{refused:?}");
    let copy = dir.path().join(format!(
        "{}.tmp",
        format::segment_file_name(c.segment_base_seq)
    ));
    let meta = fs::metadata(&copy).expect("the repaired copy, not yet renamed");
    let id = (meta.dev(), meta.ino());
    assert!(
        syncs.contains(&(fault::Sync::Barrier, id)),
        "no barrier on the copy before the rename: {syncs:?}"
    );

    // The next open removes the copy and repairs again, and the journal opens at the damage.
    assert_eq!(
        opens(dir.path(), RecoveryPolicy::Quarantine).next_seq(),
        455
    );
}

/// The same for a segment recovery creates (a new journal's first segment, or a successor after a lost roll): its
/// header is on the drive before the link publishes it. A new journal's first segment whose header a power loss kept
/// from the disk would make every later open fail (recovery never restores the header of the segment it starts in).
#[test]
fn a_segment_recovery_creates_is_on_the_drive_before_its_link() {
    let dir = tempfile::tempdir().unwrap();
    fault::record_syncs();
    fault::fail_next(fault::Point::Link, 5 /* EIO */);
    let refused = Writer::open(dir.path(), small_config());
    let syncs = fault::take_syncs();
    fault::clear();
    assert!(matches!(refused, Err(Error::Io { .. })), "{refused:?}");
    let meta = fs::metadata(dir.path().join("checkpoint")).unwrap();
    let marker_id = (meta.dev(), meta.ino());
    assert!(
        syncs
            .iter()
            .any(|&(sync, id)| sync == fault::Sync::Barrier && id != marker_id && id.1 != 0),
        "no barrier on the new segment before its link: {syncs:?}"
    );
    assert!(segments(dir.path()).is_empty());
    assert_eq!(opens(dir.path(), RecoveryPolicy::Fail).next_seq(), 0);
}

/// The other states that look like a contradiction and are not: the marker's own segment cut short only after the
/// marker, a repair in place and a repair by replacement after it (each reopened, checkpointed and reopened), a short
/// marker file, and a long last covered record.
#[test]
fn other_legitimate_states_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 450);
    w.checkpoint().unwrap();
    append_n(&mut w, 20);
    drop(w);

    // The commit words of the uncheckpointed records zeroed from the newest back: a repair in place.
    let in_place = copy_journal(dir.path());
    tickertape_tests::zero_commit_words_from(in_place.path(), 460);
    // A record after the marker made corrupt: a repair by replacement under `Quarantine`.
    let replaced = copy_journal(dir.path());
    tickertape_tests::corrupt_by_replacement(replaced.path(), 455);
    for (dir, next) in [(in_place.path(), 460), (replaced.path(), 455)] {
        let mut w = opens(dir, RecoveryPolicy::Quarantine);
        assert_eq!(w.next_seq(), next);
        assert!(!w.recovery_report().actions.is_empty());
        append_n(&mut w, 3);
        w.checkpoint().unwrap();
        drop(w);
        for policy in POLICIES {
            let w = opens(dir, policy);
            assert_eq!(w.next_seq(), next + 3);
            assert!(w.recovery_report().actions.is_empty());
        }
        assert_records(dir, next + 3);
    }

    // A marker file cut to its first slot, which is the current one (generation 2).
    let short = tempfile::tempdir().unwrap();
    let mut w = Writer::open(short.path(), small_config()).unwrap();
    append_n(&mut w, 10);
    w.checkpoint().unwrap();
    append_n(&mut w, 10);
    w.checkpoint().unwrap();
    drop(w);
    assert_eq!(marker(short.path()).generation, 2);
    fs::OpenOptions::new()
        .write(true)
        .open(short.path().join("checkpoint"))
        .unwrap()
        .set_len(64)
        .unwrap();
    for policy in POLICIES {
        assert_eq!(opens(short.path(), policy).next_seq(), 20);
    }

    // The last covered record nearly fills its segment: the probe reads back across its whole payload.
    let long = tempfile::tempdir().unwrap();
    let mut w = Writer::open(long.path(), small_config()).unwrap();
    append_n(&mut w, 3);
    let max = format::max_payload_len(SMALL_SEGMENT) as usize;
    w.append(&NewRecord {
        schema_id: 0,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload: &vec![0x11; max],
    })
    .unwrap();
    w.checkpoint().unwrap();
    drop(w);
    let c = marker(long.path());
    // Records 0 to 2 in the first segment; the long record 3 rolled into its own segment and ends at its trailer.
    assert_eq!(
        (c.segment_base_seq, c.offset, c.next_seq),
        (3, SMALL_SEGMENT - 8, 4)
    );
    for policy in POLICIES {
        assert_eq!(opens(long.path(), policy).next_seq(), 4);
    }
}

/// The record and seal positions of each segment of `dir`, in chain order: `(base, offset, next_seq)` for every
/// record start, every seal, and the end of the newest segment's records.
fn slot_positions(dir: &Path) -> Vec<(u64, u64, u64)> {
    let mut out = Vec::new();
    for (base, path) in segments(dir) {
        let image = fs::read(&path).unwrap();
        let (mut offset, mut seq) = (format::SEGMENT_HEADER_LEN as u64, base);
        loop {
            out.push((base, offset, seq));
            match format::read_slot(&image[..], offset, seq) {
                Ok(format::Slot::Record { next_offset, .. }) => {
                    offset = next_offset;
                    seq += 1;
                }
                _ => break,
            }
        }
    }
    out
}

/// Appends `n` records of 400 to 1,399 bytes (about 68 to a 64 KiB segment), so that a sweep over every slot of a
/// journal of several segments stays short.
fn append_large(w: &mut Writer, n: u64) {
    for _ in 0..n {
        let seq = w.next_seq();
        let payload = tickertape_tests::payload(seq, 400 + ((seq * 389) % 1000) as usize);
        w.append(&NewRecord {
            schema_id: 2,
            venue_ts_ns: seq,
            local_ts_ns: seq,
            payload: &payload,
        })
        .unwrap();
    }
}

/// Runs [`every_position_a_writer_can_give_a_marker_opens`] on every `stride`-th position (and every offset 64).
fn marker_positions(stride: usize) {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 300);
    append_large(&mut w, 150);
    append_n(&mut w, 300);
    let end = w.next_seq();
    drop(w);
    let positions = slot_positions(dir.path());
    assert!(segments(dir.path()).len() >= 4);
    let mut opened = 0;
    for (i, &(base, offset, next_seq)) in positions.iter().enumerate() {
        if i % stride != 0 && offset != 64 {
            continue;
        }
        let copy = copy_journal(dir.path());
        forge(copy.path(), base, offset, next_seq);
        for policy in POLICIES {
            let w = opens(copy.path(), policy);
            assert_eq!(
                w.next_seq(),
                end,
                "marker at {base}/{offset} for {next_seq}"
            );
            assert_eq!(w.recovery_report().scanned_from_seq, next_seq);
            assert!(w.recovery_report().actions.is_empty());
            opened += 1;
        }
    }
    eprintln!(
        "marker positions: {opened} opens, {} positions in {} segments, stride {stride}",
        positions.len(),
        segments(dir.path()).len()
    );
}

/// Every position a writer can give a marker (each record boundary of every segment, a seal's place, and offset 64
/// of each segment with its base) is accepted: a marker forged at each, as the current slot, opens under both
/// policies at the end of the journal, and recovery does nothing. The positions are the record boundaries of a finished
/// journal, not markers a writer wrote; they cover what a writer can produce, because a checkpoint always writes the
/// writer's current segment, offset and next sequence number, which is such a boundary (or offset 64 of a segment it
/// has just created). `tests/kill_loop.rs` and `tests/soak.rs` reopen journals with markers the writer wrote itself. A sample here; every position in
/// [`every_position_a_writer_can_give_a_marker_opens_all`].
#[test]
fn every_position_a_writer_can_give_a_marker_opens() {
    marker_positions(if cfg!(debug_assertions) { 23 } else { 5 });
}

#[test]
#[ignore = "long: every position; run in release with --ignored"]
fn every_position_a_writer_can_give_a_marker_opens_all() {
    marker_positions(1);
}

/// Runs [`no_crash_state_after_the_marker_is_refused`] on every `stride`-th slot.
fn crash_states(stride: usize) {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_large(&mut w, 40);
    w.checkpoint().unwrap();
    append_large(&mut w, 40);
    w.checkpoint().unwrap();
    append_large(&mut w, 170);
    drop(w);
    let newer = marker(dir.path());
    assert_eq!(newer.next_seq, 80);
    assert!(segments(dir.path()).len() >= 4);
    let after: Vec<_> = slot_positions(dir.path())
        .into_iter()
        .filter(|&(_, _, seq)| seq >= 40)
        .collect();
    let (mut states, mut opened, mut fail_refusals) = (0, 0, 0);
    for (i, &(base, offset, seq)) in after.iter().enumerate() {
        if i % stride != 0 {
            continue;
        }
        for state in ["cut", "cut, later segments kept", "commit word zeroed"] {
            for torn in [false, true] {
                // With the newer slot current, only the slots at or after its position are after the marker.
                if !torn && seq < newer.next_seq {
                    continue;
                }
                let copy = copy_journal(dir.path());
                let segs = segments(copy.path());
                let path = &segs.iter().find(|(b, _)| *b == base).unwrap().1;
                if state == "commit word zeroed" {
                    open_raw(path).write_all_at(&[0; 4], offset).unwrap();
                } else {
                    let len = SMALL_SEGMENT - offset;
                    open_raw(path)
                        .write_all_at(&vec![0; len as usize], offset)
                        .unwrap();
                    if state == "cut" {
                        for (b, p) in &segs {
                            if *b > base {
                                fs::remove_file(p).unwrap();
                            }
                        }
                    }
                }
                if torn {
                    tear_current_slot(copy.path());
                }
                states += 1;
                let what =
                    format!("{state} at {base}/{offset} (seq {seq}), newer slot torn: {torn}");
                for policy in POLICIES {
                    let run = copy_journal(copy.path());
                    match Writer::open(run.path(), small_config().recovery(policy)) {
                        Ok(_) => opened += 1,
                        Err(Error::Corrupt {
                            reason: CorruptReason::DanglingCheckpoint(_),
                            ..
                        }) => panic!("{what}, {policy:?}: refused as a dangling marker"),
                        Err(e @ Error::Corrupt { .. }) if policy == RecoveryPolicy::Fail => {
                            // Damage that `Fail` refused before: a hidden committed tail or a disconnected segment.
                            assert!(state != "cut", "{what}: {e}");
                            fail_refusals += 1;
                        }
                        Err(e) => panic!("{what}, {policy:?}: {e}"),
                    }
                }
            }
        }
    }
    eprintln!(
        "crash states: {states} states from {} slots (stride {stride}), {opened} opens, {fail_refusals} refusals \
         under Fail for damage it refused before, none as a dangling marker",
        after.len()
    );
}

/// The crash and damage states after the marker, at the slots of every segment of the recovery domain: the journal cut
/// at a slot (it and everything after it in its segment zeroed, later segments removed: a power loss), cut with the
/// later segments kept, and the slot's commit word alone zeroed; each with the newer marker slot current, and with it
/// torn (the older one current, so that the slots between the two markers are after the marker too). The ADR 0056
/// check refuses none of them; `Quarantine` opens every one, and `Fail` refuses only damage it refused before (never
/// a cut, never `DanglingCheckpoint`). A sample here; every slot in
/// [`no_crash_state_after_the_marker_is_refused_at_every_slot`].
#[test]
fn no_crash_state_after_the_marker_is_refused() {
    crash_states(if cfg!(debug_assertions) { 17 } else { 4 });
}

#[test]
#[ignore = "long: every slot; run in release with --ignored"]
fn no_crash_state_after_the_marker_is_refused_at_every_slot() {
    crash_states(1);
}
