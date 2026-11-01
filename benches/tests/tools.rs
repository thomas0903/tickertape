//! Functional smoke tests, never recorded benchmark results.
use serde_json::Value;
use std::process::Command;

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
fn latency_wait_modes() {
    for wait in ["default", "block", "sleep"] {
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
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{wait}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["wait"], wait);
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
