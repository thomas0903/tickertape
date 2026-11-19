use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::FileExt;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use tickertape::status::ReaderBeacon;
use tickertape::{NewRecord, Reader, ReaderConfig, Writer, WriterConfig};

fn command() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("tickertape"))
}

fn journal(records: u64, checkpoint: bool) -> (tempfile::TempDir, Writer) {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), WriterConfig::default().segment_size(65536)).unwrap();
    for _ in 0..records {
        writer
            .append(&NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: tickertape::clock::realtime_ns(),
                payload: &[7; 1000],
            })
            .unwrap();
    }
    if checkpoint {
        writer.checkpoint().unwrap();
    }
    (dir, writer)
}

fn status_json(args: &[&str]) -> (Value, i32) {
    let out = command()
        .arg("status")
        .args(args)
        .arg("--json")
        .output()
        .unwrap();
    let code = out.status.code().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.ends_with('\n') && text.lines().count() == 1, "{text}");
    (serde_json::from_str(&text).unwrap(), code)
}

fn keys(v: &Value) -> BTreeSet<&str> {
    v.as_object().unwrap().keys().map(String::as_str).collect()
}

fn set<'a>(names: &[&'a str]) -> BTreeSet<&'a str> {
    names.iter().copied().collect()
}

#[test]
fn help_and_usage_errors() {
    command()
        .args(["status", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("--watch")
                .and(predicate::str::contains("--format"))
                .and(predicate::str::contains("prometheus"))
                .and(predicate::str::contains("--max-checkpoint-age"))
                .and(predicate::str::contains("--min-headroom"))
                .and(predicate::str::contains("--min-headroom-segments"))
                .and(predicate::str::contains("--max-reader-lag"))
                .and(predicate::str::contains("--readers-dir"))
                .and(predicate::str::contains("schema_version"))
                .and(predicate::str::contains("3 a health threshold")),
        );
    for args in [
        vec!["status"],
        vec!["status", "x", "--watch", "0s"],
        vec!["status", "x", "--watch", "1d"],
        vec!["status", "x", "--watch", "5"],
        vec!["status", "x", "--samples", "2"],
        vec!["status", "x", "--watch", "1s", "--samples", "0"],
        vec!["status", "x", "--json", "--format", "text"],
        vec!["status", "x", "--format", "xml"],
        vec!["status", "x", "--max-checkpoint-age", "5"],
        vec!["status", "x", "--min-headroom", "1Q"],
    ] {
        command().args(&args).assert().code(1).stdout("");
    }
    let dir = tempfile::tempdir().unwrap();
    command()
        .args(["status", dir.path().to_str().unwrap()])
        .assert()
        .code(1)
        .stdout("")
        .stderr(predicate::str::contains("no journal segments"));
}

#[test]
fn json_schema_is_stable() {
    let (dir, mut writer) = journal(3, true);
    writer
        .append(&NewRecord {
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns: 123,
            payload: b"x",
        })
        .unwrap();
    let reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let _beacon = ReaderBeacon::create(dir.path(), "consumer", &reader).unwrap();
    let path = dir.path().to_str().unwrap();
    let (v, code) = status_json(&[path]);
    assert_eq!(code, 0);
    assert_eq!(
        keys(&v),
        set(&[
            "schema_version",
            "journal",
            "sampled_unix_ns",
            "head",
            "segments",
            "checkpoint",
            "exposure",
            "storage",
            "rates",
            "writer",
            "readers",
            "health",
        ])
    );
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["journal"], path);
    assert!(v["sampled_unix_ns"].is_u64());
    assert_eq!(
        keys(&v["head"]),
        set(&[
            "next_seq",
            "segment_base_seq",
            "offset",
            "segment_size_bytes",
            "end",
            "invalid_reason",
            "last_local_ts_ns",
        ])
    );
    assert_eq!(v["head"]["next_seq"], 4);
    assert_eq!(v["head"]["end"], "open");
    assert_eq!(v["head"]["invalid_reason"], Value::Null);
    assert_eq!(v["head"]["last_local_ts_ns"], 123);
    assert_eq!(v["head"]["segment_size_bytes"], 65536);
    assert_eq!(
        keys(&v["segments"]),
        set(&[
            "count",
            "first_seq",
            "total_bytes",
            "oldest_created_unix_ns",
            "before_checkpoint",
        ])
    );
    assert_eq!(v["segments"]["count"], 1);
    assert_eq!(
        keys(&v["checkpoint"]),
        set(&[
            "generation",
            "next_seq",
            "segment_base_seq",
            "offset",
            "written_unix_ns",
            "age_s",
            "clock_skew",
        ])
    );
    assert_eq!(v["checkpoint"]["next_seq"], 3);
    assert!(v["checkpoint"]["age_s"].is_f64());
    assert_eq!(v["checkpoint"]["clock_skew"], false);
    assert_eq!(
        keys(&v["exposure"]),
        set(&["records", "bytes", "age_s", "oldest_local_ts_ns"])
    );
    assert_eq!(v["exposure"]["records"], 1);
    assert_eq!(v["exposure"]["oldest_local_ts_ns"], 123);
    assert_eq!(
        keys(&v["storage"]),
        set(&[
            "available_bytes",
            "total_bytes",
            "segment_size_bytes",
            "spare_segment",
            "rolls_left",
            "time_to_full_s",
        ])
    );
    assert!(v["storage"]["available_bytes"].is_u64());
    assert_eq!(v["storage"]["time_to_full_s"], Value::Null);
    assert_eq!(v["rates"], Value::Null);
    assert_eq!(keys(&v["writer"]), set(&["pid", "alive"]));
    assert_eq!(v["writer"]["pid"], std::process::id());
    assert_eq!(v["writer"]["alive"], true);
    let readers = v["readers"].as_array().unwrap();
    assert_eq!(readers.len(), 1);
    assert_eq!(
        keys(&readers[0]),
        set(&[
            "name",
            "state",
            "pid",
            "next_seq",
            "lag_records",
            "lag_bytes",
            "unread_age_s",
            "oldest_unread_local_ts_ns",
            "updated_unix_ns",
            "update_age_s",
        ])
    );
    assert_eq!(readers[0]["name"], "consumer");
    assert_eq!(readers[0]["state"], "live");
    assert_eq!(readers[0]["lag_records"], 4);
    assert_eq!(v["health"], serde_json::json!({"ok": true, "breaches": []}));
}

#[test]
fn health_exit_codes() {
    let (dir, mut writer) = journal(3, true);
    let path = dir.path().to_str().unwrap();
    // Healthy thresholds.
    let (v, code) = status_json(&[
        path,
        "--max-checkpoint-age",
        "1d",
        "--min-headroom",
        "1",
        "--min-headroom-segments",
        "1",
        "--max-reader-lag",
        "0",
    ]);
    assert_eq!((code, v["health"]["ok"].clone()), (0, Value::Bool(true)));
    // Everything covered: an old checkpoint is no exposure.
    assert_eq!(status_json(&[path, "--max-checkpoint-age", "0s"]).1, 0);
    // Uncovered records older than the limit.
    writer
        .append(&NewRecord {
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: b"x",
        })
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (v, code) = status_json(&[path, "--max-checkpoint-age", "1s"]);
    assert_eq!(code, 3);
    assert_eq!(
        v["health"],
        serde_json::json!({"ok": false, "breaches": [{"code": "checkpoint_age", "reader": null}]})
    );
    // Headroom.
    command()
        .args(["status", path, "--min-headroom", "1048576T"])
        .assert()
        .code(3)
        .stdout(predicate::str::contains("UNHEALTHY: headroom_bytes"))
        .stderr(predicate::str::contains("unhealthy: headroom_bytes"));
    let (v, code) = status_json(&[path, "--min-headroom-segments", "18446744073709551615"]);
    assert_eq!(code, 3);
    assert_eq!(v["health"]["breaches"][0]["code"], "headroom_segments");
    // A live lagging reader, then the same beacon after its owner is gone (stale: not lag).
    let reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let beacon = ReaderBeacon::create(dir.path(), "slow", &reader).unwrap();
    let (v, code) = status_json(&[path, "--max-reader-lag", "2"]);
    assert_eq!(code, 3);
    assert_eq!(
        v["health"]["breaches"],
        serde_json::json!([{"code": "reader_lag", "reader": "slow"}])
    );
    let stale = fs::read(beacon.path()).unwrap();
    let beacon_path = beacon.path().to_path_buf();
    drop(beacon);
    fs::write(&beacon_path, stale).unwrap();
    let (v, code) = status_json(&[path, "--max-reader-lag", "2"]);
    assert_eq!(code, 0);
    assert_eq!(v["readers"][0]["state"], "stale");
    drop(writer);
}

#[test]
fn invalid_head_and_corrupt_header() {
    let (dir, writer) = journal(3, false);
    drop(writer);
    let path = dir.path().to_str().unwrap();
    let segment = dir.path().join("00000000000000000000.tape");
    let file = fs::OpenOptions::new().write(true).open(&segment).unwrap();
    // Damage the last record's payload: the head stops before it and reports invalid bytes.
    file.write_all_at(&[0], 64 + 2 * 1040 + 40).unwrap();
    let (v, code) = status_json(&[path]);
    assert_eq!(code, 3);
    assert_eq!(v["head"]["end"], "invalid");
    assert_eq!(v["head"]["next_seq"], 2);
    assert_eq!(v["head"]["invalid_reason"], "record checksum mismatch");
    assert_eq!(v["health"]["breaches"][0]["code"], "head_invalid");
    // A damaged newest header is corruption.
    file.write_all_at(&[0], 0).unwrap();
    command().args(["status", path]).assert().code(2).stdout("");
}

/// Checks the Prometheus text exposition format 0.0.4: every sample is preceded by its family's HELP and TYPE
/// lines, names and label names are valid, label values are properly quoted and escaped, values parse as floats,
/// and no series repeats.
fn check_exposition(text: &str) -> Vec<(String, String, f64)> {
    fn valid_name(name: &str) -> bool {
        let mut chars = name.chars();
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == ':')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
    }
    assert!(text.ends_with('\n'));
    let mut families = BTreeSet::new();
    let mut current: Option<String> = None;
    let mut series = BTreeSet::new();
    let mut samples = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(rest) = line.strip_prefix("# HELP ") {
            let (name, help) = rest.split_once(' ').unwrap();
            assert!(valid_name(name) && !help.is_empty(), "{line}");
            let ty = lines.next().unwrap();
            assert_eq!(ty, format!("# TYPE {name} gauge"));
            assert!(families.insert(name.to_string()), "family {name} repeated");
            current = Some(name.to_string());
            continue;
        }
        assert!(!line.starts_with('#'), "unexpected comment {line}");
        let open = line.find('{').unwrap();
        let close = line.rfind('}').unwrap();
        let name = &line[..open];
        assert_eq!(
            Some(name),
            current.as_deref(),
            "sample outside its family: {line}"
        );
        // Labels: name="value" pairs separated by commas; values escape \ " and newline.
        let labels = &line[open + 1..close];
        let mut rest = labels;
        let mut parsed = Vec::new();
        while !rest.is_empty() {
            let eq = rest.find("=\"").unwrap();
            let label = &rest[..eq];
            assert!(valid_name(label), "label {label}");
            let mut value = String::new();
            let mut chars = rest[eq + 2..].char_indices();
            let end = loop {
                let (i, c) = chars.next().expect("unterminated label value");
                match c {
                    '\\' => match chars.next().unwrap().1 {
                        'n' => value.push('\n'),
                        c @ ('\\' | '"') => value.push(c),
                        c => panic!("bad escape \\{c}"),
                    },
                    '"' => break eq + 2 + i + 1,
                    '\n' => panic!("raw newline"),
                    c => value.push(c),
                }
            };
            parsed.push((label.to_string(), value));
            rest = &rest[end..];
            rest = rest.strip_prefix(',').unwrap_or(rest);
        }
        let value: f64 = line[close + 1..].trim_start_matches(' ').parse().unwrap();
        assert!(value.is_finite());
        assert!(
            series.insert(line[..close + 1].to_string()),
            "series repeated: {line}"
        );
        let reader = parsed
            .iter()
            .find(|(l, _)| l == "reader")
            .map_or(String::new(), |(_, v)| v.clone());
        assert_eq!(parsed[0].0, "journal");
        samples.push((name.to_string(), reader, value));
    }
    samples
}

#[test]
fn prometheus_output_is_valid() {
    let tmp = tempfile::tempdir().unwrap();
    // A journal path that needs label escaping.
    let dir = tmp.path().join("we\"ird\\name");
    let mut writer = Writer::open(&dir, WriterConfig::default().segment_size(65536)).unwrap();
    for _ in 0..3 {
        writer
            .append(&NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: tickertape::clock::realtime_ns(),
                payload: &[1; 10],
            })
            .unwrap();
    }
    writer.checkpoint().unwrap();
    let reader = Reader::open(&dir, ReaderConfig::default()).unwrap();
    let _beacon = ReaderBeacon::create(&dir, "r-1", &reader).unwrap();
    let out = command()
        .args(["status", dir.to_str().unwrap(), "--format", "prometheus"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    let samples = check_exposition(&text);
    let get = |name: &str, reader: &str| {
        samples
            .iter()
            .find(|(n, r, _)| n == name && r == reader)
            .unwrap_or_else(|| panic!("missing {name} {reader}\n{text}"))
            .2
    };
    assert_eq!(get("tickertape_head_next_seq", ""), 3.0);
    assert_eq!(get("tickertape_healthy", ""), 1.0);
    assert_eq!(get("tickertape_checkpoint_present", ""), 1.0);
    assert_eq!(get("tickertape_exposure_records", ""), 0.0);
    assert_eq!(get("tickertape_exposure_age_seconds", ""), 0.0);
    assert!(get("tickertape_storage_available_bytes", "") > 0.0);
    assert_eq!(get("tickertape_writer_alive", ""), 1.0);
    assert_eq!(get("tickertape_reader_live", "r-1"), 1.0);
    assert_eq!(get("tickertape_reader_lag_records", "r-1"), 3.0);
    assert!(text.contains(r#"journal=""#) && text.contains(r#"we\"ird\\name"#));
    drop(writer);
}

#[test]
fn watch_output_file_and_samples() {
    let (dir, writer) = journal(3, true);
    let path = dir.path().to_str().unwrap();
    let out = command()
        .args([
            "status",
            path,
            "--json",
            "--watch",
            "20ms",
            "--samples",
            "3",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["rates"], Value::Null);
    assert!(lines[2]["rates"]["interval_s"].as_f64().unwrap() >= 0.02);
    assert_eq!(lines[2]["rates"]["records_per_s"], 0.0);

    // --watch reports health instead of exiting with 3.
    command()
        .args([
            "status",
            path,
            "--watch",
            "10ms",
            "--samples",
            "2",
            "--min-headroom",
            "1048576T",
        ])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("UNHEALTHY").count(2));

    // --output replaces the file atomically and leaves no temporary file behind.
    let outdir = tempfile::tempdir().unwrap();
    let target = outdir.path().join("tickertape.prom");
    fs::write(&target, "old").unwrap();
    command()
        .args(["status", path, "--format", "prometheus", "--output"])
        .arg(&target)
        .args(["--watch", "10ms", "--samples", "2"])
        .assert()
        .code(0)
        .stdout("");
    let text = fs::read_to_string(&target).unwrap();
    check_exposition(&text);
    assert_eq!(fs::read_dir(outdir.path()).unwrap().count(), 1);
    // Never inside the journal.
    command()
        .args(["status", path, "--output"])
        .arg(dir.path().join("status.prom"))
        .assert()
        .code(1)
        .stderr(predicate::str::contains("outside the journal"));
    assert!(!dir.path().join("status.prom").exists());
    drop(writer);
}

#[test]
fn text_output() {
    let (dir, writer) = journal(70, true);
    command()
        .args(["status", dir.path().to_str().unwrap()])
        .assert()
        .code(0)
        .stdout(
            predicate::str::contains("head        next seq 70, segment 62")
                .and(predicate::str::contains(
                    "segments    2 (128.0 KiB), first seq 0",
                ))
                .and(predicate::str::contains(
                    "checkpoint  generation 1, next seq 70",
                ))
                .and(predicate::str::contains("exposure    0 records"))
                .and(predicate::str::contains("more segments of 64.0 KiB"))
                .and(predicate::str::contains("readers     none"))
                .and(predicate::str::contains("health      ok")),
        );
    drop(writer);
}
