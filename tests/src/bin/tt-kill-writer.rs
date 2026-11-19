//! Test helper for the kill loop: opens the journal (running recovery), then appends deterministic records until
//! it is killed. Every 16 appends it prints the last sequence number whose `append` returned, so the test knows a
//! lower bound of what was committed. It also checkpoints at random intervals.
//!
//! Usage: `tt-kill-writer <dir> <segment_size> <seed> [slow-fill]`
//!
//! With `slow-fill`, payloads are written by `append_with` in 8-byte steps with a pause in between, so that most
//! kills land inside the fill closure: after the header fields and before the CRC and the commit word.

use std::io::Write;
use std::path::PathBuf;

use tickertape::{NewRecord, Writer, WriterConfig};
use tickertape_tests::{Rng, kill_len, payload};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(&args[1]);
    let segment_size: u64 = args[2].parse().expect("segment_size");
    let mut rng = Rng::new(args[3].parse().expect("seed"));
    let slow_fill = args.get(4).is_some_and(|a| a == "slow-fill");

    let mut writer = Writer::open(&dir, WriterConfig::default().segment_size(segment_size))
        .expect("open writer");
    let mut out = std::io::stdout().lock();
    let mut next_checkpoint = rng.below(2000);
    loop {
        let seq = writer.next_seq();
        let data = payload(seq, kill_len(seq));
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
