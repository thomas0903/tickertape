//! Test helper for the kill loop: opens the journal (running recovery), then appends deterministic records until
//! it is killed. Every 16 appends it prints the last sequence number whose `append` returned, so the test knows a
//! lower bound of what was committed. It also checkpoints at random intervals.
//!
//! Usage: `tt-kill-writer <dir> <segment_size> <seed> [mixed] [slow-fill]`
//!
//! With `mixed`, payload lengths follow `mixed_len` (up to 3,199 bytes) instead of `kill_len` (up to 512 bytes).
//! With `slow-fill`, payloads are written by `append_with` in 8-byte steps with a pause in between, so that most
//! kills land inside the fill closure: after the header fields and before the CRC and the commit word.
//!
//! It exits on its own when the process that started it is gone (checked every 16 appends), so that a test that is
//! itself killed (a time-out, an interrupt) does not leave it appending until the disk is full.

use std::io::Write;
use std::path::PathBuf;

use tickertape::{NewRecord, Writer, WriterConfig};
use tickertape_tests::{Rng, kill_len, mixed_len, payload};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(&args[1]);
    let segment_size: u64 = args[2].parse().expect("segment_size");
    let mut rng = Rng::new(args[3].parse().expect("seed"));
    let options = &args[4.min(args.len())..];
    let slow_fill = options.iter().any(|a| a == "slow-fill");
    let len_of: fn(u64) -> usize = if options.iter().any(|a| a == "mixed") {
        mixed_len
    } else {
        kill_len
    };

    let parent = std::os::unix::process::parent_id();
    let mut writer = Writer::open(&dir, WriterConfig::default().segment_size(segment_size))
        .expect("open writer");
    let mut out = std::io::stdout().lock();
    let mut next_checkpoint = rng.below(2000);
    loop {
        let seq = writer.next_seq();
        let data = payload(seq, len_of(seq));
        let record = NewRecord {
            schema_id: (seq % 7) as u32,
            venue_ts_ns: seq * 10,
            local_ts_ns: seq * 10 + 1,
            payload: &data,
        };
        if slow_fill {
            writer
                .append_with(&record, data.len(), |buf| {
                    for (dst, src) in buf.chunks_mut(8).zip(data.chunks(8)) {
                        dst.copy_from_slice(src);
                        std::thread::sleep(std::time::Duration::from_micros(20));
                    }
                })
                .expect("append");
        } else {
            writer.append(&record).expect("append");
        }
        if seq % 16 == 0 {
            // Reparented: to init (or a subreaper) once the parent is gone.
            let now = std::os::unix::process::parent_id();
            if now != parent || now == 1 {
                return;
            }
            writeln!(out, "{seq}").expect("stdout");
            out.flush().expect("stdout");
        }
        if next_checkpoint == 0 {
            writer.checkpoint().expect("checkpoint");
            next_checkpoint = rng.below(2000);
        } else {
            next_checkpoint -= 1;
        }
    }
}
