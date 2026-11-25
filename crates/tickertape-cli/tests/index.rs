//! `tickertape index`, and `--seek-index` of the commands that take `--from` (ADR 0045).

use std::fs;
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{Value, json};
use tickertape::seek_index::{self, codec};
use tickertape::{NewRecord, Writer, WriterConfig, format};

fn command() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("tickertape"))
}

/// Three sealed segments of 128 KiB and an active one.
fn journal() -> (tempfile::TempDir, Writer) {
    let dir = tempfile::tempdir().unwrap();
    let config = WriterConfig::default().segment_size(131_072);
    let mut writer = Writer::open(dir.path(), config).unwrap();
    while tickertape::inspect::list_segments(dir.path())
        .unwrap()
        .len()
        < 4
    {
        writer
            .append(&NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: 1,
                payload: &[7; 64],
            })
            .unwrap();
    }
    (dir, writer)
}

fn run(dir: &Path, args: &[&str], code: i32) -> Value {
    let output = command()
        .arg("index")
        .arg(dir)
        .args(args)
        .arg("--json")
        .assert()
        .code(code)
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn states(document: &Value) -> Vec<&str> {
    document["segments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["state"].as_str().unwrap())
        .collect()
}

#[test]
fn help_and_usage_errors() {
    command()
        .args(["index", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("--rebuild")
                .and(predicate::str::contains("--check"))
                .and(predicate::str::contains("--remove"))
                .and(predicate::str::contains("\"not_covered\""))
                .and(predicate::str::contains("Exit codes: 0")),
        );
    for args in [
        vec!["index"],
        vec!["index", "unused", "--check", "--rebuild"],
        vec!["index", "unused", "--check", "--remove"],
        vec!["index", "/nonexistent/journal"],
        vec!["index", "/nonexistent/journal", "--check"],
        vec!["index", "/nonexistent/journal", "--remove"],
    ] {
        command().args(&args).assert().code(1);
    }
    let empty = tempfile::tempdir().unwrap();
    command().arg("index").arg(empty.path()).assert().code(1);
}

#[test]
fn build_check_and_remove() {
    let (dir, mut writer) = journal();
    // No marker, and the writer is open.
    command()
        .arg("index")
        .arg(dir.path())
        .assert()
        .code(1)
        .stderr(predicate::str::contains("locked").or(predicate::str::contains("writer")));
    writer.checkpoint().unwrap();
    let bases: Vec<u64> = tickertape::inspect::list_segments(dir.path())
        .unwrap()
        .iter()
        .map(|s| s.base_seq)
        .collect();
    let name = |i: usize| format::segment_file_name(bases[i]);

    let built = run(dir.path(), &[], 0);
    assert_eq!(built["schema_version"], 1);
    assert_eq!(built["action"], "build");
    assert_eq!(built["removed"], Value::Null);
    assert_eq!(states(&built), ["built", "built", "built", "not_covered"]);
    assert_eq!(
        built["segments"][1],
        json!({
            "file": name(1),
            "base_seq": bases[1],
            "state": "built",
            "ignored_by_readers": false,
            "reason": null
        })
    );
    assert_eq!(states(&run(dir.path(), &[], 0))[..3], ["valid"; 3]);
    assert_eq!(
        states(&run(dir.path(), &["--rebuild"], 0))[..3],
        ["built"; 3]
    );
    let checked = run(dir.path(), &["--check"], 0);
    assert_eq!(checked["action"], "check");
    assert_eq!(states(&checked), ["valid", "valid", "valid", "not_covered"]);

    // One that is missing, one that is ignored for its mode, one that is damaged: not failures.
    let index = |i: usize| {
        dir.path()
            .join(seek_index::DIR_NAME)
            .join(std::format!("{:020}.idx", bases[i]))
    };
    fs::remove_file(index(0)).unwrap();
    fs::set_permissions(index(1), fs::Permissions::from_mode(0o664)).unwrap();
    fs::write(index(2), b"junk").unwrap();
    let checked = run(dir.path(), &["--check"], 0);
    assert_eq!(states(&checked)[..3], ["missing", "valid", "ignored"]);
    assert_eq!(checked["segments"][1]["ignored_by_readers"], true);
    assert!(checked["segments"][1]["reason"].is_string());
    command()
        .args(["index", "--check"])
        .arg(dir.path())
        .assert()
        .success()
        .stdout(
            predicate::str::contains(std::format!("{} missing\n", name(0)))
                .and(predicate::str::contains(std::format!(
                    "{} valid (ignored by readers): ",
                    name(1)
                )))
                .and(predicate::str::contains(std::format!(
                    "{} ignored: ",
                    name(2)
                ))),
        );
    assert_eq!(states(&run(dir.path(), &[], 0))[..3], ["built"; 3]);

    // A wrong one, with a checksum that fits: exit 2, and the report is complete.
    let path = dir.path().join(name(0));
    let header: [u8; 64] = fs::read(&path).unwrap()[..64].try_into().unwrap();
    let good = fs::read(index(0)).unwrap();
    let decoded = codec::decode(&good, &header).unwrap();
    let entry = decoded.entries().next().unwrap();
    // A frame planted in the payload of the record before the indexed one, for that record's number.
    let outer = entry.offset - 104;
    let mut frame = vec![0; 64];
    let body = format::RecordHeader {
        seq: entry.seq - 1,
        schema_id: 9,
        venue_ts_ns: 0,
        local_ts_ns: 0,
    };
    format::encode_record_body(&body, &[], &mut frame[4..40]);
    frame[..4].copy_from_slice(&40_u32.to_le_bytes());
    let raw = fs::OpenOptions::new().write(true).open(&path).unwrap();
    raw.write_all_at(&frame, outer + 40).unwrap();
    let mut encoder = codec::Encoder::with_spacing(4_096);
    encoder.push(codec::Entry {
        seq: entry.seq - 1,
        offset: outer + 40,
        len: 40,
        crc: u32::from_le_bytes(frame[4..8].try_into().unwrap()),
    });
    // The record around it must be right too: its checksum.
    let mut image = fs::read(&path).unwrap();
    let body = &mut image[outer as usize + 4..outer as usize + 104];
    format::finish_record_body(104, body);
    raw.write_all_at(&image[outer as usize..outer as usize + 104], outer)
        .unwrap();
    let wrong = encoder
        .finish(&header, decoded.seal_offset, decoded.next_seq)
        .unwrap();
    fs::write(index(0), wrong).unwrap();
    let checked = run(dir.path(), &["--check"], 2);
    assert_eq!(states(&checked)[..3], ["wrong", "valid", "valid"]);
    command()
        .args(["index", "--check"])
        .arg(dir.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains("1 seek index(es) are wrong"));
    assert_eq!(states(&run(dir.path(), &["--rebuild"], 0))[0], "built");
    command().arg("verify").arg(dir.path()).assert().success();

    let removed = run(dir.path(), &["--remove"], 0);
    assert_eq!(removed["action"], "remove");
    assert_eq!(removed["removed"], 3);
    assert_eq!(removed["segments"], json!([]));
    assert_eq!(run(dir.path(), &["--remove"], 0)["removed"], 0);
    assert!(dir.path().join(seek_index::DIR_NAME).is_dir());

    // A marker that names a missing segment is corruption.
    drop(writer);
    fs::remove_file(dir.path().join(name(3))).unwrap();
    for action in ["--rebuild", "--check"] {
        command()
            .arg("index")
            .arg(dir.path())
            .arg(action)
            .assert()
            .code(2);
    }
}

/// `--seek-index` changes where a seek starts, not what is printed; with damage before the landing record it shows.
#[test]
fn seek_index_of_tail_export_and_stats() {
    let (dir, mut writer) = journal();
    writer.checkpoint().unwrap();
    command().arg("index").arg(dir.path()).assert().success();
    let from = "1200";
    let commands: [&[&str]; 4] = [
        &["tail", "--no-follow", "--format", "json", "--count", "3"],
        &["tail", "--no-follow", "--durable-only"],
        &["export", "--format", "jsonl"],
        &["stats", "--json"],
    ];
    let output = |args: &[&str], index: bool| {
        let mut command = command();
        command
            .arg(args[0])
            .arg(dir.path())
            .args(&args[1..])
            .args(["--from", from]);
        if index {
            command.arg("--seek-index");
        }
        command.output().unwrap()
    };
    for args in commands {
        let (plain, index) = (output(args, false), output(args, true));
        assert!(plain.status.success(), "{args:?}");
        assert!(!plain.stdout.is_empty());
        assert_eq!(plain, index, "{args:?}");
    }
    let file = fs::OpenOptions::new()
        .write(true)
        .open(dir.path().join(format::segment_file_name(0)))
        .unwrap();
    file.write_all_at(&[0xEE], 64 + 50).unwrap();
    for args in commands {
        assert_eq!(output(args, false).status.code(), Some(2), "{args:?}");
        assert!(output(args, true).status.success(), "{args:?}");
    }
}

/// An unreadable `seek-index/` fails no export (it fails those of 1.0.0), and its files are no journal files.
#[test]
fn export_does_not_descend_into_the_index_directory() {
    let (dir, mut writer) = journal();
    writer.checkpoint().unwrap();
    command().arg("index").arg(dir.path()).assert().success();
    let indexes = dir.path().join(seek_index::DIR_NAME);
    let out = tempfile::tempdir().unwrap();
    fs::set_permissions(&indexes, fs::Permissions::from_mode(0o000)).unwrap();
    let export = |file: &str| {
        command()
            .arg("export")
            .arg(dir.path())
            .args(["--format", "jsonl", "--output"])
            .arg(out.path().join(file))
            .assert()
    };
    export("a.jsonl").success();
    fs::set_permissions(&indexes, fs::Permissions::from_mode(0o755)).unwrap();
    export("b.jsonl").success();
    // Another subdirectory that cannot be read still does.
    let other = dir.path().join("quarantine");
    fs::create_dir(&other).unwrap();
    fs::set_permissions(&other, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&other).is_err() {
        export("c.jsonl").code(1);
    }
    fs::set_permissions(&other, fs::Permissions::from_mode(0o755)).unwrap();
}
