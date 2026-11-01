//! Crash contract C14: retention removes a contiguous, checkpoint-covered prefix only.

use std::fs;
use std::time::Duration;

use tickertape::retention::{self, Action, Policy, Reason};
use tickertape::{Error, NewRecord, Reader, ReaderConfig, Writer};
use tickertape_tests::{read_all, reopen_writer, segments, small_config};

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
    assert_eq!(reopen_writer(dir.path(), small_config()).next_seq(), 6);
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
    assert_eq!(reopen_writer(dir.path(), config).next_seq(), 40);

    let (dir, mut writer) = journal(true);
    append(&mut writer);
    writer.checkpoint().unwrap();
    drop(writer);
    let dest = dir.path().join("missing");
    let config = small_config().retention(
        Policy::default().max_segments(1),
        Action::MoveTo(dest.clone()),
    );
    let mut writer = reopen_writer(dir.path(), config);
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
        let writer = reopen_writer(dir.path(), small_config());
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
    let mut removed_total = 0;
    for iteration in 0..50 {
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
        if cross_device.is_none() && iteration % 2 == 1 {
            command.arg("force-copy");
        }
        let mut child = command.spawn().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut ready = String::new();
        stdout.read_line(&mut ready).unwrap();
        assert_eq!(ready.trim(), "ready");
        std::thread::sleep(Duration::from_micros(rng.below(5000)));
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
        assert_eq!(reopen_writer(dir.path(), small_config()).next_seq(), 12);
        assert!(
            tickertape::inspect::verify(dir.path())
                .unwrap()
                .problems
                .is_empty()
        );
    }
    assert!(removed_total > 0, "kills never overlapped a completed move");
    eprintln!("retention_kill_loop_50: 50 kills, {removed_total} preserved removals");
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
            assert_eq!(reopen_writer(dir.path(), small_config()).next_seq(), 6);
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
    let mut writer = reopen_writer(dir.path(), config);
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
