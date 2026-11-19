use super::*;
use crate::retention::{Action, Policy};
use crate::{NewRecord, ReaderConfig, Writer, WriterConfig};

const SEG: u64 = format::SEGMENT_SIZE_UNIT;

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
    assert_eq!(s.segments.count, 1);
    assert_eq!(s.segments.first_seq, 0);
    assert_eq!(s.segments.total_bytes, SEG);
    assert!(s.segments.oldest_created_unix_ns.is_some());
    assert_eq!(s.segments.before_checkpoint, 0);
    // The writer creates the marker with no valid slot.
    assert_eq!(s.checkpoint, None);
    assert_eq!(s.exposure.records, 0);
    assert_eq!(s.exposure.bytes, Some(0));
    assert_eq!(s.exposure.age, Some(Duration::ZERO));
    assert_eq!(s.exposure.oldest_local_ts_ns, None);
    assert_eq!(s.rates, None);
    assert_eq!(s.storage.time_to_full, None);
    assert!(s.readers.is_empty());
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
    assert_eq!(s.checkpoint, None);
    assert_eq!(s.exposure.records, 5);
    assert_eq!(s.exposure.bytes, Some(5 * frame(10)));
    assert_eq!(s.exposure.age, None);
    // The oldest uncovered record has no timestamp set.
    assert_eq!(s.exposure.oldest_local_ts_ns, None);
    let t = Thresholds::default().max_checkpoint_age(Duration::from_secs(3600));
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
    assert!(!c.clock_skew);
    assert!(c.written_unix_ns.unwrap() <= after_checkpoint);
    assert!(c.age.unwrap() < Duration::from_secs(60));
    assert_eq!(s.exposure.records, 0);
    assert_eq!(s.exposure.age, Some(Duration::ZERO));

    append(&mut writer, 3, 10, 9);
    let s = status(dir.path()).unwrap();
    assert_eq!(s.exposure.records, 3);
    assert_eq!(s.exposure.bytes, Some(3 * frame(10)));
    assert_eq!(s.exposure.oldest_local_ts_ns, Some(9));
    assert_eq!(s.exposure.age, s.checkpoint.unwrap().age);
    assert!(
        s.breaches(&Thresholds::default().max_checkpoint_age(Duration::from_secs(3600)))
            .is_empty()
    );
    assert_eq!(
        s.breaches(&Thresholds::default().max_checkpoint_age(Duration::ZERO)),
        vec![Breach::CheckpointAge]
    );
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
    assert_eq!(s.exposure.records, per);
    assert_eq!(s.exposure.oldest_local_ts_ns, Some(12));
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
    assert_eq!(s.exposure.records, 0);
}

#[test]
fn checkpoint_clock_stepped_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 2, 10, 0);
    writer.checkpoint().unwrap();
    append(&mut writer, 1, 10, 0);
    let marker = File::options()
        .write(true)
        .open(dir.path().join("checkpoint"))
        .unwrap();
    marker
        .set_modified(std::time::SystemTime::now() + Duration::from_secs(3600))
        .unwrap();
    let s = status(dir.path()).unwrap();
    let c = s.checkpoint.unwrap();
    assert!(c.clock_skew);
    assert_eq!(c.age, Some(Duration::ZERO));
    assert_eq!(s.exposure.age, Some(Duration::ZERO));

    // A forward step (or an old copy) makes the age large.
    marker
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    let s = status(dir.path()).unwrap();
    assert!(s.checkpoint.unwrap().age.unwrap() >= Duration::from_secs(3599));
    assert!(!s.checkpoint.unwrap().clock_skew);
}

#[test]
fn writer_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 3, 10, 0);
    writer.checkpoint().unwrap();
    drop(writer);
    // A PID that certainly doesn't exist any more: a child that has been reaped.
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    std::fs::write(dir.path().join("writer.lock"), pid.to_string()).unwrap();
    let s = status(dir.path()).unwrap();
    assert_eq!(s.writer.pid, Some(pid));
    assert_eq!(s.writer.alive, Some(false));
    assert_eq!(s.head.position.next_seq, 3);
    std::fs::remove_file(dir.path().join("writer.lock")).unwrap();
    let s = status(dir.path()).unwrap();
    assert_eq!(
        s.writer,
        WriterProcess {
            pid: None,
            alive: None
        }
    );
}

#[test]
fn sealed_newest_segment_without_successor() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    let per = records_per_segment(1000);
    append(&mut writer, per + 1, 1000, 0);
    drop(writer);
    // As if the writer died between sealing and... (or the successor was lost): the newest segment is sealed.
    std::fs::remove_file(journal::segment_path(dir.path(), per)).unwrap();
    let s = status(dir.path()).unwrap();
    assert_eq!(s.head.end, HeadEnd::Sealed);
    assert_eq!(s.head.position.next_seq, per);
    assert!(s.breaches(&Thresholds::default()).is_empty());
}

#[test]
fn invalid_head_is_reported_and_breaches() {
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
    assert_eq!(
        s.breaches(&Thresholds::default()),
        vec![Breach::HeadInvalid]
    );
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

#[test]
fn leftovers_of_recovery_and_spare_are_handled() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 2, 10, 0);
    drop(writer);
    // Files a crashed writer or a recovery in progress leaves behind: never segments.
    std::fs::write(dir.path().join("00000000000000000002.tape.tmp"), [1u8; 100]).unwrap();
    std::fs::create_dir(dir.path().join("quarantine")).unwrap();
    let s = status(dir.path()).unwrap();
    assert_eq!(s.segments.count, 1);
    assert_eq!(s.head.position.next_seq, 2);
    assert!(!s.storage.spare_segment);
    // A prepared spare of the right size counts as one more roll.
    let spare = File::create(dir.path().join(journal::SPARE_FILE)).unwrap();
    spare.set_len(SEG).unwrap();
    let with_spare = status(dir.path()).unwrap();
    assert!(with_spare.storage.spare_segment);
    let base = with_spare.storage.available_bytes / SEG;
    assert!(with_spare.storage.rolls_left == base + 1);
}

#[test]
fn storage_headroom_is_plausible() {
    let dir = tempfile::tempdir().unwrap();
    let _writer = Writer::open(dir.path(), config()).unwrap();
    let s = status(dir.path()).unwrap();
    assert!(s.storage.total_bytes > 0);
    assert!(s.storage.available_bytes <= s.storage.total_bytes);
    assert_eq!(s.storage.segment_size, SEG);
    assert_eq!(s.storage.rolls_left, s.storage.available_bytes / SEG);
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

#[test]
fn sampler_resumes_and_measures_rates() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 5, 100, 1);
    let mut sampler = Sampler::new(dir.path());
    let first = sampler.sample().unwrap();
    assert_eq!(first.head.position.next_seq, 5);
    assert_eq!(first.rates, None);
    std::thread::sleep(Duration::from_millis(20));
    append(&mut writer, 7, 100, 2);
    let second = sampler.sample().unwrap();
    assert_eq!(second.head.position.next_seq, 12);
    assert_eq!(second.head.last_local_ts_ns, Some(2));
    let rates = second.rates.unwrap();
    assert!(rates.interval >= Duration::from_millis(20));
    assert!(rates.records_per_sec > 0.0);
    assert!(rates.bytes_per_sec.unwrap() > 0.0);
    assert!(second.storage.time_to_full.is_some());
    // Nothing new: the carried timestamp stays, the rate is zero and no estimate is made.
    let third = sampler.sample().unwrap();
    assert_eq!(third.head, second.head);
    assert_eq!(third.rates.unwrap().records_per_sec, 0.0);
    assert_eq!(third.storage.time_to_full, None);
    // Across a roll.
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
    let mut image = std::fs::read(&path).unwrap();
    let cut = (64 + 2 * frame(10)) as usize;
    image[cut..].fill(0);
    let tmp = dir.path().join("copy");
    std::fs::write(&tmp, &image).unwrap();
    std::fs::rename(&tmp, &path).unwrap();
    let s = sampler.sample().unwrap();
    assert_eq!(s.head.position, Position::new(2, 0, cut as u64));
}

#[test]
fn reader_lag_in_process() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    let per = records_per_segment(1000);
    append(&mut writer, per + 3, 1000, 0);
    let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    assert_eq!(reader.cursor(), Position::new(0, 0, 64));
    let s = status(dir.path()).unwrap();
    let lag = s.lag(reader.cursor());
    assert_eq!(lag.records, per + 3);
    assert_eq!(lag.bytes, Some(SEG + s.head.position.offset - 64));
    for _ in 0..per + 1 {
        reader.try_next().unwrap().unwrap();
    }
    assert_eq!(
        reader.cursor(),
        Position::new(per + 1, per, 64 + frame(1000))
    );
    let lag = s.lag(reader.cursor());
    assert_eq!(lag.records, 2);
    assert_eq!(lag.bytes, Some(2 * frame(1000)));
    while reader.try_next().unwrap().is_some() {}
    assert_eq!(
        s.lag(reader.cursor()),
        Lag {
            records: 0,
            bytes: Some(0)
        }
    );
    // A cursor in a segment that is no longer listed has no byte lag; one past the head has none to make up.
    assert_eq!(s.lag(Position::new(5, 5, 64)).bytes, None);
    assert_eq!(
        s.lag(Position::new(u64::MAX, u64::MAX, 64)),
        Lag {
            records: 0,
            bytes: Some(0)
        }
    );
}

#[test]
fn distance_cases() {
    let sizes = [(0, 100), (10, 200), (20, 300)];
    let p = Position::new;
    assert_eq!(distance(&sizes, p(1, 0, 64), p(3, 0, 80)), Some(16));
    assert_eq!(distance(&sizes, p(3, 0, 80), p(1, 0, 64)), Some(0));
    assert_eq!(
        distance(&sizes, p(1, 0, 64), p(21, 20, 72)),
        Some(36 + 200 + 72)
    );
    assert_eq!(distance(&sizes, p(21, 20, 72), p(1, 0, 64)), Some(0));
    assert_eq!(distance(&sizes, p(1, 5, 64), p(21, 20, 72)), None);
    assert_eq!(distance(&sizes, p(1, 0, 64), p(31, 30, 72)), None);
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
    assert_eq!(s.writer.pid, Some(std::process::id()));
    let r = &s.readers[0];
    assert_eq!(r.state, BeaconState::Live);
    assert_eq!(r.lag.unwrap().records, 4 * per + 1);
    assert_eq!(r.lag.unwrap().bytes, None);
    assert_eq!(r.oldest_unread_local_ts_ns, None);
    assert_eq!(writer.metrics().retention_failures, 0);
}

#[test]
fn dangling_checkpoint_has_no_exposure_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    let per = records_per_segment(1000);
    append(&mut writer, 2, 1000, 0);
    writer.checkpoint().unwrap();
    append(&mut writer, per, 1000, 0);
    drop(writer);
    std::fs::remove_file(journal::segment_path(dir.path(), 0)).unwrap();
    let s = status(dir.path()).unwrap();
    assert_eq!(s.exposure.records, per);
    assert_eq!(s.exposure.bytes, None);
}

#[test]
fn beacons_live_stale_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    let now = crate::clock::realtime_ns();
    append(&mut writer, 10, 10, now);
    let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    for _ in 0..4 {
        reader.try_next().unwrap().unwrap();
    }
    let beacon = ReaderBeacon::create(dir.path(), "live-1", &reader).unwrap();
    assert_eq!(beacon.path(), dir.path().join("readers").join("live-1"));
    let readers = dir.path().join("readers");
    // A beacon whose owner died: valid contents, nobody holds the lock.
    std::fs::write(
        readers.join("dead"),
        encode_beacon(
            4242,
            Position::new(1, 0, 64 + frame(10)),
            now - 5_000_000_000,
        ),
    )
    .unwrap();
    std::fs::write(readers.join("garbage"), [7u8; 64]).unwrap();
    std::fs::write(readers.join("short"), [7u8; 10]).unwrap();
    // Ignored: invalid names, hidden files, directories, symlinks.
    std::fs::write(readers.join(".hidden"), [0u8; 64]).unwrap();
    std::fs::create_dir(readers.join("subdir")).unwrap();
    std::os::unix::fs::symlink(readers.join("dead"), readers.join("link")).unwrap();

    let s = status(dir.path()).unwrap();
    let names: Vec<_> = s.readers.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["dead", "garbage", "live-1", "short"]);
    let dead = &s.readers[0];
    assert_eq!(dead.state, BeaconState::Stale);
    assert_eq!(dead.pid, Some(4242));
    assert_eq!(
        dead.lag,
        Some(Lag {
            records: 9,
            bytes: Some(9 * frame(10))
        })
    );
    assert!(dead.update_age.unwrap() >= Duration::from_secs(5));
    assert_eq!(s.readers[1].state, BeaconState::Invalid);
    assert_eq!(s.readers[1].lag, None);
    assert_eq!(s.readers[3].state, BeaconState::Invalid);
    let live = &s.readers[2];
    assert_eq!(live.state, BeaconState::Live);
    assert_eq!(live.pid, Some(std::process::id()));
    assert_eq!(live.position, Some(reader.cursor()));
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
    assert!(
        matches!(&err, Error::Io { source, .. } if source.kind() == io::ErrorKind::WouldBlock),
        "{err}"
    );
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

#[test]
fn beacon_update_is_rate_limited() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 5, 10, 0);
    let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let mut beacon = ReaderBeacon::create(dir.path(), "r", &reader).unwrap();
    let read_back = |dir: &Path| status(dir).unwrap().readers[0].position.unwrap();
    // Unchanged position: no write.
    assert!(!beacon.update(&reader).unwrap());
    reader.try_next().unwrap().unwrap();
    // Moved, but within the interval since the write at creation.
    assert!(!beacon.update(&reader).unwrap());
    assert_eq!(read_back(dir.path()).next_seq, 0);
    beacon.flush(&reader).unwrap();
    assert_eq!(read_back(dir.path()).next_seq, 1);
    beacon.set_interval(Duration::ZERO);
    reader.try_next().unwrap().unwrap();
    assert!(beacon.update(&reader).unwrap());
    assert_eq!(read_back(dir.path()).next_seq, 2);
    assert!(
        beacon
            .update_position(Position::new(3, 0, 64 + 3 * frame(10)))
            .unwrap()
    );
    assert_eq!(read_back(dir.path()).next_seq, 3);
}

#[test]
fn beacon_in_another_directory() {
    let dir = tempfile::tempdir().unwrap();
    let beacons = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    append(&mut writer, 3, 10, 0);
    let reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let _beacon = ReaderBeacon::create_in(beacons.path(), "elsewhere", &reader).unwrap();
    assert!(status(dir.path()).unwrap().readers.is_empty());
    let s = Sampler::new(dir.path())
        .readers_dir(beacons.path())
        .sample()
        .unwrap();
    assert_eq!(s.readers.len(), 1);
    assert_eq!(s.readers[0].lag.unwrap().records, 3);
    assert!(!dir.path().join("readers").exists());
}

#[test]
fn beacon_encoding_round_trips_and_rejects_damage() {
    let p = Position::new(u64::MAX - 1, 12345, 4096);
    let b = encode_beacon(7, p, 99);
    assert_eq!(decode_beacon(&b), Some((7, p, 99)));
    for i in 0..BEACON_LEN {
        let mut damaged = b;
        damaged[i] ^= 1;
        assert_eq!(decode_beacon(&damaged), None, "byte {i}");
    }
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
    assert!(m.since_checkpoint.unwrap() < Duration::from_secs(60));
    assert_eq!(m.uncheckpointed_records, 0);
    assert_eq!(m.uncheckpointed_bytes, 0);
    append(&mut writer, 1, 1000, 0);
    assert_eq!(writer.metrics().uncheckpointed_bytes, frame(1000));
    drop(writer);

    // A new writer reads the marker but has no monotonic age for it; everything after the marker is dirty.
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
