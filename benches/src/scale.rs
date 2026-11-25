//! Building blocks for `tt-scale`: generating a journal of a given shape, and the operations it times (startup,
//! seek, replay, recovery, verification and listing) with their CPU time, page faults and peak RSS.
//!
//! Everything here is safe Rust: the safe reader (`Reader::open`, `pread`) only, and `getrusage` through `nix`.
//! Each operation is meant to run in a process of its own (the binary starts one per operation), so that the peak
//! RSS it reports belongs to that operation.

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{Value, json};
use tickertape::inspect;
use tickertape::{NewRecord, Reader, ReaderConfig, Writer, WriterConfig};
use tickertape::{retention, seek_index};

use crate::{Result, fresh_directory, segment_capacity};

/// Name the checkpoint marker is moved to while `writer-open-without-marker` runs.
pub const SAVED_MARKER: &str = "checkpoint.tt-scale-saved";

/// Every operation, in the order `measure` runs them. Writer operations come last because they change the journal:
/// `writer-open-after-kill` appends a tail first, and every writer operation leaves a checkpoint at the end. The seek
/// index operations (ADR 0045) come after them, so that every other operation runs without indexes.
pub const OPS: &[&str] = &[
    "list-segments",
    "reader-open",
    "seek-first",
    "seek-middle",
    "seek-last",
    "seek-sealed",
    "replay",
    "verify",
    "retention-plan",
    "writer-open-clean",
    "writer-open-after-kill",
    "writer-open-without-marker",
    "index-build",
    "seek-sealed-indexed",
];

/// Process resource usage at one instant (`getrusage(RUSAGE_SELF)`).
#[derive(Debug, Clone, Copy)]
struct Usage {
    wall: Instant,
    user_us: i64,
    system_us: i64,
    minor_faults: i64,
    major_faults: i64,
    max_rss_bytes: i64,
}

impl Usage {
    // The libc field types (`time_t`, `suseconds_t`, `c_long`) differ between systems.
    #[allow(clippy::useless_conversion)]
    fn now() -> Result<Usage> {
        let usage = nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_SELF)?;
        let micros =
            |t: nix::sys::time::TimeVal| i64::from(t.tv_sec()) * 1_000_000 + i64::from(t.tv_usec());
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

/// Resets the peak RSS to the current RSS (Linux: writing 5 to `/proc/self/clear_refs`), so that the peak read after
/// an operation is the operation's own. `false` where that is not possible (other systems, older kernels).
fn reset_peak_rss() -> bool {
    std::fs::write("/proc/self/clear_refs", "5").is_ok()
}

/// Runs `f` and returns its result with its cost merged in.
fn timed(f: impl FnOnce() -> Result<Value>) -> Result<Value> {
    let reset = reset_peak_rss();
    let before = Usage::now()?;
    let extra = f()?;
    let after = Usage::now()?;
    let mut out = before.json(&after);
    if let (Value::Object(out), Value::Object(extra)) = (&mut out, extra) {
        out.extend(extra);
    }
    out["peak_rss_reset"] = json!(reset);
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
    writer.checkpoint()?;
    writer.close()?;
    let seconds = start.elapsed().as_secs_f64();
    let journal_bytes = segments * segment_size;
    Ok(json!({
        "segments": segments, "segment_size": segment_size, "payload": payload, "records": records,
        "records_per_segment": capacity, "records_in_last_segment": last, "fill_last": fill_last,
        "journal_bytes": journal_bytes, "checkpoint_every": checkpoint_every,
        "seconds": seconds, "bytes_per_s": journal_bytes as f64 / seconds,
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
    /// The checkpoint marker's `next_seq`: one past the last record of a journal `build` or a writer operation left.
    pub next_seq: u64,
}

impl Journal {
    /// Reads the shape of the journal in `dir`. It must have a checkpoint marker at a record (as `build` leaves it).
    pub fn describe(dir: &Path) -> Result<Journal> {
        let segments = inspect::list_segments(dir)?;
        let checkpoint = inspect::read_checkpoint(dir)?
            .ok_or("the journal has no checkpoint marker; build it with `tt-scale build`")?;
        let first_seq = segments.first().map_or(0, |s| s.base_seq);
        if checkpoint.next_seq <= first_seq {
            return Err("the journal holds no records".into());
        }
        Ok(Journal {
            bases: segments.iter().map(|s| s.base_seq).collect(),
            bytes: segments.iter().map(|s| s.segment_size).sum(),
            segment_size: segments.last().map_or(0, |s| s.segment_size),
            first_seq,
            next_seq: checkpoint.next_seq,
        })
    }

    /// The sequence number a seek operation targets: the first, middle or last record, or the last one of the newest
    /// sealed segment (of the newest segment, if it is the only one).
    pub fn seek_target(&self, op: &str) -> Option<u64> {
        match op {
            "seek-first" => Some(self.first_seq),
            "seek-middle" => Some(self.first_seq + (self.next_seq - self.first_seq) / 2),
            "seek-last" => Some(self.next_seq - 1),
            "seek-sealed" | "seek-sealed-indexed" => Some(match self.bases[..] {
                [.., newest] if self.bases.len() > 1 => newest - 1,
                _ => self.next_seq - 1,
            }),
            _ => None,
        }
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

/// Runs one operation in this process and returns its cost and what it found. `seq` is the target of a seek
/// operation. Writer operations use `segment_size` for any new segment and leave a checkpoint marker at the end.
/// `writer-open-after-kill` times only the open: the caller must first leave a tail behind a killed writer.
pub fn run_op(dir: &Path, op: &str, seq: Option<u64>, segment_size: u64) -> Result<Value> {
    let writer_config = WriterConfig::default().segment_size(segment_size);
    let mut result = match op {
        "list-segments" => timed(|| {
            let segments = inspect::list_segments(dir)?;
            Ok(json!({"segments": segments.len()}))
        })?,
        "reader-open" => timed(|| {
            let reader = Reader::open(dir, ReaderConfig::default())?;
            Ok(json!({"position": reader.position()}))
        })?,
        "seek-first" | "seek-middle" | "seek-last" | "seek-sealed" | "seek-sealed-indexed" => {
            let seq = seq.ok_or("a seek operation needs --seq")?;
            let open = Instant::now();
            let indexed = op == "seek-sealed-indexed";
            let mut reader = Reader::open(dir, ReaderConfig::default().seek_index(indexed))?;
            let open_s = open.elapsed().as_secs_f64();
            // The seek itself opens the segment; the records before `seq` in it are read by the first `try_next`.
            let mut out = timed(|| {
                reader.seek(seq)?;
                let found = reader
                    .try_next()?
                    .ok_or("no record at the seek target")?
                    .seq;
                if found != seq {
                    return Err(format!("seek to {seq} returned {found}").into());
                }
                Ok(json!({"seq": seq}))
            })?;
            out["reader_open_s"] = json!(open_s);
            out["used_index"] = json!(reader.seek_used_index());
            out
        }
        "replay" => timed(|| {
            let mut reader = Reader::open(dir, ReaderConfig::default())?;
            let (mut records, mut payload_bytes, mut last) = (0u64, 0u64, None);
            while let Some(record) = reader.try_next()? {
                records += 1;
                payload_bytes += record.payload.len() as u64;
                last = Some(record.seq);
            }
            Ok(json!({"records": records, "payload_bytes": payload_bytes, "last_seq": last}))
        })
        .map(|mut out| {
            let wall = out["wall_s"].as_f64().unwrap_or(0.0);
            if wall > 0.0 {
                out["records_per_s"] = json!(out["records"].as_f64().unwrap_or(0.0) / wall);
                out["payload_bytes_per_s"] =
                    json!(out["payload_bytes"].as_f64().unwrap_or(0.0) / wall);
            }
            out
        })?,
        "verify" => timed(|| {
            let report = inspect::verify(dir)?;
            Ok(
                json!({"records": report.records, "segments": report.segments.len(),
                "problems": report.problems.len()}),
            )
        })?,
        "index-build" => timed(|| {
            let report = seek_index::build(dir, true)?;
            let built = report.segments.iter();
            let built = built.filter(|s| s.state == seek_index::IndexState::Built);
            let mut bytes = 0;
            for entry in std::fs::read_dir(dir.join(seek_index::DIR_NAME))
                .into_iter()
                .flatten()
            {
                bytes += entry?.metadata()?.len();
            }
            Ok(json!({"built": built.count(), "index_bytes": bytes}))
        })?,
        "retention-plan" => timed(|| {
            // A plan only, under a limit no journal reaches: what every retention run pays before it removes
            // anything (the writer makes a plan at open and after checkpoints when retention is configured).
            // Removing a segment costs more: the plan scans each candidate for its seal.
            let plan =
                retention::plan(dir, &retention::Policy::default().max_segments(usize::MAX))?;
            Ok(json!({"planned_segments": plan.segments().len()}))
        })?,
        "writer-open-clean" | "writer-open-after-kill" | "writer-open-without-marker" => {
            let saved = dir.join(SAVED_MARKER);
            if op == "writer-open-without-marker" {
                std::fs::rename(dir.join("checkpoint"), &saved)?;
            }
            let mut writer = None;
            let opened = timed(|| {
                let w = Writer::open(dir, writer_config)?;
                let report = w.recovery_report();
                let out = json!({"next_seq": report.next_seq, "scanned_from_seq": report.scanned_from_seq,
                    "had_checkpoint": report.had_checkpoint, "recovery_actions": report.actions.len()});
                writer = Some(w);
                Ok(out)
            });
            let out = match opened {
                Ok(out) => out,
                Err(e) => {
                    if saved.exists() {
                        // Put the marker back; the open changed nothing it could not repeat.
                        std::fs::rename(&saved, dir.join("checkpoint"))?;
                    }
                    return Err(e);
                }
            };
            let mut writer = writer.ok_or("writer missing after open")?;
            writer.checkpoint()?;
            writer.close()?;
            if saved.exists() {
                std::fs::remove_file(&saved)?;
            }
            out
        }
        _ => return Err(format!("unknown operation {op}").into()),
    };
    result["op"] = json!(op);
    Ok(result)
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
        let journal = Journal::describe(&dir).unwrap();
        assert_eq!(journal.bases.len(), 3);
        assert_eq!(journal.bytes, 3 * SIZE);
        assert_eq!(journal.next_seq, built["records"].as_u64().unwrap());
        assert_eq!(
            journal.bases[1],
            built["records_per_segment"].as_u64().unwrap()
        );
        assert_eq!(payload_size(&dir).unwrap(), 100);
        let indexes = run_op(&dir, "index-build", None, SIZE).unwrap();
        for op in [
            "seek-first",
            "seek-middle",
            "seek-last",
            "seek-sealed",
            "seek-sealed-indexed",
        ] {
            let seq = journal.seek_target(op).unwrap();
            let out = run_op(&dir, op, Some(seq), SIZE).unwrap();
            assert_eq!(out["seq"], seq);
            let indexed = op == "seek-sealed-indexed" && indexes["built"] != 0;
            assert_eq!(out["used_index"], indexed, "{op}");
            assert!(out["wall_s"].as_f64().unwrap() >= 0.0);
            assert!(out["peak_rss_bytes"].as_i64().unwrap() > 0);
        }
        assert_eq!(journal.segment_of(journal.next_seq - 1), journal.bases[2]);
        assert_eq!(journal.segment_of(0), 0);
        let replay = run_op(&dir, "replay", None, SIZE).unwrap();
        assert_eq!(replay["records"], journal.next_seq);
        assert_eq!(replay["last_seq"], journal.next_seq - 1);
        let verify = run_op(&dir, "verify", None, SIZE).unwrap();
        assert_eq!(verify["problems"], 0);
        assert_eq!(verify["records"], journal.next_seq);
        assert_eq!(
            run_op(&dir, "list-segments", None, SIZE).unwrap()["segments"],
            3
        );
        assert_eq!(
            run_op(&dir, "retention-plan", None, SIZE).unwrap()["planned_segments"],
            0
        );
        assert!(run_op(&dir, "seek-last", None, SIZE).is_err());
        assert!(run_op(&dir, "wat", None, SIZE).is_err());
        // A second build into the same directory is refused and leaves the journal alone.
        assert!(build(&dir, 1, SIZE, 100, 0, 100).is_err());
        assert_eq!(Journal::describe(&dir).unwrap().next_seq, journal.next_seq);
    }

    #[test]
    fn writer_operations_recover_and_leave_a_checkpoint() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("journal");
        let built = build(&dir, 2, SIZE, 64, 0, 50).unwrap();
        let capacity = built["records_per_segment"].as_u64().unwrap();
        assert_eq!(built["records"], capacity + capacity / 2);
        assert_eq!(Journal::describe(&dir).unwrap().bases, [0, capacity]);
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
        let before = Journal::describe(&dir).unwrap();
        let clean = run_op(&dir, "writer-open-clean", None, SIZE).unwrap();
        assert_eq!(clean["had_checkpoint"], true);
        assert_eq!(clean["scanned_from_seq"], before.next_seq);
        assert_eq!(clean["next_seq"], before.next_seq);
        // A tail without a checkpoint, as a killed writer leaves it (dropping without a checkpoint leaves the
        // same committed, unrecorded records).
        drop(write_tail(&dir, 10, 64, SIZE).unwrap());
        let killed = run_op(&dir, "writer-open-after-kill", None, SIZE).unwrap();
        assert_eq!(killed["scanned_from_seq"], before.next_seq);
        assert_eq!(killed["next_seq"], before.next_seq + 10);
        assert_eq!(
            Journal::describe(&dir).unwrap().next_seq,
            before.next_seq + 10
        );
        let unmarked = run_op(&dir, "writer-open-without-marker", None, SIZE).unwrap();
        assert_eq!(unmarked["had_checkpoint"], false);
        assert_eq!(unmarked["scanned_from_seq"], 0);
        assert_eq!(unmarked["next_seq"], before.next_seq + 10);
        assert!(!dir.join(SAVED_MARKER).exists());
        assert_eq!(
            Journal::describe(&dir).unwrap().next_seq,
            before.next_seq + 10
        );
    }
}
