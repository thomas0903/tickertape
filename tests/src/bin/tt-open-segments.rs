//! Helper for `tests/open_segments.rs`, run with a low descriptor limit: a writer that rolls many times between
//! checkpoints, or a writer whose recovery scans many segments, must not run out of descriptors.
//!
//! Usage:
//! - `tt-open-segments roll <dir> <rolls> <request_every> <spare>`: with `WriterConfig::spare_segment` set to `<spare>`
//!   (`on`, `off` or `default`), appends until the writer has rolled `rolls` times, with a
//!   background checkpoint requested every `request_every` rolls (0: none), then checkpoints. Prints the next sequence
//!   number, then the change in open descriptors and (on Linux) in mappings from the first roll to the end.
//! - `tt-open-segments worst <dir> <rolls> <request_every> <spare>`: drives the writer, with the hold points of
//!   `tickertape::fault`, into the state where it holds the most descriptors (`docs/operations.md` → Open files): the
//!   write-back helper (Linux) held with a segment it was handed, a background checkpoint held with a segment it
//!   reopened, a second one queued behind it, the writer's own list full, a spare being prepared (Linux, if on), and a
//!   roll in progress. Prints the change in the writer's descriptors and in mappings of journal files (Linux) from just
//!   after the open to that state; then
//!   releases the holds, rolls until `rolls` with a background checkpoint requested every `request_every` rolls,
//!   samples after each roll, checkpoints, and prints the next sequence number and the largest sampled changes. On
//!   Linux the open descriptors and mappings at the held state go to standard error.
//! - `tt-open-segments open <dir> <fail|quarantine>`: opens a writer (recovery), appends one record, checkpoints, and
//!   prints the next sequence number.
//! - `tt-open-segments chdir <parent> <other>`: changes to `<parent>`, opens a writer on the relative path `j`, rolls
//!   12 times without a checkpoint, changes to `<other>` (which holds another journal `j`), checkpoints, and prints
//!   the next sequence number.

use std::time::Duration;

use tickertape::fault::{self, Point};
use tickertape::{RecoveryPolicy, Writer, WriterConfig};
use tickertape_tests::{append_n, small_config};

/// How long a hold may take to be reached before the helper gives up.
const LONG: Duration = Duration::from_secs(60);

/// Open descriptors of this process.
fn descriptors() -> usize {
    std::fs::read_dir("/dev/fd").map_or(0, |d| d.count())
}

/// Mappings of this process (Linux only; 0 elsewhere).
fn mappings() -> usize {
    std::fs::read_to_string("/proc/self/maps").map_or(0, |m| m.lines().count())
}

/// Open descriptors of this process, except (Linux) those under `/sys/devices/system/cpu/`: a helper thread reads
/// `/sys/devices/system/cpu/isolated` once when it starts, to place itself, which may still be open just after the
/// writer's open. Everything else counts, also a descriptor the writer leaked outside the journal directory.
fn writer_descriptors() -> usize {
    if !cfg!(target_os = "linux") {
        return descriptors();
    }
    std::fs::read_dir("/proc/self/fd").map_or(0, |fds| {
        fds.flatten()
            .filter(|fd| {
                !std::fs::read_link(fd.path())
                    .is_ok_and(|target| target.starts_with("/sys/devices/system/cpu/"))
            })
            .count()
    })
}

/// Mappings of files under `dir` (Linux only; 0 elsewhere): the segments and the spare.
fn journal_mappings(dir: &str) -> usize {
    std::fs::read_to_string("/proc/self/maps")
        .map_or(0, |m| m.lines().filter(|l| l.contains(dir)).count())
}

/// What the open descriptors and the journal's mappings name (Linux only), for a failure message.
fn report_open_files(dir: &str) {
    if let Ok(fds) = std::fs::read_dir("/proc/self/fd") {
        for fd in fds.flatten() {
            let target = std::fs::read_link(fd.path()).unwrap_or_default();
            eprintln!(
                "fd {}: {}",
                fd.file_name().to_string_lossy(),
                target.display()
            );
        }
    }
    if let Ok(maps) = std::fs::read_to_string("/proc/self/maps") {
        for line in maps.lines().filter(|l| l.contains(dir)) {
            eprintln!("map {line}");
        }
    }
}

/// The writer config for `<spare>` (`on`, `off` or `default`).
fn config(spare: &str) -> WriterConfig {
    match spare {
        "on" => small_config().spare_segment(true),
        "off" => small_config().spare_segment(false),
        _ => small_config(),
    }
}

/// Appends until `writer` has rolled `rolls` times.
fn roll(writer: &mut Writer, rolls: u64) {
    let mut base = writer.metrics().head.segment_base_seq;
    let mut rolled = 0;
    while rolled < rolls {
        append_n(writer, 1);
        let now = writer.metrics().head.segment_base_seq;
        if now != base {
            base = now;
            rolled += 1;
        }
    }
}

/// The `worst` mode (see the module documentation).
fn worst(dir: &str, rolls: u64, every: u64, spare: &str) {
    // On Linux a spare being prepared holds its file while the rolls create their segments themselves (they do not
    // wait for it): it is held there from its first request, before it maps the file. On macOS a roll waits for the
    // spare and the spare becomes the new segment, so it adds nothing to a roll; it is not held there.
    let linux_spare = cfg!(target_os = "linux") && spare == "on";
    let spare_hold = linux_spare.then(|| fault::hold_background(Point::SpareMap));
    let mut writer = Writer::open(dir, config(spare)).unwrap_or_else(|e| fail("open", e));
    let (fds0, maps0) = (writer_descriptors() as i64, journal_mappings(dir) as i64);
    let change = move |dir: &str| {
        (
            writer_descriptors() as i64 - fds0,
            journal_mappings(dir) as i64 - maps0,
        )
    };
    // The write-back helper (Linux) is held with the first segment the writer rolls past: it keeps that file open
    // after every list of the writer has let go of it.
    let writeback = fault::hold_background(Point::EarlyWriteback);
    roll(&mut writer, 1);
    if cfg!(target_os = "linux") {
        assert!(
            writeback.wait_reached(LONG),
            "the write-back helper did not start"
        );
    }
    if let Some(hold) = &spare_hold {
        assert!(hold.wait_reached(LONG), "the spare was not requested");
    }
    // A checkpoint whose list holds four segments and has released the older ones, held after it has reopened the
    // second oldest (so that a checkpoint keeping the first one open as well would show): segments 0 to 5 rolled past
    // and the active segment 6, of which 0 to 2 released.
    roll(&mut writer, 5);
    let running = fault::hold_background(Point::CheckpointReopen);
    writer
        .request_checkpoint()
        .unwrap_or_else(|e| fail("request checkpoint", e));
    assert!(
        running.wait_reached(LONG),
        "the checkpoint did not reopen a segment"
    );
    // A hold is used up when it is reached: arm the next one before letting the first go.
    let second = fault::hold_background(Point::CheckpointReopen);
    drop(running);
    assert!(
        second.wait_reached(LONG),
        "the checkpoint did not reopen a second segment"
    );
    let running = second;
    // A second checkpoint queued behind it, with four segments of its own (and released ones).
    roll(&mut writer, 6);
    writer
        .request_checkpoint()
        .unwrap_or_else(|e| fail("request checkpoint", e));
    // The writer's own list holding four segments; then one more roll, counted after it has created and linked the
    // new segment and before the old one joins the list.
    roll(&mut writer, 6);
    let at_roll = std::rc::Rc::new(std::cell::Cell::new(None));
    let seen = std::rc::Rc::clone(&at_roll);
    let dir_owned = dir.to_owned();
    fault::run_at(Point::RollAfterLink, 0, move || {
        seen.set(Some(change(&dir_owned)));
        report_open_files(&dir_owned);
    });
    roll(&mut writer, 1);
    let held = at_roll.get().expect("the roll did not reach its hook");
    drop(running);
    drop(writeback);
    drop(spare_hold);
    // From here the background threads run freely: whatever the timing, no sample may exceed the bound.
    let mut most = held;
    let mut base = writer.metrics().head.segment_base_seq;
    let mut rolled = 0;
    while rolled < rolls {
        append_n(&mut writer, 1);
        let now = writer.metrics().head.segment_base_seq;
        if now != base {
            base = now;
            rolled += 1;
            let (fds, maps) = change(dir);
            most = (most.0.max(fds), most.1.max(maps));
            if every > 0 && rolled % every == 0 {
                writer
                    .request_checkpoint()
                    .unwrap_or_else(|e| fail("request checkpoint", e));
            }
        }
    }
    writer
        .checkpoint()
        .unwrap_or_else(|e| fail("checkpoint", e));
    println!("{}", writer.next_seq());
    println!("{}", held.0);
    println!("{}", held.1);
    println!("{}", most.0);
    println!("{}", most.1);
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
            let mut writer =
                Writer::open(dir, config(&args[5])).unwrap_or_else(|e| fail("open", e));
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
        "worst" => worst(
            dir,
            args[3].parse().expect("rolls"),
            args[4].parse().expect("request_every"),
            &args[5],
        ),
        other => panic!("unknown mode {other}"),
    }
}
