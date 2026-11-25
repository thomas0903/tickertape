//! Functional smoke tests, never recorded benchmark results.
use serde_json::Value;
use std::process::Command;

/// Two threads on two CPUs, the writer's helpers on a third: what the kernel says, not what was asked for.
#[cfg(target_os = "linux")]
#[test]
fn latency_threads_run_on_the_requested_cpus() {
    let allowed = tickertape_bench::affinity::report().unwrap()["cpus_allowed"].clone();
    let cpus: Vec<u64> = allowed
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_u64().unwrap())
        .collect();
    let [.., writer, reader, helper] = cpus[..] else {
        eprintln!("skipped: needs three CPUs");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tt-latency"))
        .args(["run", "--dir"])
        .arg(root.path().join("journal"))
        .args(["--rate", "10000", "--count", "1000", "--payload", "64"])
        .args(["--reader", "mapped", "--json"])
        .args(["--writer-cpu", &writer.to_string()])
        .args(["--reader-cpu", &reader.to_string()])
        .args(["--helper-cpus", &helper.to_string()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    for (role, cpu) in [("writer", writer), ("reader", reader)] {
        let affinity = &result["affinity"][role];
        assert_eq!(affinity["cpus_allowed"], serde_json::json!([cpu]));
        assert_eq!(affinity["last_cpu"], cpu);
    }
    let helpers = result["affinity"]["writer"]["other_threads"]
        .as_array()
        .unwrap();
    assert!(!helpers.is_empty());
    for thread in helpers {
        assert_eq!(
            thread["cpus_allowed"],
            serde_json::json!([helper]),
            "{thread}"
        );
    }
}

#[test]
fn latency_both_readers_and_warmup() {
    for reader in ["safe", "mapped"] {
        let root = tempfile::tempdir().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_tt-latency"))
            .args(["run", "--dir"])
            .arg(root.path().join("journal"))
            .args([
                "--rate",
                "10000",
                "--count",
                "1000",
                "--payload",
                "64",
                "--reader",
                reader,
                "--warmup",
                "100",
                "--segment-size",
                "65536",
                "--json",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["count"], 1000);
        assert_eq!(result["latency"]["count"], 900);
        assert_eq!(result["reader"], reader);
        assert!(result["latency"].get("affinity").is_none());
        for role in ["writer", "reader"] {
            let affinity = &result["affinity"][role];
            if cfg!(target_os = "linux") {
                assert!(!affinity["cpus_allowed"].as_array().unwrap().is_empty());
            } else {
                assert!(affinity["unsupported"].is_string());
            }
        }
        assert!(result["writer"]["achieved_records_per_s"].as_f64().unwrap() > 0.0);
        assert!(result["writer"]["late_records"].as_u64().unwrap() <= 1000);
        assert!(
            result["latency"]["max_us"].as_f64().unwrap()
                >= result["latency"]["p50_us"].as_f64().unwrap()
        );
        // Timestamps must preserve the schedule rather than actual append times.
        let mut journal = tickertape::Reader::open(
            root.path().join("journal"),
            tickertape::ReaderConfig::default(),
        )
        .unwrap();
        let t0 = journal.try_next().unwrap().unwrap().local_ts_ns;
        for i in 1..1000 {
            let record = journal.try_next().unwrap().unwrap();
            assert_eq!(
                record.local_ts_ns,
                tickertape_bench::intended_ns(t0, i, 10_000).unwrap()
            );
        }
        // Existing data must survive a refused rerun.
        let refused = Command::new(env!("CARGO_BIN_EXE_tt-latency"))
            .args(["run", "--dir"])
            .arg(root.path().join("journal"))
            .args([
                "--rate",
                "1000",
                "--count",
                "1",
                "--payload",
                "64",
                "--reader",
                reader,
            ])
            .output()
            .unwrap();
        assert!(!refused.status.success());
    }
}

#[test]
fn latency_wait_modes_and_no_prefault() {
    for (wait, prefault) in [("default", true), ("block", true), ("sleep", false)] {
        let root = tempfile::tempdir().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_tt-latency"))
            .args(["run", "--dir"])
            .arg(root.path().join("journal"))
            .args([
                "--rate",
                "5000",
                "--count",
                "500",
                "--payload",
                "64",
                "--reader",
                "safe",
                "--wait",
                wait,
                "--json",
            ])
            .args((!prefault).then_some("--no-prefault"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{wait}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["wait"], wait);
        assert_eq!(result["prefault"], prefault);
        assert_eq!(result["latency"]["count"], 500);
    }
}

#[test]
fn cold_replay_with_mock_purge_and_unavailable_purge() {
    use std::os::unix::fs::PermissionsExt;
    // Mock sudo so this test never requests privileges or changes the host cache.
    for purge_succeeds in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let sudo = root.path().join("sudo");
        std::fs::write(
            &sudo,
            if purge_succeeds {
                "#!/bin/sh\nexit 0\n"
            } else {
                "#!/bin/sh\nexit 1\n"
            },
        )
        .unwrap();
        std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_tt-latency"))
            .env("PATH", root.path())
            .args(["run", "--dir"])
            .arg(root.path().join("journal"))
            .args([
                "--rate",
                "10000",
                "--count",
                "1000",
                "--payload",
                "64",
                "--reader",
                "mapped",
                "--segment-size",
                "65536",
                "--cold",
                "--json",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if purge_succeeds {
            let result: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(result["mode"], "cold_replay");
            assert_eq!(result["replay"]["count"], 1000);
            assert!(result.get("latency").is_none());
            let records_per_s = result["replay"]["records_per_s"].as_f64().unwrap();
            let mb_per_s = result["replay"]["payload_mb_per_s"].as_f64().unwrap();
            assert!((mb_per_s / records_per_s - 64.0 / 1_000_000.0).abs() < 1e-10);
        } else {
            assert_eq!(
                String::from_utf8(output.stdout).unwrap().trim(),
                "cold cache unavailable (needs root)"
            );
        }
    }
}

#[test]
fn checkpoint_separates_rolls_and_empty_checkpoint() {
    let root = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tt-checkpoint"))
        .arg("--dir")
        .arg(root.path().join("journal"))
        .args([
            "--payload",
            "1024",
            "--batch",
            "100",
            "--checkpoints",
            "2",
            "--segment-size",
            "65536",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["checkpoint"]["count"], 2);
    assert_eq!(result["empty_checkpoint"]["count"], 1);
    assert_eq!(result["roll_append"]["count"], 3);
    assert_eq!(result["other_append"]["count"], 197);
}

#[test]
fn invalid_inputs_fail_before_creating_a_journal() {
    for options in [
        vec!["--rate", "0"],
        vec!["--count", "0"],
        vec!["--warmup", "10"],
        vec!["--reader", "invalid"],
        vec!["--wait", "invalid"],
        vec!["--segment-size", "1"],
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal");
        let mut args = vec![
            "run",
            "--rate",
            "100",
            "--count",
            "10",
            "--payload",
            "64",
            "--reader",
            "safe",
        ];
        // Replace an existing option rather than duplicating it.
        if let Some(i) = args.iter().position(|arg| *arg == options[0]) {
            args[i + 1] = options[1];
        } else {
            args.extend(options);
        }
        let output = Command::new(env!("CARGO_BIN_EXE_tt-latency"))
            .args(args)
            .arg("--dir")
            .arg(&path)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!path.exists());
    }
}

fn sustained(root: &std::path::Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_tt-sustained"))
        .args(["run", "--dir"])
        .arg(root.join("journal"))
        .arg("--out")
        .arg(root.join("out"))
        .args(args)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let saved: Value =
        serde_json::from_slice(&std::fs::read(root.join("out/result.json")).unwrap()).unwrap();
    assert_eq!(result, saved);
    result
}

fn journal_timestamps(dir: &std::path::Path) -> Vec<u64> {
    let mut reader = tickertape::Reader::open(dir, tickertape::ReaderConfig::default()).unwrap();
    let mut timestamps = Vec::new();
    while let Some(record) = reader.try_next().unwrap() {
        assert_eq!(record.seq, timestamps.len() as u64);
        timestamps.push(record.local_ts_ns);
    }
    timestamps
}

#[test]
fn sustained_steady_run_with_checkpoints_rolls_and_two_readers() {
    let root = tempfile::tempdir().unwrap();
    let result = sustained(
        root.path(),
        &[
            "--rate",
            "5000",
            "--duration",
            "2s",
            "--warmup",
            "500ms",
            "--payload",
            "1024",
            "--segment-size",
            "65536",
            "--checkpoint-records",
            "1000",
            "--readers",
            "safe:wait=default,mapped:durable:wait=block",
        ],
    );
    let writer = &result["writer"];
    // 2.5 s at 5,000 records/s, every one sent: no skipped or rebased sends.
    assert_eq!(writer["count"], 12_500);
    assert_eq!(writer["planned_records"], 12_500);
    assert_eq!(writer["unsent_records"], 0);
    assert_eq!(writer["stopped_by"], "schedule_end");
    // Records intended in the 500 ms warmup are excluded from latency statistics only.
    assert_eq!(writer["records_measured"], 10_000);
    assert_eq!(writer["append_latency"]["count"], 10_000);
    assert_eq!(writer["checkpoints"], 12);
    assert!(writer["final_checkpoint_us"].as_f64().unwrap() > 0.0);
    // 61 records of 1,064 bytes fit in a 64 KiB segment; the first segment is created on open.
    assert_eq!(writer["rolls"], (12_500 - 1) / 61);
    assert!(writer["roll_append_latency"]["count"].as_u64().unwrap() > 0);
    assert!(writer["cpu"]["user_cpu_s"].as_f64().unwrap() >= 0.0);
    assert!(writer["late_records"].as_u64().unwrap() <= 12_500);
    assert_eq!(result["config"]["record_bytes"], 1064);
    assert_eq!(result["config"]["schedule"]["kind"], "steady");
    assert_eq!(result["dataset"]["appended_bytes"], 12_500 * 1064);
    let readers = result["readers"].as_array().unwrap();
    assert_eq!(readers.len(), 2);
    assert_eq!(readers[0]["latency_kind"], "visible");
    assert_eq!(readers[1]["latency_kind"], "durable_only");
    assert_eq!(readers[1]["wait"], "block");
    for reader in readers {
        assert_eq!(reader["records_read"], 12_500);
        assert_eq!(reader["remaining_records"], 0);
        assert_eq!(reader["skipped_by_retention"], 0);
        assert_eq!(reader["drained"], true);
        assert_eq!(reader["latency"]["count"], 10_000);
        assert!(reader["lag"]["samples"].as_u64().unwrap() > 0);
        assert!(reader["lag"]["max_bytes"].as_f64().unwrap() >= 0.0);
        let p50 = reader["latency"]["p50_us"].as_f64().unwrap();
        assert!(reader["latency"]["max_us"].as_f64().unwrap() >= p50);
    }
    // Paced records carry their intended send time.
    let t0 = result["start_unix_ns"].as_u64().unwrap();
    let schedule = tickertape_bench::sustained::Schedule::steady(5000).unwrap();
    for (i, ts) in journal_timestamps(&root.path().join("journal"))
        .iter()
        .enumerate()
    {
        assert_eq!(*ts, schedule.intended_ns(t0, i as u64).unwrap());
    }
    // One merged line per wall-clock second, covering every record sent.
    let series: Vec<Value> = std::fs::read_to_string(root.path().join("out/series.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        series.len() as u64,
        result["series_seconds"].as_u64().unwrap()
    );
    assert!(series.len() >= 3);
    let seconds: Vec<u64> = series
        .iter()
        .map(|l| l["t_unix_s"].as_u64().unwrap())
        .collect();
    assert!(seconds.windows(2).all(|w| w[1] == w[0] + 1));
    assert_eq!(seconds[0], t0 / 1_000_000_000);
    let sent: f64 = series
        .iter()
        .map(|l| l["writer"]["records"].as_f64().unwrap())
        .sum();
    assert_eq!(sent, 12_500.0);
    let received: f64 = series
        .iter()
        .map(|l| l["readers"][1]["records"].as_f64().unwrap())
        .sum();
    assert_eq!(received, 12_500.0);
    assert!(series[0]["t_utc"].as_str().unwrap().ends_with('Z'));
}

/// Both checkpoint modes record the call time, the call-to-durable time and the writer's pause, per checkpoint; a
/// durable-only reader receives every record in background mode too. The helper-thread and early write-back switches
/// reach the writer's configuration and the result, which carries the page cache's maxima (Linux; null elsewhere).
#[test]
fn sustained_checkpoint_modes_record_durable_time_and_pause() {
    for (mode, extra, early_writeback, helper_cpus) in [
        (
            "blocking",
            &["--no-early-writeback", "--helper-cpus", "inherited"][..],
            false,
            serde_json::json!("inherited"),
        ),
        (
            "background",
            &["--helper-cpus", "0"][..],
            true,
            serde_json::json!([0]),
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut options = extra.to_vec();
        options.extend([
            "--rate",
            "5000",
            "--duration",
            "1s",
            "--payload",
            "256",
            "--segment-size",
            "65536",
            "--checkpoint-every",
            "100ms",
            "--checkpoint-mode",
            mode,
            "--readers",
            "safe:durable:wait=block",
        ]);
        let result = sustained(root.path(), &options);
        let writer = &result["writer"];
        assert_eq!(result["config"]["early_writeback"], early_writeback);
        assert_eq!(result["config"]["helper_cpus"], helper_cpus);
        for field in ["dirty_max_bytes", "writeback_max_bytes"] {
            assert_eq!(
                writer["page_cache"][field].is_u64(),
                cfg!(target_os = "linux"),
                "{mode}: {}",
                writer["page_cache"]
            );
        }
        assert_eq!(writer["checkpoint_mode"], mode);
        assert_eq!(result["config"]["checkpoint"]["mode"], mode);
        assert_eq!(writer["count"], 5_000);
        let checkpoints = writer["checkpoints"].as_u64().unwrap();
        // Ten are scheduled; a blocking checkpoint took about 300 ms on a busy CI runner (three in the run).
        assert!(checkpoints >= 2, "{mode}: {checkpoints}");
        assert_eq!(writer["checkpoint_latency"]["count"], checkpoints, "{mode}");
        assert_eq!(
            writer["checkpoint_durable_latency"]["count"], checkpoints,
            "{mode}"
        );
        assert_eq!(writer["checkpoint_pause"]["count"], checkpoints, "{mode}");
        let call = writer["checkpoint_latency"]["max_us"].as_f64().unwrap();
        let pause = writer["checkpoint_pause"]["max_us"].as_f64().unwrap();
        assert!(pause >= call * 0.99, "{mode}: pause {pause} < call {call}");
        assert!(
            writer["checkpoint_durable_latency"]["max_us"]
                .as_f64()
                .unwrap()
                > 0.0
        );
        if mode == "blocking" {
            assert_eq!(writer["checkpoint_requests_while_in_flight"], 0);
        }
        let reader = &result["readers"][0];
        assert_eq!(reader["latency_kind"], "durable_only");
        assert_eq!(reader["records_read"], 5_000, "{mode}");
    }
}

#[test]
fn sustained_bursty_schedule_is_stored_in_the_journal() {
    let root = tempfile::tempdir().unwrap();
    let result = sustained(
        root.path(),
        &[
            "--burst-rate",
            "20000",
            "--burst-period",
            "200ms",
            "--burst-duty",
            "25",
            "--duration",
            "1s",
            "--readers",
            "mapped",
        ],
    );
    // Bursts of 1,000 records every 200 ms: 5,000 records, a mean of 5,000 records/s.
    assert_eq!(result["writer"]["count"], 5_000);
    assert_eq!(result["config"]["schedule"]["burst_records"], 1_000);
    assert_eq!(result["config"]["schedule"]["mean_records_per_s"], 5_000.0);
    assert_eq!(result["readers"][0]["records_read"], 5_000);
    let t0 = result["start_unix_ns"].as_u64().unwrap();
    let schedule = tickertape_bench::sustained::Schedule::bursty(
        20_000,
        std::time::Duration::from_millis(200),
        25,
    )
    .unwrap();
    let timestamps = journal_timestamps(&root.path().join("journal"));
    assert_eq!(timestamps.len(), 5_000);
    for (i, ts) in timestamps.iter().enumerate() {
        assert_eq!(*ts, schedule.intended_ns(t0, i as u64).unwrap());
    }
    assert_eq!(
        timestamps[1_000] - timestamps[999],
        200_000_000 - 999 * 50_000
    );
}

#[test]
fn sustained_unpaced_retention_removes_segments_under_a_lagging_reader() {
    // Whether retention overtakes the lagging reader within a short run depends on how fast the writer gets on
    // this machine at this moment (a loaded runner has given it as few as 60 records/s). No fixed duration makes
    // that certain, so: every run must satisfy the accounting, and the test repeats the run, for longer each time,
    // until one shows the reader being overtaken. The settings make that the usual first outcome: the reader
    // stalls for 400 ms in every 500 ms, a 64 KiB segment holds 15 of the 4 KiB records, and the writer checkpoints
    // every 15 records, so retention (two segments) acts after every roll. The longer attempts are for machines
    // under heavy load, where two seconds of writing have not always been enough even after several tries.
    const DURATIONS: [&str; 5] = ["2s", "2s", "4s", "8s", "16s"];
    let mut last = Value::Null;
    for duration in DURATIONS {
        let root = tempfile::tempdir().unwrap();
        let result = sustained(
            root.path(),
            &[
                "--unpaced",
                "--duration",
                duration,
                "--payload",
                "4096",
                "--segment-size",
                "65536",
                "--checkpoint-records",
                "15",
                "--retain-segments",
                "2",
                "--readers",
                "safe:wait=sleep,safe:stall=400ms/500ms",
            ],
        );
        let writer = &result["writer"];
        let count = writer["count"].as_u64().unwrap();
        assert_eq!(writer["stopped_by"], "duration");
        assert_eq!(writer["late_records"], Value::Null);
        assert_eq!(writer["unsent_records"], Value::Null);
        // The lagging reader stops when the writer finishes; every record is read, skipped or reported as
        // remaining, whether or not retention overtook it.
        let lagging = &result["readers"][1];
        assert_eq!(lagging["lagging"], true);
        let accounted = ["records_read", "skipped_by_retention", "remaining_records"]
            .iter()
            .map(|k| lagging[*k].as_u64().unwrap())
            .sum::<u64>();
        assert_eq!(accounted, count);
        assert_eq!(
            result["coordinated_omission"],
            "unpaced: latency measured from the actual send start"
        );
        let overtaken = lagging["skipped_by_retention"].as_u64().unwrap() > 0
            && lagging["discard_events"].as_u64().unwrap() > 0;
        if overtaken {
            assert!(writer["retention"]["removed_segments"].as_u64().unwrap() > 0);
            assert!(lagging["lag"]["max_records"].as_f64().unwrap() > 0.0);
            return;
        }
        last = serde_json::json!({ "writer": writer, "lagging_reader": lagging });
    }
    panic!(
        "retention never overtook the lagging reader in {} runs of up to 16 s; last run: {last}",
        DURATIONS.len()
    );
}

#[test]
fn sustained_rejects_invalid_configurations_before_creating_directories() {
    for options in [
        vec!["--rate", "100", "--unpaced"],
        vec!["--duration", "1s"],
        vec!["--rate", "0"],
        vec!["--rate", "100", "--duration", "0s"],
        vec!["--rate", "100", "--retain-segments", "2"],
        vec!["--rate", "100", "--readers", "safe:durable"],
        vec!["--rate", "100", "--readers", "fast"],
        vec![
            "--rate",
            "100",
            "--checkpoint-every",
            "1s",
            "--checkpoint-records",
            "5",
        ],
        vec!["--rate", "100", "--burst-duty", "5"],
        vec!["--burst-rate", "1", "--burst-period", "1ms"],
        vec!["--rate", "100", "--cpus", "0", "--readers", "safe"],
        vec!["--rate", "100", "--segment-size", "1000"],
        vec!["--rate", "100", "--lag-interval", "10us"],
        vec!["--rate", "100", "--checkpoint-mode", "background"],
        vec![
            "--rate",
            "100",
            "--checkpoint-every",
            "1s",
            "--checkpoint-mode",
            "later",
        ],
        vec!["--rate", "100", "--helper-cpus", "some"],
    ] {
        let root = tempfile::tempdir().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_tt-sustained"))
            .arg("run")
            .args(&options)
            .arg("--dir")
            .arg(root.path().join("journal"))
            .arg("--out")
            .arg(root.path().join("out"))
            .output()
            .unwrap();
        assert!(!output.status.success(), "{options:?}");
        assert!(!root.path().join("journal").exists(), "{options:?}");
        assert!(!root.path().join("out").exists(), "{options:?}");
    }
}

#[test]
fn scale_measures_every_operation_on_a_tiny_journal() {
    use std::os::unix::fs::PermissionsExt;
    // Mock sudo so this test never requests privileges or changes the host cache.
    for drop_succeeds in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let sudo = root.path().join("sudo");
        let script = if drop_succeeds {
            "#!/bin/sh\nexit 0\n"
        } else {
            "#!/bin/sh\nexit 1\n"
        };
        std::fs::write(&sudo, script).unwrap();
        std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dir = root.path().join("journal");
        let output = Command::new(env!("CARGO_BIN_EXE_tt-scale"))
            .env("PATH", root.path())
            .args(["run", "--dir"])
            .arg(&dir)
            .args([
                "--segments",
                "3",
                "--segment-size",
                "65536",
                "--payload",
                "100",
                "--tail-records",
                "7",
                "--repeats",
                "2",
                "--count",
                "1",
                "--cold",
                "--json",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        let built = &result["build"];
        let capacity = built["records_per_segment"].as_u64().unwrap();
        let records = built["records"].as_u64().unwrap();
        assert_eq!(records, 2 * capacity + capacity / 2);
        let measure = &result["measure"];
        assert_eq!(measure["journal"]["segments"], 3);
        assert_eq!(measure["journal"]["records"], records);
        assert_eq!(measure["cold_unavailable"], !drop_succeeds);
        let runs = measure["runs"].as_array().unwrap();
        assert_eq!(runs.len(), tickertape_bench::scale::OPS.len() * 2);
        for run in runs {
            assert_eq!(run["cache_dropped"], drop_succeeds, "{run}");
            assert!(run["wall_s"].as_f64().unwrap() >= 0.0, "{run}");
            assert!(run["peak_rss_bytes"].as_i64().unwrap() > 0, "{run}");
        }
        let run = |op: &str, repeat: u64| {
            runs.iter()
                .find(|r| r["op"] == op && r["repeat"] == repeat)
                .unwrap()
        };
        assert_eq!(run("seek-first", 0)["skipped_records"], 0);
        assert_eq!(run("seek-last", 1)["seq"], records - 1);
        assert_eq!(run("seek-last", 1)["skipped_records"], capacity / 2 - 1);
        assert_eq!(run("replay", 0)["records"], records);
        assert_eq!(run("verify", 1)["problems"], 0);
        assert_eq!(run("writer-open-clean", 0)["scanned_from_seq"], records);
        // Each kill leaves 7 records after the marker; each recovery keeps them and checkpoints.
        let killed = run("writer-open-after-kill", 1);
        assert_eq!(killed["tail_records"], 7);
        assert_eq!(killed["scanned_from_seq"], records + 7);
        assert_eq!(killed["next_seq"], records + 14);
        let unmarked = run("writer-open-without-marker", 1);
        assert_eq!(unmarked["had_checkpoint"], false);
        assert_eq!(unmarked["next_seq"], records + 14);
        // Measuring again works on what the first measurement left; building into it is refused.
        let again = Command::new(env!("CARGO_BIN_EXE_tt-scale"))
            .args(["measure", "--dir"])
            .arg(&dir)
            .args(["--ops", "replay,seek-middle", "--json"])
            .output()
            .unwrap();
        assert!(again.status.success());
        let again: Value = serde_json::from_slice(&again.stdout).unwrap();
        // The kills left 14 records, and the two checkpoint loops appended one in each of their two runs.
        assert_eq!(again["measure"]["runs"][0]["records"], records + 18);
        for args in [
            vec!["build", "--segments", "1"],
            vec!["measure", "--ops", "wat"],
            vec!["measure", "--repeats", "0"],
        ] {
            let refused = Command::new(env!("CARGO_BIN_EXE_tt-scale"))
                .args(&args[..1])
                .arg("--dir")
                .arg(&dir)
                .args(&args[1..])
                .output()
                .unwrap();
            assert!(!refused.status.success(), "{args:?}");
        }
    }
}

fn tt_scale(args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_tt-scale"))
        .args(args)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn scale_measures_both_readers_under_a_low_open_files_limit() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("journal");
    let result = tt_scale(&[
        "run",
        "--dir",
        dir.to_str().unwrap(),
        "--segments",
        "80",
        "--segment-size",
        "65536",
        "--checkpoint-every",
        "0",
        "--readers",
        "safe,mapped",
        "--count",
        "3",
        "--max-open-files",
        "48",
        "--sample-open-files",
    ]);
    let measure = &result["measure"];
    assert_eq!(measure["errors"], 0, "{measure}");
    let runs = measure["runs"].as_array().unwrap();
    for op in tickertape_bench::scale::READER_OPS {
        for reader in ["safe", "mapped"] {
            assert!(
                runs.iter()
                    .any(|r| r["op"] == *op && r["reader"] == reader && r["error"].is_null()),
                "{op} {reader}"
            );
        }
    }
    for run in runs {
        assert_eq!(run["open_files_limit"], 48);
        assert!(run["open_files_most"].as_u64().unwrap() <= 48, "{run}");
    }
}

#[test]
fn scale_archive_then_measure_the_archive() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("journal");
    let to = root.path().join("archive");
    let (dir, to) = (dir.to_str().unwrap(), to.to_str().unwrap());
    tt_scale(&[
        "build",
        "--dir",
        dir,
        "--segments",
        "8",
        "--segment-size",
        "65536",
    ]);
    let archived = tt_scale(&["archive", "--dir", dir, "--to", to]);
    let end = archived["boundary_seq"].as_u64().unwrap();
    let result = tt_scale(&[
        "measure",
        "--dir",
        to,
        "--end",
        &end.to_string(),
        "--ops",
        "seek-first,seek-segment-end,seek-last,replay,verify",
        "--readers",
        "safe,mapped",
    ]);
    let runs = result["measure"]["runs"].as_array().unwrap();
    assert_eq!(result["measure"]["errors"], 0, "{result}");
    for run in runs.iter().filter(|r| r["op"] == "replay") {
        assert_eq!(run["records"], end);
    }
    // Reading on past the archive's last record is an error, reported and not hidden.
    let output = Command::new(env!("CARGO_BIN_EXE_tt-scale"))
        .args(["measure", "--dir", to, "--end", &(end + 1).to_string()])
        .args(["--ops", "replay", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        result["measure"]["runs"][0]["error"].is_string(),
        "{result}"
    );
}

#[test]
fn scale_damage_reports_every_probe_of_a_case() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("damage");
    let result = tt_scale(&[
        "damage",
        "--dir",
        dir.to_str().unwrap(),
        "--segments",
        "8",
        "--segment-size",
        "65536",
        "--payload",
        "256",
        "--cases",
        "intact,record-newest-after-marker",
    ]);
    let cases = result["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 2);
    for case in cases {
        assert_eq!(case["probes"].as_array().unwrap().len(), 16);
        for writer in case["writers"].as_array().unwrap() {
            assert_eq!(writer["gone"], 0, "{writer}");
        }
    }
    let fail = &cases[1]["writers"][1];
    assert!(
        fail["outcome"].as_str().unwrap().starts_with("Corrupt"),
        "{fail}"
    );
}

#[test]
fn scale_catch_up_reads_every_record() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("journal");
    let dir = dir.to_str().unwrap();
    tt_scale(&[
        "build",
        "--dir",
        dir,
        "--segments",
        "8",
        "--segment-size",
        "65536",
    ]);
    for (reader, wait) in [("safe", "spin"), ("mapped", "block")] {
        let result = tt_scale(&[
            "catch-up",
            "--dir",
            dir,
            "--reader",
            reader,
            "--wait",
            wait,
            "--rate",
            "2000",
            "--backlog",
            "300",
            "--warmup",
            "100ms",
            "--baseline",
            "300ms",
            "--live",
            "300ms",
            "--checkpoint-every",
            "100ms",
        ]);
        assert_eq!(result["outcome"], "done", "{result}");
        let caught_up = result["caught_up"]["records"].as_u64().unwrap();
        assert!(caught_up >= 300, "{result}");
        assert_eq!(
            caught_up + result["live"]["records"].as_u64().unwrap(),
            300 + result["records_appended"].as_u64().unwrap(),
            "{result}"
        );
        assert!(result["writer"]["live"]["records"].as_u64().unwrap() > 0);
    }
}
