//! `latency::RecordAge` around real readers (ADR 0047): the loop its documentation shows.

use std::time::Duration;

use tickertape::clock::realtime_ns;
use tickertape::latency::RecordAge;
use tickertape::{NewRecord, Reader, ReaderConfig, Writer};
use tickertape_tests::small_config;

fn append(writer: &mut Writer, local_ts_ns: u64) {
    writer
        .append(&NewRecord {
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns,
            payload: b"age",
        })
        .unwrap();
}

/// Reads what there is, as the documented loop does, until a wait of `idle` finds nothing.
fn follow(reader: &mut Reader, age: &mut RecordAge, idle: Duration) {
    loop {
        let record = match reader.try_next().unwrap() {
            Some(record) => Some(record),
            None => {
                age.set_live(true);
                reader.next_timeout(Some(idle)).unwrap()
            }
        };
        match record {
            Some(record) => age.observe(&record),
            None => return,
        };
    }
}

#[test]
fn only_records_returned_while_live_count() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    for _ in 0..10 {
        append(&mut writer, realtime_ns());
    }
    let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let mut age = RecordAge::new();
    let view = age.view();
    // A tape to which nothing is appended: nothing counts.
    follow(&mut reader, &mut age, Duration::from_millis(1));
    let s = view.snapshot();
    assert_eq!((s.age.count(), s.before_live), (0, 10));
    // A backlog that builds up while the reader is live counts, with its lateness.
    for _ in 0..5 {
        append(&mut writer, realtime_ns());
    }
    std::thread::sleep(Duration::from_millis(50));
    follow(&mut reader, &mut age, Duration::from_millis(1));
    let s = view.snapshot();
    assert_eq!((s.age.count(), s.before_live), (5, 10));
    assert!(s.age.min() >= Some(Duration::from_millis(50)), "{s:?}");
    // Replayed records, declared as the documentation asks.
    reader.seek(3).unwrap();
    age.set_live(false);
    follow(&mut reader, &mut age, Duration::from_millis(1));
    let s = view.snapshot();
    assert_eq!((s.age.count(), s.after_discontinuity), (5, 12));
    append(&mut writer, 0);
    append(&mut writer, realtime_ns() + 3_600_000_000_000);
    append(&mut writer, realtime_ns());
    follow(&mut reader, &mut age, Duration::from_millis(1));
    let s = view.snapshot();
    assert_eq!((s.age.count(), s.unset, s.ahead), (6, 1, 1));
    assert_eq!((s.before_live, s.after_discontinuity), (10, 12));
}

#[test]
fn durable_only_ages_span_the_wait_for_the_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    let mut reader = Reader::open(dir.path(), ReaderConfig::default().durable_only(true)).unwrap();
    let mut age = RecordAge::new();
    follow(&mut reader, &mut age, Duration::from_millis(1));
    append(&mut writer, realtime_ns());
    std::thread::sleep(Duration::from_millis(100));
    follow(&mut reader, &mut age, Duration::from_millis(1));
    assert_eq!(age.view().snapshot().age.count(), 0);
    writer.checkpoint().unwrap();
    follow(&mut reader, &mut age, Duration::from_millis(1));
    let s = age.view().snapshot();
    assert_eq!(s.age.count(), 1);
    assert!(s.age.min() >= Some(Duration::from_millis(100)), "{s:?}");
}
