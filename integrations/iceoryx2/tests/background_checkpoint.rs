//! The recorder's periodic checkpoints run in the background (ADR 0038); one that fails stops the recorder through its
//! journal error path. A test binary of its own: the core's background failure points are process-wide.

#![cfg(feature = "iceoryx2")]

mod common;

#[path = "../examples/common/types.rs"]
mod types;

use std::time::{Duration, Instant};

use common::*;
use tickertape::fault::{self, Point};
use tickertape_iceoryx2::Error;
use tickertape_iceoryx2::recorder::{Recorder, TypedService};
use types::{Tick, TickHeader};

const EIO: i32 = 5;

#[test]
fn failed_background_checkpoint_stops_the_recorder() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let service = unique("background-failure");
    fault::fail_next_background(Point::CheckpointSync, EIO);
    let started = Instant::now();
    let result = Recorder::builder(&journal)
        .typed(TypedService::<Tick, TickHeader>::new(&service))
        .writer_config(
            tickertape::WriterConfig::default()
                .segment_size(65_536)
                .prefault(false),
        )
        .checkpoint_interval(Duration::from_millis(10))
        .handle_signals(false)
        .max_duration(Duration::from_secs(60))
        .build()
        .unwrap()
        .run();
    // The next periodic request returns the failure (once); the recorder stops with it, and the final checkpoint
    // writes no marker for a poisoned writer.
    match result {
        Err(Error::Journal(tickertape::Error::Io { source, .. })) => {
            assert_eq!(source.raw_os_error(), Some(EIO));
        }
        other => panic!("expected the background checkpoint's I/O error, got {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the recorder stopped at a request, not at max_duration"
    );
    assert_eq!(
        tickertape::inspect::read_checkpoint(&journal).unwrap(),
        None
    );
}
