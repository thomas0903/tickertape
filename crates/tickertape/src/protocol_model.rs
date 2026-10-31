//! A `loom` model of the publication protocol (ADR 0003), checked by exhaustively exploring thread interleavings.
//!
//! Run with: `RUSTFLAGS="--cfg loom" cargo test -p tickertape --lib --release protocol_model`
//!
//! The model uses loom's atomics and `UnsafeCell` (which reports any data race) in place of the shared mapping:
//! a few slots, each a commit word plus a body. The writer writes a body with ordinary stores, then release-stores
//! the commit word. The reader does what the zero-copy reader does: a `Relaxed` load of the commit word, an `Acquire`
//! fence if it is non-zero, then ordinary reads of the body. Loom fails the test if any interleaving lets the reader
//! read a body concurrently with its writing, or see a committed word without the body it publishes.

use loom::cell::UnsafeCell;
use loom::sync::Arc;
use loom::sync::atomic::{AtomicU32, Ordering, fence};
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
