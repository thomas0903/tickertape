//! Background checkpoint helper (ADR 0038).
//!
//! - `durable-follow <dir> <target>`: a durable-only reader that reads records `0..target` (checking each), then
//!   checks that nothing more becomes readable within 200 ms, and prints `target`.
//! - `crash <dir> <before|after> <checkpointed> <requested> <during>`: appends `checkpointed` records and checkpoints
//!   them (blocking), appends `requested` more and requests a background checkpoint, which is held right before it
//!   writes its marker. `before`: appends `during` records (rolling segments) while it is held, prints `ready` and
//!   waits to be killed, so the process dies between the checkpoint's syncs and its marker. `after`: lets the
//!   checkpoint finish first, then appends `during` records, prints `ready` and waits to be killed.

use std::io::Write;
use std::time::{Duration, Instant};

use tickertape::fault::{self, Point};
use tickertape::{Reader, ReaderConfig, Writer};
use tickertape_tests::{append_n, check_record, small_config};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args[1].as_str() {
        "durable-follow" => {
            let target: u64 = args[3].parse().expect("target");
            let mut reader =
                Reader::open(&args[2], ReaderConfig::default().durable_only(true)).unwrap();
            for seq in 0..target {
                let record = reader
                    .next_timeout(Some(Duration::from_secs(60)))
                    .unwrap()
                    .map(|r| r.to_owned())
                    .unwrap_or_else(|| panic!("record {seq} did not become durable"));
                assert_eq!(record.seq, seq);
                check_record(&record);
            }
            assert!(
                reader
                    .next_timeout(Some(Duration::from_millis(200)))
                    .unwrap()
                    .is_none(),
                "a record past the background checkpoint's frontier was returned"
            );
            println!("{target}");
        }
        "crash" => {
            let number = |i: usize| -> u64 { args[i].parse().expect("count") };
            let mut writer = Writer::open(&args[2], small_config()).unwrap();
            append_n(&mut writer, number(4));
            writer.checkpoint().unwrap();
            append_n(&mut writer, number(5));
            let hold = fault::hold_background(Point::CheckpointBeforeMarker);
            let covered = writer.request_checkpoint().unwrap();
            assert!(hold.wait_reached(Duration::from_secs(30)));
            match args[3].as_str() {
                "before" => {
                    append_n(&mut writer, number(6));
                    // Keep the hold: the process dies with the checkpoint between its syncs and its marker.
                    std::mem::forget(hold);
                }
                "after" => {
                    hold.release();
                    let deadline = Instant::now() + Duration::from_secs(30);
                    while writer.checkpoint_progress().unwrap().pending.is_some() {
                        assert!(Instant::now() < deadline, "the checkpoint did not finish");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    assert_eq!(
                        writer
                            .checkpoint_progress()
                            .unwrap()
                            .durable
                            .unwrap()
                            .next_seq,
                        covered
                    );
                    append_n(&mut writer, number(6));
                }
                other => panic!("unknown crash point {other}"),
            }
            println!("ready");
            std::io::stdout().flush().unwrap();
            std::thread::sleep(Duration::from_secs(600));
            panic!("not killed");
        }
        other => panic!("unknown mode {other}"),
    }
}
