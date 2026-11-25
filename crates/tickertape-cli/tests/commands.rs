use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
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
        (None, vec!["tail", "inspect", "verify", "export", "stats"]),
        (
            Some("tail"),
            vec![
                "--from",
                "--from-end",
                "--count",
                "--no-follow",
                "--durable-only",
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
            Some("stats"),
            vec![
                "--from",
                "--to",
                "--durable-only",
                "--json",
                "4096",
                "upper bounds",
                "nearest-rank",
                "busiest_second",
                "decreases",
                "mean_rate_per_s",
                "payload_bytes",
                "no stats",
                "startup scan",
            ],
        ),
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
                "--durable-only",
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

/// `tail --from-end` reads the newest segment only: damage in an older one does not stop it, while damage in the newest
/// is reported. Following from there returns what is appended after it started, with nothing left out.
#[test]
fn tail_from_end_does_not_read_older_segments() {
    let (dir, mut writer) = journal(10);
    corrupt(&dir, 64 + 40);
    command()
        .args(["tail", "--no-follow"])
        .arg(dir.path())
        .assert()
        .code(2);
    assert!(output(&["tail", "--from-end", "--no-follow"], &dir).is_empty());
    let done = AtomicBool::new(false);
    let stdout = std::thread::scope(|scope| {
        scope.spawn(|| {
            for n in 10.. {
                append(&mut writer, n);
                if done.load(Ordering::SeqCst) || n > 10_000 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let output = command()
            .args(["tail", "--from-end", "--count", "3", "--format", "json"])
            .arg(dir.path())
            .timeout(Duration::from_secs(30))
            .output()
            .unwrap();
        done.store(true, Ordering::SeqCst);
        assert!(output.status.success());
        output.stdout
    });
    let records = lines(&stdout);
    let first = records[0]["seq"].as_u64().unwrap() as usize;
    assert!(first >= 10, "{first}");
    assert_eq!(records.len(), 3);
    for (record, n) in records.iter().zip(first..) {
        check_record(record, n);
    }
    // The newest segment is read: damage its first record.
    drop(writer);
    let newest = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.unwrap().file_name().into_string().ok())
        .filter(|name| name.ends_with(".tape"))
        .max()
        .unwrap();
    corrupt_file(&dir.path().join(newest), 64 + 40);
    command()
        .args(["tail", "--from-end", "--no-follow"])
        .arg(dir.path())
        .assert()
        .code(2);
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
    assert_eq!(json["schema_version"], 1);
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
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["segments"], 4);
    assert_eq!(json["records"], 10);
    assert_eq!(json["problems"], serde_json::json!([]));
}

fn corrupt(dir: &TempDir, offset: u64) {
    corrupt_file(&dir.path().join("00000000000000000000.tape"), offset);
}

fn corrupt_file(path: &std::path::Path, offset: u64) {
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
        assert_eq!(json["schema_version"], 1);
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
        vec!["stats"],
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
    // Segments, checkpoint marker, lock file and the notify file (ADR 0017); nothing added by the commands.
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 7);
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
        vec!["stats"],
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
    // No retry: the dropped writer released its lock, even if a child forked by a parallel test (to spawn the CLI)
    // still holds a copy of its descriptor.
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

/// A durable-only `tail` whose position retention removed gets `Error::Discarded` when the checkpoint moves past it
/// (ADR 0034), and `seek` to that position then fails with `SeqUnavailable`. `tail` continues at the oldest record
/// still in the journal and names the skipped records on standard error; standard output keeps its format.
#[test]
fn durable_only_tail_continues_after_retention_removed_its_position() {
    use std::io::BufRead;
    use tickertape::retention::{self, Action, Policy};

    let signal = |child: &std::process::Child, signal: &str| {
        let status = ProcessCommand::new("kill")
            .arg(signal)
            .arg(child.id().to_string())
            .status()
            .unwrap();
        assert!(status.success());
    };
    for format in ["text", "json"] {
        let (dir, mut writer) = journal(1);
        writer.checkpoint().unwrap();
        let mut child = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("tickertape"))
            .args([
                "tail",
                "--durable-only",
                "--count",
                "2",
                "--timeout-secs",
                "10",
            ])
            .args(["--format", format])
            .arg(dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut first = String::new();
        stdout.read_line(&mut first).unwrap();
        // Hold tail still while the checkpoint moves past its position and retention removes that segment, so that
        // it reads neither the records it is waiting for nor the marker before they are gone.
        signal(&child, "-STOP");
        for n in 1..12 {
            append(&mut writer, n);
        }
        writer.checkpoint().unwrap();
        let plan = retention::plan(dir.path(), &Policy::default().max_segments(2)).unwrap();
        retention::apply(dir.path(), &plan, &Action::Delete).unwrap();
        let oldest = tickertape::inspect::list_segments(dir.path()).unwrap()[0].base_seq;
        assert!(oldest > 1, "{oldest}");
        signal(&child, "-CONT");
        let mut rest = String::new();
        stdout.read_to_string(&mut rest).unwrap();
        let result = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(result.status.success(), "{format}: {result:?}");
        let seqs: Vec<u64> = match format {
            "text" => [first.as_str(), rest.as_str()]
                .concat()
                .lines()
                .map(|line| {
                    line.strip_prefix("seq=")
                        .and_then(|l| l.split(' ').next())
                        .unwrap()
                        .parse()
                        .unwrap()
                })
                .collect(),
            _ => {
                let records = lines([first, rest].concat().as_bytes());
                check_record(&records[0], 0);
                check_record(&records[1], oldest as usize);
                records.iter().map(|r| r["seq"].as_u64().unwrap()).collect()
            }
        };
        assert_eq!(seqs, [0, oldest], "{format}");
        assert!(
            stderr.contains(&format!(
                "warning: records 1 to {} were removed before tail read them; continuing at sequence {oldest}",
                oldest - 1
            )),
            "{format}: {stderr}"
        );
    }
}

/// As above, with exactly one record removed before `tail` read it: the warning names that record alone.
#[test]
fn durable_only_tail_names_a_single_removed_record() {
    use std::io::BufRead;
    use tickertape::retention::{self, Action, Policy};

    let (dir, mut writer) = journal(2); // three records fill a segment: 0..3, 3..6, ...
    writer.checkpoint().unwrap();
    let mut child = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("tickertape"))
        .args([
            "tail",
            "--durable-only",
            "--count",
            "3",
            "--timeout-secs",
            "10",
        ])
        .arg(dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut read = String::new();
    for _ in 0..2 {
        stdout.read_line(&mut read).unwrap();
    }
    // Hold tail at record 2 while the checkpoint moves past it and retention removes segment 0, which ends with it.
    let signal = |signal: &str| {
        let status = ProcessCommand::new("kill")
            .arg(signal)
            .arg(child.id().to_string())
            .status()
            .unwrap();
        assert!(status.success());
    };
    signal("-STOP");
    for n in 2..7 {
        append(&mut writer, n);
    }
    writer.checkpoint().unwrap();
    let plan = retention::plan(dir.path(), &Policy::default().max_segments(2)).unwrap();
    retention::apply(dir.path(), &plan, &Action::Delete).unwrap();
    assert_eq!(
        tickertape::inspect::list_segments(dir.path()).unwrap()[0].base_seq,
        3
    );
    signal("-CONT");
    stdout.read_to_string(&mut read).unwrap();
    let result = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "{result:?}");
    let seqs: Vec<&str> = read
        .lines()
        .map(|line| {
            line.strip_prefix("seq=")
                .and_then(|l| l.split(' ').next())
                .unwrap()
        })
        .collect();
    assert_eq!(seqs, ["0", "1", "3"]);
    assert!(
        stderr.contains(
            "warning: record 2 was removed before tail read it; continuing at sequence 3"
        ),
        "{stderr}"
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

#[test]
fn durable_only_tail_and_jsonl_export() {
    let (dir, mut writer) = journal(4);
    let commands = [
        vec!["tail", "--durable-only", "--no-follow", "--format", "json"],
        vec!["export", "--durable-only", "--format", "jsonl"],
    ];
    for args in &commands {
        assert!(output(args, &dir).is_empty());
    }
    writer.checkpoint().unwrap();
    append(&mut writer, 4);
    for args in &commands {
        let records = lines(&output(args, &dir));
        assert_eq!(records.len(), 4);
        for (seq, record) in records.iter().enumerate() {
            check_record(record, seq);
        }
    }
    assert_eq!(
        lines(&output(
            &[
                "tail",
                "--durable-only",
                "--format",
                "json",
                "--timeout-secs",
                "0.01"
            ],
            &dir
        ))
        .len(),
        4
    );
    assert!(
        output(
            &[
                "export",
                "--durable-only",
                "--format",
                "jsonl",
                "--from",
                "4"
            ],
            &dir
        )
        .is_empty()
    );
    let range = lines(&output(
        &[
            "export",
            "--durable-only",
            "--format",
            "jsonl",
            "--from",
            "2",
            "--to",
            "10",
        ],
        &dir,
    ));
    assert_eq!(range.len(), 2);
    check_record(&range[0], 2);
    check_record(&range[1], 3);
    writer.checkpoint().unwrap();
    for args in &commands {
        assert_eq!(lines(&output(args, &dir)).len(), 5);
    }
}

#[test]
fn tail_decodes_market_demo_records() {
    use tickertape_cli::market_demo::{
        Action, Book, BookSide, Trade, TradeSide, encode_book, encode_trade,
    };
    let dir = TempDir::new().unwrap();
    let mut writer = Writer::open(dir.path(), WriterConfig::default().segment_size(65536)).unwrap();
    let trade = encode_trade(Trade {
        instrument_id: 7,
        side: TradeSide::Buy,
        price: 12_350_000_000,
        qty: 200_000_000,
        trade_id: 42,
    });
    let book = encode_book(Book {
        instrument_id: 3,
        side: BookSide::Ask,
        action: Action::Delete,
        price: 10_000_000_000,
        qty: 0,
    });
    for (schema_id, payload) in [(1, &trade[..]), (2, &book[..]), (9, &b"opaque"[..])] {
        writer
            .append(&NewRecord {
                schema_id,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload,
            })
            .unwrap();
    }

    let text = output(&["tail", "--no-follow", "--decode", "market-demo"], &dir);
    let lines: Vec<&str> = std::str::from_utf8(&text).unwrap().lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(
        lines[0].ends_with(" decoded=trade instrument=7 side=buy price=123.5 qty=2 trade_id=42"),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].ends_with(" decoded=book instrument=3 side=ask price=100 qty=0 action=delete"),
        "{}",
        lines[1]
    );
    assert!(lines[2].ends_with(" decoded=-"), "{}", lines[2]);

    let json = output(
        &[
            "tail",
            "--no-follow",
            "--format",
            "json",
            "--decode",
            "market-demo",
        ],
        &dir,
    );
    let values: Vec<Value> = std::str::from_utf8(&json)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(values[0]["decoded"]["kind"], "trade");
    assert_eq!(values[0]["decoded"]["price"], 123.5);
    assert_eq!(values[1]["decoded"]["action"], "delete");
    assert!(values[2]["decoded"].is_null());
    // Without --decode, the JSON schema is unchanged (no "decoded" key).
    let plain = output(&["tail", "--no-follow", "--format", "json"], &dir);
    assert!(!std::str::from_utf8(&plain).unwrap().contains("decoded"));
}
