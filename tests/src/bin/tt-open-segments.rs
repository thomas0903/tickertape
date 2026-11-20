//! Helper for `tests/open_segments.rs`, run with a low descriptor limit: a writer that rolls many times between
//! checkpoints, or a writer whose recovery scans many segments, must not run out of descriptors.
//!
//! Usage:
//! - `tt-open-segments roll <dir> <rolls> <request_every>`: appends until the writer has rolled `rolls` times, with a
//!   background checkpoint requested every `request_every` rolls (0: none), then checkpoints. Prints the next sequence
//!   number, then the change in open descriptors and (on Linux) in mappings from the first roll to the end.
//! - `tt-open-segments open <dir> <fail|quarantine>`: opens a writer (recovery), appends one record, checkpoints, and
//!   prints the next sequence number.
//! - `tt-open-segments chdir <parent> <other>`: changes to `<parent>`, opens a writer on the relative path `j`, rolls
//!   12 times without a checkpoint, changes to `<other>` (which holds another journal `j`), checkpoints, and prints
//!   the next sequence number.

use tickertape::{RecoveryPolicy, Writer};
use tickertape_tests::{append_n, small_config};

/// Open descriptors of this process.
fn descriptors() -> usize {
    std::fs::read_dir("/dev/fd").map_or(0, |d| d.count())
}

/// Mappings of this process (Linux only; 0 elsewhere).
fn mappings() -> usize {
    std::fs::read_to_string("/proc/self/maps").map_or(0, |m| m.lines().count())
}

fn fail(what: &str, e: tickertape::Error) -> ! {
    eprintln!("tt-open-segments: {what}: {e}");
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = &args[2];
    match args[1].as_str() {
        "roll" => {
            let rolls: u64 = args[3].parse().expect("rolls");
            let every: u64 = args[4].parse().expect("request_every");
            let mut writer = Writer::open(dir, small_config()).unwrap_or_else(|e| fail("open", e));
            let mut base = writer.metrics().head.segment_base_seq;
            let mut rolled = 0;
            let mut first = None;
            while rolled < rolls {
                append_n(&mut writer, 1);
                let now = writer.metrics().head.segment_base_seq;
                if now != base {
                    base = now;
                    rolled += 1;
                    first.get_or_insert((descriptors(), mappings()));
                    if every > 0 && rolled % every == 0 {
                        writer
                            .request_checkpoint()
                            .unwrap_or_else(|e| fail("request checkpoint", e));
                    }
                }
            }
            let (fds, maps) = (descriptors(), mappings());
            writer
                .checkpoint()
                .unwrap_or_else(|e| fail("checkpoint", e));
            let (fds0, maps0) = first.unwrap_or((fds, maps));
            println!("{}", writer.next_seq());
            println!("{}", fds as i64 - fds0 as i64);
            println!("{}", maps as i64 - maps0 as i64);
        }
        "open" => {
            let recovery = match args[3].as_str() {
                "fail" => RecoveryPolicy::Fail,
                _ => RecoveryPolicy::Quarantine,
            };
            let mut writer = Writer::open(dir, small_config().recovery(recovery))
                .unwrap_or_else(|e| fail("open", e));
            append_n(&mut writer, 1);
            writer
                .checkpoint()
                .unwrap_or_else(|e| fail("checkpoint", e));
            println!("{}", writer.next_seq());
        }
        "chdir" => {
            std::env::set_current_dir(dir).expect("change to <parent>");
            let mut writer = Writer::open("j", small_config()).unwrap_or_else(|e| fail("open", e));
            let mut base = writer.metrics().head.segment_base_seq;
            let mut rolled = 0;
            while rolled < 12 {
                append_n(&mut writer, 1);
                let now = writer.metrics().head.segment_base_seq;
                if now != base {
                    base = now;
                    rolled += 1;
                }
            }
            std::env::set_current_dir(&args[3]).expect("change to <other>");
            writer
                .checkpoint()
                .unwrap_or_else(|e| fail("checkpoint", e));
            println!("{}", writer.next_seq());
        }
        other => panic!("unknown mode {other}"),
    }
}
