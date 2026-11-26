//! The single writer.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::checkpoint::{
    self, Background, Context, DirtySegment, DirtySegments, FileId, Job, Marker, On,
};
use crate::config::{RecoveryPolicy, WriterConfig};
use crate::error::{CorruptReason, Error, Result};
use crate::format::{self, CheckpointSlot, RecordHeader, ScanEnd, SegmentHeader, SegmentView};
use crate::journal::{self, CHECKPOINT_FILE, LOCK_FILE, QUARANTINE_DIR};
use crate::latency::{Recorder, WriterLatency, WriterRecorders};
use crate::mmap::{self, MapRw};
use crate::prefault::{Prefaulter, Spare, Spares, Waited};
use crate::record::NewRecord;
use crate::seek_index;
use crate::writeback::WriteBack;

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
    /// The journal directory made absolute at the open: the files the writer creates, reopens or syncs after the open
    /// are named under it, so a change of the process's working directory does not redirect them.
    root: PathBuf,
    /// The path given to [`Writer::open`], which [`Writer::dir`] returns.
    dir: PathBuf,
    config: WriterConfig,
    active: Segment,
    /// Offset of the next record in `active`.
    offset: u64,
    next_seq: u64,
    /// Start of the not-yet-checkpointed range of `active`.
    active_dirty_from: u64,
    /// Segments with not-yet-checkpointed data other than `active`, since the last checkpoint was requested. Only the
    /// newest few keep their file and mapping open ([`checkpoint::OPEN_SEGMENTS`]).
    dirty: DirtySegments,
    /// Directories whose entries changed since the last checkpoint was requested.
    dirty_dirs: BTreeSet<PathBuf>,
    /// The marker file, the lost-durability flag and the notify publisher, shared with the checkpoint thread.
    ctx: Arc<Context>,
    /// The wake-up region, if enabled and available (ADRs 0017 and 0021).
    notify: Option<mmap::Notify>,
    /// The background prefault thread, if enabled and started (ADR 0020).
    prefault: Option<Prefaulter>,
    /// The early write-back thread, if enabled and started (Linux; ADR 0038).
    writeback: Option<WriteBack>,
    /// Prepares the next segment in the background, if enabled and started (ADR 0022).
    spares: Option<Spares>,
    /// The generation of the newest checkpoint this writer started (or of the marker read at open).
    generation: u64,
    poisoned: bool,
    /// Bytes of checkpoints that failed on this thread (for `metrics`): no checkpoint of this writer covers them.
    failed_bytes: u64,
    /// Set if an abandoned write's zeroing could not be made durable; see `abandon`.
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
    /// Records appends, with `LatencyMode::All` (ADR 0047). The one thing of the histograms that the append path looks at.
    append_latency: Option<Recorder>,
    /// The other histograms, and the views of all of them, unless `LatencyMode::Off`.
    latency: Option<Box<WriterRecorders>>,
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
        // Recovery uses `dir` as given (its report and errors name paths as before); what the writer keeps for later
        // (its segments' paths, the directories to sync, the directory it rolls and runs retention in) is made
        // absolute against the working directory now.
        let cwd = if dir.is_relative() {
            Some(
                std::env::current_dir()
                    .map_err(|e| Error::io("resolve journal directory", &dir, e))?,
            )
        } else {
            None
        };
        let resolve = |p: &Path| match &cwd {
            Some(cwd) if p.is_relative() => cwd.join(p),
            _ => p.to_path_buf(),
        };
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
            held: Vec::new(),
        };
        rec.remove_tmp_files()?;
        let (checkpoint_file, checkpoint, checkpoint_file_short) = rec.open_checkpoint_file()?;
        let mut state = rec.recover(checkpoint)?;
        let mut dirty_dirs: BTreeSet<PathBuf> = rec.dirty_dirs.iter().map(|d| resolve(d)).collect();
        // A new writer can't know whether directory entries made by earlier writers are durable, including the
        // journal directory's own entry in its parent (an earlier writer may have died right after creating it).
        dirty_dirs.insert(resolve(&dir));
        if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
            dirty_dirs.insert(resolve(parent));
        }
        state.active.path = resolve(&state.active.path);
        state.dirty.map_paths(resolve);
        let root = resolve(&dir);

        report.next_seq = state.next_seq;
        let notify = crate::notify::open_for_writer(&dir, config.notify);
        // The helper runs on macOS (ADR 0020) and Linux (ADR 0025); elsewhere it would have no advice to give.
        let prefault = (config.prefault && cfg!(any(target_vendor = "apple", target_os = "linux")))
            .then(|| {
                Prefaulter::start(
                    state.active.map.advice(),
                    state.offset,
                    config.helper_cpus.clone(),
                )
            })
            .flatten();
        let spares = (config.prefault
            && config.spare_segment
            && cfg!(any(target_vendor = "apple", target_os = "linux")))
        .then(|| Spares::start(&root, config.segment_size, config.helper_cpus.clone()))
        .flatten();
        let (append_latency, latency) = WriterRecorders::new(config.latency);
        let ctx = Arc::new(Context {
            dir: root.clone(),
            marker: Mutex::new(Marker {
                file: checkpoint_file,
                short: checkpoint_file_short,
            }),
            lost: Mutex::new(false),
            writeback_failure: Mutex::new(None),
            writeback_failed: std::sync::atomic::AtomicBool::new(false),
            notify: notify.as_ref().map(mmap::Notify::publisher),
            retention_sizes: Mutex::default(),
            durable: latency.as_ref().map(|latency| latency.durable()),
        });
        // Linux only (`WriteBack::start` returns `None` elsewhere); `prefault(false)` means no helper threads.
        let writeback = (config.early_writeback && config.prefault)
            .then(|| {
                WriteBack::start(
                    Arc::clone(&state.active.file),
                    state.offset,
                    Arc::clone(&ctx),
                    config.helper_cpus.clone(),
                )
            })
            .flatten();
        let mut writer = Writer {
            root,
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
            writeback,
            spares,
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
            append_latency,
            latency,
        };
        writer.run_retention();
        Ok(writer)
    }

    /// Appends a record and returns its sequence number.
    ///
    /// When this returns `Ok`, the record is committed: visible to all readers and safe from a crash of this process.
    /// It is not durable until a later [`Writer::checkpoint`]. If the record doesn't fit in the current segment, this
    /// call rolls to a new segment first (slower: it creates or publishes a file; it does not sync it, the next
    /// checkpoint does).
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
    /// To give up on a record without a panic, use [`Writer::try_append_with`].
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
        let filled = self.try_append_with(record, payload_len, |buf| {
            fill(buf);
            Ok::<(), std::convert::Infallible>(())
        })?;
        match filled {
            Ok(seq) => Ok(seq),
            Err(never) => match never {},
        }
    }

    /// Appends a record whose payload is written in place by `fill`, which may decline to finish it.
    ///
    /// As [`Writer::append_with`], except that `fill` returns a `Result`: `Ok(())` commits the record, and
    /// `Err(e)` abandons it, and once the abandoned bytes are cleaned up this returns `Ok(Err(e))`, without a panic or
    /// any unwinding. The outer `Result` carries the journal's errors, so `?` passes those on and leaves the caller
    /// with its own outcome.
    ///
    /// Declining does what a panic in the closure does, except that this returns instead of the panic continuing:
    /// nothing is committed and the sequence number is not used. The writer zeroes the whole record it had begun (the
    /// header fields it wrote and `fill`'s buffer: everything after the commit word, which was never written) and
    /// syncs that zeroing (`docs/format.md` → Abandoned writes) before it returns, so no bytes from it can later be
    /// mistaken for a record. The next record is written at the same place if it fits there. A roll that this call
    /// completed before `fill` ran stays in place: the old segment is sealed and the writer is at the new segment's
    /// first slot. The sync makes declining slow: keep it for exceptional cases.
    ///
    /// If `fill` declines and the zeroing cannot be made durable, this returns the outer `Err`: the [`Error::Io`]
    /// that every later append returns, and the caller's `e` is dropped. The stale bytes may still be on disk and the
    /// writer is stopped: [`Writer::abandoned_write_error`] returns the cause, and every later append fails with
    /// [`Error::Io`] and every checkpoint with [`Error::DurabilityLost`]. A panic in `fill` behaves as in
    /// [`Writer::append_with`]: the panic continues, and a failed clean-up is recorded the same way.
    /// `record.payload` is ignored.
    ///
    /// ```no_run
    /// # use tickertape::{NewRecord, Writer, WriterConfig};
    /// # fn main() -> tickertape::Result<()> {
    /// let mut writer = Writer::open("journal", WriterConfig::default())?;
    /// let record = NewRecord { schema_id: 1, venue_ts_ns: 0, local_ts_ns: 0, payload: &[] };
    /// let text = "price=101.5";
    /// match writer.try_append_with(&record, 16, |buf| {
    ///     if text.len() > buf.len() {
    ///         return Err("does not fit");
    ///     }
    ///     buf[..text.len()].copy_from_slice(text.as_bytes());
    ///     Ok(())
    /// })? {
    ///     Ok(seq) => println!("appended {seq}"),
    ///     Err(reason) => println!("abandoned: {reason}"),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// As for [`Writer::append`]. An outer `Err` means the journal failed. `fill` did not run, except when it
    /// declined and the clean-up failed, which is the one case where [`Writer::abandoned_write_error`] is set by this
    /// call.
    pub fn try_append_with<F, E>(
        &mut self,
        record: &NewRecord<'_>,
        payload_len: usize,
        fill: F,
    ) -> Result<std::result::Result<u64, E>>
    where
        F: FnOnce(&mut [u8]) -> std::result::Result<(), E>,
    {
        // `--cfg tickertape_latency_check_out` compiles this test out, for measuring what it costs (ADR 0047, gate 1);
        // appends are then never timed.
        #[cfg(not(tickertape_latency_check_out))]
        if self.append_latency.is_some() {
            return self.try_append_timed(record, payload_len, fill);
        }
        // `write_record` publishes the wake-up right after the commit, before its helper work.
        self.write_record::<_, _, true>(record, payload_len, fill)
    }

    /// [`Writer::try_append_with`] with `LatencyMode::All`: times the call if it appends the record. Out of line, so that
    /// the append path without it stays as it is, but for the test that leads here.
    #[cfg_attr(tickertape_latency_check_out, allow(dead_code))]
    #[inline(never)]
    fn try_append_timed<F, E>(
        &mut self,
        record: &NewRecord<'_>,
        payload_len: usize,
        fill: F,
    ) -> Result<std::result::Result<u64, E>>
    where
        F: FnOnce(&mut [u8]) -> std::result::Result<(), E>,
    {
        let started = Instant::now();
        let appended = self.write_record::<_, _, true>(record, payload_len, fill);
        if let (Ok(Ok(_)), Some(recorder)) = (&appended, &mut self.append_latency) {
            recorder.record(started.elapsed());
        }
        appended
    }

    /// Appends `records` in order, as consecutive calls to [`Writer::append`] would, and returns the range of
    /// sequence numbers they got. Blocked readers are woken once for the batch, and at each roll inside it, instead of
    /// once per record.
    ///
    /// Each record is committed as soon as it is written, exactly as [`Writer::append`] commits it, so the records
    /// become visible one by one and in order: a reader that polls can read the first records of a batch before this
    /// call returns, and no reader can see a later record of the batch without the earlier ones. What is batched is
    /// the wake-up of blocked readers (ADRs 0017 and 0021). When wake-ups are enabled
    /// ([`WriterConfig::notify`](crate::WriterConfig::notify)), a batch that commits at least one record publishes one
    /// wake-up after the last record it appended, plus one at each roll inside the batch (a roll publishes as it does
    /// for [`Writer::append`]), instead of one after each record. So a blocked reader learns of the batch's first
    /// records at the first roll inside the batch, or when the batch ends if it does not roll. Keep batches short where
    /// that latency matters. See ADR 0041 for what is batched and what is not.
    ///
    /// A record that does not fit in the current segment rolls to a new one, as [`Writer::append`] does; a batch can
    /// span several segments. Durability is as for [`Writer::append`]: the writer's thread syncs nothing (on macOS a
    /// roll can wait for the thread preparing the next segment, which syncs it), and the records are durable once a
    /// later checkpoint covers them.
    ///
    /// An empty `records` returns the empty range at [`Writer::next_seq`] and wakes nobody.
    ///
    /// # Errors
    ///
    /// The batch stops at the first record that cannot be appended, for any reason [`Writer::append`] gives. The
    /// returned [`BatchError`] says how many records were appended: the first [`BatchError::appended`] records are
    /// committed and stay committed, with sequence numbers from [`BatchError::first_seq`]; the failing record (the
    /// next one) and the records after it are not, and the iterator is not advanced past the failing record. Blocked
    /// readers are woken for the records that were appended.
    pub fn append_batch<'a, I>(&mut self, records: I) -> std::result::Result<Range<u64>, BatchError>
    where
        I: IntoIterator<Item = NewRecord<'a>>,
    {
        #[cfg(not(tickertape_latency_check_out))] // as in `try_append_with`
        if self.append_latency.is_some() {
            return self.append_batch_timed(records);
        }
        self.write_batch(records)
    }

    /// [`Writer::append_batch`] with `LatencyMode::All`: times the call if it appends at least one record and succeeds.
    #[cfg_attr(tickertape_latency_check_out, allow(dead_code))]
    #[inline(never)]
    fn append_batch_timed<'a, I>(
        &mut self,
        records: I,
    ) -> std::result::Result<Range<u64>, BatchError>
    where
        I: IntoIterator<Item = NewRecord<'a>>,
    {
        let started = Instant::now();
        let appended = self.write_batch(records);
        if let (Ok(seqs), Some(latency)) = (&appended, &mut self.latency) {
            if !seqs.is_empty() {
                latency.batch(started);
            }
        }
        appended
    }

    #[inline(always)]
    fn write_batch<'a, I>(&mut self, records: I) -> std::result::Result<Range<u64>, BatchError>
    where
        I: IntoIterator<Item = NewRecord<'a>>,
    {
        let first_seq = self.next_seq;
        let mut appended = 0;
        // The iterator lives outside the `catch_unwind` and is dropped only after the publication, so that neither its
        // destructor (which may wait for a reader to see the committed records) nor a panic from it delays or prevents
        // the wake-up for the records already committed.
        let mut records = records.into_iter();
        let written = panic::catch_unwind(AssertUnwindSafe(|| {
            for record in &mut records {
                let payload = record.payload;
                match self.write_record::<_, _, false>(&record, payload.len(), |buf| {
                    buf.copy_from_slice(payload);
                    Ok::<(), std::convert::Infallible>(())
                }) {
                    Ok(Ok(_)) => appended += 1,
                    Ok(Err(never)) => match never {},
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        }));
        if appended > 0 {
            self.publish();
        }
        drop(records);
        match written {
            Ok(Ok(())) => Ok(first_seq..self.next_seq),
            Ok(Err(error)) => Err(BatchError {
                appended,
                first_seq,
                error,
            }),
            Err(cause) => panic::resume_unwind(cause),
        }
    }

    /// Wakes blocked readers (ADRs 0017 and 0021), if the wake-up region is enabled.
    #[inline]
    fn publish(&self) {
        if let Some(notify) = &self.notify {
            notify.publish();
        }
    }

    /// The append path. With `PUBLISH`, it publishes the wake-up right after the commit, before the helper work that
    /// follows ([`Writer::try_append_with`]); without, the caller publishes once for many records
    /// ([`Writer::append_batch`]).
    #[inline(always)]
    fn write_record<F, E, const PUBLISH: bool>(
        &mut self,
        record: &NewRecord<'_>,
        payload_len: usize,
        fill: F,
    ) -> Result<std::result::Result<u64, E>>
    where
        F: FnOnce(&mut [u8]) -> std::result::Result<(), E>,
    {
        if let Some(cause) = &self.abandoned_write_error {
            return Err(refused_append(cause, &self.active.path));
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
            self.roll_measured()?;
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
        match filled {
            Ok(Ok(())) => {}
            Ok(Err(declined)) => {
                // A failed clean-up is the journal's error: the caller's value is dropped.
                self.abandon(offset, len)?;
                return Ok(Err(declined));
            }
            Err(cause) => {
                // The panic continues even if the clean-up failed; the cause is recorded all the same.
                let _ = self.abandon(offset, len);
                panic::resume_unwind(cause);
            }
        }
        format::finish_record_body(len, body);
        crate::fault::pause_before_publish();
        self.active
            .map
            .store_commit_word(offset, len)
            .ok_or_else(|| internal(&self.active.path, "commit word out of bounds"))?;
        self.offset = offset + format::align8(u64::from(len));
        if PUBLISH {
            self.publish();
        }
        #[cfg(test)]
        if let Some(hook) = tests::AFTER_COMMIT.with(std::cell::Cell::take) {
            hook();
        }
        if let Some(prefault) = &mut self.prefault {
            prefault.advanced(self.offset);
        }
        if let Some(writeback) = &mut self.writeback {
            writeback.advanced(self.offset);
        }
        // Once the active segment is half full, prepare the next one off the writer's thread (ADR 0022; on Linux
        // without a sync and only with `WriterConfig::spare_segment`, ADR 0049).
        if let Some(spares) = &mut self.spares {
            if self.offset >= self.active.size / 2 && !spares.requested() {
                spares.request();
            }
        }
        self.next_seq = seq + 1;
        Ok(Ok(seq))
    }

    /// Abandons the uncommitted record of frame length `len` at `offset` (its fill closure declined or panicked):
    /// zeroes its body and makes the zeroing durable (full platform barrier) before the space can be reused
    /// (`docs/format.md` → Abandoned writes). If that fails, stops the writer ([`Writer::abandoned_write_error`]) and
    /// returns the error that every later append returns. The one clean-up for both ways of abandoning a record.
    #[cold]
    #[inline(never)]
    fn abandon(&mut self, offset: u64, len: u32) -> Result<()> {
        let body_len = u64::from(len) - format::COMMIT_WORD_LEN as u64;
        let zeroed = match self
            .active
            .map
            .bytes_mut(offset + format::COMMIT_WORD_LEN as u64, body_len)
        {
            Some(body) => {
                body.fill(0);
                Ok(())
            }
            None => Err(io::Error::other("record body out of bounds")),
        };
        // The zeroing must be durable (full platform barrier) before this space is reused. Under the lock a
        // checkpoint holds from its first sync to its marker (it waits for a background checkpoint in flight): a
        // failed sync here must keep any later marker from being written (`Context::lost`).
        let synced = {
            let mut lost = checkpoint::lock(&self.ctx.lost);
            let synced = zeroed
                .and_then(|()| crate::fault::inject(crate::fault::Point::AbandonedWriteSync))
                .and_then(|()| self.active.map.sync_range(offset, u64::from(len)))
                .and_then(|()| mmap::sync_data(&self.active.file))
                .and_then(|()| mmap::full_barrier(&self.active.file));
            if synced.is_err() {
                *lost = true;
            }
            synced
        };
        let Err(e) = synced else {
            return Ok(());
        };
        // Stale bytes could resurface after a power loss and be read as a record after a shorter one,
        // so this writer must not reuse the space: refuse further appends and checkpoints.
        self.poisoned = true;
        // The OS error is wrapped (kind `Other`) so that it is never `is_storage_full`.
        let cause = Error::io(
            "sync the zeroing of an abandoned write",
            &self.active.path,
            io::Error::other(e),
        );
        let refused = refused_append(&cause, &self.active.path);
        self.abandoned_write_error = Some(cause);
        Err(refused)
    }

    /// Makes every record appended so far durable, using the platform's sync calls (see
    /// `docs/crash-contract.md` → What `checkpoint()` does). Blocks until they finish.
    /// Configured retention then runs on the calling thread until it has removed what the policy asks for or fails;
    /// its failures are recorded by [`Writer::last_retention_error`] and do not change the successful checkpoint
    /// result or poison the writer.
    ///
    /// If background checkpoints ([`Writer::request_checkpoint`]) are in flight, this first waits for them; it then
    /// checkpoints on the calling thread, as without them.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if a sync call fails. The writer is then poisoned: every later call returns
    /// [`Error::DurabilityLost`], because a failed sync can drop data that a retry
    /// would not rewrite. Appends keep working. If a background checkpoint or the early write-back helper (Linux,
    /// [`WriterConfig::early_writeback`](crate::WriterConfig::early_writeback)) failed and no call has returned its
    /// error yet, this returns that error (and the writer is poisoned).
    pub fn checkpoint(&mut self) -> Result<()> {
        let requested_at = self.request_time();
        self.finish_background()?;
        if self.poisoned {
            return Err(Error::DurabilityLost);
        }
        let result = self.checkpoint_inner(requested_at);
        if result.is_err() {
            self.poisoned = true;
        } else {
            self.run_retention();
        }
        result
    }

    /// Now, if checkpoints are timed.
    fn request_time(&self) -> Option<Instant> {
        self.latency.as_ref().map(|_| Instant::now())
    }

    fn checkpoint_inner(&mut self, requested_at: Option<Instant>) -> Result<()> {
        if self.generation >= u64::MAX - 1 {
            return Err(Error::SequenceExhausted);
        }
        self.generation += 1;
        let job = self.take_job(self.generation, requested_at);
        match checkpoint::run(&job, &self.ctx, On::Writer) {
            Ok(slot) => {
                self.checkpointed = Some(slot);
                let finished = Instant::now();
                self.last_checkpoint_at = Some(finished);
                self.ctx.record_durable(requested_at, finished);
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
    fn take_job(&mut self, generation: u64, requested_at: Option<Instant>) -> Job {
        let mut segments = std::mem::take(&mut self.dirty);
        segments.push(DirtySegment::written(
            Arc::clone(&self.active.file),
            self.active.map.flush_handle(),
            self.active_dirty_from,
            self.offset,
            self.active.path.clone(),
        ));
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
            requested_at,
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
    /// [`Writer::checkpoint_progress`] can report a checkpoint finished while its retention run goes on. The run
    /// stops before its next removal when [`Writer::checkpoint`], [`Writer::close`] or dropping the writer waits for
    /// the thread, once it has handled the first segment of its plan, and when another checkpoint is requested, once
    /// it has handled as many segments as the checkpoint it follows covered (the segments filled since the checkpoint
    /// before, and the one being written) or has taken as long as that checkpoint, at least one segment; the run after
    /// the next checkpoint plans again and continues (ADR 0044). So `close` waits for the removal in progress (and the
    /// planning before it) and a requested checkpoint for at most as many removals as the checkpoint before it covered
    /// and about as long as that checkpoint took, never for a backlog of removals, while retention still removes as
    /// many segments as the writer fills when checkpoints take longer than the interval between requests, unless
    /// removing them takes longer than the checkpoints; segments still eligible when the writer closes stay until a
    /// later retention run (a writer's at open or after a checkpoint, or an archiver's).
    ///
    /// Cost on the calling thread: one lock of the state shared with the checkpoint thread and a few small moves; no
    /// system call, except for starting the thread at the first call, and when the request leaves a checkpoint's list
    /// of dirty segments longer than four (many rolls since the last request, or requests that join one waiting behind
    /// a slow checkpoint): then it reads the identity (`fstat`) of each segment it lets go of, and, if it dropped the
    /// last reference to that segment's file or mapping, closes or unmaps it; the checkpoint later reopens it.
    ///
    /// # Errors
    ///
    /// - The error of an earlier background checkpoint that failed, or of the early write-back helper (Linux), if no
    ///   call has returned it yet. The writer is then poisoned, as after a failed [`Writer::checkpoint`], and nothing
    ///   is requested.
    /// - [`Error::DurabilityLost`] if the writer is poisoned.
    /// - [`Error::Io`] if the checkpoint thread cannot be started. Nothing is requested, and the writer is not
    ///   poisoned.
    /// - [`Error::SequenceExhausted`] if checkpoint generations are exhausted.
    pub fn request_checkpoint(&mut self) -> Result<u64> {
        let requested_at = self.request_time();
        self.collect()?;
        if self.poisoned {
            return Err(Error::DurabilityLost);
        }
        let shared = match &self.background {
            Some(background) => Arc::clone(&background.shared),
            None => {
                let started = Background::start(
                    Arc::clone(&self.ctx),
                    self.config.retention.clone(),
                    self.config.helper_cpus.clone(),
                )
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
                let job = self.take_job(generation, requested_at);
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
                state.queued = Some(self.take_job(self.generation, requested_at));
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
    /// The error of a background checkpoint or of the early write-back helper (Linux) that failed, the first time it
    /// is reported (the writer is then poisoned);
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
    /// - The error of a background checkpoint or of the early write-back helper (Linux) that failed, if no earlier call
    ///   has returned it. The helper is joined first, so an error from a call it was making is included.
    /// - [`Error::DurabilityLost`] if the writer is poisoned: a checkpoint failed earlier (and its error was returned
    ///   then), or an abandoned write could not be zeroed durably ([`Writer::abandoned_write_error`]).
    pub fn close(mut self) -> Result<()> {
        if let Some(background) = &self.background {
            background.wait_idle();
        }
        // Join the early write-back helper first, so that an error from a call it was making is reported here.
        drop(self.writeback.take());
        let result = match self.collect() {
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
    /// results, and the first failure, which is returned (once) and poisons the writer. A failure of the early
    /// write-back helper that no checkpoint has returned yet is such a failure too (ADR 0038 → Early write-back).
    fn collect(&mut self) -> Result<()> {
        let background = self.collect_background();
        let writeback = if self.ctx.writeback_failed.load(Ordering::Acquire) {
            // Taken once, here or by the checkpoint that found `lost` set (whichever comes first).
            checkpoint::lock(&self.ctx.writeback_failure).take()
        } else {
            None
        };
        match background {
            Err(error) => Err(error),
            Ok(()) => match writeback {
                Some(error) if !self.poisoned => {
                    self.poisoned = true;
                    Err(error)
                }
                _ => Ok(()),
            },
        }
    }

    /// The checkpoint thread's part of [`Writer::collect`].
    fn collect_background(&mut self) -> Result<()> {
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

    /// The error that stopped this writer when the zeroing of an abandoned write (a fill closure of
    /// [`append_with`](Writer::append_with) that panicked, or one of [`try_append_with`](Writer::try_append_with)
    /// that declined or panicked) could not be made durable, if that happened.
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

    /// Releases this process's copy of a writer inherited across `fork`. **Call it only in a child process created
    /// by `fork` from the process that opened the writer**; in the process that opened it, drop the writer instead.
    ///
    /// Closes the child's copies of the writer's file descriptors and unmaps its mappings, so that the lock is held
    /// only by the parent's descriptor again and the journal can be reopened once the parent closes its writer. It
    /// does not unlock the lock file (the lock belongs to the open file description that parent and child share),
    /// does not sync, does not remove or change any file or the wake-up region, and does not join or signal the
    /// helper threads, which exist only in the parent.
    ///
    /// # What it leaks
    ///
    /// Until the child exits: the helper threads' handles and the state they share with the writer, including the
    /// prefault thread's reference to the active segment's mapping (so with prefaulting enabled that mapping stays
    /// mapped in the child, although its file descriptor is closed), the checkpoint thread's reference to the
    /// checkpoint marker (after [`Writer::request_checkpoint`]: that descriptor stays open in the child), a spare
    /// segment prepared in the background (its file descriptor and mapping, which the spare thread's result owns),
    /// and on Linux, if the fork came while the early write-back helper held its lock or was starting a write-back,
    /// the segment files it was following (their descriptors stay open in the child).
    ///
    /// # Preconditions
    ///
    /// - **Not async-signal-safe.** It frees heap memory (paths, lists, reference counts of mappings) through the
    ///   global allocator. In the child of a multithreaded process, call it only where the allocator is usable after
    ///   `fork`: the glibc and macOS system allocators reinitialise their locks in the child, but with an allocator
    ///   that does not, a lock held by another thread at the fork can stall the child here, and the journal then stays
    ///   locked by the child's descriptor until the child is killed.
    /// - **No call on the writer may have been in progress in another thread at the moment of the fork.** The child's
    ///   copy could then be half-updated (for example in the middle of a roll), and releasing it is not supported.
    ///   Code that shares a writer between threads must make sure of this itself (the Python bindings, for example,
    ///   never release a copy whose lock was held at the fork).
    /// - **Only in a forked child.** Called in the process that opened the writer, it releases the journal lock
    ///   while the prefault thread and its mapping stay alive, a spare-segment thread may still be running (its
    ///   spare file is then left for the next writer's recovery to remove), waiting readers are not told that the
    ///   writer has gone, and on macOS the wake-up region's shared-memory name stays until the next writer opens the
    ///   journal. Another writer could then open the journal while those threads still run.
    pub fn release_after_fork(mut self) {
        if let Some(spares) = self.spares.take() {
            spares.release_after_fork();
        }
        std::mem::forget(self.prefault.take());
        // In a child, dropping the write-back helper's handle neither stops nor joins it: it only drops the child's
        // references to the segment files the helper follows, if the helper's lock was free at the fork.
        drop(self.writeback.take());
        if let Some(notify) = self.notify.take() {
            notify.release_after_fork();
        }
        // `Drop` finds no spare; the remaining fields only close descriptors and unmap.
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
        let mut poisoned = self.poisoned || self.ctx.writeback_failed.load(Ordering::Relaxed);
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

    /// The writer's latency histograms ([`WriterConfig::latency`](crate::WriterConfig::latency), ADR 0047), or `None` with
    /// [`LatencyMode::Off`](crate::LatencyMode::Off). From 1.1.
    ///
    /// Clone the result, or one of its views, to read it from another thread while this one appends. Calling this costs
    /// nothing on the append path; recording does ([`LatencyMode`](crate::LatencyMode)).
    ///
    /// ```
    /// use tickertape::{LatencyMode, NewRecord, Writer, WriterConfig};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let dir = tempfile::tempdir()?;
    /// # let dir = dir.path().join("journal");
    /// let mut writer = Writer::open(&dir, WriterConfig::default().latency(LatencyMode::All))?;
    /// let Some(latency) = writer.latency().cloned() else {
    ///     return Ok(()); // LatencyMode::Off
    /// };
    /// # let record = NewRecord { schema_id: 1, venue_ts_ns: 0, local_ts_ns: 0, payload: b"hello" };
    /// writer.append(&record)?;
    /// writer.checkpoint()?;
    ///
    /// // On any thread, whenever it is scraped:
    /// for (name, view) in [
    ///     ("append_service_seconds", latency.append()),
    ///     ("roll_seconds", latency.roll()),
    ///     ("time_to_durable_seconds", latency.time_to_durable()),
    /// ] {
    ///     if let Some(view) = view {
    ///         let s = view.snapshot();
    ///         println!("{name}: {} values, p99.9 {:?}, max {:?}", s.count(), s.value_at_quantile(0.999), s.max());
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn latency(&self) -> Option<&WriterLatency> {
        self.latency.as_ref().map(|latency| latency.views())
    }

    fn run_retention(&mut self) {
        let Some((policy, action)) = &self.config.retention else {
            return;
        };
        let result = crate::retention::run(
            &self.root,
            policy,
            action,
            &mut self.retention_report,
            &mut checkpoint::lock(&self.ctx.retention_sizes),
            None,
        );
        if let Err(error) = result {
            self.last_retention_error = Some(error);
            self.retention_failures = self.retention_failures.saturating_add(1);
        }
    }

    /// The journal directory, as given to [`Writer::open`].
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn fits(&self, len: u32) -> bool {
        format::record_fits(self.active.size, self.offset, len)
    }

    /// [`Writer::roll`], timed if the latency mode asks for it.
    #[cold]
    #[inline(never)]
    fn roll_measured(&mut self) -> Result<()> {
        if self.latency.is_none() {
            return self.roll();
        }
        let started = Instant::now();
        self.roll()?;
        if let Some(latency) = &mut self.latency {
            latency.roll(started);
        }
        Ok(())
    }

    /// Rolls to a new segment (`docs/format.md` → Rolling).
    fn roll(&mut self) -> Result<()> {
        let next_base = self.next_seq;
        // No sync here: the new segment's header and size become durable at the next checkpoint, which syncs every
        // segment linked before its request before it writes a marker; recovery restores a header that a crash kept
        // from reaching the disk (ADR 0038 → Rolls without a sync; crash contract C8).
        let size = self.config.segment_size;
        let new = match self.take_spare() {
            Some(spare) => publish_spare(&self.root, next_base, size, spare)?,
            None => {
                // Whether a spare may hold space while this one is created. Asked before: a preparation that has ended
                // by now has given its blocks back or is ready.
                let preparing = self.spares.as_ref().is_some_and(Spares::preparing);
                match create_segment(&self.root, next_base, size, false) {
                    // Only now is the spare worth waiting for: it may hold the space this needed, or, when both
                    // preallocate at the same time, have run out of it and given it back (ADR 0049).
                    Err(full) if full.is_storage_full() => match self.wait_for_spare() {
                        Waited::Ready(spare) => publish_spare(&self.root, next_base, size, spare)?,
                        Waited::Nothing if !preparing => return Err(full),
                        Waited::Freed | Waited::Nothing => {
                            create_segment(&self.root, next_base, size, false)?
                        }
                    },
                    created => created?,
                }
            }
        };
        crate::fault::hook(crate::fault::Point::RollAfterLink);
        if let Some(spares) = &mut self.spares {
            spares.rolled();
        }
        self.dirty_dirs.insert(self.root.clone());
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
        if let Some(writeback) = &mut self.writeback {
            writeback.switch(
                Arc::clone(&old.file),
                seal_at + format::SEAL_LEN as u64,
                Arc::clone(&self.active.file),
                format::SEGMENT_HEADER_LEN as u64,
            );
        }
        // The list keeps only the newest few segments open: pushing this one releases the file and mapping of the one
        // that leaves its window (`checkpoint::OPEN_SEGMENTS`), and the checkpoint that covers that one reopens it.
        self.dirty.push(DirtySegment::written(
            old.file,
            old.map.flush_handle(),
            self.active_dirty_from,
            seal_at + format::SEAL_LEN as u64,
            old.path,
        ));
        self.offset = format::SEGMENT_HEADER_LEN as u64;
        self.active_dirty_from = self.offset;
        if let Some(prefault) = &mut self.prefault {
            prefault.switch(self.active.map.advice(), self.offset);
        }
        self.publish();
        Ok(())
    }
}

impl Writer {
    /// Returns the prepared spare, if any, if it is usable for this journal's segment size. On macOS it waits for one
    /// still being prepared; on Linux never ([`Spares::take`]).
    fn take_spare(&mut self) -> Option<Spare> {
        let spare = self.spares.as_mut()?.take()?;
        self.usable(spare)
    }

    /// As [`Writer::take_spare`], but waits for a spare being prepared ([`Spares::wait`]).
    fn wait_for_spare(&mut self) -> Waited {
        let Some(spares) = &mut self.spares else {
            return Waited::Nothing;
        };
        match spares.wait() {
            // Discarding one of another size frees its space.
            Waited::Ready(spare) => self.usable(spare).map_or(Waited::Freed, Waited::Ready),
            other => other,
        }
    }

    /// The spare, if it has this journal's segment size; otherwise it is discarded.
    fn usable(&self, spare: Spare) -> Option<Spare> {
        if spare.size == self.config.segment_size {
            return Some(spare);
        }
        spare.discard(&self.root.join(journal::SPARE_FILE));
        None
    }

    /// Test support: whether no spare segment is being prepared.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn spare_settled(&self) -> bool {
        self.spares.as_ref().is_none_or(Spares::settled)
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
            drop(self.spares.take());
            drop(self.prefault.take());
            drop(self.writeback.take());
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

/// Publishes a prepared spare as segment `base` (ADR 0022): write its header, then link it into place, as
/// [`create_segment`] does at a roll. The header is not synced here: the next checkpoint syncs it before its marker
/// (and the file's size, which only macOS syncs when it prepares the spare). If the header can't be written, the spare
/// is discarded and the segment is created synchronously instead; a failed link is reported like [`create_segment`]'s.
fn publish_spare(dir: &Path, base: u64, size: u64, spare: Spare) -> Result<Segment> {
    let spare_path = dir.join(journal::SPARE_FILE);
    let path = journal::segment_path(dir, base);
    let header = format::encode_segment_header(&SegmentHeader {
        segment_size: size,
        base_seq: base,
        created_unix_ns: crate::clock::realtime_ns(),
    });
    let prepared = crate::fault::inject(crate::fault::Point::SpareHeader)
        .and_then(|()| spare.file.write_all_at(&header, 0));
    if prepared.is_err() {
        spare.discard(&spare_path);
        return create_segment(dir, base, size, false);
    }
    if let Err(e) = crate::fault::inject(crate::fault::Point::Link)
        .and_then(|()| fs::hard_link(&spare_path, &path))
    {
        spare.discard(&spare_path);
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

/// The error of an append refused because the zeroing of an abandoned write could not be made durable (`cause`, the
/// writer's [`Writer::abandoned_write_error`]): an [`Error::Io`] with the cause's OS error. Returned by every append
/// from then on, and by the `try_append_with` call whose decline left that clean-up failed.
#[cold]
#[inline(never)]
fn refused_append(cause: &Error, path: &Path) -> Error {
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
    // Wrapped, like the cause, so that the OS error (say ENOSPC) is never `is_storage_full`: freeing space does not
    // make this writer accept appends again.
    Error::io(
        "append (refused: an earlier abandoned write could not be zeroed durably; reopen the journal)",
        path,
        io::Error::other(source),
    )
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
///
/// With `sync`, the header and the preallocated size are made durable, with the platform barrier, before the link, as
/// recovery needs for a segment it creates (a new journal's first segment, a completed roll). A roll passes `false`:
/// the writer's thread
/// does not wait for the sync there, and the next checkpoint syncs the segment before its marker (ADR 0038 → Rolls
/// without a sync).
fn create_segment(dir: &Path, base: u64, size: u64, sync: bool) -> Result<Segment> {
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
        if sync {
            // Durable on the drive before the link publishes it (macOS: `sync_data` leaves it in the drive's cache):
            // a new journal's first segment whose header a power loss kept from the disk would make every later open
            // fail, since recovery never restores the header of the segment it starts in.
            mmap::sync_data(&file)
                .and_then(|()| mmap::full_barrier(&file))
                .map_err(|e| Error::io("sync segment", &tmp, e))?;
        }
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
            // A partial preallocation keeps its blocks on some filesystems (ext4): they go back before a retry.
            journal::discard_unpublished(&file, &tmp);
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
    dirty: DirtySegments,
}

/// How [`Recovery::restore_unwritten_header`] restores a segment whose header never reached the disk.
#[derive(Debug, Clone, Copy)]
enum Restore {
    /// Write the header again; the file's length `len` is a valid segment size.
    Header { len: u64 },
    /// Replace the file, which holds only zeros and whose length is not a valid segment size, by a new empty segment.
    Replace,
}

/// A change to the journal that recovery under [`RecoveryPolicy::Fail`] holds back once it has decided to restore a
/// header, until the open can no longer refuse (crash contract C11; [`Recovery::commit_held`]).
enum Held {
    /// Write the header of the segment for `base`, whose file at `path` (identity `id`) has the valid segment size
    /// `len`, again. The file is not kept open: a scan may hold back many of these (one per restored header), and
    /// each is reopened when it is made.
    Header {
        id: FileId,
        path: PathBuf,
        base: u64,
        len: u64,
    },
    /// Rename the segment prepared and synced under `tmp` over the all-zero file at `path` (`Restore::Replace`).
    Rename { tmp: PathBuf, path: PathBuf },
    /// Publish the segment prepared and synced under `tmp` as `path`, which does not exist: a created successor.
    Link { tmp: PathBuf, path: PathBuf },
    /// Zero `len` bytes at `start` of the segment at `path` (identity `id`, of size `size`): an uncommitted tail.
    Zero {
        id: FileId,
        path: PathBuf,
        size: u64,
        start: u64,
        len: u64,
    },
    /// Write a seal for `next_seq` at `offset` of the segment at `path` (identity `id`, of size `size`).
    Seal {
        id: FileId,
        path: PathBuf,
        size: u64,
        offset: u64,
        next_seq: u64,
    },
}

/// Recovery context (`docs/format.md` → Recovery).
struct Recovery<'a> {
    dir: &'a Path,
    config: &'a WriterConfig,
    report: &'a mut RecoveryReport,
    dirty_dirs: BTreeSet<PathBuf>,
    /// Under [`RecoveryPolicy::Fail`]: every change the scan decided from its first restored header on, in order, not
    /// yet made (C11). Empty under `Quarantine`, which makes each change when it decides it.
    held: Vec<Held>,
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

    /// Runs the scan. Under `Fail`, a scan that restores a header holds back that change and every later one
    /// ([`Held`]) and makes them only once nothing can refuse the open any more; when the open is refused, or fails,
    /// before they are all made, the segments it prepared under temporary names are removed (an interrupted removal
    /// leaves temporary files, which the next open removes).
    fn recover(&mut self, checkpoint: Option<CheckpointSlot>) -> Result<RecoveredState> {
        let result = self.scan(checkpoint);
        for held in self.held.drain(..) {
            if let Held::Rename { tmp, .. } | Held::Link { tmp, .. } = held {
                let _ = fs::remove_file(tmp);
            }
        }
        result
    }

    fn scan(&mut self, checkpoint: Option<CheckpointSlot>) -> Result<RecoveredState> {
        let segments = journal::list_segments(self.dir)?;
        let bases: BTreeSet<u64> = segments.iter().map(|(b, _)| *b).collect();
        if bases.is_empty() {
            if let Some(c) = checkpoint {
                return Err(self.dangling(c));
            }
            // Nothing is left that an index could belong to.
            let mut swept = seek_index::dir::sweep(self.dir, 0, None);
            self.count_swept(&mut swept);
            let active = create_segment(self.dir, 0, self.config.segment_size, true)?;
            journal::sync_dir(self.dir)?;
            mmap::full_barrier(&active.file)
                .map_err(|e| Error::io("full sync segment", &active.path, e))?;
            self.report.actions.push(RecoveryAction::Created);
            return Ok(RecoveredState {
                active,
                offset: format::SEGMENT_HEADER_LEN as u64,
                next_seq: 0,
                active_dirty_from: format::SEGMENT_HEADER_LEN as u64,
                dirty: DirtySegments::default(),
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
            // The record before the marker's position must be there and end exactly at it (ADR 0056): otherwise the
            // marker vouches for records the segment does not hold (segments restored from a backup older than the
            // marker, or the last covered record damaged), and appending at the marker would make that gap part of
            // the journal. Refused under every policy, before anything is changed.
            if !format::marker_reached(&RwView(&seg.map), &c) {
                return Err(Error::corrupt(
                    &seg.path,
                    offset,
                    CorruptReason::DanglingCheckpoint(c.segment_base_seq),
                ));
            }
        }
        // Step 2a: the seek indexes of every segment recovery may change go before it changes anything (ADR 0045, T4).
        let mut swept = seek_index::dir::sweep(
            self.dir,
            checkpoint.map_or(0, |c| c.segment_base_seq),
            bases.first().copied(),
        );
        let mut visited = BTreeSet::new();
        // Segments after the marker, synced by the first checkpoint; only the newest few stay open, so a scan of any
        // number of segments holds a bounded number of descriptors.
        let mut dirty = DirtySegments::default();
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
                    // A segment for exactly `seq` that is not this one (at the first record slot `seq` is this
                    // segment's own base) and has not been visited.
                    let successor_pending =
                        seq != seg.base && bases.contains(&seq) && !visited.contains(&seq);
                    if successor_pending {
                        // "Does not open" means the successor itself is invalid: `Corrupt` (a short file, a bad
                        // header, a size or base mismatch) or `UnsupportedVersion`, as in the seal branch below. Any
                        // other error (an I/O error opening, reading or mapping it: `EACCES`, `EMFILE`, `ENOMEM`) says
                        // nothing about the journal and is returned here, before anything is changed, under both
                        // policies.
                        // A successor whose header never reached the disk counts as opening once its header is
                        // restored (under `Fail` that change is held back until nothing can refuse the open, C11).
                        let opened = match self.open_successor(seq) {
                            Ok(next) => Some(next),
                            Err(Error::Corrupt { .. } | Error::UnsupportedVersion { .. }) => None,
                            Err(e) => return Err(e),
                        };
                        if let Some(next) = opened {
                            // The successor for exactly `seq` exists, is not this segment (at the first record slot
                            // `seq` is this segment's own base), has not been visited, and opens and maps: `offset`
                            // is where this segment's seal belongs, and the chain continues in the successor (ADR
                            // 0035, "Damage around a seal"). A valid header alone is not enough: `open_segment`
                            // must succeed.
                            let beyond_seal =
                                tail_end.is_some_and(|end| end > offset + format::SEAL_LEN as u64);
                            if beyond_seal && self.config.recovery == RecoveryPolicy::Fail {
                                return Err(Error::corrupt(
                                    &seg.path,
                                    offset,
                                    CorruptReason::BadSeal,
                                ));
                            }
                            if beyond_seal {
                                // Bytes beyond the seal: no crash leaves them (a roll writes nothing after the seal,
                                // and records `seq` onwards are in the successor), so this is a damaged seal, not
                                // missing history, and later segments are not evidence. They are never records of
                                // this segment either: those would repeat the successor's sequence numbers. Preserve
                                // them as any repair does, then reseal and continue into the successor in this open.
                                // This is exactly where an interrupted repair leaves the next open (the segment ends
                                // at `offset`, followed by the same branch below), so an interruption at any point
                                // gives the same journal. `Fail` refuses (above, before anything is changed): the
                                // bytes are damage.
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
                        committed_after_empty(&seg, offset, seq, end, &bases, successor_pending)
                    }) {
                        // Evidence of committed history, or a search stopped at its work bound: a committed tail (ADR
                        // 0035; usually storage damage, but a torn payload can lead here too, crash contract C1).
                        if self.config.recovery == RecoveryPolicy::Fail {
                            return Err(Error::corrupt(&seg.path, offset, reason));
                        }
                        seg = self.repair_tail(seg, offset, reason)?;
                        break;
                    }
                    if successor_pending && tail_end.is_some() {
                        // The successor for exactly `seq` exists but did not open (it would have been entered above),
                        // and the search found no evidence: the chain is broken here, at the seal's place, and cannot
                        // continue. The non-zero bytes after the commit word can be the only ones left for diagnosis
                        // or salvage, so they are a corrupt tail, not an uncommitted one:
                        // `Fail` refuses before changing a byte of this segment, and `Quarantine` preserves them and
                        // replaces the segment (ADR 0035, "Damage around a seal"). Either way it is reported as
                        // `BrokenChain` at `offset`, where `verify` reports this state. The successor and every later
                        // segment are then disconnected and quarantined below. An interrupted repair leaves this
                        // segment ending at `offset` with nothing after it, which the next open handles as below, with
                        // the same result.
                        let reason = CorruptReason::BrokenChain(seq);
                        if self.config.recovery == RecoveryPolicy::Fail {
                            return Err(Error::corrupt(&seg.path, offset, reason));
                        }
                        seg = self.repair_tail(seg, offset, reason)?;
                        break;
                    }
                    // The search completed without evidence: a torn append or the clean end, or damage that left no
                    // evidence (ADR 0035, "What cannot be detected"). In front of a successor for exactly `seq` that
                    // does not open, nothing after the commit word is non-zero here (that case is handled above), so
                    // nothing is zeroed, the chain ends at `offset`, and the successor is quarantined below, or
                    // refused as disconnected under `Fail`.
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
                        match self.open_successor(seq) {
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
                    // Crash or power loss during a roll: create the successor (held back under `Fail` after a
                    // restored header, C11).
                    let next = if self.held.is_empty() {
                        let next = create_segment(self.dir, seq, self.config.segment_size, true)?;
                        journal::sync_dir(self.dir)?;
                        next
                    } else {
                        let (tmp, next) = self.prepare_segment(seq, self.config.segment_size)?;
                        self.held.push(Held::Link {
                            tmp,
                            path: next.path.clone(),
                        });
                        next
                    };
                    self.dirty_dirs.insert(self.dir.to_path_buf());
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
        // Nothing can refuse the open from here on.
        self.commit_held()?;
        if !self.report.actions.is_empty() {
            for d in &self.dirty_dirs {
                journal::sync_dir(d)?;
            }
            mmap::full_barrier(&seg.file)
                .map_err(|e| Error::io("full sync segment", &seg.path, e))?;
        } else {
            // The removal of indexes reaches the drive before anything is appended; best effort, as the removal is.
            // Through the directory's own descriptor, never the segment's: a sync here that fails would consume the
            // write-back error that the first checkpoint must report (crash contract, "Errors are sticky").
            swept.barrier();
        }
        self.count_swept(&mut swept);

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

    /// After recovery: removes the seek indexes of segments that are gone, and reports what the removal did.
    fn count_swept(&mut self, swept: &mut seek_index::dir::Swept) {
        swept.remove_orphans();
        (
            self.report.seek_indexes_removed,
            self.report.seek_index_errors,
        ) = swept.counts();
    }

    /// Opens the segment for `base` that the scan enters after the segment it scanned (named by a valid seal, or the
    /// successor for exactly the expected sequence number at the seal's place). A roll does not sync the new
    /// segment's header (ADR 0038 → Rolls without a sync), so after an OS crash or power loss its name can be on disk
    /// while its header is not: its first 64 bytes read as zeros. Such a header is written again (base from the
    /// name, size from the file, creation time now) and synced, and the segment opens as if it had reached the disk;
    /// a file that is shorter than a header or whose length is not a valid segment size, and that holds only zeros,
    /// is replaced by a new empty segment for `base`. Both are reported as [`RecoveryAction::RestoredHeader`]. Under
    /// `Fail` the change is held back ([`Held`]) and the segment is scanned as if it had been made. Any other failure
    /// to open is returned unchanged.
    fn open_successor(&mut self, base: u64) -> Result<Segment> {
        match open_segment(self.dir, base) {
            Err(e @ Error::Corrupt { .. }) => match self.probe_unwritten_header(base)? {
                Some(restore) => self.restore_unwritten_header(base, restore),
                None => Err(e),
            },
            other => other,
        }
    }

    /// See [`Recovery::open_successor`]: whether the segment for `base`, which did not open, has a header that never
    /// reached the disk, and how it would be restored. Reads only.
    fn probe_unwritten_header(&mut self, base: u64) -> Result<Option<Restore>> {
        let path = journal::segment_path(self.dir, base);
        let file = File::open(&path).map_err(|e| Error::io("open segment", &path, e))?;
        let len = file
            .metadata()
            .map_err(|e| Error::io("stat segment", &path, e))?
            .len();
        let head_len = len.min(format::SEGMENT_HEADER_LEN as u64);
        if !all_zero(&file, &path, 0, head_len)? {
            return Ok(None);
        }
        if len >= format::SEGMENT_HEADER_LEN as u64 && format::validate_segment_size(len).is_ok() {
            return Ok(Some(Restore::Header { len }));
        }
        if !all_zero(&file, &path, head_len, len)? {
            return Ok(None);
        }
        Ok(Some(Restore::Replace))
    }

    /// Restores the header of the segment for `base` as `restore` says (see [`Recovery::open_successor`]), reports it
    /// and opens the segment. Under `Fail` the change is held back: the segment is mapped from its file as it is
    /// (`Restore::Header`; the scan does not read the header), or from the prepared replacement (`Restore::Replace`).
    ///
    /// A file replaced by a new empty segment is replaced by renaming a prepared, synced copy over it, as a tail repair
    /// does, so that the name always holds a segment for `base`: an interruption leaves either the old all-zero file,
    /// which the next open restores in the same way, or the new segment, and at most a temporary file, which the next
    /// open removes.
    fn restore_unwritten_header(&mut self, base: u64, restore: Restore) -> Result<Segment> {
        let path = journal::segment_path(self.dir, base);
        let hold = self.config.recovery == RecoveryPolicy::Fail;
        let seg = match restore {
            Restore::Header { len } => {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .map_err(|e| Error::io("open segment", &path, e))?;
                if hold {
                    let map = usize::try_from(len)
                        .map_err(|_| io::Error::other("segment too large"))
                        .and_then(|len| MapRw::new(&file, len))
                        .map_err(|e| Error::io("map segment", &path, e))?;
                    let id = FileId::of(&file).map_err(|e| Error::io("stat segment", &path, e))?;
                    let file = Arc::new(file);
                    self.held.push(Held::Header {
                        id,
                        path: path.clone(),
                        base,
                        len,
                    });
                    Segment {
                        base,
                        size: len,
                        path: path.clone(),
                        file,
                        map,
                    }
                } else {
                    write_header(&file, &path, base, len)?;
                    drop(file);
                    open_segment(self.dir, base)?
                }
            }
            Restore::Replace => {
                let (tmp, seg) = self.prepare_segment(base, self.config.segment_size)?;
                if hold {
                    self.held.push(Held::Rename {
                        tmp,
                        path: path.clone(),
                    });
                } else {
                    self.rename_prepared(&tmp, &path)?;
                }
                seg
            }
        };
        self.report
            .actions
            .push(RecoveryAction::RestoredHeader { segment: path });
        Ok(seg)
    }

    /// Prepares a new empty segment for `base` of `size` bytes under its temporary name, synced and mapped, not yet
    /// under its name ([`Recovery::rename_prepared`] does that). Returns the temporary name and the segment, whose
    /// `path` is already its name.
    fn prepare_segment(&mut self, base: u64, size: u64) -> Result<(PathBuf, Segment)> {
        let tmp = journal::tmp_path(self.dir, base);
        let _ = fs::remove_file(&tmp);
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
            write_header(&file, &tmp, base, size)?;
            usize::try_from(size)
                .map_err(|_| io::Error::other("segment too large"))
                .and_then(|len| MapRw::new(&file, len))
                .map_err(|e| Error::io("map segment", &tmp, e))
        })();
        let map = match prepared {
            Ok(map) => map,
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        };
        let seg = Segment {
            base,
            size,
            path: journal::segment_path(self.dir, base),
            file: Arc::new(file),
            map,
        };
        Ok((tmp, seg))
    }

    /// Renames a segment prepared by [`Recovery::prepare_segment`] to its name, durably.
    fn rename_prepared(&mut self, tmp: &Path, path: &Path) -> Result<()> {
        crate::fault::inject(crate::fault::Point::RestoreBeforeRename)
            .map_err(|e| Error::io("replace segment", path, e))?;
        fs::rename(tmp, path).map_err(|e| Error::io("replace segment", path, e))?;
        journal::sync_dir(self.dir)?;
        self.dirty_dirs.insert(self.dir.to_path_buf());
        Ok(())
    }

    /// Makes the changes held back under `Fail` ([`Held`]), in the order the scan decided them. Each is a change the
    /// scan would have made at that point under `Quarantine`, so an interruption leaves the journal an interrupted
    /// `Quarantine` scan leaves at the same point, which the next open handles the same way.
    fn commit_held(&mut self) -> Result<()> {
        let held = std::mem::take(&mut self.held);
        let result = held.iter().enumerate().try_for_each(|(n, change)| {
            if n > 0 {
                crate::fault::inject(crate::fault::Point::BetweenHeldChanges)
                    .map_err(|e| Error::io("recover", self.dir, e))?;
            }
            self.make_held(change)
        });
        if result.is_err() {
            // `recover` removes the temporary files of the changes not made (the others no longer exist).
            self.held = held;
        }
        result
    }

    /// Makes one change held back under `Fail` (see [`Recovery::commit_held`]).
    fn make_held(&mut self, change: &Held) -> Result<()> {
        match change {
            Held::Header {
                id,
                path,
                base,
                len,
            } => write_header(&reopen_held(path, *id)?, path, *base, *len),
            Held::Rename { tmp, path } => self.rename_prepared(tmp, path),
            Held::Link { tmp, path } => {
                // As `create_segment` publishes: a link that replaces nothing, then the temporary name removed (a
                // leftover one is removed by the next open).
                fs::hard_link(tmp, path).map_err(|e| Error::io("publish segment", path, e))?;
                let _ = fs::remove_file(tmp);
                journal::sync_dir(self.dir)
            }
            Held::Zero {
                id,
                path,
                size,
                start,
                len,
            } => {
                let file = reopen_held(path, *id)?;
                zero_bytes(
                    &mut map_held(&file, path, *size)?,
                    &file,
                    path,
                    *start,
                    *len,
                )
            }
            Held::Seal {
                id,
                path,
                size,
                offset,
                next_seq,
            } => {
                let file = reopen_held(path, *id)?;
                put_seal(
                    &mut map_held(&file, path, *size)?,
                    &file,
                    path,
                    *offset,
                    *next_seq,
                )
            }
        }
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
        if self.held.is_empty() {
            zero_bytes(&mut seg.map, &seg.file, &seg.path, start, len)?;
        } else {
            self.held.push(Held::Zero {
                id: held_id(seg)?,
                path: seg.path.clone(),
                size: seg.size,
                start,
                len,
            });
        }
        self.report.actions.push(RecoveryAction::ZeroedUncommitted {
            segment: seg.path.clone(),
            offset: start,
            len,
        });
        Ok(())
    }

    /// Writes a seal for `next_seq` at `offset` and syncs it (held back under `Fail` after a restored header, C11).
    fn write_seal(&mut self, seg: &mut Segment, offset: u64, next_seq: u64) -> Result<()> {
        if self.held.is_empty() {
            return put_seal(&mut seg.map, &seg.file, &seg.path, offset, next_seq);
        }
        self.held.push(Held::Seal {
            id: held_id(seg)?,
            path: seg.path.clone(),
            size: seg.size,
            offset,
            next_seq,
        });
        Ok(())
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
        // The copy holds the records before the marker. On macOS `sync_data` does not flush the drive's cache, so
        // without this barrier a power loss after the rename could keep the new name and lose those bytes, and the next
        // open would refuse the marker (ADR 0056). Linux's `fdatasync` already flushed them.
        mmap::full_barrier(&file).map_err(|e| Error::io("full sync repaired segment", &tmp, e))?;
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
/// The caller has already handled a successor for exactly `seq` that opens (a seal belongs at `offset`).
/// `successor_pending` says that one exists, is not `seg` and has not been visited, so here it did not open. A later
/// segment is still not evidence when the non-zero bytes then end within a seal at `offset` (what a crash between
/// publishing the successor and sealing leaves): the caller reports that tail as a chain broken at `offset`.
/// Writes a segment header for `base` and size `len` at the start of `file` and syncs it.
fn write_header(file: &File, path: &Path, base: u64, len: u64) -> Result<()> {
    let header = format::encode_segment_header(&SegmentHeader {
        segment_size: len,
        base_seq: base,
        created_unix_ns: crate::clock::realtime_ns(),
    });
    file.write_all_at(&header, 0)
        .map_err(|e| Error::io("write segment header", path, e))?;
    mmap::sync_data(file).map_err(|e| Error::io("sync segment", path, e))
}

/// The identity of `seg`'s file, for a change held back on it ([`Held`]).
fn held_id(seg: &Segment) -> Result<FileId> {
    FileId::of(&seg.file).map_err(|e| Error::io("stat segment", &seg.path, e))
}

/// Opens the segment at `path` again to make a held-back change ([`Held`]), checking that it is still the file `id`
/// was read from.
fn reopen_held(path: &Path, id: FileId) -> Result<File> {
    checkpoint::reopen(path, id).map_err(|e| Error::io("open segment", path, e))
}

/// Maps the segment in `file` (of size `size`) to make a held-back change ([`Held`]).
fn map_held(file: &File, path: &Path, size: u64) -> Result<MapRw> {
    usize::try_from(size)
        .map_err(|_| io::Error::other("segment too large"))
        .and_then(|len| MapRw::new(file, len))
        .map_err(|e| Error::io("map segment", path, e))
}

/// Zeroes `len` bytes at `start` of a mapped segment and syncs them.
fn zero_bytes(map: &mut MapRw, file: &File, path: &Path, start: u64, len: u64) -> Result<()> {
    map.bytes_mut(start, len)
        .ok_or_else(|| Error::io("zero tail", path, io::Error::other("out of bounds")))?
        .fill(0);
    map.sync_range(start, len)
        .and_then(|()| mmap::sync_data(file))
        .map_err(|e| Error::io("sync zeroed tail", path, e))
}

/// Writes a seal for `next_seq` at `offset` of a mapped segment (its CRC word first, then its marker, so a reader
/// never sees the marker without the CRC) and syncs it.
fn put_seal(map: &mut MapRw, file: &File, path: &Path, offset: u64, next_seq: u64) -> Result<()> {
    let oob = || Error::io("write seal", path, io::Error::other("out of bounds"));
    map.bytes_mut(offset + format::COMMIT_WORD_LEN as u64, 4)
        .ok_or_else(oob)?
        .copy_from_slice(&format::seal_crc(next_seq).to_le_bytes());
    map.store_commit_word(offset, format::SEAL_MARKER)
        .ok_or_else(oob)?;
    map.sync_range(offset, format::SEAL_LEN as u64)
        .and_then(|()| mmap::sync_data(file))
        .map_err(|e| Error::io("sync seal", path, e))
}

/// Whether bytes `[from, to)` of `file` are all zero (read in chunks; used only for a segment whose header is
/// missing).
fn all_zero(file: &File, path: &Path, from: u64, to: u64) -> Result<bool> {
    let mut buf = vec![0u8; 1 << 16];
    let mut at = from;
    while at < to {
        let n = usize::try_from((to - at).min(buf.len() as u64)).unwrap_or(buf.len());
        file.read_exact_at(&mut buf[..n], at)
            .map_err(|e| Error::io("read segment", path, e))?;
        if buf[..n].iter().any(|&b| b != 0) {
            return Ok(false);
        }
        at += n as u64;
    }
    Ok(true)
}

fn committed_after_empty(
    seg: &Segment,
    offset: u64,
    seq: u64,
    tail_end: u64,
    bases: &BTreeSet<u64>,
    successor_pending: bool,
) -> Option<CorruptReason> {
    if let Some(found) = format::find_history_before(&RwView(&seg.map), offset, seq, tail_end) {
        return Some(found.reason());
    }
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
/// [`Writer::metrics`] (`uncheckpointed_bytes`). The segment's mapping is dropped here.
fn inherited(seg: Segment, from: u64, seal_at: u64) -> DirtySegment {
    DirtySegment {
        from,
        to: seal_at + format::SEAL_LEN as u64,
        file: Some(seg.file),
        flush: None,
        id: None,
        mapped: false,
        path: seg.path,
    }
}

/// [`Writer::append_batch`] stopped at a record it could not append.
///
/// The first [`appended`](BatchError::appended) records of the batch are committed, with sequence numbers
/// `first_seq..first_seq + appended`; the record at index `appended` and the records after it are not. To retry once
/// the cause is resolved, append again from index `appended`.
#[derive(Debug, thiserror::Error)]
#[error("append_batch stopped at record {appended} after appending {appended} record(s)")]
#[non_exhaustive]
pub struct BatchError {
    /// How many records of the batch were appended, which is also the index of the record that failed.
    pub appended: usize,
    /// The sequence number of the batch's first record: [`Writer::next_seq`] when the call began.
    pub first_seq: u64,
    /// Why the record at index `appended` was not appended: an error [`Writer::append`] can return.
    #[source]
    pub error: Error,
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
    /// Names removed from `seek-index/` (ADR 0045): the seek indexes of segments that recovery may change, those of
    /// segments that are gone, and indexes left unfinished. Not repairs, so not in [`RecoveryReport::actions`]. Added
    /// in 1.1.
    pub seek_indexes_removed: u64,
    /// What failed of that removal (a name that could not be removed, a listing, a sync). It never fails the open: an
    /// index that stays is still checked against its segment by every reader that uses it. Added in 1.1.
    pub seek_index_errors: u64,
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
    /// or the CRC word of a seal recovery is about to write, which no reader saw, but after storage damage that left
    /// no evidence they can be published records a reader has already returned (C11 (3)). Bytes after an empty commit
    /// word in front of a segment for the expected sequence number that exists but does not open are not zeroed: when
    /// the search finds no evidence of committed history after the word, they are preserved as
    /// [`RecoveryAction::RepairedTail`] with [`CorruptReason::BrokenChain`](crate::CorruptReason::BrokenChain) (C11
    /// (3)); when it finds some (other bytes after the seal's place and a segment with a higher base, for example),
    /// they are preserved as for any evidence, with
    /// [`CorruptReason::EmptyCommitWord`](crate::CorruptReason::EmptyCommitWord) or
    /// [`CorruptReason::UncheckedTail`](crate::CorruptReason::UncheckedTail) (C11 (2)).
    #[non_exhaustive]
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
    /// [`CorruptReason::UncheckedTail`](crate::CorruptReason::UncheckedTail)); at the place of a seal with damage
    /// beyond it ([`CorruptReason::BadSeal`](crate::CorruptReason::BadSeal)); or at an empty commit word with
    /// non-zero bytes after it in front of a segment for the expected sequence number that exists but does not open,
    /// when the search finds no evidence after it ([`CorruptReason::BrokenChain`](crate::CorruptReason::BrokenChain);
    /// `docs/crash-contract.md`, C11 (3)). What followed `offset` leaves the segment; the quarantine file keeps its
    /// bytes up to the last non-zero one.
    #[non_exhaustive]
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
    #[non_exhaustive]
    QuarantinedSegment {
        /// Where the segment was.
        segment: PathBuf,
        /// Where it is now.
        quarantine_file: PathBuf,
        /// Why it was removed from the chain.
        reason: crate::CorruptReason,
    },
    /// The previous segment was missing its seal (crash during a roll) and was sealed.
    #[non_exhaustive]
    Sealed {
        /// The segment that was sealed.
        segment: PathBuf,
    },
    /// The newest segment was sealed but its successor was missing; the successor was created.
    #[non_exhaustive]
    CompletedRoll {
        /// The new segment.
        segment: PathBuf,
    },
    /// A segment that the scan entered after the previous one had no header: its first 64 bytes were zeros, because
    /// a roll does not sync the new segment's header and an OS crash or power loss kept it from reaching the disk
    /// before a checkpoint did (ADR 0038 → Rolls without a sync; `docs/crash-contract.md`, C8). Its header was
    /// written again (base from the name, size from the file, creation time at recovery) and synced; a segment
    /// holding only zeros whose length was not a valid segment size was replaced by a new empty segment instead.
    /// The scan then continued into it as into any segment.
    #[non_exhaustive]
    RestoredHeader {
        /// The segment.
        segment: PathBuf,
    },
    /// A stale `*.tape.tmp` file from an interrupted roll was removed.
    #[non_exhaustive]
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
        /// Called (once) by an append on this thread right after a record's commit (and, for a single-record append,
        /// its wake-up), before the prefault, write-back and spare-segment work.
        pub(super) static AFTER_COMMIT: Cell<Option<Box<dyn FnOnce()>>> = const { Cell::new(None) };
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

    /// A forked child that releases its copy of a writer whose early write-back helper runs (Linux; ADR 0038) closes
    /// its descriptor of the active segment, which the helper's shared state also refers to, without stopping,
    /// joining or signalling the helper, which does not exist in the child. The parent's helper is left as it was.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_forked_child_releasing_its_copy_closes_the_writeback_helpers_files() {
        const CLOSED: i32 = 42;
        const STILL_OPEN: i32 = 43;
        let _forked = forked::LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let config = WriterConfig::default()
            .segment_size(1 << 20)
            .prefault(true)
            .early_writeback(true)
            .notify(false);
        let mut writer = Some(Writer::open(dir.path(), config.clone()).unwrap());
        let open = writer.as_mut().unwrap();
        append_one(open);
        assert!(
            open.writeback.is_some(),
            "the helper must run for this test"
        );
        let fd = open.active.file.as_raw_fd();
        // Fork while the helper waits to be woken: it then holds no file of its own, and its lock is free.
        drop(open.writeback.as_ref().unwrap().test_handle().idle());
        let child = forked::fork(|| {
            writer.take().unwrap().release_after_fork();
            forked::exit(if forked::is_open(fd) {
                STILL_OPEN
            } else {
                CLOSED
            });
        })
        .unwrap();
        assert_eq!(
            child.wait_within(std::time::Duration::from_secs(20)),
            Some(CLOSED),
            "the child must close its descriptor of the segment the helper follows"
        );
        let mut writer = writer.unwrap();
        for _ in 0..3000 {
            append_one(&mut writer);
        }
        writer.close().unwrap();
        assert_eq!(Writer::open(dir.path(), config).unwrap().next_seq(), 3001);
    }

    /// A forked child that releases its copy of a writer whose early write-back helper's lock was held at the fork
    /// (here by this test) must not wait for that lock, which stays held in the child for good (Linux; ADR 0038).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_forked_child_releasing_its_copy_leaves_a_writeback_lock_held_at_the_fork_alone() {
        let _forked = forked::LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let config = WriterConfig::default()
            .segment_size(1 << 20)
            .prefault(true)
            .early_writeback(true)
            .notify(false);
        let mut writer = Some(Writer::open(dir.path(), config.clone()).unwrap());
        let open = writer.as_mut().unwrap();
        append_one(open);
        let handle = open.writeback.as_ref().unwrap().test_handle();
        let held = handle.idle();
        let child = forked::fork(|| writer.take().unwrap().release_after_fork()).unwrap();
        drop(held);
        assert_eq!(
            child.wait_within(std::time::Duration::from_secs(20)),
            Some(0),
            "the child must not wait for the write-back helper's lock"
        );
        let mut writer = writer.unwrap();
        append_one(&mut writer);
        writer.close().unwrap();
        assert_eq!(Writer::open(dir.path(), config).unwrap().next_seq(), 2);
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
        drop(create_segment(dir.path(), 500, 1 << 20, true).unwrap());
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

    /// By default (`writeback::STOP_AT_FOLIO_BOUNDARY`, from 1.1), a writer's early write-back helper never starts a
    /// range that reaches into the 128 KiB block that holds the position it was given, and every segment the writer
    /// rolled past is written back from its first byte to the end, without gaps (Linux; ADR 0058). Fails with the old
    /// limit (the page that holds the position): records of about 50 KB put the position the helper is given at a step
    /// anywhere in the first 50 KB of a block, not only in its first page.
    #[cfg(target_os = "linux")]
    #[test]
    fn early_writeback_stops_at_the_folio_boundary_and_finishes_rolled_segments() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let config = WriterConfig::default()
            .segment_size(4 << 20)
            .early_writeback(true);
        let mut w = Writer::open(dir.path(), config).unwrap();
        let payload = vec![7u8; 50_000];
        let first = w.active.base;
        let mut rolls = 0;
        while rolls < 3 || w.offset < 2 * crate::writeback::STEP + 70_000 {
            let base = w.active.base;
            w.append(&NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload: &payload,
            })
            .unwrap();
            if w.active.base != base {
                rolls += 1;
            }
        }
        assert_ne!(w.active.base, first);
        let issued = w.writeback.as_ref().unwrap().settled();
        let active = w.active.file.metadata().unwrap().ino();
        let mut files: Vec<u64> = Vec::new();
        for range in &issued {
            if let Some(position) = range.position {
                assert!(
                    range.to <= position / crate::writeback::FOLIO * crate::writeback::FOLIO,
                    "{range:?} reaches into the writer's block"
                );
            }
            if !files.contains(&range.ino) {
                files.push(range.ino);
            }
        }
        assert!(files.len() >= 4, "{files:?}");
        for ino in files {
            let ranges: Vec<_> = issued.iter().filter(|r| r.ino == ino).collect();
            assert_eq!(ranges[0].from, 0);
            for pair in ranges.windows(2) {
                assert_eq!(pair[0].to, pair[1].from, "{ranges:?}");
            }
            let last = ranges.last().unwrap();
            if ino == active {
                assert!(last.position.is_some());
            } else {
                // Written to the end of its seal, past the last boundary below it.
                assert!(last.position.is_none(), "{ranges:?}");
                assert!(last.to % crate::writeback::FOLIO != 0, "{last:?}");
            }
        }
    }

    /// An early write-back helper that stopped on its own (here as for an unsupported call) is handed nothing more:
    /// after many rolls, with retention deleting the segments rolled past, no rolled segment waits for it and it holds
    /// no segment file, so no descriptor or deleted segment's space is kept (Linux; ADR 0038 → Early write-back).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_stopped_writeback_helper_is_handed_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let config = WriterConfig::default()
            .segment_size(format::SEGMENT_SIZE_UNIT)
            .retention(
                crate::retention::Policy::default().max_segments(2),
                crate::retention::Action::Delete,
            );
        let mut w = Writer::open(dir.path(), config).unwrap();
        // This writer's helper only: a process-wide `fault::fail_next_background` may be taken by the helper of any
        // other test running at the same time, and this one's then never stops (issue #73).
        w.writeback
            .as_ref()
            .unwrap()
            .test_handle()
            .fail_next(libc::EINVAL);
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        let first = w.active.base;
        while w.active.base == first {
            append_one(&mut w);
        }
        while !w.writeback.as_ref().unwrap().held().0 {
            assert!(Instant::now() < deadline, "the helper did not stop");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        for _ in 0..50 {
            let base = w.active.base;
            while w.active.base == base {
                append_one(&mut w);
            }
            w.checkpoint().unwrap();
        }
        assert!(w.retention_report().removed_segments >= 40);
        assert_eq!(w.writeback.as_ref().unwrap().held(), (true, 0, false));
    }

    /// Every helper thread started from a writer thread pinned to one CPU runs on the process's CPUs by default,
    /// keeps the pin with `HelperCpus::Inherited`, and runs on the named CPU with `HelperCpus::List` (Linux; ADR 0038).
    #[cfg(target_os = "linux")]
    #[test]
    fn helper_threads_of_a_pinned_writer_are_placed_as_configured() {
        use crate::config::HelperCpus;
        let allowed = mmap::thread_affinity().unwrap();
        if allowed.len() < 2 {
            eprintln!("skipped: one CPU available");
            return;
        }
        let (pinned, other) = (allowed[0], allowed[1]);
        /// The CPU list of each of this process's threads whose name starts with `tickertape-` (as `comm` keeps it).
        fn helpers() -> Vec<(String, Vec<usize>)> {
            let mut out = Vec::new();
            for task in fs::read_dir("/proc/self/task").unwrap() {
                let task = task.unwrap().path();
                let Ok(name) = fs::read_to_string(task.join("comm")) else {
                    continue;
                };
                if !name.starts_with("tickertape-") {
                    continue;
                }
                let status = fs::read_to_string(task.join("status")).unwrap();
                let list = status
                    .lines()
                    .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
                    .unwrap();
                out.push((
                    name.trim().to_string(),
                    crate::affinity::parse_cpu_list(list).unwrap(),
                ));
            }
            out.sort();
            out
        }
        for (cpus, expect_pinned) in [
            (HelperCpus::Inherited, Some(vec![pinned])),
            (HelperCpus::List(vec![other]), Some(vec![other])),
            (HelperCpus::Process, None),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let config = WriterConfig::default()
                .segment_size(format::SEGMENT_SIZE_UNIT)
                .spare_segment(true)
                .helper_cpus(cpus.clone());
            // In a forked child, whose only threads are the writer's thread and the helpers it starts (other tests
            // in this process run writers of their own).
            let _forked = forked::LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let child = forked::fork(|| {
                mmap::set_thread_affinity([pinned]).unwrap();
                let mut writer = Writer::open(dir.path(), config).unwrap();
                append_one(&mut writer);
                writer.request_checkpoint().unwrap();
                // The helpers place themselves when they start: wait until each has.
                let deadline = Instant::now() + std::time::Duration::from_secs(10);
                let seen = loop {
                    let seen = helpers();
                    let names: Vec<&str> = seen.iter().map(|(n, _)| n.as_str()).collect();
                    let started = [
                        "tickertape-chec",
                        "tickertape-pref",
                        "tickertape-spar",
                        "tickertape-writ",
                    ]
                    .iter()
                    .all(|n| names.contains(n));
                    let settled = matches!(cpus, HelperCpus::Inherited)
                        || seen.iter().all(|(_, l)| l != &vec![pinned]);
                    if started && settled {
                        break seen;
                    }
                    assert!(Instant::now() < deadline, "{cpus:?}: {seen:?}");
                    std::thread::sleep(std::time::Duration::from_millis(1));
                };
                for (name, list) in &seen {
                    match &expect_pinned {
                        Some(expected) => assert_eq!(list, expected, "{cpus:?}: {name}"),
                        None => assert!(list.len() > 1, "{cpus:?}: {name} on {list:?}"),
                    }
                }
                drop(writer);
            })
            .unwrap();
            assert_eq!(child.wait(), Some(0), "{cpus:?}");
        }
    }

    fn record(payload: &[u8]) -> NewRecord<'_> {
        NewRecord {
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload,
        }
    }

    /// A single-record append publishes its wake-up right after the commit, before the prefault, write-back and
    /// spare-segment work; a batch publishes once, after its last record.
    #[test]
    fn append_wakes_readers_before_the_helper_work() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            Writer::open(dir.path(), WriterConfig::default().segment_size(1 << 20)).unwrap();
        let notify = writer
            .notify
            .as_ref()
            .expect("wake-ups are enabled")
            .publisher();
        let seen = Rc::new(Cell::new(None));

        let before = notify.event();
        let (at_hook, n) = (Rc::clone(&seen), notify.clone());
        AFTER_COMMIT.set(Some(Box::new(move || at_hook.set(Some(n.event())))));
        append_one(&mut writer);
        assert_eq!(
            seen.take(),
            Some(before.wrapping_add(1)),
            "published before the helper work"
        );

        let before = notify.event();
        let (at_hook, n) = (Rc::clone(&seen), notify.clone());
        AFTER_COMMIT.set(Some(Box::new(move || at_hook.set(Some(n.event())))));
        writer.append_batch([record(b"a"), record(b"b")]).unwrap();
        assert_eq!(
            seen.take(),
            Some(before),
            "a batch does not publish per record"
        );
        assert_eq!(
            notify.event(),
            before.wrapping_add(1),
            "one publication per batch"
        );
    }

    /// Yields its records, then optionally panics; when dropped, notes whether the batch's wake-up was published.
    struct CheckedOnDrop {
        records: std::vec::IntoIter<NewRecord<'static>>,
        panic_at_end: bool,
        notify: mmap::NotifyPublisher,
        expected: u32,
        published_before_drop: Rc<Cell<Option<bool>>>,
    }

    impl Iterator for CheckedOnDrop {
        type Item = NewRecord<'static>;

        fn next(&mut self) -> Option<NewRecord<'static>> {
            match self.records.next() {
                None if self.panic_at_end => panic!("the caller's iterator fails"),
                next => next,
            }
        }
    }

    impl Drop for CheckedOnDrop {
        fn drop(&mut self) {
            self.published_before_drop
                .set(Some(self.notify.event() == self.expected));
        }
    }

    /// `append_batch` publishes the wake-up before it drops the caller's iterator, also when the iterator panics.
    #[test]
    fn batch_wakes_readers_before_dropping_the_iterator() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            Writer::open(dir.path(), WriterConfig::default().segment_size(1 << 20)).unwrap();
        let notify = writer
            .notify
            .as_ref()
            .expect("wake-ups are enabled")
            .publisher();
        for panic_at_end in [false, true] {
            let published_before_drop = Rc::new(Cell::new(None));
            let records = CheckedOnDrop {
                records: vec![record(b"a"), record(b"b"), record(b"c")].into_iter(),
                panic_at_end,
                notify: notify.clone(),
                expected: notify.event().wrapping_add(1),
                published_before_drop: Rc::clone(&published_before_drop),
            };
            let next_seq = writer.next_seq();
            let result = panic::catch_unwind(AssertUnwindSafe(|| writer.append_batch(records)));
            assert_eq!(result.is_err(), panic_at_end);
            assert_eq!(writer.next_seq(), next_seq + 3);
            assert_eq!(
                published_before_drop.get(),
                Some(true),
                "panic_at_end {panic_at_end}"
            );
        }
    }
}
