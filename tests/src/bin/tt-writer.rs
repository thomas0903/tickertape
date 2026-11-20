//! Test helper: appends `count` deterministic records to a journal.
//!
//! Usage: `tt-writer <dir> <count> <segment_size> <max_payload> [pause_at] [go_file]`
//!
//! If `pause_at` and `go_file` are given, the writer stops after `pause_at` records and waits until `go_file`
//! exists before writing the rest.
//!
//! With `TT_WRITER_BATCH=<n>` (n > 1) in the environment, it appends with `Writer::append_batch` in batches of up to
//! `n` records (a batch ends at `pause_at` and at `count`) instead of one `append` per record.

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

    let batch: u64 = std::env::var("TT_WRITER_BATCH")
        .ok()
        .map(|s| s.parse().expect("TT_WRITER_BATCH"))
        .unwrap_or(1);

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
        if batch > 1 {
            let mut end = count.min(seq + batch);
            if let Some(p) = pause_at.filter(|&p| p > seq) {
                end = end.min(p);
            }
            let data: Vec<Vec<u8>> = (seq..end)
                .map(|s| payload(s, bounded_len(s, max_payload)))
                .collect();
            let seqs = writer
                .append_batch((seq..end).zip(&data).map(|(s, d)| record(s, d)))
                .expect("append_batch");
            assert_eq!(seqs, seq..end);
        } else {
            let data = payload(seq, bounded_len(seq, max_payload));
            writer.append(&record(seq, &data)).expect("append");
        }
    }
    writer.checkpoint().expect("checkpoint");
}

fn record(seq: u64, data: &[u8]) -> NewRecord<'_> {
    NewRecord {
        schema_id: (seq % 7) as u32,
        venue_ts_ns: seq,
        local_ts_ns: tickertape::clock::realtime_ns(),
        payload: data,
    }
}
