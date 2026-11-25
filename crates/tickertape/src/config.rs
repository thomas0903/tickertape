//! Writer and reader configuration.

use std::time::Duration;

/// What the writer does on open when it finds corruption after the last checkpoint: an invalid record or seal, a
/// segment disconnected from the chain, an empty commit word after which recovery finds evidence of committed
/// history ([`CorruptReason::EmptyCommitWord`](crate::CorruptReason::EmptyCommitWord), or
/// [`CorruptReason::UncheckedTail`](crate::CorruptReason::UncheckedTail) when its search stops at its work bound), or
/// non-zero bytes after an empty commit word in front of a segment for the expected sequence number that exists but
/// does not open ([`CorruptReason::BrokenChain`](crate::CorruptReason::BrokenChain); `docs/crash-contract.md`, C11,
/// and ADR 0035 have the rules).
///
/// A writer *process* crash in an otherwise intact journal usually does not produce this; power loss, storage errors
/// or outside modification do. The exception (`docs/crash-contract.md`, C1): the payload of the record being written
/// can hold bytes that recovery's search counts as committed history (raw frames copied from another journal, for
/// example). Recovery cannot tell those bytes from damage and reports
/// [`CorruptReason::EmptyCommitWord`](crate::CorruptReason::EmptyCommitWord) or
/// [`CorruptReason::UncheckedTail`](crate::CorruptReason::UncheckedTail); `Quarantine` preserves them and `Fail`
/// refuses. Nothing is lost (ADR 0035).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum RecoveryPolicy {
    /// Copy the corrupt tail (from the corrupt position to the last non-zero byte of its segment) to `quarantine/`
    /// in the journal directory, fsync the copy, then replace the segment by a copy with the tail zeroed (never
    /// edited in place), move disconnected segments into `quarantine/`, and continue appending after the last valid
    /// record. No segment and no repaired tail is deleted. Some bytes are zeroed in place without a copy, as after a
    /// crash (`docs/crash-contract.md`, C8). The default.
    #[default]
    Quarantine,
    /// Return [`Error::Corrupt`](crate::Error::Corrupt) from [`Writer::open`](crate::Writer::open) instead of
    /// quarantining or repairing anything. The bytes at the corrupt position it reports, and everything after them in
    /// that segment, are left untouched, and no segment is moved. The open is not read-only, though: before it scans
    /// it may create the lock file and the checkpoint file, writes its PID into the lock file (best
    /// effort) and removes stale temporary files, and it keeps what recovery changed earlier in the scan, including
    /// bytes zeroed in place, which after storage damage can be published records (`docs/crash-contract.md`, C11,
    /// lists these changes).
    ///
    /// After a plain process crash this usually opens; it refuses in the exception above (`EmptyCommitWord` or
    /// `UncheckedTail` at the start of the record being written). The bytes alone cannot tell that from storage
    /// damage. If the application knows it was writing such a payload when the process died, open once with
    /// [`RecoveryPolicy::Quarantine`], which preserves those bytes in `quarantine/` and continues after the last
    /// committed record (`docs/operations.md` → After a crash).
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
    /// Create a shared-memory wake-up region, named in the journal's `notify` record, so that blocked readers wake up
    /// as soon as a record is published (ADRs 0017 and 0021), or, for
    /// [`Writer::append_batch`](crate::Writer::append_batch), once the batch's last record is, plus at each roll inside
    /// the batch (ADR 0041).
    /// Costs one atomic increment and one load per append (per batch for `append_batch`), plus a wake-up system call
    /// when readers are blocked.
    /// Default: true.
    pub notify: bool,
    /// Do segment work in a background thread, off the writer's thread. That thread never writes records.
    /// - **Linux:** a helper thread (`tickertape-prefault`) faults in the 256 KiB ahead of the write position with
    ///   `MADV_POPULATE_WRITE`, so `append` rarely takes a page fault (ADR 0025). On kernels before 5.14 the helper
    ///   stops on first use.
    /// - **macOS:** the helper keeps 8 MiB ahead with `madvise(MADV_WILLNEED)` (ADR 0020).
    ///
    /// It is also the switch for [`WriterConfig::early_writeback`] and [`WriterConfig::spare_segment`], which have no
    /// effect without it.
    ///
    /// Set to `false` for a writer that starts no threads, except the checkpoint thread that
    /// [`Writer::request_checkpoint`](crate::Writer::request_checkpoint) starts; that also turns off
    /// [`WriterConfig::early_writeback`] and [`WriterConfig::spare_segment`]. Default: true.
    pub prefault: bool,
    /// **Linux:** start the kernel's write-back of the pages the writer has finished with, as it goes, so that a
    /// checkpoint finds little left to write (ADR 0038 → Early write-back). Applies only with
    /// [`WriterConfig::prefault`] on, which is the switch for the writer's helper threads. A helper thread
    /// (`tickertape-writeback`)
    /// calls `sync_file_range(SYNC_FILE_RANGE_WRITE)` over the whole pages behind the write position each time it
    /// has moved 1 MiB, and over the rest of a segment when the writer rolls past it. That call only starts writing;
    /// it makes nothing durable: what is durable when is unchanged, and only a checkpoint promises it
    /// (`docs/crash-contract.md`). If the call fails with an I/O error, no later checkpoint of this writer succeeds
    /// (the error is returned by the next checkpoint, as a failed sync would be); if the filesystem does not support
    /// it, the helper stops. **macOS:** no effect (it has no call that starts write-back of a range without waiting
    /// for it). Default: true.
    pub early_writeback: bool,
    /// Prepare the next segment in a background thread (`tickertape-spare`) once the active one is half full, so that a
    /// roll writes its header and links it instead of creating and preallocating a file (ADR 0022; on Linux ADR 0049).
    /// The thread preallocates `next-segment.tmp` in the journal directory, which reserves one more segment's worth of
    /// disk space, and holds one more descriptor and one more segment mapping, until the roll. Applies only with
    /// [`WriterConfig::prefault`] on. From 1.1. Default: on on macOS, **off on Linux**; no effect elsewhere.
    ///
    /// - **macOS:** a thread is started for each spare and syncs it, and a roll waits for one still being prepared.
    /// - **Linux:** one thread lives as long as the writer, started in [`Writer::open`](crate::Writer::open), and does
    ///   not sync the spare: the checkpoint that first covers the segment syncs it, as it does a segment a roll creates
    ///   (ADR 0038). A roll that finds the spare not ready creates the segment itself, without waiting, and the spare
    ///   is the next roll's; only if creating it then fails for lack of space (or over a quota) does the roll wait for
    ///   the spare, which may have taken that space, and use it, or, if the spare came to nothing, try once more. That
    ///   wait has no time limit: it lasts as long as a `fallocate` that the filesystem holds up. Dropping the writer
    ///   waits for a spare being prepared in the same way. If the thread panics, that writer prepares no more spares.
    ///   Whether it helps depends on the load and the disk: measure before turning it on (ADR 0049 and
    ///   `docs/benchmarks.md` say what is and is not known, including cases where it was worse).
    pub spare_segment: bool,
    /// Where the writer's helper threads run: the checkpoint thread (`tickertape-checkpoint`), the prefault and
    /// write-back helpers (`tickertape-prefault`, `tickertape-writeback`) and the spare-segment thread
    /// (`tickertape-spare`). See [`HelperCpus`]. Default: [`HelperCpus::Process`].
    pub helper_cpus: HelperCpus,
    /// Which latency histograms the writer keeps of its own calls ([`Writer::latency`](crate::Writer::latency), ADR
    /// 0047). See [`LatencyMode`]. From 1.1. Default: [`LatencyMode::Off`].
    pub latency: LatencyMode,
    /// Optional retention policy and action, run after open and successful checkpoints. Default: none.
    pub retention: Option<(crate::retention::Policy, crate::retention::Action)>,
}

impl WriterConfig {
    /// Default segment size: 128 MiB.
    pub const DEFAULT_SEGMENT_SIZE: u64 = 128 * 1024 * 1024;

    /// Sets the size of new segment files. Checked by [`Writer::open`](crate::Writer::open).
    pub fn segment_size(mut self, bytes: u64) -> Self {
        self.segment_size = bytes;
        self
    }

    /// Enables retention after open and every successful checkpoint.
    /// Failures are recorded by [`Writer::last_retention_error`](crate::Writer::last_retention_error),
    /// without failing open, appends or checkpoints. The archive directory must already exist.
    pub fn retention(
        mut self,
        policy: crate::retention::Policy,
        action: crate::retention::Action,
    ) -> Self {
        self.retention = Some((policy, action));
        self
    }

    /// Sets the recovery policy.
    pub fn recovery(mut self, policy: RecoveryPolicy) -> Self {
        self.recovery = policy;
        self
    }

    /// Sets whether to wake blocked readers (see [`WriterConfig::notify`]).
    pub fn notify(mut self, on: bool) -> Self {
        self.notify = on;
        self
    }

    /// Sets whether to do segment work in a background thread (see [`WriterConfig::prefault`]).
    pub fn prefault(mut self, on: bool) -> Self {
        self.prefault = on;
        self
    }

    /// Sets whether to start write-back early on Linux (see [`WriterConfig::early_writeback`]).
    pub fn early_writeback(mut self, on: bool) -> Self {
        self.early_writeback = on;
        self
    }

    /// Sets whether to prepare the next segment in a background thread (see [`WriterConfig::spare_segment`]; from 1.1).
    pub fn spare_segment(mut self, on: bool) -> Self {
        self.spare_segment = on;
        self
    }

    /// Sets where the writer's helper threads run (see [`HelperCpus`]).
    pub fn helper_cpus(mut self, cpus: HelperCpus) -> Self {
        self.helper_cpus = cpus;
        self
    }

    /// Sets which latency histograms the writer keeps (see [`LatencyMode`]; from 1.1).
    pub fn latency(mut self, mode: LatencyMode) -> Self {
        self.latency = mode;
        self
    }
}

/// Which latency histograms a writer keeps of its own calls ([`WriterConfig::latency`], ADR 0047). From 1.1.
///
/// The histograms are read through [`Writer::latency`](crate::Writer::latency), in the writer's process only.
/// [`latency::WriterLatency`](crate::latency::WriterLatency) says what each one measures and what it is not.
///
/// Each histogram takes about 30 KB, allocated and written by [`Writer::open`](crate::Writer::open). Recording does not
/// allocate, lock or fail, has no effect on what is committed or durable, and writes nothing to the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum LatencyMode {
    /// None. [`Writer::latency`](crate::Writer::latency) is `None`, and nothing reads a clock for it. The default.
    #[default]
    Off,
    /// Rolls and checkpoints ([`roll`](crate::latency::WriterLatency::roll),
    /// [`time_to_durable`](crate::latency::WriterLatency::time_to_durable)). An append that does not roll does the same
    /// work as with [`LatencyMode::Off`].
    RollsAndCheckpoints,
    /// Also every append and every batch ([`append`](crate::latency::WriterLatency::append),
    /// [`batch`](crate::latency::WriterLatency::batch)), without sampling: two reads of the monotonic clock and one
    /// histogram update per call, which can cost as much as a small append itself (`docs/operations.md` has the measured
    /// cost).
    ///
    /// **Linux:** do not use this where the clock source is not one the vDSO reads: each clock read is then a system
    /// call. `/sys/devices/system/clocksource/clocksource0/current_clocksource` names it; `tsc`, `kvm-clock`,
    /// `hyperv_clocksource_tsc_page` and `arch_sys_counter` are read without one, `hpet` and `acpi_pm` are not. The
    /// library does not check it. macOS reads the clock without a system call.
    All,
}

/// Where the writer's helper threads run ([`WriterConfig::helper_cpus`]).
///
/// A new thread starts with the CPU affinity of the thread that creates it. The writer creates its helpers from the
/// calling thread: the prefault and write-back helpers in [`Writer::open`](crate::Writer::open), the checkpoint
/// thread at the first [`Writer::request_checkpoint`](crate::Writer::request_checkpoint), the spare-segment thread
/// ([`WriterConfig::spare_segment`]) in `Writer::open` on Linux and during an append on macOS. An application that
/// pins its writer thread to one CPU would otherwise get every helper on that CPU, where it competes with the writer:
/// a checkpoint's sync calls and their CPU time then delay appends.
///
/// **Linux:** each helper sets its own affinity when it starts (`sched_setaffinity`). The kernel always intersects
/// the mask with the CPUs the process's cpuset (cgroup) allows. **macOS:** no effect; the platform has no call to
/// bind a thread to CPUs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum HelperCpus {
    /// Every CPU the process's cpuset allows, except the CPUs the kernel lists as isolated
    /// (`/sys/devices/system/cpu/isolated`, from the `isolcpus` boot parameter), unless that would leave none. This
    /// is not limited by a mask set with `taskset` or `sched_setaffinity` on the process or the calling thread: such a
    /// mask belongs to each thread, and the calling thread's may be one CPU. To keep the helpers inside such a mask,
    /// use [`HelperCpus::Inherited`] from a thread that has it, or name the CPUs. The default.
    #[default]
    Process,
    /// Keep the mask inherited from the creating thread (the behaviour before this option existed).
    Inherited,
    /// These CPUs (numbers as the kernel counts them, below 8,192), intersected with the process's cpuset. If the
    /// kernel refuses the mask (no listed CPU is allowed, or the list is empty), the helper uses
    /// [`HelperCpus::Process`] instead.
    List(Vec<usize>),
}

impl Default for WriterConfig {
    fn default() -> Self {
        WriterConfig {
            segment_size: Self::DEFAULT_SEGMENT_SIZE,
            recovery: RecoveryPolicy::default(),
            notify: true,
            prefault: true,
            early_writeback: true,
            spare_segment: cfg!(target_vendor = "apple"),
            helper_cpus: HelperCpus::default(),
            latency: LatencyMode::default(),
            retention: None,
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
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReaderConfig {
    /// How to wait for new records.
    pub backoff: Backoff,
    /// Return only records below the current checkpoint marker's `next_seq`. Default: false.
    ///
    /// Without a valid marker, no records are returned. See ADR 0010 (`docs/decisions/0010-durable-only-reads.md` in the
    /// repository) and crash contract C13 (`docs/crash-contract.md`) for the durability guarantee and storage assumptions.
    pub durable_only: bool,
    /// After the spin and yield phases of [`Backoff`], block on the writer's shared-memory wake-up region instead of
    /// sleeping, so a new record wakes the reader immediately (ADRs 0017 and 0021). The region is found through the
    /// journal's `notify` record, which the reader only reads. It can't shrink, so this never exposes a safe reader to
    /// `SIGBUS`. Readers fall back to sleeping when there is no running writer, on macOS before 14.4, when the writer
    /// runs as another user (or, on Linux, in another pid namespace), or if the platform's wait call fails.
    /// Default: true.
    pub notify: bool,
    /// Let [`Reader::seek`](crate::Reader::seek) start at a record named by a seek index, if there is one. Default:
    /// false. See [`ReaderConfig::seek_index`]. Added in 1.1.
    pub seek_index: bool,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        ReaderConfig {
            backoff: Backoff::default(),
            durable_only: false,
            notify: true,
            seek_index: false,
        }
    }
}

impl ReaderConfig {
    /// Sets whether to return only records covered by a checkpoint. Default: false.
    ///
    /// A record is returned only when its sequence number is below the checkpoint marker's `next_seq`.
    /// A missing or invalid marker covers no records. At the boundary, `try_next` returns `None` without
    /// consuming the record, and `next_timeout` polls until a checkpoint covers it or the timeout expires.
    /// If the writer never checkpoints, this reader returns nothing.
    ///
    /// A marker describes the journal's current files. When a refreshed marker covers records past the cursor, the
    /// reader first confirms that its segment's path still names the file it has open (one `stat`, only then, never
    /// per record). If recovery replaced that segment or retention removed it, the reader returns
    /// [`Error::Discarded`](crate::Error::Discarded) rather than read records the marker doesn't vouch for; call
    /// [`Reader::seek`](crate::Reader::seek) to continue. If the check passes, the reader reads the records the marker
    /// newly covers from the file's current bytes, not from what it had read ahead before, so recovery removing
    /// records after the checkpoint in place (in the same file) can't make it return them. A reader overtaken by
    /// retention may therefore learn of the removal when the checkpoint moves past its cursor, before the end of the
    /// removed segment (ADR 0034).
    ///
    /// See ADR 0010 (`docs/decisions/0010-durable-only-reads.md` in the repository) and crash contract C13
    /// (`docs/crash-contract.md`), including its storage assumptions. The marker is read from the file, never mapped, and cached until needed.
    ///
    /// ```
    /// use tickertape::ReaderConfig;
    /// let config = ReaderConfig::default().durable_only(true);
    /// assert!(config.durable_only);
    /// ```
    pub fn durable_only(mut self, on: bool) -> Self {
        self.durable_only = on;
        self
    }

    /// Sets whether to block on the wake-up region instead of sleeping (see [`ReaderConfig::notify`]).
    pub fn notify(mut self, on: bool) -> Self {
        self.notify = on;
        self
    }

    /// Sets the wait strategy.
    pub fn backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// Sets whether [`Reader::seek`](crate::Reader::seek) may start at a record named by a seek index (ADR 0045).
    /// Default: false. Added in 1.1.
    ///
    /// Indexes are optional files that `tickertape index` ([`seek_index::build`](crate::seek_index::build)) writes
    /// next to the journal, one per sealed segment. With one, a seek validates the record it lands on and every record
    /// from there, less than 64 KiB of records before the target, instead of every record from the segment's start.
    /// Without one, or if anything about it is in doubt, a seek does what it does without the option, and it never
    /// fails because of an index. A durable-only reader, and a mapped one
    /// ([`Reader::open_mapped`](crate::Reader::open_mapped)), uses an index only for a segment before the one the
    /// checkpoint marker names, so in an archive without a marker only a safe, visible reader uses them.
    ///
    /// The option is off by default because it changes what a seek promises: **records before the landing record are
    /// not read,** so damage there no longer stops the reader (`tickertape verify` and a reader without the option
    /// still report it); and where `seek` otherwise lands on the current chain, a stale or forged index can lead it
    /// elsewhere (`docs/crash-contract.md`, C15). A stale one can do so only if a payload holds, at the indexed offset,
    /// a complete frame identical in sequence number, length and CRC to the record indexed there before. A forged one
    /// can whenever a payload contains a suitable frame. `tickertape index --check` finds both. **Do not enable this
    /// where someone who may not write the segments can write `seek-index/`.**
    pub fn seek_index(mut self, on: bool) -> Self {
        self.seek_index = on;
        self
    }
}
