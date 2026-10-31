# 0003: Publication protocol and access to mapped memory

- **Status:** accepted (owner approval, 2026-10-31)
- **Date:** 2026-10-31 (revised twice after cross-provider reviews the same day)

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
- The **safe reader** (`Reader::open`) doesn't map anything. It reads the file with `pread` into a buffer it owns,
  normally a chunk of many records at once. From Rust's point of view `pread` is an opaque system call that fills
  our buffer, so no behaviour of other processes can make this reader unsound. To see a committed body, it relies on
  ordering: it reads the commit word, issues `atomic::fence(Ordering::Acquire)` (a hardware barrier: `dmb ish` on
  aarch64, which orders this thread's later loads, including those the kernel performs in the next `pread`), and only
  then trusts body bytes read after the fence. Within a chunk, a record's body may have been copied before its commit
  word; such a record fails its CRC, so the reader re-reads from that position before ever reporting corruption, and
  reports `Error::Corrupt` only if the commit word is unchanged and the CRC still fails on a re-read after the fence.
- The **zero-copy reader** (`unsafe Reader::open_mapped`) maps segments `PROT_READ`, `MAP_SHARED` (a read-only file
  handle, so it works without write permission). It loads a commit word with `Ordering::Relaxed` through a
  `&AtomicU32` made by casting the pointer (not `from_ptr`, which requires writable memory), then issues
  `atomic::fence(Ordering::Acquire)` if the word is non-zero. Rust documents 4- and 8-byte `Relaxed` atomic loads as
  permitted on read-only memory on x86-64 and aarch64 ("Atomic accesses to read-only memory" in `std::sync::atomic`,
  present at our MSRV 1.85); `Acquire` loads are not, which is why the ordering comes from the fence. It checks bounds
  from the commit word, copies the frame out with `Relaxed` 8-byte atomic loads (no reference into the mapping),
  validates the copy, and only then hands out a `&[u8]` covering exactly the payload. (Validating before any slice
  exists was added after the M4 review: a corrupted length could otherwise make a slice cover bytes still being
  written.) This is sound only while nobody modifies committed bytes, which is the caller's stated precondition
  (ADR 0006). The writer never modifies committed bytes: abandoned writes are zeroed before any later commit, and
  repairs of corrupt data replace the file instead of editing it (ADR 0007).
- **Pure format code** works on precisely bounded slices: a record body, a seal CRC, a header. Whole-segment
  functions (scanning, fuzzing, Miri) take a `SegmentView` trait whose methods are safe for every input
  (bounds-checked, returning `None` when out of range). Its implementations are an in-memory image, the `pread`
  reader (copies), and the writer's own mapping, which only the lock holder can construct (crate-private).

**Cross-process assumption.** The Rust memory model formally describes threads in one process. We rely on the
platform behaviour that atomic operations on the same naturally aligned location of a `MAP_SHARED` file mapping
synchronise across processes as they do across threads. This holds on x86-64 and aarch64 under Linux and macOS,
where both mappings refer to the same physical page; it is what shared-memory IPC on these platforms relies on.

## Consequences

- The safe reader pays a copy per record and a system call per chunk; zero-copy is opt-in and `unsafe`.
- The writer's API is safe. Its mappings could be modified by an outside process; Rust's standard library treats such
  outside entities as beyond its safety guarantees (`std::os::unix::io`, "`/proc/self/mem` and similar OS
  features"), and the crash contract documents the precondition.
- No reader registration and no back-pressure: readers can't slow the writer; the writer can't see how far behind
  readers are. Readers busy-poll or back off (ADR 0006).
- A `loom` model of the protocol (one writer, one reader, a few slots) is planned in M2 if it fits; the kill-loop and
  cross-process tests (M3, M4) exercise the real thing. The `unsafe` module gets an independent review in M4.
