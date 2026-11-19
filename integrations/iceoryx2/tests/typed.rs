//! The typed recorder and replayer as a library, using only iceoryx2's public API: record with
//! `Recorder::builder(..).typed(..)`, replay with `Replayer::builder(..).typed::<P, H>(..)`, and refuse a typed
//! registration whose Rust types don't match the recording.

#![cfg(feature = "iceoryx2")]

mod common;

#[path = "../examples/common/types.rs"]
mod types;

use std::time::Duration;

use common::*;
use tickertape_iceoryx2::Error;
use tickertape_iceoryx2::layout::{CounterField, CounterLocation};
use tickertape_iceoryx2::recorder::{Recorder, StopReason, TypedService};
use tickertape_iceoryx2::replayer::Replayer;
use types::{Tick, TickHeader};
use zerocopy::IntoBytes;

#[test]
fn typed_record_then_typed_replay() {
    let _serial = serial();
    const N: u64 = 200;
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let service = unique("lib-typed");

    let recorder = Recorder::builder(&journal)
        .typed(
            TypedService::<Tick, TickHeader>::new(&service)
                .venue_ts(|h| h.venue_ts_ns)
                .counter(CounterField {
                    location: CounterLocation::UserHeader,
                    offset: 0,
                    width: 8,
                }),
        )
        .handle_signals(false)
        .max_messages(N)
        .max_duration(Duration::from_secs(60))
        .build()
        .unwrap();
    let recording = std::thread::spawn(move || recorder.run());
    run_ok(
        &example("publisher"),
        &[
            "--service",
            &service,
            "--count",
            &N.to_string(),
            "--rate",
            "2000",
            "--wait-subscribers",
            "1",
        ],
        SECS(60),
    );
    let report = recording.join().unwrap().unwrap();
    assert_eq!(report.messages, N);
    assert_eq!(report.stop_reason, StopReason::MaxMessages);
    assert_eq!(report.services[0].gaps, 0);

    let target = format!("replay/{service}");
    let subscriber = Proc::spawn(
        &example("subscriber"),
        &["--service", &target, "--count", &N.to_string()],
    );
    let report = Replayer::builder(&journal)
        .typed::<Tick, TickHeader>(&service)
        .handle_signals(false)
        .wait_for_subscribers(1, Duration::from_secs(20))
        .linger(Duration::from_millis(500))
        .build()
        .unwrap()
        .run()
        .unwrap();
    assert_eq!(report.messages, N);
    let (status, out, err) = subscriber.wait(SECS(30));
    assert!(status.success(), "{err}");
    let received = parse_subscriber(&out);
    let counters: Vec<u64> = received.iter().map(|(h, _)| counter_of(h)).collect();
    assert_eq!(
        received.len() as u64,
        N,
        "received counters {counters:?}\n{err}"
    );
    for (i, (h, p)) in received.iter().enumerate() {
        assert_eq!(counter_of(h), i as u64);
        assert_eq!(p.as_slice(), Tick::for_counter(i as u64).as_bytes());
    }

    // A typed registration with the wrong Rust type is refused before anything is sent.
    let err = Replayer::builder(&journal)
        .typed::<u64, TickHeader>(&service)
        .remap(tickertape_iceoryx2::replayer::Remap::default().prefix("wrong/"))
        .handle_signals(false)
        .build()
        .unwrap()
        .run()
        .unwrap_err();
    assert!(matches!(err, Error::Incompatible { .. }), "{err}");
    assert!(err.to_string().contains("registered Rust types"), "{err}");
}

/// With small segments and retention, the recorder writes every open service's descriptor at the start of each
/// segment, so every segment that survives can be decoded on its own: no record is left without its descriptor.
#[test]
fn descriptors_repeat_per_segment_and_survive_retention() {
    let _serial = serial();
    const N: u64 = 4000;
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let service = unique("retention");
    let publisher = Proc::spawn(
        &example("publisher"),
        &[
            "--service",
            &service,
            "--count",
            &N.to_string(),
            "--rate",
            "4000",
            "--wait-subscribers",
            "1",
            "--blocking",
        ],
    );
    wait_for_service(&service, SECS(20));
    let recorder = Recorder::builder(&journal)
        .typed(
            TypedService::<Tick, TickHeader>::new(&service).counter(CounterField {
                location: CounterLocation::UserHeader,
                offset: 0,
                width: 8,
            }),
        )
        .writer_config(
            tickertape::WriterConfig::default()
                .segment_size(65_536)
                .prefault(false)
                .retention(
                    tickertape::retention::Policy::default().max_segments(3),
                    tickertape::retention::Action::Delete,
                ),
        )
        .checkpoint_interval(Duration::from_millis(20))
        .handle_signals(false)
        .max_messages(N)
        .max_duration(Duration::from_secs(60))
        .build()
        .unwrap();
    let report = recorder.run().unwrap();
    let (status, _, err) = publisher.wait(SECS(30));
    assert!(status.success(), "{err}");
    assert_eq!(report.messages, N);
    assert!(report.services[0].descriptors >= 6, "{report:?}");

    let segments = tickertape::inspect::list_segments(&journal).unwrap();
    assert_eq!(segments.len(), 3, "retention kept three segments");
    assert!(segments[0].base_seq > 0, "older segments were removed");
    for segment in &segments {
        let mut reader =
            tickertape::Reader::open(&journal, tickertape::ReaderConfig::default()).unwrap();
        reader.seek(segment.base_seq).unwrap();
        let first = reader.try_next().unwrap().unwrap();
        assert_eq!(
            first.schema_id,
            tickertape_iceoryx2::layout::SCHEMA_DESCRIPTOR,
            "segment at {} starts with the descriptor",
            segment.base_seq
        );
    }
    let info = tickertape_iceoryx2::info::scan(&journal, false).unwrap();
    assert_eq!(info.undescribed_records, 0, "{info:?}");
    assert_eq!(info.runs.len(), 1);
    assert_eq!(info.services[0].runs, 1);
    let data = info.services[0].messages;
    assert!(data > 0 && data < N, "{data}");
}

/// The safe `Replayer::run` replays only services registered with their Rust types; everything else needs the
/// `unsafe` `run_dynamic`, whose caller vouches for the recorded bytes.
#[test]
fn safe_replay_refuses_services_without_rust_types() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let service = unique("untyped");
    append_records(
        &journal,
        1 << 20,
        &run_of(
            &RawDescriptor::new(&service, RawType::fixed("u64", 8, 8)),
            0,
            2,
        ),
    );
    let err = Replayer::builder(&journal)
        .handle_signals(false)
        .build()
        .unwrap()
        .run()
        .unwrap_err();
    assert!(matches!(err, Error::Usage(_)), "{err}");
    assert!(err.to_string().contains("run_dynamic"), "{err}");
    // Registered with matching Rust types, it replays.
    let report = Replayer::builder(&journal)
        .typed::<u64, ()>(&service)
        .remap(
            tickertape_iceoryx2::replayer::Remap::default()
                .prefix(format!("{}/", unique("typed-target"))),
        )
        .handle_signals(false)
        .build()
        .unwrap()
        .run()
        .unwrap();
    assert_eq!(report.messages, 2);
}
