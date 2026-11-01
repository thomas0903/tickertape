//! ADRs 0017 and 0021: reader wake-ups through a shared region named by the journal's `notify` record.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use tickertape::{Backoff, NewRecord, Reader, ReaderConfig, Writer};
use tickertape_tests::{Rng, small_config};

/// A back-off that skips spinning and yielding, so every wait goes straight to blocking (on the wake-up region, or
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

#[test]
fn writer_publishes_a_notify_record() {
    let dir = tempfile::tempdir().unwrap();
    let w = Writer::open(dir.path(), small_config()).unwrap();
    let bytes = std::fs::read(dir.path().join("notify")).unwrap();
    assert_eq!(bytes.len(), 80);
    assert_eq!(&bytes[..8], b"TTNOTIFY");
    drop(w);
    // A writer with wake-ups disabled removes the record, so readers poll.
    let _w = tickertape_tests::reopen_writer(dir.path(), small_config().notify(false));
    assert!(!dir.path().join("notify").exists());
}

/// Delivery delays when the writer appends one record at a time and waits for the reader to take it, with a short
/// random pause in between so that the reader is usually blocked when the record arrives.
fn ping_pong(
    dir: &std::path::Path,
    config: ReaderConfig,
    rounds: u64,
    seed_name: &str,
) -> Vec<Duration> {
    let mut w = tickertape_tests::reopen_writer(dir, small_config());
    let first = w.next_seq();
    let path = dir.to_path_buf();
    let (sent_tx, sent_rx) = mpsc::channel::<Instant>();
    let (ack_tx, ack_rx) = mpsc::channel::<()>();
    let reader = std::thread::spawn(move || {
        let mut r = Reader::open(&path, config).unwrap();
        r.seek(first).unwrap();
        let mut delays = Vec::new();
        for expected in first..first + rounds {
            let rec = r
                .next_timeout(Some(Duration::from_secs(30)))
                .unwrap()
                .unwrap_or_else(|| panic!("record {expected} never arrived"));
            let got = Instant::now();
            assert_eq!(rec.seq, expected);
            delays.push(got.saturating_duration_since(sent_rx.recv().unwrap()));
            ack_tx.send(()).unwrap();
        }
        delays
    });
    let mut rng = Rng::from_env(seed_name);
    for seq in first..first + rounds {
        std::thread::sleep(Duration::from_micros(rng.below(2_000)));
        let sent = Instant::now();
        append(&mut w, seq);
        sent_tx.send(sent).unwrap();
        ack_rx.recv().unwrap();
    }
    reader.join().unwrap()
}

#[test]
fn no_lost_wake_ups() {
    // Each wait is capped at 100 ms, so a lost wake-up shows up as a delivery that takes about that long. The poll
    // interval is 10 s, so polling can't hide one either.
    let dir = tempfile::tempdir().unwrap();
    let delays = ping_pong(
        dir.path(),
        ReaderConfig::default().backoff(blocking(Duration::from_secs(10))),
        300,
        "no_lost_wake_ups",
    );
    let slow = delays
        .iter()
        .filter(|d| **d >= Duration::from_millis(90))
        .count();
    // Allow two scheduling hiccups on a loaded CI machine; a broken wake-up path makes nearly all 300 slow.
    assert!(
        slow <= 2,
        "{slow} of {} deliveries took >= 90 ms",
        delays.len()
    );
}

#[test]
fn blocked_reader_wakes_up_promptly() {
    // With a 400 ms poll interval, a polling reader would see each record ~200 ms late on average; with wake-ups the
    // median delay must be far below that (bound kept generous for slow CI machines).
    let median = |mut d: Vec<Duration>| {
        d.sort();
        d[d.len() / 2]
    };
    let dir = tempfile::tempdir().unwrap();
    let woken = median(ping_pong(
        dir.path(),
        ReaderConfig::default().backoff(blocking(Duration::from_millis(400))),
        15,
        "blocked_reader_wakes_up_promptly",
    ));
    eprintln!("median wake-up delay {woken:?}");
    assert!(
        woken < Duration::from_millis(20),
        "median wake-up delay {woken:?}"
    );
    let polling = median(ping_pong(
        dir.path(),
        ReaderConfig::default()
            .notify(false)
            .backoff(blocking(Duration::from_millis(400))),
        5,
        "blocked_reader_wakes_up_promptly_polling",
    ));
    assert!(polling > woken, "polling {polling:?} vs woken {woken:?}");
}

#[test]
fn reader_started_before_the_writer_gets_wake_ups() {
    let dir = tempfile::tempdir().unwrap();
    drop(Writer::open(dir.path(), small_config()).unwrap());
    // No writer runs, so the record names a region that's gone: the reader starts without one.
    let config = ReaderConfig::default().backoff(blocking(Duration::from_millis(400)));
    let path = dir.path().to_path_buf();
    let (sent_tx, sent_rx) = mpsc::channel::<Instant>();
    let reader = std::thread::spawn(move || {
        let mut r = Reader::open(&path, config).unwrap();
        // The first wait re-reads the record within 100 ms and attaches to the new writer's region.
        assert!(
            r.next_timeout(Some(Duration::from_millis(250)))
                .unwrap()
                .is_none()
        );
        let mut delays = Vec::new();
        for _ in 0..10 {
            r.next_timeout(Some(Duration::from_secs(30)))
                .unwrap()
                .expect("record");
            delays.push(Instant::now().saturating_duration_since(sent_rx.recv().unwrap()));
        }
        delays.sort();
        delays[delays.len() / 2]
    });
    std::thread::sleep(Duration::from_millis(50));
    let mut w = tickertape_tests::reopen_writer(dir.path(), small_config());
    std::thread::sleep(Duration::from_millis(350));
    for seq in 0..10 {
        std::thread::sleep(Duration::from_millis(20));
        let sent = Instant::now();
        append(&mut w, seq);
        sent_tx.send(sent).unwrap();
    }
    let median = reader.join().unwrap();
    assert!(
        median < Duration::from_millis(20),
        "median delay {median:?}"
    );
}

#[test]
fn damaging_the_record_never_hurts_a_waiting_reader() {
    // ADR 0021: readers only `pread` the record, and the region it names can't shrink, so truncating or
    // overwriting the file in the journal directory can't crash a reader (it used to, with SIGBUS). The reader falls
    // back to polling.
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    let path = dir.path().to_path_buf();
    let reader = std::thread::spawn(move || {
        let mut r = Reader::open(
            &path,
            ReaderConfig::default().backoff(blocking(Duration::from_millis(20))),
        )
        .unwrap();
        for expected in 0..3 {
            let rec = r
                .next_timeout(Some(Duration::from_secs(10)))
                .unwrap()
                .expect("record");
            assert_eq!(rec.seq, expected);
        }
    });
    std::thread::sleep(Duration::from_millis(50));
    let notify = dir.path().join("notify");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&notify)
        .unwrap()
        .set_len(0)
        .unwrap();
    append(&mut w, 0);
    std::thread::sleep(Duration::from_millis(150));
    std::fs::write(&notify, [0xffu8; 64]).unwrap();
    append(&mut w, 1);
    std::thread::sleep(Duration::from_millis(150));
    std::fs::remove_file(&notify).unwrap();
    append(&mut w, 2);
    reader.join().unwrap();
}

#[test]
fn deadline_below_the_cap_is_honoured() {
    let dir = tempfile::tempdir().unwrap();
    let _w = Writer::open(dir.path(), small_config()).unwrap();
    let mut r = Reader::open(
        dir.path(),
        ReaderConfig::default().backoff(blocking(Duration::from_secs(10))),
    )
    .unwrap();
    for _ in 0..5 {
        let started = Instant::now();
        assert!(
            r.next_timeout(Some(Duration::from_millis(30)))
                .unwrap()
                .is_none()
        );
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(30),
            "returned early: {took:?}"
        );
        assert!(took < Duration::from_millis(90), "overslept: {took:?}");
    }
}

#[test]
fn a_fifo_in_place_of_the_record_never_hangs() {
    let dir = tempfile::tempdir().unwrap();
    drop(Writer::open(dir.path(), small_config()).unwrap());
    let notify = dir.path().join("notify");
    std::fs::remove_file(&notify).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&notify)
            .status()
            .unwrap()
            .success()
    );
    // Neither the reader's refresh nor the writer's clean-up may block on opening the FIFO.
    let mut r = Reader::open(
        dir.path(),
        ReaderConfig::default().backoff(blocking(Duration::from_millis(20))),
    )
    .unwrap();
    let started = Instant::now();
    assert!(
        r.next_timeout(Some(Duration::from_millis(300)))
            .unwrap()
            .is_none()
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "reader hung: {:?}",
        started.elapsed()
    );
    let mut w = tickertape_tests::reopen_writer(dir.path(), small_config());
    append(&mut w, 0);
    assert_eq!(
        r.next_timeout(Some(Duration::from_secs(5)))
            .unwrap()
            .unwrap()
            .seq,
        0
    );
}

#[test]
fn a_copied_record_does_not_disturb_another_journals_wake_ups() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let mut wa = Writer::open(a.path(), small_config()).unwrap();
    // Journal B starts with A's record; B's writer must not remove A's region when it cleans up.
    std::fs::copy(a.path().join("notify"), b.path().join("notify")).unwrap();
    let _wb = Writer::open(b.path(), small_config()).unwrap();
    // A reader that opens A now must still attach to A's region and be woken promptly.
    let path = a.path().to_path_buf();
    let (tx, rx) = mpsc::channel::<Instant>();
    let reader = std::thread::spawn(move || {
        let mut r = Reader::open(
            &path,
            ReaderConfig::default().backoff(blocking(Duration::from_millis(400))),
        )
        .unwrap();
        let mut delays = Vec::new();
        for _ in 0..9 {
            r.next_timeout(Some(Duration::from_secs(30)))
                .unwrap()
                .expect("record");
            delays.push(Instant::now().saturating_duration_since(rx.recv().unwrap()));
        }
        delays.sort();
        delays[delays.len() / 2]
    });
    for seq in 0..9 {
        std::thread::sleep(Duration::from_millis(20));
        let sent = Instant::now();
        append(&mut wa, seq);
        tx.send(sent).unwrap();
    }
    let median = reader.join().unwrap();
    assert!(
        median < Duration::from_millis(20),
        "median delay {median:?}"
    );
}

#[test]
fn a_restarted_writer_is_noticed_within_the_refresh_interval() {
    // The review's scenario: the old writer is gone (its flag cleared), the reader polls with 800 ms sleeps, and a
    // new writer appends 60 ms later. The reader must re-read the record within 100 ms and be woken, not oversleep.
    let dir = tempfile::tempdir().unwrap();
    let w1 = Writer::open(dir.path(), small_config()).unwrap();
    let mut r = Reader::open(
        dir.path(),
        ReaderConfig::default().backoff(blocking(Duration::from_millis(800))),
    )
    .unwrap();
    drop(w1);
    let path = dir.path().to_path_buf();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(60));
        let mut w2 = tickertape_tests::reopen_writer(&path, small_config());
        let sent = Instant::now();
        append(&mut w2, 0);
        (sent, w2)
    });
    let rec = r
        .next_timeout(Some(Duration::from_secs(5)))
        .unwrap()
        .expect("record");
    let got = Instant::now();
    assert_eq!(rec.seq, 0);
    let (sent, _w2) = writer.join().unwrap();
    let delay = got.saturating_duration_since(sent);
    assert!(
        delay < Duration::from_millis(300),
        "delivered {delay:?} after the append"
    );
}
