//! Helpers shared by the integration, cross-process and crash tests.
//!
//! These manipulate journal files directly (to simulate crashes and corruption) using only the public
//! `tickertape::format` module and the standard library.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use tickertape::format::{self, SegmentHeader};
use tickertape::{NewRecord, OwnedRecord, Reader, ReaderConfig, Writer, WriterConfig};

/// Smallest allowed segment size, to make rolls happen quickly.
pub const SMALL_SEGMENT: u64 = format::SEGMENT_SIZE_UNIT;

/// A writer config with small segments.
pub fn small_config() -> WriterConfig {
    WriterConfig::default().segment_size(SMALL_SEGMENT)
}

/// Deterministic payload for sequence number `seq`: `len` bytes derived from `seq`.
pub fn payload(seq: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (seq as usize).wrapping_mul(31).wrapping_add(i) as u8)
        .collect()
}

/// The payload length used for `seq` by [`append_n`]: varies between 0 and 300 bytes.
pub fn payload_len(seq: u64) -> usize {
    ((seq * 37) % 301) as usize
}

/// Appends records `writer.next_seq() .. writer.next_seq() + n` with deterministic payloads.
pub fn append_n(writer: &mut Writer, n: u64) {
    for _ in 0..n {
        let seq = writer.next_seq();
        let data = payload(seq, payload_len(seq));
        let got = writer
            .append(&NewRecord {
                schema_id: (seq % 7) as u32,
                venue_ts_ns: seq * 10,
                local_ts_ns: seq * 10 + 1,
                payload: &data,
            })
            .expect("append");
        assert_eq!(got, seq);
    }
}

/// Checks that a record matches what [`append_n`] wrote for its sequence number.
pub fn check_record(r: &OwnedRecord) {
    assert_eq!(r.schema_id, (r.seq % 7) as u32, "schema id of {}", r.seq);
    assert_eq!(r.venue_ts_ns, r.seq * 10);
    assert_eq!(r.local_ts_ns, r.seq * 10 + 1);
    assert_eq!(
        r.payload,
        payload(r.seq, payload_len(r.seq)),
        "payload of {}",
        r.seq
    );
}

/// Reads every currently committed record with a fresh safe reader.
pub fn read_all(dir: &Path) -> Vec<OwnedRecord> {
    let mut reader = Reader::open(dir, ReaderConfig::default()).expect("open reader");
    let mut out = Vec::new();
    while let Some(r) = reader.next_owned().expect("read") {
        out.push(r);
    }
    out
}

/// Segment files in `dir`, sorted by base sequence number.
pub fn segments(dir: &Path) -> Vec<(u64, PathBuf)> {
    let mut out: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let e = e.unwrap();
            let base = format::parse_segment_file_name(e.file_name().to_str()?)?;
            Some((base, e.path()))
        })
        .collect();
    out.sort();
    out
}

/// Offset of the first empty slot after the records in segment `path` (whose first record is `base`).
pub fn end_of_records(path: &Path) -> (u64, u64) {
    let image = std::fs::read(path).unwrap();
    let header = format::decode_segment_header(image[..64].try_into().unwrap()).unwrap();
    let scan = format::scan_segment(
        &image[..],
        format::SEGMENT_HEADER_LEN as u64,
        header.base_seq,
    );
    (scan.end_offset, scan.next_seq)
}

/// Opens a segment file for raw writes.
pub fn open_raw(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap()
}

/// Creates a valid, empty segment file named for `base` (as a writer would, without the tmp/link dance).
pub fn create_raw_segment(dir: &Path, base: u64, size: u64) -> PathBuf {
    let path = dir.join(format::segment_file_name(base));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    file.set_len(size).unwrap();
    let header = format::encode_segment_header(&SegmentHeader {
        segment_size: size,
        base_seq: base,
        created_unix_ns: 1,
    });
    file.write_all_at(&header, 0).unwrap();
    path
}

/// Payload length for `seq` bounded by `max` (inclusive), used by the cross-process tests.
pub fn bounded_len(seq: u64, max: usize) -> usize {
    ((seq * 37) % (max as u64 + 1)) as usize
}

/// FNV-1a over the record fields, chained across records: two readers that saw the same records in the same order
/// produce the same digest.
pub fn digest_step(digest: u64, seq: u64, schema_id: u32, payload: &[u8]) -> u64 {
    let mut h = digest ^ 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    eat(&seq.to_le_bytes());
    eat(&schema_id.to_le_bytes());
    eat(&(payload.len() as u64).to_le_bytes());
    eat(payload);
    h
}

/// A small deterministic RNG (xorshift64*), so randomised tests can be replayed from a seed.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// Seeds from `TICKERTAPE_TEST_SEED` if set, else from the clock. Prints the seed so failures can be replayed.
    pub fn from_env(test: &str) -> Rng {
        let seed = std::env::var("TICKERTAPE_TEST_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| tickertape::clock::realtime_ns() | 1);
        eprintln!("{test}: TICKERTAPE_TEST_SEED={seed}");
        Rng::new(seed)
    }

    /// A generator with a fixed seed.
    pub fn new(seed: u64) -> Rng {
        Rng(seed.max(1))
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A number in `0..n` (n > 0).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// Payload length used by `tt-kill-writer` for `seq` (0..=512 bytes).
pub fn kill_len(seq: u64) -> usize {
    bounded_len(seq, 512)
}

/// Opens a writer, retrying briefly while the lock is held.
///
/// Rust opens files with close-on-exec, but a child process forked by a test running in parallel holds a copy of
/// every open descriptor until it calls `exec`, including a just-dropped writer's lock. Tests that drop a writer and
/// immediately reopen the journal use this to ride out that window.
pub fn reopen_writer(dir: &Path, config: WriterConfig) -> Writer {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match Writer::open(dir, config.clone()) {
            Err(tickertape::Error::Locked { .. }) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            other => return other.expect("reopen writer"),
        }
    }
}
