# Crash contract

Status: **accepted** (M1, approved by the owner on 2026-10-31). This document says exactly what
tickertape guarantees when things go wrong, on each supported platform, and which test checks each guarantee. A
guarantee without a passing test is marked **untested**, and nothing is claimed beyond what is written here.
Rationale: ADRs [0004](decisions/0004-durability-levels.md) and [0007](decisions/0007-checkpoint-marker-and-recovery.md).
Byte-level details: [`format.md`](format.md).

Supported: Linux (x86-64, aarch64) and macOS (aarch64), on a **local** filesystem (Linux: ext4, xfs, btrfs; macOS:
APFS). Network filesystems, Windows and multiple writers are not supported.

## Two levels: visible and durable

| Level | Reached when | What it means |
|---|---|---|
| **Visible** | `Writer::append` returns `Ok(seq)` | The record is in the shared page cache. Readers in other processes on the same host can read it, and it survives the *writer process* dying. It is **not** on stable storage |
| **Durable** | `Writer::checkpoint` returns `Ok(())` | Every record appended before the call is on stable storage, under the storage assumptions below |

Visibility is not durability. A reader can see, and act on, a record that a later power loss erases, and the writer
then reuses that sequence number for a different record. Applications that must never act on a record that could
disappear should act only on records up to the writer's last checkpoint (communicated by the application; a
durable-only read mode is on the "Later" list).

### What `checkpoint()` does

Everything written since the last successful checkpoint is "dirty": records and seals in every segment touched
(including segments the writer has already rolled past, whose mappings it keeps until the checkpoint), files and
directories created or renamed, and repairs made at open. A new writer treats everything after the current
checkpoint marker as dirty, including records an earlier writer appended but never checkpointed.

| Step | Linux | macOS |
|---|---|---|
| 1. Data | `msync(MS_SYNC)` on each dirty range, then `fdatasync` on each dirty segment | `msync(MS_SYNC)` on each dirty range, then `fsync` on each dirty segment |
| 2. Names | `fsync` on the journal directory, `quarantine/`, and the parent of any directory the writer created, if any entry in them changed | `fsync` on the same directories |
| 3. Barrier | (the `fdatasync`/`fsync` calls above already flush the device cache on filesystems mounted with barriers, the default) | `fcntl(F_FULLFSYNC)` on the last dirty segment, after all of the above. It asks the drive to flush its whole write cache, which covers the earlier writes and directory updates too |
| 4. Marker | Write the next `checkpoint` slot with `pwrite`, then `fdatasync` it | Write the next `checkpoint` slot with `pwrite`, then `fcntl(F_FULLFSYNC)` on it |

The marker file itself is created, synced, and its directory entry synced when the journal is created, so step 4
only ever overwrites a slot in an existing file.

The marker is written only after steps 1–3 succeed, so it never points past data that isn't durable. On macOS,
plain `fsync` does not flush the drive's write cache; `F_FULLFSYNC` does, and is much slower. tickertape always uses
it, and if it isn't supported (`ENOTSUP`), `checkpoint()` fails rather than claiming less.

**Errors are sticky.** If any step fails, `checkpoint()` returns the error and the writer becomes **poisoned**:
every later `checkpoint()` fails with `Error::DurabilityLost`, while appends continue to work (visibility is
unaffected). This is because on Linux a failed `fsync` can mark the dirty pages clean without writing them, so a
later successful call proves nothing. A new writer opened in the same boot cannot detect this either: **after a
checkpoint error, records written before it may not be durable even if a later writer's checkpoint succeeds.** Treat
a checkpoint error as an incident.

## Storage assumptions for durability

The durable level holds only if: (a) the filesystem and drive honour the sync calls above (no lying write caches);
(b) a sector or filesystem block that has been made durable is not damaged by an interrupted later write *to the
same block*. Records are small and packed, so a checkpointed record often shares a block with records appended
after the checkpoint. Drives with power-loss protection, and filesystems with copy-on-write data blocks
(btrfs, APFS), avoid in-place rewrites of durable blocks; ext4 and xfs in their default modes rewrite blocks in place
and rely on the drive's sector-write atomicity. If (b) fails, the checkpointed records in that block are lost.
Recovery does not re-read data before the checkpoint marker, so it won't necessarily notice; readers and
`tickertape verify` detect the damage by checksum when they reach it (C11).

## Guarantees by failure

"Committed" means the record's commit word was stored (`format.md` → Publication); `append` returning `Ok` implies
committed.

| # | Failure | Guarantee | Platforms | Test |
|---|---|---|---|---|
| C1 | Writer process dies (`kill -9`, panic, abort) at any point | Every committed record is intact and readable after restart. A record being appended is entirely present or entirely absent. The next writer continues at the next sequence number, with no gap and no duplicate | Linux, macOS | `tests/kill_loop.rs` (M4, ≥200 iterations) — **not yet written** |
| C2 | Writer process dies with a partly written record, or the caller's fill closure panics | The partial bytes are never visible to readers. The writer (or the next one) zeroes them and makes the zeroing durable before committing anything after them, so no stale bytes can later pass as a record | Linux, macOS | `tests/recovery.rs::torn_final_record`, `::panic_in_fill_then_shorter_append` (simulated crash state); process kill: `tests/kill_loop.rs` (M4) — **not yet written** |
| C3 | Writer process dies while holding the lock | The OS releases the `flock` when the process dies; a new writer can open immediately. No stale-lock cleanup is needed | Linux, macOS | `tests/lock.rs::stale_lock_after_kill` (M4) — **not yet written** |
| C4 | Writer process dies during a roll | No committed record is lost. The next writer completes the roll (`format.md` → Recovery). Readers waiting at the end of the old segment continue once the seal is written | Linux, macOS | `tests/recovery.rs::crash_between_link_and_seal` (simulated crash state); process kill: `tests/kill_loop.rs` with small segments (M4) — **not yet written** |
| C5 | A second writer tries to open the same journal | It fails with `Error::Locked` without touching any file | Linux, macOS | `tests/lock.rs::second_writer_fails` |
| C6 | A reader process dies, or is slow | No effect on the writer or other readers. The writer never waits for readers. A slow reader falls behind and disk use grows; nothing is deleted | Linux, macOS | `tests/slow_reader.rs` (M4) — **not yet written** |
| C7 | OS crash or power loss **after** a successful `checkpoint()` | Every record appended before that checkpoint is intact after reboot, under the storage assumptions above, and unless a checkpoint has failed earlier in the same boot | Linux, macOS | **Untested.** Process-kill tests cannot show this |
| C8 | OS crash or power loss, records **not** covered by a checkpoint | Any of them may be lost, including records readers already saw. Recovery keeps the longest valid chain starting at the last checkpoint, never accepts a record that fails validation, and moves (never deletes) anything after the first invalid position to `quarantine/` | Linux, macOS | Validation and quarantine: `tests/recovery.rs::disconnected_segment_is_quarantined`, `::dangling_checkpoint_is_refused`, `::missing_middle_segment_is_refused`, fuzz targets. Power loss itself: **untested** |
| C9 | Disk full while creating a segment | `append` returns an error for which `Error::is_storage_full()` is true. The journal stays consistent and readable; a later `append` retries the roll | Linux, macOS | `tests/disk_full.rs` (M4, injected error or small filesystem) — **not yet written** |
| C10 | Disk full while writing into an existing segment | Designed not to happen: segments are preallocated, so writes into them should need no new blocks on ext4 and xfs. On copy-on-write filesystems (btrfs, APFS) this is not guaranteed; see "Known risks". If it does happen, the process gets `SIGBUS` | Linux (ext4, xfs) | **Untested** |
| C11 | Corrupted bytes in a segment (storage error, outside modification) | Readers and `verify` detect corruption that breaks the CRC32C, length, sequence or chain checks and report it with the file and offset; they never return a record that fails validation. The writer quarantines (or, with `RecoveryPolicy::Fail`, refuses) a corrupt tail; it repairs by writing a new file, never by changing bytes a reader may have mapped | Linux, macOS | `tests/recovery.rs::malformed_checksum_is_quarantined_and_repaired`, `::malformed_length_is_rejected`; fuzz targets `parse_segment`, `decode_record` |
| C12 | A checkpoint fails | The error is returned and the writer stays poisoned (`Error::DurabilityLost` on later checkpoints). See "Errors are sticky" | Linux, macOS | `tests/checkpoint.rs::poisoned_after_failure` (M4, injected error) — **not yet written** |

## Readers, the writer and memory safety

- **`Reader::open` (safe, the default)** doesn't map the files. It reads them with `pread` into a buffer it owns and
  validates the copy, so it is memory-safe whatever other processes do to the files, and it can't receive `SIGBUS`.
  At worst it reports corruption.
- **`Reader::open_mapped` (`unsafe`, zero-copy)** hands out `&[u8]` slices pointing into the shared mapping. The caller
  promises that no process other than a tickertape writer modifies the journal's segment files while the reader
  exists. A correct writer never modifies committed bytes and repairs by writing new files, so this holds in normal
  operation, including writer crashes and recovery.
- **The writer** maps the segments it writes. Its API is safe, following the Rust standard library's position that
  its safety guarantees "only cover what the program itself can do, and not what entities outside the program can
  do to it" (`std::os::unix::io`, on `/proc/self/mem`). Another process writing into or truncating the files the
  writer has mapped is such an outside entity, and can crash or corrupt the writer process.

## Known risks and limits

- **SIGBUS.** The writer and `open_mapped` readers access segments through memory maps. If a segment file is
  truncated by another process, or the storage returns an I/O error on a page fault, the process receives `SIGBUS`
  and dies. tickertape does not catch it. Don't truncate or edit segment files while the journal is in use.
- **Copy-on-write filesystems** (btrfs, APFS). Preallocation reserves space, but copy-on-write may still need new
  blocks on the first write into a block. `TODO(verify)`: whether a full btrfs or APFS volume can make writes into a
  preallocated segment fail, and how that surfaces (SIGBUS) — measure in M4.
- **Readers after a repair.** A reader whose segment recovery replaced or moved to `quarantine/` returns
  `Error::Discarded` once it notices (on a validation error, after 100 ms of waiting, or at the next segment
  boundary); it never silently continues into the repaired history. Until then it may return records that recovery
  removed, like records lost to a power loss.
- **Missing segments.** If the checkpoint marker's segment, or a segment in the middle of the chain, is missing,
  the writer refuses to open (under every recovery policy) rather than discard history on its own.
- **CRC32C** detects all burst errors up to 32 bits and most other corruption, not all of it. It is not a
  cryptographic integrity check.
- **Clocks.** Timestamps are caller-supplied data. tickertape never orders or validates by timestamp.
- **`flock`** works on local filesystems. On network filesystems it may silently fail to exclude other hosts; they
  are not supported.
