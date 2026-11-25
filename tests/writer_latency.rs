//! The writer's latency histograms (ADR 0047, step 3): what each mode records, and that it changes nothing else.
//!
//! The holds and background failure points are process-wide, so the tests that make checkpoints or rolls fail, or that
//! use the background threads, run one at a time.

use std::cell::Cell;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tickertape::fault::{self, Point};
use tickertape::latency::{Snapshot, View, WriterLatency};
use tickertape::retention::{Action, Policy};
use tickertape::{Error, LatencyMode, NewRecord, Writer, WriterConfig};
use tickertape_tests::{Rng, append_n, mixed_len, payload, read_all, segments, small_config};

const EIO: i32 = 5;
const ENOSPC: i32 = 28;
const LONG: Duration = Duration::from_secs(30);
const MODES: [LatencyMode; 3] = [
    LatencyMode::Off,
    LatencyMode::RollsAndCheckpoints,
    LatencyMode::All,
];

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    fault::clear();
    guard
}

fn record(payload: &[u8]) -> NewRecord<'_> {
    NewRecord {
        schema_id: 1,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload,
    }
}

fn open(dir: &Path, mode: LatencyMode) -> Writer {
    // Without helper threads: every segment is made by the appending thread, where the failure points are.
    Writer::open(dir, small_config().prefault(false).latency(mode)).unwrap()
}

fn all(dir: &Path) -> (Writer, WriterLatency) {
    let writer = open(dir, LatencyMode::All);
    let latency = writer.latency().unwrap().clone();
    (writer, latency)
}

fn snapshot(view: Option<&View>) -> Snapshot {
    view.expect("not measured in this mode").snapshot()
}

/// The counts of append, batch, roll and time_to_durable; `None` for what is not measured.
fn counts(latency: &WriterLatency) -> [Option<u64>; 4] {
    [
        latency.append(),
        latency.batch(),
        latency.roll(),
        latency.time_to_durable(),
    ]
    .map(|view| view.map(|view| view.snapshot().count()))
}

fn counted(n: [u64; 4]) -> [Option<u64>; 4] {
    n.map(Some)
}

/// A record that takes more than half of a 64 KiB segment, so that each one after the first rolls.
fn append_big(writer: &mut Writer) -> Result<u64, Error> {
    writer.append(&record(&[7; 40_000]))
}

fn wait_finished(writer: &mut Writer) {
    let deadline = Instant::now() + LONG;
    while writer.checkpoint_progress().unwrap().pending.is_some() {
        assert!(Instant::now() < deadline, "the checkpoint did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn wait_failed(writer: &Writer) {
    let deadline = Instant::now() + LONG;
    while !writer.metrics().poisoned {
        assert!(Instant::now() < deadline, "the checkpoint did not fail");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn off_is_the_default_and_measures_nothing() {
    assert_eq!(WriterConfig::default().latency, LatencyMode::Off);
    assert_eq!(LatencyMode::default(), LatencyMode::Off);
    let dir = tempfile::tempdir().unwrap();
    let writer = Writer::open(dir.path(), WriterConfig::default()).unwrap();
    assert!(writer.latency().is_none());
    drop(writer);
    let writer = open(dir.path(), LatencyMode::Off);
    assert!(writer.latency().is_none());
}

/// Every mode, the same calls: the counts are exactly what the mode measures.
#[test]
fn each_mode_records_what_it_says_and_nothing_else() {
    let _serial = serial();
    for mode in MODES {
        let dir = tempfile::tempdir().unwrap();
        let mut w = open(dir.path(), mode);
        let latency = w.latency().cloned();
        let data = [7; 1_000];
        // 3 x 100 appends, and 20 batches of 10: about 520 KB, in segments of 64 KiB.
        for _ in 0..100 {
            w.append(&record(&data)).unwrap();
            w.append_with(&record(&[]), 1_000, |buf| buf.fill(7))
                .unwrap();
            w.try_append_with(&record(&[]), 1_000, |buf| {
                buf.fill(7);
                Ok::<(), ()>(())
            })
            .unwrap()
            .unwrap();
        }
        let in_appends = segments(dir.path()).len() as u64 - 1;
        for _ in 0..20 {
            w.append_batch([record(&data); 10]).unwrap();
        }
        let rolls = segments(dir.path()).len() as u64 - 1;
        assert!(in_appends >= 3 && rolls >= in_appends + 2);
        for _ in 0..3 {
            w.append(&record(&data)).unwrap();
            w.checkpoint().unwrap();
        }
        // Background ones, each finished before the next.
        for _ in 0..2 {
            w.append(&record(&data)).unwrap();
            w.request_checkpoint().unwrap();
            wait_finished(&mut w);
        }
        let rolls_now = segments(dir.path()).len() as u64 - 1;
        let Some(latency) = latency else {
            assert_eq!(mode, LatencyMode::Off);
            continue;
        };
        let expected = match mode {
            LatencyMode::RollsAndCheckpoints => [None, None, Some(rolls_now), Some(5)],
            _ => counted([305, 20, rolls_now, 5]),
        };
        assert_eq!(counts(&latency), expected, "{mode:?}");
        // The views outlive the writer.
        drop(w);
        assert_eq!(counts(&latency), expected, "{mode:?}");
    }
}

#[test]
fn the_views_can_be_moved_to_another_thread() {
    fn send_and_sync<T: Send + Sync + Clone + std::fmt::Debug + 'static>() {}
    send_and_sync::<WriterLatency>();
    fn send<T: Send>() {}
    send::<Writer>();
}

/// The extremes are those of the calls: a closure, and a batch's iterator, that take 20 ms.
#[test]
fn a_slow_call_is_the_maximum_and_stays_it() {
    let slow = Duration::from_millis(20);
    let dir = tempfile::tempdir().unwrap();
    let (mut w, latency) = all(dir.path());
    for _ in 0..50 {
        w.append(&record(b"fast")).unwrap();
        w.append_batch([record(b"fast")]).unwrap();
    }
    w.append_with(&record(&[]), 8, |_| std::thread::sleep(slow))
        .unwrap();
    let in_append = snapshot(latency.append());
    assert!(in_append.max() >= Some(slow), "{in_append:?}");
    w.append_batch((0..3).map(|_| {
        std::thread::sleep(slow);
        record(b"slow to come")
    }))
    .unwrap();
    let in_batch = snapshot(latency.batch());
    assert!(in_batch.max() >= Some(3 * slow), "{in_batch:?}");
    for _ in 0..50 {
        w.append(&record(b"fast")).unwrap();
        w.append_batch([record(b"fast")]).unwrap();
    }
    let append = snapshot(latency.append());
    let batch = snapshot(latency.batch());
    assert_eq!((append.count(), batch.count()), (101, 101));
    // Another call can have been even slower (a busy machine), never hide that one.
    assert!(append.max() >= in_append.max() && batch.max() >= in_batch.max());
    assert!(append.min() < Some(slow), "{append:?}");
}

/// A roll inside an append, or inside a batch, is in `roll` and in the sample of that call.
#[test]
fn a_roll_is_counted_by_itself_and_in_its_call() {
    let _serial = serial();
    let slow = Duration::from_millis(20);
    let dir = tempfile::tempdir().unwrap();
    let (mut w, latency) = all(dir.path());
    append_big(&mut w).unwrap();
    assert_eq!(counts(&latency), counted([1, 0, 0, 0]));
    fault::run_at(Point::RollAfterLink, 0, move || std::thread::sleep(slow));
    append_big(&mut w).unwrap();
    assert_eq!(counts(&latency), counted([2, 0, 1, 0]));
    let roll = snapshot(latency.roll());
    assert!(roll.min() >= Some(slow), "{roll:?}");
    assert!(snapshot(latency.append()).max() >= roll.max());
    fault::run_at(Point::RollAfterLink, 0, move || std::thread::sleep(slow));
    let data = [7; 40_000];
    w.append_batch([record(&data)]).unwrap();
    assert_eq!(counts(&latency), counted([2, 1, 2, 0]));
    let interval = snapshot(latency.roll()).since(&roll);
    assert!(interval.min() >= Some(slow), "{interval:?}");
    assert!(snapshot(latency.batch()).max() >= interval.min());
    // Two rolls in one batch are two samples.
    w.append_batch([record(&data), record(&data)]).unwrap();
    assert_eq!(counts(&latency), counted([2, 2, 4, 0]));
}

#[test]
fn failed_declined_and_panicking_appends_record_nothing() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let (mut w, latency) = all(dir.path());
    w.append(&record(b"one")).unwrap();
    let before = counts(&latency);
    assert!(matches!(
        w.append(&record(&[0; 70_000])),
        Err(Error::RecordTooLarge { .. })
    ));
    let declined = w.try_append_with(&record(&[]), 8, |_| Err("no"));
    assert!(matches!(declined, Ok(Err("no"))));
    let panicked = panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = w.append_with(&record(&[]), 8, |_| panic!("expected in this test"));
    }));
    assert!(panicked.is_err());
    assert_eq!(counts(&latency), before);
    // A roll made for a record that is then declined was a roll, and no append.
    append_big(&mut w).unwrap();
    let declined = w.try_append_with(&record(&[]), 40_000, |_| Err("no"));
    assert!(matches!(declined, Ok(Err("no"))));
    assert_eq!(counts(&latency), counted([2, 0, 1, 0]));
    // A declined append whose clean-up fails, and the appends that are refused after it.
    fault::fail_next(Point::AbandonedWriteSync, EIO);
    assert!(
        w.try_append_with(&record(&[]), 8, |_| Err::<(), _>("no"))
            .is_err()
    );
    assert!(w.append(&record(b"refused")).is_err());
    assert!(w.append_batch([record(b"refused")]).is_err());
    assert!(w.checkpoint().is_err());
    assert_eq!(counts(&latency), counted([2, 0, 1, 0]));
}

#[test]
fn a_failed_roll_records_nothing_and_its_retry_counts_once() {
    let _serial = serial();
    for point in [Point::Preallocate, Point::MapSegment, Point::Link] {
        let dir = tempfile::tempdir().unwrap();
        let (mut w, latency) = all(dir.path());
        append_big(&mut w).unwrap();
        fault::fail_next(point, ENOSPC);
        assert!(append_big(&mut w).is_err(), "{point:?}");
        assert_eq!(counts(&latency), counted([1, 0, 0, 0]), "{point:?}");
        append_big(&mut w).unwrap();
        assert_eq!(counts(&latency), counted([2, 0, 1, 0]), "{point:?}");
        fault::fail_next(point, ENOSPC);
        let data = [7; 40_000];
        let failed = w.append_batch([record(b"fits"), record(&data)]);
        assert_eq!(failed.unwrap_err().appended, 1, "{point:?}");
        assert_eq!(counts(&latency), counted([2, 0, 1, 0]), "{point:?}");
    }
}

#[test]
fn empty_and_failed_batches_record_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut w, latency) = all(dir.path());
    assert!(w.append_batch(std::iter::empty()).unwrap().is_empty());
    let too_large = [0; 70_000];
    let failed = w.append_batch([record(b"one"), record(&too_large), record(b"two")]);
    assert_eq!(failed.unwrap_err().appended, 1);
    let n = Cell::new(0);
    let panicked = panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = w.append_batch(std::iter::from_fn(|| {
            n.set(n.get() + 1);
            assert!(n.get() < 3, "expected in this test");
            Some(record(b"one"))
        }));
    }));
    assert!(panicked.is_err());
    assert_eq!(w.next_seq(), 3);
    assert_eq!(counts(&latency), counted([0; 4]));
    w.append_batch([record(b"one")]).unwrap();
    assert_eq!(counts(&latency), counted([0, 1, 0, 0]));
}

#[test]
fn a_failed_checkpoint_records_nothing() {
    let _serial = serial();
    for point in [Point::CheckpointSync, Point::CheckpointMarkerSync] {
        let dir = tempfile::tempdir().unwrap();
        let (mut w, latency) = all(dir.path());
        append_n(&mut w, 10);
        w.checkpoint().unwrap();
        fault::fail_next(point, EIO);
        assert!(w.checkpoint().is_err(), "{point:?}");
        assert!(matches!(w.checkpoint(), Err(Error::DurabilityLost)));
        assert!(matches!(w.request_checkpoint(), Err(Error::DurabilityLost)));
        assert_eq!(counts(&latency), counted([10, 0, 0, 1]), "{point:?}");
    }
}

#[test]
fn a_failed_background_checkpoint_records_nothing_nor_do_those_behind_it() {
    let _serial = serial();
    for point in [
        Point::CheckpointSync,
        Point::CheckpointBeforeMarker,
        Point::CheckpointMarkerSync,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (mut w, latency) = all(dir.path());
        append_n(&mut w, 10);
        let hold = fault::hold_background(Point::CheckpointSync);
        fault::fail_next_background(point, EIO);
        w.request_checkpoint().unwrap();
        assert!(hold.wait_reached(LONG));
        append_n(&mut w, 10);
        w.request_checkpoint().unwrap(); // Queued behind the one that fails: dropped without running.
        hold.release();
        wait_failed(&w);
        assert!(w.checkpoint().is_err());
        drop(w);
        assert_eq!(counts(&latency), counted([20, 0, 0, 0]), "{point:?}");
    }
}

/// Requests that join a waiting checkpoint give one sample, from the earliest of them; a finished checkpoint's sample
/// is there when `checkpoint_progress` reports it.
#[test]
fn joined_requests_are_one_sample_from_the_earliest() {
    let _serial = serial();
    let (first, second) = (Duration::from_millis(60), Duration::from_millis(40));
    for mode in [LatencyMode::RollsAndCheckpoints, LatencyMode::All] {
        let dir = tempfile::tempdir().unwrap();
        let mut w = open(dir.path(), mode);
        let latency = w.latency().unwrap().clone();
        append_n(&mut w, 10);
        let hold = fault::hold_background(Point::CheckpointSync);
        w.request_checkpoint().unwrap();
        assert!(hold.wait_reached(LONG));
        append_n(&mut w, 10);
        w.request_checkpoint().unwrap();
        std::thread::sleep(first);
        append_n(&mut w, 400); // rolls
        w.request_checkpoint().unwrap(); // joins
        std::thread::sleep(second);
        append_n(&mut w, 10);
        w.request_checkpoint().unwrap(); // joins
        assert_eq!(snapshot(latency.time_to_durable()).count(), 0);
        hold.release();
        wait_finished(&mut w);
        let durable = snapshot(latency.time_to_durable());
        assert_eq!(durable.count(), 2, "{mode:?}");
        // Both, the running one and the joined one, cover the whole time held.
        assert!(durable.min() >= Some(first + second), "{durable:?}");
    }
}

/// A blocking checkpoint includes waiting behind a background one: its sample starts at the entry of the call.
#[test]
fn a_blocking_checkpoint_includes_waiting_behind_a_background_one() {
    let _serial = serial();
    let held = Duration::from_millis(60);
    let dir = tempfile::tempdir().unwrap();
    let (mut w, latency) = all(dir.path());
    append_n(&mut w, 10);
    let hold = fault::hold_background(Point::CheckpointBeforeMarker);
    w.request_checkpoint().unwrap();
    assert!(hold.wait_reached(LONG));
    // Released `held` after the blocking checkpoint has started to wait.
    let (tx, rx) = std::sync::mpsc::channel();
    fault::run_at(Point::CheckpointWait, 0, move || tx.send(()).unwrap());
    let releasing = std::thread::spawn(move || {
        rx.recv().unwrap();
        std::thread::sleep(held);
        hold.release();
    });
    w.checkpoint().unwrap();
    releasing.join().unwrap();
    let durable = snapshot(latency.time_to_durable());
    assert_eq!(durable.count(), 2);
    assert!(durable.min() >= Some(held), "{durable:?}");
}

/// The sample of a background checkpoint is recorded before its retention run, so it does not include it.
#[test]
fn time_to_durable_excludes_retention() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut policy = Policy::default();
    policy.max_segments = Some(1);
    let config = small_config()
        .prefault(false)
        .latency(LatencyMode::RollsAndCheckpoints)
        .retention(policy, Action::Delete);
    let mut w = Writer::open(dir.path(), config).unwrap();
    let latency = w.latency().unwrap().clone();
    append_n(&mut w, 2_000);
    let hold = fault::hold_background(Point::RetentionRemove);
    w.request_checkpoint().unwrap();
    assert!(hold.wait_reached(LONG));
    assert_eq!(snapshot(latency.time_to_durable()).count(), 1);
    hold.release();
    w.close().unwrap();
    assert_eq!(snapshot(latency.time_to_durable()).count(), 1);
}

/// Dropping the writer finishes the requested checkpoints, and their samples are there afterwards.
#[test]
fn a_checkpoint_finished_by_the_drop_is_counted() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let (mut w, latency) = all(dir.path());
    append_n(&mut w, 10);
    let hold = fault::hold_background(Point::CheckpointSync);
    w.request_checkpoint().unwrap();
    assert!(hold.wait_reached(LONG));
    append_n(&mut w, 10);
    w.request_checkpoint().unwrap();
    hold.release();
    drop(w);
    assert_eq!(counts(&latency), counted([20, 0, 0, 2]));
}

/// Snapshots on another thread while the writer appends, rolls and checkpoints: counts never fall, the maximum is never
/// below a bucket that is counted, and nothing is lost.
#[test]
fn snapshots_while_the_writer_appends() {
    const APPENDS: u64 = if cfg!(debug_assertions) {
        30_000
    } else {
        300_000
    };
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(
        dir.path(),
        WriterConfig::default()
            .segment_size(1 << 20)
            .latency(LatencyMode::All),
    )
    .unwrap();
    let latency = w.latency().unwrap().clone();
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let reading = scope.spawn(|| {
            let views = [
                latency.append().unwrap(),
                latency.batch().unwrap(),
                latency.roll().unwrap(),
                latency.time_to_durable().unwrap(),
            ];
            let mut last: Vec<Snapshot> = views.iter().map(|view| view.snapshot()).collect();
            let mut taken = 0u64;
            while !done.load(Ordering::Acquire) {
                for (view, last) in views.iter().zip(&mut last) {
                    let now = view.snapshot();
                    assert!(now.count() >= last.count());
                    assert_eq!(now.since(last).count(), now.count() - last.count());
                    for bucket in now.buckets() {
                        assert!(now.max() >= Some(Duration::from_nanos(bucket.low_ns)));
                        assert!(now.min() <= Some(Duration::from_nanos(bucket.high_ns)));
                    }
                    *last = now;
                }
                taken += 1;
            }
            taken
        });
        for i in 0..APPENDS {
            w.append(&record(&[7; 64])).unwrap();
            if i % 10 == 0 {
                w.append_batch([record(&[7; 64]); 2]).unwrap();
            }
            if i % 5_000 == 0 {
                w.request_checkpoint().unwrap();
            }
        }
        wait_finished(&mut w);
        done.store(true, Ordering::Release);
        assert!(reading.join().unwrap() > 0);
    });
    let rolls = segments(dir.path()).len() as u64 - 1;
    let [append, batch, roll, durable] = counts(&latency);
    assert_eq!(
        (append, batch, roll),
        (Some(APPENDS), Some(APPENDS / 10), Some(rolls))
    );
    // Requests join each other, so there are at most as many checkpoints as requests.
    assert!((1..=APPENDS / 5_000).contains(&durable.unwrap()));
}

/// The same appends give the same bytes whatever the mode, except for the segments' creation time (and so their header's
/// CRC), and leave the same files.
#[test]
fn the_journal_does_not_depend_on_the_mode() {
    let _serial = serial();
    let write = |mode| {
        let dir = tempfile::tempdir().unwrap();
        let mut w = open(dir.path(), mode);
        let mut rng = Rng::new(7);
        for i in 0..300u64 {
            let data = payload(i, mixed_len(i));
            w.append(&record(&data)).unwrap();
            if i % 7 == 0 {
                w.append_batch([record(&data), record(b"batched")]).unwrap();
            }
            if rng.below(40) == 0 {
                w.checkpoint().unwrap();
            }
        }
        w.close().unwrap();
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        let files: Vec<Vec<u8>> = segments(dir.path())
            .iter()
            .map(|(_, path)| std::fs::read(path).unwrap()[64..].to_vec())
            .collect();
        let marker = std::fs::read(dir.path().join("checkpoint")).unwrap();
        (names, files, marker, read_all(dir.path()).len())
    };
    let off = write(LatencyMode::Off);
    assert!(off.1.len() > 3, "expected rolls");
    for mode in MODES {
        let other = write(mode);
        assert_eq!(other.0, off.0, "{mode:?}");
        assert!(other.1 == off.1, "{mode:?}: the segments differ");
        assert!(other.2 == off.2, "{mode:?}: the markers differ");
        assert_eq!(other.3, off.3);
    }
}

/// The child of [`appends_that_do_not_roll_make_no_system_call`]: appends between two system calls that no append makes.
#[test]
fn child_that_appends() {
    let Ok(dir) = std::env::var("TT_WRITER_LATENCY_CHILD") else {
        return;
    };
    for mode in MODES {
        let dir = Path::new(&dir).join(format!("{mode:?}"));
        // In one segment of 128 MiB, without helper threads to hand work to (a hand-off wakes a thread, with any mode).
        let config = WriterConfig::default().prefault(false).latency(mode);
        let mut w = Writer::open(&dir, config).unwrap();
        // Everything that is done once (the CRC's tables) happens before the marks.
        append_n(&mut w, 100);
        let batch = [record(&[7; 64]); 16];
        w.append_batch(batch).unwrap();
        let _ = std::fs::metadata(format!("/tickertape-mark-begin-{mode:?}"));
        for _ in 0..20_000 {
            w.append(&batch[0]).unwrap();
        }
        for _ in 0..2_000 {
            w.append_batch(batch).unwrap();
        }
        let _ = std::fs::metadata(format!("/tickertape-mark-end-{mode:?}"));
    }
}

/// With every mode, an append that does not roll makes no system call, where the clock is read without one (which the
/// kernel decides). Needs `strace`: skipped without it, unless `TICKERTAPE_REQUIRE_STRACE` is set (as in CI).
#[cfg(target_os = "linux")]
#[test]
fn appends_that_do_not_roll_make_no_system_call() {
    const VDSO: [&str; 4] = [
        "tsc",
        "kvm-clock",
        "hyperv_clocksource_tsc_page",
        "arch_sys_counter",
    ];
    let skip = |why: String| {
        assert!(
            std::env::var_os("TICKERTAPE_REQUIRE_STRACE").is_none(),
            "{why}"
        );
        eprintln!("skipped: {why}");
    };
    let source = "/sys/devices/system/clocksource/clocksource0/current_clocksource";
    let source = std::fs::read_to_string(source).unwrap_or_default();
    if !VDSO.contains(&source.trim()) {
        return skip(format!("the clock source is {source:?}"));
    }
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("trace");
    let strace = std::process::Command::new("strace")
        .args(["-f", "-o"])
        .arg(&trace)
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "child_that_appends"])
        .env("TT_WRITER_LATENCY_CHILD", dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match strace {
        Ok(status) if status.success() => {}
        other => return skip(format!("strace: {other:?}")),
    }
    let trace = std::fs::read_to_string(&trace).unwrap();
    for mode in MODES {
        let mark = |what: &str| {
            let name = format!("/tickertape-mark-{what}-{mode:?}\"");
            let at = trace.lines().position(|line| line.contains(&name));
            at.unwrap_or_else(|| panic!("{name} is not in the trace"))
        };
        let (begin, end) = (mark("begin"), mark("end"));
        assert!(begin < end);
        // `-f` starts each line with the thread's id; other threads (the test harness's) are not the appending one.
        let thread = |line: &str| line.split_whitespace().next().map(str::to_owned);
        let appending = thread(trace.lines().nth(begin).unwrap());
        let made: Vec<&str> = trace
            .lines()
            .skip(begin + 1)
            .take(end - begin - 1)
            .filter(|line| thread(line) == appending)
            // A signal is no system call of the thread's.
            .filter(|line| !line.contains("--- SIG"))
            .collect();
        assert!(made.is_empty(), "{mode:?}: {made:?}");
    }
}
