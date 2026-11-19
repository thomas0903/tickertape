//! Format-v1 compatibility (ADR 0033): the CLI (`verify`, `inspect`, `export --format jsonl`, `stats`) and the
//! dependency-free Python decoder (`python/tickertape_reader.py`) agree with the manifest for every journal written
//! by a released version (`tests/compat/fixtures/`). Each check runs on an unpacked copy.

#[path = "../../../tests/compat/sha256.rs"]
mod sha256;
#[path = "../../../tests/compat/support.rs"]
#[allow(dead_code)] // shared with tests/compat.rs, which uses all of it
mod support;

use std::path::Path;
use std::process::Command as ProcessCommand;

use assert_cmd::Command;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;
use support::{Fixture, fixtures};

fn cli(args: &[&str], dir: &Path) -> Vec<u8> {
    let out = Command::new(assert_cmd::cargo::cargo_bin!("tickertape"))
        .args(args)
        .arg(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tickertape {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

/// Record JSONL (as written by `export --format jsonl` and the Python decoder): count, first, last and stream digest.
fn jsonl_stream(bytes: &[u8]) -> (u64, Option<u64>, Option<u64>, String) {
    let mut h = sha256::Sha256::default();
    let (mut count, mut first, mut last) = (0, None, None::<u64>);
    for line in std::str::from_utf8(bytes).unwrap().lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        let seq = v["seq"].as_u64().unwrap();
        if let Some(prev) = last {
            assert_eq!(seq, prev + 1, "gap in JSONL");
        }
        first.get_or_insert(seq);
        last = Some(seq);
        count += 1;
        let payload = STANDARD.decode(v["payload_b64"].as_str().unwrap()).unwrap();
        sha256::add_record(
            &mut h,
            seq,
            u32::try_from(v["schema_id"].as_u64().unwrap()).unwrap(),
            v["venue_ts_ns"].as_u64().unwrap(),
            v["local_ts_ns"].as_u64().unwrap(),
            &payload,
        );
    }
    (count, first, last, h.hex())
}

fn check_full(f: &Fixture, got: (u64, Option<u64>, Option<u64>, String), who: &str) {
    assert_eq!(
        got,
        (
            f.u64("records"),
            Some(f.u64("first_seq")),
            Some(f.u64("last_seq")),
            f.str("stream_sha256").to_string()
        ),
        "{}: {who}",
        f.name
    );
}

fn check_durable(f: &Fixture, got: (u64, Option<u64>, Option<u64>, String), who: &str) {
    assert_eq!(got.0, f.u64("durable_records"), "{}: {who}", f.name);
    assert_eq!(
        got.2,
        Some(f.checkpoint("next_seq") - 1),
        "{}: {who}",
        f.name
    );
    assert_eq!(got.3, f.str("durable_stream_sha256"), "{}: {who}", f.name);
}

#[test]
fn cli_agrees_with_the_manifest() {
    for f in fixtures() {
        let (_tmp, dir) = f.unpack();

        let verify = json(&cli(&["verify", "--json"], &dir));
        assert_eq!(verify["records"], f.u64("records"), "{}", f.name);
        assert_eq!(verify["first_seq"], f.u64("first_seq"));
        assert_eq!(verify["last_seq"], f.u64("last_seq"));
        assert_eq!(verify["problems"], Value::Array(vec![]), "{}", f.name);
        let segments = f.json["segments"].as_array().unwrap();
        assert_eq!(verify["segments"], segments.len() as u64);

        let inspect = json(&cli(&["inspect", "--json"], &dir));
        assert_eq!(inspect["totals"]["records"], f.u64("records"), "{}", f.name);
        assert_eq!(inspect["checkpoint"], f.json["checkpoint"], "{}", f.name);
        assert_eq!(inspect["problems"], Value::Array(vec![]));
        let listed = inspect["segments"].as_array().unwrap();
        assert_eq!(listed.len(), segments.len());
        for (got, want) in listed.iter().zip(segments) {
            for key in ["file", "base_seq", "records", "end_seq", "used_bytes"] {
                assert_eq!(got[key], want[key], "{}: inspect {key}", f.name);
            }
            assert_eq!(got["end_state"], want["end"], "{}", f.name);
            assert_eq!(got["size_bytes"], 65_536);
        }

        let export = cli(&["export", "--format", "jsonl"], &dir);
        check_full(&f, jsonl_stream(&export), "export");
        let durable = cli(&["export", "--format", "jsonl", "--durable-only"], &dir);
        check_durable(&f, jsonl_stream(&durable), "export --durable-only");

        let stats = json(&cli(&["stats", "--json"], &dir));
        assert_eq!(stats["totals"]["records"], f.u64("records"), "{}", f.name);
        assert_eq!(stats["totals"]["first_seq"], f.u64("first_seq"));
        assert_eq!(stats["totals"]["last_seq"], f.u64("last_seq"));
        let schemas: u64 = stats["schemas"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["records"].as_u64().unwrap())
            .sum();
        assert_eq!(schemas, f.u64("records"));
        let durable_stats = json(&cli(&["stats", "--json", "--durable-only"], &dir));
        assert_eq!(durable_stats["totals"]["records"], f.u64("durable_records"));
    }
}

/// Runs the Python decoder if a Python 3 interpreter is available (`TICKERTAPE_PYTHON`, default `python3`).
/// Required in CI (`CI` set) or with `TICKERTAPE_REQUIRE_PYTHON=1`.
#[test]
fn python_decoder_agrees_with_the_manifest() {
    let python = std::env::var_os("TICKERTAPE_PYTHON").unwrap_or_else(|| "python3".into());
    let available = ProcessCommand::new(&python)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !available {
        assert!(
            std::env::var_os("CI").is_none()
                && std::env::var("TICKERTAPE_REQUIRE_PYTHON").as_deref() != Ok("1"),
            "python3 is required here"
        );
        eprintln!("python_decoder_agrees_with_the_manifest: skipped, no python3");
        return;
    }
    let decoder = support::fixtures_dir().join("../../../python/tickertape_reader.py");
    for f in fixtures() {
        let (_tmp, dir) = f.unpack();
        let run = |extra: &[&str]| {
            let out = ProcessCommand::new(&python)
                .arg("-I")
                .arg(&decoder)
                .arg(&dir)
                .args(extra)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}: decoder failed: {}",
                f.name,
                String::from_utf8_lossy(&out.stderr)
            );
            out.stdout
        };
        let all = run(&[]);
        check_full(&f, jsonl_stream(&all), "python");
        // Same records as the CLI export, field for field.
        let export = cli(&["export", "--format", "jsonl"], &dir);
        let parse = |b: &[u8]| -> Vec<Value> {
            std::str::from_utf8(b)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        };
        assert_eq!(parse(&all), parse(&export), "{}: python vs export", f.name);
        check_durable(
            &f,
            jsonl_stream(&run(&["--durable-only"])),
            "python --durable-only",
        );
    }
}
