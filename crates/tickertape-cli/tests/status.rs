//! `tickertape status` from the outside. Every test takes `SERIAL`: each spawns the CLI, and a spawned child briefly
//! holds copies of all of this process's descriptors (between `fork` and `exec`), including the locks of beacons
//! that another test drops and expects to see stale.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::FileExt;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use tickertape::status::ReaderBeacon;
use tickertape::{NewRecord, Reader, ReaderConfig, Writer, WriterConfig};

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

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

/// Makes every segment and the checkpoint look `by` older: the creation time in each segment header and the marker's
/// modification time (a checkpoint can't predate its segment, so both move).
fn backdate(dir: &std::path::Path, by: Duration) {
    use tickertape::format;
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "tape") {
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            let mut buf = [0u8; format::SEGMENT_HEADER_LEN];
            file.read_exact_at(&mut buf, 0).unwrap();
            let mut header = format::decode_segment_header(&buf).unwrap();
            header.created_unix_ns -= by.as_nanos() as u64;
            file.write_all_at(&format::encode_segment_header(&header), 0)
                .unwrap();
        }
    }
    let marker = fs::OpenOptions::new()
        .write(true)
        .open(dir.join("checkpoint"))
        .unwrap();
    let modified = marker.metadata().unwrap().modified().unwrap();
    marker.set_modified(modified - by).unwrap();
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
    let _serial = serial();
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
                .and(predicate::str::contains("3 a health threshold"))
                // Round 3, item 9: the exception to an unknown age breaching --max-checkpoint-age.
                .and(predicate::str::contains("except marker_changing"))
                .and(predicate::str::contains(
                    "2 the sample found the journal corrupt",
                )),
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
        vec!["status", "x", "--watch", "1s", "--format", "prometheus"],
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
    let _serial = serial();
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
    let canonical = fs::canonicalize(dir.path()).unwrap();
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
            "readers_dir",
            "readers",
            "health",
        ])
    );
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["journal"], canonical.to_str().unwrap());
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
            "verified",
            "chain_broken",
        ])
    );
    assert_eq!(v["head"]["verified"], true);
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
            "problem",
            "modified_unix_ns",
            "estimated_age_s",
            "age_unknown_reason",
        ])
    );
    assert_eq!(v["checkpoint"]["next_seq"], 3);
    assert!(v["checkpoint"]["estimated_age_s"].is_f64());
    assert_eq!(v["checkpoint"]["problem"], Value::Null);
    assert_eq!(v["checkpoint"]["age_unknown_reason"], Value::Null);
    assert_eq!(
        keys(&v["exposure"]),
        set(&[
            "records",
            "bytes",
            "estimated_age_s",
            "oldest_local_ts_ns",
            "verified"
        ])
    );
    assert_eq!(v["exposure"]["verified"], true);
    assert_eq!(v["exposure"]["records"], 1);
    assert_eq!(v["exposure"]["oldest_local_ts_ns"], 123);
    assert_eq!(
        keys(&v["storage"]),
        set(&[
            "available_bytes",
            "total_bytes",
            "next_segment_size_bytes",
            "spare_segment",
            "estimated_rolls_left",
            "estimated_time_to_full_s",
        ])
    );
    assert!(v["storage"]["available_bytes"].is_u64());
    assert_eq!(v["storage"]["estimated_time_to_full_s"], Value::Null);
    assert_eq!(v["rates"], Value::Null);
    assert_eq!(keys(&v["writer"]), set(&["pid", "alive"]));
    assert_eq!(v["writer"]["pid"], std::process::id());
    assert_eq!(v["writer"]["alive"], true);
    assert_eq!(keys(&v["readers_dir"]), set(&["path", "state", "error"]));
    assert_eq!(v["readers_dir"]["state"], "listed");
    let readers = v["readers"].as_array().unwrap();
    assert_eq!(readers.len(), 1);
    assert_eq!(
        keys(&readers[0]),
        set(&[
            "name",
            "state",
            "pid",
            "next_seq",
            "durable_only",
            "seek_pending",
            "lag_records",
            "lag_bytes",
            "lag_unknown_reason",
            "unread_age_s",
            "oldest_unread_local_ts_ns",
            "updated_unix_ns",
            "update_age_s",
        ])
    );
    assert_eq!(readers[0]["name"], "consumer");
    assert_eq!(readers[0]["state"], "live");
    assert_eq!(readers[0]["lag_records"], 4);
    assert_eq!(readers[0]["durable_only"], false);
    assert_eq!(readers[0]["lag_unknown_reason"], Value::Null);
    assert_eq!(
        v["health"],
        serde_json::json!({"ok": true, "corrupt": false, "verified": true, "breaches": []})
    );
}

#[test]
fn health_exit_codes() {
    let _serial = serial();
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
    assert_eq!(status_json(&[path, "--max-checkpoint-age", "1h"]).1, 0);
    // An hour-old checkpoint (the segment's creation time is moved back too: a checkpoint can't predate it).
    backdate(dir.path(), Duration::from_secs(7200));
    let (v, code) = status_json(&[path, "--max-checkpoint-age", "1h"]);
    assert_eq!(code, 3);
    assert_eq!(
        v["health"],
        serde_json::json!({"ok": false, "corrupt": false, "verified": true, "breaches": [{"code": "checkpoint_age", "reader": null}]})
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
    let _serial = serial();
    let (dir, writer) = journal(3, false);
    drop(writer);
    let path = dir.path().to_str().unwrap();
    let segment = dir.path().join("00000000000000000000.tape");
    let file = fs::OpenOptions::new().write(true).open(&segment).unwrap();
    // Damage the last record's payload: the head stops before it and reports invalid bytes.
    file.write_all_at(&[0], 64 + 2 * 1040 + 40).unwrap();
    // Corruption, as `verify` reports it: exit 2 (not 3), with the sample on stdout.
    let (v, code) = status_json(&[path]);
    assert_eq!(code, 2);
    assert_eq!(v["head"]["end"], "invalid");
    assert_eq!(v["head"]["next_seq"], 2);
    assert_eq!(v["head"]["invalid_reason"], "record checksum mismatch");
    assert_eq!(v["health"]["breaches"][0]["code"], "head_invalid");
    assert_eq!(v["health"]["corrupt"], true);
    // Round 3, item 4: an invalid head is not verified, and the exposure is unknown (before the fix: verified, two
    // records, 120 bytes), in the JSON, the text and the Prometheus output.
    assert_eq!(v["head"]["verified"], false, "{v}");
    assert_eq!(v["health"]["verified"], false, "{v}");
    assert_eq!(v["exposure"]["verified"], false, "{v}");
    assert_eq!(v["exposure"]["records"], Value::Null, "{v}");
    assert_eq!(v["exposure"]["bytes"], Value::Null, "{v}");
    assert_eq!(v["exposure"]["estimated_age_s"], Value::Null, "{v}");
    let text = command().args(["status", path]).output().unwrap();
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(
        text.contains("(not verified: the head is at invalid bytes)"),
        "{text}"
    );
    assert!(
        text.contains("exposure    unknown records, ? bytes"),
        "{text}"
    );
    assert!(text.contains("(not verified)"), "{text}");
    assert!(text.contains("health      CORRUPT: head_invalid"), "{text}");
    let prom = command()
        .args(["status", path, "--format", "prometheus"])
        .output()
        .unwrap();
    let prom = String::from_utf8(prom.stdout).unwrap();
    let samples = check_exposition(&prom);
    let value = |name: &str| samples.iter().find(|(n, _, _)| n == name).map(|s| s.2);
    assert_eq!(value("tickertape_head_verified"), Some(0.0), "{prom}");
    assert_eq!(value("tickertape_exposure_verified"), Some(0.0), "{prom}");
    assert_eq!(value("tickertape_exposure_records"), None, "{prom}");
    assert_eq!(value("tickertape_exposure_bytes"), None, "{prom}");
    // Corruption takes precedence over a breached threshold.
    assert_eq!(status_json(&[path, "--min-headroom", "1048576T"]).1, 2);
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
    let _serial = serial();
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
    assert_eq!(get("tickertape_up", ""), 1.0);
    assert_eq!(get("tickertape_healthy", ""), 1.0);
    assert_eq!(get("tickertape_corrupt", ""), 0.0);
    assert_eq!(get("tickertape_head_verified", ""), 1.0);
    assert_eq!(get("tickertape_checkpoint_invalid", ""), 0.0);
    assert!(get("tickertape_status_timestamp_seconds", "") > 1e9);
    assert_eq!(get("tickertape_checkpoint_present", ""), 1.0);
    assert_eq!(get("tickertape_exposure_records", ""), 0.0);
    assert_eq!(get("tickertape_exposure_estimated_age_seconds", ""), 0.0);
    assert!(get("tickertape_storage_available_bytes", "") > 0.0);
    assert_eq!(get("tickertape_writer_alive", ""), 1.0);
    assert_eq!(get("tickertape_reader_live", "r-1"), 1.0);
    assert_eq!(get("tickertape_reader_lag_records", "r-1"), 3.0);
    assert!(text.contains(r#"journal=""#) && text.contains(r#"we\"ird\\name"#));
    // The label is the canonical path.
    let canonical = fs::canonicalize(&dir).unwrap();
    assert!(text.contains(&format!(
        "journal=\"{}\"",
        canonical.to_str().unwrap().replace('\\', "\\\\").replace('"', "\\\"")
    )));
    drop(writer);
}

#[test]
fn watch_output_file_and_samples() {
    let _serial = serial();
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

/// Round 2, C1: a journal that is missing when the command starts goes through failure reporting: a one-shot sample
/// replaces the --output file with `tickertape_up 0` (never leaving a stale healthy one), and a watch reports every
/// failed sample and goes on, picking the journal up once it exists.
#[test]
fn missing_journal_at_start_is_reported() {
    let _serial = serial();
    let (dir, writer) = journal(3, true);
    drop(writer);
    let outdir = tempfile::tempdir().unwrap();
    let target = outdir.path().join("t.prom");
    command()
        .args([
            "status",
            dir.path().to_str().unwrap(),
            "--format",
            "prometheus",
            "--output",
        ])
        .arg(&target)
        .assert()
        .code(0);
    assert!(
        fs::read_to_string(&target)
            .unwrap()
            .contains("tickertape_up{")
    );
    let moved = outdir.path().join("moved");
    fs::rename(dir.path(), &moved).unwrap();
    command()
        .args([
            "status",
            dir.path().to_str().unwrap(),
            "--format",
            "prometheus",
            "--output",
        ])
        .arg(&target)
        .assert()
        .code(1);
    let text = fs::read_to_string(&target).unwrap();
    let samples = check_exposition(&text);
    let up: Vec<_> = samples
        .iter()
        .filter(|(name, _, _)| name == "tickertape_up")
        .collect();
    assert_eq!(up.len(), 1, "{text}");
    assert_eq!(up[0].2, 0.0, "{text}");

    let out = command()
        .args([
            "status",
            dir.path().to_str().unwrap(),
            "--json",
            "--watch",
            "10ms",
            "--samples",
            "2",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let lines: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    for line in &lines {
        assert_eq!(line["error"]["code"], "not_found", "{line}");
    }
    fs::rename(&moved, dir.path()).unwrap();
}

/// Round 2, B2: a missing middle segment that the sample can't see (it doesn't read the segments between an older
/// checkpoint and the head) exits 0 but says "unverified" in the JSON and the text; with every segment gone and a
/// valid checkpoint, the sample reports a dangling checkpoint as the writer does (exit 2).
#[test]
fn unverified_chain_and_dangling_checkpoint_without_segments() {
    let _serial = serial();
    let (dir, mut writer) = journal(2, true);
    let path = dir.path().to_str().unwrap().to_string();
    // Fill a few more segments of 64 KiB (about 60 records of 1000 bytes each).
    for _ in 0..200 {
        writer
            .append(&NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload: &[7; 1000],
            })
            .unwrap();
    }
    drop(writer);
    let mut segments: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "tape"))
        .collect();
    segments.sort();
    assert!(segments.len() >= 3, "{segments:?}");
    fs::remove_file(&segments[1]).unwrap();
    assert!(Writer::open(dir.path(), WriterConfig::default().segment_size(65536)).is_err());
    let (v, code) = status_json(&[&path]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["head"]["verified"], false);
    assert_eq!(v["head"]["chain_broken"], false);
    assert_eq!(v["health"]["ok"], true);
    assert_eq!(v["health"]["verified"], false);
    command()
        .args(["status", &path])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("health      ok (unverified)\n"));

    let (dir, writer) = journal(3, true);
    drop(writer);
    for entry in fs::read_dir(dir.path()).unwrap() {
        let p = entry.unwrap().path();
        if p.extension().is_some_and(|e| e == "tape") {
            fs::remove_file(p).unwrap();
        }
    }
    command()
        .args(["status", dir.path().to_str().unwrap()])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("checkpoint"));
}

/// Round 2, A4: `--output` never replaces a file the journal reads through a symlink (a segment kept outside the
/// journal directory), nor a file in the beacon directory.
#[test]
fn output_never_replaces_a_file_the_journal_uses() {
    let _serial = serial();
    let (dir, writer) = journal(3, true);
    drop(writer);
    let outside = tempfile::tempdir().unwrap();
    let segment = dir.path().join("00000000000000000000.tape");
    let backing = outside.path().join("backing.tape");
    fs::rename(&segment, &backing).unwrap();
    std::os::unix::fs::symlink(&backing, &segment).unwrap();
    let before = fs::read(&backing).unwrap();
    // The journal is readable through the symlink.
    let (json, code) = status_json(&[dir.path().to_str().unwrap()]);
    assert_eq!(code, 0, "{json}");
    assert_eq!(json["head"]["next_seq"], 3);
    for args in [&[][..], &["--watch", "10ms", "--samples", "1"][..]] {
        command()
            .args([
                "status",
                dir.path().to_str().unwrap(),
                "--format",
                "prometheus",
                "--output",
            ])
            .arg(&backing)
            .args(args)
            .assert()
            .code(1)
            .stderr(predicate::str::contains(
                "the journal or its beacon directory uses",
            ));
        assert_eq!(
            fs::read(&backing).unwrap(),
            before,
            "the segment was replaced"
        );
    }
    // A beacon directory chosen with --readers-dir is off limits too.
    let beacons = tempfile::tempdir().unwrap();
    let victim = beacons.path().join("reader");
    fs::write(&victim, b"beacon").unwrap();
    command()
        .args(["status", dir.path().to_str().unwrap(), "--readers-dir"])
        .arg(beacons.path())
        .arg("--output")
        .arg(&victim)
        .assert()
        .code(1);
    assert_eq!(fs::read(&victim).unwrap(), b"beacon");
}

#[test]
fn text_output() {
    let _serial = serial();
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

/// Finding 16: with --watch, a failed sample is reported and the watch goes on; a Prometheus file then says
/// `tickertape_up 0` with a fresh timestamp instead of keeping the last good sample.
#[test]
fn watch_reports_failed_samples_and_continues() {
    let _serial = serial();
    let empty = tempfile::tempdir().unwrap();
    let path = empty.path().to_str().unwrap();
    let out = command()
        .args([
            "status",
            path,
            "--json",
            "--watch",
            "10ms",
            "--samples",
            "2",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    for line in &lines {
        assert_eq!(
            keys(line),
            set(&["schema_version", "journal", "sampled_unix_ns", "error"])
        );
        assert_eq!(line["error"]["code"], "not_found");
        assert!(
            line["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no journal segments")
        );
    }
    command()
        .args(["status", path, "--watch", "10ms", "--samples", "2"])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("error       no journal segments").count(2));

    // A journal whose sample fails after a good one: the file is replaced by `up 0`.
    let (dir, writer) = journal(3, true);
    let outdir = tempfile::tempdir().unwrap();
    let target = outdir.path().join("t.prom");
    command()
        .args([
            "status",
            dir.path().to_str().unwrap(),
            "--format",
            "prometheus",
            "--output",
        ])
        .arg(&target)
        .assert()
        .code(0);
    assert!(
        fs::read_to_string(&target)
            .unwrap()
            .contains("tickertape_up{")
    );
    drop(writer);
    let segment = dir.path().join("00000000000000000000.tape");
    fs::OpenOptions::new()
        .write(true)
        .open(&segment)
        .unwrap()
        .write_all_at(&[0], 0)
        .unwrap();
    command()
        .args([
            "status",
            dir.path().to_str().unwrap(),
            "--format",
            "prometheus",
            "--output",
        ])
        .arg(&target)
        .assert()
        .code(2);
    let text = fs::read_to_string(&target).unwrap();
    let samples = check_exposition(&text);
    let names: Vec<&str> = samples.iter().map(|(n, _, _)| n.as_str()).collect();
    assert_eq!(
        names,
        ["tickertape_status_timestamp_seconds", "tickertape_up"]
    );
    assert_eq!(samples[1].2, 0.0);
    assert!(samples[0].2 > 1e9);
    assert_eq!(fs::read_dir(outdir.path()).unwrap().count(), 1);
}

/// Finding 17: the journal is identified by its canonical path, however it was typed.
#[test]
fn journal_is_the_canonical_path() {
    let _serial = serial();
    let (dir, writer) = journal(1, true);
    let parent = dir.path().parent().unwrap();
    let name = dir.path().file_name().unwrap().to_str().unwrap();
    let out = command()
        .current_dir(parent)
        .args(["status", &format!("./{name}"), "--json"])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["journal"],
        fs::canonicalize(dir.path()).unwrap().to_str().unwrap()
    );
    drop(writer);
}

/// Finding 10: a checkpoint marker the writer refuses is corruption (exit 2), whatever else is healthy.
#[test]
fn dangling_checkpoint_exits_2() {
    let _serial = serial();
    let (dir, writer) = journal(130, true);
    drop(writer);
    let mut segments: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "tape"))
        .collect();
    segments.sort();
    fs::remove_file(segments.last().unwrap()).unwrap();
    let (v, code) = status_json(&[dir.path().to_str().unwrap()]);
    assert_eq!(code, 2);
    assert_eq!(v["checkpoint"]["problem"], "missing_segment");
    assert_eq!(v["exposure"]["records"], Value::Null);
    assert_eq!(
        v["health"]["breaches"],
        serde_json::json!([{"code": "checkpoint_invalid", "reader": null}])
    );
    command()
        .args(["status", dir.path().to_str().unwrap()])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("CORRUPT: checkpoint_invalid"));
}

/// Findings 12 and 18: a modification time in the future is an unknown age (named as such, not "no checkpoint"),
/// which breaches --max-checkpoint-age when records are uncovered.
#[test]
fn unreliable_checkpoint_time() {
    let _serial = serial();
    let (dir, mut writer) = journal(3, true);
    writer
        .append(&NewRecord {
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: b"x",
        })
        .unwrap();
    let marker = fs::OpenOptions::new()
        .write(true)
        .open(dir.path().join("checkpoint"))
        .unwrap();
    marker
        .set_modified(SystemTime::now() + Duration::from_secs(3600))
        .unwrap();
    let path = dir.path().to_str().unwrap();
    let (v, code) = status_json(&[path, "--max-checkpoint-age", "1d"]);
    assert_eq!(code, 3);
    assert_eq!(v["checkpoint"]["estimated_age_s"], Value::Null);
    assert_eq!(
        v["checkpoint"]["age_unknown_reason"],
        "modification_time_in_future"
    );
    command().args(["status", path]).assert().code(0).stdout(
        predicate::str::contains("unknown (the marker's modification time is in the future)")
            .and(predicate::str::contains("no checkpoint").not()),
    );
    drop(writer);
}

/// Finding 3: a FIFO in place of the marker fails the sample (exit 1) instead of hanging.
#[test]
fn fifo_marker_does_not_hang() {
    let _serial = serial();
    let (dir, writer) = journal(3, false);
    drop(writer);
    fs::remove_file(dir.path().join("checkpoint")).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(dir.path().join("checkpoint"))
            .status()
            .unwrap()
            .success()
    );
    command()
        .args(["status", dir.path().to_str().unwrap()])
        .timeout(Duration::from_secs(30))
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not a regular file"));
}

/// Finding 7: a beacon directory that can't be listed is reported, without failing the sample.
#[test]
fn unusable_readers_directory() {
    let _serial = serial();
    let (dir, writer) = journal(3, true);
    fs::write(dir.path().join("readers"), b"not a directory").unwrap();
    let (v, code) = status_json(&[dir.path().to_str().unwrap()]);
    assert_eq!(code, 0);
    assert_eq!(v["readers_dir"]["state"], "unusable");
    assert!(v["readers_dir"]["error"].is_string());
    assert_eq!(v["readers"], serde_json::json!([]));
    assert_eq!(v["head"]["next_seq"], 3);
    drop(writer);
}

/// The value of `tickertape_up` in an --output file.
fn up_in(path: &std::path::Path) -> f64 {
    let text = fs::read_to_string(path).unwrap();
    let samples = check_exposition(&text);
    let up: Vec<_> = samples
        .iter()
        .filter(|(name, _, _)| name == "tickertape_up")
        .collect();
    assert_eq!(up.len(), 1, "{text}");
    up[0].2
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// Round 3, item 1: a journal that can't be walked (unreadable, not a directory, a symlink loop) fails the sample,
/// and the --output file that held a healthy sample is replaced by `tickertape_up 0`, once (exit 1) and in a watch,
/// which goes on (exit 0 after its samples). Before the fix, the output check's walk of the journal failed first: the
/// command exited 1 and left `tickertape_up 1` in place, and a watch exited.
#[test]
fn output_reports_a_journal_that_cant_be_walked() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("j");
    let outdir = tempfile::tempdir().unwrap();
    let target = outdir.path().join("t.prom");
    let healthy = |target: &std::path::Path| {
        let mut writer = Writer::open(&path, WriterConfig::default().segment_size(65536)).unwrap();
        writer
            .append(&NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload: b"x",
            })
            .unwrap();
        writer.checkpoint().unwrap();
        drop(writer);
        command()
            .args([
                "status",
                path.to_str().unwrap(),
                "--format",
                "prometheus",
                "--output",
            ])
            .arg(target)
            .assert()
            .code(0);
        assert_eq!(up_in(target), 1.0);
    };
    type Step = Box<dyn Fn()>;
    let mut cases: Vec<(&str, Step, Step)> = Vec::new();
    let p = path.clone();
    cases.push((
        "not a directory",
        Box::new(move || {
            fs::rename(&p, p.with_extension("away")).unwrap();
            fs::write(&p, b"a file").unwrap();
        }),
        Box::new({
            let p = path.clone();
            move || {
                fs::remove_file(&p).unwrap();
                fs::rename(p.with_extension("away"), &p).unwrap();
            }
        }),
    ));
    let p = path.clone();
    cases.push((
        "symlink loop",
        Box::new(move || {
            fs::rename(&p, p.with_extension("away")).unwrap();
            std::os::unix::fs::symlink(&p, &p).unwrap();
        }),
        Box::new({
            let p = path.clone();
            move || {
                fs::remove_file(&p).unwrap();
                fs::rename(p.with_extension("away"), &p).unwrap();
            }
        }),
    ));
    if !is_root() {
        use std::os::unix::fs::PermissionsExt;
        let p = path.clone();
        cases.push((
            "unreadable",
            Box::new(move || fs::set_permissions(&p, fs::Permissions::from_mode(0o000)).unwrap()),
            Box::new({
                let p = path.clone();
                move || fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap()
            }),
        ));
    }
    for (case, break_it, repair) in &cases {
        for watch in [false, true] {
            healthy(&target);
            break_it();
            let mut cmd = command();
            cmd.args([
                "status",
                path.to_str().unwrap(),
                "--format",
                "prometheus",
                "--output",
            ])
            .arg(&target);
            if watch {
                cmd.args(["--watch", "10ms", "--samples", "3"]);
            }
            let out = cmd.output().unwrap();
            repair();
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert_eq!(
                out.status.code(),
                Some(if watch { 0 } else { 1 }),
                "{case}, watch {watch}: {stderr}"
            );
            assert_eq!(up_in(&target), 0.0, "{case}, watch {watch}: {stderr}");
            assert!(
                !stderr.contains("not written"),
                "{case}, watch {watch}: {stderr}"
            );
        }
    }
    assert_eq!(fs::read_dir(outdir.path()).unwrap().count(), 1);
    // An output that is itself a symlink loop names no file: it is replaced.
    fs::remove_file(&target).unwrap();
    std::os::unix::fs::symlink(&target, &target).unwrap();
    command()
        .args([
            "status",
            path.to_str().unwrap(),
            "--format",
            "prometheus",
            "--output",
        ])
        .arg(&target)
        .assert()
        .code(0);
    assert_eq!(up_in(&target), 1.0);
}

/// Round 3, item 1 (c): when it can't be established that the destination is not something the journal uses (here
/// the beacon directory chosen with --readers-dir can't be resolved), nothing is written, the reason goes to standard
/// error every time, and a watch goes on; a one-shot status exits 1.
#[test]
fn output_not_established_is_reported_and_the_watch_goes_on() {
    use std::os::unix::fs::PermissionsExt;
    if is_root() {
        return;
    }
    let _serial = serial();
    let (dir, writer) = journal(2, true);
    drop(writer);
    let locked = tempfile::tempdir().unwrap();
    let readers = locked.path().join("sub").join("readers");
    fs::create_dir_all(&readers).unwrap();
    fs::set_permissions(locked.path().join("sub"), fs::Permissions::from_mode(0o000)).unwrap();
    let outdir = tempfile::tempdir().unwrap();
    let target = outdir.path().join("t.prom");
    fs::write(&target, b"previous").unwrap();
    let run = |watch: bool| {
        let mut cmd = command();
        cmd.args([
            "status",
            dir.path().to_str().unwrap(),
            "--format",
            "prometheus",
        ])
        .arg("--readers-dir")
        .arg(&readers)
        .arg("--output")
        .arg(&target);
        if watch {
            cmd.args(["--watch", "10ms", "--samples", "3"]);
        }
        cmd.output().unwrap()
    };
    let watched = run(true);
    let once = run(false);
    fs::set_permissions(locked.path().join("sub"), fs::Permissions::from_mode(0o755)).unwrap();
    let stderr = String::from_utf8_lossy(&watched.stderr);
    assert_eq!(watched.status.code(), Some(0), "{stderr}");
    assert_eq!(stderr.matches("not written").count(), 3, "{stderr}");
    assert_eq!(once.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&once.stderr).contains("not written"));
    assert_eq!(fs::read(&target).unwrap(), b"previous");
}

/// Round 3, item 2: `--output` never replaces quarantined evidence: not a file in a `quarantine/` that is a symlink
/// to another directory (before the fix the check compared the output with the directory only, and replaced the
/// file), not a new file in that directory, and not a file a quarantine entry reaches through a symlink or a hard
/// link.
#[test]
fn output_never_replaces_quarantined_evidence() {
    let _serial = serial();
    let (dir, writer) = journal(2, true);
    drop(writer);
    let elsewhere = tempfile::tempdir().unwrap();
    let evidence = elsewhere.path().join("evidence");
    fs::create_dir(&evidence).unwrap();
    std::os::unix::fs::symlink(&evidence, dir.path().join("quarantine")).unwrap();
    let kept = evidence.join("00000000000000000000.tape.0");
    fs::write(&kept, b"repaired-away bytes").unwrap();
    // A quarantine entry that is a symlink to a file outside, and one hard-linked to a file outside.
    let linked_target = elsewhere.path().join("linked");
    fs::write(&linked_target, b"evidence through a symlink").unwrap();
    std::os::unix::fs::symlink(&linked_target, evidence.join("x.1")).unwrap();
    let hard = elsewhere.path().join("hard");
    fs::write(&hard, b"evidence through a hard link").unwrap();
    fs::hard_link(&hard, evidence.join("x.2")).unwrap();
    let fresh = evidence.join("new.prom");
    for victim in [&kept, &linked_target, &hard, &fresh] {
        let before = fs::read(victim).ok();
        for watch in [false, true] {
            let mut cmd = command();
            cmd.args([
                "status",
                dir.path().to_str().unwrap(),
                "--format",
                "prometheus",
                "--output",
            ])
            .arg(victim);
            if watch {
                cmd.args(["--watch", "10ms", "--samples", "1"]);
            }
            cmd.assert().code(1).stderr(predicate::str::contains(
                "the journal or its beacon directory uses",
            ));
            assert_eq!(fs::read(victim).ok(), before, "{}", victim.display());
        }
    }
}
