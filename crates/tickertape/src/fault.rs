//! Fault injection for tests (cargo feature `fault-injection`, not part of the stable API).
//!
//! A test arms a failure point on its own thread; the next time the writer reaches that point on the same thread,
//! the operation fails with the given OS error instead of running. A test can also run code at the moment a record
//! is complete but not yet published ([`before_next_publish`]). Readers and inspection have hook points too, where a
//! test can run an action of its own ([`run_at`]), for example removing files at exactly the moment a concurrent
//! retention run could. A test can also install a pause hook on a thread ([`on_pause`]): it runs whenever that thread
//! reaches a [`Pause`] point in the reader-beacon protocol, so a test can stop one party there and run another party's
//! steps in between, deterministically. A test can hold the writer's background threads at a point
//! ([`hold_background`]) and do things on its own thread meanwhile, such as appending during a background checkpoint.
//! Without the feature, the injection, hook, pause and hold points compile to nothing.

use std::io;

/// Places in the writer where a failure can be injected ([`fail_next`], [`fail_next_background`]), and places in the
/// readers and inspection where an action can run ([`run_at`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Point {
    /// Preallocating a new segment (as if the disk were full).
    Preallocate,
    /// Mapping a new segment (before it is published).
    MapSegment,
    /// Publishing a new segment with `link`.
    Link,
    /// Syncing segment data during `checkpoint()`.
    CheckpointSync,
    /// A roll right after the new segment was linked into place, before the old segment is sealed (and before any
    /// sync of the new segment, which the next checkpoint does). Arm with [`run_at`] on the writer's thread.
    RollAfterLink,
    /// Syncing the checkpoint marker after writing its slot, during `checkpoint()`.
    CheckpointMarkerSync,
    /// A background checkpoint (`Writer::request_checkpoint`) after its data, names and barrier are durable, before it
    /// writes the marker: arm with [`fail_next_background`] to fail it there, or hold it there with
    /// [`hold_background`]. A background checkpoint also stops at [`Point::CheckpointSync`] (before it syncs anything)
    /// and [`Point::CheckpointMarkerSync`], on its own thread.
    CheckpointBeforeMarker,
    /// A background checkpoint has opened a segment its list had released again (ADR 0038 → Bounded open segments),
    /// before it syncs it: hold it there with [`hold_background`].
    CheckpointReopen,
    /// Renaming a retention segment (inject EXDEV to exercise the copy path).
    RetentionRename,
    /// Syncing the checkpoint marker in `retention::apply` (an archiver other than the writer), after reading the slot
    /// that decides a removal and before removing anything.
    RetentionMarkerSync,
    /// Syncing an archive copy before removing its source: arm with [`fail_next`], or with [`run_at`] to act after the
    /// copy is written and before it is synced.
    RetentionCopySync,
    /// Syncing the destination directory after an archive rename or copy.
    RetentionDestinationSync,
    /// Before each planned segment of a retention run, before anything about it is checked: on the writer's checkpoint
    /// thread once the run has decided not to stop there (ADR 0044; hold it there with [`hold_background`]), and in any
    /// other run (arm with [`run_at`]).
    RetentionRemove,
    /// Between the checks of a planned segment and its removal, in any retention run. Arm with [`run_at`].
    RetentionChecked,
    /// The rename that takes a segment out of the journal under a temporary name, in a removal by an archiver other
    /// than the writer (issue #83): arm with [`fail_next`] (`EINVAL` exercises the fallback for a file system without
    /// a no-replace rename).
    RetentionTakeOut,
    /// An archiver other than the writer has taken a segment out of the journal (renamed it to its temporary name, or
    /// into the destination), before it checks that the file is the one it checked and that the marker still names a
    /// later segment. Arm with [`run_at`].
    RetentionTakenOut,
    /// Those checks passed, before the temporary name is unlinked (or the move made durable). Arm with [`run_at`].
    RetentionVerified,
    /// Those checks failed, before the segment is renamed back into the journal: arm with [`run_at`], or with
    /// [`fail_next`] to make the no-replace rename fail (`EINVAL` exercises the fallback with `link`).
    RetentionPutBack,
    /// Unlinking the temporary name after putting a segment back with `link` (no no-replace rename): arm with
    /// [`fail_next`].
    RetentionPutBackUnlink,
    /// `Writer::checkpoint` or `Writer::close` has told the checkpoint thread that it is waiting for it (a retention
    /// run there then stops after the removal in progress), and is about to wait. Arm with [`run_at`] on the thread
    /// that calls them.
    CheckpointWait,
    /// Preallocating the spare next segment on its background thread (arm with [`fail_next_background`],
    /// or stop it there with [`hold_background`]).
    SparePreallocate,
    /// Mapping the spare next segment on its background thread, after its blocks are allocated (arm with
    /// [`fail_next_background`], or stop it there with [`hold_background`]).
    SpareMap,
    /// Panics the thread that prepares the spare next segment, after its blocks are allocated (arm with
    /// [`fail_next_background`]; the error is not used).
    SparePanic,
    /// Writing the spare's header when the writer publishes it at roll (arm with [`fail_next`]).
    SpareHeader,
    /// Recovery repairing a corrupt tail, after the tail was preserved in `quarantine/` and the repaired copy written,
    /// before that copy is renamed over the segment (stands in for a crash there).
    RepairBeforeRename,
    /// Recovery repairing a corrupt tail, after the repaired copy replaced the segment, before later segments are
    /// handled (stands in for a crash there).
    RepairAfterRename,
    /// Recovery replacing an all-zero segment file of invalid length (a header that never reached the disk) by a new
    /// empty segment, after the new one was prepared and synced under a temporary name, before it is renamed over the
    /// old file (stands in for a crash there).
    RestoreBeforeRename,
    /// Recovery under `RecoveryPolicy::Fail` making the changes it held back until the open could no longer refuse
    /// (crash contract C11), after one of them and before the next (stands in for a crash there).
    BetweenHeldChanges,
    /// After a reader lists the segments and before it opens the one it chose (`Reader::open`,
    /// `Reader::open_mapped`, `Reader::seek`, `Reader::seek_to_end`). Arm with [`run_at`].
    ReaderOpen,
    /// After a reader at a seal has checked that its segment is still in place and before it opens the successor.
    /// Arm with [`run_at`].
    ReaderSeal,
    /// After a reader at a seal found the successor missing while its segment is still in place, before it reads the
    /// slot at the cursor again from the file (the seal may have come from bytes an in-place repair has since
    /// zeroed). Arm with [`run_at`].
    ReaderSealReread,
    /// In `Reader::seek`, after it has read the checkpoint marker that later seeks may rely on, before it checks that
    /// its segment is still in place. Arm with [`run_at`].
    ReaderSeekCoverage,
    /// `inspect::read_checkpoint` found no valid slot in an existing marker, before it reads the slots once more. Arm
    /// with [`run_at`].
    CheckpointReread,
    /// Before a durable-only reader reads the checkpoint marker. Arm with [`run_at`].
    ReaderCheckpoint,
    /// After a durable-only reader has read a checkpoint marker that moved past its cursor, before it checks that its
    /// segment is still in place. Arm with [`run_at`].
    ReaderCheckpointIdentity,
    /// Before a visible zero-copy reader reads the checkpoint marker for the bound on its loads (ADR 0048). Arm with
    /// [`run_at`], or with [`fail_next`] to make that read fail.
    ReaderLimitMarker,
    /// After a visible zero-copy reader that has reached its bound has read the marker again, before it checks that its
    /// segment is still in place. Arm with [`run_at`], or with [`fail_next`] to make that check fail.
    ReaderLimitIdentity,
    /// A reader has read an empty slot, before it checks whether that slot can be the head of the journal (ADR 0050), that
    /// is before it reads the checkpoint marker for it. Arm with [`run_at`].
    ReaderEmptySlotCheck,
    /// A reader at an empty slot is about to list the segments. Arm with [`run_at`].
    ReaderEmptySlotList,
    /// A reader at an empty slot has found that the journal continues after it (ADR 0050), before it reads the slot again.
    /// Arm with [`run_at`].
    ReaderEmptySlot,
    /// A reader at an empty slot has found that again, after it read the slot again for the validation window and before
    /// it checks that its segment is still in place and reads the slot for the last time. Arm with [`run_at`].
    ReaderEmptySlotConfirm,
    /// After inspection (`inspect::list_segments`, `inspect::verify`) lists the segments, before it opens each one.
    /// Arm with [`run_at`].
    InspectOpen,
    /// After `inspect::verify` finds the newest listed segment sealed, before it looks for the successor. Arm with
    /// [`run_at`].
    InspectSeal,
    /// In `inspect::verify`, after a scan found a segment in a state a running writer passes through and verify has
    /// paused, before it scans that segment again. That includes the pauses of the scans that follow a live writer
    /// past an empty commit word at the end of the newest segment, and the one before verify reads that commit word
    /// again (ADR 0035). Arm with [`run_at`].
    InspectRescan,
    /// Syncing the zeroing of an abandoned write (a fill closure that panicked or declined).
    AbandonedWriteSync,
    /// Starting the early write-back of segment data on the write-back helper thread (Linux; arm with
    /// [`fail_next_background`], or hold the helper there, with the segment file it is about to write back, with
    /// [`hold_background`]).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    EarlyWriteback,
    /// The prefault helper has taken a segment the writer released (ADR 0059) and is about to drop its file and mapping
    /// handles (`close` and `munmap`): hold it there, with the segment in hand, with [`hold_background`].
    SegmentRelease,
    /// Panics the prefault helper there, with the segment in hand (arm with [`fail_next_background`]; the error is not
    /// used).
    SegmentReleasePanic,
    /// A seek index builder has created its temporary file (the fence), before it reads the marker. The points of the
    /// seek index (ADR 0045) are hooks ([`run_at`]) unless they say otherwise.
    SeekIndexFence,
    /// A seek index builder has read the marker (or probed the lock) and found the segment eligible, before it opens
    /// and walks it.
    SeekIndexEligible,
    /// Writing the temporary index: arm with [`fail_next`].
    SeekIndexWrite,
    /// A seek index builder has written the temporary index, before it renames it.
    SeekIndexBeforeRename,
    /// A seek index builder has renamed the index into place, before it checks that the segment is still in place.
    SeekIndexAfterRename,
    /// A seek index builder holds the writer's lock shared, before it releases it (a directory without a marker).
    SeekIndexProbe,
    /// Listing `seek-index/` (by the writer's open, `seek_index::remove`): arm with [`fail_next`].
    SeekIndexList,
    /// Removing a name in `seek-index/`: arm with [`fail_next`].
    SeekIndexUnlink,
    /// Reading on in a listing of `seek-index/` after a name with a meaning: arm with [`fail_next`].
    SeekIndexListEntry,
    /// Syncing `seek-index/` after the removal at the writer's open: arm with [`fail_next`].
    SeekIndexSync,
    /// The platform barrier on `seek-index/` at the end of an open that repaired nothing: arm with [`fail_next`].
    SeekIndexBarrier,
    /// Giving a new `seek-index/` its group and mode: arm with [`fail_next`].
    SeekIndexDirMode,
    /// Reading the checkpoint marker for retention or a seek index builder, before its segment is opened.
    MarkerRead,
    /// The writer's open has finished one of its two passes over `seek-index/` (recovery step 2a).
    SeekIndexListed,
    /// The writer's open has finished step 2a, before recovery changes anything.
    SeekIndexRemoved,
    /// A seek that may use an index has read the checkpoint marker, or did not have to (its step 2).
    ReaderSeekIndexMarker,
    /// A seek has read and checked an index (its step 4), before it checks that its segment is still in place.
    ReaderSeekIndexLoaded,
    /// A seek has found the indexed record's first 8 bytes in place (its step 8a), before it reads the record.
    ReaderSeekIndexLanding,
}

/// Points in the reader-beacon protocol (ADR 0032 → "The beacon protocol") where a test's pause hook runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Pause {
    /// A creator found the beacon directory's lock held by another creator and is about to wait for it.
    CreatorWaitsForDirectoryLock,
    /// A creator holds the directory lock and has written its locked temporary beacon, before publishing it.
    CreatorWroteTemporary,
    /// A creator found the name taken and opened the file there, before checking it.
    CreatorOpenedExisting,
    /// A creator found the file at the name to be a stale beacon, before replacing it.
    CreatorFoundStale,
    /// A dropping beacon confirmed that its path still names its file, before unlinking it.
    OwnerBeforeUnlink,
    /// An observer opened a beacon file, before reading it.
    ObserverOpened,
    /// An observer read a beacon's contents, before probing its lock.
    ObserverRead,
    /// An observer holds its shared probe lock on a beacon, before releasing it.
    ObserverHoldsProbe,
}

#[cfg(feature = "fault-injection")]
mod paused {
    use super::Pause;
    use std::cell::RefCell;

    type Hook = Box<dyn FnMut(Pause)>;

    thread_local! {
        pub(super) static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }
}

/// Installs (or, with `None`, removes) this thread's pause hook. It is called with each [`Pause`] point this thread
/// reaches until it is removed.
#[cfg(feature = "fault-injection")]
pub fn on_pause(hook: Option<Box<dyn FnMut(Pause)>>) {
    // The old hook is dropped after the borrow ends: dropping what it owns (a beacon, say) may reach a pause point.
    let old = paused::HOOK.with(|h| std::mem::replace(&mut *h.borrow_mut(), hook));
    drop(old);
}

/// Called by the beacon protocol at each pause point.
#[cfg(feature = "fault-injection")]
pub(crate) fn pause(point: Pause) {
    // Take the hook out while it runs, so that a hook may itself reach pause points without re-entering.
    let hook = paused::HOOK.with(|h| h.borrow_mut().take());
    if let Some(mut hook) = hook {
        hook(point);
        paused::HOOK.with(|h| {
            let mut slot = h.borrow_mut();
            if slot.is_none() {
                *slot = Some(hook);
            }
        });
    }
}

/// Called by the beacon protocol at each pause point (no-op without the `fault-injection` feature).
#[cfg(not(feature = "fault-injection"))]
#[inline(always)]
pub(crate) fn pause(_point: Pause) {}

#[cfg(feature = "fault-injection")]
mod armed {
    use super::Point;
    use std::cell::RefCell;

    /// An action armed with [`super::run_at`]: the point, how many passes to let by first, and the action.
    pub(super) type Hook = (Point, usize, Box<dyn FnOnce()>);

    thread_local! {
        pub(super) static ARMED: RefCell<Vec<(Point, i32)>> = const { RefCell::new(Vec::new()) };
        pub(super) static HOOKS: RefCell<Vec<Hook>> = const { RefCell::new(Vec::new()) };
    }
}

/// Runs `action` on this thread when a reader or inspection reaches hook point `point` on this thread, after letting
/// `skip` earlier passes through that point go by. Each armed action runs once.
#[cfg(feature = "fault-injection")]
pub fn run_at(point: Point, skip: usize, action: impl FnOnce() + 'static) {
    armed::HOOKS.with(|h| h.borrow_mut().push((point, skip, Box::new(action))));
}

/// Called by readers and inspection at each hook point.
#[cfg(feature = "fault-injection")]
pub(crate) fn hook(point: Point) {
    let action = armed::HOOKS.with(|h| {
        let mut h = h.borrow_mut();
        let i = h.iter().position(|(p, _, _)| *p == point)?;
        if h[i].1 > 0 {
            h[i].1 -= 1;
            return None;
        }
        Some(h.remove(i).2)
    });
    // Run outside the borrow, so the action may arm further hooks.
    if let Some(action) = action {
        action();
    }
}

/// Called by readers and inspection at each hook point (no-op without the `fault-injection` feature).
#[cfg(not(feature = "fault-injection"))]
#[inline(always)]
pub(crate) fn hook(_point: Point) {}

#[cfg(feature = "fault-injection")]
mod naming {
    use std::cell::Cell;

    thread_local! {
        pub(super) static NS: Cell<Option<u64>> = const { Cell::new(None) };
    }
}

/// Makes this thread's retention removals name their temporary files with this time instead of the clock's (`None`:
/// the clock again), so that a test can put a file at that name first.
#[cfg(feature = "fault-injection")]
pub fn temporary_names_at(unix_ns: Option<u64>) {
    naming::NS.with(|n| n.set(unix_ns));
}

/// The time [`temporary_names_at`] set on this thread.
#[cfg(feature = "fault-injection")]
pub(crate) fn temporary_name_ns() -> Option<u64> {
    naming::NS.with(|n| n.get())
}

/// The time [`temporary_names_at`] set (always `None` without the `fault-injection` feature).
#[cfg(not(feature = "fault-injection"))]
#[inline(always)]
pub(crate) fn temporary_name_ns() -> Option<u64> {
    None
}

/// Makes the next operation at `point` on this thread fail with OS error `errno`.
#[cfg(feature = "fault-injection")]
pub fn fail_next(point: Point, errno: i32) {
    armed::ARMED.with(|a| a.borrow_mut().push((point, errno)));
}

/// Disarms every failure point and hook on this thread (the publish hook too), and every background failure point
/// (holds stay until their [`Hold`] is dropped).
#[cfg(feature = "fault-injection")]
pub fn clear() {
    armed::ARMED.with(|a| a.borrow_mut().clear());
    armed::HOOKS.with(|h| h.borrow_mut().clear());
    background::ARMED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    publish::HOOK.with(|h| *h.borrow_mut() = None);
    listing::OMIT.with(|o| o.borrow_mut().clear());
    naming::NS.with(|n| n.set(None));
}

/// Makes the next operation at `point` on any of the writer's background threads fail with OS error `errno`.
#[cfg(feature = "fault-injection")]
pub fn fail_next_background(point: Point, errno: i32) {
    background::ARMED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((point, errno));
}

#[cfg(feature = "fault-injection")]
mod background {
    use super::Point;
    use std::sync::{Arc, Condvar, Mutex};

    pub(super) static ARMED: Mutex<Vec<(Point, i32)>> = Mutex::new(Vec::new());
    pub(super) static HOLDS: Mutex<Vec<(Point, Arc<HoldState>)>> = Mutex::new(Vec::new());

    #[derive(Debug, Default)]
    pub(super) struct HoldState {
        /// (reached, released)
        pub(super) flags: Mutex<(bool, bool)>,
        pub(super) changed: Condvar,
    }
}

/// A background thread held at a point ([`hold_background`]). Dropping it releases the thread, as
/// [`Hold::release`] does, and disarms the point if no thread reached it.
#[cfg(feature = "fault-injection")]
#[derive(Debug)]
pub struct Hold {
    point: Point,
    state: std::sync::Arc<background::HoldState>,
}

/// Makes the next background thread that reaches `point` wait there until the returned [`Hold`] is released or
/// dropped. Like [`fail_next_background`], this applies to the writer's background threads in the whole process.
#[cfg(feature = "fault-injection")]
pub fn hold_background(point: Point) -> Hold {
    let state = std::sync::Arc::new(background::HoldState::default());
    background::HOLDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((point, std::sync::Arc::clone(&state)));
    Hold { point, state }
}

#[cfg(feature = "fault-injection")]
impl Hold {
    /// Waits until a background thread has reached the point; false if `timeout` passed first.
    pub fn wait_reached(&self, timeout: std::time::Duration) -> bool {
        let flags = self.state.flags.lock().unwrap_or_else(|e| e.into_inner());
        let (flags, _) = self
            .state
            .changed
            .wait_timeout_while(flags, timeout, |f| !f.0)
            .unwrap_or_else(|e| e.into_inner());
        flags.0
    }

    /// Whether a background thread has reached the point.
    pub fn reached(&self) -> bool {
        self.state.flags.lock().unwrap_or_else(|e| e.into_inner()).0
    }

    /// Lets the held thread continue.
    pub fn release(self) {
        drop(self);
    }
}

#[cfg(feature = "fault-injection")]
impl Drop for Hold {
    fn drop(&mut self) {
        background::HOLDS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(p, s)| !(*p == self.point && std::sync::Arc::ptr_eq(s, &self.state)));
        self.state.flags.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
        self.state.changed.notify_all();
    }
}

/// Called on the writer's background threads at each point a test can hold ([`hold_background`]).
#[cfg(feature = "fault-injection")]
pub(crate) fn hold_point(point: Point) {
    let state = {
        let mut holds = background::HOLDS.lock().unwrap_or_else(|e| e.into_inner());
        match holds.iter().position(|(p, _)| *p == point) {
            Some(i) => holds.remove(i).1,
            None => return,
        }
    };
    let mut flags = state.flags.lock().unwrap_or_else(|e| e.into_inner());
    flags.0 = true;
    state.changed.notify_all();
    while !flags.1 {
        flags = state.changed.wait(flags).unwrap_or_else(|e| e.into_inner());
    }
}

/// Called on background threads at each hold point (no-op without the `fault-injection` feature).
#[cfg(not(feature = "fault-injection"))]
#[inline(always)]
pub(crate) fn hold_point(_point: Point) {}

/// Called on the writer's background threads at each failure point armed with [`fail_next_background`].
#[cfg(feature = "fault-injection")]
pub(crate) fn inject_background(point: Point) -> io::Result<()> {
    let mut armed = background::ARMED.lock().unwrap_or_else(|e| e.into_inner());
    match armed.iter().position(|(p, _)| *p == point) {
        Some(i) => Err(io::Error::from_raw_os_error(armed.remove(i).1)),
        None => Ok(()),
    }
}

/// Called on background threads (no-op without the `fault-injection` feature).
#[cfg(not(feature = "fault-injection"))]
#[inline(always)]
pub(crate) fn inject_background(_point: Point) -> io::Result<()> {
    Ok(())
}

/// Called by the writer at each failure point.
#[cfg(feature = "fault-injection")]
pub(crate) fn inject(point: Point) -> io::Result<()> {
    armed::ARMED.with(|a| {
        let mut a = a.borrow_mut();
        match a.iter().position(|(p, _)| *p == point) {
            Some(i) => Err(io::Error::from_raw_os_error(a.remove(i).1)),
            None => Ok(()),
        }
    })
}

/// Called by the writer at each failure point (no-op without the `fault-injection` feature).
#[cfg(not(feature = "fault-injection"))]
#[inline(always)]
pub(crate) fn inject(_point: Point) -> io::Result<()> {
    Ok(())
}

#[cfg(feature = "fault-injection")]
mod publish {
    use std::cell::RefCell;

    thread_local! {
        pub(super) static HOOK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    }
}

/// Runs `hook` on this thread the next time the writer has written a whole record (header, payload and CRC) but not
/// yet stored its commit word: the moment a reader must still see an empty slot.
#[cfg(feature = "fault-injection")]
pub fn before_next_publish(hook: impl FnOnce() + 'static) {
    publish::HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
}

/// Called by the writer just before it stores a record's commit word.
#[cfg(feature = "fault-injection")]
pub(crate) fn pause_before_publish() {
    if let Some(hook) = publish::HOOK.with(|h| h.borrow_mut().take()) {
        hook();
    }
}

/// Called by the writer just before it stores a record's commit word (no-op without the `fault-injection` feature).
#[cfg(not(feature = "fault-injection"))]
#[inline(always)]
pub(crate) fn pause_before_publish() {}

#[cfg(feature = "fault-injection")]
mod listing {
    use std::cell::RefCell;
    use std::ffi::OsString;

    thread_local! {
        pub(super) static OMIT: RefCell<Vec<Vec<OsString>>> = const { RefCell::new(Vec::new()) };
    }
}

/// Makes this thread's next listing of `seek-index/` leave out these names, as a listing may leave out both names of
/// a rename that happens while it runs. Calls queue up, one per listing.
#[cfg(feature = "fault-injection")]
pub fn next_listing_omits(names: impl IntoIterator<Item = std::ffi::OsString>) {
    listing::OMIT.with(|o| o.borrow_mut().push(names.into_iter().collect()));
}

/// Called on each listing of `seek-index/`.
#[cfg(feature = "fault-injection")]
pub(crate) fn filter_listing<T>(entries: &mut Vec<T>, name: impl Fn(&T) -> &std::ffi::OsStr) {
    let omit = listing::OMIT.with(|o| {
        let mut o = o.borrow_mut();
        (!o.is_empty()).then(|| o.remove(0))
    });
    if let Some(omit) = omit {
        entries.retain(|e| !omit.iter().any(|n| n == name(e)));
    }
}

/// Called on each listing of `seek-index/` (no-op without the `fault-injection` feature).
#[cfg(not(feature = "fault-injection"))]
#[inline(always)]
pub(crate) fn filter_listing<T>(_entries: &mut Vec<T>, _name: impl Fn(&T) -> &std::ffi::OsStr) {}

/// A sync that a thread made ([`take_syncs`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Sync {
    /// The data of a file (`fdatasync`, `fsync` on macOS).
    Data,
    /// A file or directory (`fsync`).
    All,
    /// The platform barrier (`F_FULLFSYNC` on macOS; on Linux it is a [`Sync::Data`] as well).
    Barrier,
    /// Data through a mapping made for it (`msync`; a [`Sync::Msync`] as well).
    MsyncFile,
    /// The start of early write-back (`sync_file_range`).
    Writeback,
    /// Part of a mapping (`msync`). Its file is not known here: recorded with device and inode 0.
    Msync,
}

#[cfg(feature = "fault-injection")]
mod syncs {
    use std::cell::RefCell;

    /// A sync and the device and inode of the file it was made on.
    pub(super) type Made = (super::Sync, (u64, u64));

    thread_local! {
        pub(super) static LOG: RefCell<Option<Vec<Made>>> = const { RefCell::new(None) };
    }
}

/// Starts recording this thread's syncs, forgetting the ones recorded before.
#[cfg(feature = "fault-injection")]
pub fn record_syncs() {
    syncs::LOG.with(|l| *l.borrow_mut() = Some(Vec::new()));
}

/// Stops recording this thread's syncs and returns them, each with the device and inode of the file it was made on.
#[cfg(feature = "fault-injection")]
pub fn take_syncs() -> Vec<(Sync, (u64, u64))> {
    syncs::LOG
        .with(|l| l.borrow_mut().take())
        .unwrap_or_default()
}

/// Called at each sync of a file.
#[cfg(feature = "fault-injection")]
pub(crate) fn note_sync(sync: Sync, file: &std::fs::File) {
    use std::os::unix::fs::MetadataExt;
    syncs::LOG.with(|l| {
        if let Some(log) = l.borrow_mut().as_mut() {
            if let Ok(meta) = file.metadata() {
                log.push((sync, (meta.dev(), meta.ino())));
            }
        }
    });
}

/// Called at each sync of a file (no-op without the `fault-injection` feature).
#[cfg(not(feature = "fault-injection"))]
#[inline(always)]
pub(crate) fn note_sync(_sync: Sync, _file: &std::fs::File) {}

/// Called at each `msync`.
#[cfg(feature = "fault-injection")]
pub(crate) fn note_msync() {
    syncs::LOG.with(|l| {
        if let Some(log) = l.borrow_mut().as_mut() {
            log.push((Sync::Msync, (0, 0)));
        }
    });
}

/// Called at each `msync` (no-op without the `fault-injection` feature).
#[cfg(not(feature = "fault-injection"))]
#[inline(always)]
pub(crate) fn note_msync() {}

/// What a thread did to find out about the journal's segments ([`take_accesses`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Access {
    /// Listed the journal directory.
    ListJournal,
    /// Retention opened a segment file.
    OpenSegment,
    /// Retention asked for a segment file's size or whether it is there, without opening it.
    StatSegment,
}

#[cfg(feature = "fault-injection")]
mod accesses {
    use std::cell::RefCell;

    thread_local! {
        pub(super) static LOG: RefCell<Option<Vec<super::Access>>> = const { RefCell::new(None) };
    }
}

/// Starts recording this thread's accesses, forgetting the ones recorded before.
#[cfg(feature = "fault-injection")]
pub fn record_accesses() {
    accesses::LOG.with(|l| *l.borrow_mut() = Some(Vec::new()));
}

/// Stops recording this thread's accesses and returns them.
#[cfg(feature = "fault-injection")]
pub fn take_accesses() -> Vec<Access> {
    accesses::LOG
        .with(|l| l.borrow_mut().take())
        .unwrap_or_default()
}

/// Called at each access.
#[cfg(feature = "fault-injection")]
pub(crate) fn note_access(access: Access) {
    accesses::LOG.with(|l| {
        if let Some(log) = l.borrow_mut().as_mut() {
            log.push(access);
        }
    });
}

/// Called at each access (no-op without the `fault-injection` feature).
#[cfg(not(feature = "fault-injection"))]
#[inline(always)]
pub(crate) fn note_access(_access: Access) {}
