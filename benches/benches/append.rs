//! Append throughput on first-touch pages in fresh preallocated segments: cold
//! pages are the steady state of an append-only journal, not a warmed overwrite.
//! Each timed batch appends to a new journal; setup/open and teardown are untimed.
//! Payload copying, CRC and publication are timed; checkpoint durability is not.
//! Each size runs with the background prefault thread (ADR 0020) on and off.
//!
//! The `append/batch` group compares one `append` call per record with `append_batch` (ADR 0041) for 64-byte
//! records, with no reader and with a live reader that blocks whenever it has caught up, so that the writer's
//! wake-ups of a waiting reader are part of the timed work.

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;
use tickertape::{LatencyMode, NewRecord, Reader, ReaderConfig, Writer, WriterConfig};

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
        for (size, prefault) in [(64, true), (64, false), (1024, true), (1024, false)] {
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
            group.bench_with_input(
                BenchmarkId::new(if prefault { "prefault" } else { "no-prefault" }, size),
                &record,
                |b, record| {
                    b.iter_batched_ref(
                        || {
                            let dir = tempfile::tempdir().expect("temp journal");
                            let writer = Writer::open(
                                dir.path(),
                                WriterConfig::default().prefault(prefault),
                            )
                            .expect("open writer");
                            (writer, dir)
                        },
                        |(writer, _dir)| {
                            for _ in 0..BATCH {
                                black_box(writer.append(black_box(record)).expect("append"));
                            }
                        },
                        BatchSize::PerIteration,
                    );
                },
            );
        }
        group.finish();
    }
}

/// The same with the writer's latency histograms on (ADR 0047); `append/records/prefault` has them off.
fn latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("append/latency");
    group.sample_size(10);
    group.throughput(Throughput::Elements(BATCH));
    for mode in [LatencyMode::RollsAndCheckpoints, LatencyMode::All] {
        for size in [64, 1024] {
            let payload = vec![0x5a; size];
            let record = NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload: &payload,
            };
            group.bench_with_input(
                BenchmarkId::new(format!("{mode:?}"), size),
                &record,
                |b, record| {
                    b.iter_batched_ref(
                        || {
                            let dir = tempfile::tempdir().expect("temp journal");
                            let config = WriterConfig::default().latency(mode);
                            let writer = Writer::open(dir.path(), config).expect("open writer");
                            (writer, dir)
                        },
                        |(writer, _dir)| {
                            for _ in 0..BATCH {
                                black_box(writer.append(black_box(record)).expect("append"));
                            }
                        },
                        BatchSize::PerIteration,
                    );
                },
            );
        }
    }
    group.finish();
}

/// Records per `append_batch` call in the `append/batch` group.
const CHUNK: usize = 64;

fn batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("append/batch");
    group.sample_size(10);
    group.throughput(Throughput::Elements(BATCH));
    let payload = vec![0x5a; 64];
    let record = NewRecord {
        schema_id: 1,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload: &payload,
    };
    let records = vec![record; CHUNK];
    for reader in [false, true] {
        for batched in [false, true] {
            let name = match (batched, reader) {
                (false, false) => "append/no-reader",
                (true, false) => "append_batch/no-reader",
                (false, true) => "append/blocked-reader",
                (true, true) => "append_batch/blocked-reader",
            };
            group.bench_function(BenchmarkId::new(name, 64), |b| {
                b.iter_batched_ref(
                    || {
                        let dir = tempfile::tempdir().expect("temp journal");
                        let writer =
                            Writer::open(dir.path(), WriterConfig::default()).expect("open writer");
                        let handle = reader.then(|| spawn_reader(dir.path()));
                        (writer, handle, dir)
                    },
                    |(writer, _, _)| {
                        if batched {
                            for chunk in records.chunks(CHUNK).cycle().take(BATCH as usize / CHUNK)
                            {
                                black_box(
                                    writer.append_batch(chunk.iter().copied()).expect("append"),
                                );
                            }
                        } else {
                            for _ in 0..BATCH {
                                black_box(writer.append(black_box(&record)).expect("append"));
                            }
                        }
                    },
                    BatchSize::PerIteration,
                );
            });
        }
    }
    group.finish();
}

/// A live reader that reads every record of a run, blocking (notify on) whenever it has caught up. Returns once it
/// has opened the journal; it exits after the last record or on any error (the journal is removed after a run).
fn spawn_reader(dir: &std::path::Path) -> thread::JoinHandle<()> {
    let dir = dir.to_path_buf();
    let ready = Arc::new(Barrier::new(2));
    let opened = Arc::clone(&ready);
    let handle = thread::spawn(move || {
        let reader = Reader::open(&dir, ReaderConfig::default());
        opened.wait();
        let Ok(mut reader) = reader else { return };
        let mut read = 0;
        while read < BATCH {
            match reader.next_timeout(Some(Duration::from_secs(1))) {
                Ok(Some(_)) => read += 1,
                Ok(None) | Err(_) => return,
            }
        }
    });
    ready.wait();
    handle
}

criterion_group!(benches, append, latency, batch);
criterion_main!(benches);
