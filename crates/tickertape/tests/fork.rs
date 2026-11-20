//! `Writer::release_after_fork`: a child that releases its inherited copy of a writer no longer keeps the journal
//! locked once the parent closes its own, and does not unlock it while the parent still has it open.

use std::path::Path;
use std::time::{Duration, Instant};

use tickertape::{Error, NewRecord, Reader, Writer, WriterConfig};

fn record(payload: &[u8]) -> NewRecord<'_> {
    NewRecord {
        schema_id: 0,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload,
    }
}

/// Waits up to 20 s for `path` to exist.
fn wait_for(path: &Path) -> bool {
    let started = Instant::now();
    while !path.exists() {
        if started.elapsed() > Duration::from_secs(20) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    true
}

fn fork_and_release(prefault: bool) {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal");
    let released = dir.path().join("released");
    let done = dir.path().join("done");
    let config = WriterConfig::default()
        .segment_size(1 << 20)
        .prefault(prefault);
    let mut writer = Writer::open(&journal, config.clone()).unwrap();
    writer.append(&record(b"zero")).unwrap();

    // SAFETY: the child only releases the inherited writer, touches two files and exits with `_exit`, without
    // returning into the test harness.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        writer.release_after_fork();
        let ok = std::fs::write(&released, b"").is_ok() && wait_for(&done);
        // SAFETY: ends the child without running the parent's destructors or the test harness.
        unsafe { libc::_exit(if ok { 0 } else { 1 }) };
    }

    assert!(wait_for(&released), "the child did not release its copy");
    // The child is alive and has released its copy: the parent still holds the lock and can append.
    assert!(matches!(
        Writer::open(&journal, config.clone()),
        Err(Error::Locked { .. })
    ));
    assert_eq!(writer.append(&record(b"one")).unwrap(), 1);
    writer.checkpoint().unwrap();
    drop(writer);
    // Only the parent's descriptor held the lock: the journal opens while the child is still running.
    let mut again = Writer::open(&journal, config).unwrap();
    assert_eq!(again.append(&record(b"two")).unwrap(), 2);
    drop(again);
    std::fs::write(&done, b"").unwrap();

    let mut status = 0;
    // SAFETY: waits for the child created above; `status` is a valid out pointer.
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "child status {status}"
    );

    let mut reader = Reader::open(&journal, Default::default()).unwrap();
    let mut payloads = Vec::new();
    while let Some(r) = reader.try_next().unwrap() {
        payloads.push(r.payload.to_vec());
    }
    assert_eq!(payloads, [&b"zero"[..], b"one", b"two"]);
}

#[test]
fn release_after_fork_frees_the_lock_without_unlocking_the_parent() {
    fork_and_release(false);
    fork_and_release(true);
}
