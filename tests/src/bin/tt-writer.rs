//! Test helper: appends `count` deterministic records to a journal.
//!
//! Usage: `tt-writer <dir> <count> <segment_size> <max_payload> [pause_at] [go_file]`
//!
//! If `pause_at` and `go_file` are given, the writer stops after `pause_at` records and waits until `go_file`
//! exists before writing the rest.

use std::path::PathBuf;
use std::time::Duration;

use tickertape::{NewRecord, Writer, WriterConfig};
use tickertape_tests::{bounded_len, payload};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(&args[1]);
    let count: u64 = args[2].parse().expect("count");
    let segment_size: u64 = args[3].parse().expect("segment_size");
    let max_payload: usize = args[4].parse().expect("max_payload");
    let pause_at: Option<u64> = args.get(5).map(|s| s.parse().expect("pause_at"));
    let go_file = args.get(6).map(PathBuf::from);

    let mut writer = Writer::open(&dir, WriterConfig::default().segment_size(segment_size))
        .expect("open writer");
    while writer.next_seq() < count {
        let seq = writer.next_seq();
        if Some(seq) == pause_at {
            if let Some(go) = &go_file {
                while !go.exists() {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        let data = payload(seq, bounded_len(seq, max_payload));
        writer
            .append(&NewRecord {
                schema_id: (seq % 7) as u32,
                venue_ts_ns: seq,
                local_ts_ns: tickertape::clock::realtime_ns(),
                payload: &data,
            })
            .expect("append");
    }
    writer.checkpoint().expect("checkpoint");
}
