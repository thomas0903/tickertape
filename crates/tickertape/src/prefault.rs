//! Background prefaulting of the active segment (ADR 0020), release of the segments the writer let go of (ADR 0059),
//! and preparation of the next segment (ADR 0022).
//!
//! One helper thread per writer keeps the pages ahead of the write position faulted in, so the writer's own stores
//! rarely take a page fault. The helper only calls `madvise` through [`Advice`]; it never touches the mapped bytes.
//! Everything here is a hint: if the thread can't start, falls behind or stops, the writer faults pages itself, as it
//! would without prefaulting.
//!
//! On Linux the same thread drops the file and mapping handles of the segments the writer's dirty lists release
//! ([`Released`]), so that the `munmap` and `close` they come to happen off the writer's thread (the writer does not
//! hand them over on macOS, ADR 0059). At most
//! [`RELEASE_QUEUE`] wait for it (the one it is dropping included); when that many wait, or the thread has ended, the
//! writer drops the next one itself, as without the helper, so the files and mappings a writer holds stay bounded.

use crate::checkpoint::Released;
use crate::config::HelperCpus;
use crate::mmap::{self, Advice, MapRw};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;

/// How far past the write position the helper keeps pages prefaulted: 8 MiB of `MADV_WILLNEED` on macOS (ADR 0020),
/// 256 KiB of `MADV_POPULATE_WRITE` on Linux (ADR 0025).
pub(crate) const WINDOW: u64 = if cfg!(target_vendor = "apple") {
    8 << 20
} else {
    256 << 10
};
/// How far the write position moves between wake-ups of the helper.
const STEP: u64 = if cfg!(target_vendor = "apple") {
    1 << 20
} else {
    64 << 10
};
/// The largest range advised in one call, so that a roll or shutdown is noticed quickly.
const CHUNK: u64 = STEP;
/// How many released segments may wait for the helper to drop them, the one it is dropping included (ADR 0059). Two,
/// so that a roll finds room while the helper is still unmapping the previous roll's segment (1.7–16 ms on macOS), or
/// a request that releases a segment comes right after a roll that did. A writer holds at most this many segments more
/// than it would if it dropped them itself.
pub(crate) const RELEASE_QUEUE: usize = 2;

/// The writer's side: a handle on the helper thread, stopped and joined on drop.
#[derive(Debug)]
pub(crate) struct Prefaulter {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// The write offset at which the writer next wakes the helper.
    next_wake: u64,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    /// Test only: the mapping length of each advice call, in order.
    #[cfg(test)]
    calls: Mutex<Vec<u64>>,
}

#[derive(Debug)]
struct State {
    /// The active segment's mapping.
    advice: Advice,
    /// Changes whenever `advice` is replaced (a roll), so the helper restarts from `start`.
    epoch: u64,
    /// Where prefaulting starts in a newly switched-to segment.
    start: u64,
    /// Prefault up to this offset.
    target: u64,
    stop: bool,
    /// Released segments for the helper to drop, oldest first (ADR 0059). With `releasing`, at most [`RELEASE_QUEUE`].
    release: Vec<Released>,
    /// Whether the helper is dropping one it took from `release`.
    releasing: bool,
    /// Set when the thread has ended (stopped, or panicked), after which it takes nothing more: the writer then drops
    /// what it releases itself.
    ended: bool,
}

impl Prefaulter {
    /// Starts the helper for `advice`'s segment with the writer at `offset`. `None` if the thread can't be spawned.
    pub(crate) fn start(advice: Advice, offset: u64, cpus: HelperCpus) -> Option<Prefaulter> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                advice,
                epoch: 0,
                start: offset,
                target: offset.saturating_add(WINDOW),
                stop: false,
                release: Vec::with_capacity(RELEASE_QUEUE),
                releasing: false,
                ended: false,
            }),
            wake: Condvar::new(),
            #[cfg(test)]
            calls: Mutex::new(Vec::new()),
        });
        let helper = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("tickertape-prefault".into())
            .spawn(move || {
                crate::affinity::place_helper(&cpus);
                run(&helper);
            })
            .ok()?;
        Some(Prefaulter {
            shared,
            thread: Some(thread),
            next_wake: offset.saturating_add(STEP),
        })
    }

    /// Called after each append with the new write offset: wakes the helper once per [`STEP`].
    #[inline]
    pub(crate) fn advanced(&mut self, offset: u64) {
        if offset >= self.next_wake {
            self.next_wake = offset.saturating_add(STEP);
            let mut state = lock(&self.shared);
            state.target = offset.saturating_add(WINDOW);
            drop(state);
            self.shared.wake.notify_one();
        }
    }

    /// Called after a roll: prefault the new segment from `offset`, and drop `released` (the segment the roll's push
    /// released from the writer's dirty list, if any). In the same lock and wake-up. What the helper cannot take
    /// ([`Prefaulter::release`]) is given back, for the writer to drop.
    pub(crate) fn switch(
        &mut self,
        advice: Advice,
        offset: u64,
        released: Option<Released>,
    ) -> Option<Released> {
        self.next_wake = offset.saturating_add(STEP);
        let mut state = lock(&self.shared);
        state.advice = advice;
        state.epoch = state.epoch.wrapping_add(1);
        state.start = offset;
        state.target = offset.saturating_add(WINDOW);
        let refused = released.and_then(|released| offer(&mut state, released));
        drop(state);
        self.shared.wake.notify_one();
        refused
    }

    /// Hands `released` to the helper to drop (unmap and close, unless something else still holds them). Gives it
    /// back if [`RELEASE_QUEUE`] segments wait for the helper already, or if the thread has ended: the caller then drops
    /// it, on its own thread, as without the helper. Never waits for the helper.
    pub(crate) fn release(&self, released: Released) -> Option<Released> {
        let mut state = lock(&self.shared);
        let refused = offer(&mut state, released);
        drop(state);
        if refused.is_none() {
            self.shared.wake.notify_one();
        }
        refused
    }

    /// How many released segments wait for the helper, the one it is dropping included.
    #[cfg_attr(not(any(test, feature = "fault-injection")), allow(dead_code))]
    pub(crate) fn releases_waiting(&self) -> usize {
        let state = lock(&self.shared);
        state.release.len() + usize::from(state.releasing)
    }

    /// Lets go of the handle without stopping or joining the thread, in a forked child (the thread is the parent's):
    /// the segments waiting for it to drop them are dropped here instead, so the child does not keep their descriptors
    /// and mappings, unless the helper's lock was held at the fork (it then stays held in the child for good, and is
    /// only tried). A segment the helper was dropping at the fork stays in the child, as the helper's own references
    /// to the active segment's mapping do.
    pub(crate) fn release_after_fork(self) {
        let state = match self.shared.state.try_lock() {
            Ok(state) => Some(state),
            Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        };
        let waiting = state.map(|mut state| std::mem::take(&mut state.release));
        drop(waiting);
        // Neither stopped nor joined (the thread does not exist here), and its handle is not detached either.
        std::mem::forget(self);
    }
}

/// Queues `released` for the helper, or gives it back if the queue is full or the thread has ended.
fn offer(state: &mut State, released: Released) -> Option<Released> {
    if state.ended || state.release.len() + usize::from(state.releasing) >= RELEASE_QUEUE {
        return Some(released);
    }
    state.release.push(released);
    None
}

impl Drop for Prefaulter {
    fn drop(&mut self) {
        #[cfg(test)]
        if let Some(hook) = test_hooks::ON_DROP.with(std::cell::Cell::take) {
            hook();
        }
        lock(&self.shared).stop = true;
        self.shared.wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Test-only instrumentation of [`Prefaulter`]'s drop.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::cell::Cell;

    thread_local! {
        /// Called (once) when a `Prefaulter` starts to drop on this thread, before it stops and joins the helper.
        pub(crate) static ON_DROP: Cell<Option<fn()>> = const { Cell::new(None) };
    }
}

/// The state is plain data that stays consistent even if a holder panicked, so a poisoned lock is still usable.
fn lock(shared: &Shared) -> MutexGuard<'_, State> {
    shared.state.lock().unwrap_or_else(|e| e.into_inner())
}

/// Marks the helper as ended when it returns or panics, and drops the released segments still waiting for it (on the
/// helper's thread): from then on the writer drops what it releases itself, and nothing waits for this thread.
struct HelperEnded<'a>(&'a Shared);

impl Drop for HelperEnded<'_> {
    fn drop(&mut self) {
        let mut state = lock(self.0);
        state.ended = true;
        state.releasing = false;
        let waiting = std::mem::take(&mut state.release);
        drop(state);
        drop(waiting);
    }
}

/// What the helper does next.
enum Work {
    /// Advise `[from, to)` of a mapping.
    Advise(Advice, u64, u64),
    /// Drop a released segment's file and mapping handles.
    Release(Released),
}

/// The helper thread: advise in chunks up to the target, drop the released segments, then sleep until woken. The
/// window of a newly switched-to segment is advised before the previous segment is released.
fn run(shared: &Shared) {
    let _ended = HelperEnded(shared);
    // False once the kernel has rejected the advice (Linux before 5.14): the thread then only releases segments.
    let mut advise = true;
    let mut epoch = None;
    let mut advice: Option<Advice> = None;
    let mut done = 0;
    loop {
        let work = {
            let mut state = lock(shared);
            loop {
                if state.stop {
                    return;
                }
                if advise && epoch != Some(state.epoch) {
                    epoch = Some(state.epoch);
                    advice = Some(state.advice.clone());
                    done = state.start;
                }
                let limit = state.target.min(state.advice.len());
                if let Some(current) = advice.as_ref().filter(|_| done < limit) {
                    break Work::Advise(
                        current.clone(),
                        done,
                        limit.min(done.saturating_add(CHUNK)),
                    );
                }
                if !state.release.is_empty() {
                    state.releasing = true;
                    break Work::Release(state.release.remove(0));
                }
                state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        };
        match work {
            Work::Advise(current, from, to) => {
                #[cfg(test)]
                shared
                    .calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(current.len());
                if let Err(e) = current.prefault(from, to - from) {
                    // An unsupported advice stops prefaulting for the rest of this writer's life (the thread stays, to
                    // release segments). Other errors only mean the writer faults those pages itself.
                    if e.kind() == std::io::ErrorKind::InvalidInput {
                        advise = false;
                        advice = None;
                        continue;
                    }
                }
                done = to;
            }
            Work::Release(released) => {
                crate::fault::hold_point(crate::fault::Point::SegmentRelease);
                if crate::fault::inject_background(crate::fault::Point::SegmentReleasePanic)
                    .is_err()
                {
                    panic!("injected panic of the prefault helper while it releases a segment");
                }
                drop(released);
                lock(shared).releasing = false;
            }
        }
    }
}

/// A preallocated and mapped segment file that is not published yet (ADR 0022): all zeros, named
/// [`crate::journal::SPARE_FILE`]; synced on macOS only. At roll the writer writes its header and links it into
/// place (the next checkpoint syncs it).
#[derive(Debug)]
pub(crate) struct Spare {
    pub(crate) file: File,
    pub(crate) map: MapRw,
    pub(crate) size: u64,
}

/// The writer's side of preparing the next segment in the background (ADR 0022).
///
/// - **macOS:** a short-lived thread per spare, and [`Spares::take`] waits for it.
/// - **Linux** (`WriterConfig::spare_segment`, off by default): one thread for the writer's life, woken for each spare,
///   and `take` never waits (ADR 0049). A thread started for each spare made the writer's worst appends longer there
///   than the roll it saved. [`Spares::wait`] is for the error path only.
#[derive(Debug)]
pub(crate) struct Spares {
    dir: PathBuf,
    /// Whether a spare has been requested since the last roll that succeeded, or cannot be asked for any more.
    requested: bool,
    source: Source,
    /// The process that started the threads: in a forked child they do not exist, so they are not joined there.
    pid: u32,
}

/// What came of waiting for a spare ([`Spares::wait`]).
#[derive(Debug)]
pub(crate) enum Waited {
    Ready(Spare),
    /// A file that a panicked spare thread had left was removed.
    Freed,
    /// There was no spare, or preparing it failed.
    Nothing,
}

#[derive(Debug)]
enum Source {
    /// Starts a thread for each request; the handle is the one in progress.
    PerRequest {
        size: u64,
        cpus: HelperCpus,
        preparing: Option<JoinHandle<io::Result<Spare>>>,
    },
    Thread {
        shared: Arc<SpareShared>,
        thread: Option<JoinHandle<()>>,
    },
}

#[derive(Debug)]
struct SpareShared {
    state: Mutex<SpareState>,
    /// Signalled for a request and to stop.
    wake: Condvar,
    /// Signalled when a preparation ends, and when the thread does.
    done: Condvar,
}

#[derive(Debug, Default)]
struct SpareState {
    /// Requested or being prepared; false once the thread has ended.
    pending: bool,
    ready: Option<Spare>,
    stop: bool,
    /// The thread has ended, by request or by a panic: it prepares nothing any more.
    ended: bool,
}

fn lock_spares(shared: &SpareShared) -> MutexGuard<'_, SpareState> {
    shared.state.lock().unwrap_or_else(|e| e.into_inner())
}

impl Spares {
    /// `None` if the thread can't be spawned (the writer then creates every segment itself).
    pub(crate) fn start(dir: &Path, size: u64, cpus: HelperCpus) -> Option<Spares> {
        let source = if cfg!(target_vendor = "apple") {
            Source::PerRequest {
                size,
                cpus,
                preparing: None,
            }
        } else {
            let shared = Arc::new(SpareShared {
                state: Mutex::new(SpareState::default()),
                wake: Condvar::new(),
                done: Condvar::new(),
            });
            let helper = Arc::clone(&shared);
            let dir = dir.to_path_buf();
            let thread = std::thread::Builder::new()
                .name("tickertape-spare".into())
                .spawn(move || {
                    crate::affinity::place_helper(&cpus);
                    prepare_spares(&helper, &dir, size);
                })
                .ok()?;
            Source::Thread {
                shared,
                thread: Some(thread),
            }
        };
        Some(Spares {
            dir: dir.to_path_buf(),
            requested: false,
            source,
            pid: std::process::id(),
        })
    }

    /// Whether a spare has been requested and not used yet.
    #[inline]
    pub(crate) fn requested(&self) -> bool {
        self.requested
    }

    /// Asks for a spare. Call only while none is [`Spares::requested`].
    pub(crate) fn request(&mut self) {
        self.requested = true;
        match &mut self.source {
            Source::PerRequest {
                size,
                cpus,
                preparing,
            } => {
                let (dir, size, cpus) = (self.dir.clone(), *size, cpus.clone());
                // If the thread can't be spawned, the writer creates the next segment itself, as without a spare.
                *preparing = std::thread::Builder::new()
                    .name("tickertape-spare".into())
                    .spawn(move || {
                        crate::affinity::place_helper(&cpus);
                        make_spare(&dir, size)
                    })
                    .ok();
            }
            Source::Thread { shared, .. } => {
                let mut state = lock_spares(shared);
                state.pending = !state.ended;
                drop(state);
                shared.wake.notify_one();
            }
        }
    }

    /// The prepared spare, if there is one. On macOS one still being prepared is waited for; on Linux not, and it is the
    /// next roll's. The request stays until [`Spares::rolled`], so a roll that fails asks for no further spare.
    pub(crate) fn take(&mut self) -> Option<Spare> {
        match &mut self.source {
            Source::PerRequest { preparing, .. } => {
                // A preparation that failed removed its own file; one that panicked left it to the next preparation.
                preparing.take()?.join().ok()?.ok()
            }
            Source::Thread { shared, .. } => lock_spares(shared).ready.take(),
        }
    }

    /// The roll has published a segment: the new active one may ask for a spare, unless one is still being prepared or
    /// the thread has ended by a panic and no further spare will come.
    pub(crate) fn rolled(&mut self) {
        self.requested = match &self.source {
            Source::PerRequest { .. } => false,
            Source::Thread { shared, .. } => {
                let state = lock_spares(shared);
                state.pending || state.ended || state.ready.is_some()
            }
        };
    }

    /// Whether a spare is being prepared now and has blocks of its own, or will have them. Never on macOS after
    /// [`Spares::take`], which has waited for it.
    pub(crate) fn preparing(&self) -> bool {
        match &self.source {
            Source::PerRequest { .. } => false,
            Source::Thread { shared, .. } => lock_spares(shared).pending,
        }
    }

    /// Waits for a spare that is being prepared, and returns what came of it: for a roll that could not create its
    /// segment for lack of space, which the spare may have taken (ADR 0049). There is no time limit: it lasts as
    /// long as the filesystem's `fallocate`. On macOS [`Spares::take`] has waited already.
    pub(crate) fn wait(&mut self) -> Waited {
        match &mut self.source {
            Source::PerRequest { .. } => Waited::Nothing,
            Source::Thread { shared, .. } => {
                let mut state = lock_spares(shared);
                while state.pending {
                    state = shared.done.wait(state).unwrap_or_else(|e| e.into_inner());
                }
                let spare = state.ready.take();
                let ended = state.ended;
                drop(state);
                match spare {
                    Some(spare) => Waited::Ready(spare),
                    // A thread that panicked leaves its file, and the blocks it took: they are freed here.
                    None if ended => {
                        let removed = fs::remove_file(self.dir.join(crate::journal::SPARE_FILE));
                        if removed.is_ok() {
                            Waited::Freed
                        } else {
                            Waited::Nothing
                        }
                    }
                    None => Waited::Nothing,
                }
            }
        }
    }

    /// Whether no spare is being prepared, for tests.
    #[cfg(feature = "fault-injection")]
    pub(crate) fn settled(&self) -> bool {
        match &self.source {
            Source::PerRequest { preparing, .. } => {
                preparing.as_ref().is_none_or(JoinHandle::is_finished)
            }
            Source::Thread { shared, .. } => !lock_spares(shared).pending,
        }
    }

    /// Lets go of everything without joining or removing anything, in a forked child (the threads are the parent's).
    pub(crate) fn release_after_fork(self) {
        std::mem::forget(self);
    }
}

impl Drop for Spares {
    /// Waits for the thread, which may be inside `fallocate` for as long as the filesystem holds it, and removes a
    /// prepared spare, in the process that started it.
    fn drop(&mut self) {
        if self.pid != std::process::id() {
            // A forked child: the thread does not exist here, and the spare is the parent writer's.
            match &mut self.source {
                Source::PerRequest { preparing, .. } => std::mem::forget(preparing.take()),
                Source::Thread { thread, .. } => std::mem::forget(thread.take()),
            }
            return;
        }
        let mut panicked = false;
        let spare = match &mut self.source {
            Source::PerRequest { preparing, .. } => preparing
                .take()
                .and_then(|preparing| preparing.join().ok()?.ok()),
            Source::Thread { shared, thread } => {
                lock_spares(shared).stop = true;
                shared.wake.notify_one();
                // A thread that panicked leaves its file, as a spare that is not used does.
                panicked = thread.take().is_some_and(|thread| thread.join().is_err());
                lock_spares(shared).ready.take()
            }
        };
        if spare.is_some() || panicked {
            let _ = fs::remove_file(self.dir.join(crate::journal::SPARE_FILE));
        }
    }
}

/// Marks the spare thread as ended, also when it panics, so that [`Spares::wait`] never waits for it.
struct Ended<'a>(&'a SpareShared);

impl Drop for Ended<'_> {
    fn drop(&mut self) {
        let mut state = lock_spares(self.0);
        state.pending = false;
        state.ended = true;
        drop(state);
        self.0.done.notify_all();
    }
}

/// The spare thread (Linux): prepares one spare per request.
fn prepare_spares(shared: &SpareShared, dir: &Path, size: u64) {
    let _ended = Ended(shared);
    let mut state = lock_spares(shared);
    loop {
        if state.stop {
            return;
        }
        if !state.pending {
            state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
            continue;
        }
        drop(state);
        let made = make_spare(dir, size).ok();
        state = lock_spares(shared);
        state.pending = false;
        state.ready = made;
        shared.done.notify_all();
    }
}

fn make_spare(dir: &Path, size: u64) -> io::Result<Spare> {
    let path = dir.join(crate::journal::SPARE_FILE);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    let made = (|| {
        let len = usize::try_from(size).map_err(|_| io::Error::other("segment too large"))?;
        crate::fault::hold_point(crate::fault::Point::SparePreallocate);
        crate::fault::inject_background(crate::fault::Point::SparePreallocate)?;
        mmap::preallocate(&file, size)?;
        // Not on Linux: the sync forces a journal commit, which ADR 0023 found stalling the writer, and nothing needs
        // it there. `fallocate` has allocated the blocks (C10), and the checkpoint that first covers the segment syncs
        // its size and header, as for a segment a roll creates itself (ADR 0038 → Rolls without a sync).
        if cfg!(target_vendor = "apple") {
            mmap::sync_data(&file)?;
        }
        crate::fault::hold_point(crate::fault::Point::SpareMap);
        if crate::fault::inject_background(crate::fault::Point::SparePanic).is_err() {
            panic!("injected panic of the spare thread");
        }
        crate::fault::inject_background(crate::fault::Point::SpareMap)?;
        let map = MapRw::new(&file, len)?;
        let _ = map.advice().prefault(0, WINDOW);
        Ok(map)
    })();
    match made {
        Ok(map) => Ok(Spare { file, map, size }),
        Err(e) => {
            let _ = fs::remove_file(&path);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmap::MapRw;

    fn mapping(len: u64) -> (tempfile::TempDir, std::fs::File, MapRw) {
        let dir = tempfile::tempdir().unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.path().join("segment"))
            .unwrap();
        crate::mmap::preallocate(&file, len).unwrap();
        let map = MapRw::new(&file, len as usize).unwrap();
        (dir, file, map)
    }

    /// A spare thread that panics is seen as ended: nothing waits for it, and no further spare is requested.
    #[test]
    fn a_panicked_spare_thread_is_not_waited_for() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SpareShared {
            state: Mutex::new(SpareState::default()),
            wake: Condvar::new(),
            done: Condvar::new(),
        });
        let helper = Arc::clone(&shared);
        let thread = std::thread::spawn(move || {
            let _ended = Ended(&helper);
            let mut state = lock_spares(&helper);
            while !state.pending {
                state = helper.wake.wait(state).unwrap();
            }
            drop(state);
            panic!("a panic of the spare thread (expected in this test)");
        });
        let mut spares = Spares {
            dir: dir.path().to_path_buf(),
            requested: false,
            source: Source::Thread {
                shared,
                thread: Some(thread),
            },
            pid: std::process::id(),
        };
        spares.request();
        assert!(matches!(spares.wait(), Waited::Nothing));
        spares.rolled();
        assert!(spares.requested(), "asked for a spare again and again");
        assert!(spares.take().is_none());
        assert!(!spares.preparing());
    }

    /// Whether the platform supports the prefault advice: Linux before 5.14 rejects `MADV_POPULATE_WRITE` with
    /// `EINVAL` (`InvalidInput`), which the helper treats as "stop" (ADR 0025).
    fn supported(result: std::io::Result<()>) -> bool {
        match result {
            Ok(()) => true,
            Err(e) if cfg!(target_os = "linux") && e.kind() == std::io::ErrorKind::InvalidInput => {
                false
            }
            Err(e) => panic!("prefault failed: {e}"),
        }
    }

    #[test]
    fn prefault_keeps_contents_and_clamps_to_the_mapping() {
        let (_dir, _file, mut map) = mapping(1 << 20);
        map.bytes_mut(4096, 4).unwrap().copy_from_slice(b"abcd");
        let advice = map.advice();
        if supported(advice.prefault(0, 1 << 30)) {
            assert!(supported(advice.prefault(5000, 10)));
            assert!(supported(advice.prefault(2 << 20, 4096)));
        }
        assert_eq!(map.bytes(4096, 4).unwrap(), b"abcd");
        assert!(map.bytes(0, 4096).unwrap().iter().all(|&b| b == 0));
    }

    #[test]
    fn advice_keeps_the_mapping_alive_after_the_writer_drops_it() {
        let (_dir, _file, map) = mapping(1 << 20);
        let advice = map.advice();
        drop(map);
        supported(advice.prefault(0, 1 << 20));
    }

    fn wait_for_call(shared: &Shared, len: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !shared.calls.lock().unwrap().contains(&len) {
            assert!(
                std::time::Instant::now() < deadline,
                "no advice call for a {len}-byte mapping"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// A released segment's file, and a weak reference that tells when it has been closed.
    fn released(len: u64) -> (tempfile::TempDir, Released, std::sync::Weak<File>) {
        let (dir, file, map) = mapping(len);
        let file = Arc::new(file);
        let weak = Arc::downgrade(&file);
        let released = Released {
            file: Some(file),
            flush: Some(map.flush_handle()),
        };
        (dir, released, weak)
    }

    fn wait_closed(weak: &std::sync::Weak<File>) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while weak.strong_count() > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the helper did not drop the segment"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// The helper drops what it is handed, a roll's release included; a stopped helper takes nothing and the caller
    /// gets the segment back; dropping the handle with segments queued drops them.
    #[test]
    fn the_helper_releases_segments_and_gives_back_what_it_cannot_take() {
        let (_dir, file, map) = mapping(1 << 20);
        let mut prefaulter = Prefaulter::start(map.advice(), 64, HelperCpus::Inherited).unwrap();
        let (_d1, one, weak_one) = released(1 << 16);
        assert!(prefaulter.release(one).is_none());
        wait_closed(&weak_one);
        let (_d2, two, weak_two) = released(1 << 16);
        assert!(prefaulter.switch(map.advice(), 64, Some(two)).is_none());
        wait_closed(&weak_two);
        assert_eq!(prefaulter.releases_waiting(), 0);

        // A helper that has ended takes nothing: the caller drops the segment itself.
        lock(&prefaulter.shared).ended = true;
        let (_d3, three, weak_three) = released(1 << 16);
        let back = prefaulter
            .release(three)
            .expect("an ended helper took a segment");
        assert_eq!(weak_three.strong_count(), 1);
        drop(back);
        assert_eq!(weak_three.strong_count(), 0);

        // Segments queued that no thread takes (here: the thread is stopped and joined first, and then the queue is
        // opened again) are dropped with the handle.
        lock(&prefaulter.shared).stop = true;
        prefaulter.shared.wake.notify_one();
        drop(prefaulter.thread.take().map(JoinHandle::join));
        lock(&prefaulter.shared).ended = false;
        let (_d4, four, weak_four) = released(1 << 16);
        assert!(prefaulter.release(four).is_none(), "the queue is free");
        assert_eq!(weak_four.strong_count(), 1);
        drop(prefaulter);
        assert_eq!(weak_four.strong_count(), 0);
        drop((file, map));
    }

    /// At most [`RELEASE_QUEUE`] wait, the one being released included; the next is given back at once.
    #[test]
    fn at_most_release_queue_segments_wait() {
        let (_dir, _file, map) = mapping(1 << 20);
        let prefaulter = Prefaulter::start(map.advice(), 64, HelperCpus::Inherited).unwrap();
        let mut kept = Vec::new();
        {
            // With the lock held, the helper takes nothing; one in hand counts as waiting.
            let mut state = lock(&prefaulter.shared);
            state.releasing = true;
            for _ in 1..RELEASE_QUEUE {
                let (dir, r, weak) = released(1 << 16);
                assert!(offer(&mut state, r).is_none());
                kept.push((dir, weak));
            }
            let (_dir, r, weak) = released(1 << 16);
            assert!(
                offer(&mut state, r).is_some(),
                "a full queue took a segment"
            );
            assert_eq!(weak.strong_count(), 0);
            state.releasing = false;
        }
        prefaulter.shared.wake.notify_one();
        for (_dir, weak) in &kept {
            wait_closed(weak);
        }
    }

    #[test]
    fn helper_follows_rolls_and_stops_on_drop() {
        let (_dir1, _file1, map1) = mapping(4 << 20);
        let (_dir2, _file2, map2) = mapping(32 << 20);
        let mut prefaulter = Prefaulter::start(map1.advice(), 64, HelperCpus::Inherited).unwrap();
        let shared = Arc::clone(&prefaulter.shared);
        wait_for_call(&shared, 4 << 20);
        if !supported(map1.advice().prefault(0, 4096)) {
            // An unsupported advice stops prefaulting after the first call; the thread stays, to release segments,
            // and holds no mapping: a segment it no longer advises is unmapped when the writer lets go of it.
            assert!(prefaulter.switch(map2.advice(), 64, None).is_none());
            drop(prefaulter);
            assert_eq!(Arc::strong_count(&shared), 1);
            return;
        }
        assert!(prefaulter.switch(map2.advice(), 64, None).is_none());
        drop(map1);
        wait_for_call(&shared, 32 << 20);
        let before = shared.calls.lock().unwrap().len();
        prefaulter.advanced(20 << 20); // past the first window on any platform: more advice for the new segment
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while shared.calls.lock().unwrap().len() == before {
            assert!(
                std::time::Instant::now() < deadline,
                "wake-up after advancing was ignored"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        drop(prefaulter); // joins the thread
        assert_eq!(
            Arc::strong_count(&shared),
            1,
            "the helper thread still holds the state"
        );
        assert!(map2.bytes(0, 64).unwrap().iter().all(|&b| b == 0));
    }
}
