//! Writer lock tests (crash contract C5; C3 with a real process kill is in the M4 kill tests).

use tickertape::{Error, Writer};
use tickertape_tests::small_config;

#[test]
fn second_writer_fails() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = Writer::open(dir.path(), small_config()).unwrap();
    tickertape_tests::append_n(&mut first, 3);
    let before: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();

    match Writer::open(dir.path(), small_config()) {
        Err(Error::Locked { pid, .. }) => assert_eq!(pid, Some(std::process::id())),
        other => panic!("expected Locked, got {other:?}"),
    }
    let after: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(
        before.len(),
        after.len(),
        "a refused writer must not create or remove files"
    );

    tickertape_tests::append_n(&mut first, 1);
    drop(first);
    let reopened = tickertape_tests::reopen_writer(dir.path(), small_config());
    assert_eq!(reopened.next_seq(), 4);
}

/// C3: a writer killed with SIGKILL while holding the lock leaves no stale lock.
#[test]
fn stale_lock_after_kill() {
    use std::process::Command;
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal");
    let go = dir.path().join("never");
    // The helper writer pauses (holding the lock) after 100 records, waiting for a file that never appears.
    let mut child = Command::new(env!("CARGO_BIN_EXE_tt-writer"))
        .args([
            journal.to_str().unwrap(),
            "1000",
            "65536",
            "16",
            "100",
            go.to_str().unwrap(),
        ])
        .spawn()
        .unwrap();
    // Wait until the child has written its 100 records (so it holds the lock and is paused).
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut reader = loop {
        match tickertape::Reader::open(&journal, tickertape::ReaderConfig::default()) {
            Ok(r) => break r,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => panic!("writer never started: {e}"),
        }
    };
    reader.seek(99).unwrap();
    assert!(
        reader
            .next_timeout(Some(Duration::from_secs(60)))
            .unwrap()
            .is_some()
    );
    match Writer::open(&journal, small_config()) {
        Err(tickertape::Error::Locked { pid, .. }) => assert_eq!(pid, Some(child.id())),
        other => panic!("expected Locked while the child runs, got {other:?}"),
    }
    child.kill().unwrap();
    child.wait().unwrap();
    // The OS released the lock when the holder died (retrying only covers other tests' fork-to-exec windows).
    let w = tickertape_tests::reopen_writer(&journal, small_config());
    assert_eq!(w.next_seq(), 100);
}
