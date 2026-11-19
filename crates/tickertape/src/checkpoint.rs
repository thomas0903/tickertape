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
//! successful job, and is joined when the writer is dropped, after it has finished every requested job.

use std::collections::BTreeSet;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::error::{Error, Result};
use crate::format::{self, CheckpointSlot};
use crate::journal::{self, CHECKPOINT_FILE};
use crate::mmap::{self, Flush, NotifyPublisher};
use crate::retention::{Action, Policy, Report};

/// A segment with data a checkpoint must sync: `[from, to)` of its mapping through `flush` (if any), then the whole
/// file's data. Segments inherited from an earlier writer have no `flush`: only their file is synced.
#[derive(Debug)]
pub(crate) struct DirtySegment {
    pub(crate) file: Arc<File>,
    pub(crate) flush: Option<Flush>,
    pub(crate) from: u64,
    pub(crate) to: u64,
    pub(crate) path: PathBuf,
}

/// Everything one checkpoint makes durable, and the marker slot it then writes.
#[derive(Debug)]
pub(crate) struct Job {
    /// The marker slot: the frontier at the request, and this checkpoint's generation.
    pub(crate) slot: CheckpointSlot,
    /// In order; the last one is the segment the marker names (the writer's active segment at the request), whose
    /// file also takes the full barrier.
    pub(crate) segments: Vec<DirtySegment>,
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
        for seg in later.segments {
            match self.segments.last_mut() {
                Some(last)
                    if last.path == seg.path
                        && last.to == seg.from
                        && last.flush.is_some() == seg.flush.is_some() =>
                {
                    last.to = seg.to;
                }
                _ => self.segments.push(seg),
            }
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
        return Err(Error::DurabilityLost);
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
    // 1. Data.
    inject(on, crate::fault::Point::CheckpointSync)
        .map_err(|e| Error::io("sync segment", &last.path, e))?;
    for seg in &job.segments {
        if let Some(flush) = &seg.flush {
            flush
                .sync_range(seg.from, seg.to - seg.from)
                .map_err(|e| Error::io("msync segment", &seg.path, e))?;
        }
        mmap::sync_data(&seg.file).map_err(|e| Error::io("sync segment", &seg.path, e))?;
    }
    // 2. Names.
    for dir in &job.dirs {
        journal::sync_dir(dir)?;
    }
    // 3. Barrier (macOS: F_FULLFSYNC flushes the drive cache, covering all of the above).
    mmap::full_barrier(&last.file).map_err(|e| Error::io("full sync segment", &last.path, e))?;
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

/// One retention run: plan, then apply into `report`; the error, if any.
pub(crate) fn retention_pass(
    dir: &std::path::Path,
    policy: &Policy,
    action: &Action,
    report: &mut Report,
) -> Option<Error> {
    crate::retention::plan(dir, policy)
        .and_then(|plan| crate::retention::apply_into(dir, &plan, action, report))
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
    ) -> io::Result<Background> {
        let shared = Arc::new(Shared::default());
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("tickertape-checkpoint".into())
            .spawn(move || run_thread(&thread_shared, &ctx, retention.as_ref()))?;
        Ok(Background {
            shared,
            thread: Some(thread),
            pid: std::process::id(),
        })
    }

    /// Blocks until no job is queued or running.
    pub(crate) fn wait_idle(&self) {
        let mut state = lock(&self.shared.state);
        while state.busy() {
            state = self
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
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
        drop(job);
        match outcome {
            Ok(slot) => {
                post(shared, &mut lock(&shared.state), |s| {
                    s.done = Some((slot, finished));
                    s.running = Some(0);
                });
                if let Some((policy, action)) = retention {
                    let mut report = Report::default();
                    let error = panic::catch_unwind(AssertUnwindSafe(|| {
                        retention_pass(&ctx.dir, policy, action, &mut report)
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
