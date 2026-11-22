//! The steps of a checkpoint (`docs/crash-contract.md` → What `checkpoint()` does), run either on the writer's
//! thread ([`Writer::checkpoint`](crate::Writer::checkpoint)) or on the writer's checkpoint thread
//! ([`Writer::request_checkpoint`](crate::Writer::request_checkpoint), ADR 0038).
//!
//! A [`Job`] is what one checkpoint makes durable: the dirty ranges and files the writer had when it was requested,
//! the directories whose entries changed, and the marker slot naming the writer's position at that moment (the
//! frontier). The writer hands the job over and starts a new dirty list, so records appended while the job runs belong
//! to the next checkpoint. Both paths run the same [`run`], so the blocking checkpoint does exactly what it did before
//! this module existed.
//!
//! The checkpoint thread is started by the first request, named `tickertape-checkpoint`, runs one job at a time in
//! request order (at most one more waits, and later requests extend it), runs the configured retention after each
//! successful job, and is joined when the writer is dropped, after it has finished every requested job. A retention
//! run there stops before its next removal once the writer waits for the thread, or once a job is queued and the run
//! has handled as many planned segments as the job before it covered or has taken as long as that job (ADR 0044,
//! amended for issue #45): the rest is left for the run after the next job, so neither a checkpoint nor `close` waits
//! for a retention backlog, and under a constant queue of checkpoints retention still removes as many segments as the
//! writer fills unless removing them takes longer than the checkpoints.

use std::collections::BTreeSet;
use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::error::{Error, Result};
use crate::format::{self, CheckpointSlot};
use crate::journal::{self, CHECKPOINT_FILE};
use crate::mmap::{self, Flush, NotifyPublisher};
use crate::retention::{Action, Policy, Report};

/// How many of the most recent dirty segments a [`DirtySegments`] list keeps open (file and mapping handle); older
/// ones are remembered by path, file identity and range only, and the checkpoint that covers them reopens them. A
/// segment is released by the push that makes its list longer than four: usually a roll, but also a request that adds
/// the active segment to a list already holding four, or that joins a job queued behind a slow checkpoint
/// ([`Job::absorb`]); that request may then close or unmap the segment. Four, because a writer that checkpoints at
/// least once per roll then does not reopen a segment unless its requests pile up behind a slow checkpoint (a list
/// holds the segments rolled past since the last request and the job adds the active one), the early write-back
/// helper has usually finished a segment within a roll or two of leaving it, and a background checkpoint queued
/// behind a running one still finds its newest segments open; and because it is small: each list costs at most four descriptors and four segment
/// mappings of address space whatever the number of rolls between checkpoints, so a writer with a few lists (its own,
/// a running and a queued background checkpoint) stays far below the usual descriptor limits (commonly 256 or 1,024).
pub(crate) const OPEN_SEGMENTS: usize = 4;

/// A file's identity (device and inode number), read through a descriptor of it, to tell whether a path still names
/// that file when it is opened again. A file system may give a removed file's inode number to the next file it
/// creates, so a file removed and created again under the same name is not always told apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileId {
    dev: u64,
    ino: u64,
}

impl FileId {
    pub(crate) fn of(file: &File) -> io::Result<FileId> {
        let meta = file.metadata()?;
        Ok(FileId {
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }
}

/// Opens `path` read-write again and checks that it is still the file `id` was read from (not a file that replaced
/// it under the same name).
pub(crate) fn reopen(path: &Path, id: FileId) -> io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    if FileId::of(&file)? != id {
        return Err(io::Error::other(
            "the file at this path is not the one the writer held: it was replaced",
        ));
    }
    Ok(file)
}

/// A segment with data a checkpoint must sync: `[from, to)` of its mapping through `flush` (if any), then the whole
/// file's data. Segments inherited from an earlier writer have no `flush`: only their file is synced. A released
/// segment ([`DirtySegments`]) has neither `file` nor `flush`: the checkpoint reopens it by `path`, which is absolute,
/// and checks that it is still the file `id` names (see [`steps`]).
#[derive(Debug)]
pub(crate) struct DirtySegment {
    pub(crate) file: Option<Arc<File>>,
    pub(crate) flush: Option<Flush>,
    /// The file's identity, read when the segment was released (`None` while it is held, or if it could not be read,
    /// which fails the checkpoint that covers the segment).
    pub(crate) id: Option<FileId>,
    /// Whether this writer stored into the segment through its own mapping (false for inherited segments).
    pub(crate) mapped: bool,
    pub(crate) from: u64,
    pub(crate) to: u64,
    pub(crate) path: PathBuf,
}

impl DirtySegment {
    /// A segment this writer wrote through `flush`, a handle on its mapping of `file`.
    pub(crate) fn written(
        file: Arc<File>,
        flush: Flush,
        from: u64,
        to: u64,
        path: PathBuf,
    ) -> Self {
        DirtySegment {
            file: Some(file),
            flush: Some(flush),
            id: None,
            mapped: true,
            from,
            to,
            path,
        }
    }

    /// Lets go of the file and the mapping handle: the segment is remembered by path, file identity and range only.
    fn release(&mut self) {
        if let Some(file) = &self.file {
            self.id = FileId::of(file).ok();
        }
        self.file = None;
        self.flush = None;
    }
}

/// The dirty segments of a writer or a checkpoint job, oldest first. Only the newest [`OPEN_SEGMENTS`] keep their
/// file and mapping handle; pushing a segment releases the one that falls out of that window, so the descriptors and
/// mappings held do not grow with the number of segments (ADR 0038 → Bounded open segments).
#[derive(Debug, Default)]
pub(crate) struct DirtySegments {
    list: Vec<DirtySegment>,
    /// `list[..released]` hold no file.
    released: usize,
}

impl DirtySegments {
    /// Appends `seg` as the newest segment, releasing the segment that leaves the open window.
    pub(crate) fn push(&mut self, seg: DirtySegment) {
        self.list.push(seg);
        while self.list.len() - self.released > OPEN_SEGMENTS {
            self.list[self.released].release();
            self.released += 1;
        }
    }

    /// Appends `seg`, or extends the newest segment by it when `seg` continues it (same file, adjacent range, written
    /// the same way).
    fn push_or_extend(&mut self, seg: DirtySegment) {
        match self.list.last_mut() {
            Some(last)
                if last.path == seg.path && last.to == seg.from && last.mapped == seg.mapped =>
            {
                last.to = seg.to;
            }
            _ => self.push(seg),
        }
    }

    /// Replaces each segment's path by `resolve(path)` (the writer makes them absolute after recovery).
    pub(crate) fn map_paths(&mut self, resolve: impl Fn(&Path) -> PathBuf) {
        for seg in &mut self.list {
            seg.path = resolve(&seg.path);
        }
    }

    pub(crate) fn iter(&self) -> std::slice::Iter<'_, DirtySegment> {
        self.list.iter()
    }

    pub(crate) fn last(&self) -> Option<&DirtySegment> {
        self.list.last()
    }

    /// For tests: how many segments hold their file.
    #[cfg(test)]
    pub(crate) fn open(&self) -> usize {
        self.list.iter().filter(|seg| seg.file.is_some()).count()
    }
}

/// Everything one checkpoint makes durable, and the marker slot it then writes.
#[derive(Debug)]
pub(crate) struct Job {
    /// The marker slot: the frontier at the request, and this checkpoint's generation.
    pub(crate) slot: CheckpointSlot,
    /// In order; the last one is the segment the marker names (the writer's active segment at the request), whose
    /// file also takes the full barrier.
    pub(crate) segments: DirtySegments,
    /// Directories whose entries changed before the request.
    pub(crate) dirs: BTreeSet<PathBuf>,
}

impl Job {
    /// Bytes of records and seals this job covers (for `Writer::metrics`).
    pub(crate) fn bytes(&self) -> u64 {
        self.segments.iter().fold(0u64, |sum, seg| {
            sum.saturating_add(seg.to.saturating_sub(seg.from))
        })
    }

    /// Extends this job, which has not started, by a later request's job: one checkpoint then covers both, with this
    /// job's generation and the later job's frontier.
    pub(crate) fn absorb(&mut self, later: Job) {
        for seg in later.segments.list {
            self.segments.push_or_extend(seg);
        }
        self.dirs.extend(later.dirs);
        self.slot = CheckpointSlot {
            generation: self.slot.generation,
            ..later.slot
        };
    }
}

/// The marker file, used only by the one checkpoint running at a time.
#[derive(Debug)]
pub(crate) struct Marker {
    pub(crate) file: File,
    /// The marker file is shorter than its full length (something cut it): it is extended after the next checkpoint's
    /// marker barrier, never at open (ADR 0032 → Checkpoint age).
    pub(crate) short: bool,
}

/// What the writer and its checkpoint thread share.
#[derive(Debug)]
pub(crate) struct Context {
    pub(crate) dir: PathBuf,
    pub(crate) marker: Mutex<Marker>,
    /// Set once any sync of this writer's data has failed: a checkpoint's, or the one that makes an abandoned write's
    /// zeroing durable. A checkpoint holds this lock from its first sync to its marker barrier, and the abandoned-write
    /// path holds it around its syncs, so a marker is never written after a failed sync of the same files (on Linux a
    /// failed `fdatasync` can mark pages clean without writing them, and the error is reported to one caller only).
    pub(crate) lost: Mutex<bool>,
    /// The error that made the early write-back helper set `lost` (Linux, ADR 0038 → Early write-back), returned
    /// once: taken by the next checkpoint (which then returns it instead of `DurabilityLost`) or by the writer's next
    /// `request_checkpoint`, `checkpoint_progress`, `checkpoint` or `close`, whichever comes first. Never held while
    /// waiting for `lost`.
    pub(crate) writeback_failure: Mutex<Option<Error>>,
    /// Set (after `lost` and `writeback_failure`, with release ordering) when the early write-back helper failed, for
    /// `Writer::metrics` and the writer's collection without taking `lost`.
    pub(crate) writeback_failed: std::sync::atomic::AtomicBool,
    /// Wakes durable-only readers once a marker is durable (ADR 0017).
    pub(crate) notify: Option<NotifyPublisher>,
}

/// Which thread runs a job: the fault-injection points differ (thread-local on the writer's thread).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum On {
    Writer,
    Background,
}

fn inject(on: On, point: crate::fault::Point) -> io::Result<()> {
    match on {
        On::Writer => crate::fault::inject(point),
        On::Background => crate::fault::inject_background(point),
    }
}

/// A poisoned lock still holds consistent state here (plain flags and data), so it is used as it is.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs a checkpoint's steps for `job`. On success the marker names `job.slot` and is durable; on failure the writer's
/// data is marked lost (`Context::lost`) and no later checkpoint writes a marker.
pub(crate) fn run(job: &Job, ctx: &Context, on: On) -> Result<CheckpointSlot> {
    if on == On::Background {
        crate::fault::hold_point(crate::fault::Point::CheckpointSync);
    }
    let mut lost = lock(&ctx.lost);
    if *lost {
        return Err(lock(&ctx.writeback_failure)
            .take()
            .unwrap_or(Error::DurabilityLost));
    }
    let result = steps(job, ctx, on);
    if result.is_err() {
        *lost = true;
    }
    result
}

fn steps(job: &Job, ctx: &Context, on: On) -> Result<CheckpointSlot> {
    let Some(last) = job.segments.last() else {
        return Err(Error::io(
            "checkpoint",
            &ctx.dir,
            io::Error::other("internal: a checkpoint without the active segment"),
        ));
    };
    // 1. Data. A segment still held is synced through its own mapping handle and descriptor. A released one is
    // reopened by its absolute path and must be the file it was (device and inode, read before it was released); it is
    // after the marker, so retention never removes it, and if it is gone or replaced anyway (by hand) the checkpoint
    // fails before any marker. Syncing a reopened descriptor is enough because the data lives
    // in the file's page cache, not in a descriptor or a mapping: a store through a shared mapping dirties the
    // file's cached page (on Linux the page is marked dirty at the write fault on filesystems that track dirty pages,
    // and unmapping transfers any dirty bit left in the page tables to the page), so `fdatasync` on any descriptor of
    // the file writes back every dirty page of it and waits; on Linux it also reports a write-back error recorded on
    // the file that no descriptor has observed yet, also one recorded before the descriptor was opened (`errseq`). On Linux `msync(MS_SYNC)` is a data sync of the range's file pages and adds nothing to
    // that. On macOS the writer does not rely on `fsync` alone for pages a mapping dirtied: it maps the reopened file
    // again and `msync`s the range first, which writes back the dirty pages of the file's memory object whichever
    // mapping dirtied them, then `fsync`s; `F_FULLFSYNC` (step 3) is unchanged. What a held descriptor gave that a
    // reopened one does not: a write-back error is not reported here if the kernel dropped the cached inode, and the
    // error with it, while no descriptor or mapping of the file was open, or if another descriptor of the file already
    // observed it (crash contract → Segments the writer no longer holds → the limit).
    inject(on, crate::fault::Point::CheckpointSync)
        .map_err(|e| Error::io("sync segment", &last.path, e))?;
    let mut barrier = None;
    for seg in job.segments.iter() {
        let file = match &seg.file {
            Some(file) => {
                if let Some(flush) = &seg.flush {
                    flush
                        .sync_range(seg.from, seg.to - seg.from)
                        .map_err(|e| Error::io("msync segment", &seg.path, e))?;
                }
                Arc::clone(file)
            }
            None => {
                let file = seg
                    .id
                    .ok_or_else(|| io::Error::other("the segment's file identity is unknown"))
                    .and_then(|id| reopen(&seg.path, id))
                    .map_err(|e| Error::io("reopen segment", &seg.path, e))?;
                if cfg!(target_vendor = "apple") && seg.mapped {
                    mmap::msync_file(&file, seg.from, seg.to - seg.from)
                        .map_err(|e| Error::io("msync segment", &seg.path, e))?;
                }
                Arc::new(file)
            }
        };
        mmap::sync_data(&file).map_err(|e| Error::io("sync segment", &seg.path, e))?;
        barrier = Some(file);
    }
    let Some(barrier) = barrier else {
        return Err(Error::io(
            "checkpoint",
            &ctx.dir,
            io::Error::other("internal: a checkpoint without the active segment"),
        ));
    };
    // 2. Names.
    for dir in &job.dirs {
        journal::sync_dir(dir)?;
    }
    // 3. Barrier (macOS: F_FULLFSYNC flushes the drive cache, covering all of the above).
    mmap::full_barrier(&barrier).map_err(|e| Error::io("full sync segment", &last.path, e))?;
    // 4. Marker.
    let marker_path = ctx.dir.join(CHECKPOINT_FILE);
    if on == On::Background {
        crate::fault::hold_point(crate::fault::Point::CheckpointBeforeMarker);
        crate::fault::inject_background(crate::fault::Point::CheckpointBeforeMarker)
            .map_err(|e| Error::io("write checkpoint marker", &marker_path, e))?;
    }
    let slot = job.slot;
    let bytes = format::encode_checkpoint_slot(&slot);
    let at = format::CHECKPOINT_SLOT_OFFSETS[(slot.generation % 2) as usize] as u64;
    let mut marker = lock(&ctx.marker);
    marker
        .file
        .write_all_at(&bytes, at)
        .map_err(|e| Error::io("write checkpoint marker", &marker_path, e))?;
    inject(on, crate::fault::Point::CheckpointMarkerSync)
        .map_err(|e| Error::io("sync checkpoint marker", &marker_path, e))?;
    mmap::full_barrier(&marker.file)
        .map_err(|e| Error::io("sync checkpoint marker", &marker_path, e))?;
    // 5. A marker that something cut short is extended only now, after its slot is durable: observers take a
    // full-length marker's modification time as the checkpoint's time (ADR 0032), so the extension must not happen
    // before a checkpoint has succeeded (a short marker's age is unknown). Best effort: the checkpoint has succeeded,
    // and a marker left short is still valid (each slot is read on its own); a later checkpoint tries again.
    if marker.short
        && mmap::preallocate(&marker.file, format::CHECKPOINT_FILE_LEN as u64).is_ok()
        && mmap::sync_data(&marker.file).is_ok()
    {
        marker.short = false;
    }
    drop(marker);
    // Durable-only readers wait for the marker to move.
    if let Some(notify) = &ctx.notify {
        notify.publish();
    }
    Ok(slot)
}

/// What a retention run after a background checkpoint did, for the writer to add to its own report.
#[derive(Debug, Default)]
pub(crate) struct RetentionOutcome {
    pub(crate) report: Report,
    pub(crate) last_error: Option<Error>,
    pub(crate) failures: u64,
}

/// One retention run on the checkpoint thread: plan, then apply into `report`, stopping before the next planned
/// segment once `stop(handled)` returns true for the number of planned segments handled so far (never before the
/// first); the error, if any.
pub(crate) fn retention_pass(
    dir: &std::path::Path,
    policy: &Policy,
    action: &Action,
    report: &mut Report,
    stop: &dyn Fn(usize) -> bool,
) -> Option<Error> {
    crate::retention::plan_until(dir, policy, stop)
        .and_then(|plan| {
            crate::retention::apply_into(dir, &plan, action, report, false, Some(stop))
        })
        .err()
}

/// The writer's handle on its checkpoint thread.
#[derive(Debug)]
pub(crate) struct Background {
    pub(crate) shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// The process that started the thread: in a forked child the thread does not exist and its lock may be held
    /// forever, so nothing here is touched there.
    pid: u32,
}

#[derive(Debug, Default)]
pub(crate) struct Shared {
    pub(crate) state: Mutex<State>,
    changed: Condvar,
    /// Incremented (under `state`'s lock) whenever `state` gains something for the writer to collect, so that the
    /// writer's poll needs one atomic load when nothing happened.
    pub(crate) posted: AtomicU64,
}

#[derive(Debug, Default)]
pub(crate) struct State {
    /// The next job, waiting for the running one. Later requests extend it.
    pub(crate) queued: Option<Job>,
    /// Bytes the running job covers, or zero once its marker is durable (it may still be running retention).
    pub(crate) running: Option<u64>,
    /// The newest marker the thread made durable, and when, not yet collected by the writer.
    pub(crate) done: Option<(CheckpointSlot, Instant)>,
    /// The first failure, not yet collected by the writer.
    pub(crate) failure: Option<Error>,
    /// Set by the first failure and never cleared: later jobs are dropped without running.
    pub(crate) failed: bool,
    /// Bytes of the failed and dropped jobs (for `Writer::metrics`): no checkpoint of this writer covers them.
    pub(crate) failed_bytes: u64,
    /// Retention runs since the writer last collected.
    pub(crate) retention: Option<RetentionOutcome>,
    /// Calls waiting for the thread to be idle ([`Background::wait_idle`]): a retention run stops early for them.
    waiters: usize,
    stop: bool,
}

impl State {
    /// Whether a job is queued or running.
    pub(crate) fn busy(&self) -> bool {
        self.queued.is_some() || self.running.is_some()
    }

    /// Bytes covered by jobs queued or running whose markers are not durable yet.
    pub(crate) fn in_flight_bytes(&self) -> u64 {
        self.running
            .unwrap_or(0)
            .saturating_add(self.queued.as_ref().map_or(0, Job::bytes))
    }

    /// Whether a retention run that has handled `handled` planned segments should stop before its next removal: the
    /// writer waits for the thread to be idle or the thread is stopping, or a job waits and the run has either handled
    /// `covered` segments, the number the job it follows covered, or used its time (`out_of_time`: as long as that job
    /// took); at least one segment in every case (ADR 0044). Removing what each checkpoint covered keeps retention
    /// level with the writer when checkpoints are slower than the requests and a job always waits (issue #45); the
    /// time limit keeps a waiting checkpoint from waiting longer again than the one before it took when removals are
    /// slow.
    fn retention_should_stop(&self, handled: usize, covered: usize, out_of_time: bool) -> bool {
        handled >= 1
            && (self.waiters > 0
                || self.stop
                || (self.queued.is_some() && (handled >= covered || out_of_time)))
    }

    fn add_retention(&mut self, report: &Report, error: Option<Error>) {
        let outcome = self.retention.get_or_insert_with(RetentionOutcome::default);
        outcome.report.absorb(report);
        if let Some(error) = error {
            outcome.last_error = Some(error);
            outcome.failures = outcome.failures.saturating_add(1);
        }
    }
}

impl Shared {
    /// Wakes the thread after a job was queued.
    pub(crate) fn wake(&self) {
        self.changed.notify_all();
    }
}

impl Background {
    /// Starts the checkpoint thread.
    pub(crate) fn start(
        ctx: Arc<Context>,
        retention: Option<(Policy, Action)>,
        cpus: crate::config::HelperCpus,
    ) -> io::Result<Background> {
        let shared = Arc::new(Shared::default());
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("tickertape-checkpoint".into())
            .spawn(move || {
                crate::affinity::place_helper(&cpus);
                run_thread(&thread_shared, &ctx, retention.as_ref());
            })?;
        Ok(Background {
            shared,
            thread: Some(thread),
            pid: std::process::id(),
        })
    }

    /// Blocks until no job is queued or running. A retention run in progress stops after the removal in progress.
    pub(crate) fn wait_idle(&self) {
        /// Counts the caller out of `waiters` when it returns, also if a fault hook unwinds.
        struct Waiting<'a>(&'a Shared);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                lock(&self.0.state).waiters -= 1;
            }
        }
        lock(&self.shared.state).waiters += 1;
        let _waiting = Waiting(&self.shared);
        crate::fault::hook(crate::fault::Point::CheckpointWait);
        let mut state = lock(&self.shared.state);
        while state.busy() {
            state = self
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
        drop(state); // Before `_waiting` takes the lock again.
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        if self.pid != std::process::id() {
            // A forked child: the thread does not exist here. Detaching or joining it would name a thread of another
            // process, and its lock may have been held at the fork, so leave everything alone.
            if let Some(thread) = self.thread.take() {
                std::mem::forget(thread);
            }
            return;
        }
        // Finish what was requested (a sync in progress cannot be interrupted anyway, and the writer's lock must be
        // held until the last marker is written), then stop.
        lock(&self.shared.state).stop = true;
        self.shared.changed.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn post(shared: &Shared, state: &mut State, change: impl FnOnce(&mut State)) {
    change(state);
    shared.posted.fetch_add(1, Ordering::Release);
    shared.changed.notify_all();
}

fn run_thread(shared: &Shared, ctx: &Context, retention: Option<&(Policy, Action)>) {
    loop {
        let job = {
            let mut state = lock(&shared.state);
            loop {
                if let Some(job) = state.queued.take() {
                    if state.failed {
                        // A failure poisoned the writer after this job was queued: it must not write a marker.
                        let bytes = job.bytes();
                        post(shared, &mut state, |s| {
                            s.failed_bytes = s.failed_bytes.saturating_add(bytes);
                        });
                        continue;
                    }
                    state.running = Some(job.bytes());
                    break job;
                }
                if state.stop {
                    return;
                }
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(|e| e.into_inner());
            }
        };
        let started = Instant::now();
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| run(&job, ctx, On::Background)))
            .unwrap_or_else(|_| {
                *lock(&ctx.lost) = true;
                Err(Error::io(
                    "checkpoint",
                    &ctx.dir,
                    io::Error::other("the checkpoint thread panicked"),
                ))
            });
        let finished = Instant::now();
        let bytes = job.bytes();
        let covered = job.segments.iter().count();
        drop(job);
        match outcome {
            Ok(slot) => {
                post(shared, &mut lock(&shared.state), |s| {
                    s.done = Some((slot, finished));
                    s.running = Some(0);
                });
                if let Some((policy, action)) = retention {
                    let mut report = Report::default();
                    // While a job waits, the run may take as long as this job took.
                    let deadline = Instant::now() + finished.saturating_duration_since(started);
                    let stop = |handled| {
                        let out_of_time = Instant::now() >= deadline;
                        lock(&shared.state).retention_should_stop(handled, covered, out_of_time)
                    };
                    let error = panic::catch_unwind(AssertUnwindSafe(|| {
                        retention_pass(&ctx.dir, policy, action, &mut report, &stop)
                    }))
                    .unwrap_or_else(|_| {
                        Some(Error::io(
                            "retention",
                            &ctx.dir,
                            io::Error::other("retention panicked"),
                        ))
                    });
                    post(shared, &mut lock(&shared.state), |s| {
                        s.add_retention(&report, error);
                        s.running = None;
                    });
                } else {
                    post(shared, &mut lock(&shared.state), |s| s.running = None);
                }
            }
            Err(error) => post(shared, &mut lock(&shared.state), |s| {
                s.running = None;
                s.failed = true;
                s.failed_bytes = s.failed_bytes.saturating_add(bytes);
                s.failure.get_or_insert(error);
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(dir: &std::path::Path, i: u64) -> DirtySegment {
        let path = dir.join(format!("{i}"));
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        DirtySegment {
            file: Some(Arc::new(file)),
            flush: None,
            id: None,
            mapped: false,
            from: 0,
            to: 8,
            path,
        }
    }

    fn job(segments: DirtySegments, generation: u64) -> Job {
        Job {
            slot: CheckpointSlot {
                generation,
                next_seq: 0,
                segment_base_seq: 0,
                offset: 0,
            },
            segments,
            dirs: BTreeSet::new(),
        }
    }

    #[test]
    fn a_dirty_list_holds_only_its_newest_segments_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut list = DirtySegments::default();
        for i in 0..10 {
            list.push(segment(dir.path(), i));
            assert_eq!(list.open(), (i as usize + 1).min(OPEN_SEGMENTS));
        }
        // The newest ones are the open ones, and every segment is still listed with its range.
        let held: Vec<bool> = list.iter().map(|seg| seg.file.is_some()).collect();
        assert_eq!(held.len(), 10);
        assert!(held[..10 - OPEN_SEGMENTS].iter().all(|h| !h));
        assert!(held[10 - OPEN_SEGMENTS..].iter().all(|h| *h));
        assert_eq!(list.iter().map(|seg| seg.to - seg.from).sum::<u64>(), 80);

        // A queued job that absorbs a later one stays within the bound, and adjacent ranges of one file merge.
        let mut first = DirtySegments::default();
        for i in 0..3 {
            first.push(segment(dir.path(), 100 + i));
        }
        let mut later = DirtySegments::default();
        let mut cont = segment(dir.path(), 102);
        cont.from = 8;
        cont.to = 16;
        later.push(cont);
        for i in 3..9 {
            later.push(segment(dir.path(), 100 + i));
        }
        let mut queued = job(first, 1);
        queued.absorb(job(later, 2));
        assert_eq!(queued.segments.iter().count(), 9);
        assert_eq!(queued.segments.open(), OPEN_SEGMENTS);
        assert_eq!(queued.bytes(), 9 * 8 + 8);
        assert_eq!(queued.slot.generation, 1);
    }
}
