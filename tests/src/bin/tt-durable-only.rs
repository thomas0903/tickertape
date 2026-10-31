//! C13 helper: write a checkpointed prefix and an uncheckpointed suffix, or read only the prefix.
//! Usage: `tt-durable-only <write|read> <dir> <checkpointed_count> [uncheckpointed_count]`.

use std::time::Duration;

use tickertape::{Reader, ReaderConfig, Writer};
use tickertape_tests::{append_n, check_record, small_config};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let count: u64 = args[3].parse().expect("checkpointed count");
    match args[1].as_str() {
        "write" => {
            let mut writer = Writer::open(&args[2], small_config()).unwrap();
            append_n(&mut writer, count);
            writer.checkpoint().unwrap();
            append_n(&mut writer, args[4].parse().expect("uncheckpointed count"));
            // Dropping the writer does not checkpoint the suffix.
        }
        "read" => {
            let mut reader =
                Reader::open(&args[2], ReaderConfig::default().durable_only(true)).unwrap();
            for seq in 0..count {
                let record = reader.next_owned().unwrap().expect("checkpointed record");
                assert_eq!(record.seq, seq);
                check_record(&record);
            }
            assert!(
                reader
                    .next_timeout(Some(Duration::from_millis(200)))
                    .unwrap()
                    .is_none()
            );
            println!("{count}");
        }
        other => panic!("unknown mode {other}"),
    }
}
