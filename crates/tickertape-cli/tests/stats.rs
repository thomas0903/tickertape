use std::collections::BTreeMap;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{Value, json};
use tempfile::TempDir;
use tickertape::{NewRecord, Writer, WriterConfig};

const SECOND: u64 = 1_000_000_000;

#[derive(Clone)]
struct Event {
    seq: u64,
    schema: u32,
    len: usize,
    venue: u64,
    local: u64,
}

fn command() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("tickertape"))
}

fn fixture() -> (TempDir, Writer, Vec<Event>) {
    let dir = TempDir::new().unwrap();
    let mut writer = Writer::open(dir.path(), WriterConfig::default().segment_size(65536)).unwrap();
    let lengths = [
        0, 1, 4095, 4096, 4097, 8192, 8193, 30000, 30000, 30000, 30000, 7,
    ];
    let venues = [0, 2, 3, 1, 3, 0, 2, 3, 4, 2, 2, 5];
    let locals = [1, 1, 2, 0, 4, 6, 5, 5, 8, 10, 10, 12];
    let events: Vec<_> = lengths
        .iter()
        .enumerate()
        .map(|(i, &len)| Event {
            seq: i as u64,
            schema: [9, 2, 9, 0][i % 4],
            len,
            venue: venues[i] * SECOND,
            local: locals[i] * SECOND,
        })
        .collect();
    for event in &events {
        writer
            .append(&NewRecord {
                schema_id: event.schema,
                venue_ts_ns: event.venue,
                local_ts_ns: event.local,
                payload: &vec![17; event.len],
            })
            .unwrap();
        if event.seq == 5 {
            writer.checkpoint().unwrap();
        }
    }
    assert!(
        tickertape::inspect::list_segments(dir.path())
            .unwrap()
            .len()
            >= 3
    );
    (dir, writer, events)
}

fn utc(ns: u64) -> String {
    format!("1970-01-01T00:00:{:02}.{:09}Z", ns / SECOND, ns % SECOND)
}

// Independent reference calculation stores/sorts all fixture records; the command must stream them.
fn time_reference(events: &[Event], field: fn(&Event) -> u64) -> Value {
    let times: Vec<_> = events.iter().map(field).filter(|&t| t != 0).collect();
    let min = times.iter().min().copied();
    let max = times.iter().max().copied();
    let span = min.zip(max).map(|(a, b)| (b - a) as f64 / SECOND as f64);
    let mut seconds = BTreeMap::<u64, u64>::new();
    for time in &times {
        *seconds.entry(time / SECOND).or_default() += 1;
    }
    let busiest = seconds.iter().min_by_key(|&(second, count)| (std::cmp::Reverse(*count), *second))
        .map(|(&second, &count)| json!({"records": count, "start_ns": second * SECOND, "start_utc": utc(second * SECOND)}));
    let decreases = events
        .windows(2)
        .filter(|pair| {
            let (a, b) = (field(&pair[0]), field(&pair[1]));
            a != 0 && b != 0 && b < a
        })
        .count();
    json!({"records": times.len(), "min_ns": min, "max_ns": max,
        "min_utc": min.map(utc), "max_utc": max.map(utc), "span_s": span,
        "mean_rate_per_s": span.filter(|&s| s > 0.0).map(|s| times.len() as f64 / s),
        "busiest_second": busiest, "decreases": decreases})
}

fn reference(events: &[Event]) -> Value {
    let mut lengths: Vec<_> = events.iter().map(|e| e.len as u64).collect();
    lengths.sort_unstable();
    let percentile = |p: usize| {
        if lengths.is_empty() {
            return None;
        }
        let length = lengths[(p * lengths.len()).div_ceil(100) - 1];
        Some(if length <= 4096 {
            length
        } else {
            length.next_power_of_two()
        })
    };
    let schemas: Vec<_> = events.iter().map(|e| e.schema).collect::<std::collections::BTreeSet<_>>()
        .into_iter().map(|schema| {
            let sizes: Vec<_> = events.iter().filter(|e| e.schema == schema).map(|e| e.len as u64).collect();
            let bytes: u64 = sizes.iter().sum();
            json!({"schema_id": schema, "records": sizes.len(), "share_percent": 100.0 * sizes.len() as f64 / events.len() as f64,
                "payload_bytes": bytes, "min_payload_bytes": sizes.iter().min(),
                "mean_payload_bytes": bytes as f64 / sizes.len() as f64, "max_payload_bytes": sizes.iter().max()})
        }).collect();
    let gap = events.windows(2).filter(|p| p[0].local != 0 && p[1].local != 0 && p[1].local >= p[0].local)
        .min_by_key(|p| (std::cmp::Reverse(p[1].local - p[0].local), p[0].seq))
        .map(|p| json!({"gap_s": (p[1].local - p[0].local) as f64 / SECOND as f64, "from_seq": p[0].seq, "to_seq": p[1].seq}));
    json!({"totals": {"records": events.len(), "first_seq": events.first().map(|e| e.seq), "last_seq": events.last().map(|e| e.seq),
        "payload_bytes": lengths.iter().sum::<u64>()}, "schemas": schemas,
        "payload_length": {"min_bytes": lengths.first(), "p50_bytes": percentile(50), "p90_bytes": percentile(90), "p99_bytes": percentile(99), "max_bytes": lengths.last()},
        "venue_ts": time_reference(events, |e| e.venue), "local_ts": time_reference(events, |e| e.local), "largest_local_gap": gap})
}

fn check(dir: &TempDir, args: &[&str], events: &[Event]) {
    let result = command()
        .args(["stats", "--json"])
        .args(args)
        .arg(dir.path())
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .success();
    let bytes = &result.get_output().stdout;
    assert_eq!(bytes.last(), Some(&b'\n'));
    assert_eq!(bytes.iter().filter(|&&b| b == b'\n').count(), 1);
    let actual: Value = serde_json::from_slice(bytes).unwrap();
    // Normalize the reference through the same JSON wire representation for floating-point round trips.
    let expected: Value =
        serde_json::from_slice(&serde_json::to_vec(&reference(events)).unwrap()).unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn stats_every_json_field_ranges_checkpoint_and_text() {
    let (dir, _writer, events) = fixture();
    check(&dir, &[], &events);
    check(&dir, &["--from", "2", "--to", "8"], &events[2..=8]);
    check(&dir, &["--from", "3"], &events[3..]);
    check(&dir, &["--to", "0"], &events[..1]);
    check(&dir, &["--from", "1000"], &[]);
    check(&dir, &["--durable-only"], &events[..6]);
    check(
        &dir,
        &["--durable-only", "--from", "4", "--to", "100"],
        &events[4..6],
    );
    check(&dir, &["--durable-only", "--from", "6"], &[]);
    command()
        .arg("stats")
        .arg(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "records=12 first_seq=0 last_seq=11",
        ))
        .stdout(predicate::str::contains("schema"))
        .stdout(predicate::str::contains("p99"))
        .stdout(predicate::str::contains("decreases="));
    command()
        .args(["stats", "--from", "8", "--to", "2"])
        .arg(dir.path())
        .assert()
        .code(1)
        .stdout("")
        .stderr(predicate::str::contains("--to"));
}

#[test]
fn stats_empty_unset_timestamps_and_no_checkpoint() {
    let dir = TempDir::new().unwrap();
    let mut writer = Writer::open(dir.path(), WriterConfig::default().segment_size(65536)).unwrap();
    check(&dir, &[], &[]);
    let events = vec![Event {
        seq: 0,
        schema: 0,
        len: 0,
        venue: 0,
        local: 0,
    }];
    writer
        .append(&NewRecord {
            schema_id: 0,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: &[],
        })
        .unwrap();
    check(&dir, &[], &events);
    check(&dir, &["--durable-only"], &[]);
}

#[test]
fn stats_corruption_has_no_prefix_output_and_missing_is_exit_one() {
    let (dir, writer, _) = fixture();
    drop(writer);
    use std::os::unix::fs::FileExt;
    // First frame has empty payload (40 bytes); corrupt the second record's payload.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.path().join("00000000000000000000.tape"))
        .unwrap();
    file.write_all_at(&[18], 64 + 40 + 40).unwrap();
    for args in [vec!["stats"], vec!["stats", "--json"]] {
        command()
            .args(args)
            .arg(dir.path())
            .assert()
            .code(2)
            .stdout("")
            .stderr(predicate::str::contains("checksum mismatch"));
    }
    // The inclusive upper bound must stop before the corrupt record.
    check(
        &dir,
        &["--to", "0"],
        &[Event {
            seq: 0,
            schema: 9,
            len: 0,
            venue: 0,
            local: SECOND,
        }],
    );
    command()
        .arg("stats")
        .arg(dir.path().join("missing"))
        .assert()
        .code(1)
        .stdout("");
}

#[test]
fn stats_spills_unordered_seconds_and_keeps_earliest_tie() {
    let dir = TempDir::new().unwrap();
    let mut writer = Writer::open(dir.path(), WriterConfig::default().segment_size(65536)).unwrap();
    // More than four sort chunks, a partial last run, many distinct seconds, late repeated winner.
    let mut events = Vec::new();
    for i in 0..40_003 {
        let second = if i >= 40_000 {
            1
        } else {
            1 + (i * 7919) % 40_000
        };
        let event = Event {
            seq: i,
            schema: 2,
            len: 0,
            venue: second * SECOND,
            local: 0,
        };
        writer
            .append(&NewRecord {
                schema_id: 2,
                venue_ts_ns: event.venue,
                local_ts_ns: 0,
                payload: &[],
            })
            .unwrap();
        events.push(event);
    }
    // This fixture's timestamps extend past one minute; assert the complete scalar statistics separately.
    let result = command()
        .args(["stats", "--json"])
        .arg(dir.path())
        .assert()
        .success();
    let value: Value = serde_json::from_slice(&result.get_output().stdout).unwrap();
    assert_eq!(value["totals"]["records"], events.len());
    assert_eq!(
        value["venue_ts"]["busiest_second"],
        json!({"records":4,"start_ns":SECOND,"start_utc":utc(SECOND)})
    );
    assert_eq!(
        value["venue_ts"]["decreases"],
        events
            .windows(2)
            .filter(|p| p[1].venue < p[0].venue)
            .count()
    );
    let before: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            (path.clone(), std::fs::read(path).unwrap())
        })
        .collect();
    command()
        .args(["stats", "--json"])
        .arg(dir.path())
        .env("TMPDIR", dir.path())
        .assert()
        .code(1)
        .stdout("")
        .stderr(predicate::str::contains("outside the journal"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), before.len());
    for (path, bytes) in before {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    let scratch = TempDir::new().unwrap();
    command()
        .args(["stats", "--json"])
        .arg(dir.path())
        .env("TMPDIR", scratch.path())
        .assert()
        .success();
    assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
}

#[test]
fn stats_fractional_timestamps_and_bucket_boundaries() {
    let dir = TempDir::new().unwrap();
    let mut writer = Writer::open(dir.path(), WriterConfig::default().segment_size(65536)).unwrap();
    let events: Vec<_> = [
        1,
        SECOND - 1,
        SECOND,
        2 * SECOND - 1,
        2 * SECOND,
        SECOND + 1,
    ]
    .into_iter()
    .enumerate()
    .map(|(i, ts)| Event {
        seq: i as u64,
        schema: u32::MAX,
        len: i,
        venue: ts,
        local: 2 * SECOND - ts + 1,
    })
    .collect();
    for event in &events {
        writer
            .append(&NewRecord {
                schema_id: event.schema,
                venue_ts_ns: event.venue,
                local_ts_ns: event.local,
                payload: &vec![0; event.len],
            })
            .unwrap();
    }
    check(&dir, &[], &events);
}
