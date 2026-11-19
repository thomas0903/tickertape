//! Fault injection for tests (cargo feature `fault-injection`, not part of the stable API).
//!
//! A test arms a failure point on its own thread; the next time the writer reaches that point on the same thread,
//! the operation fails with the given OS error instead of running. A test can also install a pause hook on a thread
//! ([`on_pause`]): it runs whenever that thread reaches a [`Pause`] point in the reader-beacon protocol, so a test can
//! stop one party there and run another party's steps in between, deterministically. Without the feature, the
//! injection and pause points compile to nothing.

use std::io;

/// Places in the writer where a failure can be injected.
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
    /// Renaming a retention segment (inject EXDEV to exercise the copy path).
    RetentionRename,
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

    thread_local! {
        pub(super) static ARMED: RefCell<Vec<(Point, i32)>> = const { RefCell::new(Vec::new()) };
    }
}

/// Makes the next operation at `point` on this thread fail with OS error `errno`.
#[cfg(feature = "fault-injection")]
pub fn fail_next(point: Point, errno: i32) {
    armed::ARMED.with(|a| a.borrow_mut().push((point, errno)));
}

/// Disarms every failure point on this thread, and every background failure point.
#[cfg(feature = "fault-injection")]
pub fn clear() {
    armed::ARMED.with(|a| a.borrow_mut().clear());
    background::ARMED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
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
    use std::sync::Mutex;

    pub(super) static ARMED: Mutex<Vec<(Point, i32)>> = Mutex::new(Vec::new());
}

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
