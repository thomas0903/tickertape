//! Soak tier: the failure modes of `docs/crash-contract.md` combined, for a configurable time.
//!
//! A writer process (`tt-soak-writer`) appends at varying rates with periodic checkpoints, segment rolls,
//! retention (moving old segments to an archive directory), injected storage-full roll failures (C9) and the
//! occasional injected checkpoint failure (C12). It is killed with SIGKILL at random points, including during
//! recovery (C1–C4), or stopped cleanly, and restarted. Between restarts the test sometimes damages the journal
//! after the checkpoint (a flipped record byte, a truncated or damaged newest segment, or a damaged middle segment)
//! and checks that recovery quarantines exactly what the crash contract says, byte for byte, or refuses without
//! changing anything (C8, C11). Reader processes of both kinds (safe and mapped, visible and durable-only) follow the
//! journal the whole time and must each see a gap-free, duplicate-free stream of records that pass their checksum and
//! content checks, consistent with the history the test reconstructs; a slow one is overtaken by retention and must
//! get `Error::Discarded`/`SeqUnavailable`, never garbage (C6, C13, C14). Late readers replay from the oldest segment.
//!
//! - Duration: `TICKERTAPE_SOAK_SECS` (default 20). Seed: `TICKERTAPE_TEST_SEED` (printed; the same seed and duration
//!   replay the same decisions, though process timing still varies).
//! - Disk use is bounded (retention, and the test deletes archived and quarantined files after checking them); the
//!   run fails if the journal, archive and quarantine together exceed 64 MiB.
//! - On failure the journal, archive, logs and the seed are copied to `TICKERTAPE_SOAK_ARTIFACTS` (default
//!   `target/tmp/soak-failures/`), and the panic message names the directory.
//!
//! Run: `TICKERTAPE_SOAK_SECS=1800 cargo test -p tickertape-tests --release --test soak -- --ignored --nocapture`.
//! Corruption is injected by renaming a modified copy over a segment, never by writing into a file that a mapped
//! reader may have mapped (`Reader::open_mapped`'s safety contract).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use tickertape::format::{self, CheckpointSlot, Slot};
use tickertape::{Reader, ReaderConfig, inspect};
use tickertape_tests::{Rng, segments, soak};

const WRITER: &str = env!("CARGO_BIN_EXE_tt-soak-writer");
const READER: &str = env!("CARGO_BIN_EXE_tt-soak-reader");
const DISK_LIMIT: u64 = 64 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(120);

/// The history the test reconstructs: which writer incarnation ("epoch") wrote each sequence number, and the
/// records recovery removed after injected corruption (whose numbers later writers reused).
#[derive(Debug, Default)]
struct Truth {
    /// `(first_seq, epoch)`: records from `first_seq` up to the next entry's `first_seq` belong to `epoch`.
    ranges: Vec<(u64, u32)>,
    /// `(first, end, epoch)`: records `first..end` of `epoch` that recovery removed.
    discarded: Vec<(u64, u64, u32)>,
    /// `(base, end)`: sequence ranges of segments retention moved to the archive.
    archived: Vec<(u64, u64)>,
}

impl Truth {
    fn epoch_at(&self, seq: u64) -> Option<u32> {
        self.ranges
            .iter()
            .rev()
            .find(|(start, _)| *start <= seq)
            .map(|r| r.1)
    }

    fn start(&mut self, at: u64, epoch: u32) {
        self.ranges.retain(|r| r.0 < at);
        self.ranges.push((at, epoch));
    }

    fn discard(&mut self, from: u64, end: u64) {
        for (i, &(start, epoch)) in self.ranges.iter().enumerate() {
            let next = self.ranges.get(i + 1).map_or(u64::MAX, |r| r.0);
            let (a, b) = (start.max(from), next.min(end));
            if a < b {
                self.discarded.push((a, b, epoch));
            }
        }
        self.ranges.retain(|r| r.0 < from);
    }

    /// Splits `first..end` into pieces by the epoch the history assigns them.
    fn pieces(&self, first: u64, end: u64) -> Vec<(u64, u64, Option<u32>)> {
        let mut out = Vec::new();
        let mut at = first;
        while at < end {
            let i = self.ranges.iter().rposition(|r| r.0 <= at);
            let next = match i {
                Some(i) => self.ranges.get(i + 1).map_or(u64::MAX, |r| r.0),
                None => self.ranges.first().map_or(u64::MAX, |r| r.0),
            };
            let b = next.min(end);
            out.push((at, b, i.map(|i| self.ranges[i].1)));
            at = b;
        }
        out
    }

    fn covered(list: &[(u64, u64)], first: u64, end: u64) -> bool {
        let mut at = first;
        let mut sorted = list.to_vec();
        sorted.sort();
        for (a, b) in sorted {
            if a <= at && b > at {
                at = b;
            }
        }
        at >= end
    }

    fn is_discarded(&self, first: u64, end: u64, epoch: u32) -> bool {
        let list: Vec<(u64, u64)> = self
            .discarded
            .iter()
            .filter(|d| d.2 == epoch)
            .map(|d| (d.0, d.1))
            .collect();
        Self::covered(&list, first, end)
    }
}

/// A record found by walking the chain.
#[derive(Debug, Clone)]
struct ChainRecord {
    path: PathBuf,
    base: u64,
    offset: u64,
    seq: u64,
    len: u32,
}

#[derive(Debug)]
struct Chain {
    /// Sequence number after the last committed record.
    end: u64,
    checkpoint: Option<CheckpointSlot>,
    /// Segments visited from the scan start, in order.
    visited: Vec<(u64, PathBuf)>,
    /// Records from the scan start (the checkpoint, if any) to the end.
    uncovered: Vec<ChainRecord>,
}

/// Walks the chain the way recovery does, from the checkpoint (or the oldest segment), without changing anything.
/// Any invalid record is an error: a process kill must never leave one (C1).
fn walk(dir: &Path) -> Result<Chain, String> {
    if !dir.exists() || segments(dir).is_empty() {
        // Not created yet (the first writer was killed early).
        return Ok(Chain {
            end: 0,
            checkpoint: None,
            visited: Vec::new(),
            uncovered: Vec::new(),
        });
    }
    let checkpoint = inspect::read_checkpoint(dir).map_err(|e| e.to_string())?;
    let all = segments(dir);
    let bases: BTreeSet<u64> = all.iter().map(|s| s.0).collect();
    let Some(&(oldest, _)) = all.first() else {
        return Err("no segments".into());
    };
    let (mut base, mut offset, mut seq) = checkpoint
        .map_or((oldest, format::SEGMENT_HEADER_LEN as u64, oldest), |c| {
            (c.segment_base_seq, c.offset, c.next_seq)
        });
    let mut chain = Chain {
        end: seq,
        checkpoint,
        visited: Vec::new(),
        uncovered: Vec::new(),
    };
    loop {
        let path = dir.join(format::segment_file_name(base));
        let image = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let header = image
            .get(..64)
            .and_then(|h| format::decode_segment_header(h.try_into().unwrap()).ok())
            .ok_or_else(|| format!("{}: invalid header", path.display()))?;
        if header.segment_size != image.len() as u64 || header.base_seq != base {
            return Err(format!(
                "{}: header disagrees with the file",
                path.display()
            ));
        }
        chain.visited.push((base, path.clone()));
        loop {
            match format::read_slot(&image[..], offset, seq) {
                Ok(Slot::Record {
                    len, next_offset, ..
                }) => {
                    chain.uncovered.push(ChainRecord {
                        path: path.clone(),
                        base,
                        offset,
                        seq,
                        len,
                    });
                    offset = next_offset;
                    seq += 1;
                }
                Ok(Slot::Seal) if bases.contains(&seq) => {
                    base = seq;
                    offset = format::SEGMENT_HEADER_LEN as u64;
                    break;
                }
                Ok(Slot::Seal) => {
                    chain.end = seq;
                    return Ok(chain);
                }
                Ok(Slot::Empty)
                    if seq != base
                        && bases.contains(&seq)
                        && !chain.visited.iter().any(|v| v.0 == seq) =>
                {
                    // Killed between publishing the next segment and sealing this one (C4).
                    base = seq;
                    offset = format::SEGMENT_HEADER_LEN as u64;
                    break;
                }
                Ok(Slot::Empty) => {
                    chain.end = seq;
                    return Ok(chain);
                }
                Err(reason) => {
                    return Err(format!(
                        "{}: invalid slot at {offset} (expected {seq}): {reason}",
                        path.display()
                    ));
                }
            }
        }
    }
}

/// What the next writer's recovery must do after an injection.
#[derive(Debug)]
enum Expect {
    /// Recover normally: `next_seq == end`, nothing quarantined.
    Clean { end: u64 },
    /// A flipped byte in record `seq` at `offset` of `file`: repair that tail, quarantine every later segment.
    Repair {
        file: String,
        offset: u64,
        seq: u64,
        old_end: u64,
        /// Bytes `[offset, segment end)` of the damaged copy.
        tail: Vec<u8>,
        /// Later segments, with their contents.
        later: Vec<(String, Vec<u8>)>,
    },
    /// The newest segment `file` (base `seq`) truncated or with a damaged header: quarantine it, recreate it.
    Replace {
        file: String,
        seq: u64,
        old_end: u64,
        damaged: Vec<u8>,
        how: &'static str,
    },
    /// A middle segment damaged: recovery must refuse and change no segment byte; then the test restores it.
    Refuse {
        file: String,
        original: Vec<u8>,
        snapshot: BTreeMap<String, Vec<u8>>,
        end: u64,
    },
}

#[derive(Debug, Default)]
struct Counters {
    incarnations: u64,
    kills: u64,
    kills_before_ready: u64,
    clean_stops: u64,
    poisoned: u64,
    open_refusals: u64,
    checkpoints: u64,
    storage_full: u64,
    storage_full_recovered: u64,
    torn_tails_zeroed: u64,
    seals_completed: u64,
    rolls_completed: u64,
    temps_removed: u64,
    injected: BTreeMap<&'static str, u64>,
    detected_by_verify: u64,
    quarantined_files_checked: u64,
    records_discarded_by_recovery: u64,
    archived_segments: u64,
    archived_records_verified: u64,
    final_records_verified: u64,
    reader_records_verified: u64,
    reader_discarded_events: u64,
    reader_retention_skips: u64,
    reader_transient_errors: u64,
    reader_records_from_discarded_history: u64,
    late_readers: u64,
    max_disk_bytes: u64,
}

struct ReaderProc {
    name: String,
    child: Child,
    log: PathBuf,
    durable: bool,
    stop: PathBuf,
}

struct Soak {
    seed: u64,
    secs: u64,
    root: PathBuf,
    journal: PathBuf,
    archive: PathBuf,
    epoch: u32,
    truth: Truth,
    expect: Expect,
    committed_lb: u64,
    last_checkpoint: u64,
    bases_seen: BTreeSet<u64>,
    recreated_segments: u64,
    readers: Vec<ReaderProc>,
    late: Vec<ReaderProc>,
    writer: Option<Child>,
    final_end: u64,
    c: Counters,
}

impl Drop for Soak {
    fn drop(&mut self) {
        for r in self.readers.iter_mut().chain(self.late.iter_mut()) {
            let _ = r.child.kill();
            let _ = r.child.wait();
        }
        if let Some(w) = &mut self.writer {
            let _ = w.kill();
            let _ = w.wait();
        }
    }
}

fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| {
            let m = e.metadata().unwrap();
            if m.is_dir() {
                dir_bytes(&e.path())
            } else {
                m.len()
            }
        })
        .sum()
}

fn copy_dir(from: &Path, to: &Path) {
    let _ = fs::create_dir_all(to);
    let Ok(entries) = fs::read_dir(from) else {
        return;
    };
    for e in entries.filter_map(|e| e.ok()) {
        let target = to.join(e.file_name());
        if e.path().is_dir() {
            copy_dir(&e.path(), &target);
        } else {
            let _ = fs::copy(e.path(), target);
        }
    }
}

fn segment_snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    segments(dir)
        .into_iter()
        .map(|(_, p)| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read(&p).unwrap(),
            )
        })
        .collect()
}

/// Replaces `path` by a file holding `bytes`, through a rename (readers that have the old file open keep it).
fn replace_file(path: &Path, bytes: &[u8], scratch: &Path) {
    let tmp = scratch.join("inject.tmp");
    fs::write(&tmp, bytes).unwrap();
    File::open(&tmp).unwrap().sync_all().unwrap();
    fs::rename(&tmp, path).unwrap();
}

fn spawn_lines(child: &mut Child) -> (Receiver<String>, std::thread::JoinHandle<()>) {
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    (rx, handle)
}

impl Soak {
    fn spawn_reader(&mut self, name: &str, mapped: bool, durable: bool, delay_us: u64, late: bool) {
        let log = self.root.join(format!("reader-{name}.log"));
        let stop = self.root.join(if late {
            format!("stop-{name}")
        } else {
            "reader-stop".to_string()
        });
        let child = Command::new(READER)
            .arg(&self.journal)
            .arg(if mapped { "mapped" } else { "safe" })
            .arg(if durable { "durable" } else { "visible" })
            .arg(delay_us.to_string())
            .arg(&log)
            .arg(&stop)
            .stdout(Stdio::null())
            .stderr(File::create(self.root.join(format!("reader-{name}.err"))).unwrap())
            .spawn()
            .expect("spawn reader");
        let proc = ReaderProc {
            name: name.to_string(),
            child,
            log,
            durable,
            stop,
        };
        if late {
            self.late.push(proc);
        } else {
            self.readers.push(proc);
        }
    }

    fn check_readers_alive(&mut self) {
        for r in &mut self.readers {
            if let Some(status) = r.child.try_wait().unwrap() {
                let log = fs::read_to_string(&r.log).unwrap_or_default();
                let tail: Vec<&str> = log.lines().rev().take(5).collect();
                panic!(
                    "reader {} exited early ({status}); log tail: {tail:?}",
                    r.name
                );
            }
        }
        let mut finished = Vec::new();
        for (i, r) in self.late.iter_mut().enumerate() {
            if let Some(status) = r.child.try_wait().unwrap() {
                assert!(status.success(), "late reader {} failed ({status})", r.name);
                finished.push(i);
            }
        }
        for i in finished.into_iter().rev() {
            let r = self.late.remove(i);
            self.check_log(&r, None);
        }
    }

    /// Validates a reader's log against the reconstructed history.
    fn check_log(&mut self, r: &ReaderProc, final_target: Option<u64>) {
        let text = fs::read_to_string(&r.log).unwrap();
        let who = &r.name;
        let mut expected_next: Option<u64> = None;
        let mut discarded_since = false;
        let mut last_was_discarded = false;
        let mut ended = false;
        for line in text.lines() {
            let parts: Vec<&str> = line.splitn(4, ' ').collect();
            let num = |i: usize| -> u64 { parts[i].parse().unwrap() };
            match parts[0] {
                "R" => {
                    let (first, end, epoch) = (num(1), num(2), num(3) as u32);
                    if let Some(n) = expected_next {
                        assert_eq!(first, n, "reader {who}: gap or duplicate before {first}");
                    }
                    for (a, b, truth) in self.truth.pieces(first, end) {
                        let discarded = if truth == Some(epoch) {
                            false
                        } else if self.truth.is_discarded(a, b, epoch) {
                            true
                        } else {
                            panic!(
                                "reader {who}: records {a}..{b} of epoch {epoch} are in neither the history \
                                 (epoch {truth:?}) nor a range recovery discarded"
                            );
                        };
                        if discarded {
                            assert!(
                                !r.durable,
                                "durable-only reader {who} returned records {a}..{b} that recovery later removed"
                            );
                            self.c.reader_records_from_discarded_history += b - a;
                        } else {
                            assert!(
                                !(last_was_discarded && !discarded_since),
                                "reader {who}: moved from a discarded history into the current one at {a} \
                                 without Error::Discarded"
                            );
                        }
                        last_was_discarded = discarded;
                        discarded_since = false;
                    }
                    self.c.reader_records_verified += end - first;
                    expected_next = Some(end);
                }
                "D" => {
                    discarded_since = true;
                    self.c.reader_discarded_events += 1;
                }
                "S" => {
                    let (from, to) = (num(1), num(2));
                    assert!(
                        to > from && Truth::covered(&self.truth.archived, from, to),
                        "reader {who}: skipped {from}..{to}, which retention did not remove"
                    );
                    self.c.reader_retention_skips += 1;
                    expected_next = Some(to);
                }
                "C" => self.c.reader_transient_errors += 1,
                "END" => {
                    if let Some(t) = final_target {
                        // Beyond `t` only if it read records that recovery removed, then waited at their position.
                        assert!(num(1) >= t, "reader {who} ended at {} before {t}", num(1));
                    }
                    ended = true;
                }
                "E" => panic!("reader {who} failed: {line}"),
                other => panic!("reader {who}: unknown log line {other:?}"),
            }
        }
        assert!(ended, "reader {who}: log has no END line");
    }

    /// Checks and deletes the segments retention moved to the archive.
    fn drain_archive(&mut self) {
        let mut scratch = Vec::new();
        for (base, path) in segments(&self.archive) {
            let image = fs::read(&path).unwrap();
            let scan = format::scan_segment(&image[..], format::SEGMENT_HEADER_LEN as u64, base);
            assert_eq!(
                scan.end,
                format::ScanEnd::Seal,
                "archived segment {base} is not a sealed, valid segment"
            );
            let mut offset = format::SEGMENT_HEADER_LEN as u64;
            for seq in base..scan.next_seq {
                let Ok(Slot::Record {
                    header,
                    len,
                    next_offset,
                }) = format::read_slot(&image[..], offset, seq)
                else {
                    panic!("archived segment {base}: record {seq}");
                };
                let payload = &image[offset as usize + 40..(offset + u64::from(len)) as usize];
                let epoch = soak::check(
                    seq,
                    header.schema_id,
                    header.venue_ts_ns,
                    payload,
                    &mut scratch,
                )
                .unwrap_or_else(|e| panic!("archived segment {base}: {e}"));
                assert_eq!(
                    Some(epoch),
                    self.truth.epoch_at(seq),
                    "archived record {seq} is not from the expected writer"
                );
                offset = next_offset;
            }
            self.truth.archived.push((base, scan.next_seq));
            self.bases_seen.insert(base);
            self.c.archived_segments += 1;
            self.c.archived_records_verified += scan.next_seq - base;
            fs::remove_file(&path).unwrap();
        }
    }

    fn run(&mut self, rng: &mut Rng) {
        fs::create_dir_all(&self.archive).unwrap();
        // The first writer creates the journal; readers start once it exists.
        let deadline = Instant::now() + Duration::from_secs(self.secs);
        let mut readers_started = false;
        while Instant::now() < deadline || !matches!(self.expect, Expect::Clean { .. }) {
            self.incarnation(rng, false);
            if !readers_started && self.journal.exists() && !segments(&self.journal).is_empty() {
                readers_started = true;
                self.spawn_reader("safe-visible", false, false, 0, false);
                self.spawn_reader("mapped-visible", true, false, 0, false);
                self.spawn_reader("safe-durable", false, true, 0, false);
                self.spawn_reader("mapped-durable", true, true, 0, false);
                // Slow enough that retention overtakes it.
                self.spawn_reader("slow-safe-visible", false, false, 400, false);
                self.spawn_reader("slow-mapped-visible", true, false, 250, false);
            }
            if readers_started {
                self.check_readers_alive();
                if self.late.len() < 2 && rng.below(12) == 0 {
                    self.c.late_readers += 1;
                    let n = self.c.late_readers;
                    let (mapped, durable) = (rng.below(2) == 0, rng.below(3) == 0);
                    let name = format!("late-{n}");
                    self.spawn_reader(&name, mapped, durable, rng.below(50), true);
                    // Late readers stop at the current durable (or committed) end.
                    let target = if durable {
                        self.last_checkpoint
                    } else {
                        walk(&self.journal).map(|c| c.end).unwrap_or(0)
                    };
                    let stop = self.late.last().unwrap().stop.clone();
                    fs::write(stop, target.to_string()).unwrap();
                }
                if Instant::now() < deadline
                    && matches!(self.expect, Expect::Clean { .. })
                    && rng.below(4) == 0
                {
                    self.inject(rng);
                }
            }
        }
        // Final incarnation: a clean stop with a final checkpoint (again if an injected checkpoint failure ended it).
        let end = loop {
            let stops = self.c.clean_stops;
            let end = self.incarnation(rng, true);
            if self.c.clean_stops > stops {
                break end;
            }
        };
        let chain = walk(&self.journal).unwrap();
        assert_eq!(chain.end, end, "final chain end");
        let report = inspect::verify(&self.journal).unwrap();
        assert!(
            report.problems.is_empty(),
            "final verify: {:?}",
            report.problems
        );
        let mut reader = Reader::open(&self.journal, ReaderConfig::default()).unwrap();
        let mut scratch = Vec::new();
        let mut next = reader.position();
        while let Some(r) = reader.try_next().unwrap() {
            assert_eq!(r.seq, next);
            let epoch =
                soak::check(r.seq, r.schema_id, r.venue_ts_ns, r.payload, &mut scratch).unwrap();
            assert_eq!(
                Some(epoch),
                self.truth.epoch_at(r.seq),
                "final read: record {}",
                r.seq
            );
            next += 1;
            self.c.final_records_verified += 1;
        }
        assert_eq!(next, end, "final read ends at {next}, journal at {end}");
        self.final_end = end;
        fs::write(self.root.join("reader-stop"), end.to_string()).unwrap();
        for r in &self.late {
            // A late reader's target may lie in a range recovery removed and the last writer did not reach again.
            fs::write(&r.stop, end.to_string()).unwrap();
        }
        let readers = std::mem::take(&mut self.readers);
        for mut r in readers {
            let started = Instant::now();
            loop {
                if let Some(status) = r.child.try_wait().unwrap() {
                    assert!(status.success(), "reader {} failed ({status})", r.name);
                    break;
                }
                assert!(
                    started.elapsed() < TIMEOUT * 3,
                    "reader {} did not finish",
                    r.name
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            self.check_log(&r, Some(end));
        }
        let late = std::mem::take(&mut self.late);
        for mut r in late {
            let status = r.child.wait().unwrap();
            assert!(status.success(), "late reader {} failed ({status})", r.name);
            self.check_log(&r, None);
        }
        for (base, _) in segments(&self.journal) {
            self.bases_seen.insert(base);
        }
    }

    /// One writer incarnation: spawn, maybe kill, check what its recovery did. Returns the chain end afterwards.
    fn incarnation(&mut self, rng: &mut Rng, final_stop: bool) -> u64 {
        self.epoch += 1;
        self.c.incarnations += 1;
        let epoch = self.epoch;
        let stop = self.root.join("writer-stop");
        let _ = fs::remove_file(&stop);
        // Keep the last few writers' logs for a failure report.
        let _ = fs::remove_file(
            self.root
                .join(format!("writer-{}.err", epoch.saturating_sub(8))),
        );
        let _ = fs::remove_file(
            self.root
                .join(format!("writer-{}.out", epoch.saturating_sub(8))),
        );
        let mut child = Command::new(WRITER)
            .arg(&self.journal)
            .arg(&self.archive)
            .arg(rng.next_u64().to_string())
            .arg(epoch.to_string())
            .arg(&stop)
            .stdout(Stdio::piped())
            .stderr(File::create(self.root.join(format!("writer-{epoch}.err"))).unwrap())
            .spawn()
            .expect("spawn writer");
        let (rx, reader_thread) = spawn_lines(&mut child);
        self.writer = Some(child);
        let mut lines: Vec<String> = Vec::new();

        let refusing = matches!(self.expect, Expect::Refuse { .. });
        let mode = if final_stop {
            2
        } else if refusing || !matches!(self.expect, Expect::Clean { .. }) {
            1 // let recovery finish after an injection
        } else {
            match rng.below(20) {
                0..=2 => 0,
                3..=17 => 1,
                _ => 2,
            }
        };
        let mut ready = false;
        let mut exited = false;
        if mode == 0 {
            // Kill at a random moment, often during open and recovery.
            std::thread::sleep(Duration::from_micros(rng.below(20_000)));
        } else {
            let started = Instant::now();
            while !ready && !exited {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(line) => {
                        ready = line == "ready";
                        exited = line.starts_with("openerr");
                        lines.push(line);
                    }
                    Err(RecvTimeoutError::Disconnected) => exited = true,
                    Err(RecvTimeoutError::Timeout) => {
                        assert!(
                            started.elapsed() < TIMEOUT,
                            "writer {epoch} never became ready"
                        );
                    }
                }
            }
            if ready {
                let run = Duration::from_millis(if mode == 2 {
                    50 + rng.below(500)
                } else {
                    5 + rng.below(1_500)
                });
                let until = Instant::now() + run;
                while Instant::now() < until {
                    if self.writer.as_mut().unwrap().try_wait().unwrap().is_some() {
                        exited = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5).min(run));
                }
            }
        }
        let mut child = self.writer.take().unwrap();
        let mut killed = false;
        let status = if mode == 2 && !exited && ready {
            fs::write(&stop, b"").unwrap();
            let started = Instant::now();
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                assert!(started.elapsed() < TIMEOUT, "writer {epoch} did not stop");
                std::thread::sleep(Duration::from_millis(5));
            }
        } else if exited {
            child.wait().unwrap()
        } else {
            child.kill().unwrap();
            killed = true;
            self.c.kills += 1;
            child.wait().unwrap()
        };
        reader_thread.join().unwrap();
        lines.extend(rx.try_iter());
        fs::write(
            self.root.join(format!("writer-{epoch}.out")),
            lines.join("\n"),
        )
        .unwrap();

        // What the writer said.
        let mut open: Option<u64> = None;
        let mut actions: Vec<Vec<String>> = Vec::new();
        let mut committed: Option<u64> = None;
        let mut bye = None;
        let mut poisoned = false;
        let mut openerr = None;
        for line in &lines {
            let p: Vec<&str> = line.split(' ').collect();
            match p[0] {
                "open" => open = Some(p[1].parse().unwrap()),
                "act" => actions.push(p.iter().map(|s| s.to_string()).collect()),
                "ready" => {}
                "c" => committed = Some(p[1].parse().unwrap()),
                "k" => {
                    let k: u64 = p[1].parse().unwrap();
                    assert!(k >= self.last_checkpoint, "checkpoint went backwards: {k}");
                    self.last_checkpoint = k;
                    self.c.checkpoints += 1;
                }
                "full" => self.c.storage_full += 1,
                "fullok" => self.c.storage_full_recovered += 1,
                "poisoned" => poisoned = true,
                "bye" => bye = Some(p[1].parse::<u64>().unwrap()),
                "openerr" => openerr = Some(line.clone()),
                "reterr" => panic!("writer {epoch}: retention failed: {line}"),
                _ => panic!("writer {epoch}: unexpected output {line:?}"),
            }
        }
        if !ready && open.is_none() && mode == 0 {
            self.c.kills_before_ready += 1;
        }
        if poisoned {
            self.c.poisoned += 1;
            // It exits on its own; the test may have killed it while it was exiting.
            assert!(
                status.success() || killed,
                "writer {epoch} exited with {status} after poisoning"
            );
        } else if bye.is_some() {
            self.c.clean_stops += 1;
            assert!(status.success());
        } else if let Some(err) = &openerr {
            assert!(refusing, "writer {epoch} refused to open: {err}");
        } else if exited {
            panic!("writer {epoch} exited unexpectedly with {status}; see writer-{epoch}.err");
        }

        // Check recovery against the expectation.
        let expect = std::mem::replace(&mut self.expect, Expect::Clean { end: 0 });
        match (expect, open) {
            (
                Expect::Refuse {
                    file,
                    original,
                    snapshot,
                    end,
                },
                _,
            ) => {
                let err = openerr.unwrap_or_else(|| {
                    panic!("writer {epoch} opened a journal with a damaged middle segment {file}")
                });
                assert!(err.contains(&file), "refusal names another file: {err}");
                assert!(
                    segment_snapshot(&self.journal) == snapshot,
                    "a refused open changed segment bytes"
                );
                self.c.open_refusals += 1;
                replace_file(&self.journal.join(&file), &original, &self.root);
                self.expect = Expect::Clean { end };
                return end;
            }
            (expect, None) => {
                // Killed before recovery finished: the next writer must redo it.
                assert!(mode == 0, "writer {epoch} never reported open");
                self.expect = expect;
            }
            (Expect::Clean { end }, Some(next)) => {
                if self.c.incarnations > 1 || end > 0 {
                    assert_eq!(
                        next, end,
                        "writer {epoch}: recovery lost or invented records"
                    );
                }
                for a in &actions {
                    self.count_benign(epoch, a);
                }
                self.truth.start(next, epoch);
            }
            (
                Expect::Repair {
                    file,
                    offset,
                    seq,
                    old_end,
                    tail,
                    later,
                },
                Some(next),
            ) => {
                assert_eq!(
                    next, seq,
                    "writer {epoch}: repair must keep every record before {seq}"
                );
                let mut repaired = 0;
                let mut quarantined = BTreeSet::new();
                for a in &actions {
                    match a[1].as_str() {
                        "repaired" => {
                            assert_eq!(
                                (a[2].as_str(), a[3].parse::<u64>().unwrap()),
                                (file.as_str(), offset),
                                "{a:?}"
                            );
                            let kept = fs::read(&a[5]).unwrap();
                            assert_eq!(kept, tail[..kept.len()], "quarantined tail bytes");
                            assert!(
                                tail[kept.len()..].iter().all(|&b| b == 0),
                                "quarantine dropped bytes"
                            );
                            self.c.quarantined_files_checked += 1;
                            repaired += 1;
                        }
                        "quarantined" => {
                            let original = &later
                                .iter()
                                .find(|l| l.0 == a[2])
                                .unwrap_or_else(|| panic!("quarantined {} unexpectedly", a[2]))
                                .1;
                            assert!(
                                fs::read(&a[5]).unwrap() == *original,
                                "quarantined segment {} differs",
                                a[2]
                            );
                            self.c.quarantined_files_checked += 1;
                            quarantined.insert(a[2].clone());
                        }
                        _ => self.count_benign(epoch, a),
                    }
                }
                assert_eq!(repaired, 1, "writer {epoch}: {actions:?}");
                assert_eq!(
                    quarantined.len(),
                    later.len(),
                    "writer {epoch}: later segments not all quarantined: {actions:?}"
                );
                self.recreated_segments += later.len() as u64;
                self.c.records_discarded_by_recovery += old_end - seq;
                self.committed_lb = self.committed_lb.min(seq);
                self.truth.discard(seq, old_end);
                self.truth.start(next, epoch);
                let _ = fs::remove_dir_all(self.journal.join("quarantine"));
            }
            (
                Expect::Replace {
                    file,
                    seq,
                    old_end,
                    damaged,
                    how,
                },
                Some(next),
            ) => {
                assert_eq!(
                    next, seq,
                    "writer {epoch}: after a {how} newest segment recovery continues at its base"
                );
                let (mut quarantined, mut completed) = (false, false);
                for a in &actions {
                    match a[1].as_str() {
                        "quarantined" if a[2] == file => {
                            assert!(
                                fs::read(&a[5]).unwrap() == damaged,
                                "quarantined segment differs"
                            );
                            self.c.quarantined_files_checked += 1;
                            quarantined = true;
                        }
                        "completed" if a[2] == file => completed = true,
                        _ => self.count_benign(epoch, a),
                    }
                }
                // The successor is recreated by completing the roll, unless the previous segment was not sealed yet
                // (killed between publishing the segment and sealing its predecessor): then it simply ends the chain.
                assert!(quarantined, "writer {epoch} ({how}): {actions:?}");
                if completed {
                    self.c.rolls_completed += 1;
                }
                self.recreated_segments += 1;
                self.c.records_discarded_by_recovery += old_end - seq;
                self.committed_lb = self.committed_lb.min(seq);
                self.truth.discard(seq, old_end);
                self.truth.start(next, epoch);
                let _ = fs::remove_dir_all(self.journal.join("quarantine"));
            }
        }

        // After the writer: the chain must be intact and hold every record whose append returned (C1).
        let chain = walk(&self.journal).unwrap_or_else(|e| panic!("after writer {epoch}: {e}"));
        if let Some(c) = committed {
            self.committed_lb = self.committed_lb.max(c + 1);
        }
        assert!(
            chain.end >= self.committed_lb,
            "after writer {epoch}: committed record {} lost (chain ends at {})",
            self.committed_lb - 1,
            chain.end
        );
        if let Some(b) = bye {
            assert_eq!(b, chain.end);
        }
        if let Some(c) = chain.checkpoint {
            // A writer killed right after a checkpoint may not have reported it.
            assert!(
                c.next_seq >= self.last_checkpoint,
                "checkpoint marker {} is behind the last reported checkpoint {}",
                c.next_seq,
                self.last_checkpoint
            );
            self.last_checkpoint = c.next_seq;
        }
        for (base, _) in &chain.visited {
            self.bases_seen.insert(*base);
        }
        if matches!(self.expect, Expect::Clean { .. }) {
            self.expect = Expect::Clean { end: chain.end };
        }
        self.drain_archive();
        let bytes = dir_bytes(&self.root);
        self.c.max_disk_bytes = self.c.max_disk_bytes.max(bytes);
        assert!(
            bytes <= DISK_LIMIT,
            "disk use {bytes} bytes exceeds the bound"
        );
        chain.end
    }

    fn count_benign(&mut self, epoch: u32, a: &[String]) {
        match a[1].as_str() {
            "zeroed" => self.c.torn_tails_zeroed += 1,
            "sealed" => self.c.seals_completed += 1,
            "completed" => self.c.rolls_completed += 1,
            "removedtemp" => self.c.temps_removed += 1,
            "created" => assert!(
                self.truth.ranges.is_empty(),
                "journal created again by writer {epoch}"
            ),
            _ => panic!("writer {epoch}: a process kill must never look like corruption: {a:?}"),
        }
    }

    /// Damages the journal after its checkpoint while no writer runs, and records what recovery must do.
    fn inject(&mut self, rng: &mut Rng) {
        let Ok(chain) = walk(&self.journal) else {
            panic!("journal invalid before injection");
        };
        let Some(ckpt) = chain.checkpoint else { return };
        let after: Vec<&(u64, PathBuf)> = chain
            .visited
            .iter()
            .filter(|(b, _)| *b > ckpt.segment_base_seq)
            .collect();
        let pick = rng.below(10);
        let (target, offset, kind) = if pick < 6 || after.is_empty() {
            if chain.uncovered.is_empty() {
                return;
            }
            let r: ChainRecord =
                chain.uncovered[rng.below(chain.uncovered.len() as u64) as usize].clone();
            let mut image = fs::read(&r.path).unwrap();
            // A byte of the body (after the commit word), so the record fails its checksum.
            let at = r.offset as usize + 4 + rng.below(u64::from(r.len) - 4) as usize;
            image[at] ^= 1 << rng.below(8);
            let later: Vec<(String, Vec<u8>)> = chain
                .visited
                .iter()
                .filter(|(b, _)| *b > r.base)
                .map(|(_, p)| {
                    (
                        p.file_name().unwrap().to_string_lossy().into_owned(),
                        fs::read(p).unwrap(),
                    )
                })
                .collect();
            let file = r.path.file_name().unwrap().to_string_lossy().into_owned();
            replace_file(&r.path, &image, &self.root);
            self.expect = Expect::Repair {
                file,
                offset: r.offset,
                seq: r.seq,
                old_end: chain.end,
                tail: image[r.offset as usize..].to_vec(),
                later,
            };
            (r.path, r.offset, "flipped record byte")
        } else {
            let newest = chain.visited.last().unwrap().clone();
            let middle = after.len() > 1 && rng.below(3) == 0;
            let (base, path) = if middle {
                after[rng.below(after.len() as u64 - 1) as usize].clone()
            } else {
                newest
            };
            let original = fs::read(&path).unwrap();
            let truncate = rng.below(2) == 0;
            let damaged = if truncate {
                original[..rng.below(original.len() as u64) as usize].to_vec()
            } else {
                let mut d = original.clone();
                d[rng.below(60) as usize] ^= 1 << rng.below(8);
                d
            };
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            let how = match (middle, truncate) {
                (true, true) => "truncated middle segment",
                (true, false) => "damaged header of a middle segment",
                (false, true) => "truncated newest segment",
                (false, false) => "damaged header of the newest segment",
            };
            let snapshot = segment_snapshot(&self.journal);
            replace_file(&path, &damaged, &self.root);
            self.expect = if middle {
                let mut snapshot = snapshot;
                snapshot.insert(file.clone(), damaged);
                Expect::Refuse {
                    file,
                    original,
                    snapshot,
                    end: chain.end,
                }
            } else {
                Expect::Replace {
                    file,
                    seq: base,
                    old_end: chain.end,
                    damaged,
                    how: if truncate { "truncated" } else { "damaged" },
                }
            };
            (path, 0, how)
        };
        *self.c.injected.entry(kind).or_default() += 1;
        // Readers and `verify` detect it (C11): the first problem is at the damaged position.
        let report = inspect::verify(&self.journal).unwrap();
        let first = report
            .problems
            .first()
            .unwrap_or_else(|| panic!("verify missed a {kind}"));
        assert_eq!(first.path, target, "verify: {kind}: {:?}", report.problems);
        // A damaged format_version byte is reported at offset 8, other header damage at 0.
        assert!(
            first.offset == offset || (offset == 0 && first.offset == 8),
            "verify: {kind}: {:?}",
            report.problems
        );
        self.c.detected_by_verify += 1;
    }

    fn report(&self) -> String {
        let c = &self.c;
        let mut s = String::new();
        let _ = writeln!(
            s,
            "soak: seed {} ran {} s on {} {}",
            self.seed,
            self.secs,
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        let _ = writeln!(
            s,
            "  writer incarnations {} (SIGKILLs {}, of which {} before recovery finished; clean stops {}; poisoned by an injected checkpoint failure {}; refused opens {})",
            c.incarnations,
            c.kills,
            c.kills_before_ready,
            c.clean_stops,
            c.poisoned,
            c.open_refusals
        );
        let _ = writeln!(
            s,
            "  segments created {} (rolls), checkpoints {}, retention moved {} segments",
            self.bases_seen.len() as u64 + self.recreated_segments,
            c.checkpoints,
            c.archived_segments
        );
        let _ = writeln!(
            s,
            "  recovery after kills: torn tails zeroed {}, seals written {}, rolls completed {}, temporary files removed {}",
            c.torn_tails_zeroed, c.seals_completed, c.rolls_completed, c.temps_removed
        );
        let _ = writeln!(
            s,
            "  storage-full roll failures {} (retried successfully {})",
            c.storage_full, c.storage_full_recovered
        );
        let injected: u64 = c.injected.values().sum();
        let _ = writeln!(
            s,
            "  corruptions injected {injected} {:?}; detected by verify {}; quarantine files checked byte for byte {}; uncheckpointed records removed by recovery {}",
            c.injected,
            c.detected_by_verify,
            c.quarantined_files_checked,
            c.records_discarded_by_recovery
        );
        let _ = writeln!(
            s,
            "  records verified: readers {} ({} from removed histories, before Error::Discarded), archive {}, final read {}; final next_seq {}",
            c.reader_records_verified,
            c.reader_records_from_discarded_history,
            c.archived_records_verified,
            c.final_records_verified,
            self.final_end
        );
        let _ = writeln!(
            s,
            "  reader events: Error::Discarded {}, skips after retention {}, transient errors retried {}; late readers {}",
            c.reader_discarded_events,
            c.reader_retention_skips,
            c.reader_transient_errors,
            c.late_readers
        );
        let _ = writeln!(
            s,
            "  peak disk use {} KiB (bound {} MiB)",
            c.max_disk_bytes / 1024,
            DISK_LIMIT >> 20
        );
        s
    }

    fn preserve(&self, message: &str) -> PathBuf {
        let base = std::env::var_os("TICKERTAPE_SOAK_ARTIFACTS").map_or_else(
            || Path::new(env!("CARGO_TARGET_TMPDIR")).join("soak-failures"),
            PathBuf::from,
        );
        let dest = base.join(format!(
            "seed-{}-{}",
            self.seed,
            tickertape::clock::realtime_ns() / 1_000_000_000
        ));
        copy_dir(&self.root, &dest);
        let _ = fs::write(
            dest.join("FAILURE.txt"),
            format!(
                "{message}\n\nReplay: TICKERTAPE_TEST_SEED={} TICKERTAPE_SOAK_SECS={} cargo test -p tickertape-tests --release --test soak -- --ignored --nocapture\n\n{}",
                self.seed,
                self.secs,
                self.report()
            ),
        );
        dest
    }
}

#[test]
#[ignore = "soak: runs for TICKERTAPE_SOAK_SECS (default 20); run with --release -- --ignored"]
fn soak() {
    let secs: u64 = std::env::var("TICKERTAPE_SOAK_SECS")
        .ok()
        .map(|s| s.parse().expect("TICKERTAPE_SOAK_SECS"))
        .unwrap_or(20);
    let seed: u64 = std::env::var("TICKERTAPE_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| tickertape::clock::realtime_ns() | 1);
    eprintln!("soak: TICKERTAPE_TEST_SEED={seed} TICKERTAPE_SOAK_SECS={secs}");
    let mut rng = Rng::new(seed);
    let tmp = tempfile::Builder::new()
        .prefix("tt-soak")
        .tempdir()
        .unwrap();
    let root = tmp.path().to_path_buf();
    let mut soak = Soak {
        seed,
        secs,
        journal: root.join("journal"),
        archive: root.join("archive"),
        root,
        epoch: 0,
        truth: Truth::default(),
        expect: Expect::Clean { end: 0 },
        committed_lb: 0,
        last_checkpoint: 0,
        bases_seen: BTreeSet::new(),
        recreated_segments: 0,
        readers: Vec::new(),
        late: Vec::new(),
        writer: None,
        final_end: 0,
        c: Counters::default(),
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| soak.run(&mut rng)));
    match result {
        Ok(()) => eprint!("{}", soak.report()),
        Err(cause) => {
            let message = cause
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| cause.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "panic".into());
            // Stop the processes before copying, so the preserved journal is the failing one.
            drop(soak.writer.take().map(|mut w| (w.kill(), w.wait())));
            let dest = soak.preserve(&message);
            eprint!("{}", soak.report());
            panic!(
                "soak failed with TICKERTAPE_TEST_SEED={seed} TICKERTAPE_SOAK_SECS={secs}: {message}\n\
                 journal, archive and logs preserved in {}",
                dest.display()
            );
        }
    }
}
