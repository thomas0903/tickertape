# 0003: Publication protocol and access to mapped memory

- **Status:** proposed (M1 approval gate)
- **Date:** 2026-10-31 (revised after the cross-provider review the same day)

## Context

Readers in other processes must never see a partly written record, without locks, and a slow reader must never block
the writer. The Rust side must be sound: Rust's aliasing and atomics rules apply to how each process touches the
shared mapping. Spec: [`format.md`](../format.md) → Publication.

## Options considered

1. **Per-record commit word.** The record's `len` is stored last with release ordering; readers acquire the word at
   their cursor. Zero means "not yet". Readers poll memory they read next anyway, the writer updates no shared
   counter, and recovery is the same forward scan readers do.
2. **Published length in the segment header.** One load per poll for readers, but every reader polls the cache line
   the writer updates on each append, and the header and data can disagree after a crash, so recovery still scans.
3. **Seqlock or mutex in shared memory.** Unnecessary for immutable records; a mutex lets a dead process block others.

## Decision

Option 1. The writer writes the body `[P+4, P+len)`, then release-stores `len` into the commit word at `P`. Seals use
the same mechanism, published only after the next segment file exists, so readers find the next file by name.

**How each side touches the mapping** (all of it in the one `unsafe` module, `mmap.rs`):

- The **writer** maps segments `PROT_READ | PROT_WRITE`, `MAP_SHARED`. It writes a body through a `&mut [u8]` that
  covers exactly `[P+4, P+len)` (bytes no reader reads until the commit), and stores commit words through
  `AtomicU32::from_ptr` (valid: the memory is writable) with `Ordering::Release`. It never creates a reference that
  covers a commit word other processes may be loading, and never a whole-segment slice while readers may be active.
- **Readers** map segments `PROT_READ`, `MAP_SHARED` (read-only file handle, so readers work without write
  permission). They load a commit word with `Ordering::Relaxed` through a `&AtomicU32` made by casting the pointer,
  then issue `atomic::fence(Ordering::Acquire)` if it is non-zero. Rust documents 4- and 8-byte `Relaxed` atomic loads
  as permitted on read-only memory on x86-64 and aarch64 ("Atomic accesses to read-only memory" in `std::sync::atomic`,
  present at our MSRV 1.85); `Acquire` loads are not, which is why the ordering comes from the fence. A relaxed load
  that reads the value of a release store, followed by an acquire fence, synchronises with that store.
- The **safe reader** (`Reader::open`) then copies the frame `[P, P + align8(len))` into its own buffer with
  `Relaxed` 8-byte atomic loads (records are 8-aligned) and validates the copy. It never creates a reference into the
  mapping, so other processes can't make it unsound.
- The **zero-copy reader** (`unsafe Reader::open_mapped`) validates the body and hands out a `&[u8]` covering exactly
  the body. This is sound only while nobody modifies committed bytes, which is the caller's stated precondition
  (ADR 0006). The writer never modifies committed bytes: abandoned writes are zeroed before any later commit, and
  repairs of corrupt data replace the file instead of editing it (ADR 0007).
- **Pure format code** works on precisely bounded slices: a record body, a seal CRC, a header. Whole-segment
  functions (scanning, fuzzing, Miri) take a `SegmentView` trait, implemented both by an in-memory image and by the
  mapping wrapper, so they never require a slice over live memory.

**Cross-process assumption.** The Rust memory model formally describes threads in one process. We rely on the
platform behaviour that atomic operations on the same naturally aligned location of a `MAP_SHARED` file mapping
synchronise across processes as they do across threads. This holds on x86-64 and aarch64 under Linux and macOS,
where both mappings refer to the same physical page; it is what shared-memory IPC on these platforms relies on.

## Consequences

- The safe reader pays one copy per record into a reused buffer; zero-copy is opt-in and `unsafe`.
- No reader registration and no back-pressure: readers can't slow the writer; the writer can't see how far behind
  readers are. Readers busy-poll or back off (ADR 0006).
- A `loom` model of the protocol (one writer, one reader, a few slots) is planned in M2 if it fits; the kill-loop and
  cross-process tests (M3, M4) exercise the real thing. The `unsafe` module gets an independent review in M4.
