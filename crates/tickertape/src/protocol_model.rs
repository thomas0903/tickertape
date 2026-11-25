//! A `loom` model of the publication protocol (ADR 0003), checked by exhaustively exploring thread interleavings.
//!
//! Run with: `RUSTFLAGS="--cfg loom" cargo test -p tickertape --lib --release protocol_model`
//!
//! The model uses loom's atomics and `UnsafeCell` (which reports any data race) in place of the shared mapping:
//! a few slots, each a commit word plus a body. The writer writes a body with ordinary stores, then release-stores
//! the commit word. The reader does what the zero-copy reader does: a `Relaxed` load of the commit word, an `Acquire`
//! fence if it is non-zero, then ordinary reads of the body. Loom fails the test if any interleaving lets the reader
//! read a body concurrently with its writing, or see a committed word without the body it publishes.
//!
//! Since ADR 0038 a background checkpoint stores the checkpoint marker on the writer's checkpoint thread, not on the
//! thread that committed the records it covers; [`marker_stored_by_the_checkpoint_thread_covers_only_committed_records`]
//! models that hand-over.
//!
//! The last models are about a commit word that no writer stored (ADR 0048): a frame it claims is loaded only as far as
//! the checkpoint marker reaches. The variants without the bound, or without the order that the marker gives, must fail.

use loom::cell::UnsafeCell;
use loom::sync::atomic::{AtomicU32, Ordering, fence};
use loom::sync::{Arc, Mutex};
use loom::thread;

const SLOTS: usize = 3;
const SEAL: u32 = u32::MAX;

struct Journal {
    commit: [AtomicU32; SLOTS],
    body: [UnsafeCell<u32>; SLOTS],
}

impl Journal {
    fn new() -> Journal {
        Journal {
            commit: std::array::from_fn(|_| AtomicU32::new(0)),
            body: std::array::from_fn(|_| UnsafeCell::new(0)),
        }
    }

    /// The writer: body first (ordinary store), then the commit word (release).
    fn append(&self, slot: usize, value: u32) {
        // SAFETY: loom checks this access; a race with a reader would fail the model, which is the point.
        self.body[slot].with_mut(|p| unsafe { *p = value });
        self.commit[slot].store(value, Ordering::Release);
    }

    /// The writer's seal: no body to publish beyond the marker in this model.
    fn seal(&self, slot: usize) {
        self.commit[slot].store(SEAL, Ordering::Release);
    }

    /// The reader: relaxed load, acquire fence, then the body. Returns `None` for an empty slot.
    fn read(&self, slot: usize) -> Option<u32> {
        let word = self.commit[slot].load(Ordering::Relaxed);
        if word == 0 {
            return None;
        }
        fence(Ordering::Acquire);
        if word == SEAL {
            return Some(SEAL);
        }
        // SAFETY: as above: loom reports any read that races with the writer's store.
        let body = self.body[slot].with(|p| unsafe { *p });
        assert_eq!(body, word, "reader saw a commit word without its body");
        Some(body)
    }
}

#[test]
fn reader_never_sees_unpublished_bodies() {
    loom::model(|| {
        let j = Arc::new(Journal::new());
        let w = {
            let j = j.clone();
            thread::spawn(move || {
                j.append(0, 11);
                j.append(1, 22);
                j.seal(2);
            })
        };
        // The reader polls each slot in order, as a live reader does, never passing an empty slot.
        let mut slot = 0;
        let mut seen = Vec::new();
        while slot < SLOTS {
            match j.read(slot) {
                Some(SEAL) => break,
                Some(v) => {
                    seen.push(v);
                    slot += 1;
                }
                None => thread::yield_now(),
            }
        }
        assert_eq!(seen, [11, 22]);
        w.join().unwrap();
    });
}

#[test]
fn two_readers_agree() {
    loom::model(|| {
        let j = Arc::new(Journal::new());
        let w = {
            let j = j.clone();
            thread::spawn(move || {
                j.append(0, 7);
                j.append(1, 8);
            })
        };
        let r = {
            let j = j.clone();
            thread::spawn(move || [j.read(0), j.read(1)])
        };
        let mine = [j.read(0), j.read(1)];
        let theirs = r.join().unwrap();
        w.join().unwrap();
        for got in [mine, theirs] {
            // Each reader sees a prefix: slot 1 may be visible without slot 0 only to a reader that skips ahead,
            // which real readers never do; values, when visible, are always the published ones.
            assert!(got[0].is_none_or(|v| v == 7));
            assert!(got[1].is_none_or(|v| v == 8));
        }
    });
}

/// A record whose body is several words with a checksum, validated the way the zero-copy reader does it since
/// ADR 0027: after the commit word and the fence, each body word is read once and fed to the check as it is read,
/// with no copy and no second read. The model reads the words non-atomically (`UnsafeCell`), which is stricter than
/// the reader's `Relaxed` atomic loads: loom fails on any read that races with the writer.
struct WordRecord {
    commit: AtomicU32,
    /// Word 0 is the checksum of the commit word and words 1 and 2.
    words: [UnsafeCell<u64>; 3],
}

fn check(len: u32, a: u64, b: u64) -> u64 {
    (u64::from(len) ^ a.rotate_left(7) ^ b.rotate_left(29)).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

#[test]
fn single_pass_validation_after_the_fence_always_passes() {
    loom::model(|| {
        let r = Arc::new(WordRecord {
            commit: AtomicU32::new(0),
            words: std::array::from_fn(|_| UnsafeCell::new(0)),
        });
        let w = {
            let r = r.clone();
            thread::spawn(move || {
                let (a, b) = (0x1111_2222_3333_4444, 0x5555_6666_7777_8888);
                // SAFETY: loom checks these accesses; a race with the reader would fail the model.
                r.words[1].with_mut(|p| unsafe { *p = a });
                // SAFETY: as above.
                r.words[2].with_mut(|p| unsafe { *p = b });
                // SAFETY: as above.
                r.words[0].with_mut(|p| unsafe { *p = check(24, a, b) });
                r.commit.store(24, Ordering::Release);
            })
        };
        let word = r.commit.load(Ordering::Relaxed);
        if word != 0 {
            fence(Ordering::Acquire);
            // One pass: each word read exactly once, in the order the CRC lanes might use.
            // SAFETY: as above.
            let stored = r.words[0].with(|p| unsafe { *p });
            // SAFETY: as above.
            let a = r.words[1].with(|p| unsafe { *p });
            // SAFETY: as above.
            let b = r.words[2].with(|p| unsafe { *p });
            assert_eq!(
                stored,
                check(word, a, b),
                "a published record failed validation"
            );
        }
        w.join().unwrap();
    });
}

/// ADR 0038: the writer commits records, then hands the frontier (the next slot) to the checkpoint thread through a
/// mutex, as `Writer::request_checkpoint` hands over a job, and keeps appending; the checkpoint thread stores the marker
/// (here an atomic with release; in the journal a `pwrite` that a durable-only reader reads back). A durable-only reader
/// that loads the marker and then reads every slot below it must find each one committed with its body, in every
/// interleaving with the appends that continue meanwhile.
#[test]
fn marker_stored_by_the_checkpoint_thread_covers_only_committed_records() {
    loom::model(|| {
        let j = Arc::new(Journal::new());
        let requested = Arc::new(Mutex::new(None::<u32>));
        let marker = Arc::new(AtomicU32::new(0));
        let writer = {
            let (j, requested) = (j.clone(), requested.clone());
            thread::spawn(move || {
                j.append(0, 11);
                *requested.lock().unwrap() = Some(1);
                j.append(1, 22);
                j.append(2, 33);
            })
        };
        let checkpointer = {
            let (requested, marker) = (requested.clone(), marker.clone());
            thread::spawn(move || {
                loop {
                    if let Some(next) = *requested.lock().unwrap() {
                        marker.store(next, Ordering::Release);
                        break;
                    }
                    thread::yield_now();
                }
            })
        };
        let covered = marker.load(Ordering::Acquire);
        for slot in 0..covered as usize {
            assert!(
                j.read(slot).is_some(),
                "the marker covers slot {slot}, which the reader found empty"
            );
        }
        writer.join().unwrap();
        checkpointer.join().unwrap();
        assert_eq!(marker.load(Ordering::Acquire), 1);
    });
}

/// ADR 0048: the writer commits slots 0 and 1, the commit word of slot 0 is damaged to claim `claimed` slots, the
/// position 2 is handed to the checkpoint thread through a mutex, and the writer appends slot 2. The checkpoint thread
/// stores the marker. That store is a release here: in the journal it is a `pwrite` after `msync` and `fsync`, and that
/// it acts as one is an assumption (ADR 0048 → Ordering) that this model makes and does not test.
///
/// The reader takes its bound from the marker, with a `Relaxed` load and an `Acquire` fence, as `marker_for_limit`
/// does after its `pread`, either before it loads the commit word of slot 0 (a bound it holds), or after (`reached`: the
/// cursor has reached the old bound, and `Reader::slot_past_limit` reads the marker again). Then it reads the claimed
/// frame through the mapping if that ends inside the bound, and nothing of it otherwise (it would be read with `pread`).
/// A visible reader then reads on past the marker by the commit words; a durable-only reader stops there.
#[derive(Clone, Copy)]
struct Reads {
    claimed: usize,
    bounded: bool,
    reached: bool,
    marker_fence: bool,
    durable_only: bool,
}

fn damaged_frame(reads: Reads) {
    loom::model(move || {
        let j = Arc::new(Journal::new());
        let requested = Arc::new(Mutex::new(None::<u32>));
        let marker = Arc::new(AtomicU32::new(0));
        let writer = {
            let (j, requested) = (j.clone(), requested.clone());
            thread::spawn(move || {
                j.append(0, 11);
                j.append(1, 22);
                // The damage. Not a release: nobody who sees this value synchronises with the writer through it.
                j.commit[0].store(100 + reads.claimed as u32, Ordering::Relaxed);
                *requested.lock().unwrap() = Some(2);
                j.append(2, 33);
            })
        };
        let checkpointer = {
            let (requested, marker) = (requested.clone(), marker.clone());
            thread::spawn(move || {
                // (One look: a model in which it waits has many more interleavings of the same accesses.)
                if let Some(next) = *requested.lock().unwrap() {
                    marker.store(next, Ordering::Release);
                }
            })
        };
        let read_word = || {
            let word = j.commit[0].load(Ordering::Relaxed) as usize;
            fence(Ordering::Acquire);
            word
        };
        let read_marker = || {
            let limit = marker.load(Ordering::Relaxed) as usize;
            if reads.marker_fence {
                fence(Ordering::Acquire);
            }
            limit
        };
        let (word, limit) = if reads.reached {
            let word = read_word();
            (word, read_marker())
        } else {
            let limit = read_marker();
            (read_word(), limit)
        };
        // Without a marker the reader is as it was before the bound, and this model is not about it.
        if limit > 0 {
            assert!(reads.reached || word == 100 + reads.claimed);
            if word == 100 + reads.claimed && (!reads.bounded || reads.claimed <= limit) {
                for (slot, value) in [11, 22, 33].into_iter().enumerate().take(reads.claimed) {
                    // SAFETY: as above: loom reports any read that races with the writer's store.
                    let body = j.body[slot].with(|p| unsafe { *p });
                    assert!(body == value || slot >= limit);
                }
            }
            if reads.marker_fence {
                assert_eq!(j.read(1), Some(22));
                if !reads.durable_only {
                    assert!(j.read(2).is_none_or(|v| v == 33));
                }
            }
        }
        writer.join().unwrap();
        checkpointer.join().unwrap();
    });
}

/// The frame claims 3 slots, one past the marker; `a_damaged_frame_inside_the_marker_meets_no_store` claims 2, and covers
/// slot 1, whose commit word the reader has not acquired: there the order comes from the marker.
fn all_readers(claimed: usize) {
    for (reached, durable_only) in [(false, false), (false, true), (true, false), (true, true)] {
        damaged_frame(Reads {
            claimed,
            bounded: true,
            reached,
            marker_fence: true,
            durable_only,
        });
    }
}

#[test]
fn a_damaged_frame_is_not_loaded_past_the_marker() {
    all_readers(3);
}

#[test]
fn a_damaged_frame_inside_the_marker_meets_no_store() {
    all_readers(2);
}

#[test]
#[should_panic(expected = "Causality violation")]
fn without_the_bound_a_damaged_frame_races_with_the_writer() {
    damaged_frame(Reads {
        claimed: 3,
        bounded: false,
        reached: false,
        marker_fence: true,
        durable_only: false,
    });
}

#[test]
#[should_panic(expected = "Causality violation")]
fn without_the_fence_after_the_marker_a_damaged_frame_races_with_the_writer() {
    damaged_frame(Reads {
        claimed: 2,
        bounded: true,
        reached: true,
        marker_fence: false,
        durable_only: false,
    });
}
