# Crash contract

Status: **accepted** (M1, approved by the owner on 2026-10-31). This document says exactly what
tickertape guarantees when things go wrong, on each supported platform, and which test checks each guarantee. A
guarantee without a passing test is marked **untested**, and nothing is claimed beyond what is written here.
Rationale: ADRs [0004](decisions/0004-durability-levels.md), [0007](decisions/0007-checkpoint-marker-and-recovery.md) and
[0035](decisions/0035-recovery-preserves-a-committed-tail.md).
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
disappear should open their readers with `ReaderConfig::durable_only(true)` (ADR 0010; CLI: `--durable-only`), which
returns only records covered by the last successful checkpoint (C13).

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

The same holds when the writer cannot sync the zeroing of an abandoned write (a panic in an `append_with` fill
closure; `format.md` → Abandoned writes). That writer refuses appends as well as checkpoints
(`Writer::abandoned_write_error`), and the durability of everything written since its last successful checkpoint is
unknown, also for a later writer in the same boot.

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
committed. History deliberately removed by a retention policy or `tickertape archive` is no longer part of the
journal; the durability and restart guarantees below apply to retained history.

| # | Failure | Guarantee | Platforms | Test |
|---|---|---|---|---|
| C1 | Writer process dies (`kill -9`, panic, abort) at any point | Every committed record is intact and readable after restart. A record being appended is entirely present or entirely absent. The next writer continues at the next sequence number, with no gap and no duplicate. One exception: if the record being appended had a payload holding an 8-byte-aligned, CRC-valid frame whose sequence number falls in recovery's window (raw frames copied from another journal, for example) and its body was not completely written, recovery cannot tell it from damage (C11, ADR 0035): `Quarantine` preserves its bytes and continues as stated, `Fail` refuses to open (`EmptyCommitWord`) until an operator opens it once with `Quarantine` (`operations.md` → After a crash). Nothing is lost either way | Linux, macOS | `tests/kill_loop.rs::kill_loop_250` (250 `SIGKILL`s at random points, with a live reader across all of them; 20 in the default test run); `tests/soak.rs::soak` (kills during appends, rolls, checkpoints, retention and recovery, for minutes to hours, with six reader processes; checks every record whose `append` returned survives); journals left by killed v0.1.0–v0.4.0 writers: `tests/compat.rs::current_writer_recovers_appends_and_checkpoints` |
| C2 | Writer process dies with a partly written record, or the caller's fill closure panics | The partial bytes are never visible to readers. The writer (or the next one) zeroes them and makes the zeroing durable before committing anything after them, so no stale bytes can later pass as a record. Nothing is quarantined, unless the torn payload contains an 8-byte-aligned, CRC-valid frame (from any journal) whose sequence number falls in recovery's window and the body was not completely written: recovery cannot tell those bytes from committed records behind a damaged commit word and preserves them (`Quarantine`) or refuses (`Fail`; see C1) (ADR 0035) | Linux, macOS | `tests/recovery.rs::torn_final_record`, `::panic_in_fill_then_shorter_append`, `tests/committed_tail.rs::every_torn_append_shape_is_zeroed_in_place_with_nothing_quarantined`, `::torn_payload_holding_the_next_records_frame_is_preserved_not_zeroed`, `::payload_holding_another_journals_frames` (simulated crash states); process kill: `tests/kill_loop.rs` (including `::kill_inside_fill`, kills inside the fill closure), `tests/soak.rs::soak`; a torn append left by each released writer: `tests/compat.rs::current_writer_recovers_appends_and_checkpoints` |
| C3 | Writer process dies while holding the lock | The OS releases the `flock` when the process dies; a new writer can open immediately. No stale-lock cleanup is needed | Linux, macOS | `tests/lock.rs::stale_lock_after_kill`; and every iteration of `tests/kill_loop.rs` |
| C4 | Writer process dies during a roll | No committed record is lost. The next writer completes the roll (`format.md` → Recovery). Readers waiting at the end of the old segment continue once the seal is written | Linux, macOS | `tests/recovery.rs::crash_between_link_and_seal` (simulated crash state); process kill: `tests/kill_loop.rs` (64 KiB segments, so kills land in rolls), `tests/soak.rs::soak` |
| C5 | A second writer tries to open the same journal | It fails with `Error::Locked` without touching any file | Linux, macOS | `tests/lock.rs::second_writer_fails` |
| C6 | A reader process dies, or is slow | No effect on the writer or other readers. The writer never waits for readers. A slow reader falls behind; without retention disk use grows, and with retention it may receive `Error::Discarded` at a removed segment boundary | Linux, macOS | `tests/slow_reader.rs::stalled_reader_in_process_catches_up`, `::slow_reader_process_does_not_block_writer`; with retention: `tests/retention.rs::reader_discarded_at_removed_segment_end`; reader processes overtaken by retention while the writer is killed and restarted: `tests/soak.rs::soak` |
| C7 | OS crash or power loss **after** a successful `checkpoint()` | Every record appended before that checkpoint is intact after reboot, under the storage assumptions above, and unless a checkpoint, or the sync that zeroes an abandoned write, has failed earlier in the same boot | Linux, macOS | **Untested.** Process-kill tests cannot show this |
| C8 | OS crash or power loss, records **not** covered by a checkpoint | Any of them may be lost, including records readers already saw. Recovery keeps the longest valid chain starting at the last checkpoint, never accepts a record that fails validation, and moves (never deletes) anything after the first invalid position to `quarantine/`. An empty commit word with committed records after it (pages written back out of order, a lost write) is such a position: the records after it are preserved, not zeroed (ADR 0035) | Linux, macOS | Validation and quarantine: `tests/recovery.rs::disconnected_segment_is_quarantined`, `::dangling_checkpoint_is_refused`, `::missing_middle_segment_is_refused`, `::fail_policy_refuses_invalid_successor_header`, fuzz targets (`recover_dir` runs recovery on generated directories); `tests/soak.rs::soak` (corruption, truncation, zeroed commit words and damaged headers after the checkpoint, injected between restarts: every removed byte is found in `quarantine/`, a damaged middle segment is refused without changing a byte, and live readers return `Error::Discarded` before moving to the repaired history). Power loss itself: **untested** |
| C9 | Disk full while creating a segment | `append` returns an error for which `Error::is_storage_full()` is true. The journal stays consistent and readable; a later `append` retries the roll. A new writer can open a cleanly stopped journal on a full disk (the lock file's PID is best effort), but recovery states that need a new file (a missing successor, a quarantine copy, a repaired segment) fail with a storage-full error. A `checkpoint()` on a completely full volume succeeded on ext4, btrfs and tmpfs and **failed with `ENOSPC` on APFS** (from `fsync`), poisoning the writer as for any checkpoint error | Linux, macOS | `tests/disk_full.rs::injected_enospc_on_roll_is_reported_and_retried`, `::injected_link_failure_leaves_no_trace`, `::map_failure_during_roll_is_retried`; real full volumes: `::real_small_filesystem` (APFS, ext4, btrfs, tmpfs; `#[ignore]`d); injected failures of roll links and preallocations in a running journal, retried by the writer: `tests/soak.rs::soak` |
| C10 | Disk full while writing into an existing segment | On APFS, ext4 and tmpfs: no effect; segments are preallocated, so writing into them needs no new blocks. **On btrfs this does not hold**: with the volume full, the first write into a not-yet-written page of a preallocated segment kills the writer with `SIGBUS`; with copy-on-write disabled (`chattr +C`) the writer instead blocks inside the kernel's page-fault handler. Keep free space on btrfs volumes that hold journals | Measured: APFS (owner's Mac), ext4, btrfs and tmpfs (OrbStack VM, Linux 7.2.12) | `tests/disk_full.rs::full_volume_then_write_into_preallocated_segment` and `::full_volume_then_roll_into_prepared_spare` (macOS default path: a segment prepared in the background before the volume filled up, ADR 0022; writing into it never fails, while publishing it at the roll may report storage full as in C9) (`#[ignore]`d: need a small volume; fail on btrfs as described) |
| C11 | Corrupted bytes in a segment (storage error, outside modification) | Readers and `verify` detect corruption that breaks the CRC32C, length, sequence or chain checks and report it with the file and offset; they never return a record that fails validation. The writer quarantines (or, with `RecoveryPolicy::Fail`, refuses without changing a byte) a corrupt tail; it repairs by writing a new file, never by changing bytes a reader may have mapped. This includes a commit word zeroed in front of committed records, which readers see as the end of the journal: `verify` reports it (`EmptyCommitWord`) and recovery treats it as corruption, as long as a valid record, a final seal or a later segment follows it. Any damage after which no record validates and no final seal or later segment remains (the newest record's commit word zeroed, or the two newest, or a block from one record into the next one's header) is indistinguishable from a torn append (C2) and is zeroed like one (ADR 0035) | Linux, macOS | `tests/recovery.rs::malformed_checksum_is_quarantined_and_repaired`, `::malformed_length_is_rejected`; `tests/committed_tail.rs::zeroed_commit_word_after_checkpoint_is_quarantined`, `::zeroed_commit_word_at_first_record_after_checkpoint_is_quarantined`, `::zeroed_commit_word_in_a_sealed_segment_is_quarantined`, `::fail_policy_refuses_a_zeroed_commit_word_without_changing_a_byte`, `::live_readers_never_see_changed_bytes_across_the_repair`, `::verify_reports_a_zeroed_commit_word_before_recovery`, `::one_damaged_commit_word` (property test), `::zeroed_commit_word_of_the_newest_record_is_indistinguishable_from_a_torn_append`, `::damage_that_leaves_nothing_valid_after_it_cannot_be_detected`, `::damage_around_a_seal_is_resealed_or_preserved_never_lost`, `::zeroed_block_ending_a_sealed_segment_is_preserved`, `::crash_in_the_middle_of_a_repair`, `::verify_never_reports_a_healthy_journal_under_a_live_writer`; fuzz targets `parse_segment`, `decode_record`, `recover_dir`; `tests/soak.rs::soak` (`verify` reports each injected corruption at its file and offset); journals from v0.1.0–v0.4.0: `tests/compat.rs::corrupt_tail_of_old_journal_is_quarantined_or_refused` |
| C12 | A checkpoint fails | The error is returned and the writer stays poisoned (`Error::DurabilityLost` on later checkpoints). See "Errors are sticky" | Linux, macOS | `tests/checkpoint.rs::poisoned_after_failure` (injected error); `tests/soak.rs::soak` (injected failures in a running journal, then a restart) |
| C13 | A durable-only reader (`ReaderConfig::durable_only`) | Returns only records with a sequence number below the checkpoint marker's `next_seq`, so (under the storage assumptions) never a record that a power loss can remove; it waits while the next record is not covered | Linux, macOS | `tests/durable_only.rs::durable_only_reader_stops_at_checkpoint`, `::no_valid_checkpoint_returns_nothing`, `::durable_only_rolls_and_seeks`, `::checkpoint_boundary_at_seal_preserves_cursor`, `::next_timeout_waits_for_checkpoint`, `::durable_only_reader_process_stops_at_checkpoint`; `tests/soak.rs::soak` (durable-only reader processes never return a record that recovery later removes); journals from v0.1.0–v0.4.0: `tests/compat.rs::durable_only_readers_stop_at_the_checkpoint`. Power loss itself: **untested** |
| C14 | Retention runs, fails or is interrupted | Removes only a sealed prefix strictly before the checkpoint segment: never removes that segment or anything after it, never leaves a gap. An interrupted move leaves the original, a destination copy, or both; a partial copy never permits removing the original. Existing destinations are never replaced; conflicts require explicit resolution before retry | Linux, macOS | `tests/retention.rs::limits_and_checkpoint_boundary`, `::checkpoint_segment_and_newer_are_kept`, `::concurrent_archiver_and_missing_middle`, `::age_uses_successor_and_never_skips_oldest`, `::apply_revalidates_checkpoint_and_identity`, `::move_and_conflict_stop_at_oldest`, `::injected_cross_device_copy_and_interrupted_sync`, `::interrupted_move_preserves_original_and_retries`, `::retention_kill_loop_50` (50 SIGKILLs, ignored); reader behavior: `::reader_discarded_at_removed_segment_end`; `tests/soak.rs::soak` (every segment retention moves is a sealed prefix of valid records, while the writer is killed and restarted). Power loss itself: **untested** |

## Readers, the writer and memory safety

- **`Reader::open` (safe, the default)** doesn't map the files. It reads them with `pread` into a buffer it owns and
  validates the copy, so it is memory-safe whatever other processes do to the files, and it can't receive `SIGBUS`.
  At worst it reports corruption.
- **`Reader::open_mapped` (`unsafe`, zero-copy)** hands out `&[u8]` slices pointing into the shared mapping. The caller
  promises that no process other than a tickertape writer modifies the journal's segment files while the reader
  exists. A correct writer never modifies committed bytes and repairs by writing new files, so this holds in normal
  operation, including writer crashes and recovery. Recovery zeroes bytes in place only after an empty commit word
  with no committed records after it, where after a crash no reader can hold a slice. If something else damages the
  file (which breaks the promise) so that no record validates and no final seal or later segment remains after an
  empty commit word (the newest record's commit word zeroed, the two newest records' commit words zeroed, or a block
  zeroed from one record into the next one's header), recovery can't tell it from a torn append and zeroes those
  records in place: a reader holding any of them sees its bytes change (C11). Each record is first validated from a copy made with atomic
  loads, and a slice into the mapping is created only after it passes, so a corrupted length cannot make a slice
  cover bytes the writer is still appending (except through a CRC collision on corrupted data, about 2^-32).
- **The writer** maps the segments it writes. Its API is safe, following the Rust standard library's position that
  its safety guarantees "only cover what the program itself can do, and not what entities outside the program can
  do to it" (`std::os::unix::io`, on `/proc/self/mem`). Another process writing into or truncating the files the
  writer has mapped is such an outside entity, and can crash or corrupt the writer process.

## Known risks and limits

- **SIGBUS.** The writer and `open_mapped` readers access segments through memory maps. If a segment file is
  truncated by another process, or the storage returns an I/O error on a page fault, the process receives `SIGBUS`
  and dies. tickertape does not catch it. Don't truncate or edit segment files while the journal is in use.
- **btrfs and a full disk.** Preallocation does not protect writes on btrfs: on a full volume they fail with
  `SIGBUS`, or hang with `chattr +C` (C10, measured). APFS behaved like ext4 in the same test. Monitor free space.
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
