//! The command-line tool on journals and files written by hand: damaged descriptors, type changes between
//! recording runs, start and end bounds, retention, imports, live writers, signals. Nothing read from a journal or a
//! file may reach iceoryx2, an allocation or an `unsafe` block before it is validated.

#![cfg(feature = "cli")]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::*;
use tickertape_iceoryx2::layout::{SCHEMA_DATA, SCHEMA_DESCRIPTOR};

const SEGMENT: u64 = 1 << 20;

fn j(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("j")
}

fn replay_json(journal: &Path, extra: &[&str]) -> serde_json::Value {
    let out = run_ok(
        &cli(),
        &[
            &[
                "replay",
                journal.to_str().unwrap(),
                "--json",
                "--linger-ms",
                "0",
            ][..],
            extra,
        ]
        .concat(),
        SECS(60),
    );
    serde_json::from_str(&out).unwrap()
}

fn u64_type() -> RawType {
    RawType::fixed("u64", 8, 8)
}

/// Every damaged descriptor reproduced in review (huge or overflowing settings, a slice element size that wraps
/// iceoryx2's chunk arithmetic, alignments and sizes that make `Layout` panic or reach `from_size_align_unchecked`,
/// a dynamic user header, zero-sized slice elements) is refused as malformed data (exit 2) before any iceoryx2
/// service is created, quickly and without growing memory.
#[test]
fn damaged_descriptors_are_refused_before_reaching_iceoryx2() {
    let _serial = serial();
    let service = unique("damaged");
    let base = RawDescriptor::new(&service, u64_type());
    let with = |f: &dyn Fn(&mut RawDescriptor)| {
        let mut d = base.clone();
        f(&mut d);
        d
    };
    let cases: Vec<(RawDescriptor, &str)> = vec![
        (with(&|d| d.settings[0] = 1 << 40), "history_size"),
        (with(&|d| d.settings[0] = i64::MAX as u64), "history_size"),
        (with(&|d| d.settings[4] = u64::MAX), "max_subscribers"),
        (
            with(&|d| d.settings[1] = u64::MAX),
            "subscriber_max_buffer_size",
        ),
        (
            with(&|d| d.settings[2] = u64::MAX),
            "subscriber_max_borrowed_samples",
        ),
        (
            with(&|d| d.payload = RawType::slice("u8", u64::MAX, 1)),
            "payload size",
        ),
        (
            with(&|d| d.payload.alignment = 1 << 63),
            "payload alignment",
        ),
        (with(&|d| d.payload.size = u64::MAX - 7), "payload size"),
        (with(&|d| d.payload.alignment = 3), "power of two"),
        (
            with(&|d| d.user_header = RawType::slice("u8", 1, 1)),
            "user header type",
        ),
        (
            with(&|d| d.payload = RawType::slice("()", 0, 1)),
            "zero-sized elements",
        ),
        (
            with(&|d| d.recorder_buffer = u64::MAX),
            "recorder buffer size",
        ),
    ];
    for (descriptor, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        let journal = j(&dir);
        append_records(&journal, SEGMENT, &run_of(&descriptor, 0, 3));
        for command in ["replay", "info"] {
            let started = Instant::now();
            let (status, _, err) = run(&cli(), &[command, journal.to_str().unwrap()], SECS(30));
            assert_eq!(status.code(), Some(2), "{command} ({expected}): {err}");
            assert!(
                err.contains(expected),
                "{command}: {err} (expected {expected})"
            );
            assert!(
                started.elapsed() < SECS(10),
                "{command} took {:?}",
                started.elapsed()
            );
        }
    }
    // A data record that doesn't fit its (valid) descriptor is malformed too.
    let dir = tempfile::tempdir().unwrap();
    let journal = j(&dir);
    append_records(
        &journal,
        SEGMENT,
        &[
            (SCHEMA_DESCRIPTOR, base.bytes()),
            (SCHEMA_DATA, raw_data(0, &[], &[1, 2, 3])),
        ],
    );
    let (status, _, err) = run(&cli(), &["replay", journal.to_str().unwrap()], SECS(30));
    assert_eq!(status.code(), Some(2), "{err}");
    assert!(err.contains("doesn't match its descriptor"), "{err}");
}

/// Two recording runs of one service whose type changed (same size), or two `--map` rules onto one target, are
/// refused with the incompatibility error naming both, instead of publishing one run's bytes under the other's type.
#[test]
fn replay_refuses_different_types_on_one_target() {
    let _serial = serial();
    let service = unique("changed");
    let run1 = RawDescriptor::new(&service, u64_type());
    for other in [
        RawType::fixed("f64", 8, 8),
        RawType::fixed("[bool; 8]", 8, 1),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let journal = j(&dir);
        append_records(&journal, SEGMENT, &run_of(&run1, 0, 3));
        let mut run2 = RawDescriptor::new(&service, other.clone());
        run2.run_start_seq = 4;
        append_records(&journal, SEGMENT, &run_of(&run2, 4, 3));
        let prefix = format!("{}/", unique("changed-target"));
        let (status, _, err) = run(
            &cli(),
            &[
                "replay",
                journal.to_str().unwrap(),
                "--prefix",
                &prefix,
                "--linger-ms",
                "0",
            ],
            SECS(30),
        );
        assert_eq!(status.code(), Some(1), "{err}");
        assert!(err.contains("incompatible"), "{err}");
        assert!(err.contains("u64") && err.contains(&other.name), "{err}");
    }

    // Same types in both runs: both replay onto the one target.
    let dir = tempfile::tempdir().unwrap();
    let journal = j(&dir);
    append_records(&journal, SEGMENT, &run_of(&run1, 0, 3));
    let mut again = run1.clone();
    again.run_start_seq = 4;
    append_records(&journal, SEGMENT, &run_of(&again, 4, 3));
    let prefix = format!("{}/", unique("same-target"));
    let report = replay_json(&journal, &["--prefix", &prefix]);
    assert_eq!(report["messages"], 6);
    assert_eq!(report["services"].as_array().unwrap().len(), 1);

    // Two services with different types mapped onto one target.
    let (a, b) = (unique("map-a"), unique("map-b"));
    let dir = tempfile::tempdir().unwrap();
    let journal = j(&dir);
    append_records(
        &journal,
        SEGMENT,
        &run_of(&RawDescriptor::new(&a, u64_type()), 0, 2),
    );
    let mut db = RawDescriptor::new(&b, RawType::fixed("f64", 8, 8));
    db.run_start_seq = 0;
    append_records(&journal, SEGMENT, &run_of(&db, 3, 2));
    let target = unique("map-target");
    let (status, _, err) = run(
        &cli(),
        &[
            "replay",
            journal.to_str().unwrap(),
            "--map",
            &format!("{a}={target}"),
            "--map",
            &format!("{b}={target}"),
            "--linger-ms",
            "0",
        ],
        SECS(30),
    );
    assert_eq!(status.code(), Some(1), "{err}");
    assert!(
        err.contains("incompatible") && err.contains(&a) && err.contains(&b),
        "{err}"
    );
}

/// A replay that starts after its descriptor finds it by sequence number; `--to` ends it.
#[test]
fn replay_starts_after_the_descriptor_and_stops_at_to() {
    let _serial = serial();
    let service = unique("from-to");
    let dir = tempfile::tempdir().unwrap();
    let journal = j(&dir);
    append_records(
        &journal,
        SEGMENT,
        &run_of(&RawDescriptor::new(&service, u64_type()), 0, 10),
    );
    let prefix = format!("{}/", unique("from-to-target"));
    let report = replay_json(&journal, &["--prefix", &prefix, "--from", "5", "--to", "7"]);
    assert_eq!(report["messages"], 3);
    assert_eq!(report["first_seq"], 5);
    assert_eq!(report["last_seq"], 7);
    assert_eq!(report["end"], "to_reached");
}

/// Retention removed the segment with a run's only descriptor (journals written without repeated descriptors, or
/// records too large to share a segment with them): readers skip, count and report those records instead of
/// failing for the whole journal.
#[test]
fn readers_skip_records_whose_descriptor_was_removed() {
    let _serial = serial();
    let service = unique("retained");
    let dir = tempfile::tempdir().unwrap();
    let journal = j(&dir);
    // One descriptor, then enough data for several 64 KiB segments.
    let first = run_of(&RawDescriptor::new(&service, u64_type()), 0, 3000);
    append_records(&journal, 65_536, &first);
    let mut second = RawDescriptor::new(&service, u64_type());
    second.run_start_seq = 3001;
    append_records(&journal, 65_536, &run_of(&second, 3001, 5));
    let plan = tickertape::retention::plan(
        &journal,
        &tickertape::retention::Policy::default().max_segments(2),
    )
    .unwrap();
    assert!(!plan.segments().is_empty());
    tickertape::retention::apply(&journal, &plan, &tickertape::retention::Action::Delete).unwrap();

    let info: serde_json::Value = serde_json::from_str(&run_ok(
        &cli(),
        &["info", journal.to_str().unwrap(), "--json"],
        SECS(20),
    ))
    .unwrap();
    let undescribed = info["undescribed_records"].as_u64().unwrap();
    assert!(undescribed > 0, "{info}");
    assert_eq!(info["services"][0]["messages"], 5, "{info}");
    assert_eq!(info["runs"].as_array().unwrap().len(), 1);

    let prefix = format!("{}/", unique("retained-target"));
    let report = replay_json(&journal, &["--prefix", &prefix]);
    assert_eq!(report["messages"], 5);
    assert_eq!(report["undescribed"], undescribed);

    let out = dir.path().join("x.iox2");
    let export: serde_json::Value = serde_json::from_str(&run_ok(
        &cli(),
        &[
            "export",
            journal.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
            "--json",
        ],
        SECS(20),
    ))
    .unwrap();
    assert_eq!(export["messages"], 5);
    assert_eq!(export["undescribed"], undescribed);

    let (_, followed, skipped) = parse_follow_all(&run_ok(
        &example("follow"),
        &["--journal", journal.to_str().unwrap(), "--idle-secs", "1"],
        SECS(20),
    ));
    assert_eq!(followed.len(), 5);
    assert_eq!(skipped.len() as u64, undescribed);
}

fn hr_file(alignment: u64, pattern: &str, version: u64, records: &str) -> String {
    format!(
        "service_name = \"imported/x\"\n\n[iceoryx2_version]\nmajor = 0\nminor = 10\npatch = 0\n\n[details]\n\
         file_format_version = {version}\nmessaging_pattern = \"{pattern}\"\n\n[details.types.payload]\n\
         variant = \"FixedSize\"\ntype_name = \"u32\"\nsize = 4\nalignment = {alignment}\n\n\
         [details.types.user_header]\nvariant = \"FixedSize\"\ntype_name = \"()\"\nsize = 0\nalignment = 1\n\n\
         [details.types.system_header]\nvariant = \"FixedSize\"\ntype_name = \"S\"\nsize = 4\nalignment = 4\n\n\n\
         ### Recorded Data Start ###\n{records}"
    )
}

fn hr_records(n: u32) -> String {
    (0..n)
        .map(|i| {
            format!(
                "time:     {i}\nsys head: 00 00 00 00 \nusr head: \npayload:  {:02x} 00 00 00 \n\n",
                i
            )
        })
        .collect()
}

/// An import is validated completely before anything is appended: a bad header, a truncated or damaged record,
/// another messaging pattern or file format version, or a multi-byte character where the upstream reader slices
/// at a fixed byte index leave the journal exactly as it was. iox2dump files are refused.
#[test]
fn import_is_validated_and_all_or_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let journal = j(&dir);
    append_records(
        &journal,
        SEGMENT,
        &run_of(&RawDescriptor::new("existing/x", u64_type()), 0, 2),
    );
    let good = hr_file(4, "PublishSubscribe", 1, &hr_records(5));
    let cases: Vec<(String, &str)> = vec![
        (
            hr_file(3, "PublishSubscribe", 1, &hr_records(5)),
            "power of two",
        ),
        (
            good.trim_end().rsplit_once('\n').unwrap().0.to_string(),
            "truncated",
        ),
        // Review round 3: the last payload line cut after two of its four hex tokens was imported as a shorter
        // message; names iceoryx2 refuses were imported and only replay or export failed.
        (
            good.trim_end().strip_suffix(" 00 00").unwrap().to_string(),
            "doesn't end with a newline",
        ),
        (
            good.replace("\"imported/x\"", "\"imported/\\u0000x\""),
            "invalid iceoryx2 service name",
        ),
        (
            good.replace("\"imported/x\"", "\"imported/é\""),
            "invalid iceoryx2 service name",
        ),
        (
            good.replace("type_name = \"u32\"", "type_name = \"u\\u000032\""),
            "not valid for iceoryx2",
        ),
        (
            good.replace("payload:  02", "payload:  0x"),
            "two-digit hex",
        ),
        (hr_file(4, "Event", 1, &hr_records(5)), "messaging pattern"),
        (
            hr_file(4, "PublishSubscribe", 2, &hr_records(5)),
            "file format version",
        ),
        (good.replace("time:     3", "time:    ü3"), "not ASCII"),
    ];
    for (i, (text, expected)) in cases.into_iter().enumerate() {
        let file = dir.path().join(format!("bad-{i}.iox2"));
        std::fs::write(&file, text).unwrap();
        let before = count(&journal);
        let (status, _, err) = run(
            &cli(),
            &["import", file.to_str().unwrap(), journal.to_str().unwrap()],
            SECS(30),
        );
        assert_eq!(status.code(), Some(1), "{expected}: {err}");
        assert!(err.contains(expected), "{err} (expected {expected})");
        assert_eq!(count(&journal), before, "nothing appended ({expected})");
        // The journal is still readable as a whole.
        run_ok(&cli(), &["info", journal.to_str().unwrap()], SECS(20));
    }
    let (status, _, err) = run(
        &cli(),
        &[
            "import",
            "x",
            journal.to_str().unwrap(),
            "--representation",
            "iox2-dump",
        ],
        SECS(10),
    );
    assert_eq!(status.code(), Some(1), "{err}");
    // An absolute time that overflows is refused, not saturated.
    let file = dir.path().join("late.iox2");
    std::fs::write(&file, &good).unwrap();
    let before = count(&journal);
    let (status, _, err) = run(
        &cli(),
        &[
            "import",
            file.to_str().unwrap(),
            journal.to_str().unwrap(),
            "--start-unix-ns",
            &(u64::MAX - 3_000_000).to_string(),
        ],
        SECS(30),
    );
    assert_eq!(status.code(), Some(1), "{err}");
    assert!(err.contains("overflows"), "{err}");
    assert_eq!(count(&journal), before);

    let file = dir.path().join("good.iox2");
    std::fs::write(&file, good).unwrap();
    let report: serde_json::Value = serde_json::from_str(&run_ok(
        &cli(),
        &[
            "import",
            file.to_str().unwrap(),
            journal.to_str().unwrap(),
            "--json",
        ],
        SECS(30),
    ))
    .unwrap();
    assert_eq!(report["messages"], 5);
    assert_eq!(report["descriptor_seq"], 3);
    let info: serde_json::Value = serde_json::from_str(&run_ok(
        &cli(),
        &["info", journal.to_str().unwrap(), "--json"],
        SECS(20),
    ))
    .unwrap();
    let runs = info["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1]["imported"], true);
    assert_eq!(runs[1]["run_start_seq"], 3);
}

/// While a writer holds the journal, a target name the journal is recording is refused (it would record the replay
/// into itself), including one reached through the default `replay/` prefix; and the replay ends at the committed
/// end observed when it started, however fast the writer appends.
#[test]
fn replay_guards_against_feedback_with_a_live_writer() {
    let _serial = serial();
    let (x, replayed_x) = {
        let x = unique("live");
        let r = format!("replay/{x}");
        (x, r)
    };
    // Large segments, so the appender stays in the segment with the descriptors for the whole test.
    const LIVE_SEGMENT: u64 = 64 << 20;
    let dir = tempfile::tempdir().unwrap();
    let journal = j(&dir);
    append_records(
        &journal,
        LIVE_SEGMENT,
        &run_of(&RawDescriptor::new(&x, u64_type()), 0, 3),
    );
    let mut recorded_replay = RawDescriptor::new(&replayed_x, u64_type());
    recorded_replay.run_start_seq = 0;
    append_records(&journal, LIVE_SEGMENT, &run_of(&recorded_replay, 4, 3));
    // A live writer (as a recorder of both names would be) appends continuously.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let appender = {
        let journal = journal.clone();
        let stop = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || -> tickertape::Writer {
            let mut w = open_writer(&journal, LIVE_SEGMENT);
            let mut i = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                w.append(&tickertape::NewRecord {
                    schema_id: SCHEMA_DATA,
                    venue_ts_ns: 0,
                    local_ts_ns: 1_700_000_001_000_000_000 + i,
                    payload: &raw_data(0, &[], &i.to_le_bytes()),
                })
                .unwrap();
                i += 1;
                if i.is_multiple_of(8) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            w
        })
    };
    wait_until(SECS(10), "the appender to start", || count(&journal) > 1000);
    let path = journal.to_str().unwrap();
    let map = format!("{x}={x}");
    for args in [
        vec!["--original-names"],
        vec![],
        vec!["--map", map.as_str(), "--original-names"],
    ] {
        let (status, _, err) = run(
            &cli(),
            &[&["replay", path, "--linger-ms", "0"][..], &args].concat(),
            SECS(30),
        );
        assert_eq!(status.code(), Some(1), "{args:?}: {err}");
        assert!(
            err.contains("recording that service right now"),
            "{args:?}: {err}"
        );
    }
    let allowed = replay_json(
        &journal,
        &["--original-names", "--allow-live-targets", "--to", "7"],
    );
    assert_eq!(allowed["messages"], 6);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    // The writer stays open (a live writer); one more record is appended after the replay has started (its
    // `on_start` runs after the end is captured, before the first record is read). The replay ends exactly at the
    // record before it.
    let writer = appender.join().unwrap();
    let end = writer.next_seq();
    let writer = std::sync::Arc::new(std::sync::Mutex::new(writer));
    let report = {
        use tickertape_iceoryx2::replayer::{Remap, Replayer};
        let writer = std::sync::Arc::clone(&writer);
        Replayer::builder(&journal)
            .typed::<u64, ()>(&x)
            .typed::<u64, ()>(&replayed_x)
            .remap(Remap::default().prefix(format!("{}/", unique("elsewhere"))))
            .handle_signals(false)
            .on_start(move || {
                let seq = writer
                    .lock()
                    .unwrap()
                    .append(&tickertape::NewRecord {
                        schema_id: SCHEMA_DATA,
                        venue_ts_ns: 0,
                        local_ts_ns: 1_800_000_000_000_000_000,
                        payload: &raw_data(0, &[], &u64::MAX.to_le_bytes()),
                    })
                    .unwrap();
                assert_eq!(seq, end);
            })
            .build()
            .unwrap()
            .run()
            .unwrap()
    };
    assert_eq!(writer.lock().unwrap().next_seq(), end + 1, "appended");
    assert_eq!(report.last_seq, Some(end - 1), "{report:?}");
    assert_eq!(
        report.messages + 2,
        end,
        "every data record before it: {report:?}"
    );
}

/// What a replay target's publisher would make iceoryx2 allocate is bounded before the publisher exists (review
/// round 3): a descriptor at the old per-publisher sample limit is malformed; zero (unknown) settings next to
/// 65,536-sample buffers become iceoryx2's defaults (8 subscribers) and too many samples; a slice service counts
/// 255 data segments of reference counters; an existing target is checked with its own settings, not the
/// recording's. Nothing is sent, and the targets the replayer would have created don't remain.
#[test]
fn replay_bounds_what_a_target_allocates() {
    use iceoryx2::prelude::*;
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let node = NodeBuilder::new()
        .signal_handling_mode(SignalHandlingMode::Disabled)
        .create::<ipc::Service>()
        .unwrap();
    let exists = |name: &str| {
        ipc::Service::details(
            &ServiceName::new(name).unwrap(),
            node.config(),
            MessagingPattern::PublishSubscribe,
        )
        .unwrap()
        .is_some()
    };
    // history, buffer, borrowed, publishers, subscribers, nodes
    let cases: [(&str, [u64; 6], RawType, i32, &str); 3] = [
        (
            "old-limit",
            [0, 65_536, 0, 2, 256, 20],
            u64_type(),
            2,
            "16777216, more than the limit of 1048576",
        ),
        (
            "zero-and-65536",
            [0, 65_536, 65_536, 0, 0, 0],
            u64_type(),
            1,
            "samples a publisher preallocates is 1048578",
        ),
        (
            "slice-segments",
            [0, 4_096, 2, 2, 9, 20],
            RawType::slice("u8", 1, 1),
            1,
            "sample reference counters",
        ),
    ];
    for (i, (what, settings, payload, code, expected)) in cases.into_iter().enumerate() {
        let journal = dir.path().join(format!("j{i}"));
        let service = unique(what);
        let mut d = RawDescriptor::new(&service, payload);
        d.settings = settings;
        d.recorder_buffer = 0;
        append_records(&journal, SEGMENT, &run_of(&d, 0, 2));
        let prefix = format!("{}/", unique("bounded"));
        let (status, out, err) = run(
            &cli(),
            &["replay", journal.to_str().unwrap(), "--prefix", &prefix],
            SECS(30),
        );
        assert_eq!(status.code(), Some(code), "{what}: {out} {err}");
        assert!(err.contains(expected), "{what}: {err}");
        assert!(!exists(&format!("{prefix}{service}")), "{what}");
    }
    // An existing target with the recorded types and a buffer above the limit: refused by its own settings, though
    // the recording's are small.
    let journal = dir.path().join("existing");
    let service = unique("existing");
    append_records(
        &journal,
        SEGMENT,
        &run_of(&RawDescriptor::new(&service, u64_type()), 0, 2),
    );
    let prefix = format!("{}/", unique("big-target"));
    let target = node
        .service_builder(&ServiceName::new(&format!("{prefix}{service}")).unwrap())
        .publish_subscribe::<u64>()
        .subscriber_max_buffer_size(100_000)
        .create()
        .unwrap();
    let (status, _, err) = run(
        &cli(),
        &["replay", journal.to_str().unwrap(), "--prefix", &prefix],
        SECS(30),
    );
    assert_eq!(status.code(), Some(1), "{err}");
    assert!(
        err.contains("settings in force: subscriber_max_buffer_size is 100000"),
        "{err}"
    );
    assert_eq!(target.dynamic_config().number_of_publishers(), 0);
}

/// Example binaries are found from test binaries in both of Cargo's layouts.
#[test]
fn examples_are_found_in_both_cargo_layouts() {
    use std::path::PathBuf;
    for (exe, profile) in [
        ("/t/debug/deps/journal-0123", Some("/t/debug")),
        (
            "/t/debug/build/tickertape-iceoryx2/0123/out/journal-0123",
            Some("/t/debug"),
        ),
        (
            "/t/aarch64-unknown-linux-gnu/release/deps/journal-0123",
            Some("/t/aarch64-unknown-linux-gnu/release"),
        ),
        (
            "/t/aarch64-unknown-linux-gnu/release/build/tickertape-iceoryx2/0123/out/journal-0123",
            Some("/t/aarch64-unknown-linux-gnu/release"),
        ),
        ("/t/debug/journal-0123", None),
        ("/t/debug/x/tickertape-iceoryx2/0123/out/journal-0123", None),
    ] {
        assert_eq!(
            profile_dir_of(Path::new(exe)),
            profile.map(PathBuf::from),
            "{exe}"
        );
    }
    assert!(example("follow").exists());
}

/// Waits until the `u64` service `name` exists with a publisher.
fn wait_for_u64_publisher(name: &str, timeout: Duration) {
    use iceoryx2::prelude::*;
    let node = NodeBuilder::new()
        .signal_handling_mode(SignalHandlingMode::Disabled)
        .create::<ipc::Service>()
        .unwrap();
    let service = ServiceName::new(name).unwrap();
    wait_until(timeout, &format!("a publisher of {name}"), || {
        node.service_builder(&service)
            .publish_subscribe::<u64>()
            .open()
            .is_ok_and(|s| s.dynamic_config().number_of_publishers() > 0)
    });
}

/// Speeds that can't be scheduled are usage errors, not panics.
#[test]
fn unusable_speeds_are_usage_errors() {
    let dir = tempfile::tempdir().unwrap();
    let journal = j(&dir);
    append_records(
        &journal,
        SEGMENT,
        &run_of(&RawDescriptor::new(&unique("speed"), u64_type()), 0, 2),
    );
    for speed in ["1e-300", "1e-25", "0", "-1", "NaN", "inf"] {
        let flag = format!("--speed={speed}");
        let (status, _, err) = run(
            &cli(),
            &["replay", journal.to_str().unwrap(), &flag],
            SECS(10),
        );
        assert_eq!(status.code(), Some(1), "--speed {speed}: {err}");
        assert!(err.contains("speed must be from"), "--speed {speed}: {err}");
    }
}

/// An export that fails midway (here: the service's type changes between recording runs) leaves no output file and
/// no partial file behind.
#[test]
fn failed_export_leaves_no_file() {
    let service = unique("export-fail");
    let dir = tempfile::tempdir().unwrap();
    let journal = j(&dir);
    append_records(
        &journal,
        SEGMENT,
        &run_of(&RawDescriptor::new(&service, u64_type()), 0, 3),
    );
    let mut run2 = RawDescriptor::new(&service, RawType::fixed("f64", 8, 8));
    run2.run_start_seq = 4;
    append_records(&journal, SEGMENT, &run_of(&run2, 4, 3));
    let out = dir.path().join("out.iox2");
    let (status, _, err) = run(
        &cli(),
        &[
            "export",
            journal.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ],
        SECS(20),
    );
    assert_eq!(status.code(), Some(1), "{err}");
    assert!(err.contains("changed types"), "{err}");
    let left: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != "j")
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}

/// A second SIGINT or SIGTERM exits at once with 130 or 143; one stops cleanly with 0. A replay stopped while it
/// waits for subscribers sends nothing.
#[test]
fn signals_stop_cleanly_and_a_second_one_exits_at_once() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let journal = j(&dir);
    // A recorder that only wakes every 300 ms, waiting for a service that never appears.
    let record_args = [
        "record",
        journal.to_str().unwrap(),
        "--service",
        &unique("never"),
        "--poll-interval-us",
        "300000",
        "--json",
    ];
    // Each process says when it has started, which is after its signal handlers are installed.
    let recorder = Proc::spawn(&cli(), &record_args);
    recorder.wait_for_output("tickertape-iox2: recording into", SECS(20));
    recorder.signal("TERM");
    let (status, out, err) = recorder.wait(SECS(20));
    assert!(status.success(), "{err}");
    assert!(out.contains("\"stop_reason\":\"signal\""), "{out}");

    let recorder = Proc::spawn(&cli(), &record_args);
    recorder.wait_for_output("tickertape-iox2: recording into", SECS(20));
    let pid = recorder.pid().to_string();
    let killed = std::process::Command::new("sh")
        .args(["-c", &format!("kill -TERM {pid} && kill -INT {pid}")])
        .status()
        .unwrap();
    assert!(killed.success());
    let (status, _, err) = recorder.wait(SECS(20));
    assert!(
        matches!(status.code(), Some(130 | 143)),
        "second signal: {status} {err}"
    );

    // Replay waiting for a subscriber that never comes: stopped, it sends nothing.
    let journal = dir.path().join("r");
    let waits = unique("waits");
    append_records(
        &journal,
        SEGMENT,
        &run_of(&RawDescriptor::new(&waits, u64_type()), 0, 3),
    );
    let prefix = format!("{}/", unique("waits-target"));
    let replay = Proc::spawn(
        &cli(),
        &[
            "replay",
            journal.to_str().unwrap(),
            "--prefix",
            &prefix,
            "--wait-subscribers",
            "1",
            "--wait-timeout-secs",
            "60",
            "--json",
        ],
    );
    replay.wait_for_output("tickertape-iox2: replaying", SECS(20));
    // The "replaying" line comes before the publisher exists: signal only once it does, so the signal arrives
    // while the replayer waits for subscribers.
    wait_for_u64_publisher(&format!("{prefix}{waits}"), SECS(20));
    replay.signal("TERM");
    let (status, out, err) = replay.wait(SECS(20));
    assert!(status.success(), "{err}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["messages"], 0, "{report}");
    assert_eq!(report["end"], "signal");
}

/// A loss record that doesn't match its descriptor (here: the descriptor has no counter) is malformed for every
/// reader, not only `info`: `replay` and `export` refuse it too (exit 2), as the layout's validity rules require.
#[test]
fn loss_records_are_checked_against_their_descriptor_by_every_reader() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let journal = j(&dir);
    let service = unique("loss-check");
    let loss = tickertape_iceoryx2::layout::LossRecord {
        kind: tickertape_iceoryx2::layout::LossKind::Gap,
        width: 8,
        descriptor_seq: 0,
        publisher_id: 0,
        expected: 1,
        received: 3,
    }
    .encode()
    .unwrap()
    .to_vec();
    let mut records = run_of(&RawDescriptor::new(&service, u64_type()), 0, 2);
    records.push((tickertape_iceoryx2::layout::SCHEMA_LOSS, loss));
    records.push((SCHEMA_DATA, raw_data(0, &[], &7u64.to_le_bytes())));
    append_records(&journal, SEGMENT, &records);
    let path = journal.to_str().unwrap();
    let out = dir.path().join("x.iox2");
    let prefix = format!("{}/", unique("loss-check-target"));
    for args in [
        vec!["info", path],
        vec!["replay", path, "--linger-ms", "0", "--prefix", &prefix],
        vec!["export", path, "--output", out.to_str().unwrap()],
    ] {
        let (status, _, err) = run(&cli(), &args, SECS(30));
        assert_eq!(status.code(), Some(2), "{args:?}: {err}");
        assert!(err.contains("counter"), "{args:?}: {err}");
    }
    assert!(!out.exists());
}

/// A file written by the real upstream recorder (`iox2 service record` 0.10.0; provenance in
/// `tests/fixtures/README.md`) imports: its types, message bytes, order and relative times arrive in the journal.
#[test]
fn import_reads_a_file_written_by_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let journal = j(&dir);
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/iox2-0.10.0-record-ticks.txt");
    let report: serde_json::Value = serde_json::from_str(&run_ok(
        &cli(),
        &[
            "import",
            fixture.to_str().unwrap(),
            journal.to_str().unwrap(),
            "--start-unix-ns",
            "1700000000000000000",
            "--json",
        ],
        SECS(30),
    ))
    .unwrap();
    assert_eq!(report["service"], "tt-fixture/ticks", "{report}");
    assert_eq!(report["messages"], 5, "{report}");
    assert_eq!(report["iceoryx2_version"], "0.10.0", "{report}");
    let recs = scan(&journal);
    let Rec::Descriptor(_, d) = &recs[0] else {
        panic!("{:?}", recs[0]);
    };
    assert!(d.imported);
    assert_eq!(d.payload.name, "tickertape_iceoryx2::example::Tick");
    assert_eq!((d.payload.size, d.payload.alignment), (24, 8));
    assert_eq!(
        d.user_header.name,
        "tickertape_iceoryx2::example::TickHeader"
    );
    assert_eq!(d.user_header.size, 16);
    let offsets_ms = [0u64, 23, 40, 69, 81];
    let data: Vec<_> = recs[1..]
        .iter()
        .map(|r| match r {
            Rec::Data {
                local_ts_ns,
                header,
                payload,
                publisher_id,
                ..
            } => (*local_ts_ns, header.clone(), payload.clone(), *publisher_id),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(data.len(), 5);
    for (i, (ts, header, payload, publisher)) in data.iter().enumerate() {
        assert_eq!(*ts, 1_700_000_000_000_000_000 + offsets_ms[i] * 1_000_000);
        assert_eq!(counter_of(header), i as u64, "the user header's counter");
        assert_eq!(counter_of(payload), i as u64, "the payload's counter");
        assert_eq!(payload.len(), 24);
        assert_eq!(*publisher, 0, "publisher ids are not imported");
    }
}
