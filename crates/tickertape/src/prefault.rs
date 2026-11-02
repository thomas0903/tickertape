//! Background prefaulting of the active segment (ADR 0020) and preparation of the next one (ADR 0022).
//!
//! One helper thread per writer keeps the pages ahead of the write position faulted in, so the writer's own stores
//! rarely take a page fault. The helper only calls `madvise` through [`Advice`]; it never touches the mapped bytes.
//! Everything here is a hint: if the thread can't start, falls behind or stops, the writer faults pages itself, as it
//! would without prefaulting.

use crate::mmap::{self, Advice, MapRw};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;
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
}

impl Prefaulter {
    /// Starts the helper for `advice`'s segment with the writer at `offset`. `None` if the thread can't be spawned.
    pub(crate) fn start(advice: Advice, offset: u64) -> Option<Prefaulter> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                advice,
                epoch: 0,
                start: offset,
                target: offset.saturating_add(WINDOW),
                stop: false,
            }),
            wake: Condvar::new(),
            #[cfg(test)]
            calls: Mutex::new(Vec::new()),
        });
        let helper = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("tickertape-prefault".into())
            .spawn(move || run(&helper))
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

    /// Called after a roll: prefault the new segment from `offset`.
    pub(crate) fn switch(&mut self, advice: Advice, offset: u64) {
        self.next_wake = offset.saturating_add(STEP);
        let mut state = lock(&self.shared);
        state.advice = advice;
        state.epoch = state.epoch.wrapping_add(1);
        state.start = offset;
        state.target = offset.saturating_add(WINDOW);
        drop(state);
        self.shared.wake.notify_one();
    }
}

impl Drop for Prefaulter {
    fn drop(&mut self) {
        lock(&self.shared).stop = true;
        self.shared.wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
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

/// A preallocated, synced and mapped segment file that is not published yet (ADR 0022): all zeros, named
/// [`crate::journal::SPARE_FILE`]. At roll the writer writes its header, syncs it and links it into place.
#[derive(Debug)]
pub(crate) struct Spare {
    pub(crate) file: File,
    pub(crate) map: MapRw,
    pub(crate) size: u64,
}

/// Starts preparing a spare segment of `size` bytes in `dir` on a short-lived thread. `None` if the thread can't be
/// spawned (the writer then creates the next segment itself, as without a spare).
pub(crate) fn prepare_spare(dir: &Path, size: u64) -> Option<JoinHandle<io::Result<Spare>>> {
    let dir = dir.to_path_buf();
    std::thread::Builder::new()
        .name("tickertape-spare".into())
        .spawn(move || make_spare(&dir, size))
        .ok()
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
        crate::fault::inject_background(crate::fault::Point::SparePreallocate)?;
        mmap::preallocate(&file, size)?;
        mmap::sync_data(&file)?;
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

    #[test]
    fn prefault_keeps_contents_and_clamps_to_the_mapping() {
        let (_dir, _file, mut map) = mapping(1 << 20);
        map.bytes_mut(4096, 4).unwrap().copy_from_slice(b"abcd");
        let advice = map.advice();
        advice.prefault(0, 1 << 30).unwrap();
        advice.prefault(5000, 10).unwrap();
        advice.prefault(2 << 20, 4096).unwrap();
        assert_eq!(map.bytes(4096, 4).unwrap(), b"abcd");
        assert!(map.bytes(0, 4096).unwrap().iter().all(|&b| b == 0));
    }

    #[test]
    fn advice_keeps_the_mapping_alive_after_the_writer_drops_it() {
        let (_dir, _file, map) = mapping(1 << 20);
        let advice = map.advice();
        drop(map);
        advice.prefault(0, 1 << 20).unwrap();
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
        let mut prefaulter = Prefaulter::start(map1.advice(), 64).unwrap();
        let shared = Arc::clone(&prefaulter.shared);
        wait_for_call(&shared, 4 << 20);
        prefaulter.switch(map2.advice(), 64);
        drop(map1);
        wait_for_call(&shared, 32 << 20);
        let before = shared.calls.lock().unwrap().len();
        prefaulter.advanced(20 << 20); // past the first 8 MiB window: more advice for the new segment
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
