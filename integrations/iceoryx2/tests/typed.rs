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

/// With small segments and the writer's retention policy, every segment holds the descriptor of the records in it,
/// so every segment that survives can be decoded on its own: no record is left without its descriptor. With small
/// messages and with messages of about 1 KiB (1,008 bytes leave room for a descriptor at the end of each full
/// segment, which round 1 wrongly used).
#[test]
fn descriptors_repeat_per_segment_and_survive_retention() {
    let _serial = serial();
    retention_case(None, 4000);
    retention_case(Some(1008), 600);
}

fn retention_case(bytes: Option<usize>, n: u64) {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let service = unique("retention");
    let mut args = vec![
        "--service".to_string(),
        service.clone(),
        "--count".into(),
        n.to_string(),
        "--rate".into(),
        "4000".into(),
        "--wait-subscribers".into(),
        "1".into(),
        "--blocking".into(),
    ];
    if let Some(size) = bytes {
        args.extend([
            "--kind".into(),
            "bytes".into(),
            "--size".into(),
            size.to_string(),
        ]);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let publisher = Proc::spawn(&example("publisher"), &args);
    wait_for_service(&service, SECS(20));
    let counter = CounterField {
        location: CounterLocation::UserHeader,
        offset: 0,
        width: 8,
    };
    let builder = Recorder::builder(&journal);
    let builder = match bytes {
        None => builder.typed(TypedService::<Tick, TickHeader>::new(&service).counter(counter)),
        Some(_) => builder.typed(TypedService::<[u8], TickHeader>::new(&service).counter(counter)),
    };
    let recorder = builder
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
        .max_messages(n)
        .max_duration(Duration::from_secs(60))
        .build()
        .unwrap();
    let report = recorder.run().unwrap();
    let (status, _, err) = publisher.wait(SECS(30));
    assert!(status.success(), "{err}");
    assert_eq!(report.messages, n);
    assert!(report.services[0].descriptors >= 6, "{report:?}");

    let segments = tickertape::inspect::list_segments(&journal).unwrap();
    assert_eq!(segments.len(), 3, "retention kept three segments");
    assert!(segments[0].base_seq > 0, "older segments were removed");
    let placement = check_placement(&journal);
    assert_eq!(placement.exceptions, 0, "{placement:?}");
    assert_eq!(placement.not_leading, 0, "{placement:?}");
    assert_eq!(placement.undescribed, 0, "{placement:?}");
    let info = tickertape_iceoryx2::info::scan(&journal, false).unwrap();
    assert_eq!(info.undescribed_records, 0, "{info:?}");
    assert_eq!(info.filler_records, placement.fillers, "{info:?}");
    if bytes.is_some() {
        assert!(info.filler_records > 0, "{info:?}");
    }
    assert_eq!(info.runs.len(), 1);
    assert_eq!(info.services[0].runs, 1);
    let data = info.services[0].messages;
    assert!(data > 0 && data < n, "{data}");
    assert_eq!(data, placement.data);
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

/// Every recorded service's registration is checked before its target is opened or reused (review round 3): two
/// recorded `u64` services mapped onto one target don't share the first one's typed sink unless both are registered
/// with matching types, and a registration that doesn't match the recording is refused before the target is
/// created.
#[test]
fn every_registration_is_checked_before_a_target_is_opened_or_reused() {
    use iceoryx2::prelude::*;
    use tickertape_iceoryx2::replayer::Remap;
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let (a, b) = (unique("reg-a"), unique("reg-b"));
    let u64_type = || RawType::fixed("u64", 8, 8);
    let mut records = run_of(&RawDescriptor::new(&a, u64_type()), 0, 2);
    records.extend(run_of(&RawDescriptor::new(&b, u64_type()), 3, 2));
    append_records(&journal, 1 << 20, &records);
    let target = unique("reg-target");
    let remap = || Remap::default().rule(&a, &target).rule(&b, &target);
    let replay = |builder: tickertape_iceoryx2::replayer::ReplayerBuilder| {
        builder
            .remap(remap())
            .handle_signals(false)
            .build()
            .unwrap()
            .run()
    };
    // Only `a` registered: `b` is refused, not sent through `a`'s sink.
    let err = replay(Replayer::builder(&journal).typed::<u64, ()>(&a)).unwrap_err();
    assert!(matches!(err, Error::Usage(_)), "{err}");
    assert!(
        err.to_string().contains(&b) && err.to_string().contains("not registered"),
        "{err}"
    );
    // `b` registered as f64: refused, not ignored.
    let err = replay(
        Replayer::builder(&journal)
            .typed::<u64, ()>(&a)
            .typed::<f64, ()>(&b),
    )
    .unwrap_err();
    assert!(
        matches!(&err, Error::Incompatible { service, .. } if *service == b),
        "{err}"
    );
    // `a` registered as f64: refused for the recorded service, before the target is opened (or created).
    let err = replay(Replayer::builder(&journal).typed::<f64, ()>(&a)).unwrap_err();
    assert!(
        matches!(&err, Error::Incompatible { service, .. } if *service == a),
        "{err}"
    );
    let node = NodeBuilder::new()
        .signal_handling_mode(SignalHandlingMode::Disabled)
        .create::<ipc::Service>()
        .unwrap();
    let details = ipc::Service::details(
        &ServiceName::new(&target).unwrap(),
        node.config(),
        MessagingPattern::PublishSubscribe,
    )
    .unwrap();
    assert!(details.is_none(), "the target was not created");
    // Both registered with the recorded types: one target, both services.
    let report = replay(
        Replayer::builder(&journal)
            .typed::<u64, ()>(&a)
            .typed::<u64, ()>(&b),
    )
    .unwrap();
    assert_eq!(report.messages, 4);
    assert_eq!(report.services.len(), 1);
}

/// `Duration::MAX` for the subscriber wait, the linger time or the recorder's retry interval means "no limit", not
/// a panic adding it to the current time (review round 3).
#[test]
fn caller_supplied_durations_never_overflow() {
    use tickertape_iceoryx2::replayer::{Remap, ReplayEnd};
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let service = unique("durations");
    let report = Recorder::builder(&journal)
        .typed(TypedService::<u64>::new(&service))
        .retry_interval(Duration::MAX)
        .max_duration(Duration::from_millis(200))
        .handle_signals(false)
        .build()
        .unwrap()
        .run()
        .unwrap();
    assert_eq!(report.stop_reason, StopReason::MaxDuration);

    let journal = dir.path().join("r");
    append_records(
        &journal,
        1 << 20,
        &run_of(
            &RawDescriptor::new(&service, RawType::fixed("u64", 8, 8)),
            0,
            2,
        ),
    );
    for wait in [true, false] {
        let mut builder = Replayer::builder(&journal)
            .typed::<u64, ()>(&service)
            .remap(Remap::default().prefix(format!("{}/", unique("durations-target"))))
            .linger(Duration::MAX)
            .handle_signals(false);
        if wait {
            builder = builder.wait_for_subscribers(1, Duration::MAX);
        }
        let stop = builder.stop_flag();
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        let report = builder.build().unwrap().run().unwrap();
        stopper.join().unwrap();
        assert_eq!(report.messages, if wait { 0 } else { 2 }, "{report:?}");
        if wait {
            assert_eq!(report.end, ReplayEnd::StopFlag);
        }
    }
}

/// A padded `#[repr(C)]` type published through iceoryx2's typed API from the recorder's own process may leave its
/// padding uninitialised, which the recorder by name must not read (review round 3): such samples are refused, per
/// service, before any byte of them is viewed, and nothing is recorded.
#[cfg(feature = "dynamic")]
#[test]
fn the_recorder_by_name_refuses_samples_from_its_own_process() {
    use iceoryx2::prelude::*;
    use std::sync::{Arc, Mutex};
    use tickertape_iceoryx2::dynamic::DynamicService;

    #[derive(Debug, Clone, Copy, ZeroCopySend)]
    #[repr(C)]
    struct Padded {
        tag: u8,
        // Seven bytes of padding before this field.
        value: u64,
    }

    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let service = unique("same-process");
    let node = NodeBuilder::new()
        .signal_handling_mode(SignalHandlingMode::Disabled)
        .create::<ipc::Service>()
        .unwrap();
    let factory = node
        .service_builder(&ServiceName::new(&service).unwrap())
        .publish_subscribe::<Padded>()
        .create()
        .unwrap();
    let publisher = factory.publisher_builder().create().unwrap();
    let errors = Arc::new(Mutex::new(Vec::new()));
    let recorder = {
        let errors = Arc::clone(&errors);
        Recorder::builder(&journal)
            .dynamic(DynamicService::new(&service))
            .on_error(move |_, e| errors.lock().unwrap().push(e.to_string()))
            .max_duration(Duration::from_millis(1500))
            .handle_signals(false)
            .build()
            .unwrap()
    };
    let recording = std::thread::spawn(move || recorder.run().unwrap());
    let started = std::time::Instant::now();
    let mut i = 0u64;
    while started.elapsed() < Duration::from_millis(1300) {
        publisher
            .loan_uninit()
            .unwrap()
            .write_payload(Padded { tag: 7, value: i })
            .send()
            .unwrap();
        i += 1;
        std::thread::sleep(Duration::from_millis(5));
    }
    let report = recording.join().unwrap();
    assert_eq!(report.messages, 0, "{report:?}");
    let errors = errors.lock().unwrap();
    assert!(!errors.is_empty(), "{report:?}");
    assert!(
        errors.iter().all(|e| e.contains("recorder's own process")),
        "{errors:?}"
    );
    assert!(report.services[0].errors >= 1, "{report:?}");
    assert!(data_of_journal(&journal).is_empty());
}

#[cfg(feature = "dynamic")]
fn data_of_journal(journal: &std::path::Path) -> Vec<u64> {
    scan(journal)
        .into_iter()
        .filter_map(|r| match r {
            Rec::Data { seq, .. } => Some(seq),
            _ => None,
        })
        .collect()
}
