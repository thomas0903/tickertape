//! After a recorder or replayer run with signal handling has returned, `SIGINT` and `SIGTERM` have their default
//! action again: a library user's process can still be stopped with Ctrl-C.
//!
//! The test runs this test binary again as a child (selecting the `child` test with an environment variable): the
//! child runs a recorder and a replayer, stops both through their stop flags, prints a line and waits. The parent
//! then signals it and expects it to end by that signal.

#![cfg(feature = "iceoryx2")]

mod common;

use std::os::unix::process::ExitStatusExt;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use tickertape_iceoryx2::recorder::{Recorder, StopReason};
use tickertape_iceoryx2::replayer::{ReplayEnd, Replayer};

const CHILD: &str = "TT_SIGNAL_DEFAULT_CHILD";

/// Runs only as the child of `signals_have_their_default_action_after_a_run`.
#[test]
fn child() {
    let Ok(dir) = std::env::var(CHILD) else {
        return;
    };
    let journal = std::path::Path::new(&dir).join("j");
    let recorder = Recorder::builder(&journal)
        .service_typed::<u64>(unique("signal-default"))
        .handle_signals(true)
        .max_duration(Duration::from_secs(60))
        .build()
        .unwrap();
    let stop = recorder.stop_flag();
    stop.store(true, Ordering::Relaxed);
    assert_eq!(recorder.run().unwrap().stop_reason, StopReason::StopFlag);
    let replayer = Replayer::builder(&journal)
        .handle_signals(true)
        .build()
        .unwrap();
    replayer.stop_flag().store(true, Ordering::Relaxed);
    assert_eq!(replayer.run().unwrap().end, ReplayEnd::StopFlag);
    println!("runs finished");
    std::thread::sleep(Duration::from_secs(60));
    println!("still alive");
}

#[test]
fn signals_have_their_default_action_after_a_run() {
    if std::env::var(CHILD).is_ok() {
        return;
    }
    for (signal, number) in [("INT", 2), ("TERM", 15)] {
        let dir = tempfile::tempdir().unwrap();
        let exe = std::env::current_exe().unwrap();
        let child = Proc::spawn_with_env(
            &exe,
            &["--exact", "child", "--nocapture", "--test-threads", "1"],
            &[(CHILD, dir.path().to_str().unwrap())],
        );
        child.wait_for_output("runs finished", SECS(60));
        child.signal(signal);
        let (status, out, err) = child.wait(SECS(20));
        assert_eq!(
            status.signal(),
            Some(number),
            "SIG{signal}: {status}\n{out}\n{err}"
        );
        assert!(!out.contains("still alive"), "{out}");
    }
}
