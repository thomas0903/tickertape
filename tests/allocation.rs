//! Heap allocations on the steady paths (roadmap: "Finish eliminating avoidable work"; `docs/benchmarks.md` → Where
//! the CPU time goes): appending, following and replaying a journal allocate nothing per record. The counts come from
//! this test binary's global allocator, per thread, so the writer's helper threads are not counted. Rolls, checkpoints
//! and opening do allocate; these tests stay inside one segment and make none of them.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::time::Duration;

use tickertape::{Backoff, NewRecord, Reader, ReaderConfig, Writer, WriterConfig};

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

fn count() {
    // `try_with`: a thread that is being torn down still allocates; that isn't measured.
    let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
}

/// The system allocator, counting the allocations (and reallocations) each thread makes.
struct Counting;

// SAFETY: every call is forwarded unchanged to `System`, which upholds `GlobalAlloc`'s contract; the bookkeeping only
// touches a const-initialised thread-local `Cell<u64>` (no allocation, no destructor).
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller's `layout` is passed on as it is (non-zero size, as `GlobalAlloc::alloc` requires).
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: as for `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller guarantees `ptr` came from this allocator (so from `System`) with `layout`.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: the caller guarantees `ptr` came from this allocator with `layout` and that `new_size` is valid.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The allocations `f` makes on this thread.
fn allocations(f: impl FnOnce()) -> u64 {
    let before = ALLOCATIONS.with(Cell::get);
    f();
    ALLOCATIONS.with(Cell::get) - before
}

const RECORDS: u64 = 2_000;

fn record(payload: &[u8]) -> NewRecord<'_> {
    NewRecord {
        schema_id: 1,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload,
    }
}

/// A fresh journal in the default 128 MiB segments, which no test here fills (nor half fills: on macOS the writer then
/// starts a thread that prepares the next segment, ADR 0022), with two records: a short one, and one long enough for
/// the CRC's tables, which the first long record of the process builds in one allocation (ADR 0024).
fn journal() -> (tempfile::TempDir, Writer) {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), WriterConfig::default()).unwrap();
    writer.append(&record(&[0; 64])).unwrap();
    writer.append(&record(&[0; 1024])).unwrap();
    (dir, writer)
}

#[test]
fn counting_sees_an_allocation() {
    assert_eq!(allocations(|| drop(black_box(vec![0u8; 100]))), 1);
}

#[test]
fn appending_allocates_nothing() {
    for len in [64, 1024] {
        let (_dir, mut writer) = journal();
        let payload = vec![7; len];
        let batch = vec![record(&payload); 16];
        let append = allocations(|| {
            for _ in 0..RECORDS {
                writer.append(&record(&payload)).unwrap();
            }
        });
        let in_place = allocations(|| {
            for _ in 0..RECORDS {
                writer
                    .append_with(&record(&[]), len, |buf| buf.fill(7))
                    .unwrap();
            }
        });
        let batched = allocations(|| {
            for _ in 0..RECORDS / 16 {
                writer.append_batch(batch.iter().copied()).unwrap();
            }
        });
        assert_eq!((append, in_place, batched), (0, 0, 0), "{len} B");
    }
}

/// Every reader and wait mode, following the writer record by record and then replaying a backlog.
#[test]
fn reading_allocates_nothing() {
    let mut blocking = Backoff::default();
    blocking.spins = 0;
    blocking.yields = 0;
    for mapped in [false, true] {
        for backoff in [Backoff::busy_spin(), Backoff::default(), blocking] {
            let (dir, mut writer) = journal();
            let config = ReaderConfig::default().backoff(backoff);
            let mut reader = if mapped {
                // SAFETY: only `writer` modifies the journal.
                unsafe { Reader::open_mapped(dir.path(), config).unwrap() }
            } else {
                Reader::open(dir.path(), config).unwrap()
            };
            // The first read allocates the safe reader's buffer.
            for _ in 0..2 {
                assert!(reader.try_next().unwrap().is_some());
            }
            let mut followed = 0;
            for _ in 0..RECORDS {
                writer.append(&record(&[7; 64])).unwrap();
                followed += allocations(|| {
                    let found = reader.next_timeout(Some(Duration::from_secs(5))).unwrap();
                    assert!(found.is_some());
                    assert!(reader.try_next().unwrap().is_none());
                });
            }
            for _ in 0..RECORDS {
                writer.append(&record(&[7; 1024])).unwrap();
            }
            let replayed = allocations(|| {
                for _ in 0..RECORDS {
                    assert!(reader.try_next().unwrap().is_some());
                }
            });
            assert_eq!((followed, replayed), (0, 0), "mapped {mapped} {backoff:?}");
        }
    }
}

/// A reader that waits for the next record allocates nothing either, apart from re-reading the wake-up record, which
/// it does at most every 100 ms while it blocks.
#[test]
fn waiting_allocates_only_to_refresh_the_wake_up_record() {
    let (dir, _writer) = journal();
    let mut spin = Reader::open(
        dir.path(),
        ReaderConfig::default()
            .backoff(Backoff::busy_spin())
            .notify(false),
    )
    .unwrap();
    let mut block = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    for reader in [&mut spin, &mut block] {
        for _ in 0..2 {
            assert!(reader.try_next().unwrap().is_some());
        }
        // The first wait can initialise state that lives as long as the process (the macOS wake-up functions).
        assert!(
            reader
                .next_timeout(Some(Duration::from_millis(1)))
                .unwrap()
                .is_none()
        );
    }
    let spinning = allocations(|| {
        let waited = spin.next_timeout(Some(Duration::from_millis(20))).unwrap();
        assert!(waited.is_none());
    });
    assert_eq!(spinning, 0);
    let timeout = Duration::from_millis(350);
    let before = std::time::Instant::now();
    let blocking = allocations(|| assert!(block.next_timeout(Some(timeout)).unwrap().is_none()));
    // One refresh per 100 ms of the wait, however late it ended; each opens the record by its path, and the identity
    // check of the segment, every 100 ms too, converts its path.
    let refreshes = before.elapsed().as_millis() as u64 / 100 + 2;
    assert!(
        blocking <= 8 * refreshes,
        "{blocking} allocations in {refreshes} refresh intervals"
    );
}

/// Copying a record out is the one allocation a caller can ask for.
#[test]
fn an_owned_record_is_one_allocation() {
    let (dir, mut writer) = journal();
    for _ in 0..RECORDS {
        writer.append(&record(&[7; 64])).unwrap();
    }
    let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    for _ in 0..2 {
        assert!(reader.try_next().unwrap().is_some());
    }
    let owned = allocations(|| {
        for _ in 0..RECORDS {
            assert!(black_box(reader.next_owned().unwrap()).is_some());
        }
    });
    assert_eq!(owned, RECORDS);
}
