//! Building blocks for `tt-scale`: generating a journal of a given shape, and the operations it times (startup,
//! seek, replay, recovery, verification and listing) with their CPU time, page faults and peak RSS.
//!
//! Reader operations run with the safe reader (`pread`) or the zero-copy reader ([`ReaderKind`]). Each operation is
//! meant to run in a process of its own (the binary starts one per operation), so that the peak RSS it reports belongs
//! to that operation. An operation that fails is a result too: its cost and its error.

pub mod archive;
pub mod catch_up;
pub mod damage;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use nix::sys::resource::{Resource, UsageWho, getrlimit, getrusage, setrlimit};
use serde_json::{Value, json};
use tickertape::inspect;
use tickertape::retention;
use tickertape::{NewRecord, Reader, ReaderConfig, Writer, WriterConfig};

use crate::{Result, elapsed_ns, fresh_directory, histogram_json, segment_capacity};

/// Name the checkpoint marker is moved to while `writer-open-without-marker` runs.
pub const SAVED_MARKER: &str = "checkpoint.tt-scale-saved";

/// Every operation `measure` runs by default, in this order. Writer operations come last because they change the
/// journal: `writer-open-after-kill` appends a tail first, and every writer operation leaves a checkpoint at the end. The
/// seek index operations (ADR 0045) come after them, so that every other operation of a new journal runs without indexes.
pub const OPS: &[&str] = &[
    "list-directory",
    "list-segments",
    "reader-open",
    "seek-first",
    "seek-middle",
    "seek-segment-end",
    "seek-last",
    "seek-sealed",
    "seek-forward",
    "seek-to-end",
    "replay",
    "status",
    "verify",
    "retention-plan",
    "writer-open-clean",
    "writer-open-after-kill",
    "writer-open-without-marker",
    "checkpoint-loop",
    "checkpoint-loop-retention",
    "index-build",
    "seek-sealed-indexed",
];

/// The operations of [`OPS`] that write or use a seek index.
pub const INDEX_OPS: &[&str] = &["index-build", "seek-sealed-indexed"];

/// Operations `measure` runs only when asked: `retention-delete` shrinks the journal, the `cli-` ones need `--cli`.
pub const OPTIONAL_OPS: &[&str] = &[
    "retention-delete",
    "index-refresh",
    "index-check",
    "cli-verify",
    "cli-export",
    "cli-stats",
];

/// The operations that open a reader, and so run once for each requested [`ReaderKind`].
pub const READER_OPS: &[&str] = &[
    "reader-open",
    "seek-first",
    "seek-middle",
    "seek-segment-end",
    "seek-last",
    "seek-sealed",
    "seek-sealed-indexed",
    "seek-forward",
    "seek-to-end",
    "replay",
];

/// The seeks alone, as one name for `--ops` (a run to repeat with and without a seek index).
pub const SEEK_OPS: &[&str] = &[
    "reader-open",
    "seek-first",
    "seek-middle",
    "seek-segment-end",
    "seek-last",
    "seek-sealed",
    "seek-forward",
    "seek-to-end",
];

/// Steps `seek-forward` divides a segment into.
const FORWARD_STEPS: u64 = 16;

/// Which reader an operation uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderKind {
    /// `Reader::open` (`pread`).
    Safe,
    /// `Reader::open_mapped` (zero-copy).
    Mapped,
}

impl ReaderKind {
    /// Parses `safe` or `mapped`.
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "safe" => Ok(ReaderKind::Safe),
            "mapped" => Ok(ReaderKind::Mapped),
            _ => Err(format!("reader must be safe or mapped, not {text:?}").into()),
        }
    }

    /// The name accepted by [`ReaderKind::parse`].
    pub fn name(self) -> &'static str {
        match self {
            ReaderKind::Safe => "safe",
            ReaderKind::Mapped => "mapped",
        }
    }

    /// Opens a reader of this kind on the journal in `dir`.
    pub fn open(self, dir: &Path, config: ReaderConfig) -> tickertape::Result<Reader> {
        match self {
            ReaderKind::Safe => Reader::open(dir, config),
            // SAFETY: only the tickertape writer modifies the journals this tool measures; damage is done before any
            // reader opens them, and nothing truncates a segment.
            ReaderKind::Mapped => unsafe { Reader::open_mapped(dir, config) },
        }
    }
}

/// A histogram of latencies in nanoseconds, up to an hour (`saturating_record` does not resize one).
fn histogram() -> Result<Histogram<u64>> {
    Ok(Histogram::new_with_max(3_600_000_000_000, 3)?)
}

/// A `getrusage` time in microseconds.
// The libc field types (`time_t`, `suseconds_t`) differ between systems.
#[allow(clippy::useless_conversion)]
fn micros(t: nix::sys::time::TimeVal) -> i64 {
    i64::from(t.tv_sec()) * 1_000_000 + i64::from(t.tv_usec())
}

/// Process resource usage at one instant (`getrusage(RUSAGE_SELF)`).
#[derive(Debug, Clone, Copy)]
struct Usage {
    wall: Instant,
    user_us: i64,
    system_us: i64,
    minor_faults: i64,
    major_faults: i64,
    max_rss_bytes: i64,
    io: Option<(u64, u64)>,
}

impl Usage {
    // `c_long` differs between systems.
    #[allow(clippy::useless_conversion)]
    fn now() -> Result<Usage> {
        let usage = getrusage(UsageWho::RUSAGE_SELF)?;
        // `ru_maxrss` is in bytes on macOS and in KiB on Linux and the BSDs. On Linux it also keeps the parent's
        // high-water mark across `exec` (a child started from a process with a large RSS reports that RSS), so
        // there the mark of this process's own memory map is used instead (`VmHWM`, see `reset_peak_rss`).
        let rss_unit = if cfg!(target_vendor = "apple") {
            1
        } else {
            1024
        };
        let max_rss_bytes = linux_vm_hwm().unwrap_or(i64::from(usage.max_rss()) * rss_unit);
        Ok(Usage {
            wall: Instant::now(),
            user_us: micros(usage.user_time()),
            system_us: micros(usage.system_time()),
            minor_faults: i64::from(usage.minor_page_faults()),
            major_faults: i64::from(usage.major_page_faults()),
            max_rss_bytes,
            io: linux_storage_io(),
        })
    }

    /// The cost of what ran between `self` and `later`. Peak RSS is the process's high-water mark at `later`;
    /// `peak_rss_before_bytes` is the mark before the operation (on Linux, just after it was reset to the RSS then).
    /// On Linux it counts the file pages the process has mapped.
    fn json(&self, later: &Usage) -> Value {
        json!({
            "wall_s": later.wall.duration_since(self.wall).as_secs_f64(),
            "user_cpu_s": (later.user_us - self.user_us) as f64 / 1e6,
            "system_cpu_s": (later.system_us - self.system_us) as f64 / 1e6,
            "minor_faults": later.minor_faults - self.minor_faults,
            "major_faults": later.major_faults - self.major_faults,
            "storage_read_bytes": self.io.zip(later.io).map(|(a, b)| b.0 - a.0),
            "storage_write_bytes": self.io.zip(later.io).map(|(a, b)| b.1 - a.1),
            "peak_rss_bytes": later.max_rss_bytes,
            "peak_rss_before_bytes": self.max_rss_bytes,
        })
    }
}

/// The peak RSS of this process's memory map in bytes (`VmHWM` in `/proc/self/status`; Linux only).
fn linux_vm_hwm() -> Option<i64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kib: i64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

/// Bytes this process has caused to be read from and written to storage (`/proc/self/io`; Linux only). Reads include
/// read-ahead and the pages that faults on mappings brought in; a page another process read first is not counted.
fn linux_storage_io() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/self/io").ok()?;
    let field = |name: &str| -> Option<u64> {
        let line = text.lines().find(|l| l.starts_with(name))?;
        line.split_whitespace().nth(1)?.parse().ok()
    };
    Some((field("read_bytes:")?, field("write_bytes:")?))
}

/// The file descriptors this process has open (`/dev/fd`), not counting the one that lists them.
pub fn open_files() -> Option<u64> {
    let entries = std::fs::read_dir("/dev/fd").ok()?.count() as u64;
    entries.checked_sub(1)
}

/// The soft limit on open files of this process.
pub fn open_files_limit() -> Option<u64> {
    getrlimit(Resource::RLIMIT_NOFILE)
        .ok()
        .map(|(soft, _)| soft)
}

/// Lowers the soft limit on open files to `limit`.
pub fn limit_open_files(limit: u64) -> Result<()> {
    let (_, hard) = getrlimit(Resource::RLIMIT_NOFILE)?;
    setrlimit(Resource::RLIMIT_NOFILE, limit.min(hard), hard)?;
    Ok(())
}

/// Runs `f` while another thread counts the open files about every millisecond, and returns the most it saw. Costs a
/// little time, so runs that sample are not for timings.
fn with_open_files_sampled<T>(f: impl FnOnce() -> T) -> (T, Option<u64>) {
    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&stop);
    let sampler = std::thread::spawn(move || {
        let mut most = open_files();
        while !seen.load(Ordering::Relaxed) {
            most = most.max(open_files());
            std::thread::sleep(Duration::from_millis(1));
        }
        most
    });
    let out = f();
    stop.store(true, Ordering::Relaxed);
    (out, sampler.join().ok().flatten())
}

/// Resets the peak RSS to the current RSS (Linux: writing 5 to `/proc/self/clear_refs`), so that the peak read after
/// an operation is the operation's own. `false` where that is not possible (other systems, older kernels).
fn reset_peak_rss() -> bool {
    std::fs::write("/proc/self/clear_refs", "5").is_ok()
}

/// Runs `f` and returns its result with its cost merged in. If it fails, the cost and the error as `error`.
fn timed(f: impl FnOnce() -> Result<Value>) -> Result<Value> {
    let reset = reset_peak_rss();
    let before = Usage::now()?;
    let extra = f();
    let after = Usage::now()?;
    let mut out = before.json(&after);
    out["peak_rss_reset"] = json!(reset);
    match extra {
        Ok(extra) => {
            if let (Value::Object(out), Value::Object(extra)) = (&mut out, extra) {
                out.extend(extra);
            }
        }
        Err(error) => out["error"] = json!(error.to_string()),
    }
    Ok(out)
}

/// The record every generated record and tail record carries: `payload` bytes of `0x5a`, its index as timestamps.
fn record(payload: &[u8], index: u64) -> NewRecord<'_> {
    NewRecord {
        schema_id: 1,
        venue_ts_ns: index,
        local_ts_ns: index,
        payload,
    }
}

/// Generates a journal of exactly `segments` segments of `segment_size` bytes holding `payload`-byte records: every
/// segment but the newest full, the newest `fill_last` percent full (at least one record). Takes a checkpoint every
/// `checkpoint_every` segments (0: only at the end) and at the end, then closes the writer.
pub fn build(
    dir: &Path,
    segments: u64,
    segment_size: u64,
    payload: usize,
    checkpoint_every: u64,
    fill_last: u64,
) -> Result<Value> {
    if segments == 0 {
        return Err("segments must be positive".into());
    }
    if !(1..=100).contains(&fill_last) {
        return Err("fill-last must be a percentage from 1 to 100".into());
    }
    let capacity = segment_capacity(segment_size, payload)?;
    let last = (capacity * fill_last / 100).max(1);
    let records = (segments - 1)
        .checked_mul(capacity)
        .and_then(|r| r.checked_add(last))
        .ok_or("record count overflow")?;
    fresh_directory(dir)?;
    let bytes = vec![0x5a; payload];
    let start = Instant::now();
    let mut writer = Writer::open(dir, WriterConfig::default().segment_size(segment_size))?;
    let every = checkpoint_every.saturating_mul(capacity);
    for index in 0..records {
        writer.append(&record(&bytes, index))?;
        if every != 0 && (index + 1) % every == 0 {
            writer.checkpoint()?;
        }
    }
    let appended = start.elapsed().as_secs_f64();
    writer.checkpoint()?;
    let final_checkpoint_s = start.elapsed().as_secs_f64() - appended;
    let open_files = open_files();
    writer.close()?;
    let seconds = start.elapsed().as_secs_f64();
    let journal_bytes = segments * segment_size;
    Ok(json!({
        "segments": segments, "segment_size": segment_size, "payload": payload, "records": records,
        "records_per_segment": capacity, "records_in_last_segment": last, "fill_last": fill_last,
        "journal_bytes": journal_bytes, "checkpoint_every": checkpoint_every,
        "seconds": seconds, "bytes_per_s": journal_bytes as f64 / seconds,
        "final_checkpoint_s": final_checkpoint_s, "open_files_at_end": open_files,
    }))
}

/// The shape of an existing journal, read from the segment headers and the checkpoint marker.
#[derive(Debug, Clone)]
pub struct Journal {
    /// Base sequence number of every segment, oldest first.
    pub bases: Vec<u64>,
    /// Total size of the segment files.
    pub bytes: u64,
    /// Size of the newest segment (the size writer operations give new segments).
    pub segment_size: u64,
    /// Sequence number of the first record.
    pub first_seq: u64,
    /// One past the last record: the checkpoint marker's `next_seq`, or the given end.
    pub next_seq: u64,
}

impl Journal {
    /// Reads the shape of the journal in `dir`. It must have a checkpoint marker at the tail (as `build` leaves it),
    /// unless `end` gives the sequence number after its last record (an archive directory has no marker).
    pub fn describe(dir: &Path, end: Option<u64>) -> Result<Journal> {
        let segments = inspect::list_segments(dir)?;
        let next_seq = match end {
            Some(end) => end,
            None => {
                inspect::read_checkpoint(dir)?
                    .ok_or("the journal has no checkpoint marker; build it with `tt-scale build`, or use --end")?
                    .next_seq
            }
        };
        let first_seq = segments.first().map_or(0, |s| s.base_seq);
        if next_seq <= first_seq {
            return Err("the journal holds no records".into());
        }
        Ok(Journal {
            bases: segments.iter().map(|s| s.base_seq).collect(),
            bytes: segments.iter().map(|s| s.segment_size).sum(),
            segment_size: segments.last().map_or(0, |s| s.segment_size),
            first_seq,
            next_seq,
        })
    }

    /// The bases of a full segment in the middle of the journal and of the one after it (the end of the journal for
    /// a journal of one segment).
    fn middle_segment(&self) -> (u64, u64) {
        let i = self.bases.len().saturating_sub(1) / 2;
        let next = self.bases.get(i + 1).copied().unwrap_or(self.next_seq);
        (self.bases[i], next)
    }

    /// The sequence number a seek operation targets: the first, middle or last record, the last record of a full
    /// segment in the middle (the longest scan a seek can cost), that segment's first record for `seek-forward`, or
    /// the last record of the newest sealed segment (of the newest segment, if it is the only one).
    pub fn seek_target(&self, op: &str) -> Option<u64> {
        match op {
            "seek-sealed" | "seek-sealed-indexed" => Some(match self.bases[..] {
                [.., newest] if self.bases.len() > 1 => newest - 1,
                _ => self.next_seq - 1,
            }),
            "seek-segment-end" => Some(self.middle_segment().1 - 1),
            "seek-forward" => Some(self.middle_segment().0),
            "seek-first" => Some(self.first_seq),
            "seek-middle" => Some(self.first_seq + (self.next_seq - self.first_seq) / 2),
            "seek-last" => Some(self.next_seq - 1),
            _ => None,
        }
    }

    /// One past the last record of the segment that `seek-forward` (whose target is `seq`) moves through.
    pub fn segment_end(&self, seq: Option<u64>) -> u64 {
        let after = |seq| self.bases.iter().copied().find(|b| *b > seq);
        seq.and_then(after).unwrap_or(self.next_seq)
    }

    /// Base sequence number of the segment holding `seq`.
    pub fn segment_of(&self, seq: u64) -> u64 {
        let i = self.bases.partition_point(|b| *b <= seq);
        self.bases[i.saturating_sub(1)]
    }

    /// As JSON.
    pub fn json(&self) -> Value {
        json!({"segments": self.bases.len(), "bytes": self.bytes, "segment_size": self.segment_size,
            "first_seq": self.first_seq, "records": self.next_seq - self.first_seq})
    }
}

/// One operation to run and what it needs.
#[derive(Debug, Clone)]
pub struct Op {
    /// The journal.
    pub dir: PathBuf,
    /// Its name ([`OPS`], [`OPTIONAL_OPS`]).
    pub name: String,
    /// The reader a reader operation uses.
    pub reader: ReaderKind,
    /// The target of a seek operation.
    pub seq: Option<u64>,
    /// For `seek-forward`: one past the last record of the segment it moves through.
    pub span_end: Option<u64>,
    /// Where `replay` and the command-line operations stop (exclusive); `None` for the end of the journal.
    pub end: Option<u64>,
    /// Segment size of the segments a writer operation creates.
    pub segment_size: u64,
    /// Checkpoints a checkpoint loop takes; segments `retention-delete` removes.
    pub count: u64,
    /// The `tickertape` program for the `cli-` operations.
    pub cli: Option<PathBuf>,
    /// Let the reader of a seek use seek indexes.
    pub seek_index: bool,
    /// Count open files while the operation runs (not for timings).
    pub sample_files: bool,
}

impl Op {
    /// An operation with the safe reader and no other settings.
    pub fn new(dir: &Path, name: &str, seq: Option<u64>, segment_size: u64) -> Op {
        Op {
            dir: dir.to_path_buf(),
            name: name.to_owned(),
            reader: ReaderKind::Safe,
            seq,
            span_end: None,
            end: None,
            segment_size,
            count: 1,
            cli: None,
            seek_index: false,
            sample_files: false,
        }
    }

    fn target(&self) -> Result<u64> {
        self.seq
            .ok_or_else(|| "a seek operation needs --seq".into())
    }
}

/// Runs one operation in this process and returns its cost and what it found (`error` if it failed). Writer
/// operations leave a checkpoint marker at the end. `writer-open-after-kill` times only the open: the caller must
/// first leave a tail behind a killed writer.
pub fn run_op(op: &Op) -> Result<Value> {
    let files_before = open_files();
    let (result, most) = if op.sample_files {
        with_open_files_sampled(|| dispatch(op))
    } else {
        (dispatch(op), None)
    };
    let mut result = result?;
    result["op"] = json!(op.name);
    if READER_OPS.contains(&op.name.as_str()) {
        result["reader"] = json!(op.reader.name());
    }
    result["open_files_before"] = json!(files_before);
    result["open_files_limit"] = json!(open_files_limit());
    if op.sample_files {
        // The sampling thread's own listing needs a descriptor, which `open_files` does not count.
        result["open_files_most"] = json!(most);
    }
    Ok(result)
}

fn dispatch(op: &Op) -> Result<Value> {
    let dir = op.dir.as_path();
    match op.name.as_str() {
        "list-directory" => timed(|| {
            let entries = std::fs::read_dir(dir)?.count();
            Ok(json!({"entries": entries}))
        }),
        "list-segments" => timed(|| {
            let segments = inspect::list_segments(dir)?;
            Ok(json!({"segments": segments.len()}))
        }),
        "reader-open" => timed(|| {
            let reader = op.reader.open(dir, ReaderConfig::default())?;
            Ok(json!({"position": reader.position()}))
        }),
        "seek-first"
        | "seek-middle"
        | "seek-segment-end"
        | "seek-last"
        | "seek-sealed"
        | "seek-sealed-indexed" => {
            let seq = op.target()?;
            seek_op(op, |reader| {
                reader.seek(seq)?;
                expect_next(reader, seq)?;
                Ok(json!({"seq": seq}))
            })
        }
        "seek-forward" => seek_forward(op),
        "seek-to-end" => seek_to_end(op),
        "replay" => replay(op),
        "status" => timed(|| {
            let status = tickertape::status::status(dir)?;
            let breaches = status.breaches(&tickertape::status::Thresholds::default());
            Ok(
                json!({"segments": status.segments.count, "next_seq": status.head.position.next_seq,
                "verified": status.head.verified, "chain_broken": status.head.chain_broken,
                "corrupt": breaches.iter().any(tickertape::status::Breach::is_corruption)}),
            )
        }),
        "verify" => timed(|| {
            let report = inspect::verify(dir)?;
            Ok(
                json!({"records": report.records, "segments": report.segments.len(),
                "problems": report.problems.len()}),
            )
        }),
        "retention-plan" => timed(|| {
            // A plan only, under a limit no journal reaches: what every retention run pays before it removes
            // anything (the writer makes a plan at open and after checkpoints when retention is configured).
            // Removing a segment costs more: the plan reads each candidate's seal.
            let plan =
                retention::plan(dir, &retention::Policy::default().max_segments(usize::MAX))?;
            Ok(json!({"planned_segments": plan.segments().len()}))
        }),
        "retention-delete" => timed(|| {
            let segments = inspect::list_segments(dir)?.len();
            let keep = segments.saturating_sub(usize::try_from(op.count)?).max(1);
            let start = Instant::now();
            let plan = retention::plan(dir, &retention::Policy::default().max_segments(keep))?;
            let plan_s = start.elapsed().as_secs_f64();
            let report = retention::apply(dir, &plan, &retention::Action::Delete)?;
            Ok(
                json!({"plan_s": plan_s, "apply_s": start.elapsed().as_secs_f64() - plan_s,
                "segments_before": segments, "removed_segments": report.removed_segments,
                "removed_bytes": report.removed_bytes}),
            )
        }),
        "writer-open-clean" | "writer-open-after-kill" | "writer-open-without-marker" => {
            writer_open(op)
        }
        "checkpoint-loop" | "checkpoint-loop-retention" => checkpoint_loop(op),
        "index-build" | "index-refresh" | "index-check" => timed(|| index(dir, &op.name)),
        "cli-verify" | "cli-export" | "cli-stats" => cli(op),
        other => Err(format!("unknown operation {other}").into()),
    }
}

/// Reads the next record and checks that it is `seq`.
fn expect_next(reader: &mut Reader, seq: u64) -> Result<()> {
    let found = reader
        .try_next()?
        .ok_or("no record at the seek target")?
        .seq;
    if found != seq {
        return Err(format!("seek to {seq} returned {found}").into());
    }
    Ok(())
}

/// Opens a reader (timed apart as `reader_open_s`) and times `f` on it. A seek itself only opens the segment; the
/// records before the target in it are read by the first `try_next`, so `f` reads one record too.
fn seek_op(op: &Op, f: impl FnOnce(&mut Reader) -> Result<Value>) -> Result<Value> {
    let open = Instant::now();
    let config = with_index(
        ReaderConfig::default(),
        op.seek_index || op.name == "seek-sealed-indexed",
    )?;
    let mut reader = op.reader.open(&op.dir, config)?;
    let open_s = open.elapsed().as_secs_f64();
    let mut out = timed(|| f(&mut reader))?;
    out["reader_open_s"] = json!(open_s);
    out["used_index"] = used_index(&reader);
    Ok(out)
}

/// `config`, for a reader that may use seek indexes if `on` (1.1).
#[cfg(feature = "core-1-1")]
pub(crate) fn with_index(config: ReaderConfig, on: bool) -> Result<ReaderConfig> {
    Ok(config.seek_index(on))
}

#[cfg(not(feature = "core-1-1"))]
pub(crate) fn with_index(config: ReaderConfig, on: bool) -> Result<ReaderConfig> {
    if on {
        return Err("not measured: this core has no seek index".into());
    }
    Ok(config)
}

/// Whether the reader's last seek started at an indexed record.
#[cfg(feature = "core-1-1")]
pub(crate) fn used_index(reader: &Reader) -> Value {
    json!(reader.seek_used_index())
}

#[cfg(not(feature = "core-1-1"))]
pub(crate) fn used_index(_: &Reader) -> Value {
    Value::Null
}

/// `index-build` (every index anew), `index-refresh` (what is missing or outdated) and `index-check`: the states of the
/// segments, counted, and the size of the index files.
#[cfg(feature = "core-1-1")]
pub(crate) fn index(dir: &Path, name: &str) -> Result<Value> {
    use tickertape::seek_index;
    let segments = if name == "index-check" {
        seek_index::check(dir)?.segments
    } else {
        seek_index::build(dir, name == "index-build")?.segments
    };
    let mut states = std::collections::BTreeMap::<String, u64>::new();
    let mut reasons = Vec::new();
    for segment in &segments {
        *states.entry(format!("{:?}", segment.state)).or_default() += 1;
        if let (Some(reason), true) = (&segment.reason, reasons.len() < 5) {
            reasons.push(format!(
                "{}: {:?}: {reason}",
                segment.base_seq, segment.state
            ));
        }
    }
    let (mut files, mut bytes) = (0u64, 0u64);
    if let Ok(entries) = std::fs::read_dir(dir.join(seek_index::DIR_NAME)) {
        for entry in entries {
            files += 1;
            bytes += entry?.metadata()?.len();
        }
    }
    Ok(
        json!({"segments": segments.len(), "states": states, "reasons": reasons,
        "built": states.get("Built").copied().unwrap_or(0), "index_files": files, "index_bytes": bytes}),
    )
}

#[cfg(not(feature = "core-1-1"))]
pub(crate) fn index(_: &Path, _: &str) -> Result<Value> {
    Err("not measured: this core has no seek index".into())
}

/// Seeks to the first record of a full segment and then forward through it in equal steps, reading one record after
/// each: what a reader that samples a segment pays. From 1.1 such seeks can continue from the cursor (ADR 0046); the
/// first one after the seek to the segment's first record already can.
fn seek_forward(op: &Op) -> Result<Value> {
    let base = op.target()?;
    let end = op.span_end.ok_or("seek-forward needs --span-end")?;
    seek_op(op, |reader| {
        reader.seek(base)?;
        expect_next(reader, base)?;
        let mut steps = Vec::new();
        for i in 1..=FORWARD_STEPS {
            let seq = base + (end - 1 - base) * i / FORWARD_STEPS;
            let start = Instant::now();
            reader.seek(seq)?;
            expect_next(reader, seq)?;
            steps.push(start.elapsed().as_secs_f64());
        }
        Ok(
            json!({"seq": base, "span_end": end, "steps": FORWARD_STEPS, "steps_s": steps,
            "steps_total_s": steps.iter().sum::<f64>()}),
        )
    })
}

/// `Reader::seek_to_end` (1.1) and one `try_next`, which finds nothing on a journal without a writer.
#[cfg(feature = "core-1-1")]
fn seek_to_end(op: &Op) -> Result<Value> {
    seek_op(op, |reader| {
        reader.seek_to_end()?;
        let position = reader.position();
        Ok(json!({"position": position, "more": reader.try_next()?.is_some()}))
    })
}

#[cfg(not(feature = "core-1-1"))]
fn seek_to_end(_: &Op) -> Result<Value> {
    Ok(json!({"error": "not measured: this core has no Reader::seek_to_end"}))
}

/// Reads from the oldest record to `op.end`, or to where the reader stops. An error ends it and is part of the result.
fn replay(op: &Op) -> Result<Value> {
    let mut out = timed(|| {
        let mut reader = op.reader.open(&op.dir, ReaderConfig::default())?;
        let (mut records, mut payload_bytes, mut last) = (0u64, 0u64, None);
        let mut error = None;
        while op.end.is_none_or(|end| reader.position() < end) {
            match reader.try_next() {
                Ok(Some(record)) => {
                    records += 1;
                    payload_bytes += record.payload.len() as u64;
                    last = Some(record.seq);
                }
                Ok(None) => break,
                Err(e) => {
                    error = Some(e.to_string());
                    break;
                }
            }
        }
        let mut out = json!({"records": records, "payload_bytes": payload_bytes, "last_seq": last});
        if let Some(error) = error {
            out["error"] = json!(error);
        }
        Ok(out)
    })?;
    let wall = out["wall_s"].as_f64().unwrap_or(0.0);
    if wall > 0.0 && out.get("records").is_some() {
        out["records_per_s"] = json!(out["records"].as_f64().unwrap_or(0.0) / wall);
        out["payload_bytes_per_s"] = json!(out["payload_bytes"].as_f64().unwrap_or(0.0) / wall);
    }
    Ok(out)
}

fn writer_open(op: &Op) -> Result<Value> {
    let dir = op.dir.as_path();
    let saved = dir.join(SAVED_MARKER);
    if op.name == "writer-open-without-marker" {
        std::fs::rename(dir.join("checkpoint"), &saved)?;
    }
    let mut writer = None;
    let mut out = timed(|| {
        let w = Writer::open(dir, WriterConfig::default().segment_size(op.segment_size))?;
        let report = w.recovery_report();
        let out = json!({"next_seq": report.next_seq, "scanned_from_seq": report.scanned_from_seq,
            "had_checkpoint": report.had_checkpoint, "recovery_actions": report.actions.len()});
        writer = Some(w);
        Ok(out)
    })?;
    out["open_files_held"] = json!(open_files());
    if let Some(mut writer) = writer {
        let start = Instant::now();
        writer.checkpoint()?;
        out["checkpoint_after_s"] = json!(start.elapsed().as_secs_f64());
        writer.close()?;
        if saved.exists() {
            std::fs::remove_file(&saved)?;
        }
    } else if saved.exists() {
        // Put the marker back; the open changed nothing it could not repeat.
        std::fs::rename(&saved, dir.join("checkpoint"))?;
    }
    Ok(out)
}

/// Opens a writer (not timed) and takes `op.count` blocking checkpoints, each after one record of the journal's
/// payload size, without rolling on purpose: what a checkpoint costs in a directory this large. With retention
/// configured (a limit no journal reaches), every checkpoint also makes a retention plan.
fn checkpoint_loop(op: &Op) -> Result<Value> {
    let payload = vec![0x5a; payload_size(&op.dir)?];
    let mut config = WriterConfig::default().segment_size(op.segment_size);
    if op.name == "checkpoint-loop-retention" {
        config = config.retention(
            retention::Policy::default().max_segments(usize::MAX),
            retention::Action::Delete,
        );
    }
    let open = Instant::now();
    let mut writer = Writer::open(&op.dir, config)?;
    let open_s = open.elapsed().as_secs_f64();
    let mut latency = histogram()?;
    let mut out = timed(|| {
        for _ in 0..op.count {
            let index = writer.next_seq();
            writer.append(&record(&payload, index))?;
            let start = Instant::now();
            writer.checkpoint()?;
            latency.saturating_record(elapsed_ns(start.elapsed())?);
        }
        Ok(json!({"checkpoints": op.count}))
    })?;
    out["checkpoint_latency"] = histogram_json(&latency);
    out["writer_open_s"] = json!(open_s);
    out["retention_error"] = json!(writer.last_retention_error().map(ToString::to_string));
    out["open_files_held"] = json!(open_files());
    writer.close()?;
    Ok(out)
}

/// Runs the `tickertape` program on the journal and reports its cost (wall time here, CPU time, faults and peak RSS
/// from `getrusage(RUSAGE_CHILDREN)`), exit status and first line of standard error.
fn cli(op: &Op) -> Result<Value> {
    let program = op.cli.as_ref().ok_or("cli operations need --cli")?;
    let mut command = Command::new(program);
    let bound = match (op.name.as_str(), op.end) {
        ("cli-verify", _) => {
            command.arg("verify").arg(&op.dir);
            None
        }
        (name, end) => {
            command
                .arg(if name == "cli-export" {
                    "export"
                } else {
                    "stats"
                })
                .arg(&op.dir);
            if name == "cli-export" {
                command.args(["--format", "jsonl", "--output", "-"]);
            }
            end.map(|end| end.saturating_sub(1))
        }
    };
    if let Some(to) = bound {
        command.args(["--to", &to.to_string()]);
    }
    let start = Instant::now();
    let output = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    let wall_s = start.elapsed().as_secs_f64();
    let usage = getrusage(UsageWho::RUSAGE_CHILDREN)?;
    let seconds = |t| micros(t) as f64 / 1e6;
    let rss_unit: i64 = if cfg!(target_vendor = "apple") {
        1
    } else {
        1024
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(
        json!({"wall_s": wall_s, "user_cpu_s": seconds(usage.user_time()),
        "system_cpu_s": seconds(usage.system_time()), "minor_faults": usage.minor_page_faults(),
        "major_faults": usage.major_page_faults(), "peak_rss_bytes": usage.max_rss() * rss_unit,
        "exit_code": output.status.code(), "to": bound, "stderr": stderr.lines().next()}),
    )
}

/// Opens a writer on the journal in `dir` and appends `records` records of `payload` bytes without a checkpoint,
/// returning the writer still open: the caller kills the process to leave the tail for recovery.
pub fn write_tail(dir: &Path, records: u64, payload: usize, segment_size: u64) -> Result<Writer> {
    let mut writer = Writer::open(dir, WriterConfig::default().segment_size(segment_size))?;
    let bytes = vec![0x5a; payload];
    for _ in 0..records {
        let index = writer.next_seq();
        writer.append(&record(&bytes, index))?;
    }
    Ok(writer)
}

/// The payload size of the journal's first record (every generated record has the same size).
pub fn payload_size(dir: &Path) -> Result<usize> {
    let mut reader = Reader::open(dir, ReaderConfig::default())?;
    Ok(reader
        .try_next()?
        .ok_or("the journal holds no records")?
        .payload
        .len())
}

/// Drops the page cache with `sudo -n` (Linux: `sync` then `drop_caches`; macOS: `purge`). `false` if that is not
/// possible here (no password-less sudo, another system).
pub fn drop_caches() -> bool {
    let mut command = std::process::Command::new("sudo");
    command.arg("-n");
    if cfg!(target_os = "linux") {
        command.args(["sh", "-c", "sync && echo 3 > /proc/sys/vm/drop_caches"]);
    } else if cfg!(target_os = "macos") {
        command.arg("purge");
    } else {
        return false;
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The memory limit of this process's cgroup (Linux cgroup v2 `memory.max`), if one is set. Page cache counts
/// against it, so a limit below the journal's size makes the journal larger than the memory the run may use.
pub fn cgroup_memory_limit() -> Option<u64> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = cgroup.lines().find_map(|l| l.strip_prefix("0::"))?;
    let mut file = PathBuf::from("/sys/fs/cgroup");
    file.push(path.trim_start_matches('/'));
    file.push("memory.max");
    std::fs::read_to_string(file).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: u64 = 65_536;

    #[test]
    fn builds_the_requested_shape_and_runs_every_read_operation() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("journal");
        let built = build(&dir, 3, SIZE, 100, 2, 100).unwrap();
        let journal = Journal::describe(&dir, None).unwrap();
        assert_eq!(journal.bases.len(), 3);
        assert_eq!(journal.bytes, 3 * SIZE);
        assert_eq!(journal.next_seq, built["records"].as_u64().unwrap());
        assert_eq!(
            journal.bases[1],
            built["records_per_segment"].as_u64().unwrap()
        );
        assert_eq!(payload_size(&dir).unwrap(), 100);
        let indexes = run_op(&Op::new(&dir, "index-build", None, SIZE)).unwrap();
        for op in [
            "seek-first",
            "seek-middle",
            "seek-last",
            "seek-sealed",
            "seek-sealed-indexed",
        ] {
            let seq = journal.seek_target(op).unwrap();
            let out = run_op(&Op::new(&dir, op, Some(seq), SIZE)).unwrap();
            assert_eq!(out["seq"], seq);
            let indexed = op == "seek-sealed-indexed" && indexes["built"] != 0;
            assert_eq!(out["used_index"], indexed, "{op}");
            assert!(out["wall_s"].as_f64().unwrap() >= 0.0);
            assert!(out["peak_rss_bytes"].as_i64().unwrap() > 0);
        }
        assert_eq!(journal.segment_of(journal.next_seq - 1), journal.bases[2]);
        assert_eq!(journal.segment_of(0), 0);
        let replay = run_op(&Op::new(&dir, "replay", None, SIZE)).unwrap();
        assert_eq!(replay["records"], journal.next_seq);
        assert_eq!(replay["last_seq"], journal.next_seq - 1);
        let verify = run_op(&Op::new(&dir, "verify", None, SIZE)).unwrap();
        assert_eq!(verify["problems"], 0);
        assert_eq!(verify["records"], journal.next_seq);
        assert_eq!(
            run_op(&Op::new(&dir, "list-segments", None, SIZE)).unwrap()["segments"],
            3
        );
        assert_eq!(
            run_op(&Op::new(&dir, "retention-plan", None, SIZE)).unwrap()["planned_segments"],
            0
        );
        assert!(run_op(&Op::new(&dir, "seek-last", None, SIZE)).is_err());
        assert!(run_op(&Op::new(&dir, "wat", None, SIZE)).is_err());
        // A second build into the same directory is refused and leaves the journal alone.
        assert!(build(&dir, 1, SIZE, 100, 0, 100).is_err());
        assert_eq!(
            Journal::describe(&dir, None).unwrap().next_seq,
            journal.next_seq
        );
    }

    #[test]
    fn writer_operations_recover_and_leave_a_checkpoint() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("journal");
        let built = build(&dir, 2, SIZE, 64, 0, 50).unwrap();
        let capacity = built["records_per_segment"].as_u64().unwrap();
        assert_eq!(built["records"], capacity + capacity / 2);
        assert_eq!(Journal::describe(&dir, None).unwrap().bases, [0, capacity]);
        for fill in [0, 101] {
            assert!(
                build(
                    &root.path().join(format!("bad{fill}")),
                    1,
                    SIZE,
                    64,
                    0,
                    fill
                )
                .is_err()
            );
        }
        let before = Journal::describe(&dir, None).unwrap();
        let clean = run_op(&Op::new(&dir, "writer-open-clean", None, SIZE)).unwrap();
        assert_eq!(clean["had_checkpoint"], true);
        assert_eq!(clean["scanned_from_seq"], before.next_seq);
        assert_eq!(clean["next_seq"], before.next_seq);
        // A tail without a checkpoint, as a killed writer leaves it (dropping without a checkpoint leaves the
        // same committed, unrecorded records).
        drop(write_tail(&dir, 10, 64, SIZE).unwrap());
        let killed = run_op(&Op::new(&dir, "writer-open-after-kill", None, SIZE)).unwrap();
        assert_eq!(killed["scanned_from_seq"], before.next_seq);
        assert_eq!(killed["next_seq"], before.next_seq + 10);
        assert_eq!(
            Journal::describe(&dir, None).unwrap().next_seq,
            before.next_seq + 10
        );
        let unmarked = run_op(&Op::new(&dir, "writer-open-without-marker", None, SIZE)).unwrap();
        assert_eq!(unmarked["had_checkpoint"], false);
        assert_eq!(unmarked["scanned_from_seq"], 0);
        assert_eq!(unmarked["next_seq"], before.next_seq + 10);
        assert!(!dir.join(SAVED_MARKER).exists());
        assert_eq!(
            Journal::describe(&dir, None).unwrap().next_seq,
            before.next_seq + 10
        );
    }
}
