//! Soak helper (`tests/soak.rs`): one writer incarnation. Opens the journal (running recovery), reports what
//! recovery did, then appends records of writer `epoch` at varying rates, with periodic checkpoints, retention
//! (move to `archive`), injected storage-full roll failures and the occasional injected checkpoint failure, until it
//! is killed or `stop_file` appears.
//!
//! Usage: `tt-soak-writer <dir> <archive> <seed> <epoch> <stop_file>`
//!
//! Output lines (flushed as written):
//! - `open <next_seq> <scanned_from_seq> <had_checkpoint>`, one `act <kind> <file> <offset> <len> <aux>` per
//!   recovery action, then `ready`;
//! - `c <seq>`: `append` returned `Ok(seq)` (printed every 32 records and before each checkpoint);
//! - `k <next_seq> <removed_segments>`: a checkpoint covering every record below `next_seq` succeeded;
//! - `full <seq>` / `fullok <seq>`: the append of `seq` failed with a storage-full error, and its retry succeeded;
//! - `poisoned <next_seq>`: an injected checkpoint failure poisoned the writer; it exits (status 0);
//! - `bye <next_seq>`: clean stop after a final checkpoint.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use tickertape::fault::{self, Point};
use tickertape::retention::{Action, Policy};
use tickertape::{NewRecord, RecoveryAction, Writer, WriterConfig};
use tickertape_tests::Rng;
use tickertape_tests::soak::{self, SEGMENT};

const ENOSPC: i32 = 28; // Linux and macOS

fn name(path: &std::path::Path) -> String {
    path.file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(&args[1]);
    let archive = PathBuf::from(&args[2]);
    let mut rng = Rng::new(args[3].parse().expect("seed"));
    let epoch: u32 = args[4].parse().expect("epoch");
    let stop_file = PathBuf::from(&args[5]);

    let config = WriterConfig::default()
        .segment_size(SEGMENT)
        .prefault(rng.below(4) != 0)
        .retention(
            Policy::default().max_segments(3 + rng.below(6) as usize),
            Action::MoveTo(archive),
        );
    let mut out = std::io::stdout().lock();
    let mut writer = match Writer::open(&dir, config) {
        Ok(w) => w,
        Err(e) => {
            writeln!(out, "openerr {e}").unwrap();
            out.flush().unwrap();
            std::process::exit(3);
        }
    };
    let report = writer.recovery_report().clone();
    writeln!(
        out,
        "open {} {} {}",
        report.next_seq, report.scanned_from_seq, report.had_checkpoint
    )
    .unwrap();
    for action in &report.actions {
        let line = match action {
            RecoveryAction::Created => "act created - 0 0 -".to_string(),
            RecoveryAction::ZeroedUncommitted {
                segment,
                offset,
                len,
                ..
            } => {
                format!("act zeroed {} {offset} {len} -", name(segment))
            }
            RecoveryAction::RepairedTail {
                segment,
                offset,
                len,
                quarantine_file,
                reason,
                ..
            } => format!(
                "act repaired {} {offset} {len} {} {reason:?}",
                name(segment),
                quarantine_file.display()
            ),
            RecoveryAction::QuarantinedSegment {
                segment,
                quarantine_file,
                reason,
                ..
            } => format!(
                "act quarantined {} 0 0 {} {reason:?}",
                name(segment),
                quarantine_file.display()
            ),
            RecoveryAction::Sealed { segment, .. } => format!("act sealed {} 0 0 -", name(segment)),
            RecoveryAction::CompletedRoll { segment, .. } => {
                format!("act completed {} 0 0 -", name(segment))
            }
            RecoveryAction::RestoredHeader { segment, .. } => {
                format!("act restoredheader {} 0 0 -", name(segment))
            }
            RecoveryAction::RemovedTemp { file, .. } => {
                format!("act removedtemp {} 0 0 -", name(file))
            }
            other => format!("act other - 0 0 {other:?}"),
        };
        writeln!(out, "{line}").unwrap();
    }
    if let Some(e) = writer.last_retention_error() {
        writeln!(out, "reterr {e}").unwrap();
    }
    writeln!(out, "ready").unwrap();
    out.flush().unwrap();

    let mut data = Vec::new();
    // A first checkpoint soon after open, so retention keeps working when incarnations are short.
    let mut until_checkpoint = rng.below(200);
    let mut fault_armed = false;
    let mut since_stop_check = 0u32;
    let parent = std::os::unix::process::parent_id();
    loop {
        // A phase: a burst without pauses, a steady stream with short pauses, or an idle gap.
        let (count, pause_us) = match rng.below(10) {
            0..=4 => (200 + rng.below(3_000), 0),
            5..=8 => (50 + rng.below(500), 20 + rng.below(400)),
            _ => {
                std::thread::sleep(Duration::from_millis(2 + rng.below(30)));
                (1, 0)
            }
        };
        for _ in 0..count {
            // Storage-full episodes: the next roll's link (or synchronous preallocation) fails with ENOSPC.
            if !fault_armed && rng.below(400) == 0 {
                let point = if rng.below(4) == 0 {
                    Point::Preallocate
                } else {
                    Point::Link
                };
                fault::fail_next(point, ENOSPC);
                fault_armed = true;
            }
            let seq = writer.next_seq();
            soak::payload(seq, epoch, &mut data);
            let record = NewRecord {
                schema_id: epoch,
                venue_ts_ns: seq,
                local_ts_ns: tickertape::clock::realtime_ns(),
                payload: &data,
            };
            match writer.append(&record) {
                Ok(got) => assert_eq!(got, seq),
                Err(e) if e.is_storage_full() => {
                    fault_armed = false;
                    writeln!(out, "full {seq}").unwrap();
                    out.flush().unwrap();
                    assert_eq!(
                        writer.next_seq(),
                        seq,
                        "a failed append consumed a sequence number"
                    );
                    std::thread::sleep(Duration::from_millis(1 + rng.below(5)));
                    let got = writer.append(&record).expect("append after storage full");
                    assert_eq!(got, seq);
                    writeln!(out, "fullok {seq}").unwrap();
                }
                Err(e) => panic!("append {seq}: {e}"),
            }
            if seq % 32 == 0 {
                writeln!(out, "c {seq}").unwrap();
                out.flush().unwrap();
            }
            if until_checkpoint == 0 {
                until_checkpoint = rng.below(500);
                writeln!(out, "c {seq}").unwrap();
                out.flush().unwrap();
                let poison = rng.below(300) == 0;
                if poison {
                    fault::fail_next(Point::CheckpointSync, 5 /* EIO */);
                }
                match writer.checkpoint() {
                    Ok(()) => {
                        assert!(!poison, "injected checkpoint failure did not fail");
                        writeln!(
                            out,
                            "k {} {}",
                            writer.next_seq(),
                            writer.retention_report().removed_segments
                        )
                        .unwrap();
                        if let Some(e) = writer.last_retention_error() {
                            writeln!(out, "reterr {e}").unwrap();
                        }
                        out.flush().unwrap();
                    }
                    Err(e) if poison => {
                        assert!(matches!(
                            writer.checkpoint(),
                            Err(tickertape::Error::DurabilityLost)
                        ));
                        writeln!(out, "poisoned {} {e}", writer.next_seq()).unwrap();
                        out.flush().unwrap();
                        return;
                    }
                    Err(e) => panic!("checkpoint: {e}"),
                }
            } else {
                until_checkpoint -= 1;
            }
            if pause_us > 0 {
                std::thread::sleep(Duration::from_micros(pause_us));
            }
            since_stop_check += 1;
            if since_stop_check >= 64 {
                since_stop_check = 0;
                if std::os::unix::process::parent_id() != parent {
                    return; // the soak test exited
                }
                if stop_file.exists() {
                    fault::clear();
                    writer.checkpoint().expect("final checkpoint");
                    writeln!(
                        out,
                        "k {} {}",
                        writer.next_seq(),
                        writer.retention_report().removed_segments
                    )
                    .unwrap();
                    writeln!(out, "bye {}", writer.next_seq()).unwrap();
                    out.flush().unwrap();
                    return;
                }
            }
        }
    }
}
