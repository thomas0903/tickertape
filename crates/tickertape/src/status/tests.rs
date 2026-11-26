//! Unit tests for `status`. No test here spawns a process (a child briefly inherits every descriptor of the test
//! binary, including other tests' locks): process-spawning tests are in `tests/status.rs` and
//! `tests/beacon_races.rs`. No test assumes how much wall-clock time passes between two calls.

use std::fs::{self, File};
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::time::SystemTime;

use super::beacon::{self, BEACON_LEN};
use super::*;
use crate::retention::{Action, Policy};
use crate::{NewRecord, ReaderConfig, Writer, WriterConfig};

const SEG: u64 = format::SEGMENT_SIZE_UNIT;
const HOUR: Duration = Duration::from_secs(3600);

fn config() -> WriterConfig {
    WriterConfig::default().segment_size(SEG).prefault(false)
}

fn append(writer: &mut Writer, n: u64, payload_len: usize, local_ts_ns: u64) {
    let payload = vec![0xA5; payload_len];
    for _ in 0..n {
        writer
            .append(&NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns,
                payload: &payload,
            })
            .unwrap();
    }
}

fn frame(payload_len: usize) -> u64 {
    format::align8(u64::from(format::frame_len(payload_len).unwrap()))
}

fn records_per_segment(payload_len: usize) -> u64 {
    (SEG - format::SEGMENT_HEADER_LEN as u64 - format::SEGMENT_TRAILER_LEN as u64)
        / frame(payload_len)
}

fn marker(dir: &Path) -> File {
    File::options()
        .write(true)
        .open(dir.join("checkpoint"))
        .unwrap()
}

/// Makes the checkpoint look `by` older: moves the marker's modification time back, and the creation time in the
/// header of the marker's segment too (a checkpoint can't predate its segment).
fn backdate_checkpoint(dir: &Path, base: u64, by: Duration) {
    let path = journal::segment_path(dir, base);
    let file = File::options().read(true).write(true).open(&path).unwrap();
    let mut header = journal::read_header(&file, &path, base).unwrap();
    header.created_unix_ns -= u64::try_from(by.as_nanos()).unwrap();
    file.write_all_at(&format::encode_segment_header(&header), 0)
        .unwrap();
    let marker = marker(dir);
    let modified = marker.metadata().unwrap().modified().unwrap();
    marker.set_modified(modified - by).unwrap();
}

fn caught_up(records: u64) -> Lag {
    Lag {
        records,
        bytes: Some(0),
    }
}

#[test]
fn missing_and_empty_directories_are_not_found() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(status(dir.path()), Err(Error::NotFound { .. })));
    assert!(matches!(
        status(dir.path().join("nope")),
        Err(Error::Io { .. })
    ));
}

#[test]
fn empty_journal() {
    let dir = tempfile::tempdir().unwrap();
    let writer = Writer::open(dir.path(), config()).unwrap();
    let s = status(dir.path()).unwrap();
    assert_eq!(s.head.position, Position::new(0, 0, 64));
    assert_eq!(s.head.end, HeadEnd::Open);
    assert_eq!(s.head.segment_size, SEG);
    assert_eq!(s.head.last_local_ts_ns, None);
    assert!(s.head.verified);
    assert_eq!(s.segments.count, 1);
    assert_eq!(s.segments.first_seq, 0);
    assert_eq!(s.segments.total_bytes, SEG);
    assert!(s.segments.oldest_created_unix_ns.is_some());
    assert_eq!(s.segments.before_checkpoint, 0);
    // The writer creates the marker with no valid slot.
    assert_eq!(s.checkpoint, None);
    assert_eq!(s.exposure.records, Some(0));
    assert_eq!(s.exposure.bytes, Some(0));
    assert_eq!(s.exposure.estimated_age, Some(Duration::ZERO));
    assert_eq!(s.exposure.oldest_local_ts_ns, None);
    assert!(s.exposure.verified);
    assert_eq!(s.rates, None);
    assert_eq!(s.storage.estimated_time_to_full, None);
    assert!(s.readers.is_empty());
    assert_eq!(s.readers_dir, dir.path().join("readers"));
    assert_eq!(s.readers_dir_state, BeaconDirState::Missing);
    assert_eq!(s.writer.pid, Some(std::process::id()));
    assert_eq!(s.writer.alive, Some(true));
    assert!(
        s.breaches(&Thresholds::default().max_checkpoint_age(Duration::ZERO))
            .is_empty()
    );
    drop(writer);
}

#[test]
fn no_checkpoint_exposes_everything() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 3, 10, 0);
    append(&mut writer, 2, 10, 77);
    let s = status(dir.path()).unwrap();
    assert_eq!(s.head.position, Position::new(5, 0, 64 + 5 * frame(10)));
    assert_eq!(s.head.last_local_ts_ns, Some(77));
    assert!(s.head.verified, "one segment, scanned from its start");
    assert_eq!(s.checkpoint, None);
    assert_eq!(s.exposure.records, Some(5));
    assert_eq!(s.exposure.bytes, Some(5 * frame(10)));
    assert_eq!(s.exposure.estimated_age, None);
    // The oldest uncovered record has no timestamp set.
    assert_eq!(s.exposure.oldest_local_ts_ns, None);
    let t = Thresholds::default().max_checkpoint_age(HOUR);
    assert_eq!(s.breaches(&t), vec![Breach::CheckpointAge]);
}

#[test]
fn checkpoint_age_and_exposure_after_more_appends() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 4, 10, 5);
    writer.checkpoint().unwrap();
    let after_checkpoint = crate::clock::realtime_ns();
    let s = status(dir.path()).unwrap();
    let c = s.checkpoint.unwrap();
    assert_eq!(c.marker.next_seq, 4);
    assert_eq!(c.marker.generation, 1);
    assert_eq!(c.problem, None);
    assert_eq!(c.age_unknown, None);
    assert!(c.modified_unix_ns.unwrap() <= after_checkpoint);
    assert!(c.modified_unix_ns.unwrap() <= s.sampled_unix_ns);
    assert!(c.estimated_age.is_some());
    assert_eq!(s.exposure.records, Some(0));
    assert_eq!(s.exposure.estimated_age, Some(Duration::ZERO));

    append(&mut writer, 3, 10, 9);
    let s = status(dir.path()).unwrap();
    assert!(s.head.verified, "the scan started at the checkpoint");
    assert_eq!(s.exposure.records, Some(3));
    assert_eq!(s.exposure.bytes, Some(3 * frame(10)));
    assert_eq!(s.exposure.oldest_local_ts_ns, Some(9));
    assert_eq!(
        s.exposure.estimated_age,
        s.checkpoint.unwrap().estimated_age
    );
    // Make the checkpoint an hour old rather than waiting.
    backdate_checkpoint(dir.path(), 0, HOUR);
    let s = status(dir.path()).unwrap();
    assert!(s.checkpoint.unwrap().estimated_age.unwrap() >= HOUR);
    assert!(
        s.breaches(&Thresholds::default().max_checkpoint_age(2 * HOUR))
            .is_empty()
    );
    assert_eq!(
        s.breaches(&Thresholds::default().max_checkpoint_age(HOUR / 2)),
        vec![Breach::CheckpointAge]
    );
}

/// Finding 12: a modification time that can't be right gives an unknown age, which breaches when records are
/// uncovered.
#[test]
fn unreliable_checkpoint_times_are_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 2, 10, 0);
    writer.checkpoint().unwrap();
    // Generation 2 is in the slot at offset 0, which survives the cut below.
    writer.checkpoint().unwrap();
    append(&mut writer, 1, 10, 0);
    let t = Thresholds::default().max_checkpoint_age(HOUR);

    // In the future.
    marker(dir.path())
        .set_modified(SystemTime::now() + HOUR)
        .unwrap();
    let s = status(dir.path()).unwrap();
    let c = s.checkpoint.unwrap();
    assert_eq!(c.age_unknown, Some(AgeUnknown::InFuture));
    assert_eq!(c.estimated_age, None);
    assert_eq!(s.exposure.estimated_age, None);
    assert_eq!(s.breaches(&t), vec![Breach::CheckpointAge]);
    assert_eq!(
        s.breaches(&Thresholds::default().max_checkpoint_age(Duration::ZERO)),
        vec![Breach::CheckpointAge]
    );

    // Earlier than its segment's creation.
    marker(dir.path())
        .set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000))
        .unwrap();
    let s = status(dir.path()).unwrap();
    assert_eq!(
        s.checkpoint.unwrap().age_unknown,
        Some(AgeUnknown::BeforeSegment)
    );
    assert_eq!(s.breaches(&t), vec![Breach::CheckpointAge]);

    // Cut short (the slot at 0 still holds generation 2; the writer always writes the full length).
    drop(writer);
    marker(dir.path()).set_len(4096).unwrap();
    let s = status(dir.path()).unwrap();
    let c = s.checkpoint.unwrap();
    assert_eq!(c.marker.generation, 2);
    assert_eq!(c.age_unknown, Some(AgeUnknown::MarkerResized));
    assert_eq!(s.breaches(&t), vec![Breach::CheckpointAge]);

    // Every record covered: an unknown age doesn't matter.
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    writer.checkpoint().unwrap();
    marker(dir.path())
        .set_modified(SystemTime::now() + HOUR)
        .unwrap();
    let s = status(dir.path()).unwrap();
    assert_eq!(s.exposure.records, Some(0));
    assert!(s.breaches(&t).is_empty());
}

/// Finding 12, round 2 B4, round 3 item 7: a short marker's age stays unknown until a checkpoint succeeds. The cut set
/// the marker's modification time to the time of the cut (not a checkpoint's). A writer opening the journal leaves it
/// short (it no longer extends it at open, which needed a timestamp sentinel), and its next checkpoint extends it after
/// the slot is durable, which makes the age known again. A marker that was cut and then extended by something else (a
/// v0.4.0 writer extends at open) is recognised by its contents: the older slot is lost.
#[test]
fn reopening_with_a_short_marker_keeps_the_age_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 2, 10, 0);
    writer.checkpoint().unwrap();
    // Generation 2 in the slot at offset 0, which survives the cut; one record uncovered.
    writer.checkpoint().unwrap();
    append(&mut writer, 1, 10, 0);
    drop(writer);
    backdate_checkpoint(dir.path(), 0, 3 * HOUR);
    let full = format::CHECKPOINT_FILE_LEN as u64;
    let t = Thresholds::default().max_checkpoint_age(HOUR);
    let s = status(dir.path()).unwrap();
    assert_eq!(s.checkpoint.unwrap().age_unknown, None);
    assert_eq!(s.breaches(&t), vec![Breach::CheckpointAge]);
    // The cut refreshes the modification time, as truncating a file does.
    marker(dir.path()).set_len(4096).unwrap();
    let unknown = |dir: &Path, reason: AgeUnknown, uncovered: u64| {
        let s = status(dir).unwrap();
        let c = s.checkpoint.unwrap();
        assert_eq!(c.marker.generation, 2);
        assert_eq!(c.estimated_age, None);
        assert_eq!(c.age_unknown, Some(reason));
        assert_eq!(s.exposure.records, Some(uncovered));
        assert_eq!(s.breaches(&t), vec![Breach::CheckpointAge]);
    };
    unknown(dir.path(), AgeUnknown::MarkerResized, 1);

    // Opening leaves the marker short.
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    assert_eq!(
        fs::metadata(dir.path().join("checkpoint")).unwrap().len(),
        4096
    );
    unknown(dir.path(), AgeUnknown::MarkerResized, 1);

    // The next checkpoint extends it after its slot is durable, and the age is known again.
    append(&mut writer, 1, 10, 0);
    writer.checkpoint().unwrap();
    append(&mut writer, 1, 10, 0);
    assert_eq!(
        fs::metadata(dir.path().join("checkpoint")).unwrap().len(),
        full
    );
    let s = status(dir.path()).unwrap();
    assert_eq!(s.checkpoint.unwrap().marker.generation, 3);
    assert!(s.checkpoint.unwrap().estimated_age.unwrap() < HOUR);
    assert!(s.breaches(&t).is_empty());
    drop(writer);

    // Cut again (generation 3, in the slot at 65,536, is lost; generation 2 remains) and extended by something else:
    // full length, but the current slot's predecessor is missing from the other slot.
    marker(dir.path()).set_len(4096).unwrap();
    marker(dir.path()).set_len(full).unwrap();
    unknown(dir.path(), AgeUnknown::OlderSlotLost, 3);
    // A writer's next checkpoint (generation 3 again, in the other slot) restores both slots.
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    writer.checkpoint().unwrap();
    let s = status(dir.path()).unwrap();
    assert_eq!(s.checkpoint.unwrap().age_unknown, None);
    assert!(s.breaches(&t).is_empty());
}

#[test]
fn checkpoint_in_an_older_segment() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    let per = records_per_segment(1000);
    append(&mut writer, 2, 1000, 11);
    writer.checkpoint().unwrap();
    append(&mut writer, per, 1000, 12);
    let s = status(dir.path()).unwrap();
    assert_eq!(s.segments.count, 2);
    assert_eq!(s.head.position.segment_base_seq, per);
    assert_eq!(s.head.position.next_seq, per + 2);
    assert_eq!(s.exposure.records, Some(per));
    assert_eq!(s.exposure.oldest_local_ts_ns, Some(12));
    // The segment between the checkpoint and the head was not read.
    assert!(!s.head.verified);
    assert!(!s.exposure.verified);
    // Rest of segment 0 after the checkpoint, plus the head's offset in segment 1.
    let checkpoint_offset = 64 + 2 * frame(1000);
    assert_eq!(
        s.exposure.bytes,
        Some(SEG - checkpoint_offset + s.head.position.offset)
    );
    assert_eq!(s.segments.before_checkpoint, 0);
    writer.checkpoint().unwrap();
    let s = status(dir.path()).unwrap();
    assert_eq!(s.segments.before_checkpoint, 1);
    assert_eq!(s.exposure.records, Some(0));
    assert!(s.head.verified);
}

/// Finding 10: a marker that points to a missing segment, an impossible offset or a position its segment's records do not
/// reach is corruption, never healthy.
#[test]
fn dangling_checkpoint_is_corruption() {
    let per = records_per_segment(1000);
    let setup = || {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), config()).unwrap();
        append(&mut writer, 3 * per + 5, 1000, 0);
        writer.checkpoint().unwrap();
        append(&mut writer, 2, 1000, 0);
        drop(writer);
        dir
    };
    let check = |dir: &Path, problem: CheckpointProblem| {
        assert!(Writer::open(dir, config()).is_err(), "the writer refuses");
        let s = status(dir).unwrap();
        assert_eq!(s.checkpoint.unwrap().problem, Some(problem));
        assert_eq!(s.exposure.records, None);
        assert_eq!(s.exposure.bytes, None);
        assert_eq!(s.exposure.estimated_age, None);
        assert!(!s.head.verified);
        let breaches = s.breaches(&Thresholds::default());
        assert_eq!(breaches, vec![Breach::CheckpointInvalid]);
        assert!(breaches[0].is_corruption());
    };

    // The marker's segment (the newest) is gone: the head is now the seal of the one before.
    let dir = setup();
    fs::remove_file(journal::segment_path(dir.path(), 3 * per)).unwrap();
    check(dir.path(), CheckpointProblem::MissingSegment);

    // The marker's offset is outside its segment's record area.
    let dir = setup();
    let slot = CheckpointSlot {
        generation: 7,
        next_seq: 3 * per + 5,
        segment_base_seq: 3 * per,
        offset: SEG,
    };
    marker(dir.path())
        .write_all_at(&format::encode_checkpoint_slot(&slot), 65_536)
        .unwrap();
    check(dir.path(), CheckpointProblem::InvalidOffset);

    // The marker's segment header is damaged (it is not the newest here).
    let dir = setup();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, per, 1000, 0);
    drop(writer);
    let path = journal::segment_path(dir.path(), 3 * per);
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .write_all_at(&[0], 0)
        .unwrap();
    check(dir.path(), CheckpointProblem::InvalidSegmentHeader);

    // The last record the marker covers zeroed (ADR 0056): in the newest segment, and with the marker in an older one.
    for later in [0, per] {
        let dir = setup();
        let mut writer = Writer::open(dir.path(), config()).unwrap();
        append(&mut writer, later, 1000, 0);
        drop(writer);
        let c = crate::inspect::read_checkpoint(dir.path())
            .unwrap()
            .unwrap();
        let record = c.offset - format::align8((format::RECORD_HEADER_LEN + 1000) as u64);
        File::options()
            .write(true)
            .open(journal::segment_path(dir.path(), c.segment_base_seq))
            .unwrap()
            .write_all_at(&[0; 4], record)
            .unwrap();
        check(dir.path(), CheckpointProblem::Dangling);
    }
}

/// Finding 10: with the checkpoint in an older segment and a middle segment removed, the head and exposure are
/// reported as unverified (the writer refuses the journal, and a sample doesn't read the middle segments).
#[test]
fn missing_middle_segment_is_unverified() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    let per = records_per_segment(1000);
    append(&mut writer, 2, 1000, 0);
    writer.checkpoint().unwrap();
    append(&mut writer, 3 * per, 1000, 0);
    drop(writer);
    fs::remove_file(journal::segment_path(dir.path(), per)).unwrap();
    assert!(Writer::open(dir.path(), config()).is_err());
    let s = status(dir.path()).unwrap();
    assert!(!s.head.verified);
    assert!(!s.exposure.verified);
    assert_eq!(s.checkpoint.unwrap().problem, None);
    // Round 2, B2: not provable from the listing (the gap fits what a segment can hold), so not a breach: the sample
    // is unverified, which callers must not read as "the writer would open it".
    assert!(!s.head.chain_broken);
    assert!(s.breaches(&Thresholds::default()).is_empty());
}

#[test]
fn sealed_newest_segment_without_successor() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    let per = records_per_segment(1000);
    append(&mut writer, per + 1, 1000, 0);
    drop(writer);
    // As if the writer died between sealing and creating the successor: the newest segment is sealed.
    fs::remove_file(journal::segment_path(dir.path(), per)).unwrap();
    let s = status(dir.path()).unwrap();
    assert_eq!(s.head.end, HeadEnd::Sealed);
    assert_eq!(s.head.position.next_seq, per);
    assert!(s.breaches(&Thresholds::default()).is_empty());
}

#[test]
fn invalid_head_is_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 3, 10, 0);
    drop(writer);
    let path = journal::segment_path(dir.path(), 0);
    let file = File::options().write(true).open(&path).unwrap();
    // Flip a payload byte of the last record.
    let at = 64 + 2 * frame(10) + format::RECORD_HEADER_LEN as u64;
    file.write_all_at(&[0x5A], at).unwrap();
    let s = status(dir.path()).unwrap();
    assert_eq!(s.head.end, HeadEnd::Invalid(CorruptReason::BadChecksum));
    assert_eq!(s.head.position.next_seq, 2);
    let breaches = s.breaches(&Thresholds::default());
    assert_eq!(breaches, vec![Breach::HeadInvalid]);
    assert!(breaches[0].is_corruption());
    assert!(!Breach::CheckpointAge.is_corruption());
    // Round 3, item 4: the sample is unverified and the exposure unknown (before the fix: verified, 2 records), for
    // a one-shot sample and a sampler alike, with or without a checkpoint before the damage.
    let unverified = |s: &Status| {
        assert!(!s.head.verified);
        assert!(!s.exposure.verified);
        assert_eq!(s.exposure.records, None);
        assert_eq!(s.exposure.bytes, None);
        assert_eq!(s.exposure.estimated_age, None);
        assert_eq!(s.exposure.oldest_local_ts_ns, None);
    };
    unverified(&s);
    let mut sampler = Sampler::new(dir.path());
    unverified(&sampler.sample().unwrap());
    unverified(&sampler.sample().unwrap());
    // A checkpoint after the first record, in the head's segment: still unverified.
    file.write_all_at(&[0xA5], at).unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    writer.checkpoint().unwrap();
    append(&mut writer, 2, 10, 0);
    drop(writer);
    let at = 64 + 4 * frame(10) + format::RECORD_HEADER_LEN as u64;
    file.write_all_at(&[0x5A], at).unwrap();
    let s = status(dir.path()).unwrap();
    assert!(matches!(s.head.end, HeadEnd::Invalid(_)), "{:?}", s.head);
    assert!(s.checkpoint.unwrap().problem.is_none());
    unverified(&s);
}

#[test]
fn corrupt_newest_header_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let writer = Writer::open(dir.path(), config()).unwrap();
    drop(writer);
    let path = journal::segment_path(dir.path(), 0);
    let file = File::options().write(true).open(&path).unwrap();
    file.write_all_at(&[0], 0).unwrap();
    assert!(matches!(status(dir.path()), Err(Error::Corrupt { .. })));
}

/// Finding 13: the spare is reported but not counted as a roll; a sparse file is no spare; its size is the next
/// segment's size.
#[test]
fn spare_segment_and_headroom() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 2, 10, 0);
    drop(writer);
    // Files a crashed writer or a recovery in progress leaves behind: never segments.
    fs::write(dir.path().join("00000000000000000002.tape.tmp"), [1u8; 100]).unwrap();
    fs::create_dir(dir.path().join("quarantine")).unwrap();
    let s = status(dir.path()).unwrap();
    assert_eq!(s.segments.count, 1);
    assert_eq!(s.head.position.next_seq, 2);
    assert!(!s.storage.spare_segment);
    assert_eq!(s.storage.next_segment_size, SEG);
    assert!(s.storage.available_bytes <= s.storage.total_bytes);
    assert_eq!(
        s.storage.estimated_rolls_left,
        s.storage.available_bytes / SEG
    );

    // A sparse file of a valid segment size holds no space.
    let spare_path = dir.path().join(journal::SPARE_FILE);
    let spare = File::create(&spare_path).unwrap();
    spare.set_len(2 * SEG).unwrap();
    let s = status(dir.path()).unwrap();
    assert!(!s.storage.spare_segment, "a sparse file counted as a spare");
    // Its size is the configured size of the next segments.
    assert_eq!(s.storage.next_segment_size, 2 * SEG);
    assert_eq!(
        s.storage.estimated_rolls_left,
        s.storage.available_bytes / (2 * SEG)
    );

    // Preallocated: a spare, still not counted in the rolls.
    drop(spare);
    fs::remove_file(&spare_path).unwrap();
    let spare = File::create(&spare_path).unwrap();
    crate::mmap::preallocate(&spare, SEG).unwrap();
    let s = status(dir.path()).unwrap();
    assert!(s.storage.spare_segment);
    assert_eq!(
        s.storage.estimated_rolls_left,
        s.storage.available_bytes / SEG
    );
    let t = Thresholds::default()
        .min_headroom_bytes(u64::MAX)
        .min_headroom_segments(u64::MAX);
    assert_eq!(
        s.breaches(&t),
        vec![Breach::HeadroomBytes, Breach::HeadroomSegments]
    );
    assert!(
        s.breaches(
            &Thresholds::default()
                .min_headroom_bytes(1)
                .min_headroom_segments(0)
        )
        .is_empty()
    );
}

/// Finding 13: a sealed head has no writable bytes left in the time estimate.
#[test]
/// The estimate from fixed figures (round 3, item 10: two `statvfs` calls in a row could see different free space
/// while other tests wrote, which made this test fail once), plus the same relation through `statvfs` itself, checked
/// within a single reading.
fn time_to_full_of_a_sealed_head() {
    let rates = Rates {
        interval: Duration::from_secs(1),
        records_per_sec: 1.0,
        bytes_per_sec: Some(SEG as f64),
    };
    let mut head = Head {
        position: Position::new(5, 0, 64),
        segment_size: SEG,
        end: HeadEnd::Open,
        last_local_ts_ns: None,
        verified: true,
        chain_broken: false,
    };
    let left = (SEG - 64) as f64 / SEG as f64;
    // 10 segments and a half free; a spare of another size is used for the rolls but not counted as space.
    let available = 10 * SEG + SEG / 2;
    let open = estimate_storage(available, 100 * SEG, None, &head, Some(rates));
    assert_eq!(open.estimated_rolls_left, 10);
    assert_eq!(open.next_segment_size, SEG);
    assert!((open.estimated_time_to_full.unwrap().as_secs_f64() - (10.0 + left)).abs() < 1e-6);
    head.end = HeadEnd::Sealed;
    let sealed = estimate_storage(available, 100 * SEG, None, &head, Some(rates));
    assert_eq!(sealed.estimated_time_to_full.unwrap().as_secs_f64(), 10.0);
    let spare = estimate_storage(
        available,
        100 * SEG,
        Some((2 * SEG, true)),
        &head,
        Some(rates),
    );
    assert_eq!(spare.next_segment_size, 2 * SEG);
    assert_eq!(spare.estimated_rolls_left, 5);
    assert!(spare.spare_segment);
    assert_eq!(spare.estimated_time_to_full.unwrap().as_secs_f64(), 10.0);
    assert!(
        !estimate_storage(available, 100 * SEG, Some((2 * SEG, false)), &head, None).spare_segment
    );
    assert_eq!(
        estimate_storage(available, 100 * SEG, None, &head, None).estimated_time_to_full,
        None
    );

    // Through `statvfs` (round 4, item 4): the roll count and the sealed head's estimate follow from the one reading
    // they were computed from, so other tests writing meanwhile can't make this fail (before, it needed two identical
    // readings within 100 attempts).
    let dir = tempfile::tempdir().unwrap();
    drop(Writer::open(dir.path(), config()).unwrap());
    let sealed = storage(dir.path(), &head, Some(rates)).unwrap();
    assert!(sealed.next_segment_size >= SEG);
    assert_eq!(
        sealed.estimated_rolls_left,
        sealed.available_bytes / sealed.next_segment_size
    );
    let expected = (sealed.estimated_rolls_left * sealed.next_segment_size) as f64 / (SEG as f64);
    let estimate = sealed.estimated_time_to_full.unwrap().as_secs_f64();
    assert!(
        (estimate - expected).abs() <= expected * 1e-9 + 1e-6,
        "{estimate} vs {expected}"
    );
}

#[test]
fn sampler_resumes_and_measures_rates() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 5, 100, 1);
    let mut sampler = Sampler::new(dir.path());
    let first = sampler.sample().unwrap();
    assert_eq!(first.head.position.next_seq, 5);
    assert_eq!(first.rates, None);
    append(&mut writer, 7, 100, 2);
    let second = sampler.sample().unwrap();
    assert_eq!(second.head.position.next_seq, 12);
    assert_eq!(second.head.last_local_ts_ns, Some(2));
    assert!(
        second.head.verified,
        "resumed in the same file, from its start"
    );
    // The monotonic clock may not have advanced on a coarse platform; only check rates if it did.
    if let Some(rates) = second.rates {
        assert!(rates.records_per_sec > 0.0);
        assert!(rates.bytes_per_sec.unwrap() > 0.0);
        assert!(second.storage.estimated_time_to_full.is_some());
    }
    // Nothing new: the carried timestamp stays, the rate is zero and no estimate is made.
    let third = sampler.sample().unwrap();
    assert_eq!(third.head, second.head);
    if let Some(rates) = third.rates {
        assert_eq!(rates.records_per_sec, 0.0);
    }
    assert_eq!(third.storage.estimated_time_to_full, None);
    // Across a roll: the previous head's segment is unchanged, so rates continue.
    append(&mut writer, records_per_segment(100), 100, 3);
    let fourth = sampler.sample().unwrap();
    assert_eq!(fourth.head.position.next_seq, 12 + records_per_segment(100));
    assert_eq!(fourth.head, status(dir.path()).unwrap().head);
}

#[test]
fn sampler_rescans_a_replaced_segment() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 6, 10, 0);
    drop(writer);
    let mut sampler = Sampler::new(dir.path());
    assert_eq!(sampler.sample().unwrap().head.position.next_seq, 6);
    // Replace the segment by a copy holding only the first 2 records (as a repair would, with a new inode).
    let path = journal::segment_path(dir.path(), 0);
    let mut image = fs::read(&path).unwrap();
    let cut = (64 + 2 * frame(10)) as usize;
    image[cut..].fill(0);
    let tmp = dir.path().join("copy");
    fs::write(&tmp, &image).unwrap();
    fs::rename(&tmp, &path).unwrap();
    let s = sampler.sample().unwrap();
    assert_eq!(s.head.position, Position::new(2, 0, cut as u64));
    assert_eq!(s.rates, None, "the history changed");
}

/// Finding 11: a journal replaced at the same path whose segment file has the same inode (ext4 reuses inode numbers
/// at once; simulated here by rewriting the file in place) is recognised by its header, rescanned, and gives no
/// rates.
#[test]
fn sampler_detects_a_different_journal_in_the_same_inode() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 100, 10, 0);
    drop(writer);
    let mut sampler = Sampler::new(dir.path());
    assert_eq!(sampler.sample().unwrap().head.position.next_seq, 100);

    // Another journal with 10 records, written into the same file.
    let other = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(other.path(), config()).unwrap();
    append(&mut writer, 10, 10, 0);
    drop(writer);
    let image = fs::read(journal::segment_path(other.path(), 0)).unwrap();
    let path = journal::segment_path(dir.path(), 0);
    let ino = fs::metadata(&path).unwrap().ino();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .write_all_at(&image, 0)
        .unwrap();
    assert_eq!(fs::metadata(&path).unwrap().ino(), ino);
    let s = sampler.sample().unwrap();
    assert_eq!(s.head.position.next_seq, 10);
    assert_eq!(s.rates, None);
}

/// Finding 11: after recovery repairs a segment and the writer appends again, the sampler drops its rates and its
/// cached oldest-uncovered timestamp.
#[test]
fn sampler_after_a_repair() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    let per = records_per_segment(1000);
    append(&mut writer, 2, 1000, 1);
    writer.checkpoint().unwrap();
    // The first uncovered record (seq 2) has timestamp 2; the head moves into segment `per`.
    append(&mut writer, per, 1000, 2);
    drop(writer);
    let mut sampler = Sampler::new(dir.path());
    let first = sampler.sample().unwrap();
    assert_eq!(first.exposure.oldest_local_ts_ns, Some(2));
    assert_eq!(first.head.position.next_seq, per + 2);

    // Damage record 2: recovery quarantines from there (and the now disconnected segment), and repairs segment 0
    // into a new file; the new writer appends records with timestamp 99.
    let at = 64 + 2 * frame(1000) + format::RECORD_HEADER_LEN as u64;
    File::options()
        .write(true)
        .open(journal::segment_path(dir.path(), 0))
        .unwrap()
        .write_all_at(&[0x00], at)
        .unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, per, 1000, 99);
    drop(writer);
    let s = sampler.sample().unwrap();
    assert_eq!(s.head.position.next_seq, per + 2);
    assert_eq!(s.rates, None, "the history changed under the sampler");
    assert_eq!(s.exposure.oldest_local_ts_ns, Some(99));
}

/// Finding 11: a lookup that failed is not cached.
#[test]
fn sampler_does_not_cache_a_failed_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    let per = records_per_segment(1000);
    append(&mut writer, 2, 1000, 1);
    writer.checkpoint().unwrap();
    append(&mut writer, per, 1000, 2);
    drop(writer);
    let path = journal::segment_path(dir.path(), 0);
    let at = 64 + 2 * frame(1000) + format::RECORD_HEADER_LEN as u64;
    let file = File::options().write(true).open(&path).unwrap();
    let mut original = [0u8; 1];
    File::open(&path)
        .unwrap()
        .read_exact_at(&mut original, at)
        .unwrap();
    file.write_all_at(&[original[0] ^ 0xFF], at).unwrap();
    let mut sampler = Sampler::new(dir.path());
    assert_eq!(sampler.sample().unwrap().exposure.oldest_local_ts_ns, None);
    file.write_all_at(&original, at).unwrap();
    assert_eq!(
        sampler.sample().unwrap().exposure.oldest_local_ts_ns,
        Some(2)
    );
}

/// Round 2, B3 (a): a segment overwritten in place by an older snapshot of itself (same inode, size and header, as
/// ext4 gives a restored copy) is not resumed: the last record the sampler passed is gone, so it rescans and reports
/// what a fresh sample reports.
#[test]
fn sampler_rescans_a_snapshot_restored_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 2, 10, 0);
    let path = journal::segment_path(dir.path(), 0);
    let snapshot = fs::read(&path).unwrap();
    append(&mut writer, 4, 10, 0);
    drop(writer);
    let mut sampler = Sampler::new(dir.path());
    let first = sampler.sample().unwrap();
    assert_eq!(first.head.position.next_seq, 6);
    assert!(first.head.verified);
    let ino = fs::metadata(&path).unwrap().ino();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .write_all_at(&snapshot, 0)
        .unwrap();
    assert_eq!(fs::metadata(&path).unwrap().ino(), ino);
    let s = sampler.sample().unwrap();
    let fresh = status(dir.path()).unwrap();
    assert_eq!(fresh.head.position.next_seq, 2);
    assert_eq!(s.head, fresh.head);
    assert_eq!(s.rates, None, "the history changed under the sampler");
}

/// Writes a journal whose checkpoint is at the end of segment 0, so the oldest uncovered record is the first of
/// segment `per`, reached by following the seal; its timestamp is `ts`.
fn checkpoint_at_a_seal(dir: &Path, ts: u64) -> u64 {
    let per = records_per_segment(1000);
    let mut writer = Writer::open(dir, config()).unwrap();
    append(&mut writer, per, 1000, 1);
    writer.checkpoint().unwrap();
    append(&mut writer, 2, 1000, ts);
    per
}

/// Round 2, B3 (b): the oldest uncovered record's timestamp, found by following a seal into the next segment, is not
/// carried over when that segment is replaced: it is looked up in every sample.
#[test]
fn sampler_follows_a_replaced_successor() {
    let dir = tempfile::tempdir().unwrap();
    let per = checkpoint_at_a_seal(dir.path(), 2);
    let other = tempfile::tempdir().unwrap();
    checkpoint_at_a_seal(other.path(), 99);
    let mut sampler = Sampler::new(dir.path());
    let s = sampler.sample().unwrap();
    assert_eq!(s.checkpoint.unwrap().marker.segment_base_seq, 0);
    assert_eq!(s.head.position.segment_base_seq, per);
    assert_eq!(s.exposure.oldest_local_ts_ns, Some(2));
    // Replace segment `per` by one whose records carry timestamp 99 (as recovery replaces a segment it repairs).
    fs::rename(
        journal::segment_path(other.path(), per),
        journal::segment_path(dir.path(), per),
    )
    .unwrap();
    assert_eq!(
        status(dir.path()).unwrap().exposure.oldest_local_ts_ns,
        Some(99)
    );
    assert_eq!(
        sampler.sample().unwrap().exposure.oldest_local_ts_ns,
        Some(99)
    );
}

/// Round 2, B3 (c): a checkpoint marker that moved to a position the sampler's earlier scans passed is checked there
/// before the sampler resumes. One that isn't a record boundary with its sequence number is reported as a fresh
/// sample reports it (corruption), never as verified and healthy.
#[test]
fn sampler_checks_a_marker_that_moved_behind_its_head() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 6, 10, 0);
    drop(writer);
    let mut sampler = Sampler::new(dir.path());
    let first = sampler.sample().unwrap();
    assert!(first.head.verified);
    assert_eq!(first.checkpoint, None);
    // A marker into the middle of record 1, aligned, inside the area the sampler scanned.
    let slot = CheckpointSlot {
        generation: 5,
        next_seq: 3,
        segment_base_seq: 0,
        offset: 64 + frame(10) + 8,
    };
    marker(dir.path())
        .write_all_at(&format::encode_checkpoint_slot(&slot), 65_536)
        .unwrap();
    let fresh = status(dir.path()).unwrap();
    assert!(
        matches!(fresh.head.end, HeadEnd::Invalid(_)),
        "{:?}",
        fresh.head
    );
    let s = sampler.sample().unwrap();
    assert_eq!(s.head, fresh.head);
    // The writer refuses that marker too: no record ends at its offset (ADR 0056).
    assert_eq!(
        s.breaches(&Thresholds::default()),
        vec![Breach::HeadInvalid, Breach::CheckpointInvalid]
    );
    assert_eq!(
        s.checkpoint.unwrap().problem,
        Some(CheckpointProblem::Dangling)
    );

    // A marker that moved to a real record boundary behind the head is accepted, and the sampler resumes.
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 6, 10, 0);
    drop(writer);
    let mut sampler = Sampler::new(dir.path());
    sampler.sample().unwrap();
    let slot = CheckpointSlot {
        generation: 1,
        next_seq: 2,
        segment_base_seq: 0,
        offset: 64 + 2 * frame(10),
    };
    marker(dir.path())
        .write_all_at(&format::encode_checkpoint_slot(&slot), 65_536)
        .unwrap();
    let s = sampler.sample().unwrap();
    assert_eq!(s.head.position.next_seq, 6);
    assert!(s.head.verified);
    assert_eq!(s.exposure.records, Some(4));
}

/// Round 2, B1: a checkpoint segment that the listing showed but that is gone when it is opened (retention removed it
/// after the marker moved on) is not reported as a dangling marker: the error makes the sampler retry with a fresh
/// listing and marker.
#[test]
fn checkpoint_segment_gone_after_the_listing_is_retried() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    let per = records_per_segment(1000);
    append(&mut writer, per + 2, 1000, 0);
    writer.checkpoint().unwrap();
    append(&mut writer, per, 1000, 0);
    drop(writer);
    let files = list_files(dir.path()).unwrap();
    let marker = read_marker(dir.path()).unwrap().unwrap();
    let c = marker.slot.unwrap();
    assert_eq!(c.segment_base_seq, per);
    fs::remove_file(journal::segment_path(dir.path(), per)).unwrap();
    let mut open = OpenSegments::default();
    let result = checkpoint_status(dir.path(), &files, &mut open, c, &marker, 0);
    assert!(
        matches!(&result, Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound),
        "{result:?}"
    );
}

/// Round 2, B2: with every segment gone and a valid marker, the sample reports a dangling checkpoint (corruption, as
/// the writer reports it), not a missing journal.
#[test]
fn checkpoint_without_segments_is_dangling() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 3, 10, 0);
    writer.checkpoint().unwrap();
    drop(writer);
    fs::remove_file(journal::segment_path(dir.path(), 0)).unwrap();
    let err = Writer::open(dir.path(), config()).unwrap_err();
    assert!(
        matches!(
            &err,
            Error::Corrupt {
                reason: CorruptReason::DanglingCheckpoint(0),
                ..
            }
        ),
        "{err}"
    );
    let err = status(dir.path()).unwrap_err();
    assert!(
        matches!(
            &err,
            Error::Corrupt {
                reason: CorruptReason::DanglingCheckpoint(0),
                ..
            }
        ),
        "{err}"
    );
}

/// Round 2, B2: a segment the listing proves missing (the next segment's base is beyond what the checkpoint's segment
/// can hold) is a corruption breach, consistent with `verify`, and no exposure is summed across it.
#[test]
fn provably_missing_segment_is_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 3, 10, 0);
    writer.checkpoint().unwrap();
    append(&mut writer, 2, 10, 0);
    drop(writer);
    // A valid, empty newest segment far beyond what segment 0 can hold (at most (SEG - 72) / 40 records).
    let base = 1_000_000;
    let header = SegmentHeader {
        segment_size: SEG,
        base_seq: base,
        created_unix_ns: crate::clock::realtime_ns(),
    };
    let path = journal::segment_path(dir.path(), base);
    let file = File::create(&path).unwrap();
    file.set_len(SEG).unwrap();
    file.write_all_at(&format::encode_segment_header(&header), 0)
        .unwrap();
    let report = crate::inspect::verify(dir.path()).unwrap();
    assert!(!report.problems.is_empty(), "verify agrees");
    let s = status(dir.path()).unwrap();
    assert!(s.head.chain_broken);
    assert!(!s.head.verified);
    assert_eq!(s.exposure.records, None);
    assert_eq!(s.exposure.bytes, None);
    let breaches = s.breaches(&Thresholds::default());
    assert_eq!(breaches, vec![Breach::ChainBroken]);
    assert!(breaches[0].is_corruption());
}

#[test]
fn reader_lag_in_process() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    let per = records_per_segment(1000);
    append(&mut writer, per + 3, 1000, 0);
    let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let cursor = reader.cursor();
    assert_eq!(cursor.next_seq, 0);
    assert_eq!(cursor.position, Position::new(0, 0, 64));
    assert!(!cursor.seek_pending && !cursor.durable_only);
    let s = status(dir.path()).unwrap();
    let lag = s.lag(&reader.cursor()).unwrap();
    assert_eq!(lag.records, per + 3);
    assert_eq!(lag.bytes, Some(SEG + s.head.position.offset - 64));
    for _ in 0..per + 1 {
        reader.try_next().unwrap().unwrap();
    }
    assert_eq!(
        reader.cursor().position,
        Position::new(per + 1, per, 64 + frame(1000))
    );
    let lag = s.lag(&reader.cursor()).unwrap();
    assert_eq!(lag.records, 2);
    assert_eq!(lag.bytes, Some(2 * frame(1000)));
    while reader.try_next().unwrap().is_some() {}
    assert_eq!(s.lag(&reader.cursor()), Ok(caught_up(0)));
}

/// Finding 14: a caught-up or ahead reader has no byte lag; a pending seek has no byte position.
#[test]
fn lag_of_a_reader_that_is_ahead_or_seeking() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    let per = records_per_segment(1000);
    append(&mut writer, 2 * per, 1000, 0);
    let s = status(dir.path()).unwrap();
    let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    // Past the head: nothing to read.
    reader.seek(10 * per).unwrap();
    let cursor = reader.cursor();
    assert!(cursor.seek_pending);
    assert_eq!(cursor.next_seq, 10 * per);
    assert_eq!(s.lag(&cursor), Ok(caught_up(0)));
    // Behind the head, still skipping towards the target: records are known, bytes are not.
    reader.seek(per + 5).unwrap();
    let cursor = reader.cursor();
    assert!(cursor.seek_pending);
    let lag = s.lag(&cursor).unwrap();
    assert_eq!(lag.records, per - 5);
    assert_eq!(lag.bytes, None);
    // At the end of a segment, before following the seal: zero records, zero bytes.
    let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    for _ in 0..2 * per {
        reader.try_next().unwrap().unwrap();
    }
    assert_eq!(s.lag(&reader.cursor()), Ok(caught_up(0)));
}

/// Finding 8: a durable-only reader's lag is measured against the checkpoint, which it can't pass.
#[test]
fn lag_of_a_durable_only_reader() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 5, 10, 0);
    let mut reader = Reader::open(dir.path(), ReaderConfig::default().durable_only(true)).unwrap();
    assert!(reader.cursor().durable_only);
    // No checkpoint: it can read nothing, so it isn't behind.
    let s = status(dir.path()).unwrap();
    assert_eq!(s.lag(&reader.cursor()), Ok(caught_up(0)));
    writer.checkpoint().unwrap();
    append(&mut writer, 100, 10, 0);
    let s = status(dir.path()).unwrap();
    assert_eq!(s.lag(&reader.cursor()).unwrap().records, 5);
    while reader.try_next().unwrap().is_some() {}
    assert_eq!(reader.position(), 5);
    assert_eq!(s.lag(&reader.cursor()), Ok(caught_up(0)));
    let normal = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    assert_eq!(s.lag(&normal.cursor()).unwrap().records, 105);
}

/// Finding 8: a cursor in a segment that is gone, or in another journal's file, has no lag.
#[test]
fn lag_needs_the_same_segment_file() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 5, 10, 0);
    let mut writer2 = Writer::open(other.path(), config()).unwrap();
    append(&mut writer2, 50, 10, 0);
    let foreign = Reader::open(other.path(), ReaderConfig::default()).unwrap();
    let s = status(dir.path()).unwrap();
    assert_eq!(s.lag(&foreign.cursor()), Err(LagUnknown::OtherHistory));

    let config = config().retention(Policy::default().max_segments(1), Action::Delete);
    drop(writer);
    let reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let mut writer = Writer::open(dir.path(), config).unwrap();
    append(&mut writer, 2 * records_per_segment(10), 10, 0);
    writer.checkpoint().unwrap();
    let s = status(dir.path()).unwrap();
    assert_ne!(s.segments.first_seq, 0);
    assert_eq!(s.lag(&reader.cursor()), Err(LagUnknown::SegmentGone));
}

/// Round 2, C3: a cursor in a segment the writer created after the sample has an unknown lag (ahead of the sample),
/// not a removed segment.
#[test]
fn lag_of_a_cursor_ahead_of_the_sample() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    let per = records_per_segment(1000);
    append(&mut writer, per - 1, 1000, 0);
    let s = status(dir.path()).unwrap();
    append(&mut writer, 3, 1000, 0);
    let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    for _ in 0..per + 1 {
        reader.try_next().unwrap().unwrap();
    }
    assert_eq!(reader.cursor().position.segment_base_seq, per);
    assert_eq!(s.lag(&reader.cursor()), Err(LagUnknown::AheadOfSample));
    assert_eq!(LagUnknown::AheadOfSample.code(), "ahead_of_sample");
    let s = status(dir.path()).unwrap();
    assert_eq!(s.lag(&reader.cursor()).unwrap().records, 1);
}

/// Round 2, C3: a checkpoint age unknown because the marker was rewritten during every read (a writer checkpointing
/// very often) doesn't breach `max_checkpoint_age`: it means the checkpoint is very recent. Other unknown ages do.
#[test]
fn a_marker_changing_under_the_read_is_not_an_old_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 2, 10, 0);
    writer.checkpoint().unwrap();
    append(&mut writer, 1, 10, 0);
    let t = Thresholds::default().max_checkpoint_age(HOUR);
    let mut s = status(dir.path()).unwrap();
    assert_eq!(s.exposure.records, Some(1));
    let c = s.checkpoint.as_mut().unwrap();
    c.estimated_age = None;
    c.age_unknown = Some(AgeUnknown::Changing);
    s.exposure.estimated_age = None;
    assert!(s.breaches(&t).is_empty());
    s.checkpoint.as_mut().unwrap().age_unknown = Some(AgeUnknown::InFuture);
    assert_eq!(s.breaches(&t), vec![Breach::CheckpointAge]);
}

/// Round 2, C3: a creator checks the file it opened at the name, then `lstat`s the name before replacing it. If the
/// name was replaced meanwhile (outside the protocol), the new file was never checked, so it checks again: a
/// non-beacon file put there is refused, never renamed over.
#[cfg(feature = "fault-injection")]
#[test]
fn takeover_rechecks_a_replaced_name() {
    use crate::fault::{Pause, on_pause};
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 3, 10, 0);
    let reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let path = stale_beacon(ReaderBeacon::create(dir.path(), "r", &reader).unwrap());
    let precious = dir.path().join("precious");
    fs::write(&precious, b"not a beacon").unwrap();
    let (name, replacement) = (path.clone(), precious.clone());
    let mut done = false;
    on_pause(Some(Box::new(move |p| {
        if p == Pause::CreatorOpenedExisting && !done {
            done = true;
            fs::rename(&replacement, &name).unwrap();
        }
    })));
    let result = ReaderBeacon::create(dir.path(), "r", &reader);
    on_pause(None);
    let err = result.unwrap_err();
    assert!(matches!(err, Error::InvalidConfig(_)), "{err}");
    assert_eq!(fs::read(&path).unwrap(), b"not a beacon");
}

#[test]
fn footprint_cases() {
    let file = |base, size| SegFile {
        base,
        size,
        file: (0, base),
    };
    let files = [file(0, 100), file(10, 200), file(20, 300)];
    let p = Position::new;
    assert_eq!(footprint(&files, p(1, 0, 64), p(3, 0, 80)), Some(16));
    assert_eq!(footprint(&files, p(3, 0, 80), p(1, 0, 64)), None);
    assert_eq!(
        footprint(&files, p(1, 0, 64), p(21, 20, 72)),
        Some(36 + 200 + 72)
    );
    assert_eq!(footprint(&files, p(21, 20, 72), p(1, 0, 64)), None);
    assert_eq!(footprint(&files, p(1, 5, 64), p(21, 20, 72)), None);
    assert_eq!(footprint(&files, p(1, 0, 64), p(31, 30, 72)), None);
    assert!(valid_offset(64, SEG));
    assert!(valid_offset(SEG - 8, SEG));
    assert!(!valid_offset(SEG, SEG));
    assert!(!valid_offset(60, SEG));
    assert!(!valid_offset(68, SEG));
}

#[test]
fn retention_removed_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let config = config().retention(Policy::default().max_segments(2), Action::Delete);
    let mut writer = Writer::open(dir.path(), config).unwrap();
    let per = records_per_segment(1000);
    let reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let mut beacon = ReaderBeacon::create(dir.path(), "old", &reader).unwrap();
    beacon.flush(&reader).unwrap();
    append(&mut writer, 4 * per + 1, 1000, 0);
    writer.checkpoint().unwrap();
    let s = status(dir.path()).unwrap();
    assert_eq!(s.segments.count, 2);
    assert_eq!(s.segments.first_seq, 3 * per);
    let r = &s.readers[0];
    assert_eq!(r.state, BeaconState::Live);
    assert_eq!(r.report.unwrap().next_seq, 0);
    assert_eq!(r.lag, None);
    assert_eq!(r.lag_unknown, Some(LagUnknown::SegmentGone));
    assert_eq!(r.oldest_unread_local_ts_ns, None);
    assert_eq!(writer.metrics().retention_failures, 0);
}

/// A stale beacon: a real beacon's bytes, written back after it was dropped (so nobody holds its lock).
fn stale_beacon(beacon: ReaderBeacon) -> PathBuf {
    let bytes = fs::read(beacon.path()).unwrap();
    let path = beacon.path().to_path_buf();
    drop(beacon);
    fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn beacons_live_stale_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    let now = crate::clock::realtime_ns();
    append(&mut writer, 10, 10, now);
    let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    reader.try_next().unwrap().unwrap();
    let dead = stale_beacon(ReaderBeacon::create(dir.path(), "dead", &reader).unwrap());
    for _ in 0..3 {
        reader.try_next().unwrap().unwrap();
    }
    let beacon = ReaderBeacon::create(dir.path(), "live-1", &reader).unwrap();
    assert_eq!(beacon.path(), dir.path().join("readers").join("live-1"));
    assert!(beacon.is_published());
    let readers = dir.path().join("readers");
    let valid = fs::read(beacon.path()).unwrap();
    fs::write(readers.join("garbage"), [7u8; BEACON_LEN]).unwrap();
    fs::write(readers.join("short"), [7u8; 10]).unwrap();
    // A valid beacon with one more byte is not a beacon (exact length).
    let mut long = valid.clone();
    long.push(0);
    fs::write(readers.join("long"), long).unwrap();
    // Ignored: invalid names, hidden files, journal file names, directories, symlinks.
    fs::write(readers.join(".hidden"), &valid).unwrap();
    fs::write(readers.join("checkpoint"), &valid).unwrap();
    fs::create_dir(readers.join("subdir")).unwrap();
    std::os::unix::fs::symlink(&dead, readers.join("link")).unwrap();

    let s = status(dir.path()).unwrap();
    assert_eq!(s.readers_dir_state, BeaconDirState::Listed);
    let names: Vec<_> = s.readers.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["dead", "garbage", "live-1", "long", "short"]);
    let dead = &s.readers[0];
    assert_eq!(dead.state, BeaconState::Stale);
    assert_eq!(dead.report.unwrap().pid, std::process::id());
    assert_eq!(
        dead.lag,
        Some(Lag {
            records: 9,
            bytes: Some(9 * frame(10))
        })
    );
    for invalid in [1, 3, 4] {
        assert_eq!(s.readers[invalid].state, BeaconState::Invalid);
        assert_eq!(s.readers[invalid].report, None);
        assert_eq!(s.readers[invalid].lag, None);
        assert_eq!(s.readers[invalid].lag_unknown, None);
    }
    let live = &s.readers[2];
    assert_eq!(live.state, BeaconState::Live);
    let report = live.report.unwrap();
    assert_eq!(report.pid, std::process::id());
    assert_eq!(report.next_seq, 4);
    assert_eq!(report.position, reader.cursor().position);
    assert!(!report.durable_only && !report.seek_pending);
    assert_eq!(live.lag.unwrap().records, 6);
    assert_eq!(live.oldest_unread_local_ts_ns, Some(now));
    assert!(live.unread_age.is_some());
    // Lag thresholds apply to live beacons only: the dead one lags more but doesn't count.
    assert_eq!(
        s.breaches(&Thresholds::default().max_reader_lag(5)),
        vec![Breach::ReaderLag {
            name: "live-1".into()
        }]
    );
    assert!(
        s.breaches(&Thresholds::default().max_reader_lag(6))
            .is_empty()
    );

    // A live name can't be taken; a stale one can.
    let err = ReaderBeacon::create(dir.path(), "live-1", &reader).unwrap_err();
    assert!(matches!(&err, Error::BeaconInUse { .. }), "{err}");
    let taken = ReaderBeacon::create(dir.path(), "dead", &reader).unwrap();
    let s = status(dir.path()).unwrap();
    assert_eq!(s.readers[0].state, BeaconState::Live);
    assert_eq!(s.readers[0].lag.unwrap().records, 6);

    // Dropping a beacon removes its file.
    drop(beacon);
    drop(taken);
    assert!(!readers.join("live-1").exists());
    assert!(!readers.join("dead").exists());
}

/// Finding 1: a beacon never replaces anything but a stale beacon, and never takes a journal file's name.
#[test]
fn beacons_never_replace_other_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 3, 10, 0);
    writer.checkpoint().unwrap();
    drop(writer);
    let reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let snapshot = |dir: &Path| {
        let mut files: Vec<(String, Vec<u8>)> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| e.file_type().unwrap().is_file())
            .map(|e| {
                (
                    e.file_name().into_string().unwrap(),
                    fs::read(e.path()).unwrap(),
                )
            })
            .collect();
        files.sort();
        files
    };
    let before = snapshot(dir.path());
    // The journal directory as the beacon directory: journal names are refused outright.
    for name in [
        "checkpoint",
        "writer.lock",
        "notify",
        "next-segment.tmp",
        "quarantine",
        "readers",
        "00000000000000000000.tape",
        "00000000000000000000.tape.tmp",
        "x.tmp",
        // Round 2, A1: on a case-insensitive filesystem (APFS) these name journal files.
        "CHECKPOINT",
        "Writer.Lock",
        "NOTIFY",
        "Next-Segment.TMP",
        "Quarantine",
        "READERS",
        "00000000000000000000.TAPE",
        "x.Tmp",
    ] {
        assert!(!beacon::name_allowed(name), "{name}");
        let err = ReaderBeacon::create_in(dir.path(), name, &reader).unwrap_err();
        assert!(matches!(err, Error::InvalidConfig(_)), "{name}: {err}");
    }
    assert_eq!(snapshot(dir.path()), before, "a journal file was changed");

    // Other names in use by something that isn't a stale beacon are left alone.
    let beacons = tempfile::tempdir().unwrap();
    let valid = {
        let b = ReaderBeacon::create_in(beacons.path(), "probe", &reader).unwrap();
        fs::read(b.path()).unwrap()
    };
    let mut long = valid.clone();
    long.push(0);
    // A damaged magic is not a beacon. (A beacon whose later bytes are damaged is one whose update was cut short: see
    // `torn_stale_beacon_is_taken_over`.)
    let mut damaged = valid.clone();
    damaged[3] ^= 1;
    for (name, contents) in [
        ("notes", b"my notes".to_vec()),
        ("empty", Vec::new()),
        ("zeros", vec![0u8; BEACON_LEN]),
        ("long", long),
        ("damaged", damaged),
    ] {
        let path = beacons.path().join(name);
        fs::write(&path, &contents).unwrap();
        let err = ReaderBeacon::create_in(beacons.path(), name, &reader).unwrap_err();
        assert!(matches!(err, Error::InvalidConfig(_)), "{name}: {err}");
        assert_eq!(fs::read(&path).unwrap(), contents, "{name} was changed");
    }
    // A symlink at the name is not followed, and its target is untouched.
    let target = beacons.path().join("target");
    fs::write(&target, &valid).unwrap();
    std::os::unix::fs::symlink(&target, beacons.path().join("link")).unwrap();
    assert!(ReaderBeacon::create_in(beacons.path(), "link", &reader).is_err());
    assert_eq!(fs::read(&target).unwrap(), valid);
    // A directory at the name.
    fs::create_dir(beacons.path().join("dir")).unwrap();
    assert!(ReaderBeacon::create_in(beacons.path(), "dir", &reader).is_err());
    assert!(beacons.path().join("dir").is_dir());
    // No temporary files are left behind by the refusals.
    let leftovers: Vec<_> = fs::read_dir(beacons.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| beacon::temporary_name(n))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// A temporary file that a creator left when it died before publishing is removed by the next creator; one that is
/// still locked (its creator is working) is not.
#[test]
fn leftover_temporary_files_are_removed() {
    let dir = tempfile::tempdir().unwrap();
    drop(Writer::open(dir.path(), config()).unwrap());
    let reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let beacons = tempfile::tempdir().unwrap();
    let valid = {
        let b = ReaderBeacon::create_in(beacons.path(), "probe", &reader).unwrap();
        fs::read(b.path()).unwrap()
    };
    let dead = beacons.path().join(".r.4242.0.beacon-tmp");
    fs::write(&dead, &valid).unwrap();
    // Round 2, A2: names, file types and free locks don't prove provenance. These reach the deletion check (valid
    // temporary names, regular, unlocked) but their contents are not a beacon's, so they stay.
    let notes = beacons.path().join(".notes.123.0.beacon-tmp");
    fs::write(&notes, b"my notes").unwrap();
    let empty = beacons.path().join(".r.4244.0.beacon-tmp");
    fs::write(&empty, b"").unwrap();
    let mut longer = valid.clone();
    longer.push(0);
    let long = beacons.path().join(".r.4245.0.beacon-tmp");
    fs::write(&long, &longer).unwrap();
    let held = beacons.path().join(".r.4243.0.beacon-tmp");
    let held_file = File::create(&held).unwrap();
    rustix::fs::flock(&held_file, rustix::fs::FlockOperation::LockExclusive).unwrap();
    let unrelated = beacons.path().join(".something.beacon-tmp");
    fs::write(&unrelated, b"").unwrap();
    assert!(beacon::temporary_name(".r.4242.0.beacon-tmp"));
    assert!(!beacon::temporary_name(".something.beacon-tmp"));
    let _b = ReaderBeacon::create_in(beacons.path(), "r", &reader).unwrap();
    assert!(!dead.exists());
    assert!(held.exists());
    assert!(unrelated.exists());
    assert_eq!(fs::read(&notes).unwrap(), b"my notes");
    assert!(empty.exists());
    assert_eq!(fs::read(&long).unwrap(), longer);
}

/// Round 2, A6: a beacon whose last update was cut short (a partial `pwrite`, then its owner died) fails its checksum
/// but every byte is still one a beacon writer writes there (round 3: not only the magic and version). Unlocked, it is a stale
/// beacon: the next creator takes the name over instead of refusing it for ever. Observers report it invalid.
#[test]
fn torn_stale_beacon_is_taken_over() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 5, 10, 0);
    let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let path = stale_beacon(ReaderBeacon::create(dir.path(), "torn", &reader).unwrap());
    // The prefix of a later update: the next 40 bytes of a beacon for a reader that moved on.
    reader.try_next().unwrap().unwrap();
    let newer = {
        let beacons = tempfile::tempdir().unwrap();
        let b = ReaderBeacon::create_in(beacons.path(), "newer", &reader).unwrap();
        fs::read(b.path()).unwrap()
    };
    let old = fs::read(&path).unwrap();
    let mut torn = newer[..40].to_vec();
    torn.extend_from_slice(&old[40..]);
    assert_ne!(torn, old);
    fs::write(&path, &torn).unwrap();
    assert!(beacon::decode(&torn).is_none());
    let s = status(dir.path()).unwrap();
    assert_eq!(s.readers[0].state, BeaconState::Invalid);

    let taken = ReaderBeacon::create(dir.path(), "torn", &reader).unwrap();
    assert!(taken.is_published());
    let s = status(dir.path()).unwrap();
    assert_eq!(s.readers[0].state, BeaconState::Live);
    assert_eq!(s.readers[0].report.unwrap().next_seq, 1);
}

/// Round 3, item 3: a file is a beacon only if every byte is one a beacon writer could have written there, so a
/// 128-byte file with the magic and version and `0xff` everywhere else is neither taken over at a beacon's name nor
/// removed as a creator's leftover (before the fix, the magic and version alone decided both). Every mix of two
/// reports cut at one point (what an update cut short leaves) still counts, and is taken over.
#[test]
fn only_what_a_beacon_writer_could_write_is_a_beacon() {
    // The byte rules rest on these limits of the format.
    assert_eq!(format::SEGMENT_SIZE_UNIT, 1 << 16);
    assert_eq!(format::MAX_SEGMENT_SIZE, 1 << 40);
    assert_eq!(format::RECORD_ALIGN, 8);

    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 3, 10, 0);
    // Two reports as different as a reader's can be: another segment, offset and size, seek pending, durable-only.
    let first = {
        let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
        reader.try_next().unwrap().unwrap();
        reader.try_next().unwrap().unwrap();
        let beacons = tempfile::tempdir().unwrap();
        let b = ReaderBeacon::create_in(beacons.path(), "a", &reader).unwrap();
        fs::read(b.path()).unwrap()
    };
    drop(writer);
    let mut writer = Writer::open(
        dir.path(),
        WriterConfig::default()
            .segment_size(4 * SEG)
            .prefault(false),
    )
    .unwrap();
    append(&mut writer, records_per_segment(10) + 5, 10, 0);
    writer.checkpoint().unwrap();
    let second = {
        let mut reader =
            Reader::open(dir.path(), ReaderConfig::default().durable_only(true)).unwrap();
        reader.seek(records_per_segment(10) + 2).unwrap();
        let beacons = tempfile::tempdir().unwrap();
        let b = ReaderBeacon::create_in(beacons.path(), "b", &reader).unwrap();
        fs::read(b.path()).unwrap()
    };
    let (first, second): ([u8; BEACON_LEN], [u8; BEACON_LEN]) =
        (first.try_into().unwrap(), second.try_into().unwrap());
    for at in [12, 40, 48, 56, 64, 80] {
        assert_ne!(first[at..at + 4], second[at..at + 4], "offset {at}");
    }
    for cut in 0..=BEACON_LEN {
        for (new, old) in [(&first, &second), (&second, &first)] {
            let mut torn = *old;
            torn[..cut].copy_from_slice(&new[..cut]);
            assert!(beacon::beacon_file(&torn), "cut at {cut}");
        }
    }

    // Not a beacon: the magic and version, then 0xff; and single bytes no writer puts there.
    let mut ff = [0xffu8; BEACON_LEN];
    ff[..12].copy_from_slice(&first[..12]);
    assert!(!beacon::beacon_file(&ff));
    for (at, value) in [
        (12, 4),
        (15, 1),
        (16, 0),
        (19, 0x80),
        (20, 1),
        (48, 1),
        (53, 1),
        (64, 1),
        (69, 2),
        (71, 1),
        (100, 1),
    ] {
        let mut bad = first;
        if at == 16 {
            bad[16..20].fill(0);
        } else {
            bad[at] = value;
        }
        assert!(!beacon::beacon_file(&bad), "byte {at} = {value}");
        assert!(beacon::decode(&bad).is_none(), "byte {at} = {value}");
    }

    // Round 4, item 3: every report has a slot offset of at least the header's length and a non-zero segment size (a
    // reader always has an open segment whose header it validated), and one cut mixes at most one of those disjoint
    // fields, so a file with both zero is no mix of reports: here the magic, the version, a valid pid and zeros
    // elsewhere (before the fix, a beacon). Either field alone may be zero or small, as a mix can make it.
    for report in [&first, &second] {
        assert!(u64::from_le_bytes(report[48..56].try_into().unwrap()) >= 64);
        assert_ne!(report[64..72], [0; 8]);
    }
    let mut zeros = [0u8; BEACON_LEN];
    zeros[..20].copy_from_slice(&first[..20]);
    zeros[12..16].fill(0);
    assert!(!beacon::beacon_file(&zeros));
    for (field, value) in [(48, 0u64), (48, 56), (64, 0)] {
        let mut mix = first;
        mix[field..field + 8].copy_from_slice(&value.to_le_bytes());
        assert!(beacon::beacon_file(&mix), "field {field} = {value}");
        let mut both = mix;
        let other = if field == 48 { 64 } else { 48 };
        both[other..other + 8].fill(0);
        assert!(
            !beacon::beacon_file(&both),
            "field {field} = {value} and {other} zero"
        );
    }

    // At a beacon's name, the 0xff file is refused and kept; as a leftover temporary file, it stays.
    let reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let beacons = tempfile::tempdir().unwrap();
    let named = beacons.path().join("ff");
    fs::write(&named, ff).unwrap();
    let leftover = beacons.path().join(".r.4246.0.beacon-tmp");
    fs::write(&leftover, ff).unwrap();
    let err = ReaderBeacon::create_in(beacons.path(), "ff", &reader).unwrap_err();
    assert!(matches!(err, Error::InvalidConfig(_)), "{err}");
    assert_eq!(fs::read(&named).unwrap(), ff);
    let _r = ReaderBeacon::create_in(beacons.path(), "r", &reader).unwrap();
    assert_eq!(fs::read(&leftover).unwrap(), ff);

    // A torn mix of the two reports at the name is taken over, and as a leftover it is removed.
    let mut torn = second;
    torn[..44].copy_from_slice(&first[..44]);
    assert!(beacon::decode(&torn).is_none());
    let named = beacons.path().join("torn");
    fs::write(&named, torn).unwrap();
    let leftover = beacons.path().join(".x.4247.0.beacon-tmp");
    fs::write(&leftover, torn).unwrap();
    let taken = ReaderBeacon::create_in(beacons.path(), "torn", &reader).unwrap();
    assert!(taken.is_published());
    assert!(!leftover.exists());
    drop(writer);
}

/// Round 3, item 6: a durable-only reader that replaces a visible one at the same slot of the same file is published
/// (the mode is part of the cursor `update` compares), so its lag is measured against the checkpoint; and the reverse.
#[test]
fn a_change_of_reader_mode_is_published() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 2, 10, 0);
    writer.checkpoint().unwrap();
    append(&mut writer, 3, 10, 0);
    let mut visible = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    visible.try_next().unwrap().unwrap();
    let mut durable = Reader::open(dir.path(), ReaderConfig::default().durable_only(true)).unwrap();
    durable.try_next().unwrap().unwrap();
    assert_eq!(visible.position(), durable.position());
    let mut beacon = ReaderBeacon::create(dir.path(), "r", &visible).unwrap();
    beacon.set_interval(Duration::ZERO);
    let reported = |dir: &Path| {
        let s = status(dir).unwrap();
        let r = &s.readers[0];
        (r.report.unwrap().durable_only, r.lag.unwrap().records)
    };
    assert_eq!(reported(dir.path()), (false, 4));
    assert!(
        beacon.update(&durable).unwrap(),
        "the durable-only reader was not published"
    );
    assert_eq!(reported(dir.path()), (true, 1));
    assert!(
        beacon.update(&visible).unwrap(),
        "the visible reader was not published"
    );
    assert_eq!(reported(dir.path()), (false, 4));
    assert!(!beacon.update(&visible).unwrap());
    drop(writer);
}

/// Finding 7: unreadable beacons and unusable beacon directories never fail the sample.
#[test]
fn unreadable_beacons_and_directories() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 3, 10, 0);
    let reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let beacon = ReaderBeacon::create(dir.path(), "secret", &reader).unwrap();
    fs::set_permissions(beacon.path(), fs::Permissions::from_mode(0o000)).unwrap();
    let root = File::open(beacon.path()).is_ok();
    let s = status(dir.path()).unwrap();
    fs::set_permissions(beacon.path(), fs::Permissions::from_mode(0o644)).unwrap();
    if !root {
        assert_eq!(s.readers[0].state, BeaconState::Unreadable);
        assert_eq!(s.readers[0].report, None);
    }
    assert_eq!(s.head.position.next_seq, 3);
    drop(beacon);

    // An unlistable directory.
    let readers = dir.path().join("readers");
    fs::set_permissions(&readers, fs::Permissions::from_mode(0o000)).unwrap();
    let root = fs::read_dir(&readers).is_ok();
    let s = status(dir.path()).unwrap();
    fs::set_permissions(&readers, fs::Permissions::from_mode(0o755)).unwrap();
    if !root {
        assert_eq!(
            s.readers_dir_state,
            BeaconDirState::Unusable(io::ErrorKind::PermissionDenied)
        );
        assert!(s.readers.is_empty());
    }

    // A regular file where the directory should be.
    fs::remove_dir(&readers).unwrap();
    fs::write(&readers, b"x").unwrap();
    let s = status(dir.path()).unwrap();
    assert!(matches!(s.readers_dir_state, BeaconDirState::Unusable(_)));
    assert!(s.readers.is_empty());
    assert_eq!(s.head.position.next_seq, 3);
}

/// Finding 8: a beacon directory shared with another journal gives no lag against the wrong one.
#[test]
fn shared_beacon_directory() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let beacons = tempfile::tempdir().unwrap();
    let mut wa = Writer::open(a.path(), config()).unwrap();
    append(&mut wa, 5, 10, 0);
    let mut wb = Writer::open(b.path(), config()).unwrap();
    append(&mut wb, 150, 10, 0);
    let mut ra = Reader::open(a.path(), ReaderConfig::default()).unwrap();
    while ra.try_next().unwrap().is_some() {}
    let _beacon = ReaderBeacon::create_in(beacons.path(), "ra", &ra).unwrap();
    let s = Sampler::new(b.path())
        .readers_dir(beacons.path())
        .sample()
        .unwrap();
    let r = &s.readers[0];
    assert_eq!(r.state, BeaconState::Live);
    assert_eq!(r.lag, None);
    assert_eq!(r.lag_unknown, Some(LagUnknown::OtherHistory));
    assert!(
        s.breaches(&Thresholds::default().max_reader_lag(0))
            .is_empty()
    );
    let s = Sampler::new(a.path())
        .readers_dir(beacons.path())
        .sample()
        .unwrap();
    assert_eq!(s.readers[0].lag, Some(caught_up(0)));
}

/// Finding 8: a durable-only reader's beacon carries the flag, and its lag is against the checkpoint.
#[test]
fn durable_only_beacon() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 5, 10, 0);
    writer.checkpoint().unwrap();
    append(&mut writer, 20, 10, 0);
    let mut reader = Reader::open(dir.path(), ReaderConfig::default().durable_only(true)).unwrap();
    while reader.try_next().unwrap().is_some() {}
    let _beacon = ReaderBeacon::create(dir.path(), "durable", &reader).unwrap();
    let s = status(dir.path()).unwrap();
    let r = &s.readers[0];
    assert!(r.report.unwrap().durable_only);
    assert_eq!(r.lag, Some(caught_up(0)));
    assert_eq!(r.unread_age, Some(Duration::ZERO));
    assert!(
        s.breaches(&Thresholds::default().max_reader_lag(0))
            .is_empty()
    );
}

#[test]
fn beacon_names_are_validated() {
    let dir = tempfile::tempdir().unwrap();
    let _writer = Writer::open(dir.path(), config()).unwrap();
    let reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let long = "x".repeat(65);
    for bad in ["", ".x", "a/b", "a b", "é", long.as_str(), ".."] {
        assert!(
            matches!(
                ReaderBeacon::create(dir.path(), bad, &reader),
                Err(Error::InvalidConfig(_))
            ),
            "{bad:?}"
        );
    }
    for good in ["a", "A-z_0.9", &"x".repeat(64)] {
        ReaderBeacon::create(dir.path(), good, &reader).unwrap();
    }
}

/// Finding 20: no assumption about elapsed time; the interval is set far above or to zero.
#[test]
fn beacon_update_is_rate_limited() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 5, 10, 0);
    let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let mut beacon = ReaderBeacon::create(dir.path(), "r", &reader).unwrap();
    beacon.set_interval(HOUR);
    let read_back = |dir: &Path| status(dir).unwrap().readers[0].report.unwrap().next_seq;
    // Unchanged position: no write.
    assert!(!beacon.update(&reader).unwrap());
    reader.try_next().unwrap().unwrap();
    // Moved, but within the interval since the write at creation.
    assert!(!beacon.update(&reader).unwrap());
    assert_eq!(read_back(dir.path()), 0);
    beacon.flush(&reader).unwrap();
    assert_eq!(read_back(dir.path()), 1);
    beacon.set_interval(Duration::ZERO);
    assert!(!beacon.update(&reader).unwrap(), "position unchanged");
    reader.try_next().unwrap().unwrap();
    assert!(beacon.update(&reader).unwrap());
    assert_eq!(read_back(dir.path()), 2);
    // A seek is published with its pending flag.
    reader.seek(4).unwrap();
    assert!(beacon.update(&reader).unwrap());
    let report = status(dir.path()).unwrap().readers[0].report.unwrap();
    assert!(report.seek_pending);
    assert_eq!(report.next_seq, 4);
    // Round 2, B5: a pending seek whose cursor moves while its target (and so `Reader::position`) stays the same
    // (here it skips the records on its way to a target not written yet) is published: the whole cursor is compared.
    reader.seek(10).unwrap();
    assert!(beacon.update(&reader).unwrap());
    assert_eq!(
        status(dir.path()).unwrap().readers[0]
            .report
            .unwrap()
            .position
            .next_seq,
        0
    );
    assert!(reader.try_next().unwrap().is_none());
    assert_eq!(reader.position(), 10);
    assert!(beacon.update(&reader).unwrap(), "the cursor moved");
    let report = status(dir.path()).unwrap().readers[0].report.unwrap();
    assert_eq!(report.next_seq, 10);
    assert_eq!(report.position.next_seq, 5);
    assert!(report.seek_pending);
    assert!(!beacon.update(&reader).unwrap(), "nothing moved");
}

/// Round 2, B6: an unread record whose timestamp is later than the observer's clock gives an unknown unread age, not
/// zero.
#[test]
fn unread_age_of_a_future_timestamp_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    let future = crate::clock::realtime_ns() + u64::try_from(HOUR.as_nanos()).unwrap();
    append(&mut writer, 3, 10, future);
    let reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let _beacon = ReaderBeacon::create(dir.path(), "r", &reader).unwrap();
    let s = status(dir.path()).unwrap();
    let r = &s.readers[0];
    assert_eq!(r.lag.unwrap().records, 3);
    assert_eq!(r.oldest_unread_local_ts_ns, Some(future));
    assert_eq!(r.unread_age, None);
}

/// Round 2, B6: the sample's clock is read after the beacons, so a beacon written while the sample runs has an update
/// age rather than one "in the future".
#[cfg(feature = "fault-injection")]
#[test]
fn a_beacon_written_during_the_sample_has_an_update_age() {
    use crate::fault::{Pause, on_pause};
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 3, 10, 0);
    let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let mut beacon = ReaderBeacon::create(dir.path(), "r", &reader).unwrap();
    let mut written = false;
    on_pause(Some(Box::new(move |p| {
        if p == Pause::ObserverOpened && !written {
            written = true;
            // Later than anything the sample read before the beacons.
            std::thread::sleep(Duration::from_millis(2));
            reader.try_next().unwrap().unwrap();
            beacon.flush(&reader).unwrap();
        }
    })));
    let s = status(dir.path());
    on_pause(None);
    let s = s.unwrap();
    assert_eq!(
        s.readers[0].report.unwrap().next_seq,
        1,
        "the observer read the new write"
    );
    assert!(s.readers[0].update_age.is_some());
    assert!(s.readers[0].report.unwrap().updated_unix_ns <= s.sampled_unix_ns);
}

/// Round 2, B7: a beacon whose name is replaced during every attempt to observe it is reported as `changing`, with
/// no contents and no lag (what was seen belongs to files that no longer hold the name).
#[cfg(feature = "fault-injection")]
#[test]
fn a_beacon_replaced_during_every_observation_is_changing() {
    use crate::fault::{Pause, on_pause};
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 3, 10, 0);
    let reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let journal = dir.path().to_path_buf();
    let mut beacon = Some(ReaderBeacon::create(dir.path(), "r", &reader).unwrap());
    on_pause(Some(Box::new(move |p| {
        if p == Pause::ObserverRead {
            // The observer holds the old file open, so the new one gets another inode.
            drop(beacon.take());
            beacon = Some(ReaderBeacon::create(&journal, "r", &reader).unwrap());
        }
    })));
    let s = status(dir.path());
    on_pause(None);
    let s = s.unwrap();
    assert_eq!(s.readers.len(), 1);
    let r = &s.readers[0];
    assert_eq!(r.state, BeaconState::Changing);
    assert_eq!(r.report, None);
    assert_eq!(r.lag, None);
    assert_eq!(r.lag_unknown, None);
    assert!(
        s.breaches(&Thresholds::default().max_reader_lag(0))
            .is_empty()
    );
}

#[test]
fn beacon_encoding_round_trips_and_rejects_damage() {
    let payload = beacon::Payload {
        report: BeaconReport {
            pid: 7,
            next_seq: u64::MAX - 1,
            position: Position::new(12350, 12345, 4096),
            seek_pending: true,
            durable_only: true,
            updated_unix_ns: 99,
        },
        segment_file: (3, 4),
        segment_size: SEG,
        segment_created_unix_ns: 5,
    };
    let b = beacon::encode(&payload);
    assert_eq!(b.len(), BEACON_LEN);
    assert_eq!(beacon::decode(&b), Some(payload));
    for i in 0..BEACON_LEN {
        let mut damaged = b;
        damaged[i] ^= 1;
        assert_eq!(beacon::decode(&damaged), None, "byte {i}");
    }
    assert_eq!(beacon::decode(&b[..BEACON_LEN - 1]), None);
    let mut long = b.to_vec();
    long.push(0);
    assert_eq!(beacon::decode(&long), None);
}

#[test]
fn writer_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    let m = writer.metrics();
    assert_eq!(m.head, Position::new(0, 0, 64));
    assert_eq!(m.segment_size, SEG);
    assert_eq!(m.checkpoint, None);
    assert_eq!(m.since_checkpoint, None);
    assert_eq!(m.uncheckpointed_records, 0);
    assert_eq!(m.uncheckpointed_bytes, 0);
    assert!(!m.poisoned);
    assert_eq!(m.retention_failures, 0);

    let per = records_per_segment(1000);
    append(&mut writer, per + 2, 1000, 0);
    let m = writer.metrics();
    assert_eq!(m.head, Position::new(per + 2, per, 64 + 2 * frame(1000)));
    assert_eq!(m.uncheckpointed_records, per + 2);
    // Every record and the seal in segment 0, plus two records in segment 1.
    assert_eq!(
        m.uncheckpointed_bytes,
        per * frame(1000) + 8 + 2 * frame(1000)
    );
    // The same head as an outside observer sees.
    assert_eq!(status(dir.path()).unwrap().head.position, m.head);

    writer.checkpoint().unwrap();
    let m = writer.metrics();
    assert_eq!(m.checkpoint.unwrap().next_seq, per + 2);
    assert_eq!(m.checkpoint.unwrap().generation, 1);
    assert!(m.since_checkpoint.is_some());
    assert_eq!(m.uncheckpointed_records, 0);
    assert_eq!(m.uncheckpointed_bytes, 0);
    append(&mut writer, 1, 1000, 0);
    assert_eq!(writer.metrics().uncheckpointed_bytes, frame(1000));
    drop(writer);

    // A new writer reads the marker but has no monotonic age for it; everything after the marker is uncovered.
    let missing = dir.path().join("no-such-archive");
    let config = config().retention(Policy::default().max_segments(1), Action::MoveTo(missing));
    let mut writer = Writer::open(dir.path(), config).unwrap();
    let m = writer.metrics();
    assert_eq!(m.checkpoint.unwrap().next_seq, per + 2);
    assert_eq!(m.since_checkpoint, None);
    assert_eq!(m.uncheckpointed_records, 1);
    assert_eq!(m.uncheckpointed_bytes, frame(1000));
    // Retention to a missing directory fails at open and after each checkpoint, without failing either.
    assert_eq!(m.retention_failures, 1);
    writer.checkpoint().unwrap();
    assert_eq!(writer.metrics().retention_failures, 2);
    assert!(writer.last_retention_error().is_some());
}

/// Finding 14: reopening across a roll counts only what was appended after the marker, not whole segments.
#[test]
fn writer_metrics_after_reopening_across_rolls() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    let per = records_per_segment(1000);
    append(&mut writer, 10, 1000, 0);
    writer.checkpoint().unwrap();
    append(&mut writer, 2 * per, 1000, 0);
    let before = writer.metrics();
    drop(writer);
    let writer = Writer::open(dir.path(), config()).unwrap();
    let after = writer.metrics();
    assert_eq!(after.uncheckpointed_records, before.uncheckpointed_records);
    assert_eq!(after.uncheckpointed_bytes, before.uncheckpointed_bytes);
    assert_eq!(after.head, before.head);
}

/// Finding 14: with no marker after retention removed a prefix, the writer counts from the oldest retained record,
/// as an observer does.
#[test]
fn writer_metrics_without_a_marker_after_retention() {
    let dir = tempfile::tempdir().unwrap();
    let per = records_per_segment(1000);
    let retain = config().retention(Policy::default().max_segments(1), Action::Delete);
    let mut writer = Writer::open(dir.path(), retain).unwrap();
    append(&mut writer, 2 * per + 1, 1000, 0);
    writer.checkpoint().unwrap();
    drop(writer);
    fs::remove_file(dir.path().join("checkpoint")).unwrap();
    let writer = Writer::open(dir.path(), config()).unwrap();
    let s = status(dir.path()).unwrap();
    assert_eq!(s.segments.first_seq, 2 * per);
    assert_eq!(s.exposure.records, Some(1));
    assert_eq!(writer.metrics().uncheckpointed_records, 1);
}
