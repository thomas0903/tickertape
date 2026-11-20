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
    /// Renaming a retention segment (inject EXDEV to exercise the copy path).
    RetentionRename,
    /// Syncing the checkpoint marker in `retention::apply` (an archiver other than the writer), after reading the slot
    /// that decides a removal and before removing anything.
    RetentionMarkerSync,
    /// Syncing an archive copy before removing its source.
    RetentionCopySync,
    /// Syncing the destination directory after an archive rename or copy.
    RetentionDestinationSync,
    /// Preallocating the spare next segment on its background thread (arm with [`fail_next_background`]).
    SparePreallocate,
    /// Mapping the spare next segment on its background thread (arm with [`fail_next_background`]).
    SpareMap,
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
    /// After a reader lists the segments and before it opens the one it chose (`Reader::open`,
    /// `Reader::open_mapped`, `Reader::seek`). Arm with [`run_at`].
    ReaderOpen,
    /// After a reader at a seal has checked that its segment is still in place and before it opens the successor.
    /// Arm with [`run_at`].
    ReaderSeal,
    /// After a reader at a seal found the successor missing while its segment is still in place, before it reads the
    /// slot at the cursor again from the file (the seal may have come from bytes an in-place repair has since
    /// zeroed). Arm with [`run_at`].
    ReaderSealReread,
    /// Before a durable-only reader reads the checkpoint marker. Arm with [`run_at`].
    ReaderCheckpoint,
    /// After a durable-only reader has read a checkpoint marker that moved past its cursor, before it checks that its
    /// segment is still in place. Arm with [`run_at`].
    ReaderCheckpointIdentity,
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
    /// [`fail_next_background`]).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    EarlyWriteback,
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
