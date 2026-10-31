//! Append throughput on first-touch pages in fresh preallocated segments: cold
//! pages are the steady state of an append-only journal, not a warmed overwrite.
//! Each timed batch appends to a new journal; setup/open and teardown are untimed.
//! Payload copying, CRC and publication are timed; checkpoint durability is not.

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use tickertape::{NewRecord, Writer, WriterConfig};

const BATCH: u64 = 16_384;

fn append(c: &mut Criterion) {
    // Criterion supports one throughput unit per group; report both explicitly.
    for bytes in [true, false] {
        let mut group = c.benchmark_group(if bytes {
            "append/payload_bytes"
        } else {
            "append/records"
        });
        group.sample_size(10);
        for size in [64, 1024] {
            group.throughput(if bytes {
                Throughput::Bytes(BATCH * size as u64)
            } else {
                Throughput::Elements(BATCH)
            });
            let payload = vec![0x5a; size];
            let record = NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload: &payload,
            };
            group.bench_with_input(BenchmarkId::from_parameter(size), &record, |b, record| {
                b.iter_batched_ref(
                    || {
                        let dir = tempfile::tempdir().expect("temp journal");
                        let writer =
                            Writer::open(dir.path(), WriterConfig::default()).expect("open writer");
                        (writer, dir)
                    },
                    |(writer, _dir)| {
                        for _ in 0..BATCH {
                            black_box(writer.append(black_box(record)).expect("append"));
                        }
                    },
                    BatchSize::PerIteration,
                );
            });
        }
        group.finish();
    }
}
criterion_group!(benches, append);
criterion_main!(benches);
