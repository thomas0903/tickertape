//! Background checkpoints (ADR 0038): `Writer::request_checkpoint` covers the frontier at the request while appends
//! continue; rolls, retention, failures, drop and close with a checkpoint in flight; crash contract C7, C12, C13, C14.
//!
//! The holds and background failure points are process-wide, so the tests in this file run one at a time.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use tickertape::fault::{self, Point};
use tickertape::format::CheckpointSlot;
use tickertape::retention::{Action, Policy};
use tickertape::{CheckpointProgress, Error, NewRecord, Writer, inspect};
use tickertape_tests::{append_n, check_record, read_all, segments, small_config};

const EIO: i32 = 5;
const LONG: Duration = Duration::from_secs(30);

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    fault::clear();
    guard
}

/// Polls until no background checkpoint is pending; returns the progress then.
fn wait_finished(writer: &mut Writer) -> CheckpointProgress {
    let deadline = Instant::now() + LONG;
    loop {
        let progress = writer.checkpoint_progress().expect("progress");
        if progress.pending.is_none() {
            return progress;
        }
        assert!(
            Instant::now() < deadline,
            "the background checkpoint did not finish"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Waits until the writer's metrics say a background checkpoint failed (without collecting the failure).
fn wait_failed(writer: &Writer) {
    let deadline = Instant::now() + LONG;
    while !writer.metrics().poisoned {
        assert!(
            Instant::now() < deadline,
            "the background checkpoint did not fail"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn marker(dir: &Path) -> Option<CheckpointSlot> {
    inspect::read_checkpoint(dir).unwrap()
}

fn reopen(dir: &Path) -> Writer {
    Writer::open(dir, small_config()).unwrap()
}

/// The request returns while its syncs have not started, appends continue, and the marker it writes names the
/// position at the request, not the records appended meanwhile.
#[test]
fn request_returns_before_its_syncs_and_covers_its_frontier() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 100);
    let hold = fault::hold_background(Point::CheckpointSync);
    assert_eq!(w.request_checkpoint().unwrap(), 100);
    assert!(hold.wait_reached(LONG));
    let progress = w.checkpoint_progress().unwrap();
    assert_eq!(progress.durable, None);
    assert_eq!(progress.pending, Some(100));
    append_n(&mut w, 200);
    let metrics = w.metrics();
    assert_eq!(metrics.uncheckpointed_records, 300);
    assert!(metrics.uncheckpointed_bytes > 0);
    assert_eq!(marker(dir.path()), None, "no marker before the syncs");
    hold.release();
    let progress = wait_finished(&mut w);
    let durable = progress.durable.expect("durable");
    assert_eq!(durable.next_seq, 100);
    assert_eq!(marker(dir.path()), Some(durable));
    assert_eq!(w.metrics().uncheckpointed_records, 200);
    assert!(w.metrics().since_checkpoint.is_some());
    drop(w);
    let w = reopen(dir.path());
    assert_eq!(w.recovery_report().scanned_from_seq, 100);
    assert_eq!(w.next_seq(), 300);
    assert!(w.recovery_report().actions.is_empty());
}

/// Segments the writer rolls into while a checkpoint is in flight belong to the next checkpoint: the marker names the
/// segment and offset at the request, recovery from it finds every record, and the next request covers the rest.
#[test]
fn rolls_during_a_background_checkpoint() {
    let _serial = serial();
    for point in [Point::CheckpointSync, Point::CheckpointBeforeMarker] {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut w, 50);
        let hold = fault::hold_background(point);
        assert_eq!(w.request_checkpoint().unwrap(), 50);
        assert!(hold.wait_reached(LONG));
        append_n(&mut w, 2_000);
        assert!(
            segments(dir.path()).len() >= 4,
            "{:?}",
            segments(dir.path())
        );
        hold.release();
        let first = wait_finished(&mut w).durable.unwrap();
        assert_eq!((first.next_seq, first.segment_base_seq), (50, 0));
        assert_eq!(w.request_checkpoint().unwrap(), 2_050);
        let second = wait_finished(&mut w).durable.unwrap();
        assert_eq!(second.next_seq, 2_050);
        assert_eq!(second.generation, first.generation + 1);
        assert_eq!(
            second.segment_base_seq,
            segments(dir.path()).last().unwrap().0
        );
        drop(w);
        let w = reopen(dir.path());
        assert_eq!(w.recovery_report().scanned_from_seq, 2_050, "{point:?}");
        assert!(w.recovery_report().actions.is_empty());
        let records = read_all(dir.path());
        assert_eq!(records.len(), 2_050);
        records.iter().for_each(check_record);
    }
}

/// Requests made while a checkpoint runs wait for it and join each other: one more marker, covering the newest.
#[test]
fn requests_while_one_runs_are_coalesced() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 10);
    w.checkpoint().unwrap();
    let base = marker(dir.path()).unwrap().generation;
    append_n(&mut w, 90);
    let hold = fault::hold_background(Point::CheckpointSync);
    assert_eq!(w.request_checkpoint().unwrap(), 100);
    assert!(hold.wait_reached(LONG));
    append_n(&mut w, 400); // rolls
    assert_eq!(w.request_checkpoint().unwrap(), 500);
    append_n(&mut w, 100);
    assert_eq!(w.request_checkpoint().unwrap(), 600);
    assert_eq!(w.checkpoint_progress().unwrap().pending, Some(600));
    hold.release();
    let durable = wait_finished(&mut w).durable.unwrap();
    assert_eq!(durable.next_seq, 600);
    assert_eq!(
        durable.generation,
        base + 2,
        "one marker for the joined requests"
    );
    drop(w);
    assert_eq!(reopen(dir.path()).recovery_report().scanned_from_seq, 600);
}

/// `checkpoint()` keeps its meaning with a background checkpoint in flight: it waits for it, then makes everything
/// appended so far durable.
#[test]
fn blocking_checkpoint_waits_for_a_background_one() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 100);
    let hold = fault::hold_background(Point::CheckpointSync);
    w.request_checkpoint().unwrap();
    assert!(hold.wait_reached(LONG));
    append_n(&mut w, 100);
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        hold.release();
    });
    w.checkpoint().unwrap();
    releaser.join().unwrap();
    let progress = w.checkpoint_progress().unwrap();
    assert_eq!(progress.pending, None);
    let durable = progress.durable.unwrap();
    assert_eq!((durable.next_seq, durable.generation), (200, 2));
    assert_eq!(marker(dir.path()), Some(durable));
}

/// How each caller learns about a failed background checkpoint.
#[derive(Debug, Clone, Copy)]
enum Reporter {
    Progress,
    Request,
    Checkpoint,
    Close,
}

/// A failure on the background path is returned by the next call that reports on checkpoints, once; after it the
/// writer is poisoned (`DurabilityLost` everywhere, nothing reported durable), appends keep working, and the marker on
/// disk is the last good one (for a failure before the marker's write).
#[test]
fn background_failure_reaches_the_next_call_once() {
    let _serial = serial();
    for point in [
        Point::CheckpointSync,
        Point::CheckpointBeforeMarker,
        Point::CheckpointMarkerSync,
    ] {
        for reporter in [
            Reporter::Progress,
            Reporter::Request,
            Reporter::Checkpoint,
            Reporter::Close,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut w = Writer::open(dir.path(), small_config()).unwrap();
            append_n(&mut w, 100);
            w.checkpoint().unwrap();
            let good = marker(dir.path()).unwrap();
            append_n(&mut w, 100);
            fault::fail_next_background(point, EIO);
            assert_eq!(w.request_checkpoint().unwrap(), 200);
            append_n(&mut w, 10);
            wait_failed(&w);
            let reported = match reporter {
                Reporter::Progress => w.checkpoint_progress().map(|_| ()),
                Reporter::Request => w.request_checkpoint().map(|_| ()),
                Reporter::Checkpoint => w.checkpoint(),
                Reporter::Close => {
                    let closed = w.close();
                    assert!(
                        matches!(closed, Err(Error::Io { .. })),
                        "{point:?}: {closed:?}"
                    );
                    if point != Point::CheckpointMarkerSync {
                        assert_eq!(marker(dir.path()), Some(good));
                    }
                    continue;
                }
            };
            assert!(
                matches!(reported, Err(Error::Io { .. })),
                "{point:?} {reporter:?}: {reported:?}"
            );
            assert!(matches!(
                w.checkpoint_progress(),
                Err(Error::DurabilityLost)
            ));
            assert!(matches!(w.request_checkpoint(), Err(Error::DurabilityLost)));
            assert!(matches!(w.checkpoint(), Err(Error::DurabilityLost)));
            assert!(w.metrics().poisoned);
            append_n(&mut w, 10);
            assert_eq!(read_all(dir.path()).len(), 220);
            if point != Point::CheckpointMarkerSync {
                assert_eq!(marker(dir.path()), Some(good), "{point:?} {reporter:?}");
            }
            assert!(matches!(w.close(), Err(Error::DurabilityLost)));
        }
    }
}

/// A crash between a background checkpoint's syncs and its marker, with appends and rolls in between (simulated in
/// process: the checkpoint fails there and the writer is dropped, which leaves the files as a crash there would):
/// recovery starts at the previous marker and loses no committed record.
#[test]
fn failure_between_the_syncs_and_the_marker_keeps_the_previous_marker() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 300);
    w.checkpoint().unwrap();
    append_n(&mut w, 300);
    let hold = fault::hold_background(Point::CheckpointBeforeMarker);
    w.request_checkpoint().unwrap();
    assert!(hold.wait_reached(LONG));
    append_n(&mut w, 1_000);
    fault::fail_next_background(Point::CheckpointBeforeMarker, EIO);
    hold.release();
    wait_failed(&w);
    assert!(matches!(w.close(), Err(Error::Io { .. })));
    let w = reopen(dir.path());
    assert_eq!(w.recovery_report().scanned_from_seq, 300);
    assert_eq!(w.next_seq(), 1_600);
    assert!(w.recovery_report().actions.is_empty());
    let records = read_all(dir.path());
    assert_eq!(records.len(), 1_600);
    records.iter().for_each(check_record);
}

/// The same with a real crash: the writer process is killed while its background checkpoint is held before the
/// marker, after appending and rolling; and, for comparison, killed after the checkpoint finished.
#[test]
fn writer_killed_between_the_syncs_and_the_marker() {
    let _serial = serial();
    for (point, expected_from) in [("before", 300), ("after", 600)] {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_tt-background-checkpoint"))
            .args(["crash"])
            .arg(dir.path())
            .args([point, "300", "300", "1000"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "ready", "{point}");
        child.kill().unwrap();
        child.wait().unwrap();
        let w = reopen(dir.path());
        assert_eq!(
            w.recovery_report().scanned_from_seq,
            expected_from,
            "{point}"
        );
        assert_eq!(w.next_seq(), 1_600, "{point}");
        let records = read_all(dir.path());
        assert_eq!(records.len(), 1_600);
        records.iter().for_each(check_record);
    }
}

/// A roll does not sync the new segment on the writer's thread (ADR 0038): the writer process killed right after a
/// roll linked the new segment, before it sealed the old one and before any checkpoint synced the new one, with no
/// checkpoint in flight and with one held before its syncs. The next writer opens under `Fail`, seals the old
/// segment (the header is in the page cache, so nothing is restored) and finds every record.
#[test]
fn writer_killed_between_a_rolls_link_and_the_sync_of_the_new_segment() {
    let _serial = serial();
    for (variant, marker_at) in [("plain", 300), ("background", 300)] {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_tt-background-checkpoint"))
            .args(["roll-crash"])
            .arg(dir.path())
            .args([variant, "300"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let appended: u64 = line
            .trim()
            .strip_prefix("ready ")
            .unwrap_or_else(|| panic!("{variant}: {line:?}"))
            .parse()
            .unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(marker(dir.path()).unwrap().next_seq, marker_at, "{variant}");
        let w = Writer::open(
            dir.path(),
            small_config().recovery(tickertape::RecoveryPolicy::Fail),
        )
        .unwrap();
        let actions = &w.recovery_report().actions;
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, tickertape::RecoveryAction::Sealed { .. })),
            "{variant}: {actions:?}"
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, tickertape::RecoveryAction::RestoredHeader { .. })),
            "{variant}: {actions:?}"
        );
        assert_eq!(w.next_seq(), appended, "{variant}");
        drop(w);
        let records = read_all(dir.path());
        assert_eq!(records.len() as u64, appended, "{variant}");
        records.iter().for_each(check_record);
    }
}

/// Dropping the writer waits for a checkpoint in flight (holding the lock meanwhile) and lets it write its marker.
#[test]
fn drop_waits_for_a_checkpoint_in_flight() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 100);
    let hold = fault::hold_background(Point::CheckpointBeforeMarker);
    assert_eq!(w.request_checkpoint().unwrap(), 100);
    assert!(hold.wait_reached(LONG));
    append_n(&mut w, 50);
    let (done, dropped) = mpsc::channel();
    let dropper = std::thread::spawn(move || {
        drop(w);
        done.send(()).unwrap();
    });
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        dropped.try_recv().is_err(),
        "drop returned with a checkpoint in flight"
    );
    assert!(matches!(
        Writer::open(dir.path(), small_config()),
        Err(Error::Locked { .. })
    ));
    hold.release();
    dropped.recv_timeout(LONG).unwrap();
    dropper.join().unwrap();
    assert_eq!(marker(dir.path()).unwrap().next_seq, 100);
    let w = reopen(dir.path());
    assert_eq!(w.recovery_report().scanned_from_seq, 100);
    assert_eq!(w.next_seq(), 150);
}

/// `close` waits for the checkpoints in flight and reports success.
#[test]
fn close_waits_and_reports_success() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 100);
    let hold = fault::hold_background(Point::CheckpointSync);
    w.request_checkpoint().unwrap();
    assert!(hold.wait_reached(LONG));
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        hold.release();
    });
    w.close().unwrap();
    releaser.join().unwrap();
    assert_eq!(marker(dir.path()).unwrap().next_seq, 100);
}

/// Retention racing with a checkpoint in flight removes only segments before the marker on disk, so never one the
/// checkpoint is syncing or its new marker names; the writer's own retention runs on the checkpoint thread after the
/// marker and its results reach the writer.
#[test]
fn retention_with_a_checkpoint_in_flight() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let policy = Policy::default().max_segments(2);
    let config = small_config().retention(policy.clone(), Action::Delete);
    let mut w = Writer::open(dir.path(), config.clone()).unwrap();
    append_n(&mut w, 1_000);
    w.checkpoint().unwrap();
    let before = w.retention_report().removed_segments;
    let old_marker = marker(dir.path()).unwrap();
    append_n(&mut w, 1_500);
    let hold = fault::hold_background(Point::CheckpointSync);
    let covered = w.request_checkpoint().unwrap();
    assert!(hold.wait_reached(LONG));
    let in_flight: Vec<u64> = segments(dir.path())
        .into_iter()
        .map(|(base, _)| base)
        .filter(|&base| base >= old_marker.segment_base_seq)
        .collect();
    assert!(in_flight.len() >= 3, "{in_flight:?}");
    // An archiver's retention run while the checkpoint is held.
    let plan = tickertape::retention::plan(dir.path(), &policy).unwrap();
    tickertape::retention::apply(dir.path(), &plan, &Action::Delete).unwrap();
    let left: Vec<u64> = segments(dir.path()).into_iter().map(|(b, _)| b).collect();
    for base in &in_flight {
        assert!(left.contains(base), "segment {base} removed: {left:?}");
    }
    append_n(&mut w, 200);
    hold.release();
    let durable = wait_finished(&mut w).durable.unwrap();
    assert_eq!(durable.next_seq, covered);
    // Retention runs after the marker is durable, so its results arrive a little later.
    let deadline = Instant::now() + LONG;
    while w.retention_report().removed_segments == before {
        assert!(Instant::now() < deadline, "{:?}", w.retention_report());
        std::thread::sleep(Duration::from_millis(1));
        w.checkpoint_progress().unwrap();
    }
    assert!(w.last_retention_error().is_none());
    let left = segments(dir.path());
    assert!(left.iter().any(|(b, _)| *b == durable.segment_base_seq));
    let from_marker = left
        .iter()
        .filter(|(b, _)| *b >= durable.segment_base_seq)
        .count();
    assert!(left.len() <= from_marker.max(2), "{left:?}");
    drop(w);
    let w = Writer::open(dir.path(), config).unwrap();
    assert_eq!(w.recovery_report().scanned_from_seq, covered);
    assert!(w.recovery_report().actions.is_empty());
    assert_eq!(w.next_seq(), 2_700);
}

/// Appends one record of 40,000 bytes: with `small_config`, each one rolls to a new segment.
fn append_big(w: &mut Writer) {
    w.append(&NewRecord {
        schema_id: 1,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload: &[42; 40_000],
    })
    .unwrap();
}

/// How long a test holds a background checkpoint so that the retention run after it, which may take as long as that
/// checkpoint while another one waits (ADR 0044), is limited by the segments the checkpoint covered and not by time:
/// far longer than the few removals it allows take.
const HOLD: Duration = Duration::from_secs(1);

/// Neither a checkpoint nor `close` waits for a backlog of retention work, and deleting a segment does not read its
/// records (ADR 0044, issue #45). A damaged seal on the oldest segment stops every removal while 28 segments become
/// eligible; every one also has a damaged record, so a deletion that read the records would refuse it. Once the seal
/// is repaired, the backlog is larger than what the next checkpoint covers (two segments): the retention run after it
/// stops after two removals when another checkpoint is queued, and after one more when `close` waits for the thread; a
/// later writer's retention at open removes the rest.
#[test]
fn retention_backlog_holds_up_neither_a_checkpoint_nor_close() {
    use std::os::unix::fs::FileExt;
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let config = small_config().retention(Policy::default().max_segments(2), Action::Delete);
    let mut w = Writer::open(dir.path(), config.clone()).unwrap();
    for _ in 0..30 {
        append_big(&mut w);
    }
    let bases = |dir: &Path| -> Vec<u64> { segments(dir).into_iter().map(|(b, _)| b).collect() };
    assert_eq!(bases(dir.path()), (0..30).collect::<Vec<_>>());
    for (_, path) in &segments(dir.path())[..28] {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.write_all_at(&[0], 64 + 40 + 1_000).unwrap();
    }
    // The seal follows the one record (a 64-byte header, then a 40-byte frame header and 40,000 bytes): flip a byte of
    // its checksum.
    let oldest = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&segments(dir.path())[0].1)
        .unwrap();
    let seal_crc = 64 + 40 + 40_000 + 4;
    let mut byte = [0u8];
    oldest.read_exact_at(&mut byte, seal_crc).unwrap();
    oldest.write_all_at(&[!byte[0]], seal_crc).unwrap();

    // The first checkpoint makes segments 0 to 28 eligible; its retention run stops at the damaged seal.
    assert_eq!(w.request_checkpoint().unwrap(), 30);
    assert_eq!(wait_finished(&mut w).durable.unwrap().next_seq, 30);
    let deadline = Instant::now() + LONG;
    while w.metrics().retention_failures == 0 {
        assert!(Instant::now() < deadline, "the retention run did not fail");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(bases(dir.path()), (0..30).collect::<Vec<_>>());
    oldest.write_all_at(&byte, seal_crc).unwrap();

    // The next checkpoint covers two segments (29, now sealed, and 30), and is held while another is queued.
    append_big(&mut w);
    let first = fault::hold_background(Point::CheckpointSync);
    assert_eq!(w.request_checkpoint().unwrap(), 31);
    assert!(first.wait_reached(LONG));
    append_big(&mut w);
    assert_eq!(w.request_checkpoint().unwrap(), 32);
    // Its retention run holds before its first removal, so that the queued checkpoint can be held at its marker.
    let removing = fault::hold_background(Point::RetentionRemove);
    std::thread::sleep(HOLD);
    first.release();
    assert!(removing.wait_reached(LONG));
    let at_marker = fault::hold_background(Point::CheckpointBeforeMarker);
    removing.release();
    // The queued checkpoint reaches its marker after two removals, as many as the checkpoint before it covered.
    assert!(at_marker.wait_reached(LONG));
    assert_eq!(bases(dir.path()), (2..32).collect::<Vec<_>>());

    let second = fault::hold_background(Point::RetentionRemove);
    at_marker.release();
    assert!(second.wait_reached(LONG));
    assert_eq!(wait_finished(&mut w).durable.unwrap().next_seq, 32);
    assert_eq!(w.retention_report().removed_segments, 2);

    // `close` tells the thread it waits; the run stops after the removal in progress.
    fault::run_at(Point::CheckpointWait, 0, move || second.release());
    w.close().unwrap();
    assert_eq!(bases(dir.path()), (3..32).collect::<Vec<_>>());

    let w = Writer::open(dir.path(), config).unwrap();
    assert!(w.last_retention_error().is_none());
    assert_eq!(w.retention_report().removed_segments, 27);
    assert_eq!(bases(dir.path()), [30, 31]);
    assert_eq!(w.next_seq(), 32);
}

/// Retention keeps level with the writer when a checkpoint is always queued (issue #45). Each checkpoint is requested
/// while the one before it is still held, as when checkpoints take longer than the interval between requests (a writer
/// that outruns the disk), and every one covers ten new segments. The retention run after each checkpoint therefore
/// always finds a job waiting; it must still remove every segment that checkpoint made eligible, not one per
/// checkpoint, so the journal stays at its limit instead of growing by nine segments per checkpoint.
#[test]
fn retention_keeps_pace_under_a_constant_queue_of_checkpoints() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let config = small_config().retention(Policy::default().max_segments(2), Action::Delete);
    let mut w = Writer::open(dir.path(), config).unwrap();
    let bases = |dir: &Path| -> Vec<u64> { segments(dir).into_iter().map(|(b, _)| b).collect() };
    for _ in 0..10 {
        append_big(&mut w);
    }
    let mut running = fault::hold_background(Point::CheckpointSync);
    assert_eq!(w.request_checkpoint().unwrap(), 10);
    assert!(running.wait_reached(LONG));
    for round in 1..=3u64 {
        for _ in 0..10 {
            append_big(&mut w);
        }
        // Queued behind the held checkpoint.
        let next = fault::hold_background(Point::CheckpointSync);
        assert_eq!(w.request_checkpoint().unwrap(), 10 * (round + 1));
        // The held checkpoint takes at least this long, which is the time its retention run then has for ten
        // removals: generous, so that a slow directory sync on a busy machine does not cut the run short.
        std::thread::sleep(3 * HOLD);
        running.release();
        // The queued checkpoint starts only after the retention run that follows the one released.
        assert!(next.wait_reached(LONG));
        let marker = marker(dir.path()).unwrap();
        assert_eq!(marker.next_seq, 10 * round);
        assert_eq!(
            bases(dir.path()),
            (marker.segment_base_seq..10 * (round + 1)).collect::<Vec<_>>(),
            "round {round}: every segment before the marker's is removed"
        );
        running = next;
    }
    running.release();
    assert_eq!(wait_finished(&mut w).durable.unwrap().next_seq, 40);
    // With nothing queued, the last run removes everything the policy allows.
    let deadline = Instant::now() + LONG;
    while w.retention_report().removed_segments < 38 {
        assert!(Instant::now() < deadline, "{:?}", w.retention_report());
        std::thread::sleep(Duration::from_millis(1));
        w.checkpoint_progress().unwrap();
    }
    assert_eq!(bases(dir.path()), [38, 39]);
    assert!(w.last_retention_error().is_none());
    w.close().unwrap();
}

/// When removals are slower than checkpoints, a queued checkpoint waits for retention no longer than about as long
/// again as the checkpoint before it took (ADR 0044): durability before disk space. The first checkpoint makes 28
/// segments eligible and finishes quickly; its retention run is held before its first removal, far longer than the
/// checkpoint took, while another checkpoint is queued, so after that removal it is out of time and the queued
/// checkpoint reaches its marker with the other 27 still there.
#[test]
fn slow_retention_holds_up_a_queued_checkpoint_only_as_long_as_the_checkpoint_before_it() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let config = small_config().retention(Policy::default().max_segments(2), Action::Delete);
    let mut w = Writer::open(dir.path(), config).unwrap();
    for _ in 0..30 {
        append_big(&mut w);
    }
    let bases = |dir: &Path| -> Vec<u64> { segments(dir).into_iter().map(|(b, _)| b).collect() };
    let slow = fault::hold_background(Point::RetentionRemove);
    let requested = Instant::now();
    assert_eq!(w.request_checkpoint().unwrap(), 30);
    assert!(slow.wait_reached(LONG));
    // The checkpoint took at most this long, however slow the machine, so the run's time is over once the hold has
    // lasted longer than that.
    let took = requested.elapsed();
    append_big(&mut w);
    let at_marker = fault::hold_background(Point::CheckpointBeforeMarker);
    assert_eq!(w.request_checkpoint().unwrap(), 31);
    std::thread::sleep(took + HOLD);
    slow.release();
    assert!(at_marker.wait_reached(LONG));
    assert_eq!(bases(dir.path()), (1..31).collect::<Vec<_>>());
    at_marker.release();
    assert_eq!(wait_finished(&mut w).durable.unwrap().next_seq, 31);
    // Nothing waits for the run after the second checkpoint: it removes the rest.
    let deadline = Instant::now() + LONG;
    while w.retention_report().removed_segments < 28 {
        assert!(Instant::now() < deadline, "{:?}", w.retention_report());
        std::thread::sleep(Duration::from_millis(1));
        w.checkpoint_progress().unwrap();
    }
    assert_eq!(bases(dir.path()), [29, 30]);
    assert!(w.last_retention_error().is_none());
    w.close().unwrap();
}

/// An abandoned write whose zeroing cannot be synced while a background checkpoint waits to start: the checkpoint must
/// not write its marker afterwards (a failed sync may have dropped dirty pages of the same file).
#[test]
fn failed_abandoned_write_stops_a_background_checkpoint() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 100);
    w.checkpoint().unwrap();
    let good = marker(dir.path()).unwrap();
    append_n(&mut w, 100);
    let hold = fault::hold_background(Point::CheckpointSync);
    w.request_checkpoint().unwrap();
    assert!(hold.wait_reached(LONG));
    fault::fail_next(Point::AbandonedWriteSync, EIO);
    let record = NewRecord {
        schema_id: 1,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload: &[],
    };
    let abandoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        w.append_with(&record, 16, |_| std::panic::panic_any("abandon"))
    }));
    assert!(abandoned.is_err());
    assert!(w.abandoned_write_error().is_some());
    hold.release();
    wait_failed(&w);
    assert!(matches!(
        w.checkpoint_progress(),
        Err(Error::DurabilityLost)
    ));
    assert_eq!(marker(dir.path()), Some(good));
    assert!(matches!(w.close(), Err(Error::DurabilityLost)));
}

/// Crash contract C13 across processes: a durable-only reader in another process sees the marker move to the
/// request's frontier while the writer keeps appending, and never a record past it.
#[test]
fn durable_only_reader_process_sees_the_marker_move_while_appends_continue() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 500);
    w.checkpoint().unwrap();
    let reader = Command::new(env!("CARGO_BIN_EXE_tt-background-checkpoint"))
        .arg("durable-follow")
        .arg(dir.path())
        .arg("1500")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    append_n(&mut w, 1_000);
    assert_eq!(w.request_checkpoint().unwrap(), 1_500);
    // Keep appending while the checkpoint runs, and for a while after it finished.
    let deadline = Instant::now() + LONG;
    let mut after = 0;
    while after < 2_000 {
        append_n(&mut w, 1);
        if w.checkpoint_progress().unwrap().pending.is_none() {
            after += 1;
        }
        assert!(Instant::now() < deadline);
    }
    assert_eq!(
        w.checkpoint_progress().unwrap().durable.unwrap().next_seq,
        1_500
    );
    let output = reader.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "1500\n");
    drop(w);
}

/// Early write-back (Linux, ADR 0038): an I/O error from starting the write-back of segment data, behind the writer
/// (1 MiB steps in a large segment) or of a segment rolled past, poisons the writer at once (`metrics().poisoned`);
/// the next checkpoint returns that error and writes no marker, later ones `DurabilityLost`; appends continue.
#[cfg(target_os = "linux")]
#[test]
fn failed_early_writeback_fails_the_next_checkpoint() {
    let _serial = serial();
    for (config, records) in [
        (tickertape::WriterConfig::default(), 20_000),
        (small_config(), 1_000),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Writer::open(dir.path(), config.clone()).unwrap();
        append_n(&mut w, 10);
        w.checkpoint().unwrap();
        fault::fail_next_background(Point::EarlyWriteback, EIO);
        append_n(&mut w, records);
        wait_failed(&w);
        match w.checkpoint() {
            Err(Error::Io { op, .. }) => assert_eq!(op, "start write-back of segment data"),
            other => panic!("expected the write-back error, got {other:?}"),
        }
        assert!(matches!(w.checkpoint(), Err(Error::DurabilityLost)));
        assert_eq!(marker(dir.path()).unwrap().next_seq, 10);
        append_n(&mut w, 5);
        drop(w);
        let w = Writer::open(dir.path(), config).unwrap();
        assert_eq!(w.next_seq(), 10 + records + 5);
    }
}

/// A failure of the early write-back helper after the last checkpoint, with no background checkpoint pending, is
/// returned once by the next of `request_checkpoint`, `checkpoint_progress`, `checkpoint` and `close` (Linux, ADR
/// 0038 → Early write-back); after it, the others return `DurabilityLost`.
#[cfg(target_os = "linux")]
#[test]
fn failed_early_writeback_reaches_the_next_call_once() {
    let _serial = serial();
    for call in [
        "request_checkpoint",
        "checkpoint_progress",
        "checkpoint",
        "close",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut w, 10);
        w.request_checkpoint().unwrap();
        wait_finished(&mut w);
        fault::fail_next_background(Point::EarlyWriteback, EIO);
        append_n(&mut w, 1_000);
        wait_failed(&w);
        let result = match call {
            "request_checkpoint" => w.request_checkpoint().map(drop),
            "checkpoint_progress" => w.checkpoint_progress().map(drop),
            "checkpoint" => w.checkpoint(),
            _ => {
                let result = w.close();
                match result {
                    Err(Error::Io { op, .. }) => assert_eq!(op, "start write-back of segment data"),
                    other => panic!("close: expected the write-back error, got {other:?}"),
                }
                assert_eq!(marker(dir.path()).unwrap().next_seq, 10);
                continue;
            }
        };
        match result {
            Err(Error::Io { op, .. }) => {
                assert_eq!(op, "start write-back of segment data", "{call}")
            }
            other => panic!("{call}: expected the write-back error, got {other:?}"),
        }
        {
            assert!(
                matches!(w.request_checkpoint(), Err(Error::DurabilityLost)),
                "{call}"
            );
            assert!(
                matches!(w.checkpoint_progress(), Err(Error::DurabilityLost)),
                "{call}"
            );
            assert!(
                matches!(w.checkpoint(), Err(Error::DurabilityLost)),
                "{call}"
            );
            assert!(matches!(w.close(), Err(Error::DurabilityLost)), "{call}");
        }
        assert_eq!(marker(dir.path()).unwrap().next_seq, 10, "{call}");
    }
}
