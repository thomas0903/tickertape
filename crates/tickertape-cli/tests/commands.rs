use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::Duration;

use assert_cmd::Command;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;
use tickertape::{NewRecord, Writer, WriterConfig};

fn command() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("tickertape"))
}

fn journal(count: usize) -> (TempDir, Writer) {
    let dir = TempDir::new().unwrap();
    let mut writer = Writer::open(dir.path(), WriterConfig::default().segment_size(65536)).unwrap();
    for n in 0..count {
        append(&mut writer, n);
    }
    (dir, writer)
}

fn payload(n: usize) -> Vec<u8> {
    vec![n as u8; 20_000 + n % 3]
}

fn append(writer: &mut Writer, n: usize) {
    writer
        .append(&NewRecord {
            schema_id: n as u32 + 1,
            venue_ts_ns: n as u64 + 10,
            local_ts_ns: n as u64 + 20,
            payload: &payload(n),
        })
        .unwrap();
}

fn output(args: &[&str], dir: &TempDir) -> Vec<u8> {
    command()
        .args(args)
        .arg(dir.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone()
}

fn lines(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8(bytes.to_vec())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn help_contracts_and_usage_errors() {
    for (cmd, options) in [
        (None, vec!["tail", "inspect", "verify", "export"]),
        (
            Some("tail"),
            vec![
                "--from",
                "--from-end",
                "--count",
                "--no-follow",
                "--format",
                "--timeout-secs",
                "payload_b64",
            ],
        ),
        (
            Some("inspect"),
            vec!["--json", "checkpoint", "used_percent"],
        ),
        (Some("verify"), vec!["--json", "problems", "first_seq"]),
        (
            Some("export"),
            vec![
                "--format",
                "--output",
                "--from",
                "--to",
                "payload_b64",
                "padding",
                "parquet",
                "--decode",
                "market-demo",
                "message tickertape_record",
                "required int64 seq (INTEGER(64,false));",
                "required int32 schema_id (INTEGER(32,false));",
                "required int64 venue_ts_ns (INTEGER(64,false));",
                "required int64 local_ts_ns (INTEGER(64,false));",
                "required binary payload;",
                "optional binary kind (UTF8);",
                "optional int64 trade_id (INTEGER(64,false));",
                "100,000",
            ],
        ),
    ] {
        let mut cli = command();
        if let Some(cmd) = cmd {
            cli.arg(cmd);
        }
        let assertion = cli.arg("--help").assert().success();
        let help = String::from_utf8_lossy(&assertion.get_output().stdout);
        for option in options {
            assert!(help.contains(option), "{cmd:?}: {option}: {help}");
        }
        assert!(help.contains("Exit codes:"));
        assert!(help.contains("0"));
        assert!(help.contains("1"));
        assert!(help.contains("2"));
    }
    command()
        .arg("--unknown")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("error:"));
    command().assert().code(1);
    command()
        .args(["tail", ".", "--from", "0", "--from-end"])
        .assert()
        .code(1);
    command()
        .args(["tail", ".", "--timeout-secs", "NaN"])
        .assert()
        .code(1);
    command()
        .args(["tail", ".", "--timeout-secs", "1e300"])
        .assert()
        .code(1);
    command()
        .args(["export", ".", "--format", "parquet"])
        .assert()
        .code(1);
    command()
        .args([
            "export", ".", "--format", "jsonl", "--from", "2", "--to", "1",
        ])
        .assert()
        .code(1);
}

#[test]
fn tail_ranges_formats_and_idle_exit() {
    let (dir, _writer) = journal(10);
    let all = output(&["tail", "--no-follow"], &dir);
    let text = String::from_utf8(all).unwrap();
    assert_eq!(text.lines().count(), 10);
    assert!(text.contains("seq=9 schema=10 venue_ts=19 local_ts=29 len=20000 payload="));
    assert!(text.contains('…'));
    assert_eq!(
        String::from_utf8(output(&["tail", "--count", "3"], &dir))
            .unwrap()
            .lines()
            .count(),
        3
    );
    let from = output(
        &["tail", "--from", "8", "--no-follow", "--format", "json"],
        &dir,
    );
    let records = lines(&from);
    assert_eq!(records.len(), 2);
    check_record(&records[0], 8);
    check_record(&records[1], 9);
    assert!(output(&["tail", "--count", "0"], &dir).is_empty());
    assert!(output(&["tail", "--from-end", "--no-follow"], &dir).is_empty());
    command()
        .args(["tail", "--from-end", "--timeout-secs", "0.01"])
        .arg(dir.path())
        .timeout(Duration::from_secs(5))
        .assert()
        .success()
        .stdout("");
    assert!(output(&["tail", "--from", "100", "--no-follow"], &dir).is_empty());
}

fn check_record(record: &Value, n: usize) {
    assert_eq!(record["seq"], n as u64);
    assert_eq!(record["schema_id"], n as u32 + 1);
    assert_eq!(record["venue_ts_ns"], n as u64 + 10);
    assert_eq!(record["local_ts_ns"], n as u64 + 20);
    assert_eq!(
        STANDARD
            .decode(record["payload_b64"].as_str().unwrap())
            .unwrap(),
        payload(n)
    );
    assert_eq!(record.as_object().unwrap().len(), 5);
}

#[test]
fn tail_follows_live_writer() {
    let (dir, mut writer) = journal(0);
    let mut child = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("tickertape"))
        .args(["tail", "--count", "10", "--timeout-secs", "5"])
        .arg(dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Wait for the child's first flushed line so subsequent appends necessarily exercise live following.
    append(&mut writer, 0);
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    use std::io::BufRead;
    stdout.read_line(&mut first).unwrap();
    assert!(first.starts_with("seq=0 "));
    for n in 1..10 {
        append(&mut writer, n);
    }
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(result.status.success(), "{:?}", result);
    assert_eq!(rest.lines().count(), 9);
}

#[test]
fn inspect_and_verify_contracts() {
    let (dir, mut writer) = journal(10);
    let json: Value = serde_json::from_slice(&output(&["inspect", "--json"], &dir)).unwrap();
    assert_eq!(json["totals"]["segments"], 4);
    assert_eq!(json["totals"]["records"], 10);
    assert_eq!(json["totals"]["first_seq"], 0);
    assert_eq!(json["totals"]["last_seq"], 9);
    assert!(json["checkpoint"].is_null());
    let segment = &json["segments"][0];
    assert_eq!(segment["size_bytes"], 65536);
    assert_eq!(segment["records"], 3);
    assert_eq!(segment["end_seq"], 3);
    assert_eq!(segment["end_state"], "sealed");
    assert!(segment["created_utc"].as_str().unwrap().ends_with('Z'));
    assert!(segment["used_percent"].as_f64().unwrap() > 90.0);
    let text = String::from_utf8(output(&["inspect"], &dir)).unwrap();
    assert!(text.contains("checkpoint: none"));
    assert!(text.contains("segments=4 records=10 first_seq=0 last_seq=9"));
    writer.checkpoint().unwrap();
    let json: Value = serde_json::from_slice(&output(&["inspect", "--json"], &dir)).unwrap();
    assert_eq!(json["checkpoint"]["generation"], 1);
    assert_eq!(json["checkpoint"]["next_seq"], 10);
    assert!(
        String::from_utf8(output(&["inspect"], &dir))
            .unwrap()
            .contains("checkpoint: generation=1 next_seq=10")
    );
    command()
        .arg("verify")
        .arg(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "segments=4 records=10 first_seq=0 last_seq=9",
        ));
    let json: Value = serde_json::from_slice(&output(&["verify", "--json"], &dir)).unwrap();
    assert_eq!(json["segments"], 4);
    assert_eq!(json["records"], 10);
    assert_eq!(json["problems"], serde_json::json!([]));
}

fn corrupt(dir: &TempDir, offset: u64) {
    let path = dir.path().join("00000000000000000000.tape");
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut byte = [0];
    file.read_exact(&mut byte).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(&[byte[0] ^ 1]).unwrap();
}

#[test]
fn corruption_is_reported_and_export_preserves_prefix() {
    let (dir, writer) = journal(10);
    drop(writer);
    // Second record starts after the aligned 20,040-byte first frame.
    corrupt(&dir, 64 + 20_040 + 40);
    for cmd in ["inspect", "verify"] {
        command()
            .arg(cmd)
            .arg(dir.path())
            .assert()
            .code(2)
            .stdout(predicate::str::contains("checksum mismatch"));
        let assertion = command()
            .args([cmd, "--json"])
            .arg(dir.path())
            .assert()
            .code(2);
        let json: Value = serde_json::from_slice(&assertion.get_output().stdout).unwrap();
        assert_eq!(json["problems"][0]["offset"], 20104);
        assert_eq!(json["problems"][0]["file"], "00000000000000000000.tape");
    }
    for args in [
        vec!["tail", "--no-follow", "--format", "json"],
        vec!["export", "--format", "jsonl"],
    ] {
        let assertion = command()
            .args(args)
            .arg(dir.path())
            .assert()
            .code(2)
            .stderr(predicate::str::contains("checksum mismatch"));
        let records = lines(&assertion.get_output().stdout);
        assert_eq!(records.len(), 1);
        check_record(&records[0], 0);
    }
}

#[test]
fn export_round_trip_range_and_file() {
    let (dir, _writer) = journal(10);
    let bytes = output(&["export", "--format", "jsonl"], &dir);
    let records = lines(&bytes);
    assert_eq!(records.len(), 10);
    for (n, record) in records.iter().enumerate() {
        check_record(record, n);
    }
    assert!(String::from_utf8_lossy(&bytes).starts_with(
        "{\"seq\":0,\"schema_id\":1,\"venue_ts_ns\":10,\"local_ts_ns\":20,\"payload_b64\":"
    ));
    let range = lines(&output(
        &["export", "--format", "jsonl", "--from", "2", "--to", "4"],
        &dir,
    ));
    assert_eq!(range.len(), 3);
    for (n, record) in range.iter().enumerate() {
        check_record(record, n + 2);
    }
    let dest = TempDir::new().unwrap();
    let file = dest.path().join("export.jsonl");
    command()
        .args(["export", "--format", "jsonl", "--output"])
        .arg(&file)
        .arg(dir.path())
        .assert()
        .success()
        .stdout("");
    assert_eq!(fs::read(file).unwrap(), bytes);
    assert_eq!(
        output(&["export", "--format", "jsonl", "--output", "-"], &dir),
        bytes
    );
}

#[test]
fn commands_do_not_modify_journal_and_reject_export_into_it() {
    let (dir, _writer) = journal(10);
    let before: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| {
            let path = e.unwrap().path();
            (path.clone(), fs::read(path).unwrap())
        })
        .collect();
    for args in [
        vec!["tail", "--no-follow"],
        vec!["inspect"],
        vec!["verify"],
        vec!["export", "--format", "jsonl"],
    ] {
        output(&args, &dir);
    }
    command()
        .args(["export", "--format", "jsonl", "--output"])
        .arg(dir.path().join("00000000000000000000.tape"))
        .arg(dir.path())
        .assert()
        .code(1);
    let dest = TempDir::new().unwrap();
    let alias = dest.path().join("alias");
    fs::hard_link(dir.path().join("checkpoint"), &alias).unwrap();
    command()
        .args(["export", "--format", "jsonl", "--output"])
        .arg(alias)
        .arg(dir.path())
        .assert()
        .code(1);
    for (path, bytes) in before {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 6);
}

#[test]
fn empty_and_missing_journals() {
    let (dir, _writer) = journal(0);
    for args in [
        vec!["tail", "--no-follow"],
        vec!["export", "--format", "jsonl"],
    ] {
        assert!(output(&args, &dir).is_empty());
    }
    let json: Value = serde_json::from_slice(&output(&["inspect", "--json"], &dir)).unwrap();
    assert_eq!(json["totals"]["records"], 0);
    assert!(json["totals"]["first_seq"].is_null());
    let absent = dir.path().join("missing");
    for args in [
        vec!["tail", "--no-follow"],
        vec!["inspect"],
        vec!["verify"],
        vec!["export", "--format", "jsonl"],
    ] {
        command()
            .args(args)
            .arg(&absent)
            .assert()
            .code(1)
            .stderr(predicate::str::contains("failed"));
    }
}

#[test]
fn tail_resumes_after_discarded_history() {
    use std::io::BufRead;

    let (dir, writer) = journal(1);
    let mut child = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("tickertape"))
        .args(["tail", "--count", "2", "--timeout-secs", "5"])
        .arg(dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    stdout.read_line(&mut first).unwrap();
    assert!(first.starts_with("seq=0 "));
    drop(writer);
    corrupt(&dir, 64 + 40);
    let mut writer = Writer::open(dir.path(), WriterConfig::default().segment_size(65536)).unwrap();
    append(&mut writer, 0);
    append(&mut writer, 1);
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(rest.lines().count(), 1);
    assert!(rest.starts_with("seq=1 "));
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .contains("warning: history discarded at sequence 1")
    );
}

#[test]
fn export_rejects_dangling_symlink_into_journal() {
    let (dir, _writer) = journal(1);
    let dest = TempDir::new().unwrap();
    let link = dest.path().join("output");
    let target = dir.path().join("new-entry");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    command()
        .args(["export", "--format", "jsonl", "--output"])
        .arg(link)
        .arg(dir.path())
        .assert()
        .code(1);
    assert!(!target.exists());
}
