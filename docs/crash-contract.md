# Crash contract

Status: **draft for M1 approval**. This document says exactly what tickertape guarantees when things go wrong, on
each supported platform, and which test checks each guarantee. A guarantee without a passing test is marked
**untested**, and nothing here is claimed beyond what is written. Rationale: ADR
[0004](decisions/0004-durability-levels.md). Byte-level details: [`format.md`](format.md).

Supported: Linux (x86-64, aarch64) and macOS (aarch64), on a **local** filesystem (Linux: ext4, xfs, btrfs; macOS:
APFS). Network filesystems, Windows and multiple writers are not supported.

## Two levels: visible and durable

| Level | Reached when | What it means |
|---|---|---|
| **Visible** | `Writer::append` returns `Ok(seq)` | The record is in the shared page cache. Readers in other processes on the same host can read it, and it survives the *writer process* dying. It is **not** on stable storage |
| **Durable** | `Writer::checkpoint` returns `Ok(())` | Every record appended before the call has been written to stable storage, as far as the OS and drive honour the sync calls listed below |

Visibility is not durability. A reader can see, and act on, a record that a later power loss erases. After such a
power loss, the writer reuses those sequence numbers for new records. Consumers that must never act on a record that
could disappear should act only on records up to the writer's last checkpoint (communicated by the application;
a durable-only read mode is on the "Later" list).

### What `checkpoint()` does

| Platform | Calls, in order | Notes |
|---|---|---|
| Linux | `msync(MS_SYNC)` on the dirty range of each mapped segment written since the last checkpoint, then `fdatasync` on each such segment, then `fsync` on the journal directory if a segment was created since the last checkpoint | `fsync` makes the kernel issue a cache flush to the device if the filesystem is mounted with barriers (the default) |
| macOS | `msync(MS_SYNC)` on the dirty range, then `fcntl(F_FULLFSYNC)` on each such segment, then `fcntl(F_FULLFSYNC)` on the journal directory (falling back to `fsync` if the directory doesn't support it) if a segment was created | On macOS, `fsync` does **not** ask the drive to flush its write cache; `F_FULLFSYNC` does. It is much slower |

If any call fails, `checkpoint()` returns the error, and **nothing** about durability since the previous successful
checkpoint may be assumed. Calling `checkpoint()` again retries. (On Linux, a failed `fsync` may have dropped the
dirty pages; the error is reported, but the data may be lost even if a retry succeeds. See ADR 0004.)

## Guarantees by failure

"Committed" means the record's commit word was stored (see `format.md` → Publication); `append` returning `Ok` implies
committed.

| # | Failure | Guarantee | Platforms | Test |
|---|---|---|---|---|
| C1 | Writer process dies (`kill -9`, panic, abort) at any point | Every committed record is intact and readable after restart. A record being appended is either entirely present (committed) or entirely absent. The next writer resumes at the next sequence number with no gap and no duplicate | Linux, macOS | `tests/kill_loop.rs` (M4, ≥200 iterations) — **not yet written** |
| C2 | Writer process dies with a partly written final record | The partial bytes are never visible to readers; the next writer zeroes them and overwrites the space | Linux, macOS | `tests/recovery.rs::torn_final_record` (M4) — **not yet written** |
| C3 | Writer process dies while holding the lock | The OS releases the `flock` when the process dies; a new writer can open immediately. No stale-lock cleanup is needed | Linux, macOS | `tests/lock.rs::stale_lock_after_kill` (M4) — **not yet written** |
| C4 | Writer process dies during a roll | No committed record is lost. The next writer completes or redoes the roll (`format.md` → Recovery). Readers waiting at the end of the old segment continue once the seal is written | Linux, macOS | `tests/kill_loop.rs` with small segments (M4) — **not yet written** |
| C5 | A second writer tries to open the same journal | It fails with `Error::Locked`, without touching any file | Linux, macOS | `tests/lock.rs` (M2) — **not yet written** |
| C6 | A reader process dies, or is slow | No effect on the writer or other readers. The writer never waits for readers. A slow reader falls behind, and disk use grows; nothing is deleted | Linux, macOS | `tests/slow_reader.rs` (M4) — **not yet written** |
| C7 | OS crash or power loss **after** a successful `checkpoint()` | Every record appended before that checkpoint is intact after reboot | Linux, macOS | **Untested.** Process-kill tests cannot show this. It depends on the filesystem and the drive honouring flushes |
| C8 | OS crash or power loss, records **not** covered by a checkpoint | Any of these records may be lost, including records that readers already saw. Recovery keeps the longest valid prefix and never accepts a record that fails validation | Linux, macOS | Validation part: `tests/recovery.rs` and the fuzz targets (M4). Power-loss part: **untested** |
| C9 | Disk full while creating a segment | `append` returns an error for which `Error::is_storage_full()` is true. The journal stays consistent and readable; a later `append` retries the roll | Linux, macOS | `tests/disk_full.rs` (M4, injected or small filesystem) — **not yet written** |
| C10 | Disk full while writing into an existing segment | Designed not to happen: segments are preallocated, so writes into them should need no new blocks on ext4 and xfs. On copy-on-write filesystems (btrfs, APFS) this is not guaranteed; see "Known risks". If it does happen, the process gets `SIGBUS` | Linux (ext4, xfs) | **Untested** |
| C11 | Corrupted bytes in a segment (storage error, outside modification) | Readers and `verify` detect any corruption that breaks the CRC32C, length or sequence checks, and report it as an error with the file and offset; they never return a record that fails validation. The writer quarantines (or, with `RecoveryPolicy::Fail`, refuses) a corrupt tail rather than deleting it | Linux, macOS | `tests/recovery.rs::malformed_*` and fuzzing (M2, M4) — **not yet written** |

## Known risks and limits

- **SIGBUS.** Readers and the writer access segments through memory maps. If a segment file is truncated or deleted
  and replaced by another process, or the storage returns an I/O error on a page fault, the process receives `SIGBUS`
  and dies. tickertape does not catch it. Don't modify segment files while the journal is in use.
- **Cooperating processes only.** The zero-copy reader API hands out `&[u8]` slices into shared mappings. That is sound
  only while no process modifies committed bytes, which a correct writer never does. A malicious or buggy process
  with write access to the files can break this. Readers that cannot trust the other processes should copy records
  out (`Record::to_owned`) and treat the journal as untrusted input; validation still applies.
- **Corrupt-tail recovery and live readers.** When the writer repairs a corrupt committed tail (C11, power loss only),
  a reader that is reading that exact record at that moment can observe bytes changing. It reports a validation error
  for that record, or, with probability about 2^-32 per occurrence, accepts torn bytes that happen to match the CRC.
- **Copy-on-write filesystems** (btrfs, APFS). Preallocation reserves space, but copy-on-write may still need new
  blocks on the first write. `TODO(verify)`: whether a full btrfs or APFS volume can make writes into a preallocated
  segment fail, and how that surfaces (SIGBUS) — measure in M4.
- **CRC32C** detects all burst errors up to 32 bits and most other corruption, not all of it. It is not a
  cryptographic integrity check.
- **Clocks.** Timestamps are caller-supplied data. tickertape never orders or validates by timestamp.
- **Lock file on unusual filesystems.** `flock` works on local filesystems. On network filesystems it may silently
  not exclude other hosts; they are not supported.
