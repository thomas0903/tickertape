//! Issue #83 (crash contract C14): a recovery that replaces or restarts the oldest segment while an archiver is between
//! its checks of that segment and its removal. The marker is lost and the segment damaged, a writer opens (recovery
//! without a marker starts at the oldest segment and may end the chain there), appends and checkpoints: its marker names
//! the segment the archiver is about to remove. The archiver takes the segment out of the journal by a rename and checks
//! what it took before it lets it go; the temporary name it uses, `{base:020}.tape.{pid}.{unix_ns}.removing`, is
//! ignored by everything else and unlinked by a later retention run once it is old and its process gone.

use std::cell::RefCell;
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use tickertape::fault::{self, Point};
use tickertape::format::{self, segment_file_name};
use tickertape::retention::{self, Action, Policy, Report};
use tickertape::{Error, NewRecord, Reader, ReaderConfig, Writer, inspect};
use tickertape_tests::{segments, small_config};

/// One record of 40,000 bytes fills a segment of the small size.
fn append(writer: &mut Writer, byte: u8) {
    writer
        .append(&NewRecord {
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: &[byte; 40_000],
        })
        .unwrap();
}

const N: u64 = 6;
/// A run removes this many of the `N` segments.
const REMOVED: u64 = 3;

/// A journal of `N` segments, one record each, with the marker in the newest.
fn journal() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    for _ in 0..N {
        append(&mut writer, 42);
    }
    writer.checkpoint().unwrap();
    dir
}

fn plan(dir: &Path) -> retention::Plan {
    let plan =
        retention::plan(dir, &Policy::default().max_segments((N - REMOVED) as usize)).unwrap();
    assert_eq!(plan.segments().len(), REMOVED as usize);
    plan
}

/// Where the record of a one-record segment starts, and where its seal is.
const RECORD: u64 = format::SEGMENT_HEADER_LEN as u64;
const SEAL: u64 = RECORD + format::RECORD_HEADER_LEN as u64 + 40_000;

/// Storage damage to the oldest segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Damage {
    /// A payload byte flipped: recovery replaces the segment by a copy that ends before the record (a new file).
    Payload,
    /// The record's commit word zeroed, the rest intact: committed history after it, so the same.
    CommitWord,
    /// The seal's CRC damaged: the record is kept, and the segment is replaced by a copy that ends at the seal.
    SealCrc,
    /// The record and the seal zeroed: recovery ends the chain at the record in place and keeps the file (same inode),
    /// which the writer then writes to.
    ZeroedTail,
    /// The seal zeroed: recovery seals the segment again in place and goes on, so nothing is cut off (a control).
    ZeroedSeal,
}

const DAMAGE: [Damage; 5] = [
    Damage::Payload,
    Damage::CommitWord,
    Damage::SealCrc,
    Damage::ZeroedTail,
    Damage::ZeroedSeal,
];

fn write_at(path: &Path, offset: u64, bytes: &[u8]) {
    use std::os::unix::fs::FileExt;
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .write_all_at(bytes, offset)
        .unwrap();
}

fn damage(path: &Path, damage: Damage) {
    match damage {
        Damage::Payload => write_at(path, RECORD + 100, &[0x55]),
        Damage::CommitWord => write_at(path, RECORD, &[0; 4]),
        Damage::SealCrc => write_at(path, SEAL + 7, &[0x55]),
        Damage::ZeroedTail => write_at(path, RECORD, &vec![0; (SEAL + 8 - RECORD) as usize]),
        Damage::ZeroedSeal => write_at(path, SEAL, &[0; 8]),
    }
}

/// The marker is lost, the oldest segment damaged, and a writer opens (recovery without a marker starts at the oldest
/// segment), appends a record and checkpoints. The writer is returned, still open.
fn lose_damage_recover(dir: &Path, kind: Damage) -> Option<Writer> {
    fs::write(dir.join("checkpoint"), [0; 64]).unwrap();
    if let Some((_, oldest)) = segments(dir).first() {
        damage(oldest, kind);
    }
    let mut writer = Writer::open(dir, small_config()).ok()?;
    append(&mut writer, 7);
    writer.checkpoint().unwrap();
    Some(writer)
}

/// (base, bytes) of each segment.
fn contents(dir: &Path) -> Vec<(u64, Vec<u8>)> {
    segments(dir)
        .into_iter()
        .map(|(base, path)| (base, fs::read(path).unwrap()))
        .collect()
}

fn bases(dir: &Path) -> Vec<u64> {
    segments(dir).iter().map(|s| s.0).collect()
}

/// The temporary names of segments being removed, in `dir`.
fn leftovers(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_str().unwrap().ends_with(".removing"))
        .collect();
    out.sort();
    out
}

/// The marker's segment is there, if there is a valid marker.
fn assert_marker_segment_exists(dir: &Path, case: &str) {
    if let Some(slot) = inspect::read_checkpoint(dir).unwrap() {
        assert!(
            dir.join(segment_file_name(slot.segment_base_seq)).exists(),
            "{case}: the marker names segment {} and it is gone",
            slot.segment_base_seq
        );
    }
}

/// How a run removes: deletes, moves by a rename, or moves by a copy (a rename that fails with `EXDEV`, as across file
/// systems).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum How {
    Delete,
    Rename,
    Copy,
}

const HOW: [How; 3] = [How::Delete, How::Rename, How::Copy];

fn action(how: How, archive: &Path) -> Action {
    match how {
        How::Delete => Action::Delete,
        How::Rename | How::Copy => Action::MoveTo(archive.into()),
    }
}

/// Applies `plan`; with `How::Copy`, every rename into the destination fails with `EXDEV`.
fn apply(
    dir: &Path,
    plan: &retention::Plan,
    how: How,
    archive: &Path,
) -> tickertape::Result<Report> {
    if how == How::Copy {
        for _ in 0..REMOVED {
            fault::fail_next(Point::RetentionRename, libc::EXDEV);
        }
    }
    retention::apply(dir, plan, &action(how, archive))
}

/// The reproduction: an archiver is held between its checks of the oldest segment and its removal while the marker is
/// lost, the segment damaged, and a writer recovers, appends and checkpoints. The marker then names the segment the
/// archiver checked: replaced (`Payload`), or the same file (`ZeroedTail`). Afterwards the marker's segment exists, the
/// archiver has reported an error and left nothing behind, a writer opens, and the journal verifies.
#[test]
fn a_recovery_between_the_checks_and_the_removal_leaves_the_marker_its_segment() {
    for how in HOW {
        for kind in [Damage::Payload, Damage::ZeroedTail] {
            let case = format!("{how:?} {kind:?}");
            let dir = journal();
            let archive = tempfile::tempdir().unwrap();
            let plan = plan(dir.path());
            fault::clear();
            let (path, writer) = (dir.path().to_path_buf(), Rc::new(RefCell::new(None)));
            let held = Rc::clone(&writer);
            fault::run_at(Point::RetentionChecked, 0, move || {
                *held.borrow_mut() = lose_damage_recover(&path, kind);
            });
            let result = apply(dir.path(), &plan, how, archive.path());
            fault::clear();
            assert!(writer.borrow().is_some(), "{case}");
            assert!(result.is_err(), "{case}");
            assert_eq!(bases(dir.path()), [0], "{case}");
            assert_marker_segment_exists(dir.path(), &case);
            assert!(leftovers(dir.path()).is_empty(), "{case}");
            assert!(leftovers(archive.path()).is_empty(), "{case}");
            // Segment 0 stayed where the writer had it: the writer goes on in it and closes cleanly.
            let mut writer = writer.borrow_mut().take().unwrap();
            append(&mut writer, 8);
            writer.checkpoint().unwrap();
            drop(writer);
            Writer::open(dir.path(), small_config()).unwrap_or_else(|e| panic!("{case}: {e}"));
            assert!(
                inspect::verify(dir.path()).unwrap().problems.is_empty(),
                "{case}"
            );
        }
    }
}

/// The sweep of the review of pull request #81, as a test: at each point of an archiver's run where something can
/// happen between two of its steps, the marker is lost, the oldest segment is damaged (each kind), and a writer
/// recovers, appends and checkpoints (closed again, or still open while the run goes on). Afterwards: the marker's
/// segment exists; what remains of the journal is a suffix of what the recovery left; every moved segment is
/// byte-identical to that segment as it was before the run or as the recovery left it; a segment both moved and still
/// in the journal only after a run that failed; no temporary name is left; a writer opens and the journal verifies.
fn recoveries_at_points_of_a_run(full: bool) {
    const POINTS: [Point; 5] = [
        Point::RetentionRemove,
        Point::MarkerRead,
        Point::RetentionChecked,
        Point::RetentionTakenOut,
        Point::RetentionVerified,
    ];
    let (mut cases, mut emptied) = (0, 0);
    for how in HOW {
        for point in POINTS {
            for at in 0..REMOVED {
                for kind in DAMAGE {
                    for open in [false, true] {
                        if !full && (at + u64::from(open)) % 2 != (kind as u64) % 2 {
                            continue;
                        }
                        let case = format!("{how:?} {point:?} {at} {kind:?} open {open}");
                        let dir = journal();
                        let archive = tempfile::tempdir().unwrap();
                        let before = contents(dir.path());
                        let plan = plan(dir.path());
                        fault::clear();
                        let path = dir.path().to_path_buf();
                        let left_by_recovery = Rc::new(RefCell::new(None));
                        let writer = Rc::new(RefCell::new(None));
                        let (left, held) = (Rc::clone(&left_by_recovery), Rc::clone(&writer));
                        fault::run_at(point, at as usize, move || {
                            let recovered = lose_damage_recover(&path, kind);
                            *left.borrow_mut() = Some(contents(&path));
                            if open {
                                *held.borrow_mut() = recovered;
                            }
                        });
                        let result = apply(dir.path(), &plan, how, archive.path());
                        fault::clear();
                        let left_by_recovery = left_by_recovery.borrow_mut().take();
                        let left_by_recovery =
                            left_by_recovery.unwrap_or_else(|| panic!("{case}: never reached"));
                        drop(writer.borrow_mut().take());
                        assert_marker_segment_exists(dir.path(), &case);
                        // A suffix of what the recovery left.
                        let remaining = contents(dir.path());
                        let start = left_by_recovery.len() - remaining.len();
                        assert!(remaining == left_by_recovery[start..], "{case}");
                        // Moved segments. (A copy whose segment stayed is what a failed move leaves, as before:
                        // the copy of a file damaged after its records were checked, for one.)
                        for (base, bytes) in contents(archive.path()) {
                            if remaining.iter().any(|s| s.0 == base) {
                                assert!(result.is_err(), "{case}: {base} moved and kept");
                                continue;
                            }
                            let was = |list: &[(u64, Vec<u8>)]| {
                                list.iter().any(|s| s.0 == base && s.1 == bytes)
                            };
                            assert!(was(&before) || was(&left_by_recovery), "{case}: {base}");
                        }
                        assert!(leftovers(dir.path()).is_empty(), "{case}");
                        assert!(leftovers(archive.path()).is_empty(), "{case}");
                        if remaining.is_empty() {
                            emptied += 1;
                        } else {
                            Writer::open(dir.path(), small_config())
                                .unwrap_or_else(|e| panic!("{case}: {e}"));
                            assert!(
                                inspect::verify(dir.path()).unwrap().problems.is_empty(),
                                "{case}"
                            );
                        }
                        cases += 1;
                    }
                }
            }
        }
    }
    eprintln!("{cases} cases ({emptied} with no segment left)");
}

#[test]
fn recoveries_at_some_points_of_a_run() {
    recoveries_at_points_of_a_run(false);
}

#[test]
#[ignore = "long: every combination; run with --release -- --ignored"]
fn recoveries_at_every_point_of_a_run() {
    recoveries_at_points_of_a_run(true);
}

/// Runs `run` and makes the process "crash" at `point` (the `skip + 1`-th time it is reached): the run stops there and
/// does nothing more, as if killed.
fn crash_at(point: Point, skip: usize, run: impl FnOnce() -> tickertape::Result<Report>) {
    fault::run_at(point, skip, || panic!("crash"));
    let crashed = catch_unwind(AssertUnwindSafe(run));
    fault::clear();
    assert!(crashed.is_err(), "{point:?} not reached");
}

/// A process that has exited (its id is not in use).
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

const HOUR: u64 = 3_600_000_000_000;

/// Gives a temporary name the age and process of a crash long ago: an hour and a minute before now, by a process that
/// has exited.
fn age(leftover: &Path) -> PathBuf {
    let name = leftover.file_name().unwrap().to_str().unwrap();
    let old = leftover.with_file_name(format!(
        "{}.{}.{}.removing",
        &name[..25],
        dead_pid(),
        now_ns() - HOUR - 60_000_000_000
    ));
    fs::rename(leftover, &old).unwrap();
    old
}

/// A crash after a segment was taken out of the journal (and after it was checked): a deletion or a copy leaves its
/// temporary name in the journal directory, a rename leaves it in the destination. A writer's recovery, readers,
/// `verify`, `status` and a later archiver go on as if it had been removed; a later archiver leaves the name alone while
/// it is new or its process could still be running, and unlinks it once it is an hour old and its process gone; so does
/// a writer's own retention run. A move's segment is in the destination either way, byte-identical.
#[test]
fn a_crash_after_taking_a_segment_out_leaves_a_name_that_is_cleaned_up() {
    for how in HOW {
        for point in [Point::RetentionTakenOut, Point::RetentionVerified] {
            for by_writer in [false, true] {
                let case = format!("{how:?} {point:?} by writer {by_writer}");
                let dir = journal();
                let archive = tempfile::tempdir().unwrap();
                let before = contents(dir.path());
                let plan = plan(dir.path());
                // The second removal crashes.
                crash_at(point, 1, || apply(dir.path(), &plan, how, archive.path()));
                assert_eq!(bases(dir.path()), (2..N).collect::<Vec<_>>(), "{case}");
                let names = leftovers(dir.path());
                if how == How::Rename {
                    assert!(names.is_empty(), "{case}");
                } else {
                    assert_eq!(names.len(), 1, "{case}");
                    let name = names[0].file_name().unwrap().to_str().unwrap();
                    assert!(name.starts_with(&segment_file_name(1)), "{case}: {name}");
                    assert_eq!(fs::read(&names[0]).unwrap(), before[1].1, "{case}");
                }
                if how != How::Delete {
                    assert_eq!(contents(archive.path()), before[..2], "{case}");
                }
                // Recovery, readers, verify, status: as after a removal.
                drop(Writer::open(dir.path(), small_config()).unwrap());
                assert!(
                    inspect::verify(dir.path()).unwrap().problems.is_empty(),
                    "{case}"
                );
                tickertape::status::status(dir.path()).unwrap();
                let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
                assert_eq!(reader.next_owned().unwrap().unwrap().seq, 2, "{case}");
                assert_eq!(leftovers(dir.path()), names, "{case}");
                // A later run, while the name is new (and its process, this one, alive): left alone.
                let keep = (N - 4) as usize;
                let later = |dir: &Path| -> Report {
                    let policy = Policy::default().max_segments(keep);
                    if by_writer {
                        let config = small_config().retention(policy, action(how, archive.path()));
                        let writer = Writer::open(dir, config).unwrap();
                        assert!(writer.last_retention_error().is_none());
                        writer.retention_report().clone()
                    } else {
                        let plan = retention::plan(dir, &policy).unwrap();
                        retention::apply(dir, &plan, &Action::Delete).unwrap()
                    }
                };
                assert_eq!(later(dir.path()).removed_segments, 2, "{case}");
                assert_eq!(leftovers(dir.path()), names, "{case}");
                if let Some(name) = names.first() {
                    // An hour later, its process gone: unlinked by the next run, which removes nothing else.
                    age(name);
                    assert_eq!(later(dir.path()).removed_segments, 0, "{case}");
                    assert!(leftovers(dir.path()).is_empty(), "{case}");
                }
                assert_eq!(bases(dir.path()), (4..N).collect::<Vec<_>>(), "{case}");
            }
        }
    }
}

/// What a later run leaves alone: a name that is new, one whose process could be running, one of the marker's segment
/// or later (as a crash before putting a segment back leaves, see the next test), and names that are not temporary
/// names of a removal; what it removes: one of a segment before the marker's, an hour old, whose process is gone. None
/// of this stops its removals.
#[test]
fn a_later_run_removes_only_old_leftovers_of_gone_processes_before_the_marker() {
    let dir = journal();
    let now = now_ns();
    let (dead, alive) = (dead_pid(), std::process::id());
    let name = |base: u64, pid: u32, at: u64| {
        let path = dir
            .path()
            .join(format!("{}.{pid}.{at}.removing", segment_file_name(base)));
        fs::write(&path, b"leftover").unwrap();
        path
    };
    let gone = name(0, dead, now - HOUR - 60_000_000_000);
    let kept = [
        name(1, dead, now - 60_000_000_000),
        name(1, alive, now - 2 * HOUR),
        name(N - 1, dead, now - 2 * HOUR),
        name(N + 3, dead, now - 2 * HOUR),
    ];
    let others = [
        dir.path()
            .join(format!("{}.removing", segment_file_name(0))),
        dir.path()
            .join(format!("{}.x.1.removing", segment_file_name(0))),
        dir.path()
            .join(format!("{}.{dead}.1.removing.tmp", segment_file_name(0))),
    ];
    for other in &others {
        fs::write(other, b"other").unwrap();
    }
    let report = retention::apply(dir.path(), &plan(dir.path()), &Action::Delete).unwrap();
    assert_eq!(report.removed_segments, REMOVED);
    assert!(!gone.exists());
    for path in kept.iter().chain(&others) {
        assert!(path.exists(), "{path:?}");
    }
    assert!(inspect::verify(dir.path()).unwrap().problems.is_empty());
}

/// A crash between a failed check and putting the segment back (in the race of the first test): the marker names the
/// segment and it is out of the journal, under its temporary name or in the destination. A writer refuses to open (a
/// dangling marker), a later archiver stops at the marker and leaves the file alone, and renaming the file back, as
/// `docs/operations.md` says, restores the journal.
#[test]
fn a_crash_before_putting_a_segment_back_is_refused_and_repaired_by_renaming_it_back() {
    for how in HOW {
        let case = format!("{how:?}");
        let dir = journal();
        let archive = tempfile::tempdir().unwrap();
        let plan = plan(dir.path());
        let path = dir.path().to_path_buf();
        crash_at(Point::RetentionPutBack, 0, || {
            fault::run_at(Point::RetentionChecked, 0, move || {
                drop(lose_damage_recover(&path, Damage::Payload));
            });
            apply(dir.path(), &plan, how, archive.path())
        });
        assert!(bases(dir.path()).is_empty(), "{case}");
        let out = if how == How::Rename {
            archive.path().join(segment_file_name(0))
        } else {
            let names = leftovers(dir.path());
            assert_eq!(names.len(), 1, "{case}");
            age(&names[0])
        };
        assert!(
            matches!(
                Writer::open(dir.path(), small_config()),
                Err(Error::Corrupt { .. })
            ),
            "{case}"
        );
        let later = retention::plan(dir.path(), &Policy::default().max_segments(0));
        assert!(later.is_err(), "{case}");
        assert!(out.exists(), "{case}");
        fs::rename(&out, dir.path().join(segment_file_name(0))).unwrap();
        drop(Writer::open(dir.path(), small_config()).unwrap());
        assert!(
            inspect::verify(dir.path()).unwrap().problems.is_empty(),
            "{case}"
        );
    }
}

/// Putting back never replaces a file that has appeared under the segment's name meanwhile, with a no-replace rename
/// or, where the file system has none (`EINVAL`), with `link`: the file there stays as it is, the segment stays under
/// its temporary name (or in the destination), and the error says where it is.
#[test]
fn putting_back_never_replaces_a_file_and_falls_back_to_link() {
    for how in HOW {
        for unsupported in [false, true] {
            for collide in [false, true] {
                let case = format!("{how:?} unsupported {unsupported} collide {collide}");
                let dir = journal();
                let archive = tempfile::tempdir().unwrap();
                let plan = plan(dir.path());
                let path = dir.path().to_path_buf();
                let writer = Rc::new(RefCell::new(None));
                let held = Rc::clone(&writer);
                fault::clear();
                fault::run_at(Point::RetentionChecked, 0, move || {
                    *held.borrow_mut() = lose_damage_recover(&path, Damage::ZeroedTail);
                });
                let stranger = dir.path().join(segment_file_name(0));
                if collide {
                    let stranger = stranger.clone();
                    fault::run_at(Point::RetentionPutBack, 0, move || {
                        fs::write(stranger, b"not a segment").unwrap()
                    });
                }
                if unsupported {
                    fault::fail_next(Point::RetentionPutBack, libc::EINVAL);
                }
                let result = apply(dir.path(), &plan, how, archive.path());
                fault::clear();
                drop(writer.borrow_mut().take());
                let error = result.unwrap_err().to_string();
                let out = if how == How::Rename {
                    archive.path().join(segment_file_name(0))
                } else {
                    let names = leftovers(dir.path());
                    assert!(names.len() <= 1, "{case}: {names:?}");
                    names.first().cloned().unwrap_or_default()
                };
                if collide {
                    assert_eq!(fs::read(&stranger).unwrap(), b"not a segment", "{case}");
                    assert!(out.exists(), "{case}");
                    assert!(error.contains(out.to_str().unwrap()), "{case}: {error}");
                } else {
                    assert!(!out.exists(), "{case}: {out:?}");
                    assert_marker_segment_exists(dir.path(), &case);
                    drop(Writer::open(dir.path(), small_config()).unwrap());
                    assert!(
                        inspect::verify(dir.path()).unwrap().problems.is_empty(),
                        "{case}"
                    );
                }
            }
        }
    }
}

/// On a file system without a no-replace rename (`EINVAL`), taking a segment out uses a plain rename to the new name,
/// which is free: the removal goes ahead and leaves nothing behind.
#[test]
fn taking_out_falls_back_to_a_plain_rename() {
    for how in [How::Delete, How::Copy] {
        let dir = journal();
        let archive = tempfile::tempdir().unwrap();
        let plan = plan(dir.path());
        for _ in 0..REMOVED {
            fault::fail_next(Point::RetentionTakeOut, libc::EINVAL);
        }
        let report = apply(dir.path(), &plan, how, archive.path()).unwrap();
        fault::clear();
        assert_eq!(report.removed_segments, REMOVED);
        assert_eq!(bases(dir.path()), (REMOVED..N).collect::<Vec<_>>());
        assert!(leftovers(dir.path()).is_empty());
    }
}

/// While a segment is out of the journal, a reader that opens or seeks there gets what it gets once the segment is
/// gone: it starts at the next segment, and a seek into the segment is `SeqUnavailable`.
#[test]
fn a_reader_during_the_removal_sees_the_segment_gone() {
    for how in HOW {
        let dir = journal();
        let archive = tempfile::tempdir().unwrap();
        let plan = plan(dir.path());
        let path = dir.path().to_path_buf();
        let seen = Rc::new(RefCell::new(None));
        let saw = Rc::clone(&seen);
        fault::run_at(Point::RetentionTakenOut, 0, move || {
            let mut reader = Reader::open(&path, ReaderConfig::default()).unwrap();
            let first = reader.next_owned().unwrap().unwrap().seq;
            let seek = reader.seek(0).map_err(|e| e.to_string());
            let unavailable = matches!(reader.seek(0), Err(Error::SeqUnavailable { .. }));
            *saw.borrow_mut() = Some((first, seek, unavailable));
        });
        apply(dir.path(), &plan, how, archive.path()).unwrap();
        fault::clear();
        let (first, seek, unavailable) = seen.borrow_mut().take().unwrap();
        assert_eq!(first, 1, "{how:?}");
        assert!(unavailable, "{how:?}: {seek:?}");
    }
}
