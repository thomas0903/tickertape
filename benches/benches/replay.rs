//! Warm-page-cache replay, including reader opening and record validation.
//! An untimed full replay before each sample warms the cache. No cache purge or
//! privileged operation is used; use tt-latency --cold for best-effort cold replay.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};
use tickertape::{NewRecord, Reader, ReaderConfig, Writer, WriterConfig};

fn replay_once(dir: &Path, mapped: bool, count: u64) {
    let mut reader = if mapped {
        // SAFETY: only the tickertape writer modifies this private temporary journal.
        unsafe { Reader::open_mapped(dir, ReaderConfig::default()).expect("open mapped reader") }
    } else {
        Reader::open(dir, ReaderConfig::default()).expect("open safe reader")
    };
    for seq in 0..count {
        let record = reader.try_next().expect("read").expect("record");
        assert_eq!(record.seq, seq);
        black_box(record.payload);
    }
}

fn replay(c: &mut Criterion) {
    for (size, count) in [(64, 1_000_000), (1024, 100_000)] {
        let dir = tempfile::tempdir().expect("temp journal");
        let payload = vec![0x5a; size];
        let mut writer = Writer::open(dir.path(), WriterConfig::default()).expect("open writer");
        for _ in 0..count {
            writer
                .append(&NewRecord {
                    schema_id: 1,
                    venue_ts_ns: 0,
                    local_ts_ns: 0,
                    payload: &payload,
                })
                .expect("append");
        }
        writer.checkpoint().expect("checkpoint fixture");
        drop(writer);
        for bytes in [true, false] {
            let mut group = c.benchmark_group(if bytes {
                "replay/payload_bytes"
            } else {
                "replay/records"
            });
            group.sample_size(10);
            group.throughput(if bytes {
                Throughput::Bytes(count * size as u64)
            } else {
                Throughput::Elements(count)
            });
            for mapped in [false, true] {
                group.bench_function(
                    BenchmarkId::new(if mapped { "mapped" } else { "safe" }, size),
                    |b| {
                        b.iter_custom(|iterations| {
                            replay_once(dir.path(), mapped, count);
                            let mut elapsed = Duration::ZERO;
                            for _ in 0..iterations {
                                let start = Instant::now();
                                replay_once(dir.path(), mapped, count);
                                elapsed += start.elapsed();
                            }
                            elapsed
                        });
                    },
                );
            }
            group.finish();
        }
    }
}
criterion_group!(benches, replay);
criterion_main!(benches);
