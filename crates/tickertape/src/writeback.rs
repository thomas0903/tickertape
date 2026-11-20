//! Early write-back of the pages behind the writer (Linux; [`WriterConfig::early_writeback`], ADR 0038 → Early
//! write-back).
//!
//! A helper thread (`tickertape-writeback`) asks the kernel to start writing back the whole pages the writer has
//! finished with (`sync_file_range(SYNC_FILE_RANGE_WRITE)` through [`mmap::start_writeback`]), so that a checkpoint
//! finds little dirty data left and its syncs, and the write-back they force on the device, are short. The writer
//! only compares its offset with the next step after each append and, once per [`STEP`], takes a lock and wakes the
//! helper. The helper never touches the mapped bytes, and the writer never stores into a page behind its position
//! again, so starting that page's write-back does not make the writer take a write fault on it.
//!
//! Nothing here makes anything durable or is needed for a guarantee: what is durable when is decided by checkpoints
//! alone. An I/O error from the call marks the writer's data lost (`Context::lost`), as a failed sync does, so that no
//! later checkpoint writes a marker; the first such error is returned once, by the next checkpoint or the writer's next
//! `request_checkpoint`, `checkpoint_progress`, `checkpoint` or `close`. Once the helper exits (after such an error,
//! or because the call is unsupported), it is marked stopped and holds no file, and rolls hand it nothing more.
//!
//! [`WriterConfig::early_writeback`]: crate::WriterConfig::early_writeback

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::fs::File;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError, Weak};
use std::thread::JoinHandle;

use crate::checkpoint::Context;
use crate::config::HelperCpus;
use crate::error::Error;

/// How far the write position moves between write-back calls: 1 MiB. Large enough that the cost per call (a lock, a
/// wake-up and a system call) is spread over about a thousand 1 KiB appends and the kernel can submit large
/// requests; small enough that what a checkpoint still has to write back is a few milliseconds of appends at the
/// rates this is for, and that the helper starts each step's write-back long before the kernel's own flusher would
/// (it waits for `dirty_background_*` or `dirty_expire_centisecs`, 30 s by default).
pub(crate) const STEP: u64 = 1 << 20;

/// The writer's side: a handle on the helper thread, stopped and joined on drop in the process that started it. In a
/// forked child, dropping it only lets go of the child's references to the segment files (see [`Drop`]).
#[derive(Debug)]
pub(crate) struct WriteBack {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// The write offset at which the writer next wakes the helper.
    next_wake: u64,
    /// The process that started the thread: in a forked child the thread does not exist, so it is not joined there.
    pid: u32,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Debug)]
struct State {
    /// The active segment's file; `None` once the helper has stopped.
    file: Option<Arc<File>>,
    /// The writer's position in `file`: everything before the page that holds it is finished.
    position: u64,
    /// Segments the writer rolled past since the helper last looked, with their end (after the seal): written back
    /// to the end before the helper moves on. At most [`crate::checkpoint::OPEN_SEGMENTS`] (see `WriteBack::switch`).
    rolled: Vec<(Arc<File>, u64)>,
    stop: bool,
    /// Set by the helper when it exits on its own (the call is unsupported, or failed), with `file` and `rolled`
    /// cleared under the same lock: from then on nothing is handed to it, so it keeps no segment file open.
    stopped: bool,
    /// Whether the helper is waiting to be woken (and so holds neither the lock nor a file): for tests.
    #[cfg_attr(not(test), allow(dead_code))]
    waiting: bool,
}

/// The state is plain data that stays consistent even if a holder panicked, so a poisoned lock is still usable.
fn lock(shared: &Shared) -> MutexGuard<'_, State> {
    shared.state.lock().unwrap_or_else(|e| e.into_inner())
}

impl WriteBack {
    /// Starts the helper for the active segment `file` with the writer at `position`. `None` where it does not
    /// apply (not Linux) or if the thread can't be spawned.
    pub(crate) fn start(
        file: Arc<File>,
        position: u64,
        ctx: Arc<Context>,
        cpus: HelperCpus,
    ) -> Option<WriteBack> {
        if !cfg!(target_os = "linux") {
            return None;
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                file: Some(file),
                position,
                rolled: Vec::new(),
                stop: false,
                stopped: false,
                waiting: false,
            }),
            wake: Condvar::new(),
        });
        let helper = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("tickertape-writeback".into())
            .spawn(move || {
                crate::affinity::place_helper(&cpus);
                run(&helper, &ctx);
            })
            .ok()?;
        Some(WriteBack {
            shared,
            thread: Some(thread),
            next_wake: position.saturating_add(STEP),
            pid: std::process::id(),
        })
    }

    /// Called after each append with the new write offset: wakes the helper once per [`STEP`].
    #[inline]
    pub(crate) fn advanced(&mut self, position: u64) {
        if position >= self.next_wake {
            self.next_wake = position.saturating_add(STEP);
            lock(&self.shared).position = position;
            self.shared.wake.notify_one();
        }
    }

    /// Called after a roll: write back the old segment `old` up to `old_end`, then follow the new one from `position`.
    /// Nothing is handed to a helper that has stopped.
    pub(crate) fn switch(&mut self, old: Arc<File>, old_end: u64, file: Arc<File>, position: u64) {
        self.next_wake = position.saturating_add(STEP);
        let mut state = lock(&self.shared);
        if state.stopped {
            return;
        }
        // Bounded like the writer's dirty list: if the helper has fallen this far behind, the oldest segment's early
        // write-back is skipped (it is only a head start; the checkpoint syncs that segment anyway), so the helper
        // never holds more than a few segment files open.
        if state.rolled.len() >= crate::checkpoint::OPEN_SEGMENTS {
            state.rolled.remove(0);
        }
        state.rolled.push((old, old_end));
        state.file = Some(file);
        state.position = position;
        drop(state);
        self.shared.wake.notify_one();
    }

    /// For tests: whether the helper has stopped on its own, how many rolled segments wait for it, and whether it
    /// still holds the active segment's file.
    #[cfg(test)]
    pub(crate) fn held(&self) -> (bool, usize, bool) {
        let state = lock(&self.shared);
        (state.stopped, state.rolled.len(), state.file.is_some())
    }

    /// For tests: a handle on the state shared with the helper.
    #[cfg(test)]
    pub(crate) fn test_handle(&self) -> TestHandle {
        TestHandle(Arc::clone(&self.shared))
    }
}

/// For tests: the state shared with the helper, apart from the writer.
#[cfg(test)]
pub(crate) struct TestHandle(Arc<Shared>);

#[cfg(test)]
impl TestHandle {
    /// Waits until the helper waits to be woken, and returns its lock, which the caller then holds.
    pub(crate) fn idle(&self) -> MutexGuard<'_, impl Sized + use<'_>> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let state = lock(&self.0);
            if state.waiting {
                return state;
            }
            drop(state);
            assert!(
                std::time::Instant::now() < deadline,
                "the write-back helper did not go idle"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

impl Drop for WriteBack {
    fn drop(&mut self) {
        if self.pid != std::process::id() {
            // A forked child: the thread does not exist here, so it is neither stopped nor joined, and its handle is
            // neither joined nor detached. Its lock may have been held at the fork, when it stays held here for good:
            // it is only tried. If it is free, the child's references to the segment files the helper follows are
            // dropped, so the child keeps none of their descriptors open (the helper holds no other reference to a
            // file, except for the one it is starting the write-back of during the call). Nothing else is changed.
            let state = match self.shared.state.try_lock() {
                Ok(state) => Some(state),
                Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
                Err(TryLockError::WouldBlock) => None,
            };
            if let Some(mut state) = state {
                state.file = None;
                state.rolled.clear();
            }
            if let Some(thread) = self.thread.take() {
                std::mem::forget(thread);
            }
            return;
        }
        lock(&self.shared).stop = true;
        self.shared.wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What the helper does next.
enum Work {
    /// Write back `[from, to)` of a file.
    Range(Arc<File>, u64, u64),
    Stop,
}

/// The helper thread: write back up to the page that holds the writer's position, then sleep until woken. When it
/// exits, it marks itself stopped and lets go of every file it was given.
fn run(shared: &Shared, ctx: &Context) {
    follow(shared, ctx);
    let mut state = lock(shared);
    state.stopped = true;
    state.file = None;
    state.rolled.clear();
}

/// Writes back what the writer hands over until told to stop, or until a call fails or is unsupported.
fn follow(shared: &Shared, ctx: &Context) {
    #[cfg(target_os = "linux")]
    let page = crate::mmap::page_size() as u64;
    #[cfg(not(target_os = "linux"))]
    let page = 4096u64;
    // The file the helper is following, and how far it has written it back. Weak, so that between calls the helper
    // holds no reference that keeps a file open: a forked child, in which the helper's stack is never unwound, can
    // then close the file by dropping the shared state's references (see `Drop`). The allocation stays while the
    // `Weak` does, so a new file can't be given the same address and be mistaken for it.
    let mut current: Option<Weak<File>> = None;
    let mut done = 0u64;
    loop {
        let work = {
            let mut state = lock(shared);
            loop {
                if state.stop {
                    break Work::Stop;
                }
                if !state.rolled.is_empty() {
                    // A segment the writer rolled past: the rest of it, to the end of its seal.
                    let (file, end) = state.rolled.remove(0);
                    let from = match &current {
                        Some(c) if same(c, &file) => done,
                        _ => 0,
                    };
                    current = None;
                    if from < end {
                        break Work::Range(file, from, end);
                    }
                    continue;
                }
                // Borrowed, not cloned, until there is work: no reference to the file is kept while the helper waits.
                let Some(file) = state.file.as_ref() else {
                    break Work::Stop;
                };
                if !current.as_ref().is_some_and(|c| same(c, file)) {
                    // A new segment from its first page (its header too).
                    current = Some(Arc::downgrade(file));
                    done = 0;
                }
                // Whole pages before the one that holds the position.
                let limit = state.position / page * page;
                if done < limit {
                    let to = limit.min(done.saturating_add(STEP));
                    let from = std::mem::replace(&mut done, to);
                    break Work::Range(Arc::clone(file), from, to);
                }
                state.waiting = true;
                state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
                state.waiting = false;
            }
        };
        let Work::Range(file, from, to) = work else {
            return;
        };
        if let Err(e) = start(&file, from, to - from) {
            if unsupported(&e) {
                // The filesystem or kernel does not support it: the kernel's own flusher writes back as before.
                return;
            }
            // The write-back of these pages may have failed: no marker may claim them any more.
            let mut lost = crate::checkpoint::lock(&ctx.lost);
            *lost = true;
            crate::checkpoint::lock(&ctx.writeback_failure).get_or_insert_with(|| {
                Error::io(
                    "start write-back of segment data",
                    &ctx.dir,
                    std::io::Error::other(e),
                )
            });
            drop(lost);
            ctx.writeback_failed
                .store(true, std::sync::atomic::Ordering::Release);
            return;
        }
    }
}

/// Whether `current` refers to `file`.
fn same(current: &Weak<File>, file: &Arc<File>) -> bool {
    std::ptr::eq(current.as_ptr(), Arc::as_ptr(file))
}

#[cfg(target_os = "linux")]
fn start(file: &File, from: u64, len: u64) -> std::io::Result<()> {
    crate::fault::inject_background(crate::fault::Point::EarlyWriteback)?;
    crate::mmap::start_writeback(file, from, len)
}

#[cfg(not(target_os = "linux"))]
fn start(_file: &File, _from: u64, _len: u64) -> std::io::Result<()> {
    Ok(())
}

/// Errors that mean the call is not supported here, not that write-back failed.
fn unsupported(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP | libc::ESPIPE)
    )
}
