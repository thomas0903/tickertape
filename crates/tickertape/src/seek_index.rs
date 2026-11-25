//! Seek indexes: optional, rebuildable files that let [`Reader::seek`](crate::Reader::seek) start in the middle of a
//! sealed segment (ADR 0045).
//!
//! An index is one file per sealed segment in the subdirectory [`DIR_NAME`] of the journal. Readers use it only with
//! [`ReaderConfig::seek_index`](crate::ReaderConfig::seek_index), never write it, and scan as without one whenever
//! anything about it is in doubt; no seek fails because of an index. [`build`] (`tickertape index`) writes them, for
//! segments that no recovery will change: those before the segment of a durable checkpoint marker, or every sealed
//! segment of a directory without a marker and without a running writer (an archive). It can run beside a live writer
//! that checkpoints. The writer never writes an index; when it opens it removes those of the segments that recovery
//! may change, and retention removes a segment's index with the segment. The files can be deleted at any time.
//!
//! The file format and everything a wrong index can and cannot do are in `docs/format.md` (Seek index),
//! `docs/crash-contract.md` (C15) and ADR 0045.

use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

use rustix::fs::{AtFlags, FlockOperation, Gid, Mode};

use crate::error::{CorruptReason, Error, Result};
use crate::fault::{self, Point};
use crate::format::{self, SEGMENT_HEADER_LEN, SEGMENT_SIZE_UNIT, SegmentView, Slot};
use crate::{inspect, journal, retention};

#[doc(hidden)]
pub mod codec;
pub(crate) mod dir;
pub(crate) mod lookup;
#[cfg(test)]
mod model;

use codec::{Encoder, Entry};
use dir::IndexDir;
use lookup::Rejected;

/// The subdirectory of the journal directory that holds the seek indexes.
pub const DIR_NAME: &str = "seek-index";

/// How often a build of one segment starts again because a writer opened the journal and removed its temporary file.
const RESTARTS: u32 = 3;

/// What [`build`] or [`check`] found or did for one segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IndexState {
    /// [`build`] wrote the index.
    Built,
    /// The index is right. From [`build`]: it was there and was kept (the index, the seal and the last entry were
    /// checked). From [`check`]: every entry was compared with the segment.
    Valid,
    /// [`check`]: there is no index, and there could be one.
    Missing,
    /// There is a file that readers do not use because of its content (another version, damaged, not a regular file,
    /// stale). [`build`] reports this only for a file of a newer version, which it leaves alone.
    Ignored,
    /// [`check`]: a reader would use the index, and an entry in it is not a record of the segment's chain.
    Wrong,
    /// The segment needs no index: no record starts outside its first block.
    NotNeeded,
    /// The segment does not end in a valid seal (it is still being written, or is damaged); see the reason.
    NotSealed,
    /// The segment is not before the segment of the checkpoint marker, so recovery may still change it.
    NotCovered,
    /// [`build`]: the segment belongs to another user, or the index cannot be given the segment's group.
    NotPermitted,
    /// The segment was removed or replaced meanwhile.
    Vanished,
    /// Reading the segment or its index, or writing the index, failed (the reason says how). The other segments are
    /// handled all the same.
    Failed,
}

/// One segment in a [`BuildReport`] or [`CheckReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SegmentIndex {
    /// The segment's first sequence number.
    pub base_seq: u64,
    /// What was found or done.
    pub state: IndexState,
    /// [`check`]: the content is right, but readers ignore the file because of its owner or mode (the reason says
    /// which). [`build`] writes such a file anew.
    pub ignored_by_readers: bool,
    /// A human-readable explanation, where the state alone does not say enough.
    pub reason: Option<String>,
}

impl SegmentIndex {
    fn new(base_seq: u64, (state, reason): Outcome) -> SegmentIndex {
        SegmentIndex {
            base_seq,
            state,
            ignored_by_readers: false,
            reason,
        }
    }
}

/// What [`build`] did, one element per segment, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BuildReport {
    /// The segments.
    pub segments: Vec<SegmentIndex>,
}

/// What [`check`] found, one element per segment, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckReport {
    /// The segments.
    pub segments: Vec<SegmentIndex>,
    /// Unfinished indexes: what a build that was killed left, or one running now. Nobody reads them. The next open of a
    /// writer removes them, and so does [`remove`], which is the only way where no writer opens (an archive).
    pub unfinished: u64,
}

/// A segment's state and the reason for it.
type Outcome = (IndexState, Option<String>);

fn because(state: IndexState, reason: impl ToString) -> Outcome {
    (state, Some(reason.to_string()))
}

/// Writes the missing and outdated seek indexes of the journal in `dir`.
///
/// Eligible are the sealed segments before the segment of the checkpoint marker, which is made durable first (one
/// sync; on macOS `F_FULLFSYNC`); without a valid marker, every sealed segment, provided no writer has the journal
/// open. An index that is right is kept, unless `rebuild`; so is (unless `rebuild`) one of a newer version. Every
/// segment an index is written for is read and validated from its first record to its seal. The indexes are not
/// synced: a torn one is recognised, and written again by the next run.
///
/// The writer is not slowed down or locked out, except that without a marker a writer opening at that very moment can
/// get [`Error::Locked`].
///
/// The subdirectory [`DIR_NAME`] is created if needed, with the journal directory's group and mode, and only by the
/// owner of the journal directory. The caller must be the owner of the journal's files; root gets no exception.
///
/// # Errors
///
/// [`Error::NotFound`] if the journal has no segments; [`Error::Corrupt`] for a checkpoint marker whose segment or
/// position does not exist; [`Error::Locked`] if there is no marker and a writer has the journal open; [`Error::Io`]
/// if the journal directory or the marker cannot be read, the marker cannot be synced, or the index directory may not be
/// used or created. Indexes written before the error stay. What fails for one segment does not end the run
/// ([`IndexState::Failed`]: an I/O error, or writers that kept opening the journal during its build), and a segment whose
/// header or records are damaged is no failure at all ([`IndexState::NotSealed`]).
pub fn build(dir: impl AsRef<Path>, rebuild: bool) -> Result<BuildReport> {
    let dir = dir.as_ref();
    let segments = journal::list_segments(dir)?;
    if segments.is_empty() {
        return Err(Error::NotFound { dir: dir.into() });
    }
    let root = open_root(dir)?;
    // Only to leave the segments alone that are not covered now; what counts is read behind the fence.
    let marker = settled(dir, || retention::checkpoint(dir))?;
    let mut builder = Builder {
        dir,
        indexes: open_index_dir(dir, &root)?,
        root,
        rebuild,
        euid: rustix::process::geteuid().as_raw(),
        synced: None,
    };
    let mut report = BuildReport::default();
    for (base, path) in segments {
        let outcome = if marker.is_some_and(|m| base >= m.segment_base_seq) {
            (IndexState::NotCovered, None)
        } else {
            match builder.segment(base, &path) {
                Ok(outcome) => outcome,
                Err(Stop::Segment(e)) => because(IndexState::Failed, e),
                Err(Stop::Run(e)) => return Err(e),
            }
        };
        report.segments.push(SegmentIndex::new(base, outcome));
    }
    Ok(report)
}

struct Builder<'a> {
    dir: &'a Path,
    /// The journal directory.
    root: File,
    /// `seek-index/`, once it is known to be usable by this process.
    indexes: Option<IndexDir>,
    rebuild: bool,
    euid: u32,
    /// The marker slot last made durable.
    synced: Option<format::CheckpointSlot>,
}

/// A temporary index: the fence of a build. Removed when dropped, unless it was renamed into place.
struct Fence<'d> {
    dir: &'d IndexDir,
    name: String,
    file: File,
    published: bool,
}

impl Fence<'_> {
    /// Creates the file under the first free name from `n` on.
    fn create(dir: &IndexDir, base: u64, mut n: u64) -> io::Result<Fence<'_>> {
        loop {
            let name = dir::tmp_name(base, std::process::id(), n);
            match dir.create(&name) {
                Ok(file) => {
                    return Ok(Fence {
                        dir,
                        name,
                        file,
                        published: false,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => n = n.wrapping_add(1),
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for Fence<'_> {
    fn drop(&mut self) {
        if !self.published {
            let _ = self.dir.unlink(self.name.as_ref());
        }
    }
}

/// Why a build stopped: for one segment, or for all of them.
enum Stop {
    Segment(Error),
    Run(Error),
}

impl From<Error> for Stop {
    fn from(e: Error) -> Stop {
        Stop::Segment(e)
    }
}

impl Builder<'_> {
    fn segment(&mut self, base: u64, path: &Path) -> std::result::Result<Outcome, Stop> {
        let (file, header) = match open_segment(base, path)? {
            Ok(opened) => opened,
            Err(outcome) => return Ok(outcome),
        };
        if header.segment_size <= SEGMENT_SIZE_UNIT {
            return Ok((IndexState::NotNeeded, None));
        }
        if let Some(indexes) = self.indexes.as_ref().filter(|_| !self.rebuild) {
            if is_current(indexes, &file, base) {
                return Ok((IndexState::Valid, None));
            }
            if let Some(v) = lookup::version(indexes, base).filter(|v| *v > codec::VERSION) {
                return Ok(because(
                    IndexState::Ignored,
                    format_args!("version {v} of the index file, newer than this program; kept"),
                ));
            }
        }
        if !self.owns(&file, path)? {
            return Ok(because(IndexState::NotPermitted, NOT_OWNED));
        }
        drop(file);
        let indexes = match self.indexes.take() {
            Some(indexes) => indexes,
            None => create_index_dir(self.dir, &self.root, self.euid).map_err(Stop::Run)?,
        };
        let mut outcome = None;
        for _ in 0..=RESTARTS {
            outcome = self.attempt(&indexes, base, path).transpose();
            if outcome.is_some() {
                break;
            }
        }
        self.indexes = Some(indexes);
        outcome.unwrap_or_else(|| {
            Err(Stop::Segment(Error::io(
                "build seek index",
                path,
                io::Error::other("writers kept opening the journal during the build; run it again"),
            )))
        })
    }

    fn owns(&self, segment: &File, path: &Path) -> Result<bool> {
        let meta = segment
            .metadata()
            .map_err(|e| Error::io("stat segment", path, e))?;
        Ok(meta.uid() == self.euid)
    }

    /// Steps 2 to 6. `None`: the temporary file is gone, so a writer has opened the journal meanwhile; start again.
    fn attempt(
        &mut self,
        indexes: &IndexDir,
        base: u64,
        path: &Path,
    ) -> std::result::Result<Option<Outcome>, Stop> {
        let index_path = self.dir.join(DIR_NAME).join(dir::index_name(base));
        let io_error = |op, e| Error::io(op, &index_path, e);
        // The fence comes before everything below (ADR 0045 → Why the fence is enough).
        let mut fence = Fence::create(indexes, base, crate::clock::realtime_ns())
            .map_err(|e| io_error("create seek index", e))?;
        fault::hook(Point::SeekIndexFence);
        let marker = settled(self.dir, || {
            retention::durable_checkpoint(self.dir, &mut self.synced)
        })
        .map_err(Stop::Run)?;
        match marker {
            Some(marker) if base >= marker.segment_base_seq => {
                return Ok(Some((IndexState::NotCovered, None)));
            }
            Some(_) => {}
            None => probe_lock(self.dir).map_err(Stop::Run)?,
        }
        fault::hook(Point::SeekIndexEligible);

        // Kept open until the end, so that no other file can get its inode.
        let (segment, header) = match open_segment(base, path)? {
            Ok(opened) => opened,
            Err(outcome) => return Ok(Some(outcome)),
        };
        let meta = segment
            .metadata()
            .map_err(|e| Error::io("stat segment", path, e))?;
        if meta.uid() != self.euid {
            return Ok(Some(because(IndexState::NotPermitted, NOT_OWNED)));
        }
        if let Err(e) = set_group(&fence.file, meta.gid()) {
            if e.kind() == io::ErrorKind::PermissionDenied {
                return Ok(Some(because(
                    IndexState::NotPermitted,
                    "the index cannot be given the segment's group",
                )));
            }
            return Err(io_error("set the group of seek index", e).into());
        }
        let mut encoder = Encoder::new(header.segment_size);
        let end = walk(&segment, path, &header, |entry| {
            if encoder.wants(entry.offset) {
                encoder.push(entry);
            }
        })?;
        let (seal_offset, next_seq) = match end {
            WalkEnd::Sealed { offset, next_seq } => (offset, next_seq),
            WalkEnd::Other(reason) => return Ok(Some(because(IndexState::NotSealed, reason))),
        };
        let mut raw = [0; SEGMENT_HEADER_LEN];
        segment
            .read_exact_at(&mut raw, 0)
            .map_err(|e| Error::io("read segment header", path, e))?;
        let Some(bytes) = encoder.finish(&raw, seal_offset, next_seq) else {
            return Ok(Some((IndexState::NotNeeded, None)));
        };

        fault::inject(Point::SeekIndexWrite)
            .and_then(|()| fence.file.write_all_at(&bytes, 0))
            .map_err(|e| io_error("write seek index", e))?;
        // The group is the segment's since before the walk. Independent of the umask.
        set_mode(&fence.file, meta.mode() & 0o444 | 0o200)
            .map_err(|e| io_error("set the mode of seek index", e))?;
        fault::hook(Point::SeekIndexBeforeRename);
        match indexes.rename(&fence.name, &dir::index_name(base)) {
            Ok(()) => fence.published = true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_error("publish seek index", e).into()),
        }
        fault::hook(Point::SeekIndexAfterRename);
        let identity = (meta.dev(), meta.ino());
        if journal::path_identity(path)? != Some(identity) {
            unlink_if_same(indexes, base, &fence.file);
            return Ok(Some(because(
                IndexState::Vanished,
                "the segment was removed or replaced while its index was built",
            )));
        }
        Ok(Some((IndexState::Built, None)))
    }
}

/// `read`, which reads the marker and opens its segment, again if the marker moved on meanwhile: the writer's retention
/// can then have removed the segment in between, which is no dangling marker.
fn settled<T>(dir: &Path, mut read: impl FnMut() -> Result<T>) -> Result<T> {
    let mut attempts = 8;
    loop {
        let before = inspect::read_checkpoint(dir)?;
        match read() {
            Err(Error::Corrupt {
                reason: CorruptReason::DanglingCheckpoint(_),
                ..
            }) if attempts > 0 && inspect::read_checkpoint(dir)? != before => attempts -= 1,
            result => return result,
        }
    }
}

const NOT_OWNED: &str = "the segment belongs to another user";

/// Opens a segment read-only and validates its header; `Err` with the outcome if there is none to index.
fn open_segment(
    base: u64,
    path: &Path,
) -> Result<std::result::Result<(File, format::SegmentHeader), Outcome>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(Err((IndexState::Vanished, None)));
        }
        Err(e) => return Err(Error::io("open segment", path, e)),
    };
    match journal::read_header(&file, path, base) {
        Ok(header) => Ok(Ok((file, header))),
        Err(e @ Error::Io { .. }) => Err(e),
        Err(e) => Ok(Err(because(IndexState::NotSealed, e))),
    }
}

/// How a segment ends.
enum WalkEnd {
    Sealed { offset: u64, next_seq: u64 },
    Other(String),
}

/// Reads a segment from its first record, in windows of at most 1 MiB, validating every record, and passes each to
/// `record` before the next is read.
fn walk(
    segment: &File,
    path: &Path,
    header: &format::SegmentHeader,
    mut record: impl FnMut(Entry),
) -> Result<WalkEnd> {
    let view = journal::PreadView::new(segment, header.segment_size, journal::SCAN_WINDOW);
    let (mut offset, mut seq) = (SEGMENT_HEADER_LEN as u64, header.base_seq);
    let end = loop {
        // Before the slot is validated: a record read in pieces moves the window away from its first bytes.
        let crc = view
            .with_bytes(offset, 8, |b| u32::from_le_bytes([b[4], b[5], b[6], b[7]]))
            .unwrap_or(0);
        match format::read_slot(&view, offset, seq) {
            Ok(Slot::Record {
                len, next_offset, ..
            }) => {
                record(Entry {
                    seq,
                    offset,
                    len,
                    crc,
                });
                offset = next_offset;
                match seq.checked_add(1) {
                    Some(next) => seq = next,
                    None => break WalkEnd::Other("sequence numbers are exhausted".into()),
                }
            }
            Ok(Slot::Seal) => {
                break WalkEnd::Sealed {
                    offset,
                    next_seq: seq,
                };
            }
            Ok(Slot::Empty) => {
                break WalkEnd::Other(format!("no seal: the records end at offset {offset}"));
            }
            Err(reason) => break WalkEnd::Other(format!("at offset {offset}: {reason}")),
        }
    };
    match view.take_error() {
        Some(e) => Err(Error::io("read segment", path, e)),
        None => Ok(end),
    }
}

/// Step 1 of a build: whether the index of the segment open as `segment` is one to keep. A repair changes a suffix of
/// a segment, so a stale index is stale at its last entry.
fn is_current(indexes: &IndexDir, segment: &File, base: u64) -> bool {
    let Ok(loaded) = lookup::load(indexes, segment, base, false) else {
        return false;
    };
    loaded.index().is_ok_and(|index| {
        matches!(lookup::sealed(segment, &index), Ok(true))
            && index
                .entries()
                .next_back()
                .is_some_and(|last| matches!(lookup::in_place(segment, &last), Ok(true)))
    })
}

/// Without a marker, nothing says which segments recovery may change, so no writer may be open. The lock is probed,
/// not held: a writer that opens later finds the fence (ADR 0045 → Why the fence is enough).
fn probe_lock(dir: &Path) -> Result<()> {
    let path = dir.join(journal::LOCK_FILE);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::io("open lock file", &path, e)),
    };
    let meta = file
        .metadata()
        .map_err(|e| Error::io("stat lock file", &path, e))?;
    if !meta.is_file() {
        return Err(Error::io(
            "probe lock file",
            &path,
            io::Error::other("not a regular file"),
        ));
    }
    match rustix::io::retry_on_intr(|| {
        rustix::fs::flock(&file, FlockOperation::NonBlockingLockShared)
    }) {
        Ok(()) => {}
        Err(rustix::io::Errno::WOULDBLOCK) => {
            let mut text = [0; 10];
            let n = file.read_at(&mut text, 0).unwrap_or(0);
            return Err(Error::Locked {
                dir: dir.into(),
                pid: std::str::from_utf8(&text[..n])
                    .ok()
                    .and_then(|t| t.trim().parse().ok()),
            });
        }
        Err(e) => return Err(Error::io("probe lock file", &path, e.into())),
    }
    fault::hook(Point::SeekIndexProbe);
    // Explicitly: a child that another thread forked meanwhile holds a copy of the descriptor.
    rustix::fs::flock(&file, FlockOperation::Unlock)
        .map_err(|e| Error::io("release lock file", &path, e.into()))
}

fn set_group(file: &File, gid: u32) -> io::Result<()> {
    rustix::fs::fchown(file, None, Some(Gid::from_raw(gid))).map_err(io::Error::from)
}

fn set_mode(file: &File, mode: u32) -> io::Result<()> {
    let mode = Mode::from_raw_mode((mode & 0o777) as rustix::fs::RawMode);
    rustix::fs::fchmod(file, mode).map_err(io::Error::from)
}

/// Removes the index of segment `base` if it is still the file open as `file`.
fn unlink_if_same(indexes: &IndexDir, base: u64, file: &File) {
    let name = dir::index_name(base);
    let identity = |f: &File| f.metadata().ok().map(|m| (m.dev(), m.ino()));
    if let (Ok(there), Some(ours)) = (indexes.open_file(&name), identity(file)) {
        if identity(&there) == Some(ours) {
            let _ = indexes.unlink(name.as_ref());
        }
    }
}

fn open_root(dir: &Path) -> Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(dir)
        .map_err(|e| Error::io("open journal directory", dir, e))
}

fn refused(dir: &Path, kind: io::ErrorKind, why: &'static str) -> Error {
    Error::io(
        "use seek index directory",
        dir.join(DIR_NAME),
        io::Error::new(kind, why),
    )
}

/// Opens an existing `seek-index/` for a builder, or `None` if there is none.
fn open_index_dir(dir: &Path, root: &File) -> Result<Option<IndexDir>> {
    match IndexDir::open_at(root) {
        Ok(indexes) => {
            check_index_dir(dir, &indexes, rustix::process::geteuid().as_raw())?;
            Ok(Some(indexes))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP)) => Err(refused(
            dir,
            io::ErrorKind::NotADirectory,
            "the name exists and is not a directory (a symbolic link counts as that)",
        )),
        Err(e) => Err(Error::io(
            "open seek index directory",
            dir.join(DIR_NAME),
            e,
        )),
    }
}

/// A builder uses the directory only if it is its own and not world-writable.
fn check_index_dir(dir: &Path, indexes: &IndexDir, euid: u32) -> Result<()> {
    let meta = indexes
        .file()
        .metadata()
        .map_err(|e| Error::io("stat seek index directory", dir.join(DIR_NAME), e))?;
    if meta.uid() != euid || meta.mode() & 0o002 != 0 {
        return Err(refused(
            dir,
            io::ErrorKind::PermissionDenied,
            "it must belong to the user running this and not be writable by everyone; create it with the journal \
             directory's owner, group and mode",
        ));
    }
    Ok(())
}

/// Creates `seek-index/` with the journal directory's group and mode.
fn create_index_dir(dir: &Path, root: &File, euid: u32) -> Result<IndexDir> {
    let path = dir.join(DIR_NAME);
    let io_error = |op| {
        let path = path.clone();
        move |e: io::Error| Error::io(op, path, e)
    };
    let meta = root
        .metadata()
        .map_err(|e| Error::io("stat journal directory", dir, e))?;
    let gid = meta.gid();
    let own_group = rustix::process::getegid().as_raw() == gid
        || rustix::process::getgroups()
            .map_err(|e| Error::io("read the process's groups", dir, e.into()))?
            .iter()
            .any(|g| g.as_raw() == gid);
    // Asked to make it, nobody else's directory, or one whose group could not be given to it, gets one.
    let why = if meta.uid() != euid {
        Some(
            "the journal directory belongs to another user: its owner must create it, with the journal directory's \
             owner, group and mode",
        )
    } else if meta.mode() & 0o002 != 0 {
        Some(
            "the journal directory is writable by everyone: create it by hand, with the journal directory's owner and \
             group, not writable by everyone",
        )
    } else if !own_group {
        Some(
            "this user is not in the journal directory's group: create it by hand, with the journal directory's owner, \
             group and mode",
        )
    } else {
        None
    };
    if let Some(why) = why {
        return Err(refused(dir, io::ErrorKind::PermissionDenied, why));
    }
    let mode = meta.mode() & 0o777;
    let raw = Mode::from_raw_mode(mode as rustix::fs::RawMode);
    let made = match rustix::fs::mkdirat(root, DIR_NAME, raw) {
        Ok(()) => true,
        Err(rustix::io::Errno::EXIST) => false,
        Err(e) => return Err(io_error("create seek index directory")(e.into())),
    };
    let opened = if made {
        IndexDir::open_at(root).map_err(io_error("open seek index directory"))
    } else {
        // Another builder made it meanwhile.
        match open_index_dir(dir, root)? {
            Some(indexes) => return Ok(indexes),
            None => Err(refused(
                dir,
                io::ErrorKind::NotFound,
                "it was removed meanwhile",
            )),
        }
    };
    let ready = opened.and_then(|indexes| {
        check_index_dir(dir, &indexes, euid)?;
        fault::inject(Point::SeekIndexDirMode)
            .and_then(|()| set_group(indexes.file(), gid))
            .and_then(|()| set_mode(indexes.file(), mode))
            .map_err(io_error("set the group and mode of seek index directory"))?;
        Ok(indexes)
    });
    if ready.is_err() && made {
        // The only case in which tickertape removes the directory. It is empty.
        let _ = rustix::fs::unlinkat(root, DIR_NAME, AtFlags::REMOVEDIR);
    }
    ready
}

/// Compares the seek indexes of the journal in `dir` with its segments, changing nothing.
///
/// Every segment that has an index a reader would use is read and validated from its first record to its seal, and
/// every entry compared with it; so is every segment without one that could have one. Unlike [`build`], which keeps an
/// index that passes a reader's checks, this finds one that was forged ([`IndexState::Wrong`]). The owner and the mode
/// of an index do not decide its state: a right one that readers ignore for them is marked
/// ([`SegmentIndex::ignored_by_readers`]). If a segment's chain ends before its seal, the entries after that place
/// cannot be compared, and the state is [`IndexState::NotSealed`] unless one before it is wrong.
///
/// # Errors
///
/// [`Error::NotFound`] if the journal has no segments; [`Error::Corrupt`] for a checkpoint marker whose segment or
/// position does not exist; [`Error::Io`] if the journal directory, its `seek-index/` or the marker cannot be read. What
/// cannot be read of one segment does not end the run ([`IndexState::Failed`]).
pub fn check(dir: impl AsRef<Path>) -> Result<CheckReport> {
    let dir = dir.as_ref();
    let segments = journal::list_segments(dir)?;
    if segments.is_empty() {
        return Err(Error::NotFound { dir: dir.into() });
    }
    let marker = settled(dir, || retention::checkpoint(dir))?;
    let indexes = IndexDir::open(dir)
        .map_err(|e| Error::io("open seek index directory", dir.join(DIR_NAME), e))?;
    let mut report = CheckReport::default();
    if let Some(indexes) = &indexes {
        let (names, listed) = indexes.list();
        listed.map_err(|e| Error::io("list seek index directory", dir.join(DIR_NAME), e))?;
        let unfinished = names.iter().filter(|(_, n)| *n == dir::Name::Temporary);
        report.unfinished = unfinished.count() as u64;
    }
    for (base, path) in segments {
        let covered = marker.is_none_or(|m| base < m.segment_base_seq);
        let checked = check_segment(dir, indexes.as_ref(), base, &path, covered);
        report.segments.push(
            checked.unwrap_or_else(|e| SegmentIndex::new(base, because(IndexState::Failed, e))),
        );
    }
    Ok(report)
}

fn check_segment(
    dir: &Path,
    indexes: Option<&IndexDir>,
    base: u64,
    path: &Path,
    covered: bool,
) -> Result<SegmentIndex> {
    let done = |outcome| Ok(SegmentIndex::new(base, outcome));
    let (segment, header) = match open_segment(base, path)? {
        Ok(opened) => opened,
        Err(outcome) => return done(outcome),
    };
    let loaded = match indexes.map(|indexes| lookup::load(indexes, &segment, base, true)) {
        None | Some(Err(Rejected::Missing)) => None,
        Some(Err(Rejected::Unusable(why) | Rejected::Permissions(why))) => {
            return done(because(IndexState::Ignored, why));
        }
        Some(Err(Rejected::Io(e))) => {
            let index_path = dir.join(DIR_NAME).join(dir::index_name(base));
            return Err(Error::io("read seek index", index_path, e));
        }
        Some(Ok(loaded)) => Some(loaded),
    };
    let Some(loaded) = loaded else {
        if header.segment_size <= SEGMENT_SIZE_UNIT {
            return done((IndexState::NotNeeded, None));
        }
        if !covered {
            return done((IndexState::NotCovered, None));
        }
        let encoder = Encoder::new(header.segment_size);
        let mut needed = false;
        let end = walk(&segment, path, &header, |e| {
            needed |= encoder.wants(e.offset)
        })?;
        return done(match end {
            WalkEnd::Sealed { .. } if needed => (IndexState::Missing, None),
            WalkEnd::Sealed { .. } => (IndexState::NotNeeded, None),
            WalkEnd::Other(reason) => because(IndexState::NotSealed, reason),
        });
    };
    if header.segment_size <= SEGMENT_SIZE_UNIT {
        return done(because(
            IndexState::Ignored,
            "readers use no index for a segment this small",
        ));
    }
    let index = match loaded.index() {
        Ok(index) => index,
        Err(why) => return done(because(IndexState::Ignored, why)),
    };
    let stat = |e| Error::io("read segment", path, e);
    if !lookup::sealed(&segment, &index).map_err(stat)? {
        return done(because(
            IndexState::Ignored,
            "the segment has no seal where the index says",
        ));
    }
    let mut entries = index.entries().peekable();
    // Entries no record of the chain matches.
    let mut off_chain: Vec<Entry> = Vec::new();
    let end = walk(&segment, path, &header, |record| {
        while let Some(entry) = entries.next_if(|e| e.offset <= record.offset) {
            if entry != record {
                off_chain.push(entry);
            }
        }
    })?;
    let sealed = match end {
        WalkEnd::Sealed { offset, next_seq } => {
            off_chain.extend(entries);
            Ok((offset, next_seq))
        }
        WalkEnd::Other(reason) => Err(reason),
    };
    // Would a reader land there?
    for entry in &off_chain {
        let lands = lookup::in_place(&segment, entry).map_err(stat)? && {
            let view = journal::PreadView::new(&segment, header.segment_size, journal::SCAN_WINDOW);
            let slot = format::read_slot(&view, entry.offset, entry.seq);
            if let Some(e) = view.take_error() {
                return Err(stat(e));
            }
            matches!(slot, Ok(Slot::Record { len, .. }) if len == entry.len)
        };
        if lands {
            return done(because(
                IndexState::Wrong,
                format_args!(
                    "the entry for record {} at offset {} is not on the segment's chain",
                    entry.seq, entry.offset
                ),
            ));
        }
    }
    match sealed {
        Ok(end) if end != (index.seal_offset, index.next_seq) => done(because(
            IndexState::Wrong,
            "the seal it names is not the segment's seal",
        )),
        _ if !off_chain.is_empty() => done(because(
            IndexState::Ignored,
            format_args!("{} entries do not match the segment", off_chain.len()),
        )),
        Ok(_) => Ok(SegmentIndex {
            ignored_by_readers: loaded.ignored.is_some(),
            ..SegmentIndex::new(base, (IndexState::Valid, loaded.ignored.map(String::from)))
        }),
        Err(reason) => done(because(IndexState::NotSealed, reason)),
    }
}

/// Removes every seek index of the journal in `dir`, and every temporary file of one, and leaves the directory
/// [`DIR_NAME`] itself and everything else in it. Returns how many files were removed. Safe at any time, with readers,
/// writers and builders running.
///
/// # Errors
///
/// [`Error::Io`], after removing what can be removed; also if the name is taken by something that is not a directory.
/// A directory without the subdirectory is not an error.
pub fn remove(dir: impl AsRef<Path>) -> Result<u64> {
    let dir = dir.as_ref();
    let root = open_root(dir)?;
    let path = dir.join(DIR_NAME);
    let indexes = match IndexDir::open_at(&root) {
        Ok(indexes) => indexes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP)) => {
            return Err(refused(
                dir,
                io::ErrorKind::NotADirectory,
                "the name exists and is not a directory (a symbolic link counts as that)",
            ));
        }
        Err(e) => return Err(Error::io("open seek index directory", path, e)),
    };
    let (names, listed) = indexes.list();
    let mut failed = listed
        .err()
        .map(|e| Error::io("list seek index directory", &path, e));
    let mut removed = 0;
    for (name, _) in names {
        match indexes.unlink(&name) {
            Ok(gone) => removed += u64::from(gone),
            Err(e) => {
                failed.get_or_insert(Error::io("remove seek index", path.join(name), e));
            }
        }
    }
    failed.map_or(Ok(removed), Err)
}

/// Whether `target` is worth looking for in an index when a forward seek could start at a cursor `offset` bytes into
/// the segment starting at `base`, which expects `next_seq` there: when the estimated distance is at least 256 KiB.
pub(crate) fn far(target: u64, base: u64, next_seq: u64, offset: u64) -> bool {
    const FAR: u128 = 256 * 1024;
    let passed = u128::from(next_seq.saturating_sub(base));
    let bytes = u128::from(offset.saturating_sub(SEGMENT_HEADER_LEN as u64));
    let ahead = u128::from(target.saturating_sub(next_seq));
    passed > 0 && ahead.saturating_mul(bytes) / passed >= FAR
}

#[cfg(test)]
mod tests;
