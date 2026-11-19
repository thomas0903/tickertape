//! Operational status from another process (ADR 0032): a writer and readers in other processes, an observer
//! sampling while they run, readers that die, and inputs that must not hang or fail the observer.
//!
//! Every test here takes `SERIAL`: a spawned child briefly holds copies of all of this process's descriptors
//! (between `fork` and `exec`), including other tests' locks, so a test that releases a lock and expects it free
//! must not run while another test spawns.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tickertape::status::{self, BeaconState, Breach, HeadEnd, Sampler, Thresholds};
use tickertape::{Error, Writer};
use tickertape_tests::{append_n, small_config};

const WRITER: &str = env!("CARGO_BIN_EXE_tt-writer");
const BEACON_READER: &str = env!("CARGO_BIN_EXE_tt-beacon-reader");

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Kills a child process on drop, so a failing test doesn't leave holders behind.
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_reader(journal: &Path, name: &str, count: u64, then: &str, ready: &Path) -> Guard {
    Guard(
        Command::new(BEACON_READER)
            .args([
                journal.to_str().unwrap(),
                name,
                &count.to_string(),
                then,
                ready.to_str().unwrap(),
            ])
            .spawn()
            .expect("spawn beacon reader"),
    )
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn reader<'a>(s: &'a status::Status, name: &str) -> &'a status::ReaderStatus {
    s.readers
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("no beacon {name} in {:?}", s.readers))
}

#[test]
fn observer_sees_writer_and_readers_in_other_processes() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let journal = tmp.path().join("journal");
    let go = tmp.path().join("go");
    let ready = |name: &str| -> PathBuf { tmp.path().join(format!("{name}.ready")) };
    let (count, half) = (20_000u64, 10_000u64);

    let mut writer = Guard(
        Command::new(WRITER)
            .args([
                journal.to_str().unwrap(),
                &count.to_string(),
                &(64 * 1024).to_string(),
                "64",
                &half.to_string(),
                go.to_str().unwrap(),
            ])
            .spawn()
            .expect("spawn writer"),
    );
    let mut caught_up = spawn_reader(&journal, "caught-up", half, "hold", &ready("caught-up"));
    let mut lagging = spawn_reader(&journal, "lagging", 3_000, "hold", &ready("lagging"));
    let mut dead = spawn_reader(&journal, "dead", 5_000, "abort", &ready("dead"));
    for name in ["caught-up", "lagging", "dead"] {
        wait_until(name, || ready(name).exists());
    }
    let dead_status = dead.0.wait().unwrap();
    assert!(!dead_status.success(), "the dead reader aborts");

    // The writer is paused at `half`.
    wait_until("the writer to reach the pause", || {
        status::status(&journal).is_ok_and(|s| s.head.position.next_seq == half)
    });
    let s = status::status(&journal).unwrap();
    assert_eq!(s.head.end, HeadEnd::Open);
    assert!(
        s.segments.count >= 3,
        "several segments: {}",
        s.segments.count
    );
    assert_eq!(s.writer.pid, Some(writer.0.id()));
    assert_eq!(s.writer.alive, Some(true));
    // tt-writer checkpoints only at the end, so with several segments the chain isn't verified by one sample.
    assert_eq!(s.checkpoint, None);
    assert!(!s.head.verified);
    assert_eq!(s.exposure.records, Some(half));
    assert!(s.exposure.oldest_local_ts_ns.is_some());

    let r = reader(&s, "caught-up");
    assert_eq!(r.state, BeaconState::Live);
    assert_eq!(r.report.unwrap().pid, caught_up.0.id());
    assert_eq!(r.lag.unwrap().records, 0);
    assert_eq!(r.lag.unwrap().bytes, Some(0));
    assert_eq!(r.unread_age, Some(Duration::ZERO));
    let r = reader(&s, "lagging");
    assert_eq!(r.state, BeaconState::Live);
    assert_eq!(r.lag.unwrap().records, half - 3_000);
    assert!(r.lag.unwrap().bytes.unwrap() > 0);
    assert!(r.oldest_unread_local_ts_ns.is_some());
    assert!(r.unread_age.is_some());
    // The dead reader is stale: its position is kept, but it never counts as lag.
    let r = reader(&s, "dead");
    assert_eq!(r.state, BeaconState::Stale);
    assert_eq!(r.lag.unwrap().records, half - 5_000);
    assert_eq!(
        s.breaches(&Thresholds::default().max_reader_lag(1_000)),
        vec![Breach::ReaderLag {
            name: "lagging".into()
        }]
    );

    // Sample continuously while the writer finishes: never an error, the head only moves forward, and a head is
    // reported sealed only if its successor doesn't exist (tt-writer always creates it before sealing).
    std::fs::write(&go, b"").unwrap();
    let mut sampler = Sampler::new(&journal);
    let mut last = 0;
    let mut samples = 0;
    loop {
        let s = sampler.sample().unwrap();
        assert!(s.head.position.next_seq >= last, "head went backwards");
        assert_eq!(s.head.end, HeadEnd::Open, "a sealed head with a successor");
        assert!(s.readers.iter().all(|r| r.report.is_some()));
        last = s.head.position.next_seq;
        samples += 1;
        if last == count && s.checkpoint.is_some() {
            break;
        }
        assert!(samples < 1_000_000, "the writer never finished");
    }
    assert!(writer.0.wait().unwrap().success());
    let s = sampler.sample().unwrap();
    assert_eq!(s.exposure.records, Some(0));
    assert!(s.head.verified, "the final checkpoint is at the head");
    assert_eq!(s.checkpoint.unwrap().marker.next_seq, count);
    assert_eq!(s.writer.alive, Some(false));

    // Killed readers leave their files, but the kernel released their locks.
    caught_up.0.kill().unwrap();
    caught_up.0.wait().unwrap();
    lagging.0.kill().unwrap();
    lagging.0.wait().unwrap();
    let s = status::status(&journal).unwrap();
    for name in ["caught-up", "lagging", "dead"] {
        assert_eq!(reader(&s, name).state, BeaconState::Stale, "{name}");
    }
    assert!(
        s.breaches(&Thresholds::default().max_reader_lag(0))
            .is_empty()
    );
}

/// Sets a flag when dropped, so a failing assertion can't leave a looping thread running (and the test hanging).
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// The observer never touches the writer lock: a writer opening while status runs in a loop is never refused, and
/// status creates nothing in the journal.
#[test]
fn status_never_interferes_with_the_writer() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    drop(Writer::open(dir.path(), small_config()).unwrap());
    let names = |dir: &Path| {
        let mut v: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        v.sort();
        v
    };
    let before = names(dir.path());
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let _stop = StopOnDrop(&stop);
        scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                status::status(dir.path()).unwrap();
            }
        });
        for _ in 0..200 {
            match Writer::open(dir.path(), small_config()) {
                Ok(w) => drop(w),
                Err(Error::Locked { .. }) => panic!("status made a writer's open fail"),
                Err(e) => panic!("open: {e}"),
            }
        }
    });
    assert_eq!(names(dir.path()), before);
}

/// Round 2, B1: samples taken while the writer checkpoints and retention removes old segments never report a healthy
/// journal as corrupt. Retention (here: keep only the checkpoint's segment and newer) removes only segments before
/// the checkpoint's, so a sample whose listing and marker disagree, or whose checkpoint segment vanishes after the
/// listing, must re-read them instead of reporting a dangling marker. Runs for `TT_STRESS_MS` (default 1500 ms);
/// races are most frequent where syncs are cheap (tmpfs: `TMPDIR=/dev/shm`).
#[test]
fn samples_during_retention_are_never_corrupt() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let config = small_config().retention(
        tickertape::retention::Policy::default().max_segments(1),
        tickertape::retention::Action::Delete,
    );
    let mut writer = Writer::open(dir.path(), config).unwrap();
    let run = Duration::from_millis(
        std::env::var("TT_STRESS_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1500),
    );
    let stop = AtomicBool::new(false);
    let samples = std::sync::atomic::AtomicU64::new(0);
    let errors = std::sync::atomic::AtomicU64::new(0);
    let check = |sampled: tickertape::Result<status::Status>| match sampled {
        Ok(s) => {
            let breaches = s.breaches(&Thresholds::default());
            assert!(
                breaches.is_empty(),
                "a healthy journal sampled as {breaches:?}: {s:?}"
            );
            samples.fetch_add(1, Ordering::Relaxed);
        }
        // The journal changed faster than a sample could follow (exit 1, not a corruption verdict).
        Err(Error::Io { .. }) => {
            errors.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => panic!("sample failed: {e}"),
    };
    let segments = std::thread::scope(|scope| {
        let _stop = StopOnDrop(&stop);
        scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                check(status::status(dir.path()));
            }
        });
        scope.spawn(|| {
            let mut sampler = Sampler::new(dir.path());
            while !stop.load(Ordering::Relaxed) {
                check(sampler.sample());
            }
        });
        // Records of a third of a segment: every checkpoint follows a roll, so the marker leaves its segment, which
        // retention then removes, as often as possible.
        let payload = vec![7u8; 20_000];
        let started = Instant::now();
        let mut rolled = 0u64;
        while started.elapsed() < run {
            let before = writer.metrics().head.segment_base_seq;
            for _ in 0..3 {
                writer
                    .append(&tickertape::NewRecord {
                        schema_id: 1,
                        venue_ts_ns: 0,
                        local_ts_ns: 0,
                        payload: &payload,
                    })
                    .unwrap();
            }
            writer.checkpoint().unwrap();
            rolled += u64::from(writer.metrics().head.segment_base_seq != before);
        }
        assert_eq!(
            writer.metrics().retention_failures,
            0,
            "{:?}",
            writer.last_retention_error()
        );
        rolled
    });
    let (samples, errors) = (samples.into_inner(), errors.into_inner());
    eprintln!("{samples} samples, {errors} I/O errors, {segments} rolls");
    assert!(segments > 10, "retention had little to do");
    assert!(samples > 100);
    assert!(
        errors * 20 <= samples,
        "{errors} of {samples} samples failed"
    );
}

/// The PID in `writer.lock` of a process that no longer exists (moved here from the unit tests, which must not
/// spawn processes).
#[test]
fn writer_not_running() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 3);
    writer.checkpoint().unwrap();
    drop(writer);
    // A PID that certainly doesn't exist any more: a child that has been reaped.
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    std::fs::write(dir.path().join("writer.lock"), pid.to_string()).unwrap();
    let s = status::status(dir.path()).unwrap();
    assert_eq!(s.writer.pid, Some(pid));
    assert_eq!(s.writer.alive, Some(false));
    assert_eq!(s.head.position.next_seq, 3);
    std::fs::remove_file(dir.path().join("writer.lock")).unwrap();
    let s = status::status(dir.path()).unwrap();
    assert_eq!((s.writer.pid, s.writer.alive), (None, None));
}

fn mkfifo(path: &Path) {
    assert!(Command::new("mkfifo").arg(path).status().unwrap().success());
}

/// Runs `f` on a thread and fails if it doesn't return within 30 s.
fn within_deadline<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(30))
        .expect("the observer hung")
}

/// Finding 3: a FIFO in place of a journal file or a beacon never blocks the observer.
#[test]
fn fifos_never_block_the_observer() {
    let _serial = serial();
    // In place of the checkpoint marker: an I/O error, not a hang.
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 3);
    drop(writer);
    std::fs::remove_file(dir.path().join("checkpoint")).unwrap();
    mkfifo(&dir.path().join("checkpoint"));
    let path = dir.path().to_path_buf();
    let result = within_deadline(move || status::status(&path).map(|_| ()));
    assert!(matches!(result, Err(Error::Io { .. })), "{result:?}");

    // In place of writer.lock: ignored (informational).
    std::fs::remove_file(dir.path().join("checkpoint")).unwrap();
    std::fs::remove_file(dir.path().join("writer.lock")).unwrap();
    mkfifo(&dir.path().join("writer.lock"));
    // In the beacon directory: skipped.
    std::fs::create_dir(dir.path().join("readers")).unwrap();
    mkfifo(&dir.path().join("readers").join("fifo"));
    let path = dir.path().to_path_buf();
    let s = within_deadline(move || status::status(&path).unwrap());
    assert_eq!(s.writer.pid, None);
    assert!(s.readers.is_empty());
    assert_eq!(s.head.position.next_seq, 3);

    // In place of a segment: an I/O error, not a hang.
    mkfifo(&dir.path().join("00000000000000000099.tape"));
    let path = dir.path().to_path_buf();
    let result = within_deadline(move || status::status(&path).map(|_| ()));
    assert!(matches!(result, Err(Error::Io { .. })), "{result:?}");
}

/// Finding 7: a beacon is created with the creating process's umask, not a forced mode.
#[test]
fn beacons_respect_the_umask() {
    use std::os::unix::fs::PermissionsExt;
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let journal = tmp.path().join("journal");
    let mut writer = Writer::open(&journal, small_config()).unwrap();
    append_n(&mut writer, 3);
    let ready = tmp.path().join("ready");
    // The umask is set in a shell in front of the reader, never in this (multi-threaded) process.
    let _reader = Guard(
        Command::new("sh")
            .args([
                "-c",
                "umask 077 && exec \"$0\" \"$@\"",
                BEACON_READER,
                journal.to_str().unwrap(),
                "private",
                "3",
                "hold",
                ready.to_str().unwrap(),
            ])
            .spawn()
            .unwrap(),
    );
    wait_until("the reader", || ready.exists());
    let mode = std::fs::metadata(journal.join("readers").join("private"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let s = status::status(&journal).unwrap();
    assert_eq!(reader(&s, "private").state, BeaconState::Live);
}
