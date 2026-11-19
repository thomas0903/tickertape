//! Operational status from another process (ADR 0032): a writer and readers in other processes, an observer
//! sampling while they run, and readers that die.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use tickertape::status::{self, BeaconState, Breach, HeadEnd, Sampler, Thresholds};
use tickertape::{Error, Writer};
use tickertape_tests::small_config;

const WRITER: &str = env!("CARGO_BIN_EXE_tt-writer");
const BEACON_READER: &str = env!("CARGO_BIN_EXE_tt-beacon-reader");

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
    // tt-writer checkpoints only at the end.
    assert_eq!(s.checkpoint, None);
    assert_eq!(s.exposure.records, half);
    assert!(s.exposure.oldest_local_ts_ns.is_some());

    let r = reader(&s, "caught-up");
    assert_eq!(r.state, BeaconState::Live);
    assert_eq!(r.pid, Some(caught_up.0.id()));
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

    // Sample continuously while the writer finishes: never an error, the head only moves forward.
    std::fs::write(&go, b"").unwrap();
    let mut sampler = Sampler::new(&journal);
    let mut last = 0;
    let mut samples = 0;
    loop {
        let s = sampler.sample().unwrap();
        assert!(s.head.position.next_seq >= last, "head went backwards");
        assert_eq!(
            s.head.end,
            HeadEnd::Open,
            "a sealed head is followed into its successor"
        );
        last = s.head.position.next_seq;
        samples += 1;
        if last == count && s.checkpoint.is_some() {
            break;
        }
        assert!(samples < 1_000_000, "the writer never finished");
    }
    assert!(writer.0.wait().unwrap().success());
    let s = sampler.sample().unwrap();
    assert_eq!(s.exposure.records, 0);
    assert_eq!(s.checkpoint.unwrap().marker.next_seq, count);
    assert_eq!(s.writer.alive, Some(false));
    assert!(s.rates.is_some());

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

/// The observer never touches the writer lock: a writer opening while status runs in a loop is never refused, and
/// status creates nothing in the journal.
#[test]
fn status_never_interferes_with_the_writer() {
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
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
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
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    assert_eq!(names(dir.path()), before);
}
