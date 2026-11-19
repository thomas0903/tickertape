//! Cross-process tests: publishers, recorders, live readers and subscribers are separate processes, as in use.
//! No network, accounts or keys: everything is local iceoryx2 shared memory and a temporary journal.

#![cfg(feature = "cli")]

mod common;

#[path = "../examples/common/types.rs"]
mod types;

use common::*;
use iceoryx2::prelude::*;
use tickertape_iceoryx2::layout::LossKind;
use types::{Tick, slice_for_counter};
use zerocopy::IntoBytes;

fn expect_tick(header: &[u8], payload: &[u8], counter: u64) {
    assert_eq!(counter_of(header), counter, "header counter");
    assert_eq!(
        payload,
        Tick::for_counter(counter).as_bytes(),
        "payload of message {counter}"
    );
}

fn expect_slice(header: &[u8], payload: &[u8], counter: u64) {
    assert_eq!(counter_of(header), counter, "header counter");
    assert_eq!(
        payload,
        slice_for_counter(counter).as_bytes(),
        "slice payload of message {counter}"
    );
}

fn data_of(recs: &[Rec]) -> Vec<(u64, u64, Vec<u8>, Vec<u8>)> {
    recs.iter()
        .filter_map(|r| match r {
            Rec::Data {
                seq,
                descriptor_seq,
                header,
                payload,
                ..
            } => Some((*seq, *descriptor_seq, header.clone(), payload.clone())),
            _ => None,
        })
        .collect()
}

fn descriptors_of(recs: &[Rec]) -> Vec<(u64, String, u64)> {
    recs.iter()
        .filter_map(|r| match r {
            Rec::Descriptor(seq, d) => Some((*seq, d.service_name.clone(), d.run_start_seq)),
            _ => None,
        })
        .collect()
}

/// Starts a blocking publisher of `n` messages (no safe overflow: it waits instead of dropping when a subscriber
/// buffer is full, so delivery is exact) and waits until it has created its service.
fn blocking_publisher(service: &str, n: u64, rate: u64, extra: &[&str]) -> Proc {
    let n = n.to_string();
    let rate = rate.to_string();
    let args = [
        &[
            "--service",
            service,
            "--count",
            &n,
            "--rate",
            &rate,
            "--wait-subscribers",
            "1",
            "--blocking",
        ][..],
        extra,
    ]
    .concat();
    let publisher = Proc::spawn(&example("publisher"), &args);
    wait_for_service(service, SECS(20));
    publisher
}

/// Records `n` tick messages of a fresh service with the dynamic CLI recorder; returns (journal, service). Delivery
/// is blocking and the recorder checks the counter, so a lost message fails here, as loss, not later as a mismatch.
fn record_ticks(n: u64, rate: u64) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let service = unique("ticks");
    let publisher = blocking_publisher(&service, n, rate, &[]);
    let recorder = Proc::spawn(
        &cli(),
        &[
            "record",
            journal.to_str().unwrap(),
            "--service",
            &service,
            "--counter",
            "header:0:8",
            "--json",
        ],
    );
    let (status, _, err) = publisher.wait(SECS(60));
    assert!(status.success(), "publisher: {err}");
    recorder.signal("TERM");
    let (status, out, err) = recorder.wait(SECS(20));
    assert!(status.success(), "recorder failed: {err}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        (
            &report["services"][0]["gaps"],
            &report["services"][0]["backwards"]
        ),
        (&serde_json::json!(0), &serde_json::json!(0)),
        "messages were lost before the recorder: {out}"
    );
    assert_eq!(report["messages"], n, "{out}");
    assert_eq!(report["stop_reason"], "signal");
    (dir, service)
}

/// A typed publisher sends N messages at a fixed rate; the typed recorder (public iceoryx2 API only) records them;
/// two ordinary tickertape readers in other processes, one visible and one durable-only, follow the recording live.
#[test]
fn typed_recording_is_followed_live_in_both_modes() {
    let _serial = serial();
    const N: u64 = 1000;
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let j = journal.to_str().unwrap();
    let service = unique("typed");
    let publisher = blocking_publisher(&service, N, 2000, &[]);
    let recorder = Proc::spawn(
        &example("typed-recorder"),
        &[
            "--journal",
            j,
            "--service",
            &service,
            "--checkpoint-ms",
            "100",
        ],
    );
    let mut visible = Proc::spawn(
        &example("follow"),
        &[
            "--journal",
            j,
            "--count",
            &N.to_string(),
            "--idle-secs",
            "30",
        ],
    );
    let mut durable = Proc::spawn(
        &example("follow"),
        &[
            "--journal",
            j,
            "--durable-only",
            "--count",
            &N.to_string(),
            "--idle-secs",
            "30",
        ],
    );
    let (status, _, err) = publisher.wait(SECS(60));
    assert!(status.success(), "publisher: {err}");
    // Both readers finish while the recorder is still running: they follow the journal live, and the periodic
    // checkpoints make the records durable without a stop.
    wait_until(SECS(20), "both readers to see every message", || {
        visible.exited() && durable.exited()
    });
    let mut recorder = recorder;
    assert!(!recorder.exited(), "the recorder must still be running");
    recorder.signal("TERM");
    let (status, out, err) = recorder.wait(SECS(20));
    assert!(status.success(), "{err}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        report["services"][0]["gaps"], 0,
        "lost before the recorder: {out}"
    );
    assert_eq!(report["messages"], N);

    let recs = scan(&journal);
    let data = data_of(&recs);
    assert_eq!(descriptors_of(&recs), vec![(0, service.clone(), 0)]);
    assert_eq!(data.len() as u64, N);
    for (i, (seq, descriptor_seq, header, payload)) in data.iter().enumerate() {
        assert_eq!(*seq, i as u64 + 1);
        assert_eq!(*descriptor_seq, 0);
        expect_tick(header, payload, i as u64);
    }
    for (name, reader) in [("visible", visible), ("durable", durable)] {
        let (status, out, err) = reader.wait(SECS(5));
        assert!(status.success(), "{name} reader: {err}");
        let (descriptors, followed) = parse_follow(&out);
        assert_eq!(descriptors, vec![(0, service.clone())], "{name}");
        assert_eq!(followed.len() as u64, N, "{name}");
        for (i, f) in followed.iter().enumerate() {
            assert_eq!(f.seq, i as u64 + 1, "{name}: gap-free sequence numbers");
            expect_tick(&f.header, &f.payload, i as u64);
            // The typed recorder's extractor copies the header's event time into venue_ts_ns.
            assert_eq!(
                f.venue_ts_ns,
                u64::from_le_bytes(f.header[8..16].try_into().unwrap())
            );
            assert!(f.local_ts_ns > 0);
        }
    }
}

/// SIGKILL the recorder mid-stream, restart it on the same journal: the journal recovers and continues with no
/// duplicate or missing tickertape sequence numbers, and nothing a reader saw before the kill is lost. The publisher
/// runs until the test stops it, so the restarted recorder has all the time it needs to recover and subscribe.
#[test]
fn killed_recorder_restarts_and_continues_the_journal() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let j = journal.to_str().unwrap();
    let service = unique("kill");
    // Small segments: recovery zeroes the unused rest of the active segment, which is slow for 128 MiB in a debug
    // build, and the publisher would finish before the restarted recorder is back.
    let record_args = [
        "record",
        j,
        "--service",
        &service,
        "--counter",
        "header:0:8",
        "--checkpoint-interval-ms",
        "50",
        "--segment-size",
        "1048576",
    ];
    let first = Proc::spawn(&cli(), &record_args);
    let follower = Proc::spawn(&example("follow"), &["--journal", j, "--idle-secs", "3"]);
    let publisher = Proc::spawn(
        &example("publisher"),
        &[
            "--service",
            &service,
            "--count",
            "0",
            "--rate",
            "2000",
            "--wait-subscribers",
            "1",
        ],
    );
    wait_until(
        SECS(30),
        "the first recorder to record 400 messages",
        || count(&journal) >= 400,
    );
    first.signal("KILL");
    let (status, _, _) = first.wait(SECS(10));
    assert!(!status.success(), "SIGKILL ends the recorder abnormally");
    let after_kill = count(&journal);
    let second = Proc::spawn(&cli(), &[&record_args[..], &["--json"]].concat());
    wait_until(
        SECS(60),
        "the restarted recorder to record 400 more messages",
        || count(&journal) >= after_kill + 400,
    );
    publisher.signal("KILL");
    let _ = publisher.wait(SECS(10));
    second.signal("TERM");
    let (status, out, err) = second.wait(SECS(20));
    assert!(status.success(), "second recorder: {err}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();

    let recs = scan(&journal); // checks gap-free sequence numbers from 0
    let descriptors = descriptors_of(&recs);
    let runs: std::collections::BTreeSet<u64> = descriptors.iter().map(|d| d.2).collect();
    assert_eq!(
        runs.len(),
        2,
        "two recording runs: {descriptors:?}\nsecond recorder: {out}\n{err}"
    );
    let second_run = *runs.last().unwrap();
    let second_descriptor = descriptors.iter().find(|d| d.2 == second_run).unwrap().0;
    assert_eq!(
        second_run, second_descriptor,
        "the second run starts where the first ended, with its descriptor"
    );
    assert_eq!(report["run_start_seq"], second_run);
    let data = data_of(&recs);
    let counters: Vec<u64> = data.iter().map(|(_, _, h, _)| counter_of(h)).collect();
    assert!(
        counters.windows(2).all(|w| w[0] < w[1]),
        "no message is recorded twice"
    );
    for (_, _, header, payload) in &data {
        expect_tick(header, payload, counter_of(header));
    }
    let first_run = data.iter().filter(|(_, d, _, _)| *d < second_run).count();
    assert!(first_run >= 390, "the first run's records survive the kill");
    assert!(
        data.len() >= first_run + 390,
        "the second run recorded more"
    );
    // The follower read the journal across the kill and the restart; everything it saw is in the journal.
    let (status, out, err) = follower.wait(SECS(20));
    assert!(status.success(), "follower: {err}");
    let (_, followed) = parse_follow(&out);
    assert_eq!(followed.len(), data.len());
    for (f, (seq, _, header, payload)) in followed.iter().zip(&data) {
        assert_eq!((f.seq, &f.header, &f.payload), (*seq, header, payload));
    }
}

/// Replay to a subscriber at the recorded pace: same bytes, same order, and the schedule kept within loose bounds.
#[test]
fn replay_matches_the_recording_in_bytes_order_and_pace() {
    let _serial = serial();
    const N: u64 = 300;
    let (dir, service) = record_ticks(N, 300);
    let journal = dir.path().join("j");
    let recs = scan(&journal);
    let data = data_of(&recs);
    let local: Vec<u64> = recs
        .iter()
        .filter_map(|r| match r {
            Rec::Data { local_ts_ns, .. } => Some(*local_ts_ns),
            _ => None,
        })
        .collect();
    let recorded_span = local.last().unwrap() - local.first().unwrap();

    for speed in ["1", "4"] {
        let target = format!("replay/{service}");
        let subscriber = Proc::spawn(
            &example("subscriber"),
            &["--service", &target, "--count", &N.to_string()],
        );
        let out = run_ok(
            &cli(),
            &[
                "replay",
                journal.to_str().unwrap(),
                "--speed",
                speed,
                "--wait-subscribers",
                "1",
                "--json",
            ],
            SECS(60),
        );
        let report: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(report["messages"], N);
        assert_eq!(report["services"][0]["target"], target.as_str());
        let (status, sub_out, err) = subscriber.wait(SECS(30));
        assert!(status.success(), "{err}");
        let received = parse_subscriber(&sub_out);
        assert_eq!(received.len() as u64, N);
        for ((h, p), (_, _, header, payload)) in received.iter().zip(&data) {
            assert_eq!((h, p), (header, payload), "replayed bytes and order");
        }
        let pacing = &report["pacing"];
        let speed: f64 = speed.parse().unwrap();
        let scheduled = pacing["last_scheduled_ns"].as_u64().unwrap();
        let actual = pacing["last_actual_ns"].as_u64().unwrap();
        let expected = (recorded_span as f64 / speed) as u64;
        assert!(
            scheduled.abs_diff(expected) < 1_000_000,
            "schedule {scheduled} vs recorded span / speed {expected}"
        );
        // Loose bounds: shared CI runners can deschedule a process for tens of milliseconds.
        assert!(
            actual >= scheduled && actual - scheduled < 250_000_000,
            "finished {actual} vs scheduled {scheduled}"
        );
        assert!(
            pacing["max_late_ns"].as_u64().unwrap() < 500_000_000,
            "{pacing}"
        );
    }

    // As fast as possible: no pacing report, same bytes.
    let target = format!("replay/{service}");
    let subscriber = Proc::spawn(
        &example("subscriber"),
        &["--service", &target, "--count", &N.to_string()],
    );
    let out = run_ok(
        &cli(),
        &[
            "replay",
            journal.to_str().unwrap(),
            "--wait-subscribers",
            "1",
            "--json",
        ],
        SECS(60),
    );
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(report["pacing"].is_null());
    let (_, sub_out, _) = subscriber.wait(SECS(30));
    assert_eq!(parse_subscriber(&sub_out).len() as u64, N);
}

/// One recorder records a fixed-size and a slice service into one journal (runtime types, feature `dynamic`); a
/// reader decodes both; both replay onto their own subscribers.
#[test]
fn one_recorder_records_and_replays_several_services() {
    let _serial = serial();
    const N: u64 = 400;
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let j = journal.to_str().unwrap();
    let (ticks, slices) = (unique("multi-ticks"), unique("multi-slices"));
    let a = blocking_publisher(&ticks, N, 1000, &[]);
    let b = blocking_publisher(&slices, N, 1000, &["--kind", "slice"]);
    let recorder = Proc::spawn(
        &cli(),
        &[
            "record",
            j,
            "--service",
            &ticks,
            "--service",
            &slices,
            "--counter",
            "header:0:8",
            "--json",
        ],
    );
    assert!(a.wait(SECS(60)).0.success());
    assert!(b.wait(SECS(60)).0.success());
    recorder.signal("INT");
    let (status, out, err) = recorder.wait(SECS(20));
    assert!(status.success(), "{err}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    for s in report["services"].as_array().unwrap() {
        assert_eq!(s["gaps"], 0, "lost before the recorder: {out}");
    }
    assert_eq!(report["messages"], 2 * N);

    let info: serde_json::Value =
        serde_json::from_str(&run_ok(&cli(), &["info", j, "--json"], SECS(20))).unwrap();
    assert_eq!(info["runs"].as_array().unwrap().len(), 1);
    let services = info["services"].as_array().unwrap();
    assert_eq!(services.len(), 2);
    for s in services {
        assert_eq!(s["messages"], N);
        assert_eq!(s["descriptors"], 1);
        assert_eq!(s["runs"], 1);
    }

    let (descriptors, followed) = parse_follow(&run_ok(
        &example("follow"),
        &[
            "--journal",
            j,
            "--count",
            &(2 * N).to_string(),
            "--idle-secs",
            "2",
        ],
        SECS(30),
    ));
    assert_eq!(descriptors.len(), 2);
    let mut next = [0u64; 2];
    for f in &followed {
        if f.service == ticks {
            expect_tick(&f.header, &f.payload, next[0]);
            next[0] += 1;
        } else {
            assert_eq!(f.service, slices);
            expect_slice(&f.header, &f.payload, next[1]);
            next[1] += 1;
        }
    }
    assert_eq!(next, [N, N]);

    let sub_ticks = Proc::spawn(
        &example("subscriber"),
        &[
            "--service",
            &format!("replay/{ticks}"),
            "--count",
            &N.to_string(),
        ],
    );
    let sub_slices = Proc::spawn(
        &example("subscriber"),
        &[
            "--service",
            &format!("replay/{slices}"),
            "--kind",
            "slice",
            "--count",
            &N.to_string(),
        ],
    );
    let out = run_ok(
        &cli(),
        &["replay", j, "--wait-subscribers", "1", "--json"],
        SECS(60),
    );
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["messages"], 2 * N);
    for (sub, slice) in [(sub_ticks, false), (sub_slices, true)] {
        let (status, out, err) = sub.wait(SECS(30));
        assert!(status.success(), "{err}");
        let received = parse_subscriber(&out);
        assert_eq!(received.len() as u64, N);
        for (i, (h, p)) in received.iter().enumerate() {
            if slice {
                expect_slice(h, p, i as u64);
            } else {
                expect_tick(h, p, i as u64);
            }
        }
    }
}

/// Export to iceoryx2's human-readable file format and import it into a new journal: message bytes and order
/// survive both directions. (The test below plays an exported file with upstream's own `iox2 service replay`; a
/// unit test checks the writer byte for byte against a file upstream's recorder wrote.) iox2dump export is gone.
#[test]
fn upstream_file_format_round_trip() {
    let _serial = serial();
    const N: u64 = 200;
    let (dir, service) = record_ticks(N, 1000);
    let journal = dir.path().join("j");
    let original = data_of(&scan(&journal));
    let file = dir.path().join("export-human-readable.iox2");
    let out = run_ok(
        &cli(),
        &[
            "export",
            journal.to_str().unwrap(),
            "--output",
            file.to_str().unwrap(),
            "--json",
        ],
        SECS(30),
    );
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["messages"], N);
    assert_eq!(report["service"], service.as_str());
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        text.starts_with(&format!("service_name = \"{service}\"\n")),
        "{text}"
    );
    let times: Vec<u64> = text
        .lines()
        .filter_map(|l| l.strip_prefix("time:     "))
        .map(|t| t.parse().unwrap())
        .collect();
    assert_eq!(times.len() as u64, N);
    assert!(times.windows(2).all(|w| w[0] <= w[1]));
    assert!(
        text.ends_with(" \n\n"),
        "every record ends with its newlines"
    );
    let (status, _, err) = run(
        &cli(),
        &[
            "export",
            journal.to_str().unwrap(),
            "--output",
            dir.path().join("dump").to_str().unwrap(),
            "--representation",
            "iox2-dump",
        ],
        SECS(30),
    );
    assert!(!status.success(), "iox2dump export is gone: {err}");
    assert!(!dir.path().join("dump").exists());

    let imported = dir.path().join("imported");
    let out = run_ok(
        &cli(),
        &[
            "import",
            file.to_str().unwrap(),
            imported.to_str().unwrap(),
            "--json",
        ],
        SECS(30),
    );
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["messages"], N);
    let recs = scan(&imported);
    let Rec::Descriptor(0, d) = &recs[0] else {
        panic!("descriptor first")
    };
    assert!(d.imported);
    assert_eq!(d.service_name, service);
    let data = data_of(&recs);
    assert_eq!(data.len() as u64, N);
    for ((_, _, h, p), (_, _, header, payload)) in data.iter().zip(&original) {
        assert_eq!((h, p), (header, payload));
    }
    // Refuses to overwrite.
    let file = dir.path().join("export-human-readable.iox2");
    let (status, _, err) = run(
        &cli(),
        &[
            "export",
            journal.to_str().unwrap(),
            "--output",
            file.to_str().unwrap(),
        ],
        SECS(30),
    );
    assert_eq!(status.code(), Some(1), "{err}");
}

/// The upstream `iox2 service replay` plays an exported file onto a subscriber. Needs the `iox2` binary
/// (`cargo install iceoryx2-cli@0.10.0`); skipped without it, unless TICKERTAPE_REQUIRE_IOX2 is set (as in CI),
/// which makes a missing `iox2` a failure.
#[test]
fn upstream_cli_replays_an_exported_file() {
    let _serial = serial();
    let have_iox2 = std::process::Command::new("iox2")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !have_iox2 {
        assert!(
            std::env::var_os("TICKERTAPE_REQUIRE_IOX2").is_none(),
            "iox2 is required but not on PATH"
        );
        eprintln!("skipping: iox2 not on PATH");
        return;
    }
    const N: u64 = 100;
    let (dir, _service) = record_ticks(N, 1000);
    let journal = dir.path().join("j");
    let original = data_of(&scan(&journal));
    let file = dir.path().join("export.iox2");
    run_ok(
        &cli(),
        &[
            "export",
            journal.to_str().unwrap(),
            "--output",
            file.to_str().unwrap(),
        ],
        SECS(30),
    );
    // iox2 service replay needs the service to exist: a subscriber process creates nothing, so open it here.
    let target = unique("iox2-replay");
    let node = NodeBuilder::new().create::<ipc::Service>().unwrap();
    let _service = node
        .service_builder(&ServiceName::new(&target).unwrap())
        .publish_subscribe::<Tick>()
        .user_header::<types::TickHeader>()
        .subscriber_max_buffer_size(1024)
        .create()
        .unwrap();
    let subscriber = Proc::spawn(
        &example("subscriber"),
        &["--service", &target, "--count", &N.to_string()],
    );
    wait_until(SECS(20), "the subscriber to connect", || {
        _service.dynamic_config().number_of_subscribers() >= 1
    });
    run_ok(
        std::path::Path::new("iox2"),
        &[
            "service",
            "replay",
            "--input",
            file.to_str().unwrap(),
            &target,
        ],
        SECS(60),
    );
    let (status, out, err) = subscriber.wait(SECS(30));
    assert!(status.success(), "{err}");
    let received = parse_subscriber(&out);
    assert_eq!(received.len() as u64, N);
    for ((h, p), (_, _, header, payload)) in received.iter().zip(&original) {
        assert_eq!((h, p), (header, payload));
    }
}

/// An existing target service with other types is refused with a clear message, not overwritten or fed.
#[test]
fn replay_refuses_an_incompatible_existing_service() {
    let _serial = serial();
    let (dir, service) = record_ticks(20, 1000);
    let journal = dir.path().join("j");
    let node = NodeBuilder::new().create::<ipc::Service>().unwrap();
    let _existing = node
        .service_builder(&ServiceName::new(&format!("replay/{service}")).unwrap())
        .publish_subscribe::<u64>()
        .create()
        .unwrap();
    let (status, _, err) = run(&cli(), &["replay", journal.to_str().unwrap()], SECS(30));
    assert_eq!(status.code(), Some(1), "{err}");
    assert!(err.contains("incompatible with the recording"), "{err}");
    assert!(err.contains("payload is FixedSize u64"), "{err}");
}

/// Replay remaps names by default; the original name needs an explicit flag.
#[test]
fn replay_remaps_by_default_and_refuses_the_original_name() {
    let _serial = serial();
    let (dir, service) = record_ticks(10, 1000);
    let j = dir.path().join("j");
    let j = j.to_str().unwrap();
    let report: serde_json::Value =
        serde_json::from_str(&run_ok(&cli(), &["replay", j, "--json"], SECS(30))).unwrap();
    assert_eq!(
        report["services"][0]["target"],
        format!("replay/{service}").as_str()
    );
    let report: serde_json::Value = serde_json::from_str(&run_ok(
        &cli(),
        &["replay", j, "--prefix", "again/", "--json"],
        SECS(30),
    ))
    .unwrap();
    assert_eq!(
        report["services"][0]["target"],
        format!("again/{service}").as_str()
    );

    let map = format!("{service}={service}");
    let (status, _, err) = run(&cli(), &["replay", j, "--map", &map], SECS(30));
    assert_eq!(status.code(), Some(1));
    assert!(
        err.contains("refusing to replay onto the recorded service name"),
        "{err}"
    );
    let (status, _, err) = run(&cli(), &["replay", j, "--prefix", ""], SECS(30));
    assert_eq!(status.code(), Some(1), "{err}");

    let report: serde_json::Value = serde_json::from_str(&run_ok(
        &cli(),
        &["replay", j, "--original-names", "--json"],
        SECS(30),
    ))
    .unwrap();
    assert_eq!(report["services"][0]["target"], service.as_str());
    assert_eq!(report["messages"], 10);
}

/// A publisher that skips counter values: the recorder writes loss records and reports the gaps.
#[test]
fn counter_gaps_become_loss_records() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let j = journal.to_str().unwrap();
    let service = unique("gaps");
    let recorder = Proc::spawn(
        &cli(),
        &[
            "record",
            j,
            "--service",
            &service,
            "--counter",
            "header:0:8",
            "--json",
        ],
    );
    run_ok(
        &example("publisher"),
        &[
            "--service",
            &service,
            "--count",
            "100",
            "--rate",
            "1000",
            "--skip-every",
            "10",
            "--wait-subscribers",
            "1",
        ],
        SECS(60),
    );
    recorder.signal("TERM");
    let (status, out, err) = recorder.wait(SECS(20));
    assert!(status.success(), "{err}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    // 100 messages; after every 10th one counter value is skipped, so 9 gaps fall between messages.
    assert_eq!(report["services"][0]["gaps"], 9, "{out}");
    assert_eq!(report["services"][0]["missing"], 9);
    let losses: Vec<_> = scan(&journal)
        .into_iter()
        .filter_map(|r| match r {
            Rec::Loss(_, l) => Some(l),
            _ => None,
        })
        .collect();
    assert_eq!(losses.len(), 9);
    assert!(
        losses
            .iter()
            .all(|l| l.kind == LossKind::Gap && l.received == l.expected + 1)
    );
    let info: serde_json::Value =
        serde_json::from_str(&run_ok(&cli(), &["info", j, "--json"], SECS(20))).unwrap();
    assert_eq!(info["services"][0]["missing_messages"], 9);
}

/// Exit codes and help follow the tickertape CLI's conventions.
#[test]
fn cli_exit_codes() {
    let (status, out, _) = run(&cli(), &["--help"], SECS(10));
    assert!(status.success());
    assert!(out.contains("Exit codes"));
    let (status, _, _) = run(&cli(), &["record"], SECS(10));
    assert_eq!(status.code(), Some(1), "usage error");
    let dir = tempfile::tempdir().unwrap();
    let (status, _, err) = run(
        &cli(),
        &["info", dir.path().join("missing").to_str().unwrap()],
        SECS(10),
    );
    assert_eq!(status.code(), Some(1), "{err}");
    // A record of this crate's schema with a malformed payload is a data error.
    let journal = dir.path().join("bad");
    let mut w = tickertape::Writer::open(
        &journal,
        tickertape::WriterConfig::default().prefault(false),
    )
    .unwrap();
    w.append(&tickertape::NewRecord {
        schema_id: tickertape_iceoryx2::layout::SCHEMA_DESCRIPTOR,
        venue_ts_ns: 0,
        local_ts_ns: 1,
        payload: b"not a descriptor",
    })
    .unwrap();
    drop(w);
    let (status, _, err) = run(&cli(), &["info", journal.to_str().unwrap()], SECS(10));
    assert_eq!(status.code(), Some(2), "{err}");
}

/// A user header aligned beyond what the layout accepts (4096), for a service the recorder must refuse.
#[derive(Debug, Default, Clone, Copy, ZeroCopySend)]
#[repr(C)]
#[repr(align(8192))]
struct OverAligned {
    value: u64,
}

/// A service that appears later and can't be recorded (its types are outside the layout's limits), or whose venue
/// timestamp offset doesn't fit its user header, is reported and retried; the other services keep recording. The
/// venue timestamp extractor reads the header at the given offset.
#[test]
fn per_service_failures_do_not_stop_the_others() {
    let _serial = serial();
    const N: u64 = 200;
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let j = journal.to_str().unwrap();
    let (good, bad) = (unique("good"), unique("bad"));
    let publisher = blocking_publisher(&good, N, 200, &[]);
    let recorder = Proc::spawn(
        &cli(),
        &[
            "record",
            j,
            "--service",
            &good,
            "--service",
            &bad,
            "--venue-ts-offset",
            "8",
            "--json",
        ],
    );
    // The bad service appears while the good one is being recorded.
    wait_until(SECS(20), "the first messages", || count(&journal) > 20);
    let node = NodeBuilder::new().create::<ipc::Service>().unwrap();
    let _bad = node
        .service_builder(&ServiceName::new(&bad).unwrap())
        .publish_subscribe::<u64>()
        .user_header::<OverAligned>()
        .create()
        .unwrap();
    let (status, _, err) = publisher.wait(SECS(60));
    assert!(status.success(), "{err}");
    recorder.signal("TERM");
    let (status, out, err) = recorder.wait(SECS(20));
    assert!(status.success(), "{err}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    let services = report["services"].as_array().unwrap();
    let (g, b) = if services[0]["name"] == good.as_str() {
        (&services[0], &services[1])
    } else {
        (&services[1], &services[0])
    };
    assert_eq!(g["messages"], N, "{out}");
    assert_eq!(g["errors"], 0, "{out}");
    assert_eq!(b["messages"], 0, "{out}");
    assert!(b["errors"].as_u64().unwrap() >= 1, "{out}");
    assert!(
        b["last_error"].as_str().unwrap().contains("alignment"),
        "{out}"
    );
    assert_eq!(
        err.lines().filter(|l| l.contains(bad.as_str())).count(),
        1,
        "the failure is reported once while it doesn't change: {err}"
    );
    // venue_ts_ns is the header's u64 at offset 8.
    let recs = scan(&journal);
    let mut data = 0;
    for r in &recs {
        if let Rec::Data {
            venue_ts_ns,
            header,
            ..
        } = r
        {
            assert_eq!(
                *venue_ts_ns,
                u64::from_le_bytes(header[8..16].try_into().unwrap())
            );
            assert!(*venue_ts_ns > 0);
            data += 1;
        }
    }
    assert_eq!(data, N);

    // A venue timestamp offset that doesn't fit the 16-byte header is refused for that service.
    let service = unique("offset");
    let publisher = blocking_publisher(&service, 5, 100, &["--wait-secs", "2"]);
    let journal = dir.path().join("k");
    let recorder = Proc::spawn(
        &cli(),
        &[
            "record",
            journal.to_str().unwrap(),
            "--service",
            &service,
            "--venue-ts-offset",
            "12",
            "--json",
        ],
    );
    let _ = publisher.wait(SECS(20));
    recorder.signal("TERM");
    let (status, out, err) = recorder.wait(SECS(20));
    assert!(status.success(), "{err}");
    assert!(err.contains("venue timestamp at offset 12"), "{err}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["messages"], 0, "{out}");
}
