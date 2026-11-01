//! Background prefaulting of the active segment (ADR 0020).
//!
//! One helper thread per writer keeps the pages ahead of the write position faulted in, so the writer's own stores
//! rarely take a page fault. The helper only calls `madvise` through [`Advice`]; it never touches the mapped bytes.
//! Everything here is a hint: if the thread can't start, falls behind or stops, the writer faults pages itself, as it
//! would without prefaulting.

use crate::mmap::Advice;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;

/// How far past the write position the helper keeps pages prefaulted.
pub(crate) const WINDOW: u64 = 8 << 20;
/// How far the write position moves between wake-ups of the helper.
const STEP: u64 = 1 << 20;
/// The largest range advised in one call, so that a roll or shutdown is noticed quickly.
const CHUNK: u64 = 1 << 20;

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
        if let Err(e) = current.prefault(from, to - from) {
            // Before Linux 5.14 the advice doesn't exist: stop for the rest of this writer's life. Other errors (for
            // example a full btrfs volume) only mean the writer faults those pages itself.
            if e.kind() == std::io::ErrorKind::InvalidInput {
                return;
            }
        }
        done = to;
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

    #[test]
    fn helper_follows_rolls_and_stops_on_drop() {
        let (_dir1, _file1, map1) = mapping(4 << 20);
        let (_dir2, _file2, map2) = mapping(4 << 20);
        let mut prefaulter = Prefaulter::start(map1.advice(), 64).unwrap();
        for offset in (64..4 << 20).step_by(4096) {
            prefaulter.advanced(offset);
        }
        prefaulter.switch(map2.advice(), 64);
        drop(map1);
        prefaulter.advanced(3 << 20);
        drop(prefaulter); // joins the thread
        assert!(map2.bytes(0, 64).unwrap().iter().all(|&b| b == 0));
    }
}
