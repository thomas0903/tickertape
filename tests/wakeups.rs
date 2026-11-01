//! ADR 0017: reader wake-ups through the notify file.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use tickertape::{Backoff, NewRecord, Reader, ReaderConfig, Writer};
use tickertape_tests::{Rng, small_config};

/// A back-off that skips spinning and yielding, so every wait goes straight to blocking (on the notify file, or
/// sleeping `sleep` when wake-ups are unavailable).
fn blocking(sleep: Duration) -> Backoff {
    let mut b = Backoff::default();
    b.spins = 0;
    b.yields = 0;
    b.min_sleep = sleep;
    b.max_sleep = sleep;
    b
}

fn append(w: &mut Writer, seq: u64) {
    w.append(&NewRecord {
        schema_id: 1,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload: &seq.to_le_bytes(),
    })
    .unwrap();
}

fn notify_bytes(dir: &std::path::Path) -> Vec<u8> {
    std::fs::read(dir.join("notify")).unwrap()
}

#[test]
fn writer_creates_notify_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    let bytes = notify_bytes(dir.path());
    assert_eq!(bytes.len(), 4096);
    assert_eq!(
        u32::from_le_bytes(bytes[128..132].try_into().unwrap()),
        1,
        "writer flag"
    );
    append(&mut w, 0);
    append(&mut w, 1);
    let bytes = notify_bytes(dir.path());
    assert_eq!(
        u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
        2,
        "event counter bumped per append"
    );
    drop(w);
    // A writer with wake-ups disabled clears the flag so readers don't block long waiting for bumps.
    let _w = tickertape_tests::reopen_writer(dir.path(), small_config().notify(false));
    assert_eq!(
        u32::from_le_bytes(notify_bytes(dir.path())[128..132].try_into().unwrap()),
        0
    );
}

#[test]
fn no_lost_wake_ups_under_stress() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    let path = dir.path().to_path_buf();
    let n = 2_000u64;
    let reader = std::thread::spawn(move || {
        // A 10 s poll interval: if a wake-up were lost, the test would take minutes or time out.
        let mut r = Reader::open(
            &path,
            ReaderConfig::default().backoff(blocking(Duration::from_secs(10))),
        )
        .unwrap();
        for expected in 0..n {
            let rec = r
                .next_timeout(Some(Duration::from_secs(30)))
                .unwrap()
                .unwrap_or_else(|| panic!("record {expected} never arrived"));
            assert_eq!(rec.seq, expected);
        }
    });
    let mut rng = Rng::from_env("no_lost_wake_ups_under_stress");
    let started = Instant::now();
    for seq in 0..n {
        let pause = rng.below(300);
        if pause > 0 {
            std::thread::sleep(Duration::from_micros(pause));
        }
        append(&mut w, seq);
    }
    reader.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "took {:?}",
        started.elapsed()
    );
    let bytes = notify_bytes(dir.path());
    assert_eq!(
        u32::from_le_bytes(bytes[64..68].try_into().unwrap()),
        0,
        "waiter count back to zero"
    );
}

/// Median delay between an append and the blocked reader returning it, over `trials` appends.
fn median_delay(config: ReaderConfig, trials: usize) -> Duration {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    let path = dir.path().to_path_buf();
    let (tx, rx) = mpsc::channel::<Instant>();
    let reader = std::thread::spawn(move || {
        let mut r = Reader::open(&path, config).unwrap();
        let mut delays = Vec::new();
        for _ in 0..trials {
            let rec = r
                .next_timeout(Some(Duration::from_secs(30)))
                .unwrap()
                .expect("record");
            let got = Instant::now();
            let sent: Instant = rx.recv().unwrap();
            let _ = rec;
            delays.push(got.saturating_duration_since(sent));
        }
        delays.sort();
        delays[delays.len() / 2]
    });
    for seq in 0..trials as u64 {
        std::thread::sleep(Duration::from_millis(20)); // let the reader block
        let sent = Instant::now();
        append(&mut w, seq);
        tx.send(sent).unwrap();
    }
    reader.join().unwrap()
}

#[test]
fn blocked_reader_wakes_up_promptly() {
    // With a 400 ms poll interval, a polling reader would see each record ~200 ms late on average; with wake-ups the
    // median delay must be far below that (bound kept generous for slow CI machines).
    let woken = median_delay(
        ReaderConfig::default().backoff(blocking(Duration::from_millis(400))),
        15,
    );
    eprintln!("median wake-up delay {woken:?}");
    assert!(
        woken < Duration::from_millis(20),
        "median wake-up delay {woken:?}"
    );
    let polling = median_delay(
        ReaderConfig::default()
            .notify(false)
            .backoff(blocking(Duration::from_millis(400))),
        5,
    );
    assert!(polling > woken, "polling {polling:?} vs woken {woken:?}");
}

#[test]
fn read_only_notify_file_falls_back_to_polling() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    std::fs::set_permissions(
        dir.path().join("notify"),
        std::fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    let mut r = Reader::open(
        dir.path(),
        ReaderConfig::default().backoff(blocking(Duration::from_millis(5))),
    )
    .unwrap();
    append(&mut w, 0);
    assert_eq!(
        r.next_timeout(Some(Duration::from_secs(5)))
            .unwrap()
            .unwrap()
            .seq,
        0
    );
    assert!(
        r.next_timeout(Some(Duration::from_millis(30)))
            .unwrap()
            .is_none()
    );
}
