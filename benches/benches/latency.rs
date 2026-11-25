//! Cost of `tickertape::latency` (ADR 0047): one histogram update, one record-age observation and its clock read, and a
//! snapshot. Exploratory: no numbers from this bench are recorded in the docs.

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::time::Duration;
use tickertape::latency::{RecordAge, Recorder};
use tickertape::{Record, clock};

/// Values spread over 64 ns to 64 µs, so that updates touch many buckets.
fn values() -> Vec<u64> {
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    (0..4096)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let shift = x % 10;
            (64 << shift) + (x >> 32) % (64 << shift)
        })
        .collect()
}

fn latency(c: &mut Criterion) {
    let values = values();
    let mut group = c.benchmark_group("latency");
    let mut i = 0usize;
    // The loop and the value's load alone: subtract it from the next two.
    group.bench_function("value only", |b| {
        b.iter(|| {
            i = (i + 1) % 4096;
            black_box(values[i]);
        });
    });
    let mut recorder = Recorder::new();
    group.bench_function("Recorder::record_ns", |b| {
        b.iter(|| {
            i = (i + 1) % 4096;
            recorder.record_ns(black_box(values[i]));
        });
    });
    let mut age = RecordAge::new();
    age.set_live(true);
    group.bench_function("RecordAge::observe_at", |b| {
        b.iter(|| {
            i = (i + 1) % 4096;
            black_box(age.observe_at(1_000, black_box(1_000 + values[i])));
        });
    });
    group.bench_function("clock::realtime_ns", |b| {
        b.iter(|| black_box(clock::realtime_ns()));
    });
    let record = Record {
        seq: 0,
        schema_id: 1,
        venue_ts_ns: 0,
        local_ts_ns: clock::realtime_ns(),
        payload: b"",
    };
    group.bench_function("RecordAge::observe", |b| {
        b.iter(|| black_box(age.observe(black_box(&record))));
    });
    let view = recorder.view();
    group.bench_function("View::snapshot", |b| {
        b.iter_batched(
            || (),
            |()| black_box(view.snapshot()),
            BatchSize::SmallInput,
        );
    });
    group.finish();

    // With another thread taking snapshots without pause: the disturbance at its worst.
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                black_box(view.snapshot());
            }
        });
        let mut group = c.benchmark_group("latency/while snapshots are taken without pause");
        group.measurement_time(Duration::from_secs(3));
        group.bench_function("Recorder::record_ns", |b| {
            b.iter(|| {
                i = (i + 1) % 4096;
                recorder.record_ns(black_box(values[i]));
            });
        });
        group.finish();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    });
}

criterion_group!(benches, latency);
criterion_main!(benches);
