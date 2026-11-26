//! Early write-back of the pages behind the writer (Linux; [`WriterConfig::early_writeback`], ADR 0038 → Early
//! write-back).
//!
//! A helper thread (`tickertape-writeback`) asks the kernel to start writing back what the writer has finished with
//! (`sync_file_range(SYNC_FILE_RANGE_WRITE)` through [`mmap::start_writeback`]), so that a checkpoint finds little
//! dirty data left and its syncs, and the write-back they force on the device, are short. The writer only compares
//! its offset with the next step after each append and, once per [`STEP`], takes a lock and wakes the helper. The
//! helper never touches the mapped bytes. It writes back up to the page that holds the writer's position, or, with
//! [`STOP_AT_FOLIO_BOUNDARY`] (ADR 0058, change 1; off), up to the [`FOLIO`]-aligned boundary below it ([`limit`]):
//! the kernel writes back, and write-protects, whole page-cache folios, so a range that ends inside the folio the
//! writer is filling writes that folio now and again once it is full, and makes the writer take a write fault on each
//! of its pages it has yet to fill. The rest of a segment is written back when the writer rolls past it
//! ([`WriteBack::switch`]); the rest of the active segment is left to the checkpoints.
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

/// Whether the helper stops at the [`FOLIO`] boundary below the writer's position (ADR 0058, change 1) instead of at the
/// page that holds it. **Off.** Its measurements (ADR 0058 → "As built: change 1") removed the extra device bytes, the
/// aarch64 stalls near the device's limit and most of the writer's write-back faults, but did not meet every gate set
/// for a default: in a few cells a checkpoint took slightly longer to become durable (it may find up to 128 KiB more to
/// write) and one reader's p99.9 was slightly higher, beyond the base's own spread. Making it the default is this line;
/// the tests run both settings.
pub(crate) const STOP_AT_FOLIO_BOUNDARY: bool = false;

/// The alignment of the end of the range the helper starts in the active segment with [`STOP_AT_FOLIO_BOUNDARY`]: 128
/// KiB (ADR 0058, change 1). The helper then never starts the write-back of the 128 KiB-aligned block that holds the
/// writer's position ([`limit`]).
///
/// Why: write-back works on page-cache folios, not pages. Starting the write-back of a range writes back every dirty
/// folio that overlaps it, whole, and makes each of its pages read-only again in the writer's mapping, so that the
/// next store into one of them takes a write fault to dirty it again (on ext4 sometimes through the file system's
/// journal, to update the file's times: 20–100 µs). Ending the range at the page that holds the position writes back
/// the folio the writer is still filling: the writer then takes a fault on each page of it that it has yet to fill
/// (7–8 faults per MiB were measured), and the device receives that folio twice, partly empty and again once full
/// (6–9 % more device bytes than appended; ADR 0058, `docs/benchmarks.md` 2026-11-26).
///
/// Why 128 KiB: with 4 KiB pages, ext4 (since Linux 6.16) and xfs (since 5.18) let the page cache use large folios for
/// regular files. The folios of a segment are allocated when the prefault helper's `MADV_POPULATE_WRITE`, or the
/// writer, faults on a page that is not cached; that fault goes through the page cache's read-around for mappings,
/// which sizes folios from the readahead window, at most the device's `read_ahead_kb` (128 KiB by default). On the
/// measured kernel (7.0) every folio of a segment was 128 KiB (order 5), on both file systems, and folios are
/// naturally aligned in the file, so a boundary at a multiple of 128 KiB is a folio boundary.
///
/// If the folios are smaller (kernels or file systems without large folios for regular files, such as ext4 before
/// 6.16, or a smaller `read_ahead_kb`): the helper only lags further than it needs to, and a checkpoint finds up to
/// 128 KiB more to write; nothing is written twice and the writer takes no fault for it. If they are larger (2 MiB
/// folios with a larger `read_ahead_kb` or huge pages in the page cache; with 16 KiB or 64 KiB base pages on aarch64
/// kernels built that way, a folio is at least one such page and read-around folios can be larger): the boundary is
/// then inside the writer's folio, which is written back as before. Being wrong about the folio size therefore costs
/// at most what ending the range at the writer's page cost, never more. A 2 MiB alignment, which would cover every
/// folio size, was measured and left a checkpoint too much to write (ADR 0058).
pub(crate) const FOLIO: u64 = 128 << 10;

/// The end of the range the helper may start in the active segment for a writer at `position`, with pages of `page`
/// bytes: the start of the page that holds `position`, or with `folio` ([`STOP_AT_FOLIO_BOUNDARY`]) the start of the
/// [`FOLIO`]-aligned block that holds it (of the page, if a page is larger). Everything below it is finished; the block
/// itself, and so the first block of a segment while the writer is in it, header included, is left alone until the
/// writer has left it. The end of a segment is not limited here: at a roll the helper writes the segment back to the
/// end of its seal ([`WriteBack::switch`]).
pub(crate) fn limit(position: u64, page: u64, folio: bool) -> u64 {
    let block = if folio { FOLIO.max(page) } else { page };
    position / block * block
}

#[cfg(test)]
thread_local! {
    /// For tests: the setting a helper started from this thread uses instead of [`STOP_AT_FOLIO_BOUNDARY`].
    pub(crate) static FOLIO_FOR_TESTS: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

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
    /// [`STOP_AT_FOLIO_BOUNDARY`], read once at the start.
    folio: bool,
}

#[derive(Debug)]
struct State {
    /// The active segment's file; `None` once the helper has stopped.
    file: Option<Arc<File>>,
    /// The writer's position in `file`: everything before it is finished, but only what lies before [`limit`] of it is
    /// written back.
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
    /// For tests: how many times the writer has handed the helper a position or a roll, how many of those the helper
    /// had seen when it last went to wait, and every range it started, with what it knew then.
    #[cfg(test)]
    handed: u64,
    #[cfg(test)]
    seen: u64,
    #[cfg(test)]
    issued: Vec<Issued>,
}

fn folio_setting() -> bool {
    #[cfg(test)]
    if let Some(folio) = FOLIO_FOR_TESTS.with(|f| f.get()) {
        return folio;
    }
    STOP_AT_FOLIO_BOUNDARY
}

/// For tests: a range the helper started, with the writer's position it knew (`None` for a segment rolled past).
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Issued {
    /// The file's inode number.
    pub(crate) ino: u64,
    pub(crate) from: u64,
    pub(crate) to: u64,
    pub(crate) position: Option<u64>,
}

#[cfg(test)]
fn note_issued(state: &mut State, file: &File, from: u64, to: u64, position: Option<u64>) {
    use std::os::unix::fs::MetadataExt;
    let ino = file.metadata().map_or(0, |m| m.ino());
    state.issued.push(Issued {
        ino,
        from,
        to,
        position,
    });
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
                #[cfg(test)]
                handed: 0,
                #[cfg(test)]
                seen: 0,
                #[cfg(test)]
                issued: Vec::new(),
            }),
            wake: Condvar::new(),
            folio: folio_setting(),
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
            let mut state = lock(&self.shared);
            state.position = position;
            #[cfg(test)]
            {
                state.handed += 1;
            }
            drop(state);
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
        #[cfg(test)]
        {
            state.handed += 1;
        }
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

    /// For tests: waits until the helper has seen everything handed to it and waits to be woken, then returns the
    /// ranges it has started so far.
    #[cfg(test)]
    pub(crate) fn settled(&self) -> Vec<Issued> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let state = lock(&self.shared);
            if state.waiting && state.seen == state.handed {
                return state.issued.clone();
            }
            drop(state);
            assert!(
                std::time::Instant::now() < deadline,
                "the write-back helper did not settle"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
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

/// The helper thread: write back up to the [`limit`] of the writer's position, then sleep until woken. When it
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
                        #[cfg(test)]
                        note_issued(&mut state, &file, from, end, None);
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
                // Up to the page that holds the position, or the folio boundary below it (ADR 0058).
                let end = limit(state.position, page, shared.folio);
                if done < end {
                    let to = end.min(done.saturating_add(STEP));
                    let from = std::mem::replace(&mut done, to);
                    let file = Arc::clone(file);
                    #[cfg(test)]
                    {
                        let position = state.position;
                        note_issued(&mut state, &file, from, to, Some(position));
                    }
                    break Work::Range(file, from, to);
                }
                #[cfg(test)]
                {
                    state.seen = state.handed;
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
    crate::fault::hold_point(crate::fault::Point::EarlyWriteback);
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

#[cfg(test)]
mod tests {
    use super::*;

    const K: u64 = 1 << 10;

    /// With the folio boundary, the end of the range never reaches into the 128 KiB block that holds the position: at
    /// a boundary it is the boundary, just below one the boundary before, just above one that boundary; nothing while
    /// the writer is in a segment's first block (header included); and at the end of a segment the block the end is in
    /// is left (the roll writes it). Pages larger than 128 KiB align to the page. Without it, the end is the start of
    /// the page that holds the position, as before ADR 0058.
    #[test]
    fn the_limit_is_the_page_or_the_folio_boundary_below_the_position() {
        let page = 4 * K;
        assert_eq!(FOLIO, 128 * K);
        let cases = [
            (0, 0, 0),
            (64, 0, 0),
            (page, 0, page),
            (128 * K - 1, 0, 124 * K),
            (128 * K, 128 * K, 128 * K),
            (128 * K + 1, 128 * K, 128 * K),
            (128 * K + page, 128 * K, 128 * K + page),
            (256 * K - 1, 128 * K, 252 * K),
            (256 * K, 256 * K, 256 * K),
            (STEP + 5_000, STEP, STEP + page),
            (128 << 20, 128 << 20, 128 << 20),
            ((128 << 20) - 1, (128 << 20) - 128 * K, (128 << 20) - page),
        ];
        for (position, folio, paged) in cases {
            assert_eq!(limit(position, page, true), folio, "position {position}");
            assert_eq!(limit(position, page, false), paged, "position {position}");
            // 16 KiB and 64 KiB pages divide 128 KiB: the same folio boundaries.
            for page in [16 * K, 64 * K] {
                assert_eq!(
                    limit(position, page, true),
                    folio,
                    "position {position}, page {page}"
                );
                assert_eq!(limit(position, page, false), position / page * page);
            }
        }
        assert_eq!(limit(384 * K, 256 * K, true), 256 * K);
    }

    #[cfg(target_os = "linux")]
    fn context(dir: &std::path::Path) -> Arc<Context> {
        use crate::checkpoint::Marker;
        Arc::new(Context {
            dir: dir.to_path_buf(),
            marker: Mutex::new(Marker {
                file: File::create(dir.join("marker")).unwrap(),
                short: false,
            }),
            lost: Mutex::new(false),
            writeback_failure: Mutex::new(None),
            writeback_failed: std::sync::atomic::AtomicBool::new(false),
            notify: None,
            retention_sizes: Mutex::default(),
            durable: None,
        })
    }

    #[cfg(target_os = "linux")]
    fn segment(dir: &std::path::Path, name: &str) -> (Arc<File>, u64) {
        use std::os::unix::fs::MetadataExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.join(name))
            .unwrap();
        file.set_len(8 << 20).unwrap();
        let ino = file.metadata().unwrap().ino();
        (Arc::new(file), ino)
    }

    /// With the folio boundary, while the writer is in a 128 KiB block the helper starts no range that reaches into it,
    /// and what it starts of a segment is contiguous from its first byte; once the writer has rolled, the rest of the
    /// segment, the tail below the old boundary and the last partial block included, is written back to the end of its
    /// seal (Linux). With the old limit the ranges would have ended at the page that holds the position, inside the
    /// block.
    #[cfg(target_os = "linux")]
    #[test]
    fn with_the_folio_boundary_the_helper_leaves_the_writers_block_until_it_has_left_it() {
        follows(true);
    }

    /// Without it (the default), the helper writes back to the page that holds the position, and at a roll the rest of
    /// the segment to the end of its seal, as before ADR 0058 (Linux).
    #[cfg(target_os = "linux")]
    #[test]
    fn by_default_the_helper_writes_back_to_the_writers_page() {
        assert!(!folio_setting(), "update this test with the default");
        follows(false);
    }

    #[cfg(target_os = "linux")]
    fn follows(folio: bool) {
        let page = crate::mmap::page_size() as u64;
        let dir = tempfile::tempdir().unwrap();
        let (first, first_ino) = segment(dir.path(), "a");
        let (second, second_ino) = segment(dir.path(), "b");
        let header = crate::format::SEGMENT_HEADER_LEN as u64;
        FOLIO_FOR_TESTS.with(|f| f.set(Some(folio)));
        let mut wb = WriteBack::start(
            Arc::clone(&first),
            header,
            context(dir.path()),
            HelperCpus::default(),
        )
        .unwrap();
        FOLIO_FOR_TESTS.with(|f| f.set(None));
        // The first block of a segment, header included, is left while the writer is in it.
        assert!(wb.settled().is_empty());
        // Positions inside a block, just past a boundary and at the last byte before one, each at least a step past
        // the one before (the writer wakes the helper once per step).
        let positions = [
            STEP + 5_000,
            2 * STEP + 128 * K + 5_001,
            3 * STEP + 256 * K - 1,
        ];
        for position in positions {
            wb.advanced(position);
            let issued = wb.settled();
            let last = issued.last().unwrap();
            assert_eq!(last.ino, first_ino);
            assert_eq!(last.to, limit(position, page, folio), "position {position}");
        }
        let issued = wb.settled();
        for range in &issued {
            let position = range.position.unwrap();
            if folio {
                assert!(
                    range.to <= position / FOLIO * FOLIO,
                    "{range:?} reaches into the writer's block"
                );
            } else {
                assert!(range.to <= position / page * page, "{range:?}");
            }
        }
        // Contiguous from the segment's first byte.
        assert_eq!(issued[0].from, 0);
        for pair in issued.windows(2) {
            assert_eq!(pair[0].to, pair[1].from);
        }
        let below = issued.last().unwrap().to;
        assert_eq!(below, limit(3 * STEP + 256 * K - 1, page, folio));
        // A roll: the old segment is written back from where the helper stopped to the end of its seal, which is
        // inside a block; the new one is left while the writer is in its first block.
        let old_end = 3 * STEP + 256 * K + 3_000;
        wb.switch(Arc::clone(&first), old_end, Arc::clone(&second), header);
        let issued = wb.settled();
        let tail = issued.last().unwrap();
        assert_eq!(
            *tail,
            Issued {
                ino: first_ino,
                from: below,
                to: old_end,
                position: None,
            }
        );
        assert!(issued.iter().all(|r| r.ino != second_ino));
        wb.advanced(header + STEP + 64 * K);
        let issued = wb.settled();
        let second: Vec<_> = issued.iter().filter(|r| r.ino == second_ino).collect();
        assert_eq!(second[0].from, 0);
        assert_eq!(
            second.last().unwrap().to,
            limit(header + STEP + 64 * K, page, folio)
        );
    }
}
