//! The single writer.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::FileExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use crate::config::{RecoveryPolicy, WriterConfig};
use crate::error::{CorruptReason, Error, Result};
use crate::format::{self, CheckpointSlot, RecordHeader, ScanEnd, SegmentHeader, SegmentView};
use crate::journal::{self, CHECKPOINT_FILE, LOCK_FILE, QUARANTINE_DIR};
use crate::mmap::{self, MapRw};
use crate::record::NewRecord;

/// The only process allowed to append to a journal.
///
/// Holds an exclusive `flock` on `writer.lock` for as long as it lives; the OS releases it when the process exits
/// or dies, so a crashed writer never leaves a stale lock. `Writer` is `Send` but not `Sync`: appends take `&mut self`.
///
/// The writer maps the segments it writes. Another process writing into or truncating those files is outside the
/// contract and can crash or corrupt this process (see `docs/crash-contract.md` → Readers, the writer and memory
/// safety).
///
/// Dropping a writer does **not** checkpoint. Everything appended stays visible and survives the process exiting,
/// but is durable only after [`Writer::checkpoint`]. See `docs/crash-contract.md`.
#[derive(Debug)]
pub struct Writer {
    dir: PathBuf,
    config: WriterConfig,
    /// Holds the `flock` for the writer's lifetime.
    _lock: File,
    active: Segment,
    /// Offset of the next record in `active`.
    offset: u64,
    next_seq: u64,
    /// Start of the not-yet-checkpointed range of `active`.
    active_dirty_from: u64,
    /// Segments with not-yet-checkpointed data other than `active`.
    dirty: Vec<DirtySegment>,
    /// Directories whose entries changed since the last checkpoint.
    dirty_dirs: BTreeSet<PathBuf>,
    checkpoint_file: File,
    generation: u64,
    poisoned: bool,
    report: RecoveryReport,
}

/// An open, mapped segment.
#[derive(Debug)]
struct Segment {
    base: u64,
    size: u64,
    path: PathBuf,
    file: File,
    map: MapRw,
}

/// A segment with data that the next checkpoint must sync. Rolled-past segments keep their mapping (so `msync`
/// covers it); segments inherited from an earlier writer only need their file synced.
#[derive(Debug)]
struct DirtySegment {
    file: File,
    map: Option<MapRw>,
    from: u64,
    to: u64,
    path: PathBuf,
}

/// Read-only view of the writer's own mapping for the format module's scanner. Sound because only this process
/// writes the file (it holds the lock), and the view borrows the mapping immutably.
struct RwView<'a>(&'a MapRw);

impl SegmentView for RwView<'_> {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    fn commit_word(&self, offset: u64) -> Option<u32> {
        self.0.commit_word(offset)
    }

    fn with_bytes<R>(&self, offset: u64, len: u64, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
        self.0.bytes(offset, len).map(f)
    }
}

impl Writer {
    /// Opens the journal in `dir` for writing, creating the directory and the first segment if needed.
    ///
    /// Takes the lock, then recovers the journal as described in `docs/format.md` → Recovery: validates the chain
    /// of records from the last checkpoint marker (or the oldest segment, if there is none) to the end, repairs or
    /// quarantines what is invalid, and syncs every repair before returning. What it did is in
    /// [`Writer::recovery_report`]. Open time is proportional to the data written since the last checkpoint.
    ///
    /// # Errors
    ///
    /// - [`Error::Locked`](crate::Error::Locked) if another writer has the journal open.
    /// - [`Error::InvalidConfig`](crate::Error::InvalidConfig) if `config.segment_size` breaks the format's limits.
    /// - [`Error::Corrupt`](crate::Error::Corrupt) if the journal is damaged in a way recovery doesn't repair, or
    ///   the tail is corrupt and the policy is [`RecoveryPolicy::Fail`](crate::RecoveryPolicy::Fail).
    /// - [`Error::UnsupportedVersion`](crate::Error::UnsupportedVersion), [`Error::Io`](crate::Error::Io).
    pub fn open(dir: impl AsRef<Path>, config: WriterConfig) -> Result<Writer> {
        format::validate_segment_size(config.segment_size).map_err(|r| {
            Error::InvalidConfig(format!("segment_size {}: {r}", config.segment_size))
        })?;
        let dir = dir.as_ref().to_path_buf();
        let mut report = RecoveryReport::default();

        let created_dirs = create_dirs(&dir)?;
        for created in &created_dirs {
            if let Some(parent) = created.parent() {
                journal::sync_dir(if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                })?;
            }
        }
        let lock = take_lock(&dir)?;

        let mut rec = Recovery {
            dir: &dir,
            config: &config,
            report: &mut report,
            dirty_dirs: BTreeSet::new(),
        };
        rec.remove_tmp_files()?;
        let (checkpoint_file, checkpoint) = rec.open_checkpoint_file()?;
        let state = rec.recover(checkpoint)?;
        let mut dirty_dirs = rec.dirty_dirs;
        // A new writer can't know whether directory entries made by earlier writers are durable.
        dirty_dirs.insert(dir.clone());

        report.next_seq = state.next_seq;
        Ok(Writer {
            dir,
            config,
            _lock: lock,
            active_dirty_from: state.active_dirty_from,
            offset: state.offset,
            next_seq: state.next_seq,
            active: state.active,
            dirty: state.dirty,
            dirty_dirs,
            checkpoint_file,
            generation: checkpoint.map_or(0, |c| c.generation),
            poisoned: false,
            report,
        })
    }

    /// Appends a record and returns its sequence number.
    ///
    /// When this returns `Ok`, the record is committed: visible to all readers and safe from a crash of this process.
    /// It is not durable until a later [`Writer::checkpoint`]. If the record doesn't fit in the current segment, this
    /// call rolls to a new segment first (slower: it creates and syncs a file).
    ///
    /// # Errors
    ///
    /// - [`Error::RecordTooLarge`](crate::Error::RecordTooLarge) if the payload can't fit in one segment, or doesn't
    ///   fit in the current segment and that segment is still empty (see `docs/format.md` → Rolling).
    /// - [`Error::SequenceExhausted`](crate::Error::SequenceExhausted) after `u64::MAX - 1`.
    /// - [`Error::Io`](crate::Error::Io) if a roll fails, e.g. the disk is full
    ///   ([`Error::is_storage_full`](crate::Error::is_storage_full)). The journal is unchanged and the call can be
    ///   retried.
    pub fn append(&mut self, record: &NewRecord<'_>) -> Result<u64> {
        let payload = record.payload;
        self.append_with(record, payload.len(), |buf| buf.copy_from_slice(payload))
    }

    /// Appends a record whose payload is written in place by `fill`, avoiding a copy.
    ///
    /// `fill` receives a zeroed buffer of exactly `payload_len` bytes inside the mapped segment. No reader can see
    /// it until `fill` returns and the record is committed. If `fill` panics, nothing is committed: the writer zeroes
    /// the buffer and syncs that zeroing (`docs/format.md` → Abandoned writes) before the panic continues, so no
    /// bytes from it can later be mistaken for a record. `record.payload` is ignored.
    ///
    /// # Errors
    ///
    /// As for [`Writer::append`].
    pub fn append_with<F>(
        &mut self,
        record: &NewRecord<'_>,
        payload_len: usize,
        fill: F,
    ) -> Result<u64>
    where
        F: FnOnce(&mut [u8]),
    {
        if self.next_seq == u64::MAX {
            return Err(Error::SequenceExhausted);
        }
        let max = format::max_payload_len(self.config.segment_size);
        let len = match format::frame_len(payload_len) {
            Some(len) if payload_len as u64 <= max || self.fits(len) => len,
            _ => {
                return Err(Error::RecordTooLarge {
                    len: payload_len,
                    max: max as usize,
                });
            }
        };
        if !self.fits(len) {
            if payload_len as u64 > max || self.offset == format::SEGMENT_HEADER_LEN as u64 {
                return Err(Error::RecordTooLarge {
                    len: payload_len,
                    max: max.min(format::max_payload_len(self.active.size)) as usize,
                });
            }
            self.roll()?;
        }

        let seq = self.next_seq;
        let header = RecordHeader {
            seq,
            schema_id: record.schema_id,
            venue_ts_ns: record.venue_ts_ns,
            local_ts_ns: record.local_ts_ns,
        };
        let offset = self.offset;
        let body_len = u64::from(len) - format::COMMIT_WORD_LEN as u64;
        let body = self
            .active
            .map
            .bytes_mut(offset + format::COMMIT_WORD_LEN as u64, body_len)
            .ok_or_else(|| internal(&self.active.path, "record body out of bounds"))?;
        format::write_record_fields(&header, body);
        let payload_start = format::RECORD_HEADER_LEN - format::COMMIT_WORD_LEN;
        let filled = panic::catch_unwind(AssertUnwindSafe(|| fill(&mut body[payload_start..])));
        if let Err(cause) = filled {
            body.fill(0);
            let synced = self
                .active
                .map
                .sync_range(offset, u64::from(len))
                .and_then(|()| mmap::sync_data(&self.active.file));
            if synced.is_err() {
                // The zeroing may not be durable, so stale bytes could resurface after a power loss.
                self.poisoned = true;
            }
            panic::resume_unwind(cause);
        }
        format::finish_record_body(len, body);
        self.active
            .map
            .store_commit_word(offset, len)
            .ok_or_else(|| internal(&self.active.path, "commit word out of bounds"))?;
        self.offset = offset + format::align8(u64::from(len));
        self.next_seq = seq + 1;
        Ok(seq)
    }

    /// Makes every record appended so far durable, using the platform's sync calls (see
    /// `docs/crash-contract.md` → What `checkpoint()` does). Blocks until they finish.
    ///
    /// # Errors
    ///
    /// [`Error::Io`](crate::Error::Io) if a sync call fails. The writer is then poisoned: every later call returns
    /// [`Error::DurabilityLost`](crate::Error::DurabilityLost), because a failed sync can drop data that a retry
    /// would not rewrite. Appends keep working.
    pub fn checkpoint(&mut self) -> Result<()> {
        if self.poisoned {
            return Err(Error::DurabilityLost);
        }
        let result = self.checkpoint_inner();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn checkpoint_inner(&mut self) -> Result<()> {
        if self.generation == u64::MAX - 1 {
            return Err(Error::SequenceExhausted);
        }
        // 1. Data.
        crate::fault::inject(crate::fault::Point::CheckpointSync)
            .map_err(|e| Error::io("sync segment", &self.active.path, e))?;
        for seg in &self.dirty {
            if let Some(map) = &seg.map {
                map.sync_range(seg.from, seg.to - seg.from)
                    .map_err(|e| Error::io("msync segment", &seg.path, e))?;
            }
            mmap::sync_data(&seg.file).map_err(|e| Error::io("sync segment", &seg.path, e))?;
        }
        if self.offset > self.active_dirty_from {
            self.active
                .map
                .sync_range(self.active_dirty_from, self.offset - self.active_dirty_from)
                .map_err(|e| Error::io("msync segment", &self.active.path, e))?;
        }
        mmap::sync_data(&self.active.file)
            .map_err(|e| Error::io("sync segment", &self.active.path, e))?;
        // 2. Names.
        for dir in &self.dirty_dirs {
            journal::sync_dir(dir)?;
        }
        // 3. Barrier (macOS: F_FULLFSYNC flushes the drive cache, covering all of the above).
        mmap::full_barrier(&self.active.file)
            .map_err(|e| Error::io("full sync segment", &self.active.path, e))?;
        // 4. Marker.
        let generation = self.generation + 1;
        let slot = CheckpointSlot {
            generation,
            next_seq: self.next_seq,
            segment_base_seq: self.active.base,
            offset: self.offset,
        };
        let bytes = format::encode_checkpoint_slot(&slot);
        let marker_path = self.dir.join(CHECKPOINT_FILE);
        let at = format::CHECKPOINT_SLOT_OFFSETS[(generation % 2) as usize] as u64;
        self.checkpoint_file
            .write_all_at(&bytes, at)
            .map_err(|e| Error::io("write checkpoint marker", &marker_path, e))?;
        mmap::full_barrier(&self.checkpoint_file)
            .map_err(|e| Error::io("sync checkpoint marker", &marker_path, e))?;

        self.generation = generation;
        self.dirty.clear();
        self.dirty_dirs.clear();
        self.active_dirty_from = self.offset;
        Ok(())
    }

    /// The sequence number the next append will get. Equals the number of records in the journal.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// What recovery did when this writer opened the journal.
    pub fn recovery_report(&self) -> &RecoveryReport {
        &self.report
    }

    /// The journal directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn fits(&self, len: u32) -> bool {
        format::record_fits(self.active.size, self.offset, len)
    }

    /// Rolls to a new segment (`docs/format.md` → Rolling).
    fn roll(&mut self) -> Result<()> {
        let next_base = self.next_seq;
        let (file, path) = create_segment(&self.dir, next_base, self.config.segment_size)?;
        self.dirty_dirs.insert(self.dir.clone());
        let map = MapRw::new(&file, self.config.segment_size as usize)
            .map_err(|e| Error::io("map segment", &path, e))?;
        // After the link, the roll always completes: seal the old segment, then switch.
        let seal_at = self.offset;
        let crc = format::seal_crc(next_base).to_le_bytes();
        self.active
            .map
            .bytes_mut(seal_at + format::COMMIT_WORD_LEN as u64, 4)
            .ok_or_else(|| internal(&self.active.path, "seal out of bounds"))?
            .copy_from_slice(&crc);
        self.active
            .map
            .store_commit_word(seal_at, format::SEAL_MARKER)
            .ok_or_else(|| internal(&self.active.path, "seal out of bounds"))?;
        let new = Segment {
            base: next_base,
            size: self.config.segment_size,
            path,
            file,
            map,
        };
        let old = std::mem::replace(&mut self.active, new);
        self.dirty.push(DirtySegment {
            file: old.file,
            map: Some(old.map),
            from: self.active_dirty_from,
            to: seal_at + format::SEAL_LEN as u64,
            path: old.path,
        });
        self.offset = format::SEGMENT_HEADER_LEN as u64;
        self.active_dirty_from = self.offset;
        Ok(())
    }
}

/// An internal bounds check failed (a bug, reported as an error rather than a panic).
fn internal(path: &Path, what: &'static str) -> Error {
    Error::io(what, path, io::Error::other("internal bounds check failed"))
}

/// Creates `dir` and missing ancestors; returns the directories that were created, outermost first.
fn create_dirs(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut missing = Vec::new();
    let mut cur = Some(dir);
    while let Some(p) = cur {
        if p.as_os_str().is_empty() || p.exists() {
            break;
        }
        missing.push(p.to_path_buf());
        cur = p.parent();
    }
    missing.reverse();
    if !missing.is_empty() {
        fs::create_dir_all(dir).map_err(|e| Error::io("create journal directory", dir, e))?;
    }
    Ok(missing)
}

/// Opens `writer.lock` and takes the exclusive `flock`; records our PID in it.
fn take_lock(dir: &Path) -> Result<File> {
    let path = dir.join(LOCK_FILE);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| Error::io("open lock file", &path, e))?;
    if !mmap::try_lock_exclusive(&file).map_err(|e| Error::io("lock journal", &path, e))? {
        let mut text = String::new();
        let pid = file
            .read_to_string(&mut text)
            .ok()
            .and_then(|_| text.trim().parse().ok());
        return Err(Error::Locked {
            dir: dir.to_path_buf(),
            pid,
        });
    }
    // The PID is informational only, so failing to record it (e.g. on a full disk) must not stop the writer.
    let _ = file
        .set_len(0)
        .and_then(|()| file.write_all_at(std::process::id().to_string().as_bytes(), 0));
    Ok(file)
}

/// Creates and publishes a new segment (`docs/format.md` → Rolling, steps 2–3). Returns the open file (read-write)
/// and its final path.
fn create_segment(dir: &Path, base: u64, size: u64) -> Result<(File, PathBuf)> {
    let tmp = journal::tmp_path(dir, base);
    let path = journal::segment_path(dir, base);
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io("remove temporary segment", &tmp, e)),
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| Error::io("create segment", &tmp, e))?;
    let prepared = (|| {
        crate::fault::inject(crate::fault::Point::Preallocate)
            .and_then(|()| mmap::preallocate(&file, size))
            .map_err(|e| Error::io("preallocate segment", &tmp, e))?;
        let header = format::encode_segment_header(&SegmentHeader {
            segment_size: size,
            base_seq: base,
            created_unix_ns: crate::clock::realtime_ns(),
        });
        file.write_all_at(&header, 0)
            .map_err(|e| Error::io("write segment header", &tmp, e))?;
        mmap::sync_data(&file).map_err(|e| Error::io("sync segment", &tmp, e))?;
        crate::fault::inject(crate::fault::Point::Link)
            .and_then(|()| fs::hard_link(&tmp, &path))
            .map_err(|e| Error::io("publish segment", &path, e))
    })();
    if let Err(e) = prepared {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    // Publication happened; a leftover temporary name is harmless and removed at the next open.
    let _ = fs::remove_file(&tmp);
    Ok((file, path))
}

/// Opens an existing segment read-write, validates its header and maps it.
fn open_segment(dir: &Path, base: u64) -> Result<Segment> {
    let path = journal::segment_path(dir, base);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| Error::io("open segment", &path, e))?;
    let header = journal::read_header(&file, &path, base)?;
    let map = MapRw::new(&file, header.segment_size as usize)
        .map_err(|e| Error::io("map segment", &path, e))?;
    Ok(Segment {
        base,
        size: header.segment_size,
        path,
        file,
        map,
    })
}

/// The writer's position after recovery.
struct RecoveredState {
    active: Segment,
    offset: u64,
    next_seq: u64,
    active_dirty_from: u64,
    dirty: Vec<DirtySegment>,
}

/// Recovery context (`docs/format.md` → Recovery).
struct Recovery<'a> {
    dir: &'a Path,
    config: &'a WriterConfig,
    report: &'a mut RecoveryReport,
    dirty_dirs: BTreeSet<PathBuf>,
}

impl Recovery<'_> {
    fn remove_tmp_files(&mut self) -> Result<()> {
        for tmp in journal::list_tmp_files(self.dir)? {
            fs::remove_file(&tmp).map_err(|e| Error::io("remove temporary segment", &tmp, e))?;
            self.report
                .actions
                .push(RecoveryAction::RemovedTemp { file: tmp });
            self.dirty_dirs.insert(self.dir.to_path_buf());
        }
        Ok(())
    }

    /// Opens (creating if needed) the checkpoint marker file and decodes the current checkpoint.
    fn open_checkpoint_file(&mut self) -> Result<(File, Option<CheckpointSlot>)> {
        let path = self.dir.join(CHECKPOINT_FILE);
        let existed = path.exists();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| Error::io("open checkpoint marker", &path, e))?;
        let len = file
            .metadata()
            .map_err(|e| Error::io("stat checkpoint marker", &path, e))?
            .len();
        let mut contents = vec![0u8; len.min(format::CHECKPOINT_FILE_LEN as u64) as usize];
        file.read_exact_at(&mut contents, 0)
            .map_err(|e| Error::io("read checkpoint marker", &path, e))?;
        let checkpoint = format::decode_checkpoint(&contents);
        if len < format::CHECKPOINT_FILE_LEN as u64 {
            // Preallocated, not sparse, so that writing a slot never needs new blocks (a full disk must not make
            // checkpoints fail).
            mmap::preallocate(&file, format::CHECKPOINT_FILE_LEN as u64)
                .map_err(|e| Error::io("create checkpoint marker", &path, e))?;
            mmap::sync_data(&file).map_err(|e| Error::io("sync checkpoint marker", &path, e))?;
        }
        if !existed {
            journal::sync_dir(self.dir)?;
        }
        Ok((file, checkpoint))
    }

    fn recover(&mut self, checkpoint: Option<CheckpointSlot>) -> Result<RecoveredState> {
        let segments = journal::list_segments(self.dir)?;
        let bases: BTreeSet<u64> = segments.iter().map(|(b, _)| *b).collect();
        if bases.is_empty() {
            if let Some(c) = checkpoint {
                return Err(self.dangling(c));
            }
            let (file, path) = create_segment(self.dir, 0, self.config.segment_size)?;
            journal::sync_dir(self.dir)?;
            mmap::full_barrier(&file).map_err(|e| Error::io("full sync segment", &path, e))?;
            drop(file);
            self.report.actions.push(RecoveryAction::Created);
            let active = open_segment(self.dir, 0)?;
            return Ok(RecoveredState {
                active,
                offset: format::SEGMENT_HEADER_LEN as u64,
                next_seq: 0,
                active_dirty_from: format::SEGMENT_HEADER_LEN as u64,
                dirty: Vec::new(),
            });
        }

        // Scan start.
        let (start_base, mut offset, mut seq) = match checkpoint {
            Some(c) => {
                if !bases.contains(&c.segment_base_seq) {
                    return Err(self.dangling(c));
                }
                (c.segment_base_seq, c.offset, c.next_seq)
            }
            None => {
                let oldest = *bases.iter().next().unwrap_or(&0);
                (oldest, format::SEGMENT_HEADER_LEN as u64, oldest)
            }
        };
        self.report.had_checkpoint = checkpoint.is_some();
        self.report.scanned_from_seq = seq;

        let mut seg = open_segment(self.dir, start_base)?;
        if let Some(c) = checkpoint {
            let aligned = offset % format::RECORD_ALIGN as u64 == 0;
            if !aligned || offset < format::SEGMENT_HEADER_LEN as u64 || offset > seg.size - 8 {
                return Err(self.dangling(c));
            }
        }
        let mut visited = BTreeSet::new();
        let mut dirty = Vec::new();

        loop {
            visited.insert(seg.base);
            let scan = format::scan_segment(&RwView(&seg.map), offset, seq);
            offset = scan.end_offset;
            seq = scan.next_seq;
            match scan.end {
                ScanEnd::Empty => {
                    self.zero_uncommitted_tail(&mut seg, offset)?;
                    if seq != seg.base && bases.contains(&seq) && !visited.contains(&seq) {
                        // Crash between publishing the next segment and sealing this one.
                        let next = match open_segment(self.dir, seq) {
                            Ok(next) => next,
                            Err(_) => break,
                        };
                        self.write_seal(&mut seg, offset, seq)?;
                        self.report.actions.push(RecoveryAction::Sealed {
                            segment: seg.path.clone(),
                        });
                        dirty.push(inherited(seg));
                        seg = next;
                        offset = format::SEGMENT_HEADER_LEN as u64;
                        continue;
                    }
                    break;
                }
                ScanEnd::Seal => {
                    let later_exists = bases.range(seq.saturating_add(1)..).next().is_some();
                    if bases.contains(&seq) {
                        match open_segment(self.dir, seq) {
                            Ok(next) => {
                                dirty.push(inherited(seg));
                                seg = next;
                                offset = format::SEGMENT_HEADER_LEN as u64;
                                continue;
                            }
                            Err(Error::Corrupt { .. } | Error::UnsupportedVersion { .. })
                                if !later_exists =>
                            {
                                let path = journal::segment_path(self.dir, seq);
                                self.quarantine_segment(
                                    &path,
                                    CorruptReason::BadSegmentHeader("invalid header"),
                                )?;
                            }
                            Err(e) => return Err(e),
                        }
                    } else if later_exists {
                        return Err(Error::corrupt(
                            &seg.path,
                            offset,
                            CorruptReason::BrokenChain(seq),
                        ));
                    }
                    // Crash or power loss during a roll: create the successor.
                    let (file, path) = create_segment(self.dir, seq, self.config.segment_size)?;
                    self.dirty_dirs.insert(self.dir.to_path_buf());
                    journal::sync_dir(self.dir)?;
                    drop(file);
                    self.report
                        .actions
                        .push(RecoveryAction::CompletedRoll { segment: path });
                    dirty.push(inherited(seg));
                    seg = open_segment(self.dir, seq)?;
                    visited.insert(seq);
                    offset = format::SEGMENT_HEADER_LEN as u64;
                    break;
                }
                ScanEnd::Invalid(reason) => {
                    if self.config.recovery == RecoveryPolicy::Fail {
                        return Err(Error::corrupt(&seg.path, offset, reason));
                    }
                    seg = self.repair_tail(seg, offset, reason)?;
                    break;
                }
            }
        }

        // Disconnected segments: everything in the recovery domain that the scan did not visit.
        for (base, path) in &segments {
            if *base >= start_base && !visited.contains(base) && path.exists() {
                if self.config.recovery == RecoveryPolicy::Fail {
                    return Err(Error::corrupt(path, 0, CorruptReason::BrokenChain(*base)));
                }
                self.quarantine_segment(path, CorruptReason::BrokenChain(*base))?;
            }
        }
        if !self.report.actions.is_empty() {
            for d in &self.dirty_dirs {
                journal::sync_dir(d)?;
            }
            mmap::full_barrier(&seg.file)
                .map_err(|e| Error::io("full sync segment", &seg.path, e))?;
        }

        let active_dirty_from = if seg.base == start_base {
            checkpoint.map_or(format::SEGMENT_HEADER_LEN as u64, |c| c.offset)
        } else {
            format::SEGMENT_HEADER_LEN as u64
        };
        Ok(RecoveredState {
            active: seg,
            offset,
            next_seq: seq,
            active_dirty_from: active_dirty_from.min(offset),
            dirty,
        })
    }

    fn dangling(&self, c: CheckpointSlot) -> Error {
        Error::corrupt(
            self.dir.join(CHECKPOINT_FILE),
            0,
            CorruptReason::DanglingCheckpoint(c.segment_base_seq),
        )
    }

    /// Zeroes non-zero bytes after an uncommitted end at `offset` (commit word 0) and syncs them.
    fn zero_uncommitted_tail(&mut self, seg: &mut Segment, offset: u64) -> Result<()> {
        let start = offset + format::COMMIT_WORD_LEN as u64;
        if start >= seg.size {
            return Ok(());
        }
        let last_nonzero = seg
            .map
            .bytes(start, seg.size - start)
            .and_then(|tail| tail.iter().rposition(|&b| b != 0));
        let Some(last) = last_nonzero else {
            return Ok(());
        };
        let len = last as u64 + 1;
        seg.map
            .bytes_mut(start, len)
            .ok_or_else(|| Error::io("zero tail", &seg.path, io::Error::other("out of bounds")))?
            .fill(0);
        seg.map
            .sync_range(start, len)
            .and_then(|()| mmap::sync_data(&seg.file))
            .map_err(|e| Error::io("sync zeroed tail", &seg.path, e))?;
        self.report.actions.push(RecoveryAction::ZeroedUncommitted {
            segment: seg.path.clone(),
            offset: start,
            len,
        });
        Ok(())
    }

    /// Writes a seal for `next_seq` at `offset` and syncs it.
    fn write_seal(&mut self, seg: &mut Segment, offset: u64, next_seq: u64) -> Result<()> {
        let oob = || Error::io("write seal", &seg.path, io::Error::other("out of bounds"));
        seg.map
            .bytes_mut(offset + format::COMMIT_WORD_LEN as u64, 4)
            .ok_or_else(oob)?
            .copy_from_slice(&format::seal_crc(next_seq).to_le_bytes());
        seg.map
            .store_commit_word(offset, format::SEAL_MARKER)
            .ok_or_else(oob)?;
        seg.map
            .sync_range(offset, format::SEAL_LEN as u64)
            .and_then(|()| mmap::sync_data(&seg.file))
            .map_err(|e| Error::io("sync seal", &seg.path, e))
    }

    /// Creates `quarantine/` if needed, durably.
    fn quarantine_dir(&mut self) -> Result<PathBuf> {
        let q = self.dir.join(QUARANTINE_DIR);
        if !q.exists() {
            fs::create_dir(&q).map_err(|e| Error::io("create quarantine directory", &q, e))?;
            journal::sync_dir(self.dir)?;
        }
        Ok(q)
    }

    /// Links `from` to the first free name `{stem}-{n}.bin` in `qdir`. Never replaces a file.
    fn link_free_name(qdir: &Path, stem: &str, from: &Path) -> Result<PathBuf> {
        for n in 0u32.. {
            let to = qdir.join(format!("{stem}-{n}.bin"));
            match fs::hard_link(from, &to) {
                Ok(()) => return Ok(to),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(Error::io("publish quarantine file", &to, e)),
            }
        }
        Err(Error::io(
            "publish quarantine file",
            qdir,
            io::Error::other("no free name"),
        ))
    }

    /// Preserves `[offset, last non-zero byte]` of `seg` in `quarantine/`, durably. Returns the file.
    fn preserve_bytes(&mut self, seg: &Segment, offset: u64) -> Result<PathBuf> {
        let qdir = self.quarantine_dir()?;
        let tail = seg.map.bytes(offset, seg.size - offset).unwrap_or(&[]);
        let end = tail.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        let stem = format!(
            "{:020}-{offset:012}-{}",
            seg.base,
            crate::clock::realtime_ns()
        );
        let partial = qdir.join(format!("{stem}.bin.partial"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .map_err(|e| Error::io("create quarantine file", &partial, e))?;
        file.write_all(&tail[..end])
            .map_err(|e| Error::io("write quarantine file", &partial, e))?;
        mmap::sync_data(&file).map_err(|e| Error::io("sync quarantine file", &partial, e))?;
        let fin = Self::link_free_name(&qdir, &stem, &partial)?;
        fs::remove_file(&partial)
            .map_err(|e| Error::io("remove partial quarantine file", &partial, e))?;
        journal::sync_dir(&qdir)?;
        mmap::full_barrier(&file).map_err(|e| Error::io("full sync quarantine file", &fin, e))?;
        self.dirty_dirs.insert(qdir);
        Ok(fin)
    }

    /// Repairs a corrupt committed tail by replacement (`docs/format.md` → Recovery, step 4).
    fn repair_tail(&mut self, seg: Segment, offset: u64, reason: CorruptReason) -> Result<Segment> {
        let quarantine_file = self.preserve_bytes(&seg, offset)?;
        let tmp = journal::tmp_path(self.dir, seg.base);
        let _ = fs::remove_file(&tmp);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| Error::io("create repaired segment", &tmp, e))?;
        mmap::preallocate(&file, seg.size)
            .map_err(|e| Error::io("preallocate repaired segment", &tmp, e))?;
        let prefix = seg.map.bytes(0, offset).ok_or_else(|| {
            Error::io(
                "copy segment prefix",
                &seg.path,
                io::Error::other("out of bounds"),
            )
        })?;
        file.write_all_at(prefix, 0)
            .map_err(|e| Error::io("write repaired segment", &tmp, e))?;
        mmap::sync_data(&file).map_err(|e| Error::io("sync repaired segment", &tmp, e))?;
        fs::rename(&tmp, &seg.path).map_err(|e| Error::io("replace segment", &seg.path, e))?;
        journal::sync_dir(self.dir)?;
        mmap::full_barrier(&file)
            .map_err(|e| Error::io("full sync repaired segment", &seg.path, e))?;
        self.report.actions.push(RecoveryAction::RepairedTail {
            segment: seg.path.clone(),
            offset,
            len: seg.size - offset,
            quarantine_file,
            reason,
        });
        let base = seg.base;
        drop(seg);
        open_segment(self.dir, base)
    }

    /// Moves a whole segment into `quarantine/`, durably.
    fn quarantine_segment(&mut self, path: &Path, reason: CorruptReason) -> Result<()> {
        let qdir = self.quarantine_dir()?;
        let file = File::open(path).map_err(|e| Error::io("open segment", path, e))?;
        mmap::sync_all(&file).map_err(|e| Error::io("sync segment", path, e))?;
        let stem = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("segment")
            .to_string();
        let to = Self::link_free_name(&qdir, &stem, path)?;
        fs::remove_file(path).map_err(|e| Error::io("remove quarantined segment", path, e))?;
        journal::sync_dir(self.dir)?;
        journal::sync_dir(&qdir)?;
        mmap::full_barrier(&file).map_err(|e| Error::io("full sync segment", &to, e))?;
        self.dirty_dirs.insert(qdir);
        self.report
            .actions
            .push(RecoveryAction::QuarantinedSegment {
                segment: path.to_path_buf(),
                quarantine_file: to,
                reason,
            });
        Ok(())
    }
}

/// A segment inherited from an earlier writer: the next checkpoint syncs its file.
fn inherited(seg: Segment) -> DirtySegment {
    DirtySegment {
        from: 0,
        to: seg.size,
        file: seg.file,
        map: None,
        path: seg.path,
    }
}

/// What [`Writer::open`] found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RecoveryReport {
    /// Sequence number of the next record after recovery.
    pub next_seq: u64,
    /// Where validation started: the checkpoint marker's `next_seq`, or the oldest segment's `base_seq`.
    pub scanned_from_seq: u64,
    /// Whether a valid checkpoint marker was found.
    pub had_checkpoint: bool,
    /// Every repair action, in the order performed. Empty for a cleanly closed journal.
    pub actions: Vec<RecoveryAction>,
}

/// One repair performed by [`Writer::open`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecoveryAction {
    /// The journal didn't exist and was created.
    Created,
    /// Uncommitted bytes after the last record (a record being written when the previous writer died) were zeroed.
    ZeroedUncommitted {
        /// The segment.
        segment: PathBuf,
        /// First zeroed offset.
        offset: u64,
        /// Number of bytes zeroed.
        len: u64,
    },
    /// A corrupt committed tail was copied to `quarantine`, and the segment was replaced by a copy with the tail
    /// zeroed (never edited in place).
    RepairedTail {
        /// The segment.
        segment: PathBuf,
        /// First affected offset.
        offset: u64,
        /// Number of bytes removed from the segment.
        len: u64,
        /// The quarantine file.
        quarantine_file: PathBuf,
        /// Why the record at `offset` was invalid.
        reason: crate::CorruptReason,
    },
    /// A segment with an invalid header, or one disconnected from the valid chain, was moved to `quarantine`.
    QuarantinedSegment {
        /// Where the segment was.
        segment: PathBuf,
        /// Where it is now.
        quarantine_file: PathBuf,
        /// Why it was removed from the chain.
        reason: crate::CorruptReason,
    },
    /// The previous segment was missing its seal (crash during a roll) and was sealed.
    Sealed {
        /// The segment that was sealed.
        segment: PathBuf,
    },
    /// The newest segment was sealed but its successor was missing; the successor was created.
    CompletedRoll {
        /// The new segment.
        segment: PathBuf,
    },
    /// A stale `*.tape.tmp` file from an interrupted roll was removed.
    RemovedTemp {
        /// The removed file.
        file: PathBuf,
    },
}
