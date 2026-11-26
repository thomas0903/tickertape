//! Background prefaulting of the active segment (ADR 0020) and preparation of the next one (ADR 0022).
//!
//! One helper thread per writer keeps the pages ahead of the write position faulted in, so the writer's own stores
//! rarely take a page fault. The helper only calls `madvise` through [`Advice`]; it never touches the mapped bytes.
//! Everything here is a hint: if the thread can't start, falls behind or stops, the writer faults pages itself, as it
//! would without prefaulting.

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
/// The window the helper keeps prefaulted: [`WINDOW`], or with `--cfg tickertape_pf_exp` the experiment's
/// (`TT_PF_WINDOW_KIB`, ADR 0057; never in a default build).
#[inline]
fn window() -> u64 {
    #[cfg(tickertape_pf_exp)]
    return exp::window();
    #[cfg(not(tickertape_pf_exp))]
    WINDOW
}

/// EXPERIMENT (ADR 0057, compiled only with `--cfg tickertape_pf_exp`; no default build has it): switches read once
/// from the environment.
/// - `TT_PF_WINDOW_KIB`: the prefault window in KiB instead of [`WINDOW`].
/// - `TT_PF_RETOUCH`: at each wake-up the helper prefaults again from the writer's page, so that pages a write-back
///   cleaned (and write-protected) ahead of the writer are made writable again before the writer reaches them.
/// - `TT_PF_WB_LAG_KIB`: early write-back stops this far behind the writer, rounded down to `TT_PF_WB_ALIGN_KIB`
///   (default 2 MiB, the largest folio of the page cache with 4 KiB pages), so that it never writes back the folio
///   that holds the writer's position.
#[cfg(tickertape_pf_exp)]
pub(crate) mod exp {
    use std::sync::OnceLock;

    fn kib(name: &str) -> Option<u64> {
        std::env::var(name).ok()?.parse::<u64>().ok().map(|k| k << 10)
    }

    pub(crate) fn window() -> u64 {
        static W: OnceLock<u64> = OnceLock::new();
        *W.get_or_init(|| kib("TT_PF_WINDOW_KIB").unwrap_or(super::WINDOW))
    }

    pub(crate) fn retouch() -> bool {
        static R: OnceLock<bool> = OnceLock::new();
        *R.get_or_init(|| std::env::var_os("TT_PF_RETOUCH").is_some())
    }

    /// The end of the range early write-back may start for a writer at `position`: with `TT_PF_WB_LAG_KIB` set,
    /// `position` less the lag, rounded down to `TT_PF_WB_ALIGN_KIB` (default 2 MiB).
    pub(crate) fn writeback_limit(position: u64, page: u64) -> u64 {
        static L: OnceLock<(u64, u64)> = OnceLock::new();
        let (lag, align) = *L.get_or_init(|| {
            (
                kib("TT_PF_WB_LAG_KIB").unwrap_or(0),
                kib("TT_PF_WB_ALIGN_KIB").unwrap_or(2 << 20).max(4096),
            )
        });
        match lag {
            0 => position / page * page,
            lag => position.saturating_sub(lag) / align * align,
        }
    }
}

/// How far the write position moves between wake-ups of the helper.
const STEP: u64 = if cfg!(target_vendor = "apple") {
    1 << 20
} else {
    64 << 10
};
/// The largest range advised in one call, so that a roll or shutdown is noticed quickly.
const CHUNK: u64 = STEP;

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
    /// EXPERIMENT (`TT_PF_RETOUCH`): prefault again from this offset.
    #[cfg(tickertape_pf_exp)]
    restart: Option<u64>,
}

impl Prefaulter {
    /// Starts the helper for `advice`'s segment with the writer at `offset`. `None` if the thread can't be spawned.
    pub(crate) fn start(advice: Advice, offset: u64, cpus: HelperCpus) -> Option<Prefaulter> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                advice,
                epoch: 0,
                start: offset,
                target: offset.saturating_add(window()),
                stop: false,
                #[cfg(tickertape_pf_exp)]
                restart: None,
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
            state.target = offset.saturating_add(window());
            #[cfg(tickertape_pf_exp)]
            if exp::retouch() {
                state.restart = Some(offset);
            }
            drop(state);
            self.shared.wake.notify_one();
        }
    }

    /// Called after a roll: prefault the new segment from `offset`.
    pub(crate) fn switch(&mut self, advice: Advice, offset: u64) {
        self.next_wake = offset.saturating_add(STEP);
        let mut state = lock(&self.shared);
        state.advice = advice;
        state.epoch = state.epoch.wrapping_add(1);
        state.start = offset;
        state.target = offset.saturating_add(window());
        #[cfg(tickertape_pf_exp)]
        {
            state.restart = None;
        }
        drop(state);
        self.shared.wake.notify_one();
    }
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

/// The helper thread: advise in chunks up to the target, then sleep until woken.
fn run(shared: &Shared) {
    let mut epoch = None;
    let mut advice: Option<Advice> = None;
    let mut done = 0;
    loop {
        let (current, from, to) = {
            let mut state = lock(shared);
            loop {
                if state.stop {
                    return;
                }
                if epoch != Some(state.epoch) {
                    epoch = Some(state.epoch);
                    advice = Some(state.advice.clone());
                    done = state.start;
                }
                #[cfg(tickertape_pf_exp)]
                if let Some(from) = state.restart.take() {
                    done = done.min(from);
                }
                let limit = state.target.min(state.advice.len());
                if done < limit {
                    break;
                }
                state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            let limit = state.target.min(state.advice.len());
            match &advice {
                Some(current) => (current.clone(), done, limit.min(done.saturating_add(CHUNK))),
                None => return,
            }
        };
        #[cfg(test)]
        shared
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(current.len());
        if let Err(e) = current.prefault(from, to - from) {
            // An unsupported advice stops the helper for the rest of this writer's life. Other errors only mean the
            // writer faults those pages itself.
            if e.kind() == std::io::ErrorKind::InvalidInput {
                return;
            }
        }
        done = to;
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

    #[test]
    fn helper_follows_rolls_and_stops_on_drop() {
        let (_dir1, _file1, map1) = mapping(4 << 20);
        let (_dir2, _file2, map2) = mapping(32 << 20);
        let mut prefaulter = Prefaulter::start(map1.advice(), 64, HelperCpus::Inherited).unwrap();
        let shared = Arc::clone(&prefaulter.shared);
        wait_for_call(&shared, 4 << 20);
        if !supported(map1.advice().prefault(0, 4096)) {
            // An unsupported advice stops the helper after its first call: the thread releases the state by itself.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while Arc::strong_count(&shared) > 2 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the helper kept running after EINVAL"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            drop(prefaulter);
            assert_eq!(Arc::strong_count(&shared), 1);
            return;
        }
        prefaulter.switch(map2.advice(), 64);
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
