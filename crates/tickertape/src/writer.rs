//! The single writer.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::FileExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::checkpoint::{self, Background, Context, DirtySegment, Job, Marker, On};
use crate::config::{RecoveryPolicy, WriterConfig};
use crate::error::{CorruptReason, Error, Result};
use crate::format::{self, CheckpointSlot, RecordHeader, ScanEnd, SegmentHeader, SegmentView};
use crate::journal::{self, CHECKPOINT_FILE, LOCK_FILE, QUARANTINE_DIR};
use crate::mmap::{self, MapRw};
use crate::prefault::{Prefaulter, Spare};
use crate::record::NewRecord;

/// The only process allowed to append to a journal.
///
/// Holds an exclusive `flock` on `writer.lock` for as long as it lives. Dropping the writer in the process that
/// opened it releases the lock, after the writer's helper threads have stopped, so the journal can be reopened at
/// once, by this or another process. If the process exits or dies instead, the OS releases the lock when the last
/// copy of its descriptor closes, so a crashed writer never leaves a stale lock; but a child the process forked keeps
/// a copy until it calls `exec` or exits, for its whole life if it never execs (crash contract C3). A forked child
/// must neither use nor drop an inherited writer: if it drops one, the lock stays with the parent, but the writer's
/// other clean-up still runs in the child, where stopping the prefault helper (whose thread does not exist there)
/// can stall it and clearing the wake-up region disturbs the parent's readers. `Writer` is `Send` but not `Sync`: appends take
/// `&mut self`.
///
/// The writer maps the segments it writes. Another process writing into or truncating those files is outside the
/// contract and can crash or corrupt this process (see `docs/crash-contract.md` → Readers, the writer and memory
/// safety).
///
/// Dropping a writer does **not** checkpoint. Everything appended stays visible and survives the process exiting,
/// but is durable only after [`Writer::checkpoint`], or after a background checkpoint started with
/// [`Writer::request_checkpoint`] has finished. Dropping waits for the background checkpoints already requested
/// (and joins the thread that runs them, ADR 0038); [`Writer::close`] does the same and reports how they ended. See
/// `docs/crash-contract.md`.
#[derive(Debug)]
pub struct Writer {
    /// Holds the `flock` for the writer's lifetime. Keep this field first: fields drop in declaration order, and in a
    /// forked child the guard's descriptor must close before any field whose drop could block there (see the
    /// `Drop` implementation).
    lock: JournalLock,
    /// The checkpoint thread, once a background checkpoint has been requested (ADR 0038). Dropped (finishing the
    /// requested checkpoints and joining the thread) first in `Drop`, while the lock is still held.
    background: Option<Background>,
    /// The value of the checkpoint thread's `posted` counter when the writer last collected its results.
    collected: u64,
    /// The generation and the covered `next_seq` of the newest background checkpoint requested.
    requested: Option<(u64, u64)>,
    dir: PathBuf,
    config: WriterConfig,
    active: Segment,
    /// Offset of the next record in `active`.
    offset: u64,
    next_seq: u64,
    /// Start of the not-yet-checkpointed range of `active`.
    active_dirty_from: u64,
    /// Segments with not-yet-checkpointed data other than `active`, since the last checkpoint was requested.
    dirty: Vec<DirtySegment>,
    /// Directories whose entries changed since the last checkpoint was requested.
    dirty_dirs: BTreeSet<PathBuf>,
    /// The marker file, the lost-durability flag and the notify publisher, shared with the checkpoint thread.
    ctx: Arc<Context>,
    /// The wake-up region, if enabled and available (ADRs 0017 and 0021).
    notify: Option<mmap::Notify>,
    /// The background prefault thread, if enabled and started (ADR 0020).
    prefault: Option<Prefaulter>,
    /// The next segment being prepared in the background, if requested (ADR 0022).
    spare: Option<std::thread::JoinHandle<io::Result<Spare>>>,
    /// Whether a spare has been requested for the current active segment.
    spare_requested: bool,
    /// The generation of the newest checkpoint this writer started (or of the marker read at open).
    generation: u64,
    poisoned: bool,
    /// Bytes of checkpoints that failed on this thread (for `metrics`): no checkpoint of this writer covers them.
    failed_bytes: u64,
    /// Set if an abandoned write's zeroing could not be made durable; see `append_with`.
    abandoned_write_error: Option<Error>,
    report: RecoveryReport,
    retention_report: crate::retention::Report,
    last_retention_error: Option<Error>,
    /// Retention runs that failed since open.
    retention_failures: u64,
    /// The checkpoint marker's current slot as this writer last wrote it (or read it at open).
    checkpointed: Option<CheckpointSlot>,
    /// When this writer's last successful checkpoint finished.
    last_checkpoint_at: Option<std::time::Instant>,
}

/// An open, mapped segment.
#[derive(Debug)]
struct Segment {
    base: u64,
    size: u64,
    path: PathBuf,
    /// Shared with a background checkpoint that syncs this segment.
    file: Arc<File>,
    map: MapRw,
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
    /// [`Writer::recovery_report`]. Open time is proportional to the data written since the last checkpoint,
    /// plus any configured retention work. Retention runs after recovery and its errors are recorded separately
    /// in [`Writer::last_retention_error`], without failing open.
    ///
    /// # Errors
    ///
    /// - [`Error::Locked`] if another writer has the journal open.
    /// - [`Error::InvalidConfig`] if `config.segment_size` breaks the format's limits.
    /// - [`Error::Corrupt`] if the journal is damaged in a way recovery doesn't repair, or
    ///   the tail is corrupt and the policy is [`RecoveryPolicy::Fail`].
    /// - [`Error::UnsupportedVersion`], [`Error::Io`].
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
        #[cfg(test)]
        if let Some(hook) = tests::AFTER_LOCK.with(std::cell::Cell::take) {
            hook();
        }

        let mut rec = Recovery {
            dir: &dir,
            config: &config,
            report: &mut report,
            dirty_dirs: BTreeSet::new(),
        };
        rec.remove_tmp_files()?;
        let (checkpoint_file, checkpoint, checkpoint_file_short) = rec.open_checkpoint_file()?;
        let state = rec.recover(checkpoint)?;
        let mut dirty_dirs = rec.dirty_dirs;
        // A new writer can't know whether directory entries made by earlier writers are durable, including the
        // journal directory's own entry in its parent (an earlier writer may have died right after creating it).
        dirty_dirs.insert(dir.clone());
        if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
            dirty_dirs.insert(parent.to_path_buf());
        }

        report.next_seq = state.next_seq;
        let notify = crate::notify::open_for_writer(&dir, config.notify);
        // The helper runs on macOS (ADR 0020) and Linux (ADR 0025); elsewhere it would have no advice to give.
        let prefault = (config.prefault && cfg!(any(target_vendor = "apple", target_os = "linux")))
            .then(|| Prefaulter::start(state.active.map.advice(), state.offset))
            .flatten();
        let ctx = Arc::new(Context {
            dir: dir.clone(),
            marker: Mutex::new(Marker {
                file: checkpoint_file,
                short: checkpoint_file_short,
            }),
            lost: Mutex::new(false),
            notify: notify.as_ref().map(mmap::Notify::publisher),
        });
        let mut writer = Writer {
            dir,
            config,
            lock,
            background: None,
            collected: 0,
            requested: None,
            active_dirty_from: state.active_dirty_from,
            offset: state.offset,
            next_seq: state.next_seq,
            active: state.active,
            dirty: state.dirty,
            dirty_dirs,
            ctx,
            notify,
            prefault,
            spare: None,
            spare_requested: false,
            generation: checkpoint.map_or(0, |c| c.generation),
            poisoned: false,
            failed_bytes: 0,
            abandoned_write_error: None,
            report,
            retention_report: crate::retention::Report::default(),
            last_retention_error: None,
            retention_failures: 0,
            checkpointed: checkpoint,
            last_checkpoint_at: None,
        };
        writer.run_retention();
        Ok(writer)
    }

    /// Appends a record and returns its sequence number.
    ///
    /// When this returns `Ok`, the record is committed: visible to all readers and safe from a crash of this process.
    /// It is not durable until a later [`Writer::checkpoint`]. If the record doesn't fit in the current segment, this
    /// call rolls to a new segment first (slower: it creates and syncs a file).
    ///
    /// # Errors
    ///
    /// - [`Error::RecordTooLarge`] if the payload can't fit in one segment, or doesn't
    ///   fit in the current segment and that segment is still empty (see `docs/format.md` → Rolling).
    /// - [`Error::SequenceExhausted`] after `u64::MAX - 1`.
    /// - [`Error::Io`] if a roll fails, e.g. the disk is full
    ///   ([`Error::is_storage_full`]). The journal is unchanged and the call can be
    ///   retried.
    /// - [`Error::Io`] that no retry clears, once the zeroing of an abandoned write could not be made durable
    ///   ([`Writer::abandoned_write_error`]). [`Error::is_storage_full`] is false for it; reopen the journal.
    pub fn append(&mut self, record: &NewRecord<'_>) -> Result<u64> {
        let payload = record.payload;
        self.append_with(record, payload.len(), |buf| buf.copy_from_slice(payload))
    }

    /// Appends a record whose payload is written in place by `fill`, avoiding a copy.
    ///
    /// `fill` receives a zeroed buffer of exactly `payload_len` bytes inside the mapped segment. No reader can see
    /// it until `fill` returns and the record is committed. If `fill` panics, nothing is committed: the writer zeroes
    /// the buffer and syncs that zeroing (`docs/format.md` → Abandoned writes) before the panic continues, so no
    /// bytes from it can later be mistaken for a record. If that sync fails, the panic continues all the same, the
    /// stale bytes may still be on disk, and the writer refuses further appends and checkpoints
    /// ([`Writer::abandoned_write_error`]). `record.payload` is ignored.
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
        if let Some(cause) = &self.abandoned_write_error {
            let source = match cause {
                Error::Io { source, .. } => {
                    let os = source
                        .get_ref()
                        .and_then(|e| e.downcast_ref::<io::Error>())
                        .unwrap_or(source);
                    os.raw_os_error().map_or_else(
                        || io::Error::new(os.kind(), os.to_string()),
                        io::Error::from_raw_os_error,
                    )
                }
                other => io::Error::other(other.to_string()),
            };
            // Wrapped, like the cause, so that the OS error (say ENOSPC) is never `is_storage_full`: freeing space
            // does not make this writer accept appends again.
            return Err(Error::io(
                "append (refused: an earlier abandoned write could not be zeroed durably; reopen the journal)",
                &self.active.path,
                io::Error::other(source),
            ));
        }
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
            // The zeroing must be durable (full platform barrier) before this space is reused. Under the lock a
            // checkpoint holds from its first sync to its marker (it waits for a background checkpoint in flight): a
            // failed sync here must keep any later marker from being written (`Context::lost`).
            let synced = {
                let mut lost = checkpoint::lock(&self.ctx.lost);
                let synced = crate::fault::inject(crate::fault::Point::AbandonedWriteSync)
                    .and_then(|()| self.active.map.sync_range(offset, u64::from(len)))
                    .and_then(|()| mmap::sync_data(&self.active.file))
                    .and_then(|()| mmap::full_barrier(&self.active.file));
                if synced.is_err() {
                    *lost = true;
                }
                synced
            };
            if let Err(e) = synced {
                // Stale bytes could resurface after a power loss and be read as a record after a shorter one,
                // so this writer must not reuse the space: refuse further appends and checkpoints.
                self.poisoned = true;
                // The OS error is wrapped (kind `Other`) so that it is never `is_storage_full`.
                self.abandoned_write_error = Some(Error::io(
                    "sync the zeroing of an abandoned write",
                    &self.active.path,
                    io::Error::other(e),
                ));
            }
            panic::resume_unwind(cause);
        }
        format::finish_record_body(len, body);
        crate::fault::pause_before_publish();
        self.active
            .map
            .store_commit_word(offset, len)
            .ok_or_else(|| internal(&self.active.path, "commit word out of bounds"))?;
        self.offset = offset + format::align8(u64::from(len));
        if let Some(notify) = &self.notify {
            notify.publish();
        }
        if let Some(prefault) = &mut self.prefault {
            prefault.advanced(self.offset);
        }
        // Once the active segment is half full, prepare the next one off the writer's thread (ADR 0022). macOS only: on
        // ext4 a synchronous roll costs about 1 ms, and the spare's preallocation sync occasionally stalled the writer
        // for tens of milliseconds (ADR 0023).
        if self.config.prefault
            && cfg!(target_vendor = "apple")
            && !self.spare_requested
            && self.offset >= self.active.size / 2
        {
            self.spare_requested = true;
            self.spare = crate::prefault::prepare_spare(&self.dir, self.config.segment_size);
        }
        self.next_seq = seq + 1;
        Ok(seq)
    }

    /// Makes every record appended so far durable, using the platform's sync calls (see
    /// `docs/crash-contract.md` → What `checkpoint()` does). Blocks until they finish.
    /// Configured retention then runs; its failures are recorded by [`Writer::last_retention_error`] and do not
    /// change the successful checkpoint result or poison the writer.
    ///
    /// If background checkpoints ([`Writer::request_checkpoint`]) are in flight, this first waits for them; it then
    /// checkpoints on the calling thread, as without them.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if a sync call fails. The writer is then poisoned: every later call returns
    /// [`Error::DurabilityLost`], because a failed sync can drop data that a retry
    /// would not rewrite. Appends keep working. If a background checkpoint failed and no call has returned its error
    /// yet, this returns that error (and the writer is poisoned).
    pub fn checkpoint(&mut self) -> Result<()> {
        self.finish_background()?;
        if self.poisoned {
            return Err(Error::DurabilityLost);
        }
        let result = self.checkpoint_inner();
        if result.is_err() {
            self.poisoned = true;
        } else {
            self.run_retention();
        }
        result
    }

    fn checkpoint_inner(&mut self) -> Result<()> {
        if self.generation >= u64::MAX - 1 {
            return Err(Error::SequenceExhausted);
        }
        self.generation += 1;
        let job = self.take_job(self.generation);
        match checkpoint::run(&job, &self.ctx, On::Writer) {
            Ok(slot) => {
                self.checkpointed = Some(slot);
                self.last_checkpoint_at = Some(Instant::now());
                Ok(())
            }
            Err(e) => {
                self.failed_bytes = self.failed_bytes.saturating_add(job.bytes());
                Err(e)
            }
        }
    }

    /// Hands what is dirty now to a checkpoint with `generation` whose marker names the current position, and starts
    /// a new dirty list.
    fn take_job(&mut self, generation: u64) -> Job {
        let mut segments = std::mem::take(&mut self.dirty);
        segments.push(DirtySegment {
            file: Arc::clone(&self.active.file),
            flush: Some(self.active.map.flush_handle()),
            from: self.active_dirty_from,
            to: self.offset,
            path: self.active.path.clone(),
        });
        self.active_dirty_from = self.offset;
        Job {
            slot: CheckpointSlot {
                generation,
                next_seq: self.next_seq,
                segment_base_seq: self.active.base,
                offset: self.offset,
            },
            segments,
            dirs: std::mem::take(&mut self.dirty_dirs),
        }
    }

    /// Starts a checkpoint of every record appended so far on the writer's checkpoint thread, and returns without
    /// waiting for its sync calls (ADR 0038). Returns the sequence number it covers up to: once it has finished, every
    /// record with a smaller sequence number is durable, exactly as after [`Writer::checkpoint`]. Records appended after
    /// this call are not covered. Appends continue while the checkpoint runs.
    ///
    /// Observe completion with [`Writer::checkpoint_progress`]. [`Writer::checkpoint`] waits for background checkpoints
    /// in flight before it checkpoints itself, and [`Writer::close`] waits for them and reports how they ended. The
    /// first call starts the thread that runs them (`tickertape-checkpoint`); dropping the writer finishes the
    /// requested checkpoints, then joins it.
    ///
    /// One checkpoint runs at a time. A request made while one runs waits for it, and further requests made meanwhile
    /// join the waiting one, which then covers up to the newest of them (one marker for all of them).
    ///
    /// Configured retention runs on the checkpoint thread after each successful background checkpoint. What it did
    /// reaches [`Writer::retention_report`] and [`Writer::last_retention_error`] when the writer next collects the
    /// thread's results: in this method, [`Writer::checkpoint_progress`], [`Writer::checkpoint`] and
    /// [`Writer::close`] ([`Writer::metrics`] includes it at once). Retention runs after the marker is durable, so
    /// [`Writer::checkpoint_progress`] can report a checkpoint finished while its retention run goes on;
    /// [`Writer::checkpoint`], [`Writer::close`] and dropping the writer wait for it.
    ///
    /// Cost on the calling thread: one lock of the state shared with the checkpoint thread and a few small moves; no
    /// system call (except for starting the thread at the first call).
    ///
    /// # Errors
    ///
    /// - The error of an earlier background checkpoint that failed, if no call has returned it yet. The writer is then
    ///   poisoned, as after a failed [`Writer::checkpoint`], and nothing is requested.
    /// - [`Error::DurabilityLost`] if the writer is poisoned.
    /// - [`Error::Io`] if the checkpoint thread cannot be started. Nothing is requested, and the writer is not
    ///   poisoned.
    /// - [`Error::SequenceExhausted`] if checkpoint generations are exhausted.
    pub fn request_checkpoint(&mut self) -> Result<u64> {
        self.collect()?;
        if self.poisoned {
            return Err(Error::DurabilityLost);
        }
        let shared = match &self.background {
            Some(background) => Arc::clone(&background.shared),
            None => {
                let started =
                    Background::start(Arc::clone(&self.ctx), self.config.retention.clone())
                        .map_err(|e| Error::io("start checkpoint thread", &self.dir, e))?;
                let shared = Arc::clone(&started.shared);
                self.background = Some(started);
                shared
            }
        };
        let mut state = checkpoint::lock(&shared.state);
        if state.failed {
            // A failure arrived after `collect` above.
            drop(state);
            self.collect()?;
            return Err(Error::DurabilityLost);
        }
        let next_seq = self.next_seq;
        let generation = match state.queued.as_ref().map(|queued| queued.slot.generation) {
            Some(generation) => {
                let job = self.take_job(generation);
                if let Some(queued) = state.queued.as_mut() {
                    queued.absorb(job);
                }
                generation
            }
            None => {
                if self.generation >= u64::MAX - 1 {
                    return Err(Error::SequenceExhausted);
                }
                self.generation += 1;
                state.queued = Some(self.take_job(self.generation));
                self.generation
            }
        };
        drop(state);
        shared.wake();
        self.requested = Some((generation, next_seq));
        Ok(next_seq)
    }

    /// Where this writer's checkpoints stand: the marker it last made durable, and whether a background checkpoint
    /// ([`Writer::request_checkpoint`]) is still in flight. Collects what the checkpoint thread has finished.
    ///
    /// Cost: one atomic load when the checkpoint thread has finished nothing since the last call; otherwise one lock
    /// of the state shared with it. No system calls. Nothing is added to [`Writer::append`].
    ///
    /// # Errors
    ///
    /// The error of a background checkpoint that failed, the first time it is reported (the writer is then poisoned);
    /// [`Error::DurabilityLost`] whenever the writer is poisoned after that, also after a failed
    /// [`Writer::checkpoint`] or an abandoned write that could not be zeroed durably. A poisoned writer reports no
    /// progress: nothing it does is durable any more.
    pub fn checkpoint_progress(&mut self) -> Result<CheckpointProgress> {
        self.collect()?;
        if self.poisoned {
            return Err(Error::DurabilityLost);
        }
        Ok(CheckpointProgress {
            durable: self.checkpointed,
            pending: self.pending(),
        })
    }

    /// Waits for the background checkpoints in flight, drops the writer, and reports whether every checkpoint of this
    /// writer succeeded. Like dropping, it does not checkpoint records that no checkpoint was requested for.
    ///
    /// # Errors
    ///
    /// - The error of a background checkpoint that failed, if no earlier call has returned it.
    /// - [`Error::DurabilityLost`] if the writer is poisoned: a checkpoint failed earlier (and its error was returned
    ///   then), or an abandoned write could not be zeroed durably ([`Writer::abandoned_write_error`]).
    pub fn close(mut self) -> Result<()> {
        let result = match self.finish_background() {
            Ok(()) if self.poisoned => Err(Error::DurabilityLost),
            other => other,
        };
        drop(self);
        result
    }

    /// Waits until no background checkpoint is queued or running, then collects.
    fn finish_background(&mut self) -> Result<()> {
        if let Some(background) = &self.background {
            background.wait_idle();
        }
        self.collect()
    }

    /// Takes what the checkpoint thread has finished since the last call: the newest durable marker, retention's
    /// results, and the first failure, which is returned (once) and poisons the writer.
    fn collect(&mut self) -> Result<()> {
        let shared = match &self.background {
            Some(background)
                if background.shared.posted.load(Ordering::Acquire) != self.collected =>
            {
                Arc::clone(&background.shared)
            }
            _ => return Ok(()),
        };
        let mut state = checkpoint::lock(&shared.state);
        self.collected = shared.posted.load(Ordering::Relaxed);
        if let Some((slot, at)) = state.done.take() {
            self.checkpointed = Some(slot);
            self.last_checkpoint_at = Some(at);
        }
        if let Some(outcome) = state.retention.take() {
            self.retention_report.absorb(&outcome.report);
            if let Some(error) = outcome.last_error {
                self.last_retention_error = Some(error);
            }
            self.retention_failures = self.retention_failures.saturating_add(outcome.failures);
        }
        if let Some(error) = state.failure.take() {
            self.poisoned = true;
            return Err(error);
        }
        Ok(())
    }

    /// The `next_seq` of the newest requested background checkpoint, if it has not finished.
    fn pending(&self) -> Option<u64> {
        let (generation, next_seq) = self.requested?;
        let done = self.checkpointed.map_or(0, |c| c.generation);
        (generation > done).then_some(next_seq)
    }

    /// The sequence number the next append will get, including history already removed by retention.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// What recovery did when this writer opened the journal.
    pub fn recovery_report(&self) -> &RecoveryReport {
        &self.report
    }

    /// Cumulative successful retention removals since open, including partially failed runs.
    pub fn retention_report(&self) -> &crate::retention::Report {
        &self.retention_report
    }

    /// The error that stopped this writer when the zeroing of an abandoned write (a panic in an
    /// [`append_with`](Writer::append_with) fill closure) could not be made durable, if that happened.
    ///
    /// Treat this as a storage incident, like a failed checkpoint (`docs/crash-contract.md` → Errors are sticky).
    /// While it is set, appends fail with [`Error::Io`] and checkpoints with [`Error::DurabilityLost`], and the
    /// durability of everything written since the last successful checkpoint is unknown. Reopening the journal gives
    /// a writer that accepts appends again, but no guarantee about that tail across a power loss: the zeroed bytes may
    /// only be in the page cache, so recovery cannot tell whether the disk still holds the stale ones.
    ///
    /// The `source` of this error and of every refused append has kind [`io::ErrorKind::Other`], so
    /// [`Error::is_storage_full`] is false even when the sync failed with `ENOSPC` (retrying after freeing space does
    /// not help); the OS error is its inner error ([`io::Error::get_ref`]).
    pub fn abandoned_write_error(&self) -> Option<&Error> {
        self.abandoned_write_error.as_ref()
    }

    /// Most recent retention error since open. A later successful run does not clear it.
    /// Retention errors never poison the writer or fail open, append or checkpoint.
    pub fn last_retention_error(&self) -> Option<&Error> {
        self.last_retention_error.as_ref()
    }

    /// Operational metrics only the writer knows: the head, the last checkpoint and its age by the monotonic clock,
    /// what the next checkpoint has to sync, whether the writer is poisoned, and retention failures (ADR 0032; see
    /// [`crate::status`] for what other processes can observe).
    ///
    /// With background checkpoints ([`Writer::request_checkpoint`]), the metrics include what the checkpoint thread
    /// has finished even if the writer has not collected it yet: the checkpoint is the newest durable marker, the
    /// uncheckpointed bytes include those of checkpoints still in flight (and of failed ones), and `poisoned` is true
    /// as soon as one has failed.
    ///
    /// Cost: no system calls; one monotonic clock read, a sum over the segments touched since the last
    /// checkpoint, and, once a background checkpoint has been requested, one lock of the state shared with the
    /// checkpoint thread. Nothing is added to [`Writer::append`].
    pub fn metrics(&self) -> crate::status::WriterMetrics {
        let dirty = self.dirty.iter().fold(0u64, |sum, seg| {
            sum.saturating_add(seg.to.saturating_sub(seg.from))
        });
        let mut checkpoint = self.checkpointed;
        let mut last_at = self.last_checkpoint_at;
        let mut elsewhere = self.failed_bytes;
        let mut poisoned = self.poisoned;
        let mut retention_failures = self.retention_failures;
        if let Some(background) = &self.background {
            let state = checkpoint::lock(&background.shared.state);
            if let Some((slot, at)) = state.done {
                checkpoint = Some(slot);
                last_at = Some(at);
            }
            elsewhere = elsewhere
                .saturating_add(state.in_flight_bytes())
                .saturating_add(state.failed_bytes);
            poisoned |= state.failed;
            if let Some(outcome) = &state.retention {
                retention_failures = retention_failures.saturating_add(outcome.failures);
            }
        }
        crate::status::WriterMetrics {
            head: crate::status::Position::new(self.next_seq, self.active.base, self.offset),
            segment_size: self.active.size,
            checkpoint,
            since_checkpoint: last_at.map(|at| at.elapsed()),
            // Without a marker, everything from where recovery started (the oldest retained segment) is uncovered.
            uncheckpointed_records: self
                .next_seq
                .saturating_sub(checkpoint.map_or(self.report.scanned_from_seq, |c| c.next_seq)),
            uncheckpointed_bytes: dirty
                .saturating_add(self.offset.saturating_sub(self.active_dirty_from))
                .saturating_add(elsewhere),
            poisoned,
            retention_failures,
        }
    }

    fn run_retention(&mut self) {
        let Some((policy, action)) = &self.config.retention else {
            return;
        };
        let result = crate::retention::plan(&self.dir, policy).and_then(|plan| {
            crate::retention::apply_into(&self.dir, &plan, action, &mut self.retention_report)
        });
        if let Err(error) = result {
            self.last_retention_error = Some(error);
            self.retention_failures = self.retention_failures.saturating_add(1);
        }
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
        let new = match self.take_spare() {
            Some(spare) => publish_spare(&self.dir, next_base, self.config.segment_size, spare)?,
            None => create_segment(&self.dir, next_base, self.config.segment_size)?,
        };
        self.spare_requested = false;
        self.dirty_dirs.insert(self.dir.clone());
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
        let old = std::mem::replace(&mut self.active, new);
        self.dirty.push(DirtySegment {
            file: old.file,
            flush: Some(old.map.flush_handle()),
            from: self.active_dirty_from,
            to: seal_at + format::SEAL_LEN as u64,
            path: old.path,
        });
        self.offset = format::SEGMENT_HEADER_LEN as u64;
        self.active_dirty_from = self.offset;
        if let Some(prefault) = &mut self.prefault {
            prefault.switch(self.active.map.advice(), self.offset);
        }
        if let Some(notify) = &self.notify {
            notify.publish();
        }
        Ok(())
    }
}

impl Writer {
    /// Waits for the spare being prepared, if any, and returns it if it is usable for this journal's segment size.
    fn take_spare(&mut self) -> Option<Spare> {
        match self.spare.take()?.join() {
            Ok(Ok(spare)) if spare.size == self.config.segment_size => Some(spare),
            Ok(Ok(_)) => {
                let _ = fs::remove_file(self.dir.join(journal::SPARE_FILE));
                None
            }
            // Preparing failed (it removed its own file) or the thread panicked: create the segment synchronously.
            _ => None,
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // The order of what follows:
        // - In the process that took the lock: finish the requested background checkpoints and join their thread
        //   (ADR 0038), join the spare-segment thread and remove a prepared spare (so the
        //   next writer's recovery has nothing to clean up), stop the helper and tell waiting readers that this
        //   writer has gone, all here, while the lock is still held, so that a new writer never overlaps them. Then
        //   the fields drop in declaration order: first the lock guard, which unlocks explicitly and closes its
        //   descriptor, then the others, which only unmap and close.
        // - In any other process (a forked child that dropped its copy, which it must not do): nothing is torn down
        //   here. The spare belongs to the parent's writer and its thread does not exist in the child, so it is
        //   neither joined nor removed; nor is the checkpoint thread, whose handle leaves it alone in another process. The lock guard drops first, without unlocking (the lock still protects the
        //   parent's writer), and closes the child's copy of the descriptor before the later fields `notify` and
        //   `prefault` drop, since joining the helper there can block forever and must not keep the journal locked
        //   once the parent has gone.
        if self.lock.owned_here() {
            // Finish the requested background checkpoints and join their thread first: the last marker must be
            // written while the lock is held, and before readers are told that this writer has gone.
            drop(self.background.take());
            if let Some(Ok(Ok(spare))) = self.spare.take().map(|h| h.join()) {
                drop(spare);
                let _ = fs::remove_file(self.dir.join(journal::SPARE_FILE));
            }
            drop(self.prefault.take());
            drop(self.notify.take());
        }
    }
}

/// The exclusive `flock` on `writer.lock`, released when the guard drops, also when [`Writer::open`] fails after
/// taking it.
#[derive(Debug)]
struct JournalLock {
    file: File,
    /// The process that took the lock: only it may unlock (a forked child shares the lock's open file description).
    pid: u32,
}

impl JournalLock {
    /// Whether this is the process that took the lock.
    fn owned_here(&self) -> bool {
        self.pid == std::process::id()
    }
}

impl Drop for JournalLock {
    fn drop(&mut self) {
        // Release the lock explicitly: closing the descriptor (when `file` drops) would not release it while a child
        // forked by another thread still holds a copy of it before `exec`. Only in the process that took it: in a
        // forked child the lock still protects the parent's writer, and only the descriptor closes.
        if self.owned_here() {
            let _ = mmap::unlock(&self.file);
        }
    }
}

/// Publishes a prepared spare as segment `base` (ADR 0022): write and sync its header, then link it into place, as
/// [`create_segment`] does for a new file. If the header can't be written or synced, the spare is discarded and the
/// segment is created synchronously instead; a failed link is reported like [`create_segment`]'s.
fn publish_spare(dir: &Path, base: u64, size: u64, spare: Spare) -> Result<Segment> {
    let spare_path = dir.join(journal::SPARE_FILE);
    let path = journal::segment_path(dir, base);
    let header = format::encode_segment_header(&SegmentHeader {
        segment_size: size,
        base_seq: base,
        created_unix_ns: crate::clock::realtime_ns(),
    });
    let prepared = crate::fault::inject(crate::fault::Point::SpareHeader)
        .and_then(|()| spare.file.write_all_at(&header, 0))
        .and_then(|()| mmap::sync_data(&spare.file));
    if prepared.is_err() {
        drop(spare);
        let _ = fs::remove_file(&spare_path);
        return create_segment(dir, base, size);
    }
    if let Err(e) = crate::fault::inject(crate::fault::Point::Link)
        .and_then(|()| fs::hard_link(&spare_path, &path))
    {
        drop(spare);
        let _ = fs::remove_file(&spare_path);
        return Err(Error::io("publish segment", &path, e));
    }
    // Publication happened; a leftover spare name is harmless and removed at the next open.
    let _ = fs::remove_file(&spare_path);
    Ok(Segment {
        base,
        size,
        path,
        file: Arc::new(spare.file),
        map: spare.map,
    })
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
fn take_lock(dir: &Path) -> Result<JournalLock> {
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
    Ok(JournalLock {
        file,
        pid: std::process::id(),
    })
}

/// Creates and publishes a new segment (`docs/format.md` → Rolling, steps 2–3). Returns the open file (read-write)
/// and its final path.
fn create_segment(dir: &Path, base: u64, size: u64) -> Result<Segment> {
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
        // Map before publishing, so that nothing fallible is left after the link (the publication point).
        let len = usize::try_from(size)
            .map_err(|_| Error::io("map segment", &tmp, io::Error::other("segment too large")))?;
        let map = crate::fault::inject(crate::fault::Point::MapSegment)
            .and_then(|()| MapRw::new(&file, len))
            .map_err(|e| Error::io("map segment", &tmp, e))?;
        crate::fault::inject(crate::fault::Point::Link)
            .and_then(|()| fs::hard_link(&tmp, &path))
            .map_err(|e| Error::io("publish segment", &path, e))?;
        Ok(map)
    })();
    let map = match prepared {
        Ok(map) => map,
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
    };
    // Publication happened; a leftover temporary name is harmless and removed at the next open.
    let _ = fs::remove_file(&tmp);
    Ok(Segment {
        base,
        size,
        path,
        file: Arc::new(file),
        map,
    })
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
        file: Arc::new(file),
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
        let spare = self.dir.join(journal::SPARE_FILE);
        let leftovers = journal::list_tmp_files(self.dir)?
            .into_iter()
            .chain(spare.symlink_metadata().is_ok().then_some(spare));
        for tmp in leftovers {
            fs::remove_file(&tmp).map_err(|e| Error::io("remove temporary segment", &tmp, e))?;
            self.report
                .actions
                .push(RecoveryAction::RemovedTemp { file: tmp });
            self.dirty_dirs.insert(self.dir.to_path_buf());
        }
        Ok(())
    }

    /// Opens (creating if needed) the checkpoint marker file and decodes the current checkpoint. Also returns whether
    /// an existing marker is shorter than its full length.
    fn open_checkpoint_file(&mut self) -> Result<(File, Option<CheckpointSlot>, bool)> {
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
        if !existed {
            // A new marker: preallocated, not sparse, so that writing a slot never needs new blocks (a full disk must
            // not make checkpoints fail).
            mmap::preallocate(&file, format::CHECKPOINT_FILE_LEN as u64)
                .map_err(|e| Error::io("create checkpoint marker", &path, e))?;
            mmap::sync_data(&file).map_err(|e| Error::io("sync checkpoint marker", &path, e))?;
            journal::sync_dir(self.dir)?;
            return Ok((file, checkpoint, false));
        }
        // An existing marker that something cut short is left short here: observers take a full-length marker's
        // modification time as the time of the last checkpoint (ADR 0032), and the time of an extension is not that.
        // The next checkpoint extends it after its slot is durable. Until then, a checkpoint writes its slot into
        // the short file, which may need a new block.
        Ok((file, checkpoint, len < format::CHECKPOINT_FILE_LEN as u64))
    }

    fn recover(&mut self, checkpoint: Option<CheckpointSlot>) -> Result<RecoveredState> {
        let segments = journal::list_segments(self.dir)?;
        let bases: BTreeSet<u64> = segments.iter().map(|(b, _)| *b).collect();
        if bases.is_empty() {
            if let Some(c) = checkpoint {
                return Err(self.dangling(c));
            }
            let active = create_segment(self.dir, 0, self.config.segment_size)?;
            journal::sync_dir(self.dir)?;
            mmap::full_barrier(&active.file)
                .map_err(|e| Error::io("full sync segment", &active.path, e))?;
            self.report.actions.push(RecoveryAction::Created);
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
        // Where this writer's uncheckpointed data starts in `seg` (for `Writer::metrics`).
        let mut seg_from = offset;

        loop {
            visited.insert(seg.base);
            let scan = format::scan_segment(&RwView(&seg.map), offset, seq);
            offset = scan.end_offset;
            seq = scan.next_seq;
            match scan.end {
                ScanEnd::Empty => {
                    let tail_end = format::nonzero_end(
                        &RwView(&seg.map),
                        offset + format::COMMIT_WORD_LEN as u64,
                    );
                    if seq != seg.base && bases.contains(&seq) && !visited.contains(&seq) {
                        if let Ok(next) = open_segment(self.dir, seq) {
                            // The successor for exactly `seq` exists, is not this segment (at the first record slot
                            // `seq` is this segment's own base), has not been visited, and opens and maps: `offset`
                            // is where this segment's seal belongs, and the chain continues in the successor (ADR
                            // 0035, "Damage around a seal"). A valid header alone is not enough: `open_segment`
                            // must succeed.
                            if tail_end.is_some_and(|end| end > offset + format::SEAL_LEN as u64) {
                                // Bytes beyond the seal: no crash leaves them (a roll writes nothing after the seal,
                                // and records `seq` onwards are in the successor), so this is a damaged seal, not
                                // missing history, and later segments are not evidence. They are never records of
                                // this segment either: those would repeat the successor's sequence numbers. Preserve
                                // them as any repair does, then reseal and continue into the successor in this open.
                                // This is exactly where an interrupted repair leaves the next open (the segment ends
                                // at `offset`, followed by the same branch below), so an interruption at any point
                                // gives the same journal. `Fail` refuses: the bytes are damage.
                                if self.config.recovery == RecoveryPolicy::Fail {
                                    return Err(Error::corrupt(
                                        &seg.path,
                                        offset,
                                        CorruptReason::BadSeal,
                                    ));
                                }
                                seg = self.repair_tail(seg, offset, CorruptReason::BadSeal)?;
                            } else {
                                // A crash between publishing the successor and sealing this segment (or the seal's
                                // marker damaged): at most the seal's CRC word is left.
                                self.zero_uncommitted_tail(&mut seg, offset, tail_end)?;
                            }
                            self.write_seal(&mut seg, offset, seq)?;
                            self.report.actions.push(RecoveryAction::Sealed {
                                segment: seg.path.clone(),
                            });
                            dirty.push(inherited(seg, seg_from, offset));
                            seg = next;
                            offset = format::SEGMENT_HEADER_LEN as u64;
                            seg_from = offset;
                            continue;
                        }
                    }
                    if let Some(reason) = tail_end.and_then(|end| {
                        committed_after_empty(&seg, offset, seq, end, &bases, &visited)
                    }) {
                        // Evidence of committed history, or a search stopped at its work bound: a committed tail (ADR
                        // 0035; usually storage damage, but a torn payload can lead here too, crash contract C1).
                        if self.config.recovery == RecoveryPolicy::Fail {
                            return Err(Error::corrupt(&seg.path, offset, reason));
                        }
                        seg = self.repair_tail(seg, offset, reason)?;
                        break;
                    }
                    // The search completed without evidence: a torn append or the clean end, or damage that left no
                    // evidence (ADR 0035, "What cannot be detected"). (A successor for exactly `seq` that does not
                    // open, as before this change, ends the chain here: the bytes after `offset` are zeroed in place
                    // without a copy even under `Fail`, then the successor is quarantined below, or refused as
                    // disconnected under `Fail`. Known gap, ADR 0035: a later change will preserve them under
                    // `Quarantine` and make `Fail` refuse before zeroing.)
                    self.zero_uncommitted_tail(&mut seg, offset, tail_end)?;
                    break;
                }
                ScanEnd::Seal => {
                    if visited.contains(&seq) {
                        // A seal must lead forward; never revisit a segment.
                        return Err(Error::corrupt(
                            &seg.path,
                            offset,
                            CorruptReason::BrokenChain(seq),
                        ));
                    }
                    let later_exists = bases.range(seq.saturating_add(1)..).next().is_some();
                    if bases.contains(&seq) {
                        match open_segment(self.dir, seq) {
                            Ok(next) => {
                                dirty.push(inherited(seg, seg_from, offset));
                                seg = next;
                                offset = format::SEGMENT_HEADER_LEN as u64;
                                seg_from = offset;
                                continue;
                            }
                            Err(Error::Corrupt { .. } | Error::UnsupportedVersion { .. })
                                if !later_exists
                                    && self.config.recovery == RecoveryPolicy::Quarantine =>
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
                    let next = create_segment(self.dir, seq, self.config.segment_size)?;
                    self.dirty_dirs.insert(self.dir.to_path_buf());
                    journal::sync_dir(self.dir)?;
                    self.report.actions.push(RecoveryAction::CompletedRoll {
                        segment: next.path.clone(),
                    });
                    dirty.push(inherited(seg, seg_from, offset));
                    seg = next;
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

    /// Zeroes non-zero bytes after an uncommitted end at `offset` (commit word 0), which end at `tail_end`
    /// ([`format::nonzero_end`]), and syncs them.
    fn zero_uncommitted_tail(
        &mut self,
        seg: &mut Segment,
        offset: u64,
        tail_end: Option<u64>,
    ) -> Result<()> {
        let start = offset + format::COMMIT_WORD_LEN as u64;
        let Some(end) = tail_end.filter(|&end| end > start) else {
            return Ok(());
        };
        let len = end - start;
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

    /// Repairs a corrupt tail by replacement (`docs/format.md` → Recovery, step 4, last row).
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
        crate::fault::inject(crate::fault::Point::RepairBeforeRename)
            .map_err(|e| Error::io("replace segment", &seg.path, e))?;
        fs::rename(&tmp, &seg.path).map_err(|e| Error::io("replace segment", &seg.path, e))?;
        journal::sync_dir(self.dir)?;
        crate::fault::inject(crate::fault::Point::RepairAfterRename)
            .map_err(|e| Error::io("sync directory", self.dir, e))?;
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
        // Make the preserved name durable before removing the original.
        journal::sync_dir(&qdir)?;
        mmap::full_barrier(&file).map_err(|e| Error::io("full sync segment", &to, e))?;
        fs::remove_file(path).map_err(|e| Error::io("remove quarantined segment", path, e))?;
        journal::sync_dir(self.dir)?;
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

/// Committed history after an empty slot at `offset` of `seg`, where a scan expecting `seq` stopped and whose
/// non-zero bytes end at `tail_end` (ADR 0035): a later record or final seal in the segment that validates with a
/// sequence number its position allows, or a segment with a larger base than `seq`, which a process crash leaves
/// only after record `seq` was committed. Returns how to report it: [`CorruptReason::EmptyCommitWord`] with the sequence number the
/// evidence carries (a record's, a seal's next sequence number, or a segment's base), or
/// [`CorruptReason::UncheckedTail`] when the search in the segment stopped at its work bound. After a process crash
/// none of these is usually present; the exception is a torn payload whose bytes form a candidate (crash contract C1).
/// A segment with a larger base counts only when the caller found non-zero bytes after the commit word (`tail_end`),
/// and not in the case below.
///
/// The caller has already handled a successor for exactly `seq` that opens (a seal belongs at `offset`). A later
/// segment is still not evidence when that successor exists but does not open and the non-zero bytes end within a
/// seal at `offset`: unchanged from before, the chain then ends at `offset`.
fn committed_after_empty(
    seg: &Segment,
    offset: u64,
    seq: u64,
    tail_end: u64,
    bases: &BTreeSet<u64>,
    visited: &BTreeSet<u64>,
) -> Option<CorruptReason> {
    if let Some(found) = format::find_history_before(&RwView(&seg.map), offset, seq, tail_end) {
        return Some(found.reason());
    }
    let successor_pending = seq != seg.base && bases.contains(&seq) && !visited.contains(&seq);
    if successor_pending && tail_end <= offset + format::SEAL_LEN as u64 {
        return None;
    }
    bases
        .range(seq.checked_add(1)?..)
        .next()
        .map(|&base| CorruptReason::EmptyCommitWord(base))
}

/// A segment inherited from an earlier writer: the next checkpoint syncs its whole file. `[from, seal_at)` holds the
/// records an earlier writer appended after the marker, followed by the seal at `seal_at`; the range only feeds
/// [`Writer::metrics`] (`uncheckpointed_bytes`).
fn inherited(seg: Segment, from: u64, seal_at: u64) -> DirtySegment {
    DirtySegment {
        from,
        to: seal_at + format::SEAL_LEN as u64,
        file: seg.file,
        flush: None,
        path: seg.path,
    }
}

/// Where a writer's checkpoints stand ([`Writer::checkpoint_progress`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckpointProgress {
    /// The checkpoint marker as this writer last made it durable, or read it at open; `None` if there is none. Every
    /// record with a sequence number below its `next_seq` is durable.
    pub durable: Option<CheckpointSlot>,
    /// The sequence number the newest requested background checkpoint covers up to, while it has not finished; `None`
    /// when every requested checkpoint has finished.
    pub pending: Option<u64>,
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
    /// Non-zero bytes after an empty commit word were zeroed in place, without a copy (`docs/crash-contract.md`, C8
    /// and C11). This describes the action taken on the evidence recovery found, not what readers saw: after a
    /// process crash in an otherwise intact journal the bytes are an unpublished record body (or what is left of one)
    /// or the CRC word of a seal recovery is about to write, which no reader saw, but after storage damage that left no evidence they can be
    /// published records a reader has already returned (C11 (3)).
    ZeroedUncommitted {
        /// The segment.
        segment: PathBuf,
        /// First zeroed offset.
        offset: u64,
        /// Number of bytes zeroed.
        len: u64,
    },
    /// A corrupt tail was copied to `quarantine` (from `offset` to the segment's last non-zero byte), and the segment
    /// was replaced by a copy with the tail zeroed (never edited in place). The tail starts at an invalid record or
    /// seal; at an empty commit word with evidence of committed history after it, or after which the search stopped
    /// at its work bound ([`CorruptReason::EmptyCommitWord`](crate::CorruptReason::EmptyCommitWord),
    /// [`CorruptReason::UncheckedTail`](crate::CorruptReason::UncheckedTail)); or at the place of a seal with damage
    /// beyond it ([`CorruptReason::BadSeal`](crate::CorruptReason::BadSeal); `docs/crash-contract.md`, C11). What
    /// followed `offset` leaves the segment; the quarantine file keeps its bytes up to the last non-zero one.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmap::forked;
    use std::cell::{Cell, RefCell};
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::rc::Rc;

    thread_local! {
        /// Called (once) by [`Writer::open`] on this thread right after it has taken the lock.
        pub(super) static AFTER_LOCK: Cell<Option<Box<dyn FnOnce()>>> = const { Cell::new(None) };
        /// The writer's lock descriptor, for a forked child's probe.
        static LOCK_FD: Cell<RawFd> = const { Cell::new(-1) };
    }

    fn append_one(writer: &mut Writer) {
        writer
            .append(&NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload: b"x",
            })
            .unwrap();
    }

    /// A child forked while the writer is open (here by this test; in practice by any thread spawning a process)
    /// shares the lock's open file description until it calls `exec`. Dropping the writer must still release the
    /// lock at once, so the journal can be reopened while that child exists.
    #[test]
    fn drop_releases_the_lock_while_a_forked_child_holds_a_copy() {
        let _forked = forked::LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let config = WriterConfig::default().segment_size(1 << 20);
        let mut writer = Writer::open(dir.path(), config.clone()).unwrap();
        append_one(&mut writer);
        let child = forked::fork(|| {}).unwrap();
        drop(writer);
        let reopened = Writer::open(dir.path(), config);
        assert_eq!(child.wait(), Some(0));
        assert_eq!(
            reopened
                .expect("the dropped writer's lock must not wait for the forked child")
                .next_seq(),
            1
        );
    }

    /// A forked child that drops its copy of the writer must leave the lock with the parent, whose writer is still
    /// open: unlocking the shared open file description would let a second writer in.
    #[test]
    fn a_forked_child_dropping_its_copy_keeps_the_parents_lock() {
        let _forked = forked::LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        // No helper threads or wake-up region, which a child must not touch: only the lock is under test.
        let config = WriterConfig::default()
            .segment_size(1 << 20)
            .prefault(false)
            .notify(false);
        let mut writer = Some(Writer::open(dir.path(), config.clone()).unwrap());
        append_one(writer.as_mut().unwrap());
        let child = forked::fork(|| drop(writer.take())).unwrap();
        // The child has dropped its copy and exited, closing its descriptors.
        assert_eq!(child.wait(), Some(0));
        match Writer::open(dir.path(), config.clone()) {
            Err(Error::Locked { pid, .. }) => assert_eq!(pid, Some(std::process::id())),
            other => panic!("expected Locked while the parent's writer is open, got {other:?}"),
        }
        let mut writer = writer.unwrap();
        append_one(&mut writer);
        drop(writer);
        assert_eq!(Writer::open(dir.path(), config).unwrap().next_seq(), 2);
    }

    /// A forked child that drops its copy of the writer (which it must not do) must close its copy of the lock
    /// descriptor before it stops the prefault helper: the helper thread does not exist in the child, so stopping it
    /// can block forever there, and the child must not keep the journal locked after the parent has gone.
    #[test]
    fn a_forked_child_dropping_its_copy_closes_the_lock_before_the_helper() {
        const CLOSED: i32 = 42;
        const STILL_OPEN: i32 = 43;
        if !cfg!(any(target_vendor = "apple", target_os = "linux")) {
            return; // No helper thread elsewhere.
        }
        let _forked = forked::LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        // The prefault helper runs; no wake-up region, whose clean-up in the child would disturb nothing here but
        // is not under test.
        let config = WriterConfig::default().segment_size(1 << 20).notify(false);
        let mut writer = Some(Writer::open(dir.path(), config).unwrap());
        let open = writer.as_mut().unwrap();
        append_one(open);
        assert!(open.prefault.is_some(), "the helper must run for this test");
        let fd = open.lock.file.as_raw_fd();
        let child = forked::fork(|| {
            LOCK_FD.set(fd);
            // Leave as the helper is about to be stopped, reporting whether the lock descriptor is still open.
            crate::prefault::test_hooks::ON_DROP.set(Some(|| {
                forked::exit(if forked::is_open(LOCK_FD.get()) {
                    STILL_OPEN
                } else {
                    CLOSED
                })
            }));
            drop(writer.take());
        })
        .unwrap();
        assert_eq!(
            child.wait(),
            Some(CLOSED),
            "the child must close its lock descriptor before it stops the helper"
        );
        let mut writer = writer.unwrap();
        append_one(&mut writer);
    }

    /// A forked child that drops its copy of a writer whose checkpoint thread runs (ADR 0038), which it must not do,
    /// must leave that thread alone: it does not exist in the child, and the state it shares with the writer may have
    /// been locked at the fork (here it is, by this test), so locking, joining or detaching anything there could hang.
    #[test]
    fn a_forked_child_dropping_its_copy_leaves_the_checkpoint_thread_alone() {
        let _forked = forked::LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let config = WriterConfig::default()
            .segment_size(1 << 20)
            .prefault(false)
            .notify(false);
        let mut writer = Some(Writer::open(dir.path(), config.clone()).unwrap());
        let open = writer.as_mut().unwrap();
        append_one(open);
        assert_eq!(open.request_checkpoint().unwrap(), 1);
        let shared = Arc::clone(&open.background.as_ref().unwrap().shared);
        let held = checkpoint::lock(&shared.state);
        let child = forked::fork(|| drop(writer.take())).unwrap();
        drop(held);
        assert_eq!(
            child.wait_within(std::time::Duration::from_secs(20)),
            Some(0),
            "the child must not wait for the checkpoint thread"
        );
        let mut writer = writer.unwrap();
        append_one(&mut writer);
        assert_eq!(writer.request_checkpoint().unwrap(), 2);
        writer.close().unwrap();
        assert_eq!(
            Writer::open(dir.path(), config)
                .unwrap()
                .recovery_report()
                .scanned_from_seq,
            2
        );
    }

    /// An open that fails after taking the lock (here: `RecoveryPolicy::Fail` refusing a damaged journal) must
    /// release the lock explicitly too, so that the next open (here with `Quarantine`) succeeds at once while a child
    /// forked during the failed open still holds a copy of the descriptor.
    #[test]
    fn a_failed_open_releases_the_lock_while_a_forked_child_holds_a_copy() {
        let _forked = forked::LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let config = WriterConfig::default().segment_size(1 << 20);
        let mut writer = Writer::open(dir.path(), config.clone()).unwrap();
        append_one(&mut writer);
        drop(writer);
        // A segment off the chain, which `Fail` refuses and `Quarantine` moves away.
        drop(create_segment(dir.path(), 500, 1 << 20).unwrap());
        let child = Rc::new(RefCell::new(None));
        let slot = Rc::clone(&child);
        AFTER_LOCK.set(Some(Box::new(move || {
            *slot.borrow_mut() = Some(forked::fork(|| {}).unwrap());
        })));
        let failed = Writer::open(dir.path(), config.clone().recovery(RecoveryPolicy::Fail));
        assert!(matches!(failed, Err(Error::Corrupt { .. })), "{failed:?}");
        let child = child
            .borrow_mut()
            .take()
            .expect("the child is forked once the lock is taken");
        let reopened = Writer::open(dir.path(), config);
        assert_eq!(child.wait(), Some(0));
        let reopened = reopened.expect("the failed open's lock must not wait for the forked child");
        assert!(
            matches!(
                reopened.recovery_report().actions[..],
                [RecoveryAction::QuarantinedSegment { .. }]
            ),
            "{:?}",
            reopened.recovery_report()
        );
        assert_eq!(reopened.next_seq(), 1);
    }
}
