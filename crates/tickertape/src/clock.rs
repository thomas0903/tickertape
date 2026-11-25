//! Helpers for caller-supplied timestamps.
//!
//! tickertape never reads a clock for anything it writes into the journal, except for the informational
//! `created_unix_ns` in segment headers. Timestamps in records are data supplied by the caller; sequence numbers are the
//! only authority on order. Monitoring compares a record's `local_ts_ns` with a clock ([`latency::RecordAge`], and the
//! unread age of [`status`](crate::status)); what is read, and in which order, never depends on it.
//!
//! [`latency::RecordAge`]: crate::latency::RecordAge

/// Nanoseconds since the Unix epoch (1970-01-01T00:00:00Z) from the system wall clock.
///
/// This has `CLOCK_REALTIME` semantics: it can jump backwards or forwards (NTP, manual changes), and nanosecond
/// units do not imply nanosecond accuracy. Returns 0 if the clock is before the epoch, which the record format treats
/// as "not set".
pub fn realtime_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn realtime_is_after_2020() {
        assert!(super::realtime_ns() > 1_577_836_800_000_000_000);
    }
}
