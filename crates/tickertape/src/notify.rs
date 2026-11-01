//! The journal's `notify` record and the wake-up region it points to (ADRs 0017 and 0021).
//!
//! The record is a small file that readers only ever read with `pread`; the region itself is shared memory that the
//! kernel won't let anyone shrink (`mmap::Notify`). Everything here affects latency only: when anything is missing,
//! stale or invalid, readers poll.

use crate::journal::NOTIFY_FILE;
use crate::mmap::{NOTIFY_NAME_LEN, Notify, NotifyLocation};
use std::fs::{self, File};
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Length of the `notify` record.
pub(crate) const RECORD_LEN: usize = 64;
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
}

/// Encodes a record (ADR 0021): magic, version, kind, nonce, pid, descriptor, name, CRC32C.
pub(crate) fn encode(record: &NotifyRecord) -> [u8; RECORD_LEN] {
    let mut buf = [0u8; RECORD_LEN];
    buf[0..8].copy_from_slice(MAGIC);
    buf[8..12].copy_from_slice(&VERSION.to_le_bytes());
    buf[16..24].copy_from_slice(&record.nonce.to_le_bytes());
    match record.location {
        NotifyLocation::Memfd { pid, fd } => {
            buf[12..16].copy_from_slice(&KIND_MEMFD.to_le_bytes());
            buf[24..28].copy_from_slice(&pid.to_le_bytes());
            buf[28..32].copy_from_slice(&fd.to_le_bytes());
        }
        NotifyLocation::Shm { name } => {
            buf[12..16].copy_from_slice(&KIND_SHM.to_le_bytes());
            buf[32..60].copy_from_slice(&name);
        }
    }
    let crc = crate::crc::crc32c(&buf[..60]);
    buf[60..64].copy_from_slice(&crc.to_le_bytes());
    buf
}

/// Decodes a record; `None` unless it is exactly [`RECORD_LEN`] bytes with a valid magic, version, kind and CRC.
pub(crate) fn decode(buf: &[u8]) -> Option<NotifyRecord> {
    let buf: &[u8; RECORD_LEN] = buf.try_into().ok()?;
    let u32_at = |at: usize| u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]);
    if &buf[0..8] != MAGIC || u32_at(8) != VERSION || u32_at(60) != crate::crc::crc32c(&buf[..60]) {
        return None;
    }
    let mut nonce = [0u8; 8];
    nonce.copy_from_slice(&buf[16..24]);
    let location = match u32_at(12) {
        KIND_MEMFD => NotifyLocation::Memfd {
            pid: u32_at(24),
            fd: u32_at(28) as i32,
        },
        KIND_SHM => {
            let mut name = [0u8; NOTIFY_NAME_LEN];
            name.copy_from_slice(&buf[32..60]);
            NotifyLocation::Shm { name }
        }
        _ => return None,
    };
    Some(NotifyRecord {
        nonce: u64::from_le_bytes(nonce),
        location,
    })
}

/// Reads the journal's record with `pread`; `None` if it is missing or invalid.
pub(crate) fn read_record(dir: &Path) -> Option<NotifyRecord> {
    let file = File::open(dir.join(NOTIFY_FILE)).ok()?;
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
    if let Some(old) = read_record(dir) {
        crate::mmap::unlink_stale_region(&old.location);
    }
    if !enabled {
        let _ = fs::remove_file(dir.join(NOTIFY_FILE));
        return None;
    }
    let nonce = new_nonce();
    let (notify, location) = Notify::create(nonce).ok()?;
    write_record(dir, &NotifyRecord { nonce, location }).ok()?;
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
