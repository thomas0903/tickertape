//! A `loom` model of the wake-up region's two event counts (ADRs 0017 and 0021, and the marker words).
//!
//! Run with: `RUSTFLAGS="--cfg loom" cargo test -p tickertape --lib --release notify_model`
//!
//! A futex is modelled by a mutex and a condition variable: the wait compares the word and sleeps in one step, the
//! wake takes the mutex. Nothing times out, so a lost wake-up is a deadlock, which loom reports. Loom treats a
//! `SeqCst` access as `AcqRel`, so the model puts a `SeqCst` fence where the code relies on the single order of its
//! `SeqCst` accesses (between the increment and the load that follows it, on both sides); the variant without fences
//! must fail. The marker is a relaxed word: the file it stands for is ordered by the event count only.

use loom::sync::atomic::{AtomicU32, Ordering, fence};
use loom::sync::{Arc, Condvar, Mutex};
use loom::thread;

struct Count {
    event: AtomicU32,
    waiters: AtomicU32,
    queue: Mutex<()>,
    woken: Condvar,
}

impl Count {
    fn new() -> Count {
        Count {
            event: AtomicU32::new(0),
            waiters: AtomicU32::new(0),
            queue: Mutex::new(()),
            woken: Condvar::new(),
        }
    }

    fn publish(&self, fenced: bool) {
        self.event.fetch_add(1, Ordering::SeqCst);
        if fenced {
            fence(Ordering::SeqCst);
        }
        if self.waiters.load(Ordering::SeqCst) != 0 {
            let _queue = self.queue.lock().unwrap();
            self.woken.notify_all();
        }
    }

    fn begin_wait(&self, fenced: bool) -> u32 {
        self.waiters.fetch_add(1, Ordering::SeqCst);
        if fenced {
            fence(Ordering::SeqCst);
        }
        self.event.load(Ordering::SeqCst)
    }

    fn wait(&self, seen: u32) {
        let queue = self.queue.lock().unwrap();
        if self.event.load(Ordering::SeqCst) == seen {
            drop(self.woken.wait(queue).unwrap());
        }
    }

    fn end_wait(&self) {
        self.waiters.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Region {
    record: Count,
    marker: Count,
    committed: AtomicU32,
    covered: AtomicU32,
}

impl Region {
    fn new() -> Arc<Region> {
        Arc::new(Region {
            record: Count::new(),
            marker: Count::new(),
            committed: AtomicU32::new(0),
            covered: AtomicU32::new(0),
        })
    }

    /// The checkpoint thread: the marker, then both counts.
    fn checkpoint(&self, covered: u32, fenced: bool) {
        self.covered.store(covered, Ordering::Relaxed);
        self.marker.publish(fenced);
        self.record.publish(fenced);
    }

    fn append(&self, seq: u32, fenced: bool) {
        self.committed.store(seq, Ordering::Release);
        self.record.publish(fenced);
    }
}

/// `Reader::block_on` in a loop, waiting for `value` in `what`.
fn wait_until(count: &Count, what: &AtomicU32, value: u32, fenced: bool) {
    while what.load(Ordering::Relaxed) < value {
        let seen = count.begin_wait(fenced);
        if what.load(Ordering::Relaxed) < value {
            count.wait(seen);
        }
        count.end_wait();
    }
}

fn model(preemptions: usize, f: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(preemptions);
    builder.check(f);
}

/// A durable-only reader on the marker words is woken by the checkpoint that covers its record, with an append
/// racing. The appends never see it as a waiter.
fn durable_reader(fenced: bool) {
    model(3, move || {
        let r = Region::new();
        let writer = {
            let r = r.clone();
            thread::spawn(move || {
                r.append(1, fenced);
                assert_eq!(r.record.waiters.load(Ordering::SeqCst), 0);
            })
        };
        let checkpoint = {
            let r = r.clone();
            thread::spawn(move || r.checkpoint(1, fenced))
        };
        wait_until(&r.marker, &r.covered, 1, fenced);
        writer.join().unwrap();
        checkpoint.join().unwrap();
    });
}

#[test]
fn a_checkpoint_wakes_a_durable_only_reader_on_the_marker_words() {
    durable_reader(true);
}

#[test]
#[should_panic(expected = "deadlock")]
fn without_the_total_order_the_wake_up_is_lost() {
    durable_reader(false);
}

/// 1.0.0's durable-only reader waits on the record words: the checkpoint wakes it too.
#[test]
fn a_checkpoint_wakes_a_reader_on_the_record_words() {
    model(3, || {
        let r = Region::new();
        let checkpoint = {
            let r = r.clone();
            thread::spawn(move || r.checkpoint(1, true))
        };
        wait_until(&r.record, &r.covered, 1, true);
        checkpoint.join().unwrap();
    });
}

/// A visible reader and a durable-only reader wait at the same time; the append wakes the first, the checkpoint the
/// second.
#[test]
fn a_visible_and_a_durable_only_reader_each_get_their_wake_up() {
    model(2, || {
        let r = Region::new();
        let visible = {
            let r = r.clone();
            thread::spawn(move || wait_until(&r.record, &r.committed, 1, true))
        };
        let durable = {
            let r = r.clone();
            thread::spawn(move || wait_until(&r.marker, &r.covered, 1, true))
        };
        r.append(1, true);
        r.checkpoint(1, true);
        visible.join().unwrap();
        durable.join().unwrap();
    });
}

/// The bug to avoid: a reader that sleeps on the marker words of a writer that does not bump them is never woken
/// (the capability word says which writers do).
#[test]
#[should_panic(expected = "deadlock")]
fn a_writer_that_does_not_bump_the_marker_words_never_wakes_their_waiter() {
    model(2, || {
        let r = Region::new();
        let old_writer = {
            let r = r.clone();
            thread::spawn(move || {
                r.covered.store(1, Ordering::Relaxed);
                r.record.publish(true);
            })
        };
        wait_until(&r.marker, &r.covered, 1, true);
        old_writer.join().unwrap();
    });
}
