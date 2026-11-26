//! The segments a writer's dirty lists release (ADR 0038 → Bounded open segments) are unmapped and closed by the
//! prefault helper, not on the writer's thread (ADR 0059): at most `prefault::RELEASE_QUEUE` (2) wait for it, the one it
//! is releasing included; beyond that, or once the helper has ended, the writer releases them itself, as without the
//! helper. Nothing here measures time: the helper is held at its release point, or made to panic there.
//!
//! Linux only: on macOS the writer releases the segments itself (ADR 0059), so the tests of the hand-off are skipped
//! there with a message, and one test checks that nothing is handed over.
//!
//! The holds and background failure points are process-wide, so the tests run one at a time.

use std::path::Path;
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tickertape::Writer;
use tickertape::fault::{self, Point};
use tickertape_tests::{append_n, small_config};

/// The writer config of these tests: small segments, without the early write-back helper (Linux), which can hold a
/// segment's file for a moment after the writer let go of it and so move the descriptor counts by one
/// (`tests/open_segments.rs` counts it).
fn config() -> tickertape::WriterConfig {
    small_config().early_writeback(false)
}

/// `prefault::RELEASE_QUEUE`.
const RELEASE_QUEUE: usize = 2;
/// Segments a dirty list keeps open (`checkpoint::OPEN_SEGMENTS`): the fifth roll is the first that releases one.
const OPEN_SEGMENTS: u64 = 4;
const LONG: Duration = Duration::from_secs(60);

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    fault::clear();
    guard
}

/// Appends until `writer` has rolled `rolls` times.
fn roll(writer: &mut Writer, rolls: u64) {
    let mut base = writer.metrics().head.segment_base_seq;
    let mut rolled = 0;
    while rolled < rolls {
        append_n(writer, 1);
        let now = writer.metrics().head.segment_base_seq;
        if now != base {
            base = now;
            rolled += 1;
        }
    }
}

/// Whether this platform hands released segments to the prefault helper; if not, says that the test is skipped.
fn hands_off(test: &str) -> bool {
    if cfg!(target_os = "linux") {
        return true;
    }
    use std::io::Write;
    // Not `eprintln!`, which the test harness captures: a skip must be seen.
    let _ = writeln!(
        std::io::stderr(),
        "skipped {test}: the writer releases segments on its own thread on this platform (ADR 0059)"
    );
    false
}

/// Waits until `done(writer)`.
fn wait_for(writer: &Writer, what: &str, done: impl Fn(&Writer) -> bool) {
    let deadline = Instant::now() + LONG;
    while !done(writer) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Waits until the helper has released everything handed to it.
fn settle(writer: &Writer) {
    wait_for(writer, "the helper to release what it was given", |w| {
        w.segments_awaiting_release() == 0
    });
}

/// Open descriptors of this process.
fn descriptors() -> usize {
    std::fs::read_dir("/dev/fd").map_or(0, |d| d.count())
}

/// Mappings of files under `dir` (Linux; 0 elsewhere).
fn mappings(dir: &Path) -> usize {
    let dir = dir.to_string_lossy();
    std::fs::read_to_string("/proc/self/maps")
        .map_or(0, |m| m.lines().filter(|l| l.contains(&*dir)).count())
}

/// Runs `f` on another thread with `writer`, and fails (after `cleanup`) if it has not returned within [`LONG`]: a
/// writer that waited for a held helper would never return.
fn within<T: Send + 'static>(
    writer: Writer,
    cleanup: impl FnOnce(),
    f: impl FnOnce(Writer) -> T + Send + 'static,
) -> T {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let _ = tx.send(f(writer));
    });
    match rx.recv_timeout(LONG) {
        Ok(out) => {
            thread.join().unwrap();
            out
        }
        Err(_) => {
            cleanup();
            panic!("the writer waited for its held prefault helper");
        }
    }
}

/// With the helper held while it releases one segment and a second one waiting, the writer releases every further
/// segment itself, without waiting: its descriptors and mappings stay where they are over many rolls. Once the
/// helper goes on, it releases the two it had, and nothing else is left open.
#[test]
fn a_held_helper_bounds_what_waits_and_the_writer_releases_the_rest() {
    if !hands_off("a_held_helper_bounds_what_waits_and_the_writer_releases_the_rest") {
        return;
    }
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let mut writer = Writer::open(&journal, config()).unwrap();
    roll(&mut writer, OPEN_SEGMENTS);
    settle(&writer);
    let hold = fault::hold_background(Point::SegmentRelease);
    roll(&mut writer, 1);
    assert!(hold.wait_reached(LONG), "the helper took no segment");
    assert_eq!(writer.segments_awaiting_release(), 1);
    roll(&mut writer, 1);
    assert_eq!(writer.segments_awaiting_release(), RELEASE_QUEUE);
    let (fds, maps) = (descriptors(), mappings(&journal));
    let hold = std::sync::Arc::new(Mutex::new(Some(hold)));
    let release = std::sync::Arc::clone(&hold);
    let (writer, after) = within(
        writer,
        move || drop(release.lock().unwrap().take()),
        move |mut writer| {
            let mut after = Vec::new();
            for _ in 0..20 {
                roll(&mut writer, 1);
                after.push((
                    writer.segments_awaiting_release(),
                    descriptors(),
                    mappings(&journal),
                ));
            }
            (writer, after)
        },
    );
    // The hold goes before anything is checked: a failed check would otherwise drop the writer, which waits for the
    // held helper, and hang.
    drop(hold.lock().unwrap().take());
    for (waiting, fds_now, maps_now) in after {
        assert_eq!(waiting, RELEASE_QUEUE, "segments waiting for the helper");
        assert_eq!(fds_now, fds, "descriptors over rolls with the helper held");
        // The held helper also keeps the mapping of the segment it was prefaulting when it took the first one (it
        // advises a new segment's window before it releases), once every list has let go of that segment: one more,
        // which is not a descriptor (ADR 0054).
        assert!(
            maps_now <= maps + 1,
            "journal mappings over rolls with the helper held: {maps_now} against {maps}"
        );
    }
    settle(&writer);
    assert_eq!(
        descriptors(),
        fds - RELEASE_QUEUE,
        "the held segments were not closed"
    );
    if cfg!(target_os = "linux") {
        // Once it goes on, the helper also lets go of the mapping it was advising when it was held.
        let journal = dir.path().join("j");
        wait_for(&writer, "the journal's mappings to go down", |_| {
            mappings(&journal) == maps - RELEASE_QUEUE
        });
    }
}

/// Dropping the writer while segments wait for the held helper: the drop stops and joins the helper once it goes on,
/// and every descriptor and mapping of the journal is gone afterwards. A descriptor opened before the writer is still
/// open, so nothing was closed twice.
#[test]
fn dropping_the_writer_with_segments_waiting_releases_them_once() {
    if !hands_off("dropping_the_writer_with_segments_waiting_releases_them_once") {
        return;
    }
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let sentinel = std::fs::File::create(dir.path().join("sentinel")).unwrap();
    let fds = descriptors();
    let mut writer = Writer::open(&journal, config()).unwrap();
    roll(&mut writer, OPEN_SEGMENTS);
    settle(&writer);
    let hold = fault::hold_background(Point::SegmentRelease);
    roll(&mut writer, 2);
    assert!(hold.wait_reached(LONG), "the helper took no segment");
    assert_eq!(writer.segments_awaiting_release(), RELEASE_QUEUE);
    let dropping = std::thread::spawn(move || drop(writer));
    // The drop waits for the held helper; it may still be waiting when the hold goes, or not have started yet.
    std::thread::sleep(Duration::from_millis(20));
    drop(hold);
    dropping.join().unwrap();
    assert_eq!(descriptors(), fds, "descriptors left after the drop");
    assert_eq!(
        mappings(&journal),
        0,
        "journal mappings left after the drop"
    );
    sentinel
        .metadata()
        .expect("a descriptor the writer never had was closed");
}

/// A helper that panics while it releases a segment ends: the writer does not wait for it, releases every later
/// segment itself (none waits for the helper any more), and its drop returns. Nothing is left open.
#[test]
fn a_panicked_helper_is_not_waited_for() {
    if !hands_off("a_panicked_helper_is_not_waited_for") {
        return;
    }
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let fds = descriptors();
    let mut writer = Writer::open(&journal, config()).unwrap();
    roll(&mut writer, OPEN_SEGMENTS);
    settle(&writer);
    fault::fail_next_background(Point::SegmentReleasePanic, 1);
    roll(&mut writer, 1);
    // The panic drops the segment in hand, and the helper drops whatever else waits for it as it ends.
    settle(&writer);
    let writer = within(
        writer,
        || {},
        |mut writer| {
            for _ in 0..20 {
                roll(&mut writer, 1);
                assert_eq!(
                    writer.segments_awaiting_release(),
                    0,
                    "a segment was handed to the ended helper"
                );
            }
            writer
        },
    );
    let fds_open = descriptors();
    within(writer, || {}, drop);
    assert!(fds_open > fds);
    assert_eq!(descriptors(), fds, "descriptors left after the drop");
    assert_eq!(
        mappings(&journal),
        0,
        "journal mappings left after the drop"
    );
}

/// Without helper threads (`prefault(false)`), and on macOS with them, nothing is handed over: the writer releases
/// segments itself, as before.
#[test]
fn without_a_hand_off_the_writer_releases_segments_itself() {
    let _serial = serial();
    let mut configs = vec![config().prefault(false)];
    if !cfg!(target_os = "linux") {
        configs.push(config());
    }
    for config in configs {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("j");
        let mut writer = Writer::open(&journal, config).unwrap();
        roll(&mut writer, OPEN_SEGMENTS);
        let fds = descriptors();
        for _ in 0..10 {
            roll(&mut writer, 1);
            assert_eq!(writer.segments_awaiting_release(), 0);
            assert_eq!(descriptors(), fds);
        }
    }
}

/// The child of [`rolls_release_segments_off_the_writers_thread`]: rolls between two marks, each roll followed by a
/// wait until the helper has released what the roll handed it (so it keeps up). With `TT_SEGMENT_RELEASE_CHILD` unset,
/// it does nothing. `prefault(false)` in the `off` journal: the writer's thread releases the segments.
#[test]
fn child_that_rolls() {
    let Ok(dir) = std::env::var("TT_SEGMENT_RELEASE_CHILD") else {
        return;
    };
    for helpers in ["on", "off"] {
        let config = config().prefault(helpers == "on");
        let mut writer = Writer::open(Path::new(&dir).join(helpers), config).unwrap();
        roll(&mut writer, OPEN_SEGMENTS);
        settle(&writer);
        let _ = std::fs::metadata(format!("/tickertape-mark-begin-{helpers}"));
        for _ in 0..20 {
            roll(&mut writer, 1);
            settle(&writer);
        }
        let _ = std::fs::metadata(format!("/tickertape-mark-end-{helpers}"));
    }
}

/// Twenty rolls that each release a segment make no `munmap` and no `close` on the writer's thread when the prefault
/// helper keeps up; the helper's thread unmaps a segment for each of them. Without helper threads the writer's thread
/// does both, as before. Needs `strace`: skipped without it, unless `TICKERTAPE_REQUIRE_STRACE` is set (as in CI).
#[cfg(target_os = "linux")]
#[test]
fn rolls_release_segments_off_the_writers_thread() {
    use std::io::Write;
    let _serial = serial();
    let skip = |why: String| {
        assert!(
            std::env::var_os("TICKERTAPE_REQUIRE_STRACE").is_none(),
            "{why}"
        );
        // Not `eprintln!`, which the test harness captures: a skip must be seen.
        let _ = writeln!(std::io::stderr(), "skipped: {why}");
    };
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("trace");
    let strace = std::process::Command::new("strace")
        .args([
            "-f",
            "-y",
            "-e",
            "trace=munmap,close,newfstatat,statx,stat",
            "-o",
        ])
        .arg(&trace)
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "child_that_rolls", "--test-threads", "1"])
        .env("TT_SEGMENT_RELEASE_CHILD", dir.path())
        .stderr(std::process::Stdio::null())
        .output();
    // Skipped only when `strace` cannot run: it cannot be spawned, or the child test never reported a result (no
    // ptrace permission, say). A child that ran and failed fails this test.
    let out = match strace {
        Err(e) => return skip(format!("strace: {e}")),
        Ok(out) => out,
    };
    let child = String::from_utf8_lossy(&out.stdout);
    if !child.contains("test result:") {
        return skip(format!("strace ran no test: {:?}", out.status));
    }
    assert!(out.status.success(), "the traced child failed:\n{child}");
    let trace = std::fs::read_to_string(&trace).unwrap();
    let segment = format!(", {}) ", small_config().segment_size);
    for helpers in ["on", "off"] {
        let mark = |what: &str| {
            let name = format!("/tickertape-mark-{what}-{helpers}\"");
            let at = trace.lines().position(|line| line.contains(&name));
            at.unwrap_or_else(|| panic!("{name} is not in the trace"))
        };
        let (begin, end) = (mark("begin"), mark("end"));
        assert!(begin < end);
        // `-f` starts each line with the thread's id.
        let thread = |line: &str| line.split_whitespace().next().map(str::to_owned);
        let rolling = thread(trace.lines().nth(begin).unwrap());
        let between: Vec<&str> = trace
            .lines()
            .skip(begin + 1)
            .take(end - begin - 1)
            .filter(|line| !line.contains("--- SIG"))
            .collect();
        let (mine, others): (Vec<&str>, Vec<&str>) =
            between.iter().partition(|line| thread(line) == rolling);
        // A segment's `munmap` has its length; `-y` names the file a `close` closes (by the temporary name the
        // segment was created under, `….tape.tmp (deleted)`, or its own).
        let unmapped = |lines: &[&str]| {
            lines
                .iter()
                .filter(|l| l.contains("munmap(") && l.contains(&segment))
                .count()
        };
        let closed = |lines: &[&str]| {
            lines
                .iter()
                .filter(|l| l.contains("close(") && l.contains(".tape"))
                .count()
        };
        if helpers == "on" {
            assert_eq!(
                (unmapped(&mine), closed(&mine)),
                (0, 0),
                "the writer's thread released segments: {mine:?}"
            );
            assert!(
                unmapped(&others) >= 20 && closed(&others) >= 20,
                "the helper released {} mappings and {} files in 20 rolls: {others:?}",
                unmapped(&others),
                closed(&others)
            );
        } else {
            assert!(
                unmapped(&mine) >= 20 && closed(&mine) >= 20,
                "without helpers: {mine:?}"
            );
        }
    }
}
