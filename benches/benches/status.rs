//! Cost of operational status calls (ADR 0032), and of a reader loop that publishes a beacon, against the same loop
//! without one. Exploratory: no numbers from this bench are recorded in the docs.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::path::Path;
use std::time::Duration;
use tickertape::status::{self, ReaderBeacon, Sampler};
use tickertape::{NewRecord, Reader, ReaderConfig, Writer, WriterConfig};

const RECORDS: u64 = 100_000;

fn fill(writer: &mut Writer, count: u64, payload: &[u8]) {
    for _ in 0..count {
        writer
            .append(&NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: tickertape::clock::realtime_ns(),
                payload,
            })
            .expect("append");
    }
}

/// Reads every record; with a beacon, calls `update` after each one (the intended usage).
fn replay(dir: &Path, beacon: Option<&str>) {
    let mut reader = Reader::open(dir, ReaderConfig::default()).expect("open reader");
    let mut beacon = beacon.map(|name| ReaderBeacon::create(dir, name, &reader).expect("beacon"));
    for seq in 0..RECORDS {
        let record = reader.try_next().expect("read").expect("record");
        assert_eq!(record.seq, seq);
        black_box(record.payload);
        if let Some(beacon) = &mut beacon {
            beacon.update(&reader).expect("update");
        }
    }
}

fn status_costs(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("temp journal");
    let mut writer = Writer::open(dir.path(), WriterConfig::default()).expect("open writer");
    let payload = vec![0x5a; 64];
    fill(&mut writer, RECORDS, &payload);
    writer.checkpoint().expect("checkpoint");
    // Some uncovered records, as in a running recorder between checkpoints.
    fill(&mut writer, 10_000, &payload);
    let reader = Reader::open(dir.path(), ReaderConfig::default()).expect("open reader");
    let mut group = c.benchmark_group("status/call");
    let mut sampler = Sampler::new(dir.path());
    sampler.sample().expect("first sample");
    group.bench_function("Sampler::sample (steady state, no beacons)", |b| {
        b.iter(|| black_box(sampler.sample().expect("sample")));
    });
    group.finish();
    let _beacons: Vec<_> = (0..4)
        .map(|i| ReaderBeacon::create(dir.path(), &format!("reader-{i}"), &reader).expect("beacon"))
        .collect();

    let mut group = c.benchmark_group("status/call");
    group.bench_function("status (10k uncovered records, 4 beacons)", |b| {
        b.iter(|| black_box(status::status(dir.path()).expect("status")));
    });
    let mut sampler = Sampler::new(dir.path());
    sampler.sample().expect("first sample");
    group.bench_function("Sampler::sample (steady state, 4 beacons)", |b| {
        b.iter(|| black_box(sampler.sample().expect("sample")));
    });
    group.bench_function("Writer::metrics", |b| {
        b.iter(|| black_box(writer.metrics()))
    });
    group.bench_function("Reader::cursor", |b| b.iter(|| black_box(reader.cursor())));
    let snapshot = status::status(dir.path()).expect("status");
    group.bench_function("Status::lag", |b| {
        b.iter(|| black_box(snapshot.lag(black_box(reader.cursor()))));
    });
    let mut beacon = ReaderBeacon::create(dir.path(), "bench", &reader).expect("beacon");
    group.bench_function("ReaderBeacon::update (position unchanged)", |b| {
        b.iter(|| black_box(beacon.update(&reader).expect("update")));
    });
    beacon.set_interval(Duration::ZERO);
    let mut seq = 0u64;
    group.bench_function("ReaderBeacon::update_position (writes)", |b| {
        b.iter(|| {
            seq += 1;
            let position = status::Position::new(seq, 0, 64);
            black_box(beacon.update_position(position).expect("update"))
        });
    });
    group.finish();
    drop(beacon);

    let mut group = c.benchmark_group("status/replay_with_beacon");
    group.sample_size(10);
    group.throughput(Throughput::Elements(RECORDS));
    for (name, beacon) in [
        ("no beacon", None),
        ("beacon update per record", Some("replay")),
    ] {
        group.bench_function(BenchmarkId::new("safe", name), |b| {
            replay(dir.path(), beacon);
            b.iter(|| replay(dir.path(), beacon));
        });
    }
    group.finish();
}

criterion_group!(benches, status_costs);
criterion_main!(benches);
