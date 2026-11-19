//! Signals belong to one run: a recorder stopped by SIGTERM reports `Signal`, and a later run in the same process,
//! stopped by its stop flag, reports `StopFlag` (the flag that records a signal is not process-wide).
//!
//! This file holds a single test, because it raises SIGTERM in its own process.

#![cfg(feature = "iceoryx2")]

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use tickertape_iceoryx2::recorder::{Recorder, StopReason};

#[test]
fn a_signal_stops_only_the_run_it_arrived_in() {
    let dir = tempfile::tempdir().unwrap();
    let service = unique("signals");
    let run = |stop_with_signal: bool| {
        // The stopper starts once the run has installed its signal handlers (`on_start`).
        let (started, start) = std::sync::mpsc::channel();
        let recorder = Recorder::builder(dir.path().join("j"))
            .service_typed::<u64>(&service)
            .handle_signals(true)
            .max_duration(Duration::from_secs(30))
            .on_start(move |_| started.send(()).unwrap())
            .build()
            .unwrap();
        let stop = recorder.stop_flag();
        let stopper = std::thread::spawn(move || {
            start.recv().unwrap();
            if stop_with_signal {
                signal_hook::low_level::raise(signal_hook::consts::SIGTERM).unwrap();
            } else {
                stop.store(true, Ordering::Relaxed);
            }
        });
        let report = recorder.run().unwrap();
        stopper.join().unwrap();
        report.stop_reason
    };
    assert_eq!(run(true), StopReason::Signal);
    assert_eq!(run(false), StopReason::StopFlag);
}
