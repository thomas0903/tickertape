//! Writer and reader configuration.

use std::time::Duration;

/// What the writer does on open when it finds a corrupt committed record at the end of the newest segment.
///
/// A writer *process* crash never produces this (see `docs/crash-contract.md`, C1/C2); only power loss, storage
/// errors or outside modification do. Uncommitted partial records are always cleaned up, whatever the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum RecoveryPolicy {
    /// Copy the invalid bytes to `quarantine/` in the journal directory, fsync the copy, then zero them and continue
    /// appending after the last valid record. Nothing is ever deleted. The default.
    #[default]
    Quarantine,
    /// Return [`Error::Corrupt`](crate::Error::Corrupt) from [`Writer::open`](crate::Writer::open) and change nothing.
    Fail,
}

/// Writer settings. Construct with [`WriterConfig::default`] and the builder methods.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WriterConfig {
    /// Size of each new segment file in bytes. A multiple of 65,536, from 65,536 to 2^40. Default: 128 MiB.
    /// Existing segments keep the size they were created with.
    pub segment_size: u64,
    /// What to do with a corrupt tail on open. Default: [`RecoveryPolicy::Quarantine`].
    pub recovery: RecoveryPolicy,
}

impl WriterConfig {
    /// Default segment size: 128 MiB.
    pub const DEFAULT_SEGMENT_SIZE: u64 = 128 * 1024 * 1024;

    /// Sets the size of new segment files. Checked by [`Writer::open`](crate::Writer::open).
    pub fn segment_size(mut self, bytes: u64) -> Self {
        self.segment_size = bytes;
        self
    }

    /// Sets the recovery policy.
    pub fn recovery(mut self, policy: RecoveryPolicy) -> Self {
        self.recovery = policy;
        self
    }
}

impl Default for WriterConfig {
    fn default() -> Self {
        WriterConfig {
            segment_size: Self::DEFAULT_SEGMENT_SIZE,
            recovery: RecoveryPolicy::default(),
        }
    }
}

/// How a reader waits for new records in [`Reader::next_timeout`](crate::Reader::next_timeout).
///
/// Each poll that finds nothing moves one step along: `spins` busy polls, then `yields` polls with
/// [`std::thread::yield_now`], then sleeps that start at `min_sleep` and double up to `max_sleep`. Finding a record
/// resets the sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Backoff {
    /// Busy polls before yielding. Default: 100.
    pub spins: u32,
    /// Yielding polls before sleeping. Default: 10.
    pub yields: u32,
    /// First sleep. Default: 10 µs.
    pub min_sleep: Duration,
    /// Longest sleep. Default: 1 ms.
    pub max_sleep: Duration,
}

impl Backoff {
    /// Never sleep or yield: lowest latency, one core at 100 %.
    pub fn busy_spin() -> Self {
        Backoff {
            spins: u32::MAX,
            yields: 0,
            min_sleep: Duration::ZERO,
            max_sleep: Duration::ZERO,
        }
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff {
            spins: 100,
            yields: 10,
            min_sleep: Duration::from_micros(10),
            max_sleep: Duration::from_millis(1),
        }
    }
}

/// Reader settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReaderConfig {
    /// How to wait for new records.
    pub backoff: Backoff,
}

impl ReaderConfig {
    /// Sets the wait strategy.
    pub fn backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }
}
