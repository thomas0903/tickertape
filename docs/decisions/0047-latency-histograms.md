# 0047: Latency histograms in the library and the CLI

- **Status:** accepted (2026-11-25; design only, nothing is built; issue #61)
- **Date:** 2026-11-25
- **Relates to:** [0032](0032-operational-metrics.md) (operational metrics; it added no work to `append`, and that
  stays true unless the option here is on), [0041](0041-batched-append.md), [0038](0038-background-checkpoints.md),
  [0043](0043-compatibility-policy.md)

## Context

Only the tools under `benches/` record latency. A program that uses the library, or an operator with the CLI, cannot
see how long its appends, rolls and checkpoints take, or how old records are when a reader gets them. `status` gives
gauges at one moment; it cannot show a p99.9 or a maximum. The users are low-latency trading programs: they read the
tail, so a design that samples one call in N, or that loses the maximum, is no use.

Constraints: no mandatory telemetry service; no allocation and no system call added to the append path; no change to
the on-disk format or to the crash contract; only additions to the 1.0 surfaces, and C ABI version 1 stays; `unsafe`
stays where CONTRIBUTING allows it; `reader.rs`, `writer.rs` and the CLI's `tail` have other changes in flight.

Two facts limit what can be measured at all:

- **A record carries no time set by the library.** Its header has `venue_ts_ns` and `local_ts_ns`, both supplied by
  the caller (`format.md` → Record). By convention they are wall-clock nanoseconds since 1970, and 0 means not set;
  the library stores them and never orders or validates by them. The moment of commit is not recorded anywhere.
- **Monotonic clocks belong to one boot of one machine.** A duration with both ends in one process can use one. A time
  from one process to another, through the file, can only use what the caller stored.

## Decision in brief

- The first version measures, in the writer's process, the duration of **appends**, of **batches** and of **rolls** and
  the **time from a checkpoint request to its durable marker**, and, in a reader's process, the **age of a record when
  it is returned**.
- A new module `tickertape::latency` in the core crate holds a fixed-size histogram, its handles and the record-age
  counter. No new dependency.
- The writer's histograms are off by default. `WriterConfig::latency` takes a `LatencyMode`: `RollsAndCheckpoints` adds
  nothing to an append that does not roll; `All` also times every append and batch, without sampling. `reader.rs` is
  not touched: record age is measured by the code that calls the reader.
- `tickertape age <DIR>`, a new command, follows a live journal from its end and reports record ages, as text or JSON.
  The writer's histograms can be read in the writer's process only; issue #61 stays open for showing them elsewhere.
- The C ABI and the C++, Python and Java bindings get nothing in the first version.

## What is measured

Every duration below is wall time as the thread saw it, so it includes being descheduled. Calls that fail are not
recorded: the caller already has the error. Nothing is subtracted for the cost of the clock.

### In the first version

| Name | Start | End | Clock | One sample per |
|---|---|---|---|---|
| `append` | Entry of `Writer::try_append_with` (which `append` and `append_with` call), before any check | Just before it returns `Ok(Ok(seq))` | Monotonic (`std::time::Instant`), one thread | Successful call |
| `batch` | Entry of `Writer::append_batch` | Just before it returns `Ok` | As above | Successful call that appended at least one record |
| `roll` | Where the append path has found that the record does not fit, before the roll | The roll succeeded; the record is not written yet | As above | Successful roll, also inside `append_batch` |
| `time_to_durable` | Entry of `Writer::checkpoint`, or of the earliest `Writer::request_checkpoint` call that the checkpoint covers | The finish time the writer already takes, after `checkpoint::run` returned `Ok` | Monotonic, one process (two threads for background checkpoints) | Checkpoint that wrote its marker |
| `age` | The record's `local_ts_ns`, set by whoever appended it | The call that returned the record has returned, and the caller has handed it to the counter | The caller's clock at the start; `clock::realtime_ns()` at the end | Counted record (rules below) |

**`append`** includes the checks, the payload copy or the caller's `fill` closure, the CRC, the commit, the wake-up of
blocked readers (a system call when one waits), the hand-off to the helper threads, page faults and the roll if there
is one. It does not include building the record before the call, or the histogram's own update. A `fill` that declines
or panics is not recorded, although its clean-up syncs and is slow. Cost: two monotonic clock reads and one histogram
update per call. Parts of both reads fall inside the interval, together roughly one read, so an empty call would not
show 0; the documentation must give that floor per platform. Clocks tick: 41.7 ns on Apple silicon, the period of
`cntfrq_el0` on other aarch64 machines, so short appends show as a few multiples of it. Timing also perturbs: Linux
reads the counter with ordering instructions, which limits how far consecutive appends overlap in the CPU, and the end
read does not wait for the store buffer to drain, so part of an append's cost can fall outside its interval.

`append` is a **service time, not a response time**. If one append stalls for 5 ms, the events queued behind it are
late too, but the histogram has one 5 ms sample (coordinated omission). The library cannot know when an event should
have been appended; a program that can records `now - intended` in a `latency::Recorder` of its own. And **a call or
checkpoint that is still stuck has no sample yet**; `tickertape status` shows that (a head that stands still, a growing
exposure age). Both warnings go on `WriterLatency::append`, where the number is read, and the metric name in
`operations.md` is `append_service_seconds`.

**`batch`** includes the caller's iterator, all its records, rolls and the one wake-up, whatever the batch's size. A
batch that stops with a `BatchError` is not recorded.

**`roll`** is the part of an append's tail that comes from rolling. The `append` or `batch` sample of the same call
includes it. The definition is "the time the appending thread spends rolling", so it stays valid if issue #60 moves
part of the work to another thread.

**`time_to_durable`** has the same meaning for both kinds of checkpoint. It includes waiting behind a checkpoint
already running, and excludes the retention run that follows. Requests that join a waiting checkpoint (ADR 0038) give
one sample, timed from the earliest, so the number is never too small. It is not the time from a record's append to its
durability, which adds the wait for the next request; `age` in a durable-only reader measures that. The finish
time is taken once the marker is durable, its best-effort extension is done and waiting readers are woken; a program
that polls `checkpoint_progress()` sees the checkpoint later.

**Not the same as the published tables.** `tt-sustained` records to-durable once per request (`settle`), so joined
requests count several times there and once here. Its "roll" is the whole append that rolled; `roll` here is only the
roll, so it is smaller.

**`age`** is `clock::realtime_ns()` after the read minus `local_ts_ns`. It is called age, not writer-to-reader latency,
because it starts where the caller took its timestamp, which can be long before the append (the receive time of a
market-data packet is the usual choice). It equals writer-to-reader latency only if the caller stamps right before
`append`. `latency::RecordAge` applies these rules, **given the caller's declaration of when its reader is live**:

- **Not live: not in the histogram** (checked first), counted as `before_live` until the first declaration and as
  `after_discontinuity` later. A replayed record's age says when it was recorded, not how fast anything is. The counter
  starts not live; the caller declares it live once its reader has caught up (or has been placed at the end), and not
  live after a `seek` or `Error::Discarded`. While live, every record counts, including a backlog that builds up later:
  a slow consumer's lateness is the thing to measure. The library does not check the declaration: a forgotten
  `set_live(false)` before a backward `seek` puts a replay into the histogram.
- `local_ts_ns == 0`: counted as `unset`, not in the histogram.
- `local_ts_ns` later than the observer's clock: counted as `ahead`, not in the histogram. It is never recorded as 0.

**When `ahead` is not 0, the minimum and every percentile are biased upwards**: the fastest records are the ones
missing. That happens after a backward clock step, and with a constant offset: a NIC or PHC hardware stamp, a clock on
TAI (37 s ahead of UTC), or another clock a few microseconds fast. A constant offset the other way adds itself to every
age, unseen. On macOS `realtime_ns` ticks in 1 µs steps (measured), so ages below that show as 0 and a finer stamp can
look ahead.

Two processes on one machine (also in different containers) that both stamp and observe with `CLOCK_REALTIME` share the
clock, and a step moves the result by the step: forward shows as a false maximum, backward as `ahead`. Use a slewed
clock. On a tape copied to another machine, or replayed later, nothing new is appended, so a reader that follows the
rule counts nothing, which is the correct answer. If a program appends old records again with their original
timestamps, or stamps with something other than Unix nanoseconds, the ages mean nothing (with uptime nanoseconds they
are decades, about 1.8e18 ns each); the library cannot detect that.

### Not in the first version

| Candidate | Why not |
|---|---|
| Reader poll or wake-up delay | It starts at the commit, whose time is not recorded. Storing it would change the format and add a clock read to every append. The time since the reader's previous poll is only an upper bound. `age` contains the delay |
| Checkpoint run time (sync calls only, without queueing) | Equal to `time_to_durable` for a blocking checkpoint with none in flight. Add it when someone needs to separate queueing from the disk |
| Age from `venue_ts_ns` | Another machine's clock. Without knowing the synchronisation the number has no meaning |
| The published "Pause" of `tt-sustained`, retention after a blocking `checkpoint()`, the closes and unmaps in `request_checkpoint` | Stalls of the writer's thread outside an append. A caller can time its own `checkpoint` and `request_checkpoint` calls; "Pause" also needs the caller's polling loop |
| Declined and failed appends and batches | The caller has the error or declined on purpose |

## Where it lives

**Chosen: a module in the core crate, four timing points behind a run-time option, and nothing in the reader.**

Off, the append path gains one test. `try_append_with` becomes:

```rust
if self.append_latency.is_some() {
    return self.try_append_timed(record, payload_len, fill); // #[inline(never)]: calls write_record between two reads
}
self.write_record::<_, _, true>(record, payload_len, fill)
```

The test is on the append recorder only (`append_latency: Option<Recorder>`, set by `LatencyMode::All`), so
`RollsAndCheckpoints` takes the same path as `Off` in an append that does not roll. The other recorders are in a second
field, behind a `Box`. `append_batch` makes the same test once per call. `write_record` changes in one place, inside
its existing roll branch: `self.roll()?` becomes a call to a `#[cold]` function that times the roll if the mode asks
for it; `roll` itself and the rest of the path stay as they are. `Job` gains `requested_at: Option<Instant>` (`absorb`
keeps the earlier one). `run_thread` reuses its `finished` and `checkpoint_inner` the time it stores in
`last_checkpoint_at`; each records only after success, the thread before it posts the result, so a caller that sees the
checkpoint finished also finds the sample.

If an off-cost gate below fails, the fallback is to fold the test into the one for `abandoned_write_error` (one flag,
"this append takes the slow entry", kept in step with both), which leaves the path with as many tests as today. If that
fails too, the feature is not merged in this form.

Rejected:

1. **A crate, or a `TimedWriter`, that wraps `Writer`.** No cost when off, but it cannot see rolls or the end of a
   background checkpoint, every function taking `&mut Writer` would have to change, and the C ABI would need a second
   handle type or the same test one layer up.
2. **A Cargo feature.** Also free when off, but features add up across a dependency graph, the CLI and the binding
   builds have one setting for everybody, and CI would test two libraries. The expected cost of the test does not
   justify it.
3. **The CLI only.** A separate process cannot see an append's duration.
4. **Timing inside `Reader`.** The end point of `age` is the same from outside. A change in `reader.rs` can slow replay
   without touching the per-record code (CONTRIBUTING → Benchmark before and after) and would collide with the seek
   work (PR #66).
5. **Timing one call in N.** It loses the maximum.
6. **Reading the CPU's counter directly** (`rdtsc`, `cntvct_el0`). Cheaper, but it needs `unsafe` or assembly, a check
   for an invariant counter and a calibration, all to save a few tens of nanoseconds in an opt-in feature. It is not
   to be used to reach the budget below.

**Clock reads and "no system call".** With a vDSO clock source (`tsc`, `kvm-clock`, `hyperv_clocksource_tsc_page`,
`arch_sys_counter`) Linux answers `Instant::now()` without entering the kernel. With others (`hpet`, `acpi_pm`, some
Xen guests) each read is a system call. The library does not test for this; `operations.md` names
`/sys/devices/system/clocksource/clocksource0/current_clocksource` and says not to use `LatencyMode::All` there. macOS
reads the clock without a system call.

## The histogram

One type, nanoseconds in `u64`, log-linear: values below 128 exactly, then 64 equal buckets per power of two up to
`u64::MAX`.

```text
shift(v) = 57u32.saturating_sub((v | 1).leading_zeros())
index(v) = 64 * shift(v) + (v >> shift(v))            // 0 ..= 3775
bounds(i) = [i, i]                                    // i < 128
            [m << s, (m << s) | ((1 << s) - 1)]       // otherwise: s = i / 64 - 1, m = i % 64 + 64; no overflow at 3775
```

- **Range.** Every `u64` has a bucket, so nothing is out of range, nothing is clamped and recording cannot fail. The
  documentation need only speak of nanoseconds to minutes; the top costs nothing at run time.
- **Precision.** A bucket is at most 1/64 (1.6 %) of its lower bound wide. A quantile is the upper bound of its bucket,
  capped at the exact maximum: never below the true sample, at most 1.6 % above.
- **The maximum and the minimum are exact**, kept outside the buckets, with the sum (for the mean). They are written
  before the bucket's count, so a snapshot's maximum is never below any sample it counts. The sum saturates, because
  meaningless ages (above) would overflow a `u64` at the eleventh sample; a saturated sum, and its mean, are unknown.
- **Memory.** 3,776 counters of 8 bytes: 30,208 bytes per histogram, about 121 KB for the writer's four with `All`.
  Allocated at `Writer::open`, and every page is written there (with atomic stores, so that the compiler cannot turn it
  into a lazily zeroed allocation): the first samples must not take page faults on the append path. After a `fork` they
  do, once per page, as copy-on-write memory.
- **Recording.** One thread records. The counters are `AtomicU64`, updated with a load and a store, without a locked
  instruction; `&mut self` on the recorder enforces the single thread. The bucket is stored last with `Release`.
  `time_to_durable` is the exception: it is recorded by the writer's thread or the checkpoint thread, rarely, so it
  uses atomic read-modify-write operations.
- **Reading, from any thread of the process.** A `View` (a shared reference-counted handle, `Send + Sync`) takes a
  `Snapshot`: it loads every bucket with `Acquire`, then the sum, minimum and maximum. No lock, and the recorder never
  waits. Its count is the sum of its buckets, so its quantiles are consistent with it, but a snapshot taken during
  recording is not a prefix of the samples: the scan can miss sample n and count sample n + 1. The sum (hence the mean)
  and the extremes can already include a sample that is not counted yet; with a count of 0 everything is reported as
  absent. The one disturbance: the next update of a cache line that a snapshot read misses the cache once. Snapshots
  once a second or slower are the intended use. A `View` outlives its writer. In a forked child it is a frozen copy.
- **Intervals.** Histograms only grow; there is no reset, which would need the recorder's cooperation or lose samples.
  `later.since(&earlier)` subtracts. The interval's maximum is exact when the overall maximum rose in it, otherwise the
  upper bound of its highest bucket, capped at the overall maximum, so it is never too small (the minimum likewise,
  never too large). It can be the maximum of a sample that this interval does not count yet.

Rejected: **`hdrhistogram`** (used by the bench tools). `deny.toml` accepts it, and in the prototype it recorded as
fast as this one. But reading it needs exclusive access, or its `SyncHistogram`, where a refresh waits until every
recorder has made a record (a writer that is idle holds the monitor up) and the recording thread hands its histogram
over a channel; that also adds `crossbeam-channel`. Its type would either become part of the stable API or be wrapped
completely; recording returns a `Result`; at three significant digits it takes 221 KB for 1 ns to 60 s, and the core
crate has four dependencies today. **Moving the bench tools' code** is not possible, as it is that crate. The bench
tools stay on it, with three significant digits, so that published results remain comparable. **Locked atomic updates
everywhere** cost about 3 to 5 ns more per append for nothing, since there is one writer.

## Public surface

### Rust (additions only)

```rust
// WriterConfig is #[non_exhaustive]: a new field and its builder.
pub latency: LatencyMode,                            // default Off
pub fn latency(self, mode: LatencyMode) -> Self

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum LatencyMode { #[default] Off, RollsAndCheckpoints, All }   // at the crate root, with WriterConfig

impl Writer {
    /// The writer's histograms, or `None` with `LatencyMode::Off`.
    /// Clone the result to read it from another thread. Costs nothing on the append path.
    pub fn latency(&self) -> Option<&latency::WriterLatency>;
}

pub mod latency {
    /// Records durations; for the application's own measurements too (for example from an intended send time).
    pub struct Recorder;                             // Debug, Default, Send; not Clone
    impl Recorder {
        pub fn new() -> Recorder;
        pub fn record(&mut self, value: Duration);   // more than u64::MAX ns counts as u64::MAX
        pub fn record_ns(&mut self, ns: u64);
        pub fn view(&self) -> View;
    }

    #[derive(Debug, Clone)]
    pub struct View;                                 // Send + Sync
    impl View { pub fn snapshot(&self) -> Snapshot; }   // allocates 30 KB

    #[derive(Debug, Clone)]
    pub struct Snapshot;                             // private fields
    impl Snapshot {
        pub fn count(&self) -> u64;
        pub fn sum(&self) -> Option<Duration>;       // zero when empty; None once saturated, also through `since`
        pub fn mean(&self) -> Option<Duration>;      // None when empty or when `sum` is None
        pub fn min(&self) -> Option<Duration>;       // None when empty, as for the next two
        /// Exact for a snapshot; after `since` it may be a bucket bound, never below the true maximum.
        pub fn max(&self) -> Option<Duration>;
        /// Nearest rank: the smallest r in 1..=count with r / count >= q. None also if q is not in 0.0..=1.0.
        pub fn value_at_quantile(&self, q: f64) -> Option<Duration>;
        pub fn since(&self, earlier: &Snapshot) -> Snapshot;   // saturating
        pub fn buckets(&self) -> impl Iterator<Item = Bucket> + '_;   // the non-empty ones, ascending
    }
    #[derive(Debug, Clone, Copy)]
    #[non_exhaustive] pub struct Bucket { pub low_ns: u64, pub high_ns: u64, pub count: u64 }   // both inclusive

    #[derive(Debug, Clone)]
    pub struct WriterLatency;                        // private fields; None: not measured in this mode
    impl WriterLatency {
        pub fn append(&self) -> Option<&View>;       // All
        pub fn batch(&self) -> Option<&View>;        // All
        pub fn roll(&self) -> Option<&View>;         // All, RollsAndCheckpoints
        pub fn time_to_durable(&self) -> Option<&View>;   // All, RollsAndCheckpoints
    }

    /// Ages of records as a reader returns them. Starts not live.
    pub struct RecordAge;                            // Debug, Default, Send; not Clone
    impl RecordAge {
        pub fn new() -> RecordAge;
        pub fn set_live(&mut self, live: bool);
        /// Reads the wall clock and counts the record. The age, if it went into the histogram.
        pub fn observe(&mut self, record: &Record<'_>) -> Option<Duration>;
        /// The same from the two times: for an `OwnedRecord`, a clock read already taken, or a test.
        pub fn observe_at(&mut self, local_ts_ns: u64, now_ns: u64) -> Option<Duration>;
        pub fn view(&self) -> RecordAgeView;
    }
    #[derive(Debug, Clone)] pub struct RecordAgeView;   // Send + Sync; snapshot() -> RecordAgeSnapshot
    #[derive(Debug, Clone)]
    #[non_exhaustive]
    pub struct RecordAgeSnapshot {
        pub age: Snapshot, pub before_live: u64, pub after_discontinuity: u64, pub unset: u64, pub ahead: u64,
    }
}
```

No method panics: the arithmetic saturates. `Writer` stays `Send` and not `Sync`. `WriterMetrics` does not change: it
is `Copy` and cheap, a histogram is neither. The usual loop for `RecordAge`:

```rust
let mut age = latency::RecordAge::new();
loop {
    let record = match reader.try_next()? {
        Some(record) => Some(record),
        None => {
            age.set_live(true); // caught up: from here on every record counts
            reader.next_timeout(Some(Duration::from_secs(1)))?
        }
    };
    // ...observe(&record) first, then handle it
}
```

(The implementer may have to restructure it for the borrow checker; the documentation's version must be a compiled
example. After `Reader::seek_to_end` (ADR 0046, if it is merged) the caller can set live at once.)

Documentation to change with the code: `compatibility.md` (module row, the CLI list), `cli.md` (also its opening list
of what the CLI writes, for `--wait block`), `operations.md` (Monitoring: the option, its measured cost, how to export
it, what it does not show), the rustdoc of `clock` ("never reads a clock on its own behalf" becomes: never for anything
it writes into the journal, apart from `created_unix_ns`), and one sentence in `format.md` → Timestamps: monitoring
(`unread_age`, `RecordAge`) compares `local_ts_ns` with a clock, and no read, order or validity rule depends on it. The
crash contract does not change.

### CLI

```text
tickertape age <DIR> [--duration <DUR>] [--count <N>] [--interval <DUR>] [--durable-only]
               [--wait poll|spin|block] [--json]
```

Follows the journal with the safe reader, starting at the end as `tail --from-end` does, and counts record ages. Live
from the first time its reader has nothing more to read (at once, where it starts with `seek_to_end`); not live after
discarded history until it has caught up again (it resumes as `tail` does, with the same warnings). It prints no
records, so output speed is not part of the numbers. `--durable-only` gives the age at which a durable-only reader gets
records. `DUR` is as in `status --watch`. It ends after `--duration`, or after `--count` records returned while live,
whichever comes first, and prints the final report. `--interval` prints a report each interval before that. With none
of the three it runs until interrupted with `--interval 10s`; an interrupt kills it without a final report (no signal
handler), so at most one interval is lost. Exit codes as `tail`: 0, 1 usage, I/O or missing journal, 2 corruption or
unsupported format, after printing the report of what it measured.

**Waiting must not slow the writer it watches.** A reader that blocks registers as a waiter in the wake-up region, and
then `Notify::publish` makes a system call inside the append, before it returns. So `--wait poll`, the default, is
`notify(false)` with the default backoff: it costs the writer nothing, and adds up to its sleeps (10 µs, doubling to 1
ms while idle) to an age. `spin` is `notify(false)` with `Backoff::busy_spin()`: no floor, one core in use. `block` is
the reader's default wait: prompt, but an append that finds it waiting pays a wake-up system call, which the writer's
own `append` histogram shows. `cli.md` must give these costs.

**It measures this command's reader on the CPU it happens to run on, not the application's reader.** It says whether
the writer, the clock and the machine are healthy; the help text and `cli.md` must say that an application measures its
own reader with `RecordAge`.

A new command rather than an option on `tail`: `tail --format json` promises no summary lines in its output, flushes
every line (which would be in the numbers) and has changes in flight.

`--json`: one object per report and line, compact, as the other commands, versioned on its own:

```text
{"schema_version":1,"journal":string,"sampled_unix_ns":u64,"elapsed_s":number,"final":bool,
 "durable_only":bool,"wait":"poll"|"spin"|"block",
 "records":u64,"first_seq":u64|null,"last_seq":u64|null,
 "before_live":u64,"after_discontinuity":u64,"unset_timestamp":u64,"ahead_of_clock":u64,"discontinuities":u64,
 "age":HIST,"interval_age":HIST|null}

HIST = {"count":u64,"min_ns":u64|null,"mean_ns":number|null,"p50_ns":u64|null,"p90_ns":u64|null,"p99_ns":u64|null,
        "p99_9_ns":u64|null,"p99_99_ns":u64|null,"max_ns":u64|null}
```

`journal` is the canonical path and `sampled_unix_ns` the time of the report, as in `status --json`. `records` counts
the records returned while live and `first_seq`/`last_seq` are theirs (null when there are none). `records` is the sum
of `age.count`, `unset_timestamp` and `ahead_of_clock`; `before_live` and `after_discontinuity` count the other
records. Everything is cumulative since the command started, except `interval_age`: the samples since the previous
report, null without an interval. `final` is true on the last report. `discontinuities` counts discarded histories the
command resumed after. `min_ns` and `max_ns` are exact in `age`; in `interval_age` they can be bucket bounds.
Percentiles are nearest rank, bucket upper bounds, at most 1.6 % high; with a small `count` the high ones equal the
maximum. An empty histogram has nulls; `mean_ns` is also null once the sum has saturated. A non-zero `ahead_of_clock`
biases `min_ns` and the percentiles upwards (above). `wait` may gain values; fields may be added. The text output shows
the same values and is not a stable surface.

No Prometheus output in the first version. `operations.md` shows an application exporting a `Snapshot` as gauges with
the client library it already has, as it does for `Status`.

### Bindings

**Nothing now.** Each binding's documentation says so (as `c-api.md` does for `status::Sampler`). Waiting lets the Rust
names settle for a release, since everything in the C ABI is frozen on arrival. The design fits ABI version 1 later, by
additions: `tickertape_writer_options_set_latency(options, mode)`; a handle `tickertape_writer_latency` made from a
writer and freed by the caller, usable from another thread while the writer is in use (a writer handle is not), with
the `View`s inside; and an extensible `tickertape_latency_summary` (`struct_size` first; count, sum, minimum, maximum
and the percentiles of `HIST`, with `TICKERTAPE_UNKNOWN` when empty or unknown). No existing struct or function
changes. C++, Java and Python wrap those. The append is timed in the core, so a binding's own call overhead would not
be in the number.

## What it must not do

- No telemetry service, network, background thread, global state or new dependency.
- On the append path: no allocation, no lock, no system call (with a vDSO clock), no page fault from its own memory
  (until a `fork`), no locked instruction.
- Nothing written to the journal directory, and no change to the format or the wake-up region. Only `tickertape age
  --wait block` registers there, as any blocking reader does.
- No effect on what is committed, durable or recovered: it never fails an append, roll or checkpoint and never poisons
  a writer. If the histograms cannot be allocated, the process aborts as for any allocation at open.
- Nothing is recorded or reported that was not measured: unknown is absent, as in ADR 0032.

## Tests

1. **Histogram (unit, `proptest`).** `index` is monotonic; every `bounds(i)` maps back to `i` at both ends, bounds are
   contiguous and cover `0..=u64::MAX`; every bucket from 128 on is at most 1/64 of its lower bound wide. For random
   samples: count, sum, exact minimum and maximum; each quantile at least the true nearest-rank sample and at most 1/64
   above it; quantile ranks for counts and `q` where `q * count` rounds badly. `since` against recording the interval
   alone, including the extremes' rule. Values 0 and `u64::MAX`; `Duration` beyond `u64::MAX` ns. Eleven samples of
   1.8e18 ns: `sum` and `mean` are `None`, also through `since`, and nothing panics in a debug build.
2. **Against `hdrhistogram`** (in `benches/`, which has it already): same samples, quantiles within the two precisions.
3. **Concurrency.** A `loom` model of one recorder and one snapshot: the snapshot's maximum is never below a counted
   sample, and no count is lost. A release-mode test with a snapshotting thread: counts never decrease, and the final
   count is the number recorded. Miri on the module (it has no `unsafe`).
4. **Writer.** `Off`: `Writer::latency()` is `None`. `RollsAndCheckpoints`: `append` and `batch` are `None`. `All`:
   after N appends (all three methods), B batches, R rolls (inside both) and C checkpoints the counts are exactly N, B,
   R and C; empty and failed batches and failed, declined and panicking appends add nothing; a roll that fails (fault
   injection) adds nothing and the retry counts once. A fill closure, and a batch's iterator, that sleep 20 ms give a
   maximum of at least 20 ms, and later fast calls do not change it. Background checkpoints held at a fault point:
   joined requests give one sample of at least the held time from the earliest request; a failed checkpoint gives none;
   a finished one is visible when `checkpoint_progress` reports it. A `View` read after the writer is dropped.
5. **Record age.** With `observe_at`: `unset`, `ahead`, both not-live counts, live with a backlog, `set_live(false)`. A
   tape replayed without a writer counts nothing; a durable-only reader's ages span the wait for the checkpoint.
6. **CLI** (`assert_cmd`). The schema and its invariants on a live writer in the test; no timestamps; timestamps in the
   future; a copied tape (count 0, exit 0); `--count`, `--duration`, `--interval`; `poll` and `spin` never register
   as a waiter; corruption while following: report, then exit 2; history discarded and removed while following.
7. **Formats.** Journals written with the option on and off are byte-identical for the same appends (segment files;
   the header's creation time and CRC aside), and the compatibility fixtures and crash tests pass with it on.
8. **No allocation, no system calls.** Under Valgrind on Linux the total number of allocations is the same for 10,000
   and 20,000 appends with the option on in one segment; `strace -c` over the same run shows no `clock_gettime`
   (skipped without a vDSO clock source).

## Measurements an implementation must record

Before the feature is called cheap anywhere, in `docs/benchmarks.md` by the rules of `run-benchmark.md`, on **Linux
x86-64 and Linux aarch64** with the CPU pinned, with the clock source named:

1. **Off, instructions:** instructions per 64-byte `append`, base commit against the change, with Callgrind or `perf
   stat -e instructions:u`. Both include helper threads and set-up, so use `prefault(false)` and divide the difference
   of two runs (200,000 and 400,000 appends in one segment) by the difference of the counts. **Merge gate: at most 3
   more per append.**
2. **Off, time:** the `append` benchmark (64 B and 1,024 B, prefault on) on the base and the changed build, run as
   interleaved pairs in alternating order, at least 10; and base against a second build of the base in the same way.
   Report the mean paired difference with its 95 % confidence interval. The bound is **1 ns per append at 64 B and 2 %
   at 1,024 B**. The run is **invalid** unless the base-against-base interval lies within plus or minus the bound: take
   more pairs or a quieter machine, never a wider bound. **Merge gate: in a valid run, the upper end of the
   base-against-changed interval is at most the bound.** "Nothing measurable" means 1 and 2 passed; the docs then give
   the figures. Both modes are run, `Off` and `RollsAndCheckpoints`.
3. **On (`All`), per append:** the mean added to a 64-byte append (paired as in 2), the histogram update alone, the
   clock pair alone, and the p50 of an empty timed interval (the floor). **Published budget: at most 60 ns added per
   append, of which at most 5 ns for the histogram.** It decides whether the docs may call the feature cheap. It is not
   a merge gate. The docs give the measured figure and the throughput with the option on either way: the published 26.7
   million appends/s on Linux are about 37 ns each, so `All` can cost more than the append itself.
4. **Tail, reported, not gated:** `tt-latency` at 100,000 records/s, p50, p99, p99.9 and maximum: base, `Off`, `All`,
   and `All` with a snapshot every second and every 10 ms.
5. `RecordAge::observe` per record, and the `replay` benchmark unchanged (`reader.rs` is not touched, but the core
   crate is recompiled).

**Prototype figures, not published results.** A scratch program outside the crates, on a busy MacBook (Apple M4 Pro,
other builds running, load about 15) and in the OrbStack VM on it (Linux aarch64, `arch_sys_counter`); medians of 9
(`hdrhistogram` 7.6.0):

| | macOS, busy | OrbStack VM (Linux on macOS), busy |
|---|---|---|
| `Instant::now()` | 14.5 ns | 11.4 ns |
| Two reads and the difference | 39.7 ns | 28.3 ns |
| `clock::realtime_ns()` | 21.8 ns | 18.6 ns |
| Histogram update, load and store / locked (generator's 1.6 ns subtracted) | 0.2 / 3.3 ns | 0.2 / 4.9 ns |
| `hdrhistogram` update, 3 digits (likewise) | 0.3 ns | 0.4 ns |
| Snapshot | 1.1 µs | 1.1 µs |
| Update with a thread taking snapshots without pause (likewise) | 2.5 ns | 0.8 ns |
| 64 B append, plain → timed from outside | 42.5 → 59.1 ns | 50.9 → 112.2 ns (spread 28–96 and 82–141) |

They say only that the clock dominates and that the update is small (subtracting a loop of another shape makes the
sub-nanosecond figures rough). **The Linux append row missed the budget** (+61.3 ns against 60), in a run whose spread
is wider than the difference; macOS added 16.6 ns. Nothing here measures x86-64 or the off cost.

## The smallest useful first version

In this order, each a pull request that stands alone:

1. `latency::{Recorder, View, Snapshot, Bucket}` and their tests. Useful alone: an application can time anything,
   including from intended times and around `checkpoint`. Touches no existing file but `lib.rs` and docs.
2. `RecordAge` and `tickertape age`. Touches neither `reader.rs` nor `writer.rs`.
3. `LatencyMode` with the four writer histograms: gates 1 and 2 passed, 3 and 4 recorded. If the roll work of issue #60
   is merged first, on top of it.

**Deferred:** the bindings, to the next minor release after the Rust surface has been used; the writer's histograms
from another process and in `tickertape status` (issue #61 stays open for it; if built, an opt-in file like a reader
beacon, with its own version byte, written by the application's explicit call on a thread of its choice, not the
wake-up region and not a thread in the library); Prometheus output from the CLI; thresholds and exit code 3 for
`tickertape age`; the published "Pause" metric, retention after a blocking `checkpoint()` and `request_checkpoint`'s
closes and unmaps; checkpoint run time; merging snapshots; a cheaper clock.

No open decisions remain. The precision is 1.6 %; the bucket layout is not promised (`buckets()` returns the bounds).

## Consequences

Users can see the tail of their own appends, batches, rolls, checkpoints and reads without a harness, at a cost they
choose and that is published. The append path gains one test and the core crate one module to maintain. `local_ts_ns`
gets a second observer in the library (after `status`'s unread age), which still never affects what is read. Histograms
in one process cannot be seen from another, and a service-time histogram can look better than what a queue behind it
experienced; both are stated where the numbers are. Implementation adds the module to the `compatibility.md` surface,
so its names are then kept for 1.x.
