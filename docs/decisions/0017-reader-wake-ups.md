# 0017: Reader wake-ups instead of sleeping

- **Status:** accepted (owner decision: build the "Later" items, 2026-11-01); where the words live amended by
  [0021](0021-notify-shared-memory.md)
- **Date:** 2026-11-01

## Context

Readers poll the next commit word, spinning, then yielding, then sleeping up to `Backoff::max_sleep` (1 ms by
default). A sleeping reader therefore sees a new record up to a millisecond late, and a spinning one burns a core.
ADR 0003 deferred kernel wake-ups.

## Decision

A small shared **notify file**, `<dir>/notify` (4,096 bytes, little-endian, created by the writer): a 4-byte
**event counter** at offset 0 and a 4-byte **waiter count** at offset 64 (separate cache lines), all other bytes zero.

- **Writer** (`WriterConfig::notify`, on by default): after publishing a record or a seal, it increments the event
  counter and, if the waiter count is non-zero, wakes all waiters on the counter's address. That is one atomic
  increment and one load per append; a system call only when someone waits.
- **Reader** (`ReaderConfig::notify`, on by default): after its spin and yield phases find nothing, instead of
  sleeping it increments the waiter count, reads the event counter, checks the next commit word once more, and if
  there is still nothing, waits on the counter's address until it changes or a timeout passes (at most
  `Backoff::max_sleep`, so a missed wake-up or a writer without notify support costs no more than polling does).
  Then it decrements the waiter count. All of these use sequentially consistent ordering, the classic event-count
  protocol: a writer that publishes after the reader's last check changes the counter, so the reader's wait returns.
- **Platforms:** Linux `futex(FUTEX_WAIT)`/`futex(FUTEX_WAKE)` without the private flag (shared file mappings are keyed
  by file and offset); macOS `os_sync_wait_on_address_with_timeout` / `os_sync_wake_by_address_all` with the shared
  flags (macOS 14.4+), looked up at run time. Where unavailable, or if the notify file can't be opened read-write
  (for example a read-only copy of a journal), readers fall back to polling.

The notify file carries no data: deleting it or its contents being wrong only affects latency, never correctness.
Format version 1 readers ignore unknown files, so the format version doesn't change.

**Amendment (implementation, same day):** the notify file also has a `u32` flag at offset 128 that the writer sets
to 1 when it maintains the counter (0 when `WriterConfig::notify` is off). Readers block for at most 100 ms per wait
when the flag is set, and at most `Backoff::max_sleep` otherwise. The writer also bumps the counter after a seal and
after each successful checkpoint (durable-only readers wait for the marker to move).

## Consequences

- New `unsafe` code (system calls, atomics on a shared mapping) in `mmap.rs`; it needs the cross-provider review
  required for that module.
- A reader that dies while waiting leaves the waiter count raised; the writer then makes a wake system call per
  append until the count drops. That costs performance, never correctness; documented.
- Readers need write permission to `notify` to use wake-ups.
