//! Crash contract C14: retention removes a contiguous, checkpoint-covered prefix only.

use std::fs;
use std::time::Duration;

use tickertape::retention::{self, Action, Policy, Reason};
use tickertape::{Error, NewRecord, Reader, ReaderConfig, Writer};
use tickertape_tests::{read_all, segments, small_config};

fn append(writer: &mut Writer) {
    writer
        .append(&NewRecord {
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: &[42; 40_000],
        })
        .unwrap();
}

fn journal(checkpoint: bool) -> (tempfile::TempDir, Writer) {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    for _ in 0..6 {
        append(&mut writer);
    }
    if checkpoint {
        writer.checkpoint().unwrap();
    }
    (dir, writer)
}

#[test]
fn limits_and_checkpoint_boundary() {
    let (dir, writer) = journal(true);
    for (policy, count, reason) in [
        (Policy::default().max_segments(3), 3, Reason::MaxSegments),
        (Policy::default().max_bytes(2 * 65536), 4, Reason::MaxBytes),
        (Policy::default().max_age(Duration::ZERO), 5, Reason::MaxAge),
        (Policy::default().max_segments(0), 5, Reason::MaxSegments),
    ] {
        let plan = retention::plan(dir.path(), &policy).unwrap();
        assert_eq!(plan.segments().len(), count);
        assert!(plan.segments().iter().all(|s| s.reasons.contains(&reason)));
        assert!(plan.segments().iter().all(|s| s.base_seq < 5));
    }
    assert!(
        retention::plan(dir.path(), &Policy::default())
            .unwrap()
            .segments()
            .is_empty()
    );
    let policy = Policy::default().max_segments(4).max_bytes(3 * 65536);
    let plan = retention::plan(dir.path(), &policy).unwrap();
    assert_eq!(plan.segments().len(), 3);
    assert_eq!(
        plan.segments()[0].reasons,
        [Reason::MaxSegments, Reason::MaxBytes]
    );
    assert_eq!(plan.segments()[2].reasons, [Reason::MaxBytes]);
    let report = retention::apply(dir.path(), &plan, &Action::Delete).unwrap();
    assert_eq!(report.removed_segments, 3);
    assert_eq!(report.removed_bytes, 3 * 65536);
    assert_eq!(
        segments(dir.path()).iter().map(|s| s.0).collect::<Vec<_>>(),
        [3, 4, 5]
    );
    assert!(
        tickertape::inspect::verify(dir.path())
            .unwrap()
            .problems
            .is_empty()
    );
    drop(writer);
    assert_eq!(
        Writer::open(dir.path(), small_config()).unwrap().next_seq(),
        6
    );
    assert_eq!(read_all(dir.path()).len(), 3);
}

#[test]
fn no_checkpoint_no_removal() {
    let (dir, _writer) = journal(false);
    let policy = Policy::default()
        .max_segments(0)
        .max_bytes(0)
        .max_age(Duration::ZERO);
    assert!(
        retention::plan(dir.path(), &policy)
            .unwrap()
            .segments()
            .is_empty()
    );
    fs::write(dir.path().join("checkpoint"), [0; 64]).unwrap();
    assert!(
        retention::plan(dir.path(), &policy)
            .unwrap()
            .segments()
            .is_empty()
    );
}

#[test]
fn move_and_conflict_stop_at_oldest() {
    let (dir, _writer) = journal(true);
    let dest = tempfile::tempdir().unwrap();
    let plan = retention::plan(dir.path(), &Policy::default().max_segments(2)).unwrap();
    let first = plan.segments()[0].path.file_name().unwrap();
    fs::write(dest.path().join(first), b"keep me").unwrap();
    let action = Action::MoveTo(dest.path().into());
    assert!(retention::apply(dir.path(), &plan, &action).is_err());
    assert_eq!(segments(dir.path()).len(), 6);
    assert_eq!(fs::read(dest.path().join(first)).unwrap(), b"keep me");
    fs::remove_file(dest.path().join(first)).unwrap();
    let report = retention::apply(dir.path(), &plan, &action).unwrap();
    assert_eq!(report.removed_segments, 4);
    assert_eq!(segments(dest.path()).len(), 4);
    assert_eq!(segments(dir.path()).len(), 2);
}

#[test]
fn concurrent_archiver_and_missing_middle() {
    let (dir, _writer) = journal(true);
    let plan = retention::plan(dir.path(), &Policy::default().max_segments(2)).unwrap();
    fs::remove_file(&plan.segments()[0].path).unwrap();
    assert_eq!(
        retention::apply(dir.path(), &plan, &Action::Delete)
            .unwrap()
            .already_removed_segments,
        1
    );
    let (dir, _writer) = journal(true);
    let plan = retention::plan(dir.path(), &Policy::default().max_segments(2)).unwrap();
    fs::remove_file(&plan.segments()[1].path).unwrap();
    assert!(matches!(
        retention::apply(dir.path(), &plan, &Action::Delete),
        Err(Error::Corrupt { .. })
    ));
    assert!(plan.segments()[0].path.exists());
}

#[test]
fn apply_revalidates_checkpoint_and_identity() {
    let (dir, _writer) = journal(true);
    let plan = retention::plan(dir.path(), &Policy::default().max_segments(1)).unwrap();
    fs::write(dir.path().join("checkpoint"), [0; 131072]).unwrap();
    assert!(retention::apply(dir.path(), &plan, &Action::Delete).is_err());
    assert_eq!(segments(dir.path()).len(), 6);
    let (other, _other_writer) = journal(true);
    assert!(retention::apply(other.path(), &plan, &Action::Delete).is_err());
}

#[test]
fn writer_bounds_disk_and_reports_errors() {
    let dir = tempfile::tempdir().unwrap();
    let config = small_config().retention(Policy::default().max_segments(2), Action::Delete);
    let mut writer = Writer::open(dir.path(), config.clone()).unwrap();
    for _ in 0..40 {
        append(&mut writer);
        writer.checkpoint().unwrap();
        assert!(segments(dir.path()).len() <= 2);
    }
    assert_eq!(writer.retention_report().removed_segments, 38);
    assert!(writer.last_retention_error().is_none());
    assert!(
        tickertape::inspect::verify(dir.path())
            .unwrap()
            .problems
            .is_empty()
    );
    assert_eq!(read_all(dir.path()).len(), 2);
    drop(writer);
    assert_eq!(Writer::open(dir.path(), config).unwrap().next_seq(), 40);

    let (dir, mut writer) = journal(true);
    append(&mut writer);
    writer.checkpoint().unwrap();
    drop(writer);
    let dest = dir.path().join("missing");
    let config = small_config().retention(
        Policy::default().max_segments(1),
        Action::MoveTo(dest.clone()),
    );
    let mut writer = Writer::open(dir.path(), config).unwrap();
    assert!(writer.last_retention_error().is_some());
    append(&mut writer);
    writer.checkpoint().unwrap();
    assert!(writer.last_retention_error().is_some());
    fs::create_dir(dest).unwrap();
    writer.checkpoint().unwrap();
    assert!(writer.retention_report().removed_segments > 0);
}

#[test]
fn reader_discarded_at_removed_segment_end() {
    let (dir, _writer) = journal(true);
    let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let plan = retention::plan(dir.path(), &Policy::default().max_segments(5)).unwrap();
    retention::apply(dir.path(), &plan, &Action::Delete).unwrap();
    assert_eq!(reader.next_owned().unwrap().unwrap().seq, 0);
    assert!(matches!(reader.next_owned(), Err(Error::Discarded { .. })));
    reader.seek(1).unwrap();
    assert_eq!(reader.next_owned().unwrap().unwrap().seq, 1);
    assert!(matches!(reader.seek(0), Err(Error::SeqUnavailable { .. })));
}

#[test]
fn checkpoint_segment_and_newer_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    for _ in 0..3 {
        append(&mut writer);
    }
    writer.checkpoint().unwrap();
    for _ in 0..3 {
        append(&mut writer);
    }
    let plan = retention::plan(dir.path(), &Policy::default().max_segments(0)).unwrap();
    assert_eq!(
        plan.segments()
            .iter()
            .map(|s| s.base_seq)
            .collect::<Vec<_>>(),
        [0, 1]
    );
    retention::apply(dir.path(), &plan, &Action::Delete).unwrap();
    assert_eq!(
        segments(dir.path()).iter().map(|s| s.0).collect::<Vec<_>>(),
        [2, 3, 4, 5]
    );
}

/// Use an already mounted second local filesystem, if one is available without privileged setup.
fn second_filesystem(source: &std::path::Path) -> Option<tempfile::TempDir> {
    use std::os::unix::fs::MetadataExt;
    let device = fs::metadata(source).unwrap().dev();
    for path in ["/dev/shm", "/tmp", "/Volumes/tickertape-retention-test"] {
        if fs::metadata(path).is_ok_and(|m| m.is_dir() && m.dev() != device) {
            if let Ok(dir) = tempfile::tempdir_in(path) {
                return Some(dir);
            }
        }
    }
    eprintln!("skip cross-device path: no writable second filesystem available");
    None
}

#[test]
fn cross_device_move() {
    let (dir, _writer) = journal(true);
    let Some(dest) = second_filesystem(dir.path()) else {
        return;
    };
    let plan = retention::plan(dir.path(), &Policy::default().max_segments(1)).unwrap();
    let originals: Vec<_> = plan
        .segments()
        .iter()
        .map(|s| fs::read(&s.path).unwrap())
        .collect();
    retention::apply(dir.path(), &plan, &Action::MoveTo(dest.path().into())).unwrap();
    for (seg, bytes) in plan.segments().iter().zip(originals) {
        assert!(!seg.path.exists());
        assert_eq!(
            fs::read(dest.path().join(seg.path.file_name().unwrap())).unwrap(),
            bytes
        );
    }
}

#[test]
fn interrupted_move_preserves_original_and_retries() {
    // Simulate each crash point: original only, partial copy + original, full copy + original.
    // Existing destination entries must be resolved before retry (ADR 0016's no-replacement rule).
    for copied in [None, Some(128), Some(65536)] {
        let (dir, writer) = journal(true);
        let dest = tempfile::tempdir().unwrap();
        let plan = retention::plan(dir.path(), &Policy::default().max_segments(1)).unwrap();
        let first = &plan.segments()[0];
        let target = dest.path().join(first.path.file_name().unwrap());
        if let Some(len) = copied {
            let image = fs::read(&first.path).unwrap();
            fs::write(&target, &image[..len]).unwrap();
        }
        drop(writer);
        let writer = Writer::open(dir.path(), small_config()).unwrap();
        assert_eq!(writer.next_seq(), 6);
        let action = Action::MoveTo(dest.path().into());
        if copied.is_some() {
            assert!(retention::apply(dir.path(), &plan, &action).is_err());
            assert!(first.path.exists());
            fs::remove_file(target).unwrap(); // Explicitly resolve the simulated interruption's conflict.
        }
        assert_eq!(
            retention::apply(dir.path(), &plan, &action)
                .unwrap()
                .removed_segments,
            5
        );
        assert!(
            tickertape::inspect::verify(dir.path())
                .unwrap()
                .problems
                .is_empty()
        );
    }
}

#[test]
#[ignore = "long: 50 SIGKILLs during archiving; run with --release -- --ignored"]
fn retention_kill_loop_50() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use tickertape_tests::Rng;
    let mut rng = Rng::from_env("retention_kill_loop_50");
    let parent = tempfile::tempdir().unwrap();
    let cross_device = second_filesystem(parent.path());
    // How long the first move of a run takes here, renamed and copied: the kills land in four times that after the
    // archiver is ready, so that they fall before, between and after its removals however loaded the machine is (issue
    // #73). The slowest of three, in microseconds.
    let first_move = |copy: bool| {
        (0..3)
            .map(|_| {
                let dir = tempfile::tempdir_in(parent.path()).unwrap();
                let mut writer = Writer::open(dir.path(), small_config()).unwrap();
                for _ in 0..12 {
                    append(&mut writer);
                }
                writer.checkpoint().unwrap();
                drop(writer);
                let archive =
                    tempfile::tempdir_in(cross_device.as_ref().map_or(parent.path(), |d| d.path()))
                        .unwrap();
                let start = std::time::Instant::now();
                let plan =
                    retention::plan(dir.path(), &Policy::default().max_segments(11)).unwrap();
                if copy {
                    tickertape::fault::fail_next(tickertape::fault::Point::RetentionRename, 18);
                }
                let action = Action::MoveTo(archive.path().into());
                assert_eq!(
                    retention::apply(dir.path(), &plan, &action)
                        .unwrap()
                        .removed_segments,
                    1
                );
                start.elapsed().as_micros() as u64
            })
            .max()
            .unwrap()
            .max(100)
    };
    let (renamed, copied) = (first_move(false), first_move(true));
    let (mut removed_total, mut between) = (0, 0);
    for iteration in 0..50 {
        let removed_before = removed_total;
        let dir = tempfile::tempdir_in(parent.path()).unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        for _ in 0..12 {
            append(&mut writer);
        }
        writer.checkpoint().unwrap();
        drop(writer);
        let archive =
            tempfile::tempdir_in(cross_device.as_ref().map_or(parent.path(), |d| d.path()))
                .unwrap();
        let before: Vec<_> = segments(dir.path())
            .into_iter()
            .map(|(_, path)| {
                (
                    path.file_name().unwrap().to_owned(),
                    fs::read(path).unwrap(),
                )
            })
            .collect();
        let mut command = Command::new(env!("CARGO_BIN_EXE_tt-archiver"));
        command
            .arg(dir.path())
            .arg(archive.path())
            .stdout(Stdio::piped());
        let copy = cross_device.is_some() || iteration % 2 == 1;
        if cross_device.is_none() && copy {
            command.arg("force-copy");
        }
        let mut child = command.spawn().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut ready = String::new();
        stdout.read_line(&mut ready).unwrap();
        assert_eq!(ready.trim(), "ready");
        let first = if copy { copied } else { renamed };
        std::thread::sleep(Duration::from_micros(rng.below(4 * first)));
        child.kill().unwrap();
        child.wait().unwrap();
        for (name, original) in before {
            let journal_path = dir.path().join(&name);
            let archive_path = archive.path().join(&name);
            if journal_path.exists() {
                assert_eq!(
                    fs::read(journal_path).unwrap(),
                    original,
                    "iteration {iteration}"
                );
            } else {
                removed_total += 1;
                assert_eq!(
                    fs::read(archive_path).unwrap(),
                    original,
                    "iteration {iteration}: removed segment lost"
                );
            }
        }
        // A run moves 4, 4 and then 3 of the 12: any other count means the kill came between two removals of one run.
        if ![0, 4, 8, 11].contains(&(removed_total - removed_before)) {
            between += 1;
        }
        assert_eq!(
            Writer::open(dir.path(), small_config()).unwrap().next_seq(),
            12
        );
        assert!(
            tickertape::inspect::verify(dir.path())
                .unwrap()
                .problems
                .is_empty()
        );
    }
    assert!(removed_total > 0, "kills never overlapped a completed move");
    eprintln!(
        "retention_kill_loop_50: 50 kills, {removed_total} preserved removals, {between} kills inside a run; first move {renamed} µs renamed, {copied} µs copied"
    );
}

#[test]
fn injected_cross_device_copy_and_interrupted_sync() {
    use tickertape::fault::{self, Point};
    // Runs even on hosts without a cheaply available second filesystem.
    for fail_sync in [false, true] {
        let (dir, writer) = journal(true);
        let dest = tempfile::tempdir().unwrap();
        let plan = retention::plan(dir.path(), &Policy::default().max_segments(5)).unwrap();
        let first = &plan.segments()[0];
        let original = fs::read(&first.path).unwrap();
        fault::fail_next(Point::RetentionRename, 18); // EXDEV on both supported OSes.
        if fail_sync {
            fault::fail_next(Point::RetentionCopySync, 5);
        } // EIO
        let action = Action::MoveTo(dest.path().into());
        let result = retention::apply(dir.path(), &plan, &action);
        let target = dest.path().join(first.path.file_name().unwrap());
        assert_eq!(fs::read(&target).unwrap(), original);
        if fail_sync {
            assert!(result.is_err());
            assert!(first.path.exists());
            drop(writer);
            assert_eq!(
                Writer::open(dir.path(), small_config()).unwrap().next_seq(),
                6
            );
            assert!(retention::apply(dir.path(), &plan, &action).is_err());
            fs::remove_file(target).unwrap();
            fault::fail_next(Point::RetentionRename, 18);
            assert_eq!(
                retention::apply(dir.path(), &plan, &action)
                    .unwrap()
                    .removed_segments,
                1
            );
        } else {
            assert_eq!(result.unwrap().removed_segments, 1);
            assert!(!first.path.exists());
        }
        fault::clear();
    }
}

#[test]
fn writer_counts_partial_runs_and_sync_errors() {
    use tickertape::fault::{self, Point};
    let (dir, writer) = journal(true);
    drop(writer);
    let dest = tempfile::tempdir().unwrap();
    let paths = segments(dir.path());
    fs::write(
        dest.path().join(paths[1].1.file_name().unwrap()),
        b"conflict",
    )
    .unwrap();
    let config = small_config().retention(
        Policy::default().max_segments(1),
        Action::MoveTo(dest.path().into()),
    );
    let mut writer = Writer::open(dir.path(), config).unwrap();
    assert_eq!(writer.retention_report().removed_segments, 1);
    assert!(writer.last_retention_error().is_some());
    assert!(paths[1].1.exists());
    assert!(paths[2].1.exists());
    fs::remove_file(dest.path().join(paths[1].1.file_name().unwrap())).unwrap();
    fault::fail_next(Point::RetentionDestinationSync, 5);
    writer.checkpoint().unwrap();
    assert_eq!(writer.retention_report().removed_segments, 2);
    assert!(paths[2].1.exists());
    assert!(writer.last_retention_error().is_some());
    writer.checkpoint().unwrap();
    assert_eq!(writer.retention_report().removed_segments, 5);
    assert_eq!(segments(dir.path()).len(), 1);
    fault::clear();
}

#[test]
fn age_uses_successor_and_never_skips_oldest() {
    use std::os::unix::fs::FileExt;
    use tickertape::format::{self, SegmentHeader};
    let (dir, writer) = journal(true);
    drop(writer);
    let paths = segments(dir.path());
    let future = tickertape::clock::realtime_ns() + 60_000_000_000;
    for (i, (base, path)) in paths.iter().enumerate() {
        let header = SegmentHeader {
            base_seq: *base,
            segment_size: 65536,
            created_unix_ns: if i == 1 { future } else { 1 },
        };
        tickertape_tests::open_raw(path)
            .write_all_at(&format::encode_segment_header(&header), 0)
            .unwrap();
    }
    let policy = Policy::default().max_age(Duration::from_secs(1));
    assert!(
        retention::plan(dir.path(), &policy)
            .unwrap()
            .segments()
            .is_empty()
    );
    let plan = retention::plan(dir.path(), &policy.max_segments(5)).unwrap();
    assert_eq!(plan.segments().len(), 5);
    assert_eq!(plan.segments()[0].reasons, [Reason::MaxSegments]);
    assert_eq!(plan.segments()[1].reasons, [Reason::MaxAge]);
}

#[test]
fn stale_identity_and_rolled_back_marker_are_refused() {
    use tickertape::format::{self, CheckpointSlot};
    let (dir, _writer) = journal(true);
    let plan = retention::plan(dir.path(), &Policy::default().max_segments(1)).unwrap();
    let first = &plan.segments()[0].path;
    let replacement = dir.path().join("replacement");
    fs::copy(first, &replacement).unwrap();
    fs::rename(replacement, first).unwrap();
    assert!(matches!(
        retention::apply(dir.path(), &plan, &Action::Delete),
        Err(Error::InvalidConfig(_))
    ));
    assert_eq!(segments(dir.path()).len(), 6);

    let plan = retention::plan(dir.path(), &Policy::default().max_segments(1)).unwrap();
    let slot = CheckpointSlot {
        generation: 10,
        next_seq: 1,
        segment_base_seq: 0,
        offset: 40104,
    };
    fs::write(
        dir.path().join("checkpoint"),
        format::encode_checkpoint_slot(&slot),
    )
    .unwrap();
    assert!(retention::apply(dir.path(), &plan, &Action::Delete).is_err());
    assert_eq!(segments(dir.path()).len(), 6);
}

// Readers racing with retention. Each test removes files at exactly the racy point with a fault-injection hook, the
// way a concurrent retention run (the writer's policy, or `tickertape archive` in another process) could.

/// Applies retention with `max_segments(keep)` to `dir`, as a concurrent archiver would.
fn retain(dir: &std::path::Path, keep: usize) {
    let plan = retention::plan(dir, &Policy::default().max_segments(keep)).unwrap();
    retention::apply(dir, &plan, &Action::Delete).unwrap();
}

/// A safe reader, or a zero-copy one.
fn open_reader(dir: &std::path::Path, mapped: bool) -> tickertape::Result<Reader> {
    if mapped {
        // SAFETY: only this process's tickertape writer modifies the journal while the reader exists.
        unsafe { Reader::open_mapped(dir, ReaderConfig::default()) }
    } else {
        Reader::open(dir, ReaderConfig::default())
    }
}

/// Arms `action` to run the next time this thread reaches `point`, after `skip` earlier passes.
fn at(point: tickertape::fault::Point, skip: usize, action: impl FnOnce() + 'static) {
    tickertape::fault::clear();
    tickertape::fault::run_at(point, skip, action);
}

#[test]
fn reader_open_races_retention() {
    use tickertape::fault::Point;
    for mapped in [false, true] {
        let (dir, _writer) = journal(true);
        let path = dir.path().to_path_buf();
        at(Point::ReaderOpen, 0, move || retain(&path, 3));
        let mut reader = open_reader(dir.path(), mapped).unwrap();
        assert_eq!(
            reader.next_owned().unwrap().unwrap().seq,
            3,
            "mapped {mapped}"
        );
    }
}

/// Removes the oldest segment at each of this thread's next `left` listings by a reader, as a retention run whose
/// removals take as long as a reader's listing would.
fn remove_oldest_after_each_listing(dir: std::path::PathBuf, left: usize) {
    if left > 0 {
        tickertape::fault::run_at(tickertape::fault::Point::ReaderOpen, 0, move || {
            retain(&dir, segments(&dir).len() - 1);
            remove_oldest_after_each_listing(dir, left - 1);
        });
    }
}

/// A journal of 12 segments of one record each, checkpointed, with its writer.
fn twelve_segments() -> (tempfile::TempDir, Writer) {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    for _ in 0..12 {
        append(&mut writer);
    }
    writer.checkpoint().unwrap();
    assert_eq!(segments(dir.path()).len(), 12);
    (dir, writer)
}

/// A retention run removes the segments one after another, oldest first, and on tmpfs (where syncing the directory
/// does nothing) a removal takes about as long as a reader's listing and open. Each removal here comes right after
/// the reader's listing: had the reader listed again for each try, the run would have removed every segment it chose,
/// and it would have returned an I/O "not found" error after its eighth listing. It opens the next segment of its
/// listing instead.
#[test]
fn reader_open_keeps_up_with_a_retention_run_as_fast_as_its_listings() {
    for mapped in [false, true] {
        let (dir, _writer) = twelve_segments();
        let held = segments(dir.path()).len();
        tickertape::fault::clear();
        remove_oldest_after_each_listing(dir.path().to_path_buf(), 10);
        let opened = open_reader(dir.path(), mapped);
        tickertape::fault::clear();
        let mut reader = opened.unwrap_or_else(|e| panic!("mapped {mapped}: {e:?}"));
        // One listing, so one removal; the reader starts at the segment after the removed one.
        assert_eq!(segments(dir.path()).len(), held - 1, "mapped {mapped}");
        assert_eq!(
            reader.next_owned().unwrap().unwrap().seq,
            1,
            "mapped {mapped}"
        );
    }
}

/// A listing keeps its 8 oldest segments. When all of them are gone before the reader opens one, it lists again and
/// starts at the oldest segment that remains.
#[test]
fn reader_open_lists_again_once_the_oldest_segments_it_listed_are_gone() {
    use tickertape::fault::Point;
    for mapped in [false, true] {
        let (dir, _writer) = twelve_segments();
        let path = dir.path().to_path_buf();
        at(Point::ReaderOpen, 0, move || retain(&path, 3));
        let opened = open_reader(dir.path(), mapped);
        tickertape::fault::clear();
        let mut reader = opened.unwrap_or_else(|e| panic!("mapped {mapped}: {e:?}"));
        assert_eq!(segments(dir.path()).len(), 3, "mapped {mapped}");
        assert_eq!(
            reader.next_owned().unwrap().unwrap().seq,
            9,
            "mapped {mapped}"
        );
    }
}

/// A durable-only reader's `seek_to_end` starts at the oldest segment when there is no valid marker, or when the marker
/// it read names the oldest segment (read just before the writer moved on and retention removed that segment). It keeps
/// up with a retention run as `Reader::open` does.
#[test]
fn durable_only_seek_to_end_at_the_oldest_segment_keeps_up_with_a_retention_run() {
    use tickertape::fault::Point;
    for stale in [false, true] {
        for mapped in [false, true] {
            let case = format!("stale marker {stale}, mapped {mapped}");
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join("checkpoint");
            let mut writer = Writer::open(dir.path(), small_config()).unwrap();
            for _ in 0..3 {
                append(&mut writer);
            }
            writer.checkpoint().unwrap();
            // Names segment 2 at its end: next_seq 3, the base of the next segment.
            let old_marker = fs::read(&marker).unwrap();
            for _ in 0..9 {
                append(&mut writer);
            }
            writer.checkpoint().unwrap();
            let current = fs::read(&marker).unwrap();
            let config = ReaderConfig::default().durable_only(true);
            let mut reader = if mapped {
                // SAFETY: only this process's tickertape writer modifies the journal while the reader exists.
                unsafe { Reader::open_mapped(dir.path(), config) }.unwrap()
            } else {
                Reader::open(dir.path(), config).unwrap()
            };
            let set_aside = dir.path().join("checkpoint.aside");
            if stale {
                // Segments 0 to 2 are gone, and the reader reads the old marker: segment 3 is the oldest and the one
                // whose base is the marker's next_seq.
                retain(dir.path(), 9);
                fs::write(&marker, &old_marker).unwrap();
            } else {
                // No marker: the reader starts at the oldest segment, here segment 0.
                fs::rename(&marker, &set_aside).unwrap();
            }
            let held = segments(dir.path()).len();
            let path = dir.path().to_path_buf();
            let (marker_at, aside) = (marker.clone(), set_aside.clone());
            // After the reader's listing: the current marker is back (retention needs it), and the oldest segment goes.
            at(Point::ReaderOpen, 0, move || {
                if stale {
                    fs::write(&marker_at, &current).unwrap();
                } else {
                    fs::rename(&aside, &marker_at).unwrap();
                }
                retain(&path, segments(&path).len() - 1);
                remove_oldest_after_each_listing(path, 9);
            });
            let sought = reader.seek_to_end();
            tickertape::fault::clear();
            sought.unwrap_or_else(|e| panic!("{case}: {e:?}"));
            assert_eq!(segments(dir.path()).len(), held - 1, "{case}");
            // With a marker, it started at segment 4 and read on to the marker now in place; without one, it is at
            // the start of segment 1.
            assert_eq!(reader.position(), if stale { 12 } else { 1 }, "{case}");
            drop(writer);
        }
    }
}

/// An archiver other than the writer takes a segment out of the journal under a temporary name before it lets it go,
/// and puts it back if a check after that fails (ADR 0055). A reader that lists the segment and opens it while it is
/// out starts at the next segment of its listing, skipping the records of the segment that goes back, as crash
/// contract C14 allows.
#[test]
fn reader_opening_while_a_segment_is_out_starts_after_it() {
    use tickertape::fault::Point;
    for mapped in [false, true] {
        let (dir, _writer) = journal(true);
        let oldest = segments(dir.path())[0].1.clone();
        let out = std::path::PathBuf::from(format!(
            "{}.{}.{}.removing",
            oldest.display(),
            std::process::id(),
            tickertape::clock::realtime_ns()
        ));
        let (from, to) = (oldest.clone(), out.clone());
        at(Point::ReaderOpen, 0, move || {
            fs::rename(&from, &to).unwrap()
        });
        let opened = open_reader(dir.path(), mapped);
        tickertape::fault::clear();
        let mut reader = opened.unwrap_or_else(|e| panic!("mapped {mapped}: {e:?}"));
        // Put back.
        fs::rename(&out, &oldest).unwrap();
        assert_eq!(
            reader.next_owned().unwrap().unwrap().seq,
            1,
            "mapped {mapped}"
        );
        let mut again = open_reader(dir.path(), mapped).unwrap();
        assert_eq!(
            again.next_owned().unwrap().unwrap().seq,
            0,
            "mapped {mapped}"
        );
    }
}

#[test]
fn seek_races_retention() {
    use tickertape::fault::Point;
    for mapped in [false, true] {
        let (dir, _writer) = journal(true);
        let mut reader = open_reader(dir.path(), mapped).unwrap();
        // The segment holding the requested record disappears between the listing and the open.
        let path = dir.path().to_path_buf();
        at(Point::ReaderOpen, 0, move || retain(&path, 2));
        match reader.seek(2) {
            Err(Error::SeqUnavailable {
                requested: 2,
                first: 4,
                ..
            }) => {}
            other => panic!("mapped {mapped}: {other:?}"),
        }
        reader.seek(4).unwrap();
        assert_eq!(reader.next_owned().unwrap().unwrap().seq, 4);
        // A removal that leaves the chosen segment in place doesn't matter.
        let path = dir.path().to_path_buf();
        at(Point::ReaderOpen, 0, move || {
            fs::remove_file(segments(&path)[0].1.clone()).unwrap()
        });
        reader.seek(5).unwrap();
        assert_eq!(reader.next_owned().unwrap().unwrap().seq, 5);
    }
}

#[test]
fn seal_races_retention() {
    use tickertape::fault::Point;
    for mapped in [false, true] {
        let (dir, _writer) = journal(true);
        let mut reader = open_reader(dir.path(), mapped).unwrap();
        assert_eq!(reader.next_owned().unwrap().unwrap().seq, 0);
        // Retention removes the reader's segment and its successor between the identity check and the open of the
        // successor: the reader's segment is gone, so this is discarded history, not a broken chain.
        let path = dir.path().to_path_buf();
        at(Point::ReaderSeal, 0, move || retain(&path, 4));
        match reader.next_owned() {
            Err(Error::Discarded { seq: 1, .. }) => {}
            other => panic!("mapped {mapped}: {other:?}"),
        }
        assert!(matches!(
            reader.seek(1),
            Err(Error::SeqUnavailable { first: 2, .. })
        ));
        reader.seek(2).unwrap();
        assert_eq!(reader.next_owned().unwrap().unwrap().seq, 2);
        // Only the reader's segment goes: the successor opens, and the recheck after it reports the removal.
        let path = dir.path().to_path_buf();
        at(Point::ReaderSeal, 0, move || retain(&path, 3));
        assert!(matches!(
            reader.next_owned(),
            Err(Error::Discarded { seq: 3, .. })
        ));
    }
}

#[test]
fn missing_successor_with_segment_in_place_is_a_broken_chain() {
    use tickertape::fault::Point;
    for mapped in [false, true] {
        let (dir, _writer) = journal(true);
        let mut reader = open_reader(dir.path(), mapped).unwrap();
        assert_eq!(reader.next_owned().unwrap().unwrap().seq, 0);
        // Not something retention does (it removes the oldest segment first): the chain is broken.
        let successor = segments(dir.path())[1].1.clone();
        at(Point::ReaderSeal, 0, move || {
            fs::remove_file(successor).unwrap()
        });
        match reader.next_owned() {
            Err(Error::Corrupt {
                reason: tickertape::CorruptReason::BrokenChain(1),
                ..
            }) => {}
            other => panic!("mapped {mapped}: {other:?}"),
        }
        tickertape::fault::clear();
    }
}

/// Makes `dir` unsearchable, so that a `stat` of anything in it fails with `EACCES`. Returns false (and restores it)
/// if that has no effect, as when the tests run as root.
fn deny_search(dir: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).unwrap();
    let denied = fs::metadata(dir.join("probe"))
        .is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied);
    if !denied {
        allow_search(dir);
    }
    denied
}

fn allow_search(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
}

fn is_permission_denied(e: &Error) -> bool {
    matches!(e, Error::Io { source, .. } if source.kind() == std::io::ErrorKind::PermissionDenied)
}

/// Only "not found" means a segment is gone. A `stat` that fails for another reason (here `EACCES`; in the field also
/// `EIO` or `ENOMEM`) says nothing about whether the file is still in place, so it is an I/O error: it must not turn
/// into `Error::Discarded` for a reader at a seal, or hide a broken chain in `verify`.
#[test]
fn stat_errors_are_not_mistaken_for_removal() {
    use tickertape::fault::Point;
    for mapped in [false, true] {
        let (dir, _writer) = journal(true);
        let mut reader = open_reader(dir.path(), mapped).unwrap();
        assert_eq!(reader.next_owned().unwrap().unwrap().seq, 0);
        if !deny_search(dir.path()) {
            eprintln!(
                "stat_errors_are_not_mistaken_for_removal: permissions have no effect (root?); skipped"
            );
            return;
        }
        let result = reader.next_owned();
        allow_search(dir.path());
        match result {
            Err(e) if is_permission_denied(&e) => {}
            other => panic!("mapped {mapped}: {other:?}"),
        }
        assert_eq!(reader.next_owned().unwrap().unwrap().seq, 1);
    }

    // `verify`: the newest listed segment is sealed and its successor missing while it is still in place, but the
    // check for the successor fails with `EACCES`.
    let (dir, writer) = journal(true);
    let writer = std::rc::Rc::new(std::cell::RefCell::new(writer));
    let w = writer.clone();
    at(Point::InspectOpen, 5, move || append(&mut w.borrow_mut()));
    let denied = std::rc::Rc::new(std::cell::Cell::new(false));
    let (path, d) = (dir.path().to_path_buf(), denied.clone());
    tickertape::fault::run_at(Point::InspectSeal, 0, move || {
        fs::remove_file(segments(&path)[6].1.clone()).unwrap();
        d.set(deny_search(&path));
    });
    let result = tickertape::inspect::verify(dir.path());
    allow_search(dir.path());
    tickertape::fault::clear();
    if denied.get() {
        match result {
            Err(e) if is_permission_denied(&e) => {}
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn inspection_races_retention() {
    use tickertape::fault::Point;
    use tickertape::inspect;
    let bases = |s: &[inspect::SegmentInfo]| s.iter().map(|s| s.base_seq).collect::<Vec<_>>();

    // Segment 0 is opened, then retention removes 0..=2 before segment 1 is opened.
    let (dir, _writer) = journal(true);
    let path = dir.path().to_path_buf();
    at(Point::InspectOpen, 1, move || retain(&path, 3));
    assert_eq!(
        bases(&inspect::list_segments(dir.path()).unwrap()),
        [3, 4, 5]
    );

    let (dir, _writer) = journal(true);
    let path = dir.path().to_path_buf();
    at(Point::InspectOpen, 1, move || retain(&path, 3));
    let report = inspect::verify(dir.path()).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    assert_eq!(
        report
            .segments
            .iter()
            .map(|s| s.info.as_ref().unwrap().base_seq)
            .collect::<Vec<_>>(),
        [3, 4, 5]
    );
    assert_eq!(
        (report.records, report.first_seq, report.last_seq),
        (3, Some(3), Some(5))
    );
    tickertape::fault::clear();
}

#[test]
fn verify_tolerates_a_roll_during_the_scan() {
    use tickertape::fault::Point;
    // The writer fills and seals the newest listed segment before verify scans it: the successor exists, so the
    // seal isn't a broken chain.
    let (dir, writer) = journal(true);
    let writer = std::rc::Rc::new(std::cell::RefCell::new(writer));
    let w = writer.clone();
    at(Point::InspectOpen, 5, move || append(&mut w.borrow_mut()));
    let report = tickertape::inspect::verify(dir.path()).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    assert_eq!(report.segments.len(), 6);
    assert_eq!(report.last_seq, Some(5));
    tickertape::fault::clear();
}

/// A rolling writer links the next segment before it seals the current one. If it is descheduled in between for
/// longer than one re-scan interval, `verify` must still not report the unsealed segment as a broken chain: it scans
/// a transient state again, with growing pauses, for over half a second. With no writer running, the same state is a
/// problem (a crash left it behind).
#[test]
fn verify_waits_for_a_writer_stalled_between_linking_and_sealing() {
    use std::os::unix::fs::FileExt;
    use tickertape::format;
    use tickertape_tests::{SMALL_SEGMENT, create_raw_segment, end_of_records, open_raw};
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append(&mut writer);
    drop(writer);
    let path = segments(dir.path())[0].1.clone();
    let (end, next) = end_of_records(&path);
    create_raw_segment(dir.path(), next, SMALL_SEGMENT);
    let seal = move |path: &std::path::Path| {
        let raw = open_raw(path);
        raw.write_all_at(&format::seal_crc(next).to_le_bytes(), end + 4)
            .unwrap();
        raw.write_all_at(&format::SEAL_MARKER.to_le_bytes(), end)
            .unwrap();
    };
    // Nobody seals: a broken chain.
    let report = tickertape::inspect::verify(dir.path()).unwrap();
    assert!(
        matches!(
            report.problems.as_slice(),
            [tickertape::inspect::Problem {
                reason: tickertape::CorruptReason::BrokenChain(_),
                ..
            }]
        ),
        "{:?}",
        report.problems
    );
    // The "writer" seals only after verify's first scan and its first re-scan both found the segment unsealed, just
    // before the second re-scan (hook point `InspectRescan` runs before each re-scan): verify must keep scanning
    // rather than give up after one re-scan, and stop once the seal is there. The hook counts the re-scans and
    // re-arms itself.
    fn count_rescans(count: std::rc::Rc<std::cell::Cell<u32>>, seal: std::rc::Rc<dyn Fn()>) {
        tickertape::fault::run_at(tickertape::fault::Point::InspectRescan, 0, move || {
            count.set(count.get() + 1);
            if count.get() == 2 {
                seal();
            }
            count_rescans(count, seal);
        });
    }
    let rescans = std::rc::Rc::new(std::cell::Cell::new(0));
    tickertape::fault::clear();
    count_rescans(rescans.clone(), std::rc::Rc::new(move || seal(&path)));
    let report = tickertape::inspect::verify(dir.path()).unwrap();
    tickertape::fault::clear();
    assert_eq!(rescans.get(), 2, "re-scans of the unsealed segment");
    assert!(report.problems.is_empty(), "{:?}", report.problems);
}

#[test]
fn verify_tolerates_retention_after_a_roll_during_the_scan() {
    use tickertape::fault::Point;
    // The writer seals the newest listed segment before verify scans it; then, before verify looks for the
    // successor, it rolls again and retention removes the scanned segment and its successor. The missing successor
    // isn't a broken chain: the segment that points to it is gone too.
    let (dir, writer) = journal(true);
    let writer = std::rc::Rc::new(std::cell::RefCell::new(writer));
    let w = writer.clone();
    at(Point::InspectOpen, 5, move || append(&mut w.borrow_mut()));
    let (w, path) = (writer.clone(), dir.path().to_path_buf());
    tickertape::fault::run_at(Point::InspectSeal, 0, move || {
        let mut w = w.borrow_mut();
        append(&mut w);
        w.checkpoint().unwrap();
        retain(&path, 1);
    });
    let report = tickertape::inspect::verify(dir.path()).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    assert_eq!(segments(dir.path()).len(), 1);
    // A sealed newest segment whose successor is missing while it is still in place is still a problem.
    let (dir, writer) = journal(true);
    let writer = std::rc::Rc::new(std::cell::RefCell::new(writer));
    let w = writer.clone();
    at(Point::InspectOpen, 5, move || append(&mut w.borrow_mut()));
    let path = dir.path().to_path_buf();
    tickertape::fault::run_at(Point::InspectSeal, 0, move || {
        fs::remove_file(segments(&path)[6].1.clone()).unwrap()
    });
    let report = tickertape::inspect::verify(dir.path()).unwrap();
    assert!(
        matches!(
            report.problems.as_slice(),
            [tickertape::inspect::Problem {
                reason: tickertape::CorruptReason::BrokenChain(6),
                ..
            }]
        ),
        "{:?}",
        report.problems
    );
    tickertape::fault::clear();
}

/// An archiver's `retention::apply` syncs the checkpoint marker it read before it removes anything (C14): a writer
/// writes a marker slot before its barrier, so without the sync a power loss could bring back the previous marker,
/// naming a segment the archiver had removed durably. A failed sync removes nothing.
#[test]
fn archiver_syncs_the_marker_before_removing() {
    use tickertape::fault::{self, Point};
    let (dir, writer) = journal(true);
    let before = segments(dir.path());
    let plan = retention::plan(dir.path(), &Policy::default().max_segments(3)).unwrap();
    assert_eq!(plan.segments().len(), 3);
    fault::fail_next(Point::RetentionMarkerSync, 5); // EIO
    match retention::apply(dir.path(), &plan, &Action::Delete) {
        Err(Error::Io { op, .. }) => assert_eq!(op, "sync checkpoint marker"),
        other => panic!("expected the marker sync error, got {other:?}"),
    }
    assert_eq!(segments(dir.path()), before);
    let report = retention::apply(dir.path(), &plan, &Action::Delete).unwrap();
    assert_eq!(report.removed_segments, 3);
    fault::clear();
    drop(writer);
}

// What is read of a segment before it is removed (C14, ADR 0044). `journal` writes one record of 40,000 bytes per
// segment: the record at offset 64, the seal right after it, at 40,104, then zeros.

/// Offset of the seal in the segments `journal` writes.
const SEAL_AT: u64 = 64 + 40 + 40_000;

fn write_at(path: &std::path::Path, offset: u64, bytes: &[u8]) {
    use std::os::unix::fs::FileExt;
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .write_all_at(bytes, offset)
        .unwrap();
}

/// The eight bytes of a seal naming `next_seq`.
fn seal(next_seq: u64) -> Vec<u8> {
    let mut bytes = tickertape::format::SEAL_MARKER.to_le_bytes().to_vec();
    bytes.extend_from_slice(&tickertape::format::seal_crc(next_seq).to_le_bytes());
    bytes
}

fn bases(dir: &std::path::Path) -> Vec<u64> {
    segments(dir).iter().map(|s| s.0).collect()
}

/// A damaged record inside a sealed segment: a deletion checks the seal, not the records it discards, and removes the
/// segment; a move reads the whole segment first and refuses to move it. The plan, which reads no records, lists it.
#[test]
fn damaged_record_in_a_sealed_segment_is_deleted_but_never_moved() {
    let (dir, writer) = journal(true);
    let policy = Policy::default().max_segments(2);
    let first = segments(dir.path())[0].1.clone();
    write_at(&first, 64 + 40 + 1_000, &[0]);
    let plan = retention::plan(dir.path(), &policy).unwrap();
    assert_eq!(plan.segments().len(), 4);
    let dest = tempfile::tempdir().unwrap();
    match retention::apply(dir.path(), &plan, &Action::MoveTo(dest.path().into())) {
        Err(Error::Corrupt {
            path,
            offset: 64,
            reason: tickertape::CorruptReason::BadChecksum,
            ..
        }) => assert_eq!(path.file_name(), first.file_name()),
        other => panic!("expected the damaged record, got {other:?}"),
    }
    assert_eq!(bases(dir.path()), [0, 1, 2, 3, 4, 5]);
    assert!(segments(dest.path()).is_empty());
    let report = retention::apply(dir.path(), &plan, &Action::Delete).unwrap();
    assert_eq!(report.removed_segments, 4);
    assert_eq!(bases(dir.path()), [4, 5]);
    assert!(
        tickertape::inspect::verify(dir.path())
            .unwrap()
            .problems
            .is_empty()
    );
    drop(writer);
    assert_eq!(
        Writer::open(dir.path(), small_config()).unwrap().next_seq(),
        6
    );
}

/// A segment whose seal is damaged, gone, or names another segment is removed by neither action, whether the damage
/// is there when the plan is made (the plan fails) or appears between the plan and the removal (the removal fails and
/// removes nothing). The error is what a scan of the whole segment finds.
#[test]
fn broken_seal_is_never_removed() {
    use tickertape::CorruptReason;
    let mut wrong_checksum = seal(1);
    wrong_checksum[4] ^= 1;
    let damages = [
        ("damaged seal", wrong_checksum, CorruptReason::BadSeal),
        ("no seal", vec![0; 8], CorruptReason::BrokenChain(1)),
        ("seal naming segment 2", seal(2), CorruptReason::BadSeal),
    ];
    let policy = Policy::default().max_segments(2);
    for (what, bytes, reason) in &damages {
        for move_it in [false, true] {
            for before_plan in [true, false] {
                let case = format!("{what}, move {move_it}, before the plan {before_plan}");
                let (dir, writer) = journal(true);
                let dest = tempfile::tempdir().unwrap();
                let action = if move_it {
                    Action::MoveTo(dest.path().into())
                } else {
                    Action::Delete
                };
                let first = segments(dir.path())[0].1.clone();
                let result = if before_plan {
                    write_at(&first, SEAL_AT, bytes);
                    retention::plan(dir.path(), &policy).map(|_| ())
                } else {
                    let plan = retention::plan(dir.path(), &policy).unwrap();
                    assert_eq!(plan.segments().len(), 4, "{case}");
                    write_at(&first, SEAL_AT, bytes);
                    retention::apply(dir.path(), &plan, &action).map(|_| ())
                };
                match result {
                    Err(Error::Corrupt {
                        offset,
                        reason: found,
                        ..
                    }) => {
                        assert_eq!(found, *reason, "{case}");
                        assert_eq!(offset, SEAL_AT, "{case}");
                    }
                    other => panic!("{case}: expected corruption, got {other:?}"),
                }
                assert_eq!(bases(dir.path()), [0, 1, 2, 3, 4, 5], "{case}");
                assert!(segments(dest.path()).is_empty(), "{case}");
                drop(writer);
            }
        }
    }
}

/// What a deletion takes for a seal: the last non-zero eight bytes of the segment, a valid seal naming the successor.
/// So a segment whose own seal is gone, and whose last record ends with exactly the bytes of such a seal, is deleted as
/// if it were sealed (it lies before the checkpoint segment and its successor is in place, so the journal stays a
/// chain); a move, which reads every record, finds the chain broken there and refuses it.
#[test]
fn deletion_takes_the_last_non_zero_bytes_for_the_seal() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    let mut payload = vec![42; 40_000 - 8];
    payload.extend_from_slice(&seal(1));
    writer
        .append(&NewRecord {
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: &payload,
        })
        .unwrap();
    for _ in 1..6 {
        append(&mut writer);
    }
    writer.checkpoint().unwrap();
    write_at(&segments(dir.path())[0].1, SEAL_AT, &[0; 8]);
    let policy = Policy::default().max_segments(2);
    let plan = retention::plan(dir.path(), &policy).unwrap();
    assert_eq!(plan.segments().len(), 4);
    let dest = tempfile::tempdir().unwrap();
    assert!(matches!(
        retention::apply(dir.path(), &plan, &Action::MoveTo(dest.path().into())),
        Err(Error::Corrupt {
            offset: SEAL_AT,
            reason: tickertape::CorruptReason::BrokenChain(1),
            ..
        })
    ));
    assert_eq!(bases(dir.path()), [0, 1, 2, 3, 4, 5]);
    let report = retention::apply(dir.path(), &plan, &Action::Delete).unwrap();
    assert_eq!(report.removed_segments, 4);
    assert_eq!(bases(dir.path()), [4, 5]);
    drop(writer);
    assert_eq!(
        Writer::open(dir.path(), small_config()).unwrap().next_seq(),
        6
    );
}
