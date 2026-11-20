//! Soak helper (`tests/soak.rs`): a reader process that follows the journal across writer restarts, recoveries and
//! retention, checks every record on its own (`tickertape_tests::soak::check`) and logs what it consumed.
//!
//! Usage: `tt-soak-reader <dir> <safe|mapped> <visible|durable> <delay_us> <log> <stop_file>`
//!
//! It stops once `stop_file` exists and holds a sequence number the reader has reached. Log lines:
//! - `R <first> <end> <epoch>`: consumed records `first..end` (gap-free), all from writer `epoch`;
//! - `D <seq>`: `Error::Discarded` at `seq` (recovery or retention replaced or removed the segment);
//! - `S <from> <to>`: after that, `seek(from)` failed with `SeqUnavailable` (retention removed it); continued at `to`;
//! - `C <seq> <error>`: the first of a run of transient errors (corrupt bytes, a vanished file) at `seq`; retried;
//! - `END <position> <records>` on success, or `E <message>` before exiting with status 2.
//!
//! Errors are retried for up to 120 s; a record that fails the check, a gap or a duplicate fails at once.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tickertape::{Error, Reader, ReaderConfig};
use tickertape_tests::soak;

const PATIENCE: Duration = Duration::from_secs(120);

struct Log {
    file: File,
    run: Option<(u64, u64, u32)>,
}

impl Log {
    fn line(&mut self, text: &str) {
        writeln!(self.file, "{text}").expect("write log");
    }

    fn flush_run(&mut self) {
        if let Some((first, end, epoch)) = self.run.take() {
            self.line(&format!("R {first} {end} {epoch}"));
        }
    }

    fn record(&mut self, seq: u64, epoch: u32) {
        match &mut self.run {
            Some((_, end, e)) if *end == seq && *e == epoch => *end += 1,
            _ => {
                self.flush_run();
                self.run = Some((seq, seq + 1, epoch));
            }
        }
    }

    fn fail(&mut self, message: &str) -> ! {
        self.flush_run();
        self.line(&format!("E {message}"));
        eprintln!("tt-soak-reader: {message}");
        std::process::exit(2);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(&args[1]);
    let mapped = args[2] == "mapped";
    let durable = args[3] == "durable";
    let delay = Duration::from_micros(args[4].parse().expect("delay"));
    let mut log = Log {
        file: File::create(&args[5]).expect("create log"),
        run: None,
    };
    let stop_file = PathBuf::from(&args[6]);

    let config = ReaderConfig::default().durable_only(durable);
    let open = || {
        if mapped {
            // SAFETY: in the soak only tickertape writers modify segment files in place; the test injects
            // corruption by renaming new files over segments, never by changing a file a reader may have mapped.
            unsafe { Reader::open_mapped(&dir, config.clone()) }
        } else {
            Reader::open(&dir, config.clone())
        }
    };
    let started = Instant::now();
    let mut reader = loop {
        match open() {
            Ok(r) => break r,
            Err(e) if started.elapsed() < PATIENCE => {
                let _ = e;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => log.fail(&format!("open: {e}")),
        }
    };
    let mut expected = reader.position();
    let mut records = 0u64;
    let mut scratch = Vec::new();
    let mut pending_seek: Option<u64> = None;
    let mut troubled_since: Option<Instant> = None;
    let mut last_stop_check = Instant::now();
    let parent = std::os::unix::process::parent_id();

    loop {
        if let Some(target) = pending_seek {
            match reader.seek(target) {
                Ok(()) => {
                    pending_seek = None;
                    expected = target;
                }
                Err(Error::SeqUnavailable { first, .. }) => {
                    log.line(&format!("S {target} {first}"));
                    pending_seek = Some(first);
                    continue;
                }
                Err(e) => {
                    if troubled_since.is_none() {
                        log.line(&format!("C {target} seek: {e}"));
                    }
                    let since = *troubled_since.get_or_insert_with(Instant::now);
                    if since.elapsed() > PATIENCE {
                        log.fail(&format!("seek({target}) failing for {PATIENCE:?}: {e}"));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
            }
        }
        match reader.next_timeout(Some(Duration::from_millis(50))) {
            Ok(Some(r)) => {
                troubled_since = None;
                if r.seq != expected {
                    let msg = format!("expected record {expected}, got {}", r.seq);
                    log.fail(&msg);
                }
                match soak::check(r.seq, r.schema_id, r.venue_ts_ns, r.payload, &mut scratch) {
                    Ok(epoch) => log.record(r.seq, epoch),
                    Err(msg) => log.fail(&msg),
                }
                expected += 1;
                records += 1;
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
            }
            Ok(None) => {
                troubled_since = None;
                if last_stop_check.elapsed() > Duration::from_millis(200) {
                    last_stop_check = Instant::now();
                    if std::os::unix::process::parent_id() != parent {
                        log.fail("the soak test exited");
                    }
                    // Re-read every time: the test may lower a target.
                    let stop_at: Option<u64> = std::fs::read_to_string(&stop_file)
                        .ok()
                        .and_then(|s| s.trim().parse().ok());
                    if let Some(target) = stop_at {
                        if expected >= target {
                            log.flush_run();
                            log.line(&format!("END {expected} {records}"));
                            return;
                        }
                    }
                }
            }
            Err(Error::Discarded { seq, .. }) => {
                log.flush_run();
                log.line(&format!("D {seq}"));
                pending_seek = Some(expected);
            }
            Err(e) => {
                // Transient while an injected corruption waits for the next writer's recovery, or while recovery
                // moves files: corrupt bytes, a vanished segment.
                log.flush_run();
                if troubled_since.is_none() {
                    log.line(&format!("C {expected} {e}"));
                }
                let since = *troubled_since.get_or_insert_with(Instant::now);
                if since.elapsed() > PATIENCE {
                    log.fail(&format!("read at {expected} failing for {PATIENCE:?}: {e}"));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}
