use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use tickertape::{NewRecord, Writer, WriterConfig, inspect};

fn command() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("tickertape"))
}

fn journal() -> (tempfile::TempDir, Writer) {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), WriterConfig::default().segment_size(65536)).unwrap();
    for _ in 0..5 {
        writer
            .append(&NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload: &[7; 40000],
            })
            .unwrap();
    }
    writer.checkpoint().unwrap();
    (dir, writer)
}

#[test]
fn help_and_usage_errors() {
    command()
        .args(["archive", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("--dry-run")
                .and(predicate::str::contains("--keep-segments"))
                .and(predicate::str::contains("--older-than"))
                .and(predicate::str::contains("--max-bytes"))
                .and(predicate::str::contains("already_removed_segments")),
        );
    for args in [
        vec!["archive", "unused", "--delete"],
        vec!["archive", "unused", "--keep-segments", "1"],
        vec![
            "archive",
            "unused",
            "--keep-segments",
            "1",
            "--delete",
            "--to",
            "unused",
        ],
        vec!["archive", "unused", "--delete", "--older-than", "30"],
        vec!["archive", "unused", "--delete", "--older-than", "1w"],
        vec![
            "archive",
            "unused",
            "--delete",
            "--max-bytes",
            "18446744073709551615T",
        ],
        vec!["archive", "unused", "--delete", "--max-bytes", "1KB"],
        vec!["archive", "unused", "--delete", "--keep-segments", "-1"],
    ] {
        command().args(args).assert().code(1);
    }
}

#[test]
fn dry_run_json_delete_and_nothing_to_do() {
    let (dir, _writer) = journal();
    let before = fs::read_dir(dir.path()).unwrap().count();
    let output = command()
        .arg("archive")
        .arg(dir.path())
        .args([
            "--delete",
            "--keep-segments",
            "2",
            "--max-bytes",
            "128K",
            "--dry-run",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["report"], Value::Null);
    assert_eq!(json["segments"].as_array().unwrap().len(), 3);
    assert_eq!(json["segments"][0]["file"], "00000000000000000000.tape");
    assert_eq!(
        json["segments"][0]["reasons"],
        serde_json::json!(["max_segments", "max_bytes"])
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), before);
    let output = command()
        .arg("archive")
        .arg(dir.path())
        .args(["--delete", "--keep-segments", "2", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["report"]["removed_segments"], 3);
    assert_eq!(json["report"]["removed_bytes"], 3 * 65536);
    assert_eq!(json["report"]["already_removed_segments"], 0);
    assert_eq!(inspect::list_segments(dir.path()).unwrap().len(), 2);
    command()
        .arg("archive")
        .arg(dir.path())
        .args(["--delete", "--keep-segments", "2"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Removed 0 segments"));
}

#[test]
fn move_with_age_and_conflict() {
    let (dir, _writer) = journal();
    let dest = tempfile::tempdir().unwrap();
    let first = "00000000000000000000.tape";
    fs::write(dest.path().join(first), b"untouched").unwrap();
    command()
        .arg("archive")
        .arg(dir.path())
        .arg("--to")
        .arg(dest.path())
        .args(["--older-than", "0s"])
        .assert()
        .code(1);
    assert_eq!(inspect::list_segments(dir.path()).unwrap().len(), 5);
    assert_eq!(fs::read(dest.path().join(first)).unwrap(), b"untouched");
    fs::remove_file(dest.path().join(first)).unwrap();
    command()
        .arg("archive")
        .arg(dir.path())
        .arg("--to")
        .arg(dest.path())
        .args(["--older-than", "0s", "--json"])
        .assert()
        .success();
    assert_eq!(inspect::list_segments(dir.path()).unwrap().len(), 1);
    assert_eq!(inspect::list_segments(dest.path()).unwrap().len(), 4);
}

#[test]
fn corrupt_candidate_exit_two_missing_journal_exit_one() {
    let (dir, _writer) = journal();
    let path = dir.path().join("00000000000000000000.tape");
    let mut data = fs::read(&path).unwrap();
    data[110] ^= 1;
    fs::write(path, data).unwrap();
    command()
        .arg("archive")
        .arg(dir.path())
        .args(["--delete", "--keep-segments", "0"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty());
    command()
        .arg("archive")
        .arg(dir.path().join("missing"))
        .args(["--delete", "--keep-segments", "0"])
        .assert()
        .code(1);
}
