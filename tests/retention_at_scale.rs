//! What retention reads (ADR 0051): how many directory listings, segment files opened and file sizes asked for per
//! checkpoint and per removal, and that the guard against other archivers (crash contract C14) holds although a run
//! lists the directory once.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tickertape::fault::{self, Access, Point};
use tickertape::retention::{self, Action, Policy, Report};
use tickertape::{CorruptReason, Error, NewRecord, Writer, inspect};
use tickertape_tests::{read_all, segments, small_config};

/// Appends one record of 40,000 bytes: each one fills a segment.
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

/// A journal of `n` segments, one record each, with the marker in the newest.
fn journal(n: u64) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    for _ in 0..n {
        append(&mut writer);
    }
    writer.checkpoint().unwrap();
    dir
}

fn bases(dir: &Path) -> Vec<u64> {
    segments(dir).iter().map(|s| s.0).collect()
}

/// (listings, segments opened, files stated) while `f` runs.
fn accesses(f: impl FnOnce()) -> (usize, usize, usize) {
    fault::record_accesses();
    f();
    let all = fault::take_accesses();
    let count = |a| all.iter().filter(|b| **b == a).count();
    (
        count(Access::ListJournal),
        count(Access::OpenSegment),
        count(Access::StatSegment),
    )
}

const DAY: Duration = Duration::from_secs(86_400);

/// A checkpoint of a writer whose policy removes nothing costs the same at any number of segments: one listing, the
/// marker's segment, and by age the oldest segment and its successor. With a size limit, the sizes of the segments the
/// writer has not seen, once, and the headers of those from the marker's segment on, once.
#[test]
fn a_checkpoint_that_removes_nothing_reads_the_same_at_any_size() {
    for n in [5, 60] {
        for (policy, opened, sizes) in [
            (Policy::default().max_segments(1_000), 1, false),
            (Policy::default().max_age(DAY), 3, false),
            (Policy::default().max_bytes(1 << 40), 1, true),
            (
                Policy::default()
                    .max_segments(1_000)
                    .max_bytes(1 << 40)
                    .max_age(DAY),
                3,
                true,
            ),
        ] {
            let dir = journal(n);
            let config = small_config().retention(policy.clone(), Action::Delete);
            let mut writer = None;
            let open = accesses(|| {
                writer = Some(Writer::open(dir.path(), config).unwrap());
            });
            let mut writer = writer.unwrap();
            let sized = if sizes { n as usize } else { 0 };
            // With a size limit, every file's size, and the header of the newest (the marker's segment), once.
            let checked = usize::from(sizes);
            assert_eq!(
                (open.1, open.2),
                (opened + checked, sized),
                "{n}: {policy:?}"
            );
            for _ in 0..3 {
                // Without a roll, and with one.
                let idle = accesses(|| writer.checkpoint().unwrap());
                assert_eq!(idle, (1, opened, 0), "{n}: {policy:?}");
                append(&mut writer);
                let rolled = accesses(|| writer.checkpoint().unwrap());
                let new = usize::from(sizes);
                // The new segment's size and header.
                assert_eq!(rolled, (1, opened + new, new), "{n}: {policy:?}");
            }
            assert_eq!(writer.retention_report().removed_segments, 0);
            assert!(writer.last_retention_error().is_none());
        }
    }
}

/// A checkpoint that removes one segment: one listing in all, and the same files at any number of segments.
#[test]
fn a_checkpoint_that_removes_one_segment_lists_once() {
    for n in [5, 60] {
        for sized in [false, true] {
            let dir = journal(n);
            let mut policy = Policy::default().max_segments(n as usize);
            if sized {
                policy = policy.max_bytes(n * 65_536);
            }
            let config = small_config().retention(policy, Action::Delete);
            let mut writer = Writer::open(dir.path(), config).unwrap();
            for i in 1..4 {
                append(&mut writer);
                let removed = accesses(|| writer.checkpoint().unwrap());
                // Planning: the marker's segment, the oldest and its successor, the oldest's seal (and with a size
                // limit the new segment's size and header). Removing: the marker's segment, the oldest and its
                // successor.
                let new = usize::from(sized);
                assert_eq!(removed, (1, 7 + new, new), "{n}: sized {sized}");
                assert_eq!(writer.retention_report().removed_segments, i);
            }
        }
    }
}

/// An archiver's run lists the directory once, whatever it removes, opens three segments per removal, and asks for the
/// identity of each segment once it is out of the journal.
#[test]
fn an_archiver_lists_once_per_run() {
    for (n, keep) in [(10, 9), (10, 2), (60, 2)] {
        let dir = journal(n);
        let k = n as usize - keep;
        let mut plan = None;
        let planning = accesses(|| {
            plan = Some(retention::plan(dir.path(), &Policy::default().max_segments(keep)).unwrap())
        });
        assert_eq!(planning, (1, 1 + (k + 1) + k, 0));
        let plan = plan.unwrap();
        assert_eq!(plan.segments().len(), k);
        let applying = accesses(|| {
            let report = retention::apply(dir.path(), &plan, &Action::Delete).unwrap();
            assert_eq!(report.removed_segments, k as u64);
        });
        // The stats: that the segment removed before is still gone, and that the file taken out of the journal is the
        // one checked (issue #83).
        assert_eq!(applying, (1, 3 * k, (k - 1) + k));
        assert_eq!(bases(dir.path()).len(), keep);
    }
}

// The window between a run's listing and its removals: other archivers, at every point of it.

const POINTS: [Point; 5] = [
    Point::RetentionRemove,
    Point::MarkerRead,
    Point::RetentionChecked,
    // An archiver's own (issue #83): the segment out of the journal, before and after it is checked again. A writer's
    // run does not reach them.
    Point::RetentionTakenOut,
    Point::RetentionVerified,
];

/// Segments in the journals of the tests below, and how many a run removes.
const N: u64 = 8;
const REMOVED: u64 = 6;

/// Removes the oldest segments down to base `to`, as another archiver does, in a thread of its own (so that it does not
/// run into this thread's hooks).
fn archive_to(dir: &Path, to: u64, action: &Action) {
    let (dir, action) = (dir.to_path_buf(), action.clone());
    std::thread::spawn(move || {
        let keep = segments(&dir).iter().filter(|s| s.0 >= to).count();
        let plan = retention::plan(&dir, &Policy::default().max_segments(keep)).unwrap();
        retention::apply(&dir, &plan, &action).unwrap();
    })
    .join()
    .unwrap();
}

/// A run to take apart: an archiver's, or a writer's own after a checkpoint.
fn run(dir: &Path, by_writer: bool, action: &Action) -> tickertape::Result<Report> {
    let policy = Policy::default().max_segments((N - REMOVED) as usize);
    if !by_writer {
        let plan = retention::plan(dir, &policy).unwrap();
        assert_eq!(plan.segments().len(), REMOVED as usize);
        return retention::apply(dir, &plan, action);
    }
    // The writer's open runs it. Its plan reads the marker too, before it lists nothing more (`by_writer` runs are armed
    // with that in mind).
    let config = small_config().retention(policy, action.clone());
    let writer = Writer::open(dir, config).unwrap();
    match writer.last_retention_error() {
        None => Ok(writer.retention_report().clone()),
        Some(error) => Err(Error::InvalidConfig(error.to_string())),
    }
}

/// The marker reads that come before a run's removals: one for the plan of a writer's run (an archiver's plan is made
/// before anything is armed).
fn arm(point: Point, at: u64, by_writer: bool, action: impl FnOnce() + 'static) {
    let skip = at + u64::from(by_writer && point == Point::MarkerRead);
    fault::run_at(point, skip as usize, action);
}

fn archived(dir: &Path) -> Vec<u64> {
    if dir.exists() { bases(dir) } else { Vec::new() }
}

/// Another archiver removes segments at any one or two points of a run: each segment is removed by exactly one of them,
/// the run ends without an error, and the journal is a chain from the oldest that remains.
fn another_archiver_at_points_of_a_run(full: bool) {
    let mut cases = 0;
    for by_writer in [false, true] {
        for moved in [false, true] {
            for point in POINTS {
                for at in 0..REMOVED {
                    for to in 1..=REMOVED + 1 {
                        for second in [false, true] {
                            // A quick run leaves most combinations out.
                            if !full && (moved != second || (at + to) % 3 != 0) {
                                continue;
                            }
                            let case = format!("{by_writer} {moved} {point:?} {at} {to} {second}");
                            let dir = journal(N);
                            let before: Vec<_> = segments(dir.path())
                                .into_iter()
                                .map(|(_, p)| fs::read(p).unwrap())
                                .collect();
                            let (mine, other) =
                                (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
                            let action = if moved {
                                Action::MoveTo(mine.path().into())
                            } else {
                                Action::Delete
                            };
                            let theirs = Action::MoveTo(other.path().into());
                            fault::clear();
                            let policy = Policy::default().max_segments((N - REMOVED) as usize);
                            let plan = retention::plan(dir.path(), &policy).unwrap();
                            let (path, a) = (dir.path().to_path_buf(), theirs.clone());
                            arm(point, at, by_writer, move || archive_to(&path, to, &a));
                            if second {
                                // And once more, two removals later, the rest but the last one.
                                let path = dir.path().to_path_buf();
                                arm(Point::RetentionRemove, at + 2, by_writer, move || {
                                    archive_to(&path, REMOVED - 1, &theirs)
                                });
                            }
                            let report = if by_writer {
                                run(dir.path(), true, &action)
                            } else {
                                retention::apply(dir.path(), &plan, &action)
                            };
                            fault::clear();
                            let report = report.unwrap_or_else(|e| panic!("{case}: {e}"));
                            let by_other = bases(other.path());
                            let left = bases(dir.path());
                            let first = REMOVED.max(by_other.last().map_or(0, |b| b + 1));
                            assert_eq!(left, (first..N).collect::<Vec<_>>(), "{case}");
                            assert_eq!(
                                report.removed_segments + by_other.len() as u64,
                                first,
                                "{case}: {report:?}"
                            );
                            assert_eq!(
                                report.removed_segments + report.already_removed_segments,
                                REMOVED,
                                "{case}: {report:?}"
                            );
                            let mut all = archived(mine.path());
                            if !moved {
                                all = (0..first).filter(|b| !by_other.contains(b)).collect();
                            }
                            assert_eq!(all.len() as u64, report.removed_segments, "{case}");
                            for base in by_other {
                                assert!(!all.contains(&base), "{case}");
                                let copy = fs::read(
                                    other
                                        .path()
                                        .join(tickertape::format::segment_file_name(base)),
                                );
                                assert!(copy.unwrap() == before[base as usize], "{case}");
                            }
                            assert!(
                                inspect::verify(dir.path()).unwrap().problems.is_empty(),
                                "{case}"
                            );
                            assert_eq!(
                                read_all(dir.path())
                                    .iter()
                                    .map(|r| r.seq)
                                    .collect::<Vec<_>>(),
                                left,
                                "{case}"
                            );
                            cases += 1;
                        }
                    }
                }
            }
        }
    }
    eprintln!("{cases} interleavings");
}

#[test]
fn another_archiver_at_some_points_of_a_run() {
    another_archiver_at_points_of_a_run(false);
}

#[test]
#[ignore = "long: every combination; run with --release -- --ignored"]
fn another_archiver_at_every_point_of_a_run() {
    another_archiver_at_points_of_a_run(true);
}

/// A segment in the middle disappears (not something an archiver does) at any point of a run: the run stops with a
/// broken chain at the segment before it and removes nothing from there on.
#[test]
fn a_missing_middle_at_every_point_of_a_run_stops_it() {
    for by_writer in [false, true] {
        for point in POINTS {
            let taken_out = [Point::RetentionTakenOut, Point::RetentionVerified].contains(&point);
            if by_writer && taken_out {
                continue;
            }
            for at in 0..REMOVED {
                for gone in at + 1..N - 1 {
                    let case = format!("{by_writer} {point:?} {at} {gone}");
                    let dir = journal(N);
                    let paths: Vec<PathBuf> =
                        segments(dir.path()).into_iter().map(|(_, p)| p).collect();
                    let policy = Policy::default().max_segments((N - REMOVED) as usize);
                    let plan = retention::plan(dir.path(), &policy).unwrap();
                    let victim = paths[gone as usize].clone();
                    fault::clear();
                    arm(point, at, by_writer, move || {
                        fs::remove_file(victim).unwrap()
                    });
                    let result = if by_writer {
                        run(dir.path(), true, &Action::Delete)
                    } else {
                        retention::apply(dir.path(), &plan, &Action::Delete)
                    };
                    fault::clear();
                    if (point == Point::RetentionChecked || taken_out) && gone == at + 1 {
                        // The successor had been checked, and goes before its predecessor does (or while its
                        // predecessor is out of the journal): by the time the run gets to it, it is the oldest end
                        // that is gone, as after another archiver.
                        let report = result.unwrap();
                        let planned = u64::from(gone < REMOVED);
                        assert_eq!(report.already_removed_segments, planned, "{case}");
                        assert_eq!(report.removed_segments, REMOVED - planned, "{case}");
                        let first = REMOVED.max(gone + 1);
                        assert_eq!(bases(dir.path()), (first..N).collect::<Vec<_>>(), "{case}");
                        continue;
                    }
                    let stops = gone - 1;
                    if stops >= REMOVED {
                        // Nothing of the plan needs the missing segment.
                        result.unwrap();
                    } else {
                        let error = result.unwrap_err().to_string();
                        assert!(
                            error.contains(&format!("{stops:020}.tape")),
                            "{case}: {error}"
                        );
                    }
                    let first = stops.min(REMOVED);
                    let expected: Vec<u64> = (first..N).filter(|b| *b != gone).collect();
                    assert_eq!(bases(dir.path()), expected, "{case}");
                }
            }
        }
    }
}

/// The error for a missing successor is a broken chain, wherever in a run it is found.
#[test]
fn a_missing_successor_found_by_a_later_removal_is_a_broken_chain() {
    for at in 1..REMOVED {
        let dir = journal(N);
        let policy = Policy::default().max_segments((N - REMOVED) as usize);
        let plan = retention::plan(dir.path(), &policy).unwrap();
        let successor = segments(dir.path())[at as usize + 1].1.clone();
        fault::clear();
        fault::run_at(Point::RetentionRemove, at as usize, move || {
            fs::remove_file(successor).unwrap()
        });
        match retention::apply(dir.path(), &plan, &Action::Delete) {
            Err(Error::Corrupt {
                path,
                offset: 0,
                reason: CorruptReason::BrokenChain(next),
                ..
            }) => {
                assert_eq!(next, at + 1);
                assert_eq!(path, plan.segments()[at as usize].path);
            }
            other => panic!("{at}: {other:?}"),
        }
        assert!(plan.segments()[at as usize].path.exists());
    }
}

/// A segment the run removed is put back while it goes on: the next one is no longer the oldest, and stays.
#[test]
fn a_segment_put_back_during_a_run_stops_it() {
    for at in 1..REMOVED {
        for after in [0, 1] {
            let dir = journal(N);
            let policy = Policy::default().max_segments((N - REMOVED) as usize);
            let plan = retention::plan(dir.path(), &policy).unwrap();
            let last = plan.segments()[at as usize - 1].path.clone();
            let bytes = fs::read(&last).unwrap();
            fault::clear();
            let put = last.clone();
            fault::run_at(Point::RetentionRemove, at as usize, move || {
                fs::write(&put, bytes).unwrap();
                if after == 1 {
                    // Found by its name, whatever it is.
                    fs::remove_file(&put).unwrap();
                    std::os::unix::fs::symlink("nowhere", &put).unwrap();
                }
            });
            match retention::apply(dir.path(), &plan, &Action::Delete) {
                Err(Error::Corrupt {
                    path,
                    reason: CorruptReason::BrokenChain(base),
                    ..
                }) => {
                    assert_eq!(base, at);
                    assert_eq!(path, plan.segments()[at as usize].path);
                }
                other => panic!("{at}: {other:?}"),
            }
            let mut left = vec![at - 1];
            left.extend(at..N);
            assert_eq!(bases(dir.path()), left);
        }
    }
}

/// A run whose plan has been overtaken lists once for it, not once per segment that is gone.
#[test]
fn an_overtaken_run_lists_once_more() {
    let dir = journal(N);
    let policy = Policy::default().max_segments((N - REMOVED) as usize);
    let plan = retention::plan(dir.path(), &policy).unwrap();
    fault::clear();
    let path = dir.path().to_path_buf();
    fault::run_at(Point::RetentionRemove, 1, move || {
        archive_to(&path, REMOVED, &Action::Delete)
    });
    let found = accesses(|| {
        let report = retention::apply(dir.path(), &plan, &Action::Delete).unwrap();
        assert_eq!(report.removed_segments, 1);
        assert_eq!(report.already_removed_segments, REMOVED - 1);
    });
    assert_eq!(found.0, 2, "{found:?}");
}

/// A failed sync of the marker ends the run at once, in the middle of it too: no listing, no second sync that could
/// succeed without having written anything.
#[test]
fn a_marker_that_cannot_be_synced_ends_a_run_at_once() {
    let dir = journal(N);
    let policy = Policy::default().max_segments((N - REMOVED) as usize);
    let plan = retention::plan(dir.path(), &policy).unwrap();
    fault::clear();
    // Another archiver removes the segment, and the marker moves, before the third removal's sync of it.
    let path = dir.path().to_path_buf();
    fault::run_at(Point::RetentionRemove, 2, move || {
        let copy = path.clone();
        std::thread::spawn(move || {
            let mut writer = Writer::open(&copy, small_config()).unwrap();
            append(&mut writer);
            writer.checkpoint().unwrap();
        })
        .join()
        .unwrap();
        archive_to(&path, 4, &Action::Delete);
        fault::fail_next(Point::RetentionMarkerSync, 5); // EIO
    });
    let found = accesses(
        || match retention::apply(dir.path(), &plan, &Action::Delete) {
            Err(Error::Io { op, .. }) => assert_eq!(op, "sync checkpoint marker"),
            other => panic!("{other:?}"),
        },
    );
    fault::clear();
    assert_eq!(found.0, 1, "{found:?}");
    assert_eq!(bases(dir.path()), [4, 5, 6, 7, 8]);
}

/// The marker goes away in the middle of a run: nothing more is removed.
#[test]
fn a_marker_lost_during_a_run_stops_it() {
    for by_writer in [false, true] {
        for at in 0..REMOVED {
            let dir = journal(N);
            let policy = Policy::default().max_segments((N - REMOVED) as usize);
            let plan = retention::plan(dir.path(), &policy).unwrap();
            let path = dir.path().join("checkpoint");
            fault::clear();
            fault::run_at(Point::RetentionRemove, at as usize, move || {
                fs::write(path, [0; 64]).unwrap()
            });
            let result = if by_writer {
                run(dir.path(), true, &Action::Delete)
            } else {
                retention::apply(dir.path(), &plan, &Action::Delete)
            };
            fault::clear();
            assert!(result.is_err());
            assert_eq!(bases(dir.path()), (at..N).collect::<Vec<_>>());
        }
    }
}

/// Two archivers move the oldest segments of a journal whose writer rolls, checkpoints and runs retention of its own:
/// every segment ends up in exactly one place, and the journal is a chain.
#[test]
fn archivers_and_a_writer_at_once() {
    let rounds = if cfg!(debug_assertions) { 150 } else { 600 };
    let dir = tempfile::tempdir().unwrap();
    let homes: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let config = small_config().retention(
        Policy::default().max_segments(6),
        Action::MoveTo(homes[2].path().into()),
    );
    let mut writer = Writer::open(dir.path(), config).unwrap();
    append(&mut writer);
    writer.checkpoint().unwrap();
    let done = std::sync::atomic::AtomicBool::new(false);
    let mut removed = [0u64; 2];
    std::thread::scope(|scope| {
        for (home, removed) in homes.iter().zip(&mut removed) {
            let (dir, done) = (dir.path(), &done);
            scope.spawn(move || {
                let action = Action::MoveTo(home.path().into());
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    // A plan can meet a segment that goes away under it, and either can meet a marker whose segment
                    // goes away (a newer marker is there by then).
                    let Ok(plan) = retention::plan(dir, &Policy::default().max_segments(3)) else {
                        continue;
                    };
                    match retention::apply(dir, &plan, &action) {
                        Ok(report) => *removed += report.removed_segments,
                        Err(Error::Corrupt {
                            reason: CorruptReason::DanglingCheckpoint(_),
                            ..
                        }) => {}
                        Err(e) => panic!("{e}"),
                    }
                }
            });
        }
        for i in 0..rounds {
            append(&mut writer);
            if i % 3 == 0 {
                writer.checkpoint().unwrap();
            }
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    assert!(
        removed.iter().sum::<u64>() > 0,
        "the archivers removed nothing"
    );
    let left = bases(dir.path());
    let mut gone: Vec<u64> = homes.iter().flat_map(|h| bases(h.path())).collect();
    gone.sort_unstable();
    assert_eq!(gone, (0..left[0]).collect::<Vec<_>>());
    assert!(inspect::verify(dir.path()).unwrap().problems.is_empty());
    drop(writer);
    assert_eq!(
        Writer::open(dir.path(), small_config()).unwrap().next_seq(),
        rounds + 1
    );
}

/// The successor's header is damaged after the plan, before any removal or before a later one: the removal that needs
/// it fails with the header's error, and the segment before it stays.
#[test]
fn a_successor_damaged_after_the_plan_stops_the_removal() {
    use std::os::unix::fs::FileExt;
    for at in 0..REMOVED {
        let dir = journal(N);
        let policy = Policy::default().max_segments((N - REMOVED) as usize);
        let plan = retention::plan(dir.path(), &policy).unwrap();
        let successor = segments(dir.path())[at as usize + 1].1.clone();
        fault::clear();
        let damaged = successor.clone();
        fault::run_at(Point::RetentionRemove, at as usize, move || {
            let file = fs::OpenOptions::new().write(true).open(&damaged).unwrap();
            file.write_all_at(b"X", 0).unwrap();
        });
        let result = retention::apply(dir.path(), &plan, &Action::Delete);
        fault::clear();
        match result {
            Err(Error::Corrupt {
                path,
                offset: 0,
                reason: CorruptReason::BadMagic,
                ..
            }) => assert_eq!(path, successor, "{at}"),
            other => panic!("{at}: {other:?}"),
        }
        assert_eq!(bases(dir.path()), (at..N).collect::<Vec<_>>(), "{at}");
    }
}

/// A move whose destination goes away after the checks: the rename fails with "not found" although the segment is in
/// place, so it is not taken for a segment another archiver removed. The run stops there, and removes nothing after it.
#[test]
fn a_move_into_a_destination_that_went_away_is_an_error() {
    for at in 0..REMOVED {
        let dir = journal(N);
        let parent = tempfile::tempdir().unwrap();
        let dest = parent.path().join("archive");
        fs::create_dir(&dest).unwrap();
        let policy = Policy::default().max_segments((N - REMOVED) as usize);
        let plan = retention::plan(dir.path(), &policy).unwrap();
        fault::clear();
        let gone = dest.clone();
        fault::run_at(Point::RetentionChecked, at as usize, move || {
            fs::remove_dir_all(gone).unwrap()
        });
        let result = retention::apply(dir.path(), &plan, &Action::MoveTo(dest.clone()));
        fault::clear();
        match result {
            Err(Error::Io { source, .. }) => {
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound, "{at}")
            }
            other => panic!("{at}: {other:?}"),
        }
        assert_eq!(bases(dir.path()), (at..N).collect::<Vec<_>>(), "{at}");
    }
}
