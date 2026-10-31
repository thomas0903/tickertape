//! Helpers for caller-supplied timestamps.
//!
//! tickertape never reads a clock on its own behalf, except for the informational `created_unix_ns` in segment
//! headers. Timestamps in records are data supplied by the caller; sequence numbers are the only authority on order.

/// Nanoseconds since the Unix epoch (1970-01-01T00:00:00Z) from the system wall clock.
///
/// This has `CLOCK_REALTIME` semantics: it can jump backwards or forwards (NTP, manual changes), and nanosecond
/// units do not imply nanosecond accuracy. Returns 0 if the clock is before the epoch, which the record format treats
/// as "not set".
pub fn realtime_ns() -> u64 {
    todo!("M2")
}
