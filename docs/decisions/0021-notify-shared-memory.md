# 0021: Keep the notify words in shared memory that cannot shrink

- **Status:** accepted (amends [0017](0017-reader-wake-ups.md))
- **Date:** 2026-11-01

## Context

ADR 0017 put the wake-up words (event counter, waiter count, writer flag) in a file in the journal directory, which
the writer and every reader mapped read-write. The cross-provider review of that change found a blocker: any process
that truncates `<dir>/notify` makes the next access to the mapping raise `SIGBUS`. The review reproduced this with a
safe `Reader::open` reader. The crash contract promises that the safe reader reads only with `pread` and cannot
receive `SIGBUS`, so ADR 0017 broke a stated guarantee. Checking the file's length before each access leaves a race.

The review also found two smaller problems:
- A wait call that fails for good (for example futex blocked by a seccomp policy, or `ENOTSUP` on macOS) returned
  immediately every time, so an idle reader busy-looped.
- The lost-wake stress test passed even with every wake-up disabled, because the 100 ms cap on each wait delivered
  every record anyway.

## Options considered

1. **Wake-ups only for `open_mapped` readers**, which already accept `SIGBUS` from truncated segments. The default
   safe reader would lose the feature.
2. **A `SIGBUS` handler.** A library must not install process-wide signal handlers.
3. **System V shared memory.** It cannot be resized, but its ids leak across crashes, and macOS allows only 32
   segments system-wide by default.
4. **Shared memory that the kernel refuses to shrink** (chosen):
   - Linux: a `memfd` sealed with `F_SEAL_SHRINK`, `F_SEAL_GROW` and `F_SEAL_SEAL`.
   - macOS: a POSIX shared-memory object (`shm_open`). XNU rejects `ftruncate` on such an object once it has been
     sized.

   A mapping of either can never lose its backing pages, so no access to it can raise `SIGBUS`.

## Decision

**Region.** The writer creates a 4,096-byte shared region with these `u32` words, all others zero:

| Offset | Word |
|---|---|
| 0 | event counter |
| 64 | waiter count |
| 128 | writer flag |
| 192, 200 | 64-bit nonce, low half then high half |

How it is created:
- **Linux:** `memfd_create` with `MFD_ALLOW_SEALING`, size 4,096, then the three seals. The writer keeps the
  descriptor open while it runs.
- **macOS:** `shm_open` with `O_CREAT | O_EXCL`, mode `0600`, under a fresh name `/tickertape.<16 hex digits>`, then
  sized. The writer removes the name (`shm_unlink`) when it is dropped. A crashed writer's name is removed by the
  next writer that opens the journal.

**Record.** `<dir>/notify` becomes an 80-byte record, written to `notify.tmp` and renamed into place:

| Bytes | Field |
|---|---|
| 0–7 | magic `TTNOTIFY` |
| 8–11 | `u32` version 1 |
| 12–15 | `u32` kind (1 = Linux memfd, 2 = macOS POSIX shm) |
| 16–23 | `u64` nonce |
| 24–27 | `u32` writer pid |
| 28–31 | `i32` descriptor (Linux) |
| 32–39, 40–47 | `u64` device and inode of the journal directory |
| 48–75 | name, NUL-padded (macOS) |
| 76–79 | CRC32C of bytes 0–75 |

The record is opened non-blocking and must be a regular file, so a FIFO or device in its place can't make anyone
hang. A writer removes a previous writer's macOS name only when the record carries this journal directory's
device and inode, so a record copied from another journal can't make it remove that journal's region.

**Readers.**
- A reader reads the record with `pread` and validates it, then opens the region:
  - Linux: `/proc/<pid>/fd/<fd>`, read-write. The reader checks with `F_GET_SEALS` that `F_SEAL_SHRINK` is set.
  - macOS: `shm_open` by name.
- The region must be at least 4,096 bytes. The reader maps it and checks that the nonce in the region matches the
  record (this guards against a reused pid or descriptor). Any failure means the reader polls.
- A reader re-reads the record at most every 100 ms while it is in the blocking phase. It switches to the new region
  when the nonce changes, and drops the old region when the record disappears. So readers started before the writer,
  or across writer restarts, get wake-ups too.

**Writer lifecycle.**
- When dropped, the writer clears the writer flag and wakes all waiters before it unmaps.
- A writer with `WriterConfig::notify(false)` removes the record, so readers poll.

**Wait errors.**
- These are normal, and the caller re-checks: a value mismatch (`EAGAIN`), an interruption (`EINTR`), a timeout, a
  wake-up.
- Any other error, or missing `os_sync` functions on macOS before 14.4, means "unsupported". The reader drops its
  region, sleeps for the rest of that wait (at most `Backoff::max_sleep`), and polls from then on.

**Tests.**
- The lost-wake test becomes a ping-pong: the writer appends one record at a time to a reader that blocks at once.
  The test asserts that no delivery takes as long as the 100 ms cap. Disabling the wake call makes it fail.
- New tests:
  - truncating the record file doesn't affect a waiting reader;
  - the region cannot be shrunk on either platform;
  - a reader's deadline is honoured below the cap;
  - a reader started before the writer gets wake-ups.

## Consequences

- Readers and the writer never map a file in the journal directory for wake-ups, so `Reader::open` again maps
  nothing that another process can shrink. The crash contract's statement about the safe reader holds as written.
- Wake-ups work only between processes of the same user:
  - Linux: opening `/proc/<pid>/fd/<n>` needs the same access as reading another process's descriptors, so it fails
    across users, across pid namespaces, and with `hidepid`.
  - macOS: the object's mode is `0600`.

  In all these cases readers poll, as they did before ADR 0017.
- On macOS, if a journal is never opened again after its writer crashes, one 4 KiB shared-memory name stays until
  reboot.
- `mmap.rs` gains `memfd_create`/`fcntl` seals and `shm_open`/`shm_unlink`. It needs another cross-provider review.
