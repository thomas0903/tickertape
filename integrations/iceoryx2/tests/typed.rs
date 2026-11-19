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
