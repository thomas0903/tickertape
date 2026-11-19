//! Reader beacons (ADR 0032 → "The beacon protocol"): creation, takeover, updates, removal and observation.
//!
//! The protocol rests on one invariant: **a beacon file is locked (exclusive `flock`) only by its creator, before it
//! is published under its name, and never again.** The creator writes the complete beacon into a new temporary file
//! that it created exclusively and locked, and only then links it to its name. So a published beacon is always
//! complete, its contents are written only by the process holding its lock, and once it is seen unlocked its owner is
//! gone for good: it can only be stale. Creators serialize among themselves on an exclusive `flock` of the beacon
//! directory; observers never take that lock, and probe beacons with a shared lock, which never conflicts with a
//! creator's checks. The decision record walks through every interleaving.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{BeaconDirState, BeaconReport, BeaconState, Position, READERS_DIR};
use crate::error::{Error, Result};
use crate::fault::{Pause, pause};
use crate::journal::{self, Identity};
use crate::reader::Reader;

/// Exact length of a beacon file.
pub(super) const BEACON_LEN: usize = 128;
const BEACON_MAGIC: [u8; 8] = *b"TTREADER";
const BEACON_VERSION: u32 = 1;
const BEACON_NAME_MAX: usize = 64;
/// Flag bits: the reader is durable-only; a seek is pending.
const FLAG_DURABLE_ONLY: u32 = 1;
const FLAG_SEEK_PENDING: u32 = 2;
const KNOWN_FLAGS: u32 = FLAG_DURABLE_ONLY | FLAG_SEEK_PENDING;
/// Bytes of the CRC-covered part.
const CRC_AT: usize = BEACON_LEN - 4;
/// Suffix of a creator's temporary file (`.<name>.<pid>.<n>.beacon-tmp`). Starts with a dot, so never a beacon name.
const TMP_SUFFIX: &str = ".beacon-tmp";
/// Names in a journal directory that a beacon must never take, even in a beacon directory of the caller's choosing
/// (which may be the journal directory itself). Names ending in `.tape` or `.tmp` are refused as well.
const RESERVED_NAMES: [&str; 6] = [
    journal::CHECKPOINT_FILE,
    journal::LOCK_FILE,
    journal::NOTIFY_FILE,
    journal::SPARE_FILE,
    journal::QUARANTINE_DIR,
    READERS_DIR,
];
/// Attempts at publishing while the name keeps changing under the creator (only outside the protocol).
const PUBLISH_ATTEMPTS: u32 = 8;
/// Attempts at observing a beacon while its name keeps being replaced.
const OBSERVE_ATTEMPTS: u32 = 3;

/// Publishes a reader's position for observers in other processes ([`status`](super::status), `tickertape status`).
/// Opt-in.
///
/// A beacon is a 128-byte file, by default `<journal>/readers/<name>`, outside the journal format (ADR 0032). It
/// carries the reader's cursor, the identity of the segment file the cursor is in (so that an observer can tell
/// whether the position belongs to the journal it samples), whether the reader is durable-only, the owner's PID and
/// the time of the write. The beacon holds an exclusive `flock` on its file while it exists, so an observer can tell
/// a live reader from one that died (the kernel releases the lock with the process): a dead reader's beacon shows as
/// [`BeaconState::Stale`] and never counts as lag. Dropping the beacon removes the file. The writer, other readers
/// and retention never read beacons: a slow or dead reader can't slow the writer down or hold back retention.
///
/// [`ReaderBeacon::update`] is meant to be called often (for example after each record or batch, and when a wait
/// times out): it writes at most once per interval, and only when the position moved. Call
/// [`ReaderBeacon::flush`] before a reader stops for a long time, so the last position is published.
///
/// **Caveats.** The lock belongs to the open file, not the process: a child created with `fork` that keeps the
/// descriptor keeps the beacon live after its creator dies, until the child exits. The descriptor is close-on-exec,
/// so `exec` drops it, but from `fork` to `exec` every child holds it: while another thread is spawning a process,
/// a beacon that was just dropped can still look live for that moment. On network filesystems `flock` may be emulated or unsupported (Linux NFS emulates it with
/// byte-range locks, so a probe from the same process as the owner can release the owner's lock); beacons are
/// supported on local filesystems only, like the journal. Where the lock can't be probed, observers report
/// [`BeaconState::Uncertain`].
///
/// ```no_run
/// # use std::time::Duration;
/// # use tickertape::{Reader, ReaderConfig, status::ReaderBeacon};
/// # fn main() -> tickertape::Result<()> {
/// let mut reader = Reader::open("/tmp/journal", ReaderConfig::default())?;
/// let mut beacon = ReaderBeacon::create("/tmp/journal", "risk-engine", &reader)?;
/// loop {
///     if let Some(record) = reader.next_timeout(Some(Duration::from_secs(1)))? {
///         // ... process the record ...
///         let _ = record;
///     }
///     beacon.update(&reader)?;
/// }
/// # }
/// ```
#[derive(Debug)]
pub struct ReaderBeacon {
    path: PathBuf,
    /// The beacon file, locked exclusively for the beacon's lifetime.
    file: File,
    identity: Identity,
    interval: Duration,
    last_write: Option<Instant>,
    last_seq: Option<u64>,
    /// The reader's current segment file and its header's creation time and size, read once per segment.
    segment: Option<(Identity, u64, u64)>,
}

impl ReaderBeacon {
    /// Creates the beacon `name` in `<journal_dir>/readers` (creating that directory if needed, with the process's
    /// umask), and publishes `reader`'s current position.
    ///
    /// `name` is 1 to 64 ASCII letters, digits, `.`, `_` or `-`; it doesn't start with `.`, isn't the name of a
    /// journal file (`checkpoint`, `writer.lock`, `notify`, `next-segment.tmp`, `quarantine`, `readers`) and doesn't
    /// end in `.tape` or `.tmp`. If a file already has the name, it is replaced only if it is positively a stale beacon
    /// (exactly 128 bytes, valid magic, version and checksum, and unlocked); anything else is left untouched.
    ///
    /// The file is created with the process's umask (mode `0666` before the umask); an observer needs read access to
    /// it and to the directory. Creation waits while another process creates a beacon in the same directory
    /// (microseconds); observers never make it wait.
    ///
    /// Cost: `mkdir`, then as for [`ReaderBeacon::create_in`].
    ///
    /// # Errors
    ///
    /// [`Error::InvalidConfig`] for an invalid name, or if the name is taken by something that is not a stale beacon;
    /// [`Error::BeaconInUse`] if a live beacon has the name; [`Error::Io`], also if the filesystem doesn't support
    /// `flock`.
    pub fn create(
        journal_dir: impl AsRef<Path>,
        name: &str,
        reader: &Reader,
    ) -> Result<ReaderBeacon> {
        check_name(name)?;
        let dir = journal_dir.as_ref().join(READERS_DIR);
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(Error::io("create reader beacon directory", &dir, e)),
        }
        ReaderBeacon::create_in(dir, name, reader)
    }

    /// Like [`ReaderBeacon::create`], but in the existing directory `beacons_dir`. Use it for readers that can't
    /// write to the journal directory (for example, a reader of another user's journal), and point observers at the
    /// directory with [`Sampler::readers_dir`](super::Sampler::readers_dir) or `tickertape status --readers-dir`.
    ///
    /// Cost: `open` and `flock` of the directory, a `readdir` pass to remove temporary files that crashed creators
    /// left, `open` (exclusive create), `flock`, `fstat`, a 128-byte `pwrite` and `link` of a temporary file, then
    /// `unlink` of its temporary name. Taking over a stale beacon instead costs an `open`, `fstat`, `flock`, `pread`
    /// and `lstat` of the old file and a `rename`. Reading the reader's segment header costs one `fstat` and `pread`.
    ///
    /// # Errors
    ///
    /// As for [`ReaderBeacon::create`].
    pub fn create_in(
        beacons_dir: impl AsRef<Path>,
        name: &str,
        reader: &Reader,
    ) -> Result<ReaderBeacon> {
        check_name(name)?;
        let dir = beacons_dir.as_ref();
        let path = dir.join(name);
        let mut segment = None;
        let bytes = encode(&payload_of(reader, &mut segment)?);

        let _dir_lock = DirLock::acquire(dir)?;
        remove_leftovers(dir);
        let (mut tmp, file) = Temporary::create(dir, name)?;
        file.write_all_at(&bytes, 0)
            .map_err(|e| Error::io("write reader beacon", &tmp.path, e))?;
        pause(Pause::CreatorWroteTemporary);
        let identity = journal::file_identity(&file, &tmp.path)?;
        publish(&tmp.path, &path, identity)?;
        tmp.published = true;
        Ok(ReaderBeacon {
            path,
            file,
            identity,
            interval: super::DEFAULT_BEACON_INTERVAL,
            last_write: Some(Instant::now()),
            last_seq: Some(reader.position()),
            segment,
        })
    }

    /// Sets the minimum time between two writes made by [`ReaderBeacon::update`] (default
    /// [`DEFAULT_BEACON_INTERVAL`](super::DEFAULT_BEACON_INTERVAL)). Zero writes on every position change.
    pub fn set_interval(&mut self, interval: Duration) {
        self.interval = interval;
    }

    /// Publishes `reader`'s cursor if its sequence number ([`Reader::position`]) moved since the last write and the
    /// interval has passed. Returns whether it wrote.
    ///
    /// Cost: a comparison if the position hasn't moved; otherwise one monotonic clock read, and at most once per
    /// interval one 128-byte `pwrite` (no sync) and a wall-clock read, plus one `fstat` and a 64-byte `pread` of the
    /// segment header when the reader has moved to another segment file since the last write. Calling it after every
    /// record adds about one clock read per record; call it once per batch where that matters.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the write fails, [`Error::Corrupt`] if the reader's segment header can't be read.
    pub fn update(&mut self, reader: &Reader) -> Result<bool> {
        if self.last_seq == Some(reader.position())
            || self
                .last_write
                .is_some_and(|at| at.elapsed() < self.interval)
        {
            return Ok(false);
        }
        self.write(reader)?;
        Ok(true)
    }

    /// Publishes `reader`'s cursor now, whatever the interval.
    ///
    /// Cost: one 128-byte `pwrite`, plus a segment header read as for [`ReaderBeacon::update`].
    ///
    /// # Errors
    ///
    /// As for [`ReaderBeacon::update`].
    pub fn flush(&mut self, reader: &Reader) -> Result<()> {
        self.write(reader)
    }

    /// The beacon file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the beacon's path still names this beacon's file. The protocol never replaces a live beacon, so this
    /// is false only if something outside it removed or replaced the file.
    ///
    /// Cost: one `lstat`.
    pub fn is_published(&self) -> bool {
        lstat_identity(&self.path) == Some(self.identity)
    }

    fn write(&mut self, reader: &Reader) -> Result<()> {
        let payload = payload_of(reader, &mut self.segment)?;
        self.file
            .write_all_at(&encode(&payload), 0)
            .map_err(|e| Error::io("write reader beacon", &self.path, e))?;
        self.last_write = Some(Instant::now());
        self.last_seq = Some(payload.report.next_seq);
        Ok(())
    }
}

impl Drop for ReaderBeacon {
    fn drop(&mut self) {
        // We still hold the lock, so no creator can take the name over between this check and the unlink: only a
        // stale beacon is ever replaced. The lock is released when `file` closes, after the unlink.
        if self.is_published() {
            pause(Pause::OwnerBeforeUnlink);
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// A decoded beacon: the report and the identity of the segment file its cursor is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Payload {
    pub(super) report: BeaconReport,
    pub(super) segment_file: Identity,
    pub(super) segment_size: u64,
    pub(super) segment_created_unix_ns: u64,
}

/// A beacon as an observer found it.
#[derive(Debug)]
pub(super) struct Observed {
    pub(super) name: String,
    pub(super) path: PathBuf,
    pub(super) state: BeaconState,
    pub(super) payload: Option<Payload>,
}

fn check_name(name: &str) -> Result<()> {
    if !valid_name(name) {
        return Err(Error::InvalidConfig(format!(
            "reader beacon name {name:?}: use 1 to {BEACON_NAME_MAX} ASCII letters, digits, '.', '_' or '-', not starting with '.'"
        )));
    }
    if reserved(name) {
        return Err(Error::InvalidConfig(format!(
            "reader beacon name {name:?} is the name of a journal file"
        )));
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    (1..=BEACON_NAME_MAX).contains(&name.len())
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn reserved(name: &str) -> bool {
    RESERVED_NAMES.contains(&name) || name.ends_with(".tape") || name.ends_with(".tmp")
}

fn lstat_identity(path: &Path) -> Option<Identity> {
    fs::symlink_metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// The reader's cursor and its segment's identity, reading the segment header when the reader moved to another
/// file since `segment` was filled.
fn payload_of(reader: &Reader, segment: &mut Option<(Identity, u64, u64)>) -> Result<Payload> {
    let parts = reader.cursor_parts();
    let cursor = parts.cursor();
    let (created, size) = match *segment {
        Some((id, created, size)) if id == parts.identity => (created, size),
        _ => {
            let header =
                journal::read_header(parts.file, parts.path, cursor.position.segment_base_seq)?;
            *segment = Some((parts.identity, header.created_unix_ns, header.segment_size));
            (header.created_unix_ns, header.segment_size)
        }
    };
    Ok(Payload {
        report: BeaconReport {
            pid: std::process::id(),
            next_seq: cursor.next_seq,
            position: cursor.position,
            seek_pending: cursor.seek_pending,
            durable_only: cursor.durable_only,
            updated_unix_ns: crate::clock::realtime_ns(),
        },
        segment_file: parts.identity,
        segment_size: size,
        segment_created_unix_ns: created,
    })
}

/// Encodes a beacon (ADR 0032 → "Beacon file"), little-endian:
///
/// | Offset | Size | Field |
/// |---|---|---|
/// | 0 | 8 | magic `TTREADER` |
/// | 8 | 4 | version (1) |
/// | 12 | 4 | flags: bit 0 durable-only, bit 1 seek pending; other bits zero |
/// | 16 | 4 | pid |
/// | 20 | 4 | reserved, zero |
/// | 24 | 8 | `next_seq`: the next record the reader returns |
/// | 32 | 8 | sequence number expected at the cursor's slot |
/// | 40 | 8 | the slot's segment base sequence number |
/// | 48 | 8 | the slot's offset |
/// | 56 | 8 | the segment header's `created_unix_ns` |
/// | 64 | 8 | the segment header's `segment_size` |
/// | 72 | 8 | the segment file's device number |
/// | 80 | 8 | the segment file's inode number |
/// | 88 | 8 | wall-clock time of the write, ns since the Unix epoch |
/// | 96 | 28 | reserved, zero |
/// | 124 | 4 | CRC32C of bytes 0–123 |
pub(super) fn encode(p: &Payload) -> [u8; BEACON_LEN] {
    let mut b = [0u8; BEACON_LEN];
    let r = &p.report;
    let flags = if r.durable_only { FLAG_DURABLE_ONLY } else { 0 }
        | if r.seek_pending { FLAG_SEEK_PENDING } else { 0 };
    b[0..8].copy_from_slice(&BEACON_MAGIC);
    b[8..12].copy_from_slice(&BEACON_VERSION.to_le_bytes());
    b[12..16].copy_from_slice(&flags.to_le_bytes());
    b[16..20].copy_from_slice(&r.pid.to_le_bytes());
    for (at, value) in [
        (24, r.next_seq),
        (32, r.position.next_seq),
        (40, r.position.segment_base_seq),
        (48, r.position.offset),
        (56, p.segment_created_unix_ns),
        (64, p.segment_size),
        (72, p.segment_file.0),
        (80, p.segment_file.1),
        (88, r.updated_unix_ns),
    ] {
        b[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }
    let crc = crate::crc::crc32c(&b[..CRC_AT]);
    b[CRC_AT..].copy_from_slice(&crc.to_le_bytes());
    b
}

/// Decodes a beacon; `None` unless `b` is exactly [`BEACON_LEN`] bytes with valid magic, version, flags, reserved
/// bytes and checksum.
pub(super) fn decode(b: &[u8]) -> Option<Payload> {
    if b.len() != BEACON_LEN {
        return None;
    }
    let u32_at = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    let u64_at = |at: usize| {
        let mut x = [0u8; 8];
        x.copy_from_slice(&b[at..at + 8]);
        u64::from_le_bytes(x)
    };
    let flags = u32_at(12);
    if b[0..8] != BEACON_MAGIC
        || u32_at(8) != BEACON_VERSION
        || flags & !KNOWN_FLAGS != 0
        || b[20..24].iter().chain(&b[96..CRC_AT]).any(|&x| x != 0)
        || u32_at(CRC_AT) != crate::crc::crc32c(&b[..CRC_AT])
    {
        return None;
    }
    Some(Payload {
        report: BeaconReport {
            pid: u32_at(16),
            next_seq: u64_at(24),
            position: Position::new(u64_at(32), u64_at(40), u64_at(48)),
            seek_pending: flags & FLAG_SEEK_PENDING != 0,
            durable_only: flags & FLAG_DURABLE_ONLY != 0,
            updated_unix_ns: u64_at(88),
        },
        segment_file: (u64_at(72), u64_at(80)),
        segment_size: u64_at(64),
        segment_created_unix_ns: u64_at(56),
    })
}

/// Reads a beacon file through `file`: its exact contents if it has exactly [`BEACON_LEN`] bytes.
fn read_exact_beacon(file: &File) -> io::Result<Option<[u8; BEACON_LEN]>> {
    // One byte more than a beacon, to see a longer file without trusting `fstat` alone.
    let mut buf = [0u8; BEACON_LEN + 1];
    let mut filled = 0;
    while filled < buf.len() {
        match file.read_at(&mut buf[filled..], filled as u64) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    if filled != BEACON_LEN {
        return Ok(None);
    }
    let mut out = [0u8; BEACON_LEN];
    out.copy_from_slice(&buf[..BEACON_LEN]);
    Ok(Some(out))
}

/// Probes a beacon's lock with a non-blocking shared `flock`, released at once. Shared probes never conflict with
/// each other, and only a beacon's owner ever holds the exclusive lock.
fn probe(file: &File) -> BeaconState {
    match rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockShared) {
        Ok(()) => {
            pause(Pause::ObserverHoldsProbe);
            let _ = rustix::fs::flock(file, rustix::fs::FlockOperation::Unlock);
            BeaconState::Stale
        }
        Err(e) if e == rustix::io::Errno::WOULDBLOCK => BeaconState::Live,
        Err(_) => BeaconState::Uncertain,
    }
}

/// Opens a possible beacon without blocking and without following a symlink.
fn open_beacon(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

/// The exclusive lock on a beacon directory that creators hold while they create or take over a beacon there.
struct DirLock {
    _dir: File,
}

impl DirLock {
    fn acquire(dir: &Path) -> Result<DirLock> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(dir)
            .map_err(|e| Error::io("open reader beacon directory", dir, e))?;
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(e) if e == rustix::io::Errno::WOULDBLOCK => {
                pause(Pause::CreatorWaitsForDirectoryLock);
                rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
                    .map_err(|e| Error::io("lock reader beacon directory", dir, e.into()))?;
            }
            Err(e) => return Err(Error::io("lock reader beacon directory", dir, e.into())),
        }
        Ok(DirLock { _dir: file })
    }
}

/// The name of a creator's temporary beacon, removed unless the beacon was published.
struct Temporary {
    path: PathBuf,
    published: bool,
}

impl Temporary {
    /// Creates a temporary beacon file exclusively and locks it.
    fn create(dir: &Path, name: &str) -> Result<(Temporary, File)> {
        let pid = std::process::id();
        for n in 0u32.. {
            let path = dir.join(format!(".{name}.{pid}.{n}{TMP_SUFFIX}"));
            // `create_new` is O_CREAT | O_EXCL: it never follows a symlink and never opens an existing file.
            let file = match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_CLOEXEC)
                .open(&path)
            {
                Ok(file) => file,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && n < 1000 => continue,
                Err(e) => return Err(Error::io("create reader beacon", &path, e)),
            };
            let tmp = Temporary {
                path,
                published: false,
            };
            rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
                .map_err(|e| Error::io("lock reader beacon", &tmp.path, e.into()))?;
            return Ok((tmp, file));
        }
        unreachable!("the loop returns")
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Publishes the temporary beacon at `path`: links it there if the name is free, or replaces a stale beacon. Runs
/// under the directory lock, so no other creator changes the name meanwhile.
fn publish(tmp: &Path, path: &Path, identity: Identity) -> Result<()> {
    for _ in 0..PUBLISH_ATTEMPTS {
        match fs::hard_link(tmp, path) {
            Ok(()) => {
                // The beacon is published; a leftover temporary name is a harmless second link, removed by a later
                // creator.
                let _ = fs::remove_file(tmp);
                return Ok(());
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(Error::io("publish reader beacon", path, e)),
        }
        match check_existing(path)? {
            Existing::Changed => continue,
            Existing::Stale => {
                pause(Pause::CreatorFoundStale);
                fs::rename(tmp, path)
                    .map_err(|e| Error::io("replace stale reader beacon", path, e))?;
                if lstat_identity(path) != Some(identity) {
                    // Only something outside the protocol can have replaced it since.
                    return Err(Error::io(
                        "replace stale reader beacon",
                        path,
                        io::Error::other("the name was replaced during publication"),
                    ));
                }
                return Ok(());
            }
        }
    }
    Err(Error::io(
        "publish reader beacon",
        path,
        io::Error::other("the name kept changing during publication"),
    ))
}

enum Existing {
    /// Not the same file by the time it was checked: try again.
    Changed,
    /// A stale beacon: may be replaced.
    Stale,
}

/// Checks the file at `path` before a takeover: it must be a regular file of exactly [`BEACON_LEN`] bytes that
/// decodes, and unlocked. The lock is probed first: if it is held, the file is someone's live beacon, whatever its
/// contents show at this instant; if it is free, nobody can write it any more, so its contents are final.
fn check_existing(path: &Path) -> Result<Existing> {
    let not_a_beacon = || {
        Error::InvalidConfig(format!(
            "{} exists and is not a stale reader beacon; refusing to replace it",
            path.display()
        ))
    };
    let file = match open_beacon(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Existing::Changed),
        Err(e)
            if e.raw_os_error() == Some(libc::ELOOP) || e.raw_os_error() == Some(libc::ENXIO) =>
        {
            return Err(not_a_beacon());
        }
        Err(e) => return Err(Error::io("open existing reader beacon", path, e)),
    };
    pause(Pause::CreatorOpenedExisting);
    let meta = file
        .metadata()
        .map_err(|e| Error::io("stat existing reader beacon", path, e))?;
    if !meta.is_file() {
        return Err(not_a_beacon());
    }
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockShared) {
        Ok(()) => {
            let _ = rustix::fs::flock(&file, rustix::fs::FlockOperation::Unlock);
        }
        Err(e) if e == rustix::io::Errno::WOULDBLOCK => {
            return Err(Error::BeaconInUse {
                path: path.to_path_buf(),
            });
        }
        Err(e) => return Err(Error::io("probe existing reader beacon", path, e.into())),
    }
    let contents =
        read_exact_beacon(&file).map_err(|e| Error::io("read existing reader beacon", path, e))?;
    if contents.and_then(|b| decode(&b)).is_none() {
        return Err(not_a_beacon());
    }
    if lstat_identity(path) != Some((meta.dev(), meta.ino())) {
        return Ok(Existing::Changed);
    }
    Ok(Existing::Stale)
}

/// Removes temporary files that creators which died before publishing left behind. Runs under the directory lock,
/// so an unlocked temporary file can't belong to a creator that is still working. Best effort.
fn remove_leftovers(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_temporary_name(name) {
            continue;
        }
        let path = entry.path();
        let Ok(file) = open_beacon(&path) else {
            continue;
        };
        if file.metadata().is_ok_and(|m| m.is_file()) && probe(&file) == BeaconState::Stale {
            let _ = fs::remove_file(&path);
        }
    }
}

/// `.<name>.<pid>.<n>.beacon-tmp`
fn is_temporary_name(name: &str) -> bool {
    let Some(stem) = name
        .strip_prefix('.')
        .and_then(|n| n.strip_suffix(TMP_SUFFIX))
    else {
        return false;
    };
    let mut parts = stem.rsplitn(3, '.');
    let (Some(n), Some(pid), Some(beacon)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    digits(n) && digits(pid) && valid_name(beacon)
}

/// Lists the beacons in `dir`. A directory that can't be listed yields no beacons and its state; an entry that can't
/// be read is reported as [`BeaconState::Unreadable`]; entries that aren't regular files are skipped.
pub(super) fn observe_all(dir: &Path) -> (BeaconDirState, Vec<Observed>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return (BeaconDirState::Missing, Vec::new());
        }
        Err(e) => return (BeaconDirState::Unusable(e.kind()), Vec::new()),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => return (BeaconDirState::Unusable(e.kind()), Vec::new()),
        };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !valid_name(&name) || reserved(&name) {
            continue;
        }
        // Skip what is known not to be a regular file without opening it (directories, symlinks, FIFOs, sockets);
        // `open_beacon` checks again, race-free.
        if entry.file_type().is_ok_and(|t| !t.is_file()) {
            continue;
        }
        if let Some(observed) = observe(entry.path(), name) {
            out.push(observed);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    (BeaconDirState::Listed, out)
}

/// Observes one beacon: its contents, its file identity and its lock state, all for the same file, and checks that
/// the name still names that file afterwards (retrying if a creator replaced it meanwhile). `None` if the name no
/// longer exists or isn't a regular file.
fn observe(path: PathBuf, name: String) -> Option<Observed> {
    let unreadable = |path: PathBuf, name: String| Observed {
        name,
        path,
        state: BeaconState::Unreadable,
        payload: None,
    };
    let mut last = None;
    for _ in 0..OBSERVE_ATTEMPTS {
        let file = match open_beacon(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            // A symlink (O_NOFOLLOW) or a socket.
            Err(e)
                if e.raw_os_error() == Some(libc::ELOOP)
                    || e.raw_os_error() == Some(libc::ENXIO) =>
            {
                return None;
            }
            Err(_) => return Some(unreadable(path, name)),
        };
        let meta = match file.metadata() {
            Ok(meta) => meta,
            Err(_) => return Some(unreadable(path, name)),
        };
        if !meta.is_file() {
            return None;
        }
        pause(Pause::ObserverOpened);
        // A torn copy of an update (the owner rewrites it with one `pwrite`) fails the checksum: read again, and
        // a few more times while a live owner holds the file (it may be rewriting it right now).
        let read = |file: &File| read_exact_beacon(file).map(|b| b.and_then(|b| decode(&b)));
        let Ok(mut payload) = read(&file) else {
            return Some(unreadable(path, name));
        };
        let mut attempts = 1;
        while payload.is_none()
            && (attempts < 2 || attempts < 8 && probe(&file) == BeaconState::Live)
        {
            match read(&file) {
                Ok(again) => payload = again,
                Err(_) => return Some(unreadable(path, name)),
            }
            attempts += 1;
        }
        pause(Pause::ObserverRead);
        let state = match payload {
            None => BeaconState::Invalid,
            Some(_) => probe(&file),
        };
        let observed = Observed {
            name: name.clone(),
            path: path.clone(),
            state,
            payload,
        };
        if lstat_identity(&path) == Some((meta.dev(), meta.ino())) {
            return Some(observed);
        }
        // Replaced (or removed) while we looked: what we saw is true of a file that is no longer there.
        last = Some(observed);
    }
    // Still being replaced after every attempt: report the last file seen, which held the name moments ago.
    last
}

/// Whether `name` is something a beacon may be named (for the CLI and tests).
#[cfg(test)]
pub(super) fn name_allowed(name: &str) -> bool {
    check_name(name).is_ok()
}

#[cfg(test)]
pub(super) fn temporary_name(name: &str) -> bool {
    is_temporary_name(name)
}
