//! The reader-beacon protocol's interleavings (ADR 0032 → "The beacon protocol"), run deterministically: one party
//! stops at a pause point (`tickertape::fault::on_pause`) while the test runs another party's steps, then lets it
//! continue. Then a multi-process stress test: processes that create, update, drop and crash with beacons of a few
//! shared names while observers sample.
//!
//! flock locks belong to open files, so two beacons in one process contend exactly as two processes do.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use tickertape::fault::{Pause, on_pause};
use tickertape::status::{self, BeaconState, ReaderBeacon, Sampler};
use tickertape::{Error, Reader, ReaderConfig, Writer};
use tickertape_tests::{append_n, small_config};

const CHURN: &str = env!("CARGO_BIN_EXE_tt-beacon-churn");
const WAIT: Duration = Duration::from_secs(30);

/// A spawned child holds copies of all of this process's descriptors from `fork` until `exec`, including other
/// tests' beacon locks, which would then look held. Tests that release a lock and expect it free take this shared;
/// the test that spawns takes it exclusively.
static SPAWNING: RwLock<()> = RwLock::new(());

fn no_spawning() -> std::sync::RwLockReadGuard<'static, ()> {
    SPAWNING.read().unwrap_or_else(|e| e.into_inner())
}

fn spawning() -> std::sync::RwLockWriteGuard<'static, ()> {
    SPAWNING.write().unwrap_or_else(|e| e.into_inner())
}

struct Fixture {
    _tmp: tempfile::TempDir,
    journal: PathBuf,
    beacons: PathBuf,
    _writer: Writer,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let journal = tmp.path().join("journal");
    let beacons = tmp.path().join("beacons");
    std::fs::create_dir(&beacons).unwrap();
    let mut writer = Writer::open(&journal, small_config()).unwrap();
    append_n(&mut writer, 10);
    Fixture {
        _tmp: tmp,
        journal,
        beacons,
        _writer: writer,
    }
}

fn reader_at(journal: &Path, records: u64) -> Reader {
    let mut reader = Reader::open(journal, ReaderConfig::default()).unwrap();
    for _ in 0..records {
        reader.try_next().unwrap().unwrap();
    }
    reader
}

/// Installs a hook on this thread that, the first time it reaches `point`, reports it on `reached` and waits for
/// `go`.
fn pause_once(point: Pause, reached: Sender<()>, go: Receiver<()>) {
    let mut armed = true;
    on_pause(Some(Box::new(move |p| {
        if armed && p == point {
            armed = false;
            reached.send(()).unwrap();
            go.recv_timeout(WAIT).expect("never released");
        }
    })));
}

/// Installs a hook on this thread that reports reaching `point` without stopping.
fn notify_at(point: Pause, reached: Sender<()>) {
    on_pause(Some(Box::new(move |p| {
        if p == point {
            let _ = reached.send(());
        }
    })));
}

/// A stale beacon `name` at `records` read: a real beacon's bytes, written back after it was dropped.
fn stale(f: &Fixture, name: &str, records: u64) -> PathBuf {
    let reader = reader_at(&f.journal, records);
    let beacon = ReaderBeacon::create_in(&f.beacons, name, &reader).unwrap();
    let bytes = std::fs::read(beacon.path()).unwrap();
    let path = beacon.path().to_path_buf();
    drop(beacon);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn observe(f: &Fixture) -> status::Status {
    Sampler::new(&f.journal)
        .readers_dir(&f.beacons)
        .sample()
        .unwrap()
}

fn temporary_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with('.'))
        .collect()
}

/// Two creators of one free name: the second waits for the first, then finds its live beacon. Observers never see
/// the first beacon before it is complete.
#[test]
fn creators_of_one_name_serialize() {
    let _quiet = no_spawning();
    let f = fixture();
    let (reached, a_paused) = channel();
    let (a_go, go) = channel();
    let (journal, beacons) = (f.journal.clone(), f.beacons.clone());
    let a = thread::spawn(move || {
        pause_once(Pause::CreatorWroteTemporary, reached, go);
        ReaderBeacon::create_in(&beacons, "r", &reader_at(&journal, 1))
    });
    a_paused.recv_timeout(WAIT).unwrap();
    // A's beacon is written but not published: an observer sees nothing.
    let s = observe(&f);
    assert!(s.readers.is_empty(), "{:?}", s.readers);
    let (reached, b_waiting) = channel();
    let (journal, beacons) = (f.journal.clone(), f.beacons.clone());
    let b = thread::spawn(move || {
        notify_at(Pause::CreatorWaitsForDirectoryLock, reached);
        ReaderBeacon::create_in(&beacons, "r", &reader_at(&journal, 2))
    });
    b_waiting.recv_timeout(WAIT).unwrap();
    a_go.send(()).unwrap();
    let a = a.join().unwrap().unwrap();
    let err = b.join().unwrap().unwrap_err();
    assert!(matches!(err, Error::BeaconInUse { .. }), "{err}");
    assert!(a.is_published());
    let s = observe(&f);
    assert_eq!(s.readers.len(), 1);
    assert_eq!(s.readers[0].state, BeaconState::Live);
    assert_eq!(s.readers[0].report.unwrap().next_seq, 1);
    assert!(temporary_files(&f.beacons).is_empty());
}

/// Finding 6: an observer holding its shared probe lock on a stale beacon doesn't stop a creator from taking it over.
#[test]
fn takeover_while_an_observer_holds_its_probe() {
    let _quiet = no_spawning();
    let f = fixture();
    stale(&f, "r", 0);
    let (reached, holding) = channel();
    let (release, go) = channel();
    let (journal, beacons) = (f.journal.clone(), f.beacons.clone());
    let observer = thread::spawn(move || {
        pause_once(Pause::ObserverHoldsProbe, reached, go);
        Sampler::new(&journal)
            .readers_dir(&beacons)
            .sample()
            .unwrap()
    });
    holding.recv_timeout(WAIT).unwrap();
    let beacon = ReaderBeacon::create_in(&f.beacons, "r", &reader_at(&f.journal, 3))
        .expect("an observer's probe made creation fail");
    assert!(beacon.is_published());
    release.send(()).unwrap();
    // The observer was probing the old file; it notices the name changed and reports the new owner.
    let s = observer.join().unwrap();
    assert_eq!(s.readers.len(), 1);
    assert_eq!(s.readers[0].state, BeaconState::Live);
    assert_eq!(s.readers[0].report.unwrap().next_seq, 3);
}

/// Finding 4: the owner drops its beacon while a creator has the old file open: the creator publishes its own
/// beacon under the name (no ghost writing to a deleted file).
#[test]
fn owner_drops_while_a_creator_checks_the_name() {
    let _quiet = no_spawning();
    let f = fixture();
    let owner = ReaderBeacon::create_in(&f.beacons, "r", &reader_at(&f.journal, 1)).unwrap();
    let (reached, opened) = channel();
    let (go_tx, go) = channel();
    let (journal, beacons) = (f.journal.clone(), f.beacons.clone());
    let creator = thread::spawn(move || {
        pause_once(Pause::CreatorOpenedExisting, reached, go);
        ReaderBeacon::create_in(&beacons, "r", &reader_at(&journal, 4))
    });
    opened.recv_timeout(WAIT).unwrap();
    drop(owner);
    go_tx.send(()).unwrap();
    let beacon = creator
        .join()
        .unwrap()
        .expect("create after the owner left");
    assert!(beacon.is_published(), "a ghost beacon");
    let s = observe(&f);
    assert_eq!(s.readers.len(), 1);
    assert_eq!(s.readers[0].state, BeaconState::Live);
    assert_eq!(s.readers[0].report.unwrap().next_seq, 4);
    // A third creator can't take the name.
    let err = ReaderBeacon::create_in(&f.beacons, "r", &reader_at(&f.journal, 5)).unwrap_err();
    assert!(matches!(err, Error::BeaconInUse { .. }), "{err}");
}

/// Two creators race for one stale beacon: the one that holds the directory lock takes it over; the other then finds
/// a live beacon.
#[test]
fn two_creators_take_over_one_stale_beacon() {
    let _quiet = no_spawning();
    let f = fixture();
    stale(&f, "r", 0);
    let (reached, b_found) = channel();
    let (b_go, go) = channel();
    let (journal, beacons) = (f.journal.clone(), f.beacons.clone());
    let b = thread::spawn(move || {
        pause_once(Pause::CreatorFoundStale, reached, go);
        ReaderBeacon::create_in(&beacons, "r", &reader_at(&journal, 2))
    });
    b_found.recv_timeout(WAIT).unwrap();
    let (reached, c_waiting) = channel();
    let (journal, beacons) = (f.journal.clone(), f.beacons.clone());
    let c = thread::spawn(move || {
        notify_at(Pause::CreatorWaitsForDirectoryLock, reached);
        ReaderBeacon::create_in(&beacons, "r", &reader_at(&journal, 3))
    });
    c_waiting.recv_timeout(WAIT).unwrap();
    b_go.send(()).unwrap();
    let b = b.join().unwrap().unwrap();
    let err = c.join().unwrap().unwrap_err();
    assert!(matches!(err, Error::BeaconInUse { .. }), "{err}");
    assert!(b.is_published());
    assert_eq!(observe(&f).readers[0].report.unwrap().next_seq, 2);
}

/// The owner is between checking its name and unlinking it when a creator arrives: the owner still holds the lock,
/// so the creator is refused, and afterwards the name is free.
#[test]
fn creator_during_the_owners_unlink() {
    let _quiet = no_spawning();
    let f = fixture();
    let owner = ReaderBeacon::create_in(&f.beacons, "r", &reader_at(&f.journal, 1)).unwrap();
    let (reached, unlinking) = channel();
    let (go_tx, go) = channel();
    let dropper = thread::spawn(move || {
        pause_once(Pause::OwnerBeforeUnlink, reached, go);
        drop(owner);
    });
    unlinking.recv_timeout(WAIT).unwrap();
    let err = ReaderBeacon::create_in(&f.beacons, "r", &reader_at(&f.journal, 2)).unwrap_err();
    assert!(matches!(err, Error::BeaconInUse { .. }), "{err}");
    go_tx.send(()).unwrap();
    dropper.join().unwrap();
    assert!(observe(&f).readers.is_empty());
    let beacon = ReaderBeacon::create_in(&f.beacons, "r", &reader_at(&f.journal, 2)).unwrap();
    assert!(beacon.is_published());
}

/// Finding 5: an observer that opened a stale beacon just before a creator replaced it never pairs the dead reader's
/// position with the new owner's lock: it reports the new beacon, live, with its own position.
#[test]
fn observer_sees_the_new_owner_after_a_takeover() {
    let _quiet = no_spawning();
    for point in [
        Pause::ObserverOpened,
        Pause::ObserverRead,
        Pause::ObserverHoldsProbe,
    ] {
        let f = fixture();
        stale(&f, "r", 0);
        let (reached, paused) = channel();
        let (go_tx, go) = channel();
        let (journal, beacons) = (f.journal.clone(), f.beacons.clone());
        let observer = thread::spawn(move || {
            pause_once(point, reached, go);
            Sampler::new(&journal)
                .readers_dir(&beacons)
                .sample()
                .unwrap()
        });
        paused.recv_timeout(WAIT).unwrap();
        let beacon = ReaderBeacon::create_in(&f.beacons, "r", &reader_at(&f.journal, 3)).unwrap();
        go_tx.send(()).unwrap();
        let s = observer.join().unwrap();
        let r = &s.readers[0];
        assert_eq!(
            (r.state, r.report.unwrap().next_seq),
            (BeaconState::Live, 3),
            "at {point:?}"
        );
        drop(beacon);
    }
}

/// The owner leaves after an observer read its beacon: the observer notices the name is gone and reports nothing.
#[test]
fn observer_when_the_owner_leaves_mid_observation() {
    let _quiet = no_spawning();
    for point in [Pause::ObserverOpened, Pause::ObserverRead] {
        let f = fixture();
        let owner = ReaderBeacon::create_in(&f.beacons, "r", &reader_at(&f.journal, 1)).unwrap();
        let (reached, paused) = channel();
        let (go_tx, go) = channel();
        let (journal, beacons) = (f.journal.clone(), f.beacons.clone());
        let observer = thread::spawn(move || {
            pause_once(point, reached, go);
            Sampler::new(&journal)
                .readers_dir(&beacons)
                .sample()
                .unwrap()
        });
        paused.recv_timeout(WAIT).unwrap();
        drop(owner);
        go_tx.send(()).unwrap();
        let s = observer.join().unwrap();
        assert!(s.readers.is_empty(), "at {point:?}: {:?}", s.readers);
    }
}

/// Forks. The child runs `child` and exits with the code it returns, without running destructors or unwinding.
fn fork_child(child: impl FnOnce() -> i32) -> libc::pid_t {
    // SAFETY: fork has no memory-safety preconditions. The child only calls `child`, which the tests keep to a few
    // system calls and allocations (glibc and macOS's allocator reinitialise their locks in a forked child), then
    // leaves with `_exit`.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        let code = std::panic::catch_unwind(std::panic::AssertUnwindSafe(child)).unwrap_or(101);
        // SAFETY: `_exit` ends the process without running atexit handlers or destructors, as a forked child should.
        unsafe { libc::_exit(code) };
    }
    pid
}

/// Waits for a forked child; its exit code.
fn wait_child(pid: libc::pid_t) -> i32 {
    let mut status = 0;
    // SAFETY: `status` is a valid pointer for the call's duration.
    let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
    assert_eq!(waited, pid, "waitpid");
    assert!(
        libc::WIFEXITED(status),
        "child did not exit normally: {status}"
    );
    libc::WEXITSTATUS(status)
}

/// Round 2, A5: a child created with `fork` gets a copy of its parent's beacon. Dropping the copy never removes the
/// parent's live beacon, and updating it fails (the file belongs to the creator).
#[test]
fn a_forked_child_never_writes_or_removes_the_beacon() {
    let _spawning = spawning();
    let f = fixture();
    let reader = reader_at(&f.journal, 3);
    let moved = reader_at(&f.journal, 5);
    let mut beacon = ReaderBeacon::create_in(&f.beacons, "parent", &reader).unwrap();
    beacon.set_interval(Duration::ZERO);
    let before = std::fs::read(beacon.path()).unwrap();
    let pid = fork_child(|| {
        // SAFETY: after `fork` this process's `beacon` is a copy the parent never sees, and the child never drops the
        // original (it leaves with `_exit`): reading it bitwise gives the child one owned value, which it drops as a
        // child whose code owned the beacon would.
        let mut copy = unsafe { std::ptr::read(&beacon) };
        let refused = copy.update(&moved).is_err() && copy.flush(&moved).is_err();
        drop(copy);
        if refused { 0 } else { 1 }
    });
    assert_eq!(wait_child(pid), 0, "the child wrote the parent's beacon");
    assert!(
        beacon.is_published(),
        "the child's drop removed the parent's beacon"
    );
    assert_eq!(std::fs::read(beacon.path()).unwrap(), before);
    let s = observe(&f);
    assert_eq!(s.readers.len(), 1);
    assert_eq!(s.readers[0].state, BeaconState::Live);
    assert_eq!(s.readers[0].report.unwrap().next_seq, 3);
    // The parent's own beacon still works and is removed by the parent.
    assert!(beacon.update(&moved).unwrap());
    let path = beacon.path().to_path_buf();
    drop(beacon);
    assert!(!path.exists());
}

/// Round 2, A5: a child forked while a creator holds the beacon directory's lock doesn't keep the lock after the
/// creator releases it (the lock belongs to the open file, which the child shares): the next creator doesn't wait for
/// the child.
#[test]
fn a_forked_child_does_not_keep_the_directory_lock() {
    let _spawning = spawning();
    let f = fixture();
    let reader = reader_at(&f.journal, 2);
    let mut fds = [0; 2];
    // SAFETY: `fds` is a valid array of two descriptors for the call's duration.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (read_end, write_end) = (fds[0], fds[1]);
    let child = std::rc::Rc::new(std::cell::Cell::new(None));
    let forked = child.clone();
    on_pause(Some(Box::new(move |p| {
        if p == Pause::CreatorWroteTemporary && forked.get().is_none() {
            forked.set(Some(fork_child(|| {
                let mut byte = 0u8;
                // SAFETY: `byte` is a valid one-byte buffer; the read blocks until the parent writes or closes.
                unsafe { libc::read(read_end, (&raw mut byte).cast(), 1) };
                0
            })));
        }
    })));
    let first = ReaderBeacon::create_in(&f.beacons, "first", &reader).unwrap();
    on_pause(None);
    let pid = child.get().expect("forked at the pause point");
    // The child still runs, holding copies of every descriptor, the directory lock's included.
    let beacons = f.beacons.clone();
    let journal = f.journal.clone();
    let (done, finished) = channel();
    thread::spawn(move || {
        let reader = reader_at(&journal, 1);
        let created = ReaderBeacon::create_in(&beacons, "second", &reader).map(drop);
        let _ = done.send(created.is_ok());
    });
    let created = finished.recv_timeout(Duration::from_secs(10));
    // SAFETY: `write_end` is the pipe's write end; writing one byte releases the child.
    unsafe { libc::write(write_end, [1u8].as_ptr().cast(), 1) };
    assert_eq!(wait_child(pid), 0);
    // SAFETY: both descriptors are ours and closed once.
    unsafe {
        libc::close(read_end);
        libc::close(write_end);
    }
    assert_eq!(
        created,
        Ok(true),
        "the second creator waited for the child that inherited the directory lock"
    );
    drop(first);
}

/// Kills a child on drop.
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Processes create, update, drop, abort and are killed with beacons of two shared names while two observers
/// sample. No creator ever ends up with a ghost beacon, no error other than "in use" occurs, observers only ever see
/// complete beacons (live or stale), and afterwards nothing is left behind.
#[test]
fn beacon_churn_across_processes() {
    let _spawning = spawning();
    let f = fixture();
    let stop = AtomicBool::new(false);
    let go = f.beacons.parent().unwrap().join("go");
    let spawn = |seed: u64| {
        Guard(
            Command::new(CHURN)
                .args([
                    f.journal.to_str().unwrap(),
                    f.beacons.to_str().unwrap(),
                    "a,b",
                    "200",
                    &seed.to_string(),
                    "40",
                    go.to_str().unwrap(),
                ])
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("spawn churn"),
        )
    };
    let samples = std::sync::atomic::AtomicU64::new(0);
    thread::scope(|scope| {
        for _ in 0..2 {
            scope.spawn(|| {
                let mut sampler = Sampler::new(&f.journal).readers_dir(&f.beacons);
                while !stop.load(Ordering::Relaxed) {
                    let s = sampler.sample().unwrap();
                    for r in &s.readers {
                        // `Changing`: the name was replaced during every attempt to observe it; nothing reported.
                        assert!(
                            matches!(r.state, BeaconState::Live | BeaconState::Stale)
                                && r.report.is_some()
                                || r.state == BeaconState::Changing && r.report.is_none(),
                            "observer saw {r:?}"
                        );
                    }
                    samples.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
        let mut children: Vec<Guard> = (1..=6).map(spawn).collect();
        std::fs::write(&go, b"").unwrap();
        let mut spawned = 6u64;
        let mut finished = 0;
        let mut round = 0u64;
        while finished < 24 {
            round += 1;
            thread::sleep(Duration::from_millis(2));
            for (i, child) in children.iter_mut().enumerate() {
                // Now and then, kill one at an arbitrary point (possibly mid-creation).
                if round % 25 == i as u64 {
                    let _ = child.0.kill();
                }
                if let Some(status) = child.0.try_wait().unwrap() {
                    match status.code() {
                        Some(0) | None => {}
                        Some(code) => {
                            stop.store(true, Ordering::Relaxed);
                            panic!("churn process failed with {code} (2: error, 3: ghost)");
                        }
                    }
                    finished += 1;
                    spawned += 1;
                    *child = spawn(spawned);
                }
            }
        }
        for child in &mut children {
            let status = child.0.wait().unwrap();
            assert!(
                matches!(status.code(), Some(0) | None),
                "churn process failed: {status}"
            );
        }
        stop.store(true, Ordering::Relaxed);
    });
    assert!(samples.load(Ordering::Relaxed) > 0);
    // Take over whatever stale beacons remain and drop them: the directory ends up empty, temporary files that
    // killed creators left included, except those of creators killed between creating their temporary file and
    // writing it: an empty file doesn't prove it is a beacon, so it is left alone.
    let reader = reader_at(&f.journal, 0);
    for name in ["a", "b"] {
        drop(ReaderBeacon::create_in(&f.beacons, name, &reader).unwrap());
    }
    let left: Vec<_> = std::fs::read_dir(&f.beacons)
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| {
            let name = e.file_name().into_string().unwrap();
            !(name.ends_with(".beacon-tmp") && e.metadata().unwrap().len() < 128)
        })
        .map(|e| e.file_name())
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}
