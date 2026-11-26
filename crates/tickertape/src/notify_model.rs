//! A `loom` model of the wake-up region's two event counts (ADRs 0017, 0021 and 0053), checked by exhaustively
//! exploring thread interleavings.
//!
//! Run with: `RUSTFLAGS="--cfg loom" cargo test -p tickertape --lib --release notify_model`
//!
//! A futex is modelled by a mutex and a condition variable: the wait compares the word and sleeps in one step, the
//! wake takes the mutex. Nothing times out, so a lost wake-up is a deadlock, which loom reports. Loom treats a
//! `SeqCst` access as `AcqRel`, so the model puts a `SeqCst` fence where the code relies on the single order of its
//! `SeqCst` accesses (between an increment and the load that follows it, on both sides); the variant without the
//! fences must fail.
//!
//! The reader is `Reader::block_on` for a durable-only reader: it chooses the marker words at the checkpoint boundary
//! it knows if the writer has the capability, sleeps without registering if the writer lacks it (a timed sleep, which
//! the model writes as a yield), and the record words when it is behind its known boundary. Its look reads the marker
//! (`covered`, which a checkpoint can also set back), then the committed records. The marker is written after the
//! records it covers and read before them, which the model writes as a release store and an acquire load; the stale
//! choice is also checked with a relaxed load, weaker than the real file reads, so that a look can see a marker
//! without the records it covers and must choose again.
//!
//! This is a model of `Reader::block_on`'s steps, not the function: the real sequence is checked by `reader.rs`'s
//! `wake_tests`, which stop a reader between its choice and its registration and change the journal there.

use loom::sync::atomic::{AtomicU32, Ordering, fence};
use loom::sync::{Arc, Condvar, Mutex};
use loom::thread;
use std::cell::Cell;
use std::thread::LocalKey;

// Which paths of `block_on` some interleaving took, outside the model, so that each test can check that it reached the
// path it is about. Loom runs a model's threads on the test's own thread, so these are per test.
std::thread_local! {
    static CHOSE_AGAIN: Cell<bool> = const { Cell::new(false) };
    static WAITED_ON_RECORD_AT_BOUNDARY: Cell<bool> = const { Cell::new(false) };
    static SLEPT_UNREGISTERED: Cell<bool> = const { Cell::new(false) };
    static WAITED_ON_MARKER: Cell<bool> = const { Cell::new(false) };
}

fn reached(path: &'static LocalKey<Cell<bool>>) -> bool {
    path.with(Cell::take)
}

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

    /// The writer's step: increment, then wake if anyone is registered. Returns whether it saw a waiter.
    fn publish(&self, fenced: bool) -> bool {
        self.event.fetch_add(1, Ordering::SeqCst);
        if fenced {
            fence(Ordering::SeqCst);
        }
        let waiting = self.waiters.load(Ordering::SeqCst) != 0;
        if waiting {
            let _queue = self.queue.lock().unwrap();
            self.woken.notify_all();
        }
        waiting
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
    /// Bit 0: the writer bumps the marker words.
    capabilities: AtomicU32,
    writer_flag: AtomicU32,
    committed: AtomicU32,
    covered: AtomicU32,
}

impl Region {
    fn new(capabilities: u32, committed: u32, covered: u32) -> Arc<Region> {
        Arc::new(Region {
            record: Count::new(),
            marker: Count::new(),
            capabilities: AtomicU32::new(capabilities),
            writer_flag: AtomicU32::new(1),
            committed: AtomicU32::new(committed),
            covered: AtomicU32::new(covered),
        })
    }

    /// `append`: commit, then bump the record words. Returns whether a reader was registered there.
    fn append(&self, seq: u32) -> bool {
        self.committed.store(seq, Ordering::Release);
        self.record.publish(true)
    }

    /// The checkpoint thread: the marker, then the marker words if the writer has them (a new writer), then the
    /// record words (for 1.0.0's durable-only readers). Returns whether a reader was registered on the record words.
    fn checkpoint(&self, covered: u32, new_writer: bool, fenced: bool) -> bool {
        self.covered.store(covered, Ordering::Release);
        if new_writer {
            self.marker.publish(fenced);
        }
        self.record.publish(fenced)
    }

    /// A new writer's drop: clear the flag, then bump both.
    fn drop_writer(&self) {
        self.writer_flag.store(0, Ordering::SeqCst);
        self.marker.publish(true);
        self.record.publish(true);
    }
}

#[derive(Debug, PartialEq)]
enum Step {
    Found,
    Again,
    Gone,
}

/// A durable-only reader: its cursor and the marker it knows.
struct Reader {
    next: u32,
    known: u32,
    /// Ignore the capability bit and always use the marker words at the boundary (the bug the bit prevents).
    ignore_capability: bool,
    fenced: bool,
    /// Read the marker relaxed instead of acquire (weaker than the real reads; see the module documentation).
    relaxed_marker: bool,
}

impl Reader {
    fn at_boundary(&self) -> bool {
        self.next >= self.known
    }

    /// `advance`: read the marker, then the next record if the marker covers it and it is there.
    fn advance(&mut self, r: &Region) -> bool {
        let order = if self.relaxed_marker {
            Ordering::Relaxed
        } else {
            Ordering::Acquire
        };
        self.known = r.covered.load(order);
        if self.next < self.known && self.next < r.committed.load(Ordering::Relaxed) {
            self.next += 1;
            return true;
        }
        false
    }

    /// `Reader::block_on`.
    fn block_on(&mut self, r: &Region) -> Step {
        let capable = self.ignore_capability || r.capabilities.load(Ordering::SeqCst) & 1 != 0;
        let count = match (self.at_boundary(), capable) {
            (true, true) => &r.marker,
            (true, false) => {
                // Look once more, then a timed sleep without registering.
                if self.advance(r) {
                    return Step::Found;
                }
                SLEPT_UNREGISTERED.with(|c| c.set(true));
                thread::yield_now();
                return Step::Again;
            }
            (false, _) => &r.record,
        };
        let on_marker = std::ptr::eq(count, &r.marker);
        let seen = count.begin_wait(self.fenced);
        if self.advance(r) {
            count.end_wait();
            return Step::Found;
        }
        if on_marker && !self.at_boundary() {
            // The look moved the boundary (a new marker) without finding the record yet: choose again.
            CHOSE_AGAIN.with(|c| c.set(true));
            count.end_wait();
            return Step::Again;
        }
        if r.writer_flag.load(Ordering::SeqCst) == 0 {
            // The real wait is then capped at `max_sleep`; the model stops here.
            count.end_wait();
            return Step::Gone;
        }
        if on_marker {
            WAITED_ON_MARKER.with(|c| c.set(true));
        }
        if !on_marker && self.at_boundary() {
            WAITED_ON_RECORD_AT_BOUNDARY.with(|c| c.set(true));
        }
        count.wait(seen);
        count.end_wait();
        Step::Again
    }

    /// `next_timeout` in its blocking phase until the cursor reaches `target` or the writer is gone.
    fn follow_to(&mut self, r: &Region, target: u32) -> Step {
        while self.next < target {
            if self.block_on(r) == Step::Gone {
                return Step::Gone;
            }
        }
        Step::Found
    }
}

fn reader(next: u32, known: u32) -> Reader {
    Reader {
        next,
        known,
        ignore_capability: false,
        fenced: true,
        relaxed_marker: false,
    }
}

fn model(preemptions: usize, f: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(preemptions);
    builder.check(f);
}

/// A checkpoint wakes a durable-only reader that waits on the marker words, and the appends never find it registered
/// on the record words (no wake-up call for it).
fn appends_never_see_a_durable_only_reader(fenced: bool) {
    model(3, move || {
        let r = Region::new(1, 0, 0);
        let writer = {
            let r = r.clone();
            thread::spawn(move || {
                assert!(
                    !r.append(1),
                    "an append found the durable-only reader registered"
                );
                assert!(!r.checkpoint(1, true, fenced));
                assert!(
                    !r.append(2),
                    "an append found the durable-only reader registered"
                );
            })
        };
        let mut me = reader(0, 0);
        me.fenced = fenced;
        assert_eq!(me.follow_to(&r, 1), Step::Found);
        writer.join().unwrap();
    });
    assert!(reached(&WAITED_ON_MARKER));
}

#[test]
fn a_checkpoint_wakes_a_durable_only_reader_on_the_marker_words() {
    appends_never_see_a_durable_only_reader(true);
}

#[test]
#[should_panic(expected = "deadlock")]
fn without_the_total_order_the_wake_up_is_lost() {
    appends_never_see_a_durable_only_reader(false);
}

/// A stale choice: the reader picks the marker words from the boundary it knows while the marker has already moved,
/// and its look can see the new marker before the record (relaxed here). It must choose again, not sleep.
#[test]
fn a_stale_choice_of_the_marker_words_chooses_again() {
    model(3, || {
        let r = Region::new(1, 1, 1);
        let writer = {
            let r = r.clone();
            thread::spawn(move || {
                r.append(2);
                r.checkpoint(2, true, true);
            })
        };
        let mut me = reader(1, 1);
        me.relaxed_marker = true;
        assert_eq!(me.follow_to(&r, 2), Step::Found);
        writer.join().unwrap();
    });
    assert!(reached(&CHOSE_AGAIN));
}

/// Behind the boundary it knows (a marker set back since it was read): the reader picks the record words, and its look
/// puts it at the boundary. It waits on the record words, which the next append or checkpoint bumps.
#[test]
fn a_reader_behind_its_boundary_that_reaches_it_is_still_woken() {
    model(3, || {
        let r = Region::new(1, 1, 1);
        let writer = {
            let r = r.clone();
            thread::spawn(move || {
                r.append(2);
                r.checkpoint(2, true, true);
            })
        };
        assert_eq!(reader(1, 5).follow_to(&r, 2), Step::Found);
        writer.join().unwrap();
    });
    assert!(reached(&WAITED_ON_RECORD_AT_BOUNDARY));
}

/// The writer goes away while a durable-only reader waits on the marker words: it is woken and sees the flag.
#[test]
fn a_writer_that_goes_away_wakes_a_reader_on_the_marker_words() {
    model(3, || {
        let r = Region::new(1, 1, 1);
        let writer = {
            let r = r.clone();
            thread::spawn(move || r.drop_writer())
        };
        assert_eq!(reader(1, 1).follow_to(&r, 2), Step::Gone);
        writer.join().unwrap();
    });
}

/// A writer without the capability (1.0.0): the reader never registers, so that writer makes no wake-up call for it,
/// and it still gets the record once the marker covers it.
#[test]
fn with_a_writer_without_the_capability_the_reader_sleeps_unregistered() {
    model(3, || {
        let r = Region::new(0, 0, 0);
        let writer = {
            let r = r.clone();
            thread::spawn(move || {
                assert!(!r.append(1));
                assert!(!r.checkpoint(1, false, true));
                assert!(!r.append(2));
            })
        };
        assert_eq!(reader(0, 0).follow_to(&r, 1), Step::Found);
        writer.join().unwrap();
    });
    assert!(reached(&SLEPT_UNREGISTERED));
}

/// The bug the capability bit prevents: a reader on the marker words of a writer that does not bump them is never
/// woken.
#[test]
#[should_panic(expected = "deadlock")]
fn a_reader_on_the_marker_words_of_a_writer_without_them_is_never_woken() {
    model(2, || {
        let r = Region::new(0, 1, 0);
        let writer = {
            let r = r.clone();
            thread::spawn(move || {
                r.checkpoint(1, false, true);
            })
        };
        let mut me = reader(0, 0);
        me.ignore_capability = true;
        me.follow_to(&r, 1);
        writer.join().unwrap();
    });
}

/// A visible reader (on the record words) and a durable-only one (on the marker words) wait at the same time; the
/// append wakes the first, the checkpoint the second.
#[test]
fn a_visible_and_a_durable_only_reader_each_get_their_wake_up() {
    model(2, || {
        let r = Region::new(1, 0, 0);
        let visible = {
            let r = r.clone();
            thread::spawn(move || {
                while r.committed.load(Ordering::Acquire) < 1 {
                    let seen = r.record.begin_wait(true);
                    if r.committed.load(Ordering::Acquire) < 1 {
                        r.record.wait(seen);
                    }
                    r.record.end_wait();
                }
            })
        };
        let durable = {
            let r = r.clone();
            thread::spawn(move || assert_eq!(reader(0, 0).follow_to(&r, 1), Step::Found))
        };
        r.append(1);
        r.checkpoint(1, true, true);
        visible.join().unwrap();
        durable.join().unwrap();
    });
    assert!(reached(&WAITED_ON_MARKER));
}
