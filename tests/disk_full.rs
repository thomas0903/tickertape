//! Crash contract C9: disk full (and other failures) while creating a segment.

use tickertape::fault::{self, Point};
use tickertape::{Error, NewRecord, Writer};
use tickertape_tests::{SMALL_SEGMENT, append_n, read_all, segments, small_config};

/// Appends until the next 300-byte record would need a roll; returns how many records are in the journal.
fn fill_first_segment(w: &mut Writer, dir: &std::path::Path) -> u64 {
    while segments(dir).len() == 1 {
        append_n(w, 1);
    }
    // One record too many rolled; that's fine: the journal now has two segments and we test the next roll.
    w.next_seq()
}

fn big_record(seq: u64) -> Vec<u8> {
    tickertape_tests::payload(seq, 2000)
}

#[test]
fn injected_enospc_on_roll_is_reported_and_retried() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    fill_first_segment(&mut w, dir.path());
    let before_segments = segments(dir.path()).len();

    // Append 2,000-byte records until a roll is needed, with ENOSPC armed for the next preallocation.
    fault::fail_next(Point::Preallocate, libc_enospc());
    let mut failed_at = None;
    for _ in 0..100 {
        let seq = w.next_seq();
        let data = big_record(seq);
        match w.append(&NewRecord {
            schema_id: 0,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: &data,
        }) {
            Ok(_) => {}
            Err(e) => {
                assert!(
                    e.is_storage_full(),
                    "expected a storage-full error, got {e}"
                );
                failed_at = Some(seq);
                break;
            }
        }
    }
    let failed_at = failed_at.expect("a roll was needed within 100 large records");
    assert_eq!(
        segments(dir.path()).len(),
        before_segments,
        "no new segment after a failed roll"
    );
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary file left behind: {leftovers:?}"
    );
    assert_eq!(
        w.next_seq(),
        failed_at,
        "a failed append must not consume a sequence number"
    );
    assert_eq!(
        read_all(dir.path()).len() as u64,
        failed_at,
        "journal still readable and unchanged"
    );

    // Space is back: the same append succeeds and rolls.
    let data = big_record(failed_at);
    let seq = w
        .append(&NewRecord {
            schema_id: 0,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: &data,
        })
        .unwrap();
    assert_eq!(seq, failed_at);
    assert_eq!(segments(dir.path()).len(), before_segments + 1);
    let all = read_all(dir.path());
    assert_eq!(all.len() as u64, failed_at + 1);
    assert!(all.iter().enumerate().all(|(i, r)| r.seq == i as u64));
}

#[test]
fn injected_link_failure_leaves_no_trace() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    let max = tickertape::format::max_payload_len(SMALL_SEGMENT) as usize;
    let data = vec![1u8; max];
    let rec = NewRecord {
        schema_id: 0,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload: &data,
    };
    w.append(&rec).unwrap();
    fault::fail_next(Point::Link, 5 /* EIO */);
    assert!(matches!(
        w.append(&rec),
        Err(Error::Io {
            op: "publish segment",
            ..
        })
    ));
    assert_eq!(segments(dir.path()).len(), 1);
    assert_eq!(w.append(&rec).unwrap(), 1);
    drop(w);
    let w = Writer::open(dir.path(), small_config()).unwrap();
    assert!(
        w.recovery_report().actions.is_empty(),
        "{:?}",
        w.recovery_report()
    );
    assert_eq!(w.next_seq(), 2);
}

fn libc_enospc() -> i32 {
    std::io::Error::from(std::io::ErrorKind::StorageFull)
        .raw_os_error()
        .unwrap_or(28) // ENOSPC is 28 on Linux and macOS
}

/// A real full filesystem: a 4 MiB volume (macOS: an APFS disk image via `hdiutil`; Linux: a `tmpfs` mounted with
/// `sudo`). Skipped (with a message) where that isn't possible without a password.
#[test]
#[ignore = "needs hdiutil (macOS) or passwordless sudo (Linux); run with -- --ignored"]
fn real_small_filesystem() {
    let Some(vol) = SmallVolume::create(4) else {
        eprintln!("real_small_filesystem: skipped, no small filesystem available");
        return;
    };
    let journal = vol.path.join("journal");
    let mut w = Writer::open(
        &journal,
        tickertape::WriterConfig::default().segment_size(1024 * 1024),
    )
    .unwrap();
    let data = vec![9u8; 4000];
    let mut appended = 0u64;
    let err = loop {
        match w.append(&NewRecord {
            schema_id: 0,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: &data,
        }) {
            Ok(_) => appended += 1,
            Err(e) => break e,
        }
        assert!(appended < 100_000, "the volume never filled up");
    };
    assert!(err.is_storage_full(), "expected storage full, got {err}");
    assert_eq!(
        read_all(&journal).len() as u64,
        appended,
        "journal readable after disk full"
    );
    drop(w);
    let w = Writer::open(
        &journal,
        tickertape::WriterConfig::default().segment_size(1024 * 1024),
    )
    .unwrap();
    assert_eq!(w.next_seq(), appended);
    eprintln!("real_small_filesystem: {appended} records before ENOSPC");
}

struct SmallVolume {
    path: std::path::PathBuf,
    #[allow(dead_code)]
    image: Option<tempfile::TempDir>,
}

impl SmallVolume {
    #[cfg(target_os = "macos")]
    fn create(mib: u32) -> Option<SmallVolume> {
        use std::process::Command;
        let image_dir = tempfile::tempdir().ok()?;
        let image = image_dir.path().join("vol.dmg");
        let ok = Command::new("hdiutil")
            .args([
                "create",
                "-quiet",
                "-size",
                &format!("{mib}m"),
                "-fs",
                "APFS",
                "-volname",
                "ttfull",
            ])
            .arg(&image)
            .status()
            .ok()?
            .success();
        if !ok {
            return None;
        }
        let mount = image_dir.path().join("mnt");
        std::fs::create_dir(&mount).ok()?;
        let ok = Command::new("hdiutil")
            .args(["attach", "-quiet", "-nobrowse", "-mountpoint"])
            .arg(&mount)
            .arg(&image)
            .status()
            .ok()?
            .success();
        ok.then_some(SmallVolume {
            path: mount,
            image: Some(image_dir),
        })
    }

    /// Linux: `tmpfs` by default; with `TICKERTAPE_SMALL_FS=ext4` or `=btrfs`, a loop-mounted image of that
    /// filesystem (at least 128 MiB, the btrfs minimum).
    #[cfg(target_os = "linux")]
    fn create(mib: u32) -> Option<SmallVolume> {
        use std::process::Command;
        let dir = tempfile::tempdir().ok()?;
        let mount = dir.path().join("mnt");
        std::fs::create_dir(&mount).ok()?;
        let fs = std::env::var("TICKERTAPE_SMALL_FS").unwrap_or_else(|_| "tmpfs".into());
        let ok = if fs == "tmpfs" {
            Command::new("sudo")
                .args([
                    "-n",
                    "mount",
                    "-t",
                    "tmpfs",
                    "-o",
                    &format!("size={mib}m,mode=0777"),
                    "tmpfs",
                ])
                .arg(&mount)
                .status()
                .ok()?
                .success()
        } else {
            let image = dir.path().join("vol.img");
            std::fs::File::create(&image)
                .ok()?
                .set_len(u64::from(mib.max(128)) << 20)
                .ok()?;
            let mkfs = Command::new(format!("mkfs.{fs}"))
                .arg("-q")
                .arg(&image)
                .status()
                .ok()?;
            mkfs.success()
                && Command::new("sudo")
                    .args(["-n", "mount", "-o", "loop"])
                    .arg(&image)
                    .arg(&mount)
                    .status()
                    .ok()?
                    .success()
                && Command::new("sudo")
                    .args(["-n", "chmod", "0777"])
                    .arg(&mount)
                    .status()
                    .ok()?
                    .success()
        };
        eprintln!("small volume: {fs}");
        ok.then_some(SmallVolume {
            path: mount,
            image: Some(dir),
        })
    }
}

impl Drop for SmallVolume {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("hdiutil")
            .args(["detach", "-quiet", "-force"])
            .arg(&self.path)
            .status();
        #[cfg(target_os = "linux")]
        let _ = std::process::Command::new("sudo")
            .args(["-n", "umount"])
            .arg(&self.path)
            .status();
    }
}

/// Measures crash-contract C10: a writer appending into an already preallocated segment after the volume has
/// filled up. Runs the writer in a child process so that a SIGBUS shows up as a signal instead of killing the test.
///
/// Expected to pass on APFS, ext4 and tmpfs, and to FAIL on btrfs (SIGBUS), as recorded in the crash contract.
/// On Linux, pick the filesystem with `TICKERTAPE_SMALL_FS=ext4|btrfs` (default tmpfs).
#[test]
#[ignore = "needs hdiutil (macOS) or passwordless sudo (Linux); run with -- --ignored"]
fn full_volume_then_write_into_preallocated_segment() {
    use std::os::unix::process::ExitStatusExt;

    let Some(vol) = SmallVolume::create(16) else {
        eprintln!(
            "full_volume_then_write_into_preallocated_segment: skipped, no small filesystem available"
        );
        return;
    };
    let journal = vol.path.join("journal");
    let segment = 8 * 1024 * 1024u64;
    // Create the journal and its first (preallocated) segment, then fill the rest of the volume.
    drop(
        Writer::open(
            &journal,
            tickertape::WriterConfig::default().segment_size(segment),
        )
        .unwrap(),
    );
    let mut filler = std::fs::File::create(vol.path.join("filler")).unwrap();
    let chunk = vec![0xEEu8; 64 * 1024];
    loop {
        use std::io::Write;
        if filler
            .write_all(&chunk)
            .and_then(|()| filler.sync_all())
            .is_err()
        {
            break;
        }
    }
    // Now append ~5 MiB into the existing 8 MiB segment from a child process (no roll is needed).
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_tt-writer"))
        .args([
            journal.to_str().unwrap(),
            "60000",
            &segment.to_string(),
            "64",
        ])
        .status()
        .unwrap();
    eprintln!(
        "full_volume_then_write_into_preallocated_segment: writer exit status {status:?} (signal {:?})",
        status.signal()
    );
    assert!(
        status.success(),
        "writing into a preallocated segment on a full volume failed: {status:?}"
    );
    assert_eq!(read_all(&journal).len(), 60_000);
}
