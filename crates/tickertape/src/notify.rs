//! The journal's `notify` record and the wake-up region it points to (ADRs 0017 and 0021).
//!
//! The record is a small file that readers only ever read with `pread`; the region itself is shared memory that the
//! kernel won't let anyone shrink (`mmap::Notify`). Everything here affects latency only: when anything is missing,
//! stale or invalid, readers poll.

use crate::journal::NOTIFY_FILE;
use crate::mmap::{NOTIFY_NAME_LEN, Notify, NotifyLocation};
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Length of the `notify` record.
pub(crate) const RECORD_LEN: usize = 80;
const MAGIC: &[u8; 8] = b"TTNOTIFY";
const VERSION: u32 = 1;
const KIND_MEMFD: u32 = 1;
const KIND_SHM: u32 = 2;
/// How often a blocked reader re-reads the record, to follow writer restarts.
const REFRESH_INTERVAL: Duration = Duration::from_millis(100);

/// The decoded `notify` record: which region the current writer created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NotifyRecord {
    pub(crate) nonce: u64,
    pub(crate) location: NotifyLocation,
    /// Device and inode of the journal directory the writer had open: a record copied into another journal doesn't
    /// let that journal's writer remove the region.
    pub(crate) dir: (u64, u64),
}

/// Encodes a record (ADR 0021): magic, version, kind, nonce, pid, descriptor, journal directory identity, name,
/// CRC32C.
pub(crate) fn encode(record: &NotifyRecord) -> [u8; RECORD_LEN] {
    let mut buf = [0u8; RECORD_LEN];
    buf[0..8].copy_from_slice(MAGIC);
    buf[8..12].copy_from_slice(&VERSION.to_le_bytes());
    buf[16..24].copy_from_slice(&record.nonce.to_le_bytes());
    buf[32..40].copy_from_slice(&record.dir.0.to_le_bytes());
    buf[40..48].copy_from_slice(&record.dir.1.to_le_bytes());
    match record.location {
        NotifyLocation::Memfd { pid, fd } => {
            buf[12..16].copy_from_slice(&KIND_MEMFD.to_le_bytes());
            buf[24..28].copy_from_slice(&pid.to_le_bytes());
            buf[28..32].copy_from_slice(&fd.to_le_bytes());
        }
        NotifyLocation::Shm { name } => {
            buf[12..16].copy_from_slice(&KIND_SHM.to_le_bytes());
            buf[48..76].copy_from_slice(&name);
        }
    }
    let crc = crate::crc::crc32c(&buf[..76]);
    buf[76..80].copy_from_slice(&crc.to_le_bytes());
    buf
}

/// Decodes a record; `None` unless it is exactly [`RECORD_LEN`] bytes with a valid magic, version, kind and CRC.
pub(crate) fn decode(buf: &[u8]) -> Option<NotifyRecord> {
    let buf: &[u8; RECORD_LEN] = buf.try_into().ok()?;
    let u32_at = |at: usize| u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]);
    let u64_at = |at: usize| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&buf[at..at + 8]);
        u64::from_le_bytes(b)
    };
    if &buf[0..8] != MAGIC || u32_at(8) != VERSION || u32_at(76) != crate::crc::crc32c(&buf[..76]) {
        return None;
    }
    let location = match u32_at(12) {
        KIND_MEMFD => NotifyLocation::Memfd {
            pid: u32_at(24),
            fd: u32_at(28) as i32,
        },
        KIND_SHM => {
            let mut name = [0u8; NOTIFY_NAME_LEN];
            name.copy_from_slice(&buf[48..76]);
            NotifyLocation::Shm { name }
        }
        _ => return None,
    };
    Some(NotifyRecord {
        nonce: u64_at(16),
        location,
        dir: (u64_at(32), u64_at(40)),
    })
}

/// Reads the journal's record with `pread`; `None` if it is missing, invalid or not a regular file. Opened
/// non-blocking, so a FIFO or device put in its place can't make the caller hang.
pub(crate) fn read_record(dir: &Path) -> Option<NotifyRecord> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(dir.join(NOTIFY_FILE))
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut buf = [0u8; RECORD_LEN + 1];
    let n = file.read_at(&mut buf, 0).ok()?;
    decode(&buf[..n])
}

/// Replaces the record atomically (write a temporary file, then rename it over `notify`).
fn write_record(dir: &Path, record: &NotifyRecord) -> io::Result<()> {
    let tmp = dir.join(format!("{NOTIFY_FILE}.tmp"));
    fs::write(&tmp, encode(record))?;
    fs::rename(&tmp, dir.join(NOTIFY_FILE))
}

/// A nonce that differs between writers: std's randomly seeded hasher over the pid and the time.
fn new_nonce() -> u64 {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    hasher.write_u64(crate::clock::realtime_ns());
    hasher.finish()
}

/// Writer, on open: removes what a previous writer left, then (if `enabled`) creates a fresh region and publishes its
/// record. Any failure just disables wake-ups.
pub(crate) fn open_for_writer(dir: &Path, enabled: bool) -> Option<Notify> {
    let identity = fs::metadata(dir).ok().map(|m| (m.dev(), m.ino()));
    if let Some(old) = read_record(dir) {
        // Only a region this journal's own earlier writer created: this writer holds the lock, so that writer is gone.
        if Some(old.dir) == identity {
            crate::mmap::unlink_stale_region(&old.location);
        }
    }
    if !enabled {
        let _ = fs::remove_file(dir.join(NOTIFY_FILE));
        return None;
    }
    let nonce = new_nonce();
    let (notify, location) = Notify::create(nonce).ok()?;
    write_record(
        dir,
        &NotifyRecord {
            nonce,
            location,
            dir: identity?,
        },
    )
    .ok()?;
    Some(notify)
}

/// A reader's view of the journal's wake-up region: attaches to the region named by the record, re-reads the record
/// now and then to follow writer restarts, and gives up for good if the platform's wait call is unsupported.
#[derive(Debug)]
pub(crate) struct Waker {
    dir: PathBuf,
    current: Option<(u64, Notify)>,
    checked: Option<Instant>,
}

impl Waker {
    pub(crate) fn new(dir: &Path) -> Waker {
        let mut waker = Waker {
            dir: dir.to_path_buf(),
            current: None,
            checked: None,
        };
        waker.refresh();
        waker
    }

    /// When the record is next due to be re-read.
    pub(crate) fn refresh_due(&self) -> Instant {
        self.checked
            .map_or_else(Instant::now, |t| t + REFRESH_INTERVAL)
    }

    /// Re-reads the record if [`REFRESH_INTERVAL`] has passed since the last time.
    pub(crate) fn refresh_if_due(&mut self) {
        if self.checked.is_none_or(|t| t.elapsed() >= REFRESH_INTERVAL) {
            self.refresh();
        }
    }

    fn refresh(&mut self) {
        self.checked = Some(Instant::now());
        match read_record(&self.dir) {
            None => self.current = None,
            Some(record)
                if self
                    .current
                    .as_ref()
                    .is_some_and(|(nonce, _)| *nonce == record.nonce) => {}
            Some(record) => {
                self.current = Notify::attach(&record.location, record.nonce)
                    .ok()
                    .map(|notify| (record.nonce, notify));
            }
        }
    }

    /// Takes the attached region out (to be put back with [`Waker::put`]), so the reader can borrow itself mutably.
    pub(crate) fn take(&mut self) -> Option<(u64, Notify)> {
        self.current.take()
    }

    pub(crate) fn put(&mut self, current: (u64, Notify)) {
        self.current = Some(current);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_reject_damage() {
        let mut name = [0u8; NOTIFY_NAME_LEN];
        name.copy_from_slice(b"/tickertape.0123456789abcdef");
        for location in [
            NotifyLocation::Memfd { pid: 4242, fd: 7 },
            NotifyLocation::Shm { name },
        ] {
            let record = NotifyRecord {
                nonce: 0x0102_0304_0506_0708,
                location,
                dir: (0x1111, 0x2222_3333),
            };
            let bytes = encode(&record);
            assert_eq!(decode(&bytes), Some(record));
            for i in 0..RECORD_LEN {
                let mut damaged = bytes;
                damaged[i] ^= 0x40;
                assert_eq!(decode(&damaged), None, "byte {i}");
            }
            assert_eq!(decode(&bytes[..RECORD_LEN - 1]), None);
        }
        assert_eq!(decode(&[]), None);
    }

    #[test]
    fn nonces_differ() {
        assert_ne!(new_nonce(), new_nonce());
    }
}
