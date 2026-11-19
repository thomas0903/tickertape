//! Fault injection for tests (cargo feature `fault-injection`, not part of the stable API).
//!
//! A test arms a failure point on its own thread; the next time the writer reaches that point on the same thread,
//! the operation fails with the given OS error instead of running. Readers and inspection have hook points instead,
//! where a test can run an action of its own ([`run_at`]), for example removing files at exactly the moment a
//! concurrent retention run could. Without the feature, the injection and hook points compile to nothing.

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
    /// After a reader lists the segments and before it opens the one it chose (`Reader::open`,
    /// `Reader::open_mapped`, `Reader::seek`). Arm with [`run_at`].
    ReaderOpen,
    /// After a reader at a seal has checked that its segment is still in place and before it opens the successor.
    /// Arm with [`run_at`].
    ReaderSeal,
    /// After inspection (`inspect::list_segments`, `inspect::verify`) lists the segments, before it opens each one.
    /// Arm with [`run_at`].
    InspectOpen,
}

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

/// Disarms every failure point and hook on this thread, and every background failure point.
#[cfg(feature = "fault-injection")]
pub fn clear() {
    armed::ARMED.with(|a| a.borrow_mut().clear());
    armed::HOOKS.with(|h| h.borrow_mut().clear());
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
