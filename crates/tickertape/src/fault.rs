//! Fault injection for tests (cargo feature `fault-injection`, not part of the stable API).
//!
//! A test arms a failure point on its own thread; the next time the writer reaches that point on the same thread,
//! the operation fails with the given OS error instead of running. Without the feature, the injection points compile to
//! nothing.

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
