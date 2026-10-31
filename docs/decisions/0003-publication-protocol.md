# 0003: Publication protocol between the writer and readers

- **Status:** proposed (M1 approval gate)
- **Date:** 2026-10-31

## Context

Readers in other processes must never see a partly written record, without locks, and a slow reader must never block
the writer. Spec: [`format.md`](../format.md) → Publication.

## Options considered

1. **Per-record commit word.** The record's `len` is stored last with release ordering; readers acquire-load the
   word at their cursor. Zero means "not yet". Readers poll memory they will read next anyway, the writer touches no
   shared counter, and recovery is the same forward scan readers do.
2. **Published length in the segment header.** One `u64` "committed bytes" field updated after every append.
   Readers need only one load, but every reader polls the same cache line the writer updates on each append, and the
   header's value and the data can disagree after a crash, so recovery still has to scan.
3. **Seqlock or mutex in shared memory.** Unnecessary for immutable records; a mutex lets a dead process block others.

## Decision

Option 1. The writer writes bytes `[P+4, P+len)` with ordinary stores, then does a release store of `len` into the
4-byte-aligned commit word at `P`. Readers acquire-load it; non-zero means everything before it is visible. Segment
ends use the same mechanism with a seal value `0xFFFFFFFF` plus a CRC over the next sequence number, published only
after the next segment file exists, so readers find the next file by name without listing the directory.

**Memory-model assumption.** The Rust (C++20) memory model formally describes threads within one process. We rely on
the hardware and OS behaviour that a release store and an acquire load on the same naturally aligned 4-byte location
of a `MAP_SHARED` file mapping synchronise across processes on the same host, as they do between threads. This holds
for x86-64 (TSO; plain `mov` for both) and aarch64 (`stlr`/`ldar`) on Linux and macOS, where shared mappings of the
same file page refer to the same physical memory. Accesses go through `AtomicU32::from_ptr` on the mapping, never
through a `&[u8]` that covers a commit word. Committed bytes are immutable, which is what makes handing out `&[u8]`
for payloads sound (ADR 0006).

The writer zeroes stale bytes beyond the end during recovery *before* its first release store, so the release also
publishes those zeros to readers that later advance into them.

## Consequences

- No reader registration, no back-pressure: readers can't slow the writer, and the writer can't know how far behind
  readers are.
- Readers busy-poll or back off (ADR 0006); there are no wake-ups (futex/eventfd is on the "Later" list).
- A `loom` model of the protocol (one writer, one reader, a few slots) is planned in M2 if it fits; the kill-loop and
  cross-process tests (M3, M4) exercise the real thing.
- An independent review of the `unsafe` module is required before release (M4).
