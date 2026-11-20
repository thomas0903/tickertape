//! ADR 0041: `Writer::append_batch` (records committed one by one, one wake-up per batch, partial failure).

use std::cell::Cell;
use std::panic::{self, AssertUnwindSafe};
use std::time::{Duration, Instant};

use tickertape::{Backoff, Error, NewRecord, Reader, ReaderConfig, Writer};
use tickertape_tests::{SMALL_SEGMENT, payload, segments, small_config};

fn record(payload: &[u8]) -> NewRecord<'_> {
    NewRecord {
        schema_id: 3,
        venue_ts_ns: 5,
        local_ts_ns: 7,
        payload,
    }
}

/// Reads every committed record from 0 and checks each against `payloads[seq]`.
fn assert_journal(dir: &std::path::Path, payloads: &[Vec<u8>]) {
    let mut reader = Reader::open(dir, ReaderConfig::default()).unwrap();
    for (seq, expected) in payloads.iter().enumerate() {
        let r = reader.try_next().unwrap().expect("record missing");
        assert_eq!(r.seq, seq as u64);
        assert_eq!((r.schema_id, r.venue_ts_ns, r.local_ts_ns), (3, 5, 7));
        assert_eq!(r.payload, expected.as_slice());
    }
    assert!(reader.try_next().unwrap().is_none(), "unexpected record");
}

#[test]
fn a_batch_appends_in_order_and_returns_its_sequence_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    let payloads: Vec<Vec<u8>> = (0..10).map(|seq| payload(seq, 1 + seq as usize)).collect();
    w.append(&record(&payloads[0])).unwrap();
    let seqs = w
        .append_batch(payloads[1..].iter().map(|p| record(p)))
        .unwrap();
    assert_eq!(seqs, 1..10);
    assert_eq!(w.next_seq(), 10);
    // An empty batch appends nothing and returns the empty range at the next sequence number.
    assert_eq!(w.append_batch(std::iter::empty()).unwrap(), 10..10);
    assert_journal(dir.path(), &payloads);
}

#[test]
fn a_batch_rolls_inside_itself() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    // Three times the segment size in one batch: at least two rolls happen inside it.
    let payloads: Vec<Vec<u8>> = (0..3 * SMALL_SEGMENT / 1000)
        .map(|seq| payload(seq, 1000))
        .collect();
    let seqs = w.append_batch(payloads.iter().map(|p| record(p))).unwrap();
    assert_eq!(seqs, 0..payloads.len() as u64);
    assert!(segments(dir.path()).len() >= 3, "expected rolls");
    assert_journal(dir.path(), &payloads);
    // The journal is consistent after reopening, too.
    drop(w);
    let w = Writer::open(dir.path(), small_config()).unwrap();
    assert_eq!(w.next_seq(), payloads.len() as u64);
}

#[test]
fn a_failure_in_the_middle_keeps_the_records_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    w.append(&record(b"before")).unwrap();
    let too_large = vec![0u8; SMALL_SEGMENT as usize];
    let batch = [b"a".to_vec(), b"b".to_vec(), too_large, b"never".to_vec()];
    let taken = Cell::new(0);
    let err = w
        .append_batch(batch.iter().map(|p| {
            taken.set(taken.get() + 1);
            record(p)
        }))
        .unwrap_err();
    assert_eq!(err.appended, 2);
    assert_eq!(err.first_seq, 1);
    assert!(matches!(err.error, Error::RecordTooLarge { .. }), "{err:?}");
    assert_eq!(
        taken.get(),
        3,
        "the iterator is not advanced past the failing record"
    );
    assert_eq!(w.next_seq(), 3);
    // Retrying from the failing record's successor continues without a gap.
    let seqs = w
        .append_batch(batch[3..].iter().map(|p| record(p)))
        .unwrap();
    assert_eq!(seqs, 3..4);
    let expected = [
        b"before".to_vec(),
        b"a".to_vec(),
        b"b".to_vec(),
        b"never".to_vec(),
    ];
    assert_journal(dir.path(), &expected);
}

/// A reader whose waits go straight to blocking on the wake-up region, with a 10 s poll interval. A blocked reader
/// re-reads the `notify` record at least every 100 ms (ADR 0021), so a missing wake-up shows up as a delivery that
/// takes about that long.
fn blocking_reader(dir: &std::path::Path) -> Reader {
    let mut b = Backoff::default();
    b.spins = 0;
    b.yields = 0;
    b.min_sleep = Duration::from_secs(10);
    b.max_sleep = Duration::from_secs(10);
    Reader::open(dir, ReaderConfig::default().backoff(b)).unwrap()
}

/// Runs `rounds` rounds of: a reader thread blocks for `count` records from the writer's next sequence number, then
/// `append` appends them. Checks every round's sequence numbers and returns how many rounds took 90 ms or more from
/// the start of `append` to the reader's last record.
fn slow_rounds(
    w: &mut Writer,
    rounds: u64,
    count: u64,
    mut append: impl FnMut(&mut Writer, u64),
) -> usize {
    let mut slow = 0;
    for round in 0..rounds {
        let first = w.next_seq();
        let mut reader = blocking_reader(w.dir());
        reader.seek(first).unwrap();
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..count {
                let r = reader
                    .next_timeout(Some(Duration::from_secs(30)))
                    .unwrap()
                    .expect("record never arrived");
                seen.push(r.seq);
            }
            (seen, Instant::now())
        });
        // Give the reader time to block on the wake-up region.
        std::thread::sleep(Duration::from_millis(5));
        let started = Instant::now();
        append(w, round);
        let (seen, done) = handle.join().unwrap();
        assert_eq!(seen, (first..first + count).collect::<Vec<_>>());
        if done.saturating_duration_since(started) >= Duration::from_millis(90) {
            slow += 1;
        }
    }
    slow
}

#[test]
fn a_batch_wakes_a_blocked_reader() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    let slow = slow_rounds(&mut w, 20, 5, |w, round| {
        let payloads: Vec<Vec<u8>> = (0..5).map(|i| payload(round * 5 + i, 16)).collect();
        w.append_batch(payloads.iter().map(|p| record(p))).unwrap();
    });
    // Allow two scheduling hiccups on a loaded CI machine; without the batch's wake-up every round is slow.
    assert!(slow <= 2, "{slow} of 20 batches took >= 90 ms to arrive");
}

#[test]
fn a_panicking_iterator_still_wakes_readers_for_the_records_it_appended() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    let slow = slow_rounds(&mut w, 10, 2, |w, _| {
        let payloads = [b"one".to_vec(), b"two".to_vec()];
        let unwound = panic::catch_unwind(AssertUnwindSafe(|| {
            let records = payloads
                .iter()
                .map(|p| record(p))
                .chain(std::iter::once_with(|| {
                    panic!("the caller's iterator fails (expected by this test)")
                }));
            let _ = w.append_batch(records);
        }));
        assert!(unwound.is_err());
    });
    assert!(slow <= 2, "{slow} of 10 batches took >= 90 ms to arrive");
    // The writer goes on: the panic came from the caller's iterator, not from an append.
    assert_eq!(w.append(&record(b"three")).unwrap(), 20);
}
