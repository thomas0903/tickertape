//! Format-v1 compatibility fixture generator (`tests/compat/README.md`).
//!
//! `tests/compat/generate.sh` compiles this file against the library of a released tag (or the working tree), so
//! it only uses API that exists in every release since v0.1.0. Cargo features add later API:
//! `durable-only` (v0.2.0+).
//!
//! Usage:
//! - `tt-compat-gen clean <dir>`: write the "clean" journal (closed normally) and print its manifest entry.
//! - `tt-compat-gen torn <dir>`: write the same records plus a few more in a child process, kill the child with
//!   SIGKILL in the middle of an append (a torn, uncommitted record), and print the manifest entry.
//! - `tt-compat-gen read <dir>`: read a journal with this version's readers and `verify`, and print what they saw
//!   (used to run old readers against journals written by the current code).
//!
//! Only `clean` and `torn` write; `read` and the manifest scans never modify the journal.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use tickertape::{NewRecord, Reader, ReaderConfig, Writer, WriterConfig};

#[path = "../sha256.rs"]
#[allow(dead_code)]
mod sha256;

/// Segment size: the smallest allowed, so the fixtures stay small and roll often.
const SEGMENT: u64 = 65_536;
/// Largest payload that fits a 64 KiB segment: `65536 - 64 - 8 - 40`.
const MAX_PAYLOAD: usize = 65_424;

const SCHEMAS: [u32; 11] = [
    0,
    1,
    2,
    7,
    42,
    255,
    65_535,
    65_536,
    0x7fff_ffff,
    0x8000_0000,
    u32::MAX,
];
const TIMES: [u64; 9] = [
    0,
    1,
    u64::MAX,
    u64::MAX - 1,
    i64::MAX as u64,
    1 << 63,
    1_700_000_000_123_456_789,
    999_999_999,
    4_102_444_800_000_000_000,
];
const SMALL: [usize; 23] = [
    0, 1, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 100, 127, 128, 129, 200, 255, 256, 257, 300,
];

#[derive(Clone, Copy)]
enum Step {
    /// `n` records with small payloads (0 to 300 bytes, all byte values non-zero).
    Small(u64),
    /// One record with a mostly-zero payload of this length.
    Big(usize),
    /// `Writer::checkpoint`.
    Checkpoint,
}

/// The clean journal: 4 segments of 64 KiB. Segment 1 holds a single maximum-size record; the checkpoint lies in the
/// middle of segment 2, so a checkpoint-covered prefix is followed by an uncovered tail that spans a roll.
const PLAN: &[Step] = &[
    Step::Small(40),
    Step::Big(MAX_PAYLOAD),
    Step::Small(30),
    Step::Big(40_000),
    Step::Small(10),
    Step::Checkpoint,
    Step::Small(25),
    Step::Big(30_000),
    Step::Small(20),
];
/// Extra records the torn journal has before its torn append.
const TORN_EXTRA: u64 = 15;
/// Payload length of the torn (never committed) append.
const TORN_LEN: usize = 200;

fn small_payload(seq: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((seq as usize).wrapping_mul(131).wrapping_add(i * 7) % 251 + 1) as u8)
        .collect()
}

fn big_payload(seq: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| {
            if i < 64 || i + 64 >= len || i % 4096 == 0 {
                ((seq as usize).wrapping_add(i) as u8) | 1
            } else {
                0
            }
        })
        .collect()
}

struct Rec {
    schema_id: u32,
    venue_ts_ns: u64,
    local_ts_ns: u64,
    payload: Vec<u8>,
}

fn record(seq: u64, big: Option<usize>) -> Rec {
    let i = seq as usize;
    Rec {
        schema_id: SCHEMAS[i % SCHEMAS.len()],
        venue_ts_ns: TIMES[i % TIMES.len()],
        local_ts_ns: TIMES[(i * 4 + 3) % TIMES.len()],
        payload: match big {
            Some(len) => big_payload(seq, len),
            None => small_payload(seq, SMALL[i % SMALL.len()]),
        },
    }
}

fn append(writer: &mut Writer, big: Option<usize>, expected: &mut Vec<Rec>) {
    let seq = writer.next_seq();
    let rec = record(seq, big);
    let got = writer
        .append(&NewRecord {
            schema_id: rec.schema_id,
            venue_ts_ns: rec.venue_ts_ns,
            local_ts_ns: rec.local_ts_ns,
            payload: &rec.payload,
        })
        .expect("append");
    assert_eq!(got, seq);
    assert_eq!(expected.len() as u64, seq);
    expected.push(rec);
}

/// Runs the plan (and the torn extras if `torn`); returns the expected records.
fn write_plan(writer: &mut Writer, torn: bool) -> Vec<Rec> {
    let mut expected = Vec::new();
    for step in PLAN {
        match *step {
            Step::Small(n) => (0..n).for_each(|_| append(writer, None, &mut expected)),
            Step::Big(len) => append(writer, Some(len), &mut expected),
            Step::Checkpoint => writer.checkpoint().expect("checkpoint"),
        }
    }
    if torn {
        (0..TORN_EXTRA).for_each(|_| append(writer, None, &mut expected));
    }
    expected
}

fn config() -> WriterConfig {
    WriterConfig::default().segment_size(SEGMENT)
}

fn fresh(dir: &Path) {
    assert!(
        !dir.exists(),
        "{} already exists; generate into a new directory",
        dir.display()
    );
}

/// What a reader saw: record count, first and last sequence number, and the stream digest.
struct Seen {
    records: u64,
    first: Option<u64>,
    last: Option<u64>,
    digest: String,
}

fn read_with(mut reader: Reader) -> Seen {
    let mut h = sha256::Sha256::default();
    let mut seen = Seen {
        records: 0,
        first: None,
        last: None,
        digest: String::new(),
    };
    while let Some(r) = reader.try_next().expect("read") {
        if let Some(last) = seen.last {
            assert_eq!(r.seq, last + 1, "gap or reorder");
        }
        seen.first.get_or_insert(r.seq);
        seen.last = Some(r.seq);
        seen.records += 1;
        sha256::add_record(
            &mut h,
            r.seq,
            r.schema_id,
            r.venue_ts_ns,
            r.local_ts_ns,
            r.payload,
        );
    }
    seen.digest = h.hex();
    seen
}

fn read_safe(dir: &Path) -> Seen {
    read_with(Reader::open(dir, ReaderConfig::default()).expect("open reader"))
}

fn read_mapped(dir: &Path) -> Seen {
    // SAFETY: nothing modifies the journal while this reader exists.
    read_with(unsafe { Reader::open_mapped(dir, ReaderConfig::default()) }.expect("open mapped"))
}

#[cfg(feature = "durable-only")]
fn read_durable(dir: &Path, mapped: bool) -> Seen {
    let config = ReaderConfig::default().durable_only(true);
    if mapped {
        // SAFETY: nothing modifies the journal while this reader exists.
        read_with(unsafe { Reader::open_mapped(dir, config) }.expect("open mapped"))
    } else {
        read_with(Reader::open(dir, config).expect("open reader"))
    }
}

fn digest_of(records: &[Rec]) -> String {
    let mut h = sha256::Sha256::default();
    for (seq, r) in records.iter().enumerate() {
        sha256::add_record(
            &mut h,
            seq as u64,
            r.schema_id,
            r.venue_ts_ns,
            r.local_ts_ns,
            &r.payload,
        );
    }
    h.hex()
}

fn opt(v: Option<u64>) -> String {
    v.map_or_else(|| "null".to_string(), |v| v.to_string())
}

/// Every regular file under `dir` (relative names, `/`-separated), sorted.
fn files(dir: &Path) -> Vec<(String, PathBuf)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            let meta = std::fs::symlink_metadata(&path).expect("metadata");
            if meta.is_dir() {
                walk(root, &path, out);
            } else {
                assert!(meta.is_file(), "unexpected file type: {}", path.display());
                let name = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .expect("utf-8 name");
                out.push((name.replace('\\', "/"), path));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Prints the manifest entry for a generated journal, after checking that this version's readers return exactly
/// `expected`.
fn describe(dir: &Path, kind: &str, expected: &[Rec]) {
    let digest = digest_of(expected);
    let safe = read_safe(dir);
    let mapped = read_mapped(dir);
    for (who, seen) in [("safe", &safe), ("mapped", &mapped)] {
        assert_eq!(seen.records, expected.len() as u64, "{who} reader count");
        assert_eq!(seen.digest, digest, "{who} reader stream");
    }
    let checkpoint = tickertape::inspect::read_checkpoint(dir)
        .expect("read checkpoint")
        .expect("a checkpoint");
    let durable = &expected[..checkpoint.next_seq as usize];
    #[cfg(feature = "durable-only")]
    for mapped in [false, true] {
        let seen = read_durable(dir, mapped);
        assert_eq!(
            seen.records,
            durable.len() as u64,
            "durable-only reader count"
        );
        assert_eq!(
            seen.digest,
            digest_of(durable),
            "durable-only reader stream"
        );
    }
    let report = tickertape::inspect::verify(dir).expect("verify");
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    assert_eq!(report.records, expected.len() as u64);

    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!("  \"kind\": \"{kind}\",\n"));
    out.push_str(&format!(
        "  \"library_version\": \"{}\",\n",
        tickertape::VERSION
    ));
    out.push_str(&format!("  \"segment_size\": {SEGMENT},\n"));
    out.push_str(&format!("  \"records\": {},\n", expected.len()));
    out.push_str(&format!("  \"first_seq\": {},\n", opt(safe.first)));
    out.push_str(&format!("  \"last_seq\": {},\n", opt(safe.last)));
    out.push_str(&format!("  \"next_seq\": {},\n", expected.len()));
    out.push_str(&format!("  \"stream_sha256\": \"{digest}\",\n"));
    out.push_str(&format!(
        "  \"durable_records\": {},\n  \"durable_stream_sha256\": \"{}\",\n",
        durable.len(),
        digest_of(durable)
    ));
    out.push_str(&format!(
        "  \"checkpoint\": {{\"generation\": {}, \"next_seq\": {}, \"segment_base_seq\": {}, \"offset\": {}}},\n",
        checkpoint.generation, checkpoint.next_seq, checkpoint.segment_base_seq, checkpoint.offset
    ));
    let segments: Vec<String> = report
        .segments
        .iter()
        .map(|s| {
            let end = match s.end {
                tickertape::inspect::SegmentEnd::Sealed => "sealed",
                tickertape::inspect::SegmentEnd::Open => "open",
                _ => "damaged",
            };
            format!(
                "{{\"file\": \"{}\", \"base_seq\": {}, \"records\": {}, \"end_seq\": {}, \"used_bytes\": {}, \"end\": \"{end}\"}}",
                s.path.file_name().unwrap().to_str().unwrap(),
                s.info.as_ref().map(|i| i.base_seq).unwrap(),
                s.records,
                s.end_seq,
                s.used_bytes
            )
        })
        .collect();
    out.push_str(&format!(
        "  \"segments\": [\n    {}\n  ],\n",
        segments.join(",\n    ")
    ));
    let newest = report.segments.last().unwrap();
    if kind == "torn" {
        // The torn append's bytes lie after the newest segment's last record and were never committed.
        let image = std::fs::read(&newest.path).expect("read newest segment");
        let start = newest.used_bytes as usize;
        let end = image.iter().rposition(|&b| b != 0).map_or(start, |i| i + 1);
        assert!(end > start, "the torn append left no bytes");
        out.push_str(&format!(
            "  \"torn\": {{\"file\": \"{}\", \"offset\": {start}, \"uncommitted_end\": {end}}},\n",
            newest.path.file_name().unwrap().to_str().unwrap()
        ));
    }
    let entries: Vec<String> = files(dir)
        .into_iter()
        .map(|(name, path)| {
            let bytes = std::fs::read(&path).expect("read file");
            format!(
                "{{\"name\": \"{name}\", \"len\": {}, \"sha256\": \"{}\"}}",
                bytes.len(),
                sha256::hex(&bytes)
            )
        })
        .collect();
    out.push_str(&format!(
        "  \"files\": [\n    {}\n  ]\n}}\n",
        entries.join(",\n    ")
    ));
    std::io::stdout().write_all(out.as_bytes()).expect("stdout");
}

fn clean(dir: &Path) {
    fresh(dir);
    let mut writer = Writer::open(dir, config()).expect("open writer");
    let expected = write_plan(&mut writer, false);
    drop(writer);
    describe(dir, "clean", &expected);
}

fn torn(dir: &Path) {
    fresh(dir);
    let exe = std::env::current_exe().expect("current_exe");
    let mut child = Command::new(exe)
        .arg("torn-child")
        .arg(dir)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn child");
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .expect("read child");
    assert_eq!(line.trim(), "torn", "child did not reach the torn append");
    child.kill().expect("SIGKILL");
    child.wait().expect("wait");

    // The same plan, recomputed: the child wrote exactly these records before the torn append.
    let mut expected = Vec::new();
    let mut seq = 0;
    let mut push = |big| {
        expected.push(record(seq, big));
        seq += 1;
    };
    for step in PLAN {
        match *step {
            Step::Small(n) => (0..n).for_each(|_| push(None)),
            Step::Big(len) => push(Some(len)),
            Step::Checkpoint => {}
        }
    }
    (0..TORN_EXTRA).for_each(|_| push(None));
    describe(dir, "torn", &expected);
}

fn torn_child(dir: &Path) {
    let mut writer = Writer::open(dir, config()).expect("open writer");
    let expected = write_plan(&mut writer, true);
    // Let background work started by the appends (the macOS spare segment, v0.3.0+) finish, as it would in a
    // long-running writer.
    std::thread::sleep(Duration::from_millis(500));
    let seq = expected.len() as u64;
    let rec = record(seq, None);
    let payload = small_payload(seq, TORN_LEN);
    let _ = writer.append_with(
        &NewRecord {
            schema_id: rec.schema_id,
            venue_ts_ns: rec.venue_ts_ns,
            local_ts_ns: rec.local_ts_ns,
            payload: &[],
        },
        TORN_LEN,
        |buf| {
            buf.copy_from_slice(&payload);
            let mut out = std::io::stdout().lock();
            writeln!(out, "torn").expect("stdout");
            out.flush().expect("stdout");
            // The parent kills this process now, before the commit word is stored.
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        },
    );
    unreachable!("the torn append returned");
}

fn read(dir: &Path) {
    let safe = read_safe(dir);
    let mapped = read_mapped(dir);
    let report = tickertape::inspect::verify(dir).expect("verify");
    let checkpoint = tickertape::inspect::read_checkpoint(dir).expect("read checkpoint");
    let mut out = format!(
        "{{\"library_version\": \"{}\", \"safe\": {{\"records\": {}, \"first_seq\": {}, \"last_seq\": {}, \"stream_sha256\": \"{}\"}}, \"mapped\": {{\"records\": {}, \"stream_sha256\": \"{}\"}}, \"verify\": {{\"records\": {}, \"first_seq\": {}, \"last_seq\": {}, \"problems\": {}}}, \"checkpoint_next_seq\": {}",
        tickertape::VERSION,
        safe.records,
        opt(safe.first),
        opt(safe.last),
        safe.digest,
        mapped.records,
        mapped.digest,
        report.records,
        opt(report.first_seq),
        opt(report.last_seq),
        report.problems.len(),
        opt(checkpoint.map(|c| c.next_seq)),
    );
    #[cfg(feature = "durable-only")]
    {
        let durable = read_durable(dir, false);
        let durable_mapped = read_durable(dir, true);
        out.push_str(&format!(
            ", \"durable\": {{\"records\": {}, \"stream_sha256\": \"{}\", \"mapped_records\": {}, \"mapped_stream_sha256\": \"{}\"}}",
            durable.records, durable.digest, durable_mapped.records, durable_mapped.digest
        ));
    }
    out.push_str("}\n");
    std::io::stdout().write_all(out.as_bytes()).expect("stdout");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(mode), Some(dir)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: tt-compat-gen <clean|torn|read> <dir>");
        std::process::exit(1);
    };
    let dir = PathBuf::from(dir);
    match mode.as_str() {
        "clean" => clean(&dir),
        "torn" => torn(&dir),
        "torn-child" => torn_child(&dir),
        "read" => read(&dir),
        other => {
            eprintln!("unknown mode {other}");
            std::process::exit(1);
        }
    }
}
