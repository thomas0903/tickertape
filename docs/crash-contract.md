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
| C1 | Writer process dies (`kill -9`, panic, abort) at any point | Every committed record is intact and readable after restart. A record being appended is entirely present or entirely absent. The next writer continues at the next sequence number, with no gap and no duplicate | Linux, macOS | `tests/kill_loop.rs::kill_loop_250` (250 `SIGKILL`s at random points, with a live safe reader and a live zero-copy reader across all of them; 20 in the default test run); `tests/soak.rs::soak` (kills during appends, rolls, checkpoints, retention and recovery, for minutes to hours, with six reader processes; checks every record whose `append` returned survives); journals left by killed v0.1.0–v0.4.0 writers: `tests/compat.rs::current_writer_recovers_appends_and_checkpoints` |
| C2 | Writer process dies with a partly written record, or the caller's fill closure panics | The partial bytes are never visible to readers. The writer (or the next one) zeroes them and makes the zeroing durable before committing anything after them, so no stale bytes can later pass as a record | Linux, macOS | `tests/recovery.rs::torn_final_record`, `::panic_in_fill_then_shorter_append` (simulated crash state); process kill: `tests/kill_loop.rs`, `tests/soak.rs::soak`; a torn append left by each released writer: `tests/compat.rs::current_writer_recovers_appends_and_checkpoints` |
| C3 | Writer process dies while holding the lock | The OS releases the `flock` when the process dies; a new writer can open immediately. No stale-lock cleanup is needed. One exception: the lock is released when the last copy of its descriptor closes, and a child that the process forked (from any thread) holds a copy until it calls `exec` or exits. The journal stays locked until then: briefly for a child that is about to run a program, for its whole life for one that never execs (a pre-fork worker; Python `multiprocessing` with the fork start method), and the same for a grandchild (see the fork note below) | Linux, macOS | `tests/lock.rs::stale_lock_after_kill`; and every iteration of `tests/kill_loop.rs` |
| C4 | Writer process dies during a roll | No committed record is lost. The next writer completes the roll (`format.md` → Recovery). Readers waiting at the end of the old segment continue once the seal is written | Linux, macOS | `tests/recovery.rs::crash_between_link_and_seal` (simulated crash state); process kill: `tests/kill_loop.rs` (64 KiB segments, so kills land in rolls), `tests/soak.rs::soak` |
| C5 | A second writer tries to open the same journal | It fails with `Error::Locked` and leaves the journal's contents unchanged (it reads `writer.lock` for the PID it reports). Once the first writer is dropped in the process that opened it, or an open fails after taking the lock, the journal can be reopened at once, even while a child forked by that process still holds a copy of the lock's descriptor; a forked child dropping its copy of the writer does not release the parent's lock (see the fork note below) | Linux, macOS | `tests/lock.rs::second_writer_fails`; with a forked child: `writer::tests::drop_releases_the_lock_while_a_forked_child_holds_a_copy`, `::a_failed_open_releases_the_lock_while_a_forked_child_holds_a_copy`, `::a_forked_child_dropping_its_copy_keeps_the_parents_lock`, `::a_forked_child_dropping_its_copy_closes_the_lock_before_the_helper` |
| C6 | A reader process dies, or is slow | No effect on the writer or other readers. The writer never waits for readers. A slow reader falls behind; without retention disk use grows, and with retention it may receive `Error::Discarded` at a removed segment boundary | Linux, macOS | `tests/slow_reader.rs::stalled_reader_in_process_catches_up`, `::slow_reader_process_does_not_block_writer`; with retention: `tests/retention.rs::reader_discarded_at_removed_segment_end`; reader processes overtaken by retention while the writer is killed and restarted: `tests/soak.rs::soak` |
| C7 | OS crash or power loss **after** a successful `checkpoint()` | Every record appended before that checkpoint is intact after reboot, under the storage assumptions above, and unless a checkpoint, or the sync that zeroes an abandoned write, has failed earlier in the same boot | Linux, macOS | **Untested.** Process-kill tests cannot show this |
| C8 | OS crash or power loss, records **not** covered by a checkpoint | Any of them may be lost, including records readers already saw. Recovery keeps the longest valid chain starting at the last checkpoint, never accepts a record that fails validation, and moves (never deletes) anything after the first invalid position to `quarantine/` | Linux, macOS | Validation and quarantine: `tests/recovery.rs::disconnected_segment_is_quarantined`, `::dangling_checkpoint_is_refused`, `::missing_middle_segment_is_refused`, `::fail_policy_refuses_invalid_successor_header`, fuzz targets (`recover_dir` runs recovery on generated directories); `tests/soak.rs::soak` (corruption, truncation and damaged headers after the checkpoint, injected between restarts: every removed byte is found in `quarantine/`, a damaged middle segment is refused without changing a byte, and live readers return `Error::Discarded` before moving to the repaired history). Power loss itself: **untested** |
| C9 | Disk full while creating a segment | `append` returns an error for which `Error::is_storage_full()` is true. The journal stays consistent and readable; a later `append` retries the roll. A new writer can open a cleanly stopped journal on a full disk (the lock file's PID is best effort), but recovery states that need a new file (a missing successor, a quarantine copy, a repaired segment) fail with a storage-full error. A `checkpoint()` on a completely full volume succeeded on ext4, btrfs and tmpfs and **failed with `ENOSPC` on APFS** (from `fsync`), poisoning the writer as for any checkpoint error | Linux, macOS | `tests/disk_full.rs::injected_enospc_on_roll_is_reported_and_retried`, `::injected_link_failure_leaves_no_trace`, `::map_failure_during_roll_is_retried`; real full volumes: `::real_small_filesystem` (APFS, ext4, btrfs, tmpfs; `#[ignore]`d); injected failures of roll links and preallocations in a running journal, retried by the writer: `tests/soak.rs::soak` |
| C10 | Disk full while writing into an existing segment | On APFS, ext4 and tmpfs: no effect; segments are preallocated, so writing into them needs no new blocks. **On btrfs this does not hold**: with the volume full, the first write into a not-yet-written page of a preallocated segment kills the writer with `SIGBUS`; with copy-on-write disabled (`chattr +C`) the writer instead blocks inside the kernel's page-fault handler. Keep free space on btrfs volumes that hold journals | Measured: APFS (owner's Mac), ext4, btrfs and tmpfs (OrbStack VM, Linux 7.2.12) | `tests/disk_full.rs::full_volume_then_write_into_preallocated_segment` and `::full_volume_then_roll_into_prepared_spare` (macOS default path: a segment prepared in the background before the volume filled up, ADR 0022; writing into it never fails, while publishing it at the roll may report storage full as in C9) (`#[ignore]`d: need a small volume; fail on btrfs as described) |
| C11 | Corrupted bytes in a segment (storage error, outside modification) | Readers and `verify` detect corruption that breaks the CRC32C, length, sequence or chain checks and report it with the file and offset; they never return a record that fails validation. The writer quarantines (or, with `RecoveryPolicy::Fail`, refuses) a corrupt tail and repairs it by writing a new file. In place, recovery only zeroes the bytes after the first empty slot and writes a seal there. Damage that empties the commit word of a committed record after the checkpoint is therefore different: the records from there on look never committed, and recovery zeroes them in place, in the same file, so readers can't see that repair (no identity changes). A durable-only reader is unaffected (C13); a visible reader may return the removed records, then continue into the new history without `Error::Discarded`, or, when its old position falls inside a record of the new history, wait there or report that history as corrupt ("Readers after a repair" below; `format.md` → What readers do about repairs); a payload a zero-copy reader still holds for such a record can change ("Readers, the writer and memory safety" → Known limitation) | Linux, macOS | `tests/recovery.rs::malformed_checksum_is_quarantined_and_repaired`, `::malformed_length_is_rejected`; fuzz targets `parse_segment`, `decode_record`; `tests/soak.rs::soak` (`verify` reports each injected corruption at its file and offset); journals from v0.1.0–v0.4.0: `tests/compat.rs::corrupt_tail_of_old_journal_is_quarantined_or_refused` |
| C12 | A checkpoint fails | The error is returned and the writer stays poisoned (`Error::DurabilityLost` on later checkpoints). See "Errors are sticky" | Linux, macOS | `tests/checkpoint.rs::poisoned_after_failure` (injected error); `tests/soak.rs::soak` (injected failures in a running journal, then a restart) |
| C13 | A durable-only reader (`ReaderConfig::durable_only`) | Returns only records with a sequence number below the checkpoint marker's `next_seq`, so (under the storage assumptions) never a record that a power loss can remove; it waits while the next record is not covered. A marker speaks about the journal's current files, so the reader applies a marker it has just read only to a segment whose identity it confirmed after reading it (ADR [0034](decisions/0034-durable-only-reads-check-segment-identity.md)): when a marker moves past its cursor and recovery has replaced, or retention or recovery removed, the segment it has open, it returns `Error::Discarded` instead of reading on in that file. It reads every record a new marker covers from the file's current bytes (the safe reader discards what it buffered before the marker), so it also never returns a record that recovery removed after the checkpoint in place, in a file that kept its identity (damage that emptied a commit word, C11). It therefore never returns a record that recovery removed, even when its open file, or its read buffer, holds valid bytes where the file recovery read was damaged; the exception is damage that also destroys the checkpoint marker, after which recovery may zero records an earlier marker covered (C11) | Linux, macOS | `tests/durable_only.rs::durable_only_reader_stops_at_checkpoint`, `::no_valid_checkpoint_returns_nothing`, `::durable_only_rolls_and_seeks`, `::checkpoint_boundary_at_seal_preserves_cursor`, `::next_timeout_waits_for_checkpoint`, `::durable_only_reader_process_stops_at_checkpoint`; segment replaced by recovery (a repair injected at each point around the marker read and the identity check): `::durable_only_reader_never_returns_records_removed_by_a_repair`, `::repair_before_the_marker_read_is_discarded`, `::repair_between_the_marker_read_and_the_identity_check_is_discarded`, `::repair_after_the_identity_check_is_seen_at_the_next_marker`, `::second_repair_before_the_identity_check_is_discarded`; records removed in place (the middle and the newest uncheckpointed record's commit word zeroed): `::durable_only_reader_rereads_the_tail_after_an_in_place_repair`; with a live writer restarted every few checkpoints with corruption injected between restarts (a corrupt copy renamed over a segment, or a commit word zeroed in place), `tests/reader_retention_stress.rs::durable_readers_under_repairs` (one minute with `--ignored`); `tests/soak.rs::soak` (durable-only reader processes never return a record that recovery later removes); journals from v0.1.0–v0.4.0: `tests/compat.rs::durable_only_readers_stop_at_the_checkpoint`. Power loss itself: **untested** |
| C14 | Retention runs, fails or is interrupted | Removes only a sealed prefix strictly before the checkpoint segment: never removes that segment or anything after it, never leaves a gap. An interrupted move leaves the original, a destination copy, or both; a partial copy never permits removing the original. Existing destinations are never replaced; conflicts require explicit resolution before retry. Readers racing with a removal, in any process, get `Error::Discarded` from reads and `Error::SeqUnavailable` (with the new oldest sequence number) from `seek`; `Reader::open` starts at the oldest segment that remains. They never get a corruption error, or `Error::NotFound` from a directory listing that a removal left empty (btrfs omits entries created after a listing starts), because of a removal; nor an I/O "not found" error for a segment they had just listed, unless the segment they chose vanished on each of 8 listings in a row (`Reader::seek` documents this bound). Only "not found" counts as a removal: a `stat` that fails otherwise (`EACCES`, `EIO`) is an I/O error, not `Error::Discarded`. A reader inside a removed segment keeps reading the file it holds and gets `Error::Discarded` at its end; a durable-only reader may get it earlier, when the checkpoint moves past its cursor (C13, ADR [0034](decisions/0034-durable-only-reads-check-segment-identity.md)), and gets it at the end too when the checkpoint names a later segment but doesn't move past its cursor (the successor holds no committed record yet), checking at most every 100 ms like a visible reader at an empty slot. `inspect::list_segments` and `inspect::verify` (CLI `inspect`, `verify`) leave out segments removed before they open them, and report no problem for those; a segment already open when it was removed can still appear in the result, with what was read from it (`verify` reports no broken chain for its missing successor). The retries are bounded: a directory listing that is still empty on the eighth attempt gives `Error::NotFound`, and a listed segment that vanished on each of 8 listings an I/O error | Linux, macOS | `tests/retention.rs::limits_and_checkpoint_boundary`, `::checkpoint_segment_and_newer_are_kept`, `::concurrent_archiver_and_missing_middle`, `::age_uses_successor_and_never_skips_oldest`, `::apply_revalidates_checkpoint_and_identity`, `::move_and_conflict_stop_at_oldest`, `::injected_cross_device_copy_and_interrupted_sync`, `::interrupted_move_preserves_original_and_retries`, `::retention_kill_loop_50` (50 SIGKILLs, ignored); reader behavior: `::reader_discarded_at_removed_segment_end`, `tests/durable_only.rs::durable_only_reader_overtaken_by_retention_is_discarded_when_the_checkpoint_moves`, `::durable_only_reader_at_a_removed_segment_end_with_an_empty_successor_is_discarded`; readers racing with a removal (injected at the racy point): `::reader_open_races_retention`, `::seek_races_retention`, `::seal_races_retention`, `::inspection_races_retention`, `::stat_errors_are_not_mistaken_for_removal`, and with a live writer, `tests/reader_retention_stress.rs::readers_under_retention` (safe, zero-copy and durable-only readers, seeking and reopening readers, and inspection against `max_segments(2)` retention; one minute with `--ignored`); `tests/soak.rs::soak` (every segment retention moves is a sealed prefix of valid records, while the writer is killed and restarted). Power loss itself: **untested** |

## Readers, the writer and memory safety

- **`Reader::open` (safe, the default)** doesn't map the files. It reads them with `pread` into a buffer it owns and
  validates the copy, so it is memory-safe whatever other processes do to the files, and it can't receive `SIGBUS`.
  At worst it reports corruption.
- **`Reader::open_mapped` (`unsafe`, zero-copy)** hands out `&[u8]` slices pointing into the shared mapping. The caller
  promises that no process other than a tickertape writer modifies the journal's segment files while the reader
  exists. Each record is first validated from atomic loads of the mapping (each byte loaded once, nothing copied;
  ADR 0027), and a slice into the mapping is created only after it passes. Segments are mapped only on x86-64 and
  aarch64; on other targets `open_mapped` reads with `pread` like `Reader::open`.

  These guarantees hold **for an intact publication chain**, where every commit word the reader follows is one a
  writer published. A correct writer then never modifies committed bytes: in place, recovery only zeroes the bytes
  after the first empty slot (never committed, on an intact chain) and writes a seal there, and otherwise replaces or
  moves whole files. So in normal operation, including writer crashes and recovery, the reader's loads race with no
  store, and a returned payload stays equal to the bytes validated.

  **Known limitation (commit words damaged on storage).** A damaged commit word breaks the chain, and a mapped load
  can then touch bytes a writer is storing to without synchronisation. If the writer runs in the same process that is
  a data race in Rust's terms, before any CRC runs; for a writer in another process the language says nothing. Two
  cases:

  - *An enlarged length.* If storage damage enlarges the length in an earlier record's commit word while no writer
    runs, and a writer later appends after that record, validating it makes the reader load bytes the writer is
    storing at that moment.
  - *A zeroed commit word.* If storage damage zeroes the commit word of a record that was published but not yet
    covered by a checkpoint, the next writer's recovery takes that slot for the end of the journal and zeroes that
    record **and the published records after it** in place, under a live zero-copy reader. A payload slice the caller still holds changes, and the reader's later commit-word loads meet the new
    writer's stores.

  That such loads return some mixture of old and new bytes, which the CRC then rejects, is an assumption about the
  implementation on the supported targets (aligned single-instruction loads), not a guarantee; how likely a chance
  match is depends on the corruption, and a retry can read the same bytes. Every earlier release, which copied the
  same range with the same atomic loads, had the same exposure. A separately synchronised publication frontier, a
  planned protocol change, would bound the loads in the first case; it does not close the second, where the bytes
  change because recovery rewrites them (ADR 0027 → "Which loads can race").
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
  boundary; a durable-only reader also when the checkpoint moves past its cursor); it never silently continues into
  the repaired history. Until then a visible reader may return records that recovery removed, like records lost to a
  power loss (`tests/durable_only.rs::visible_reader_after_a_repair_is_discarded_at_the_old_end`). A durable-only
  reader doesn't: it returns only records covered by a marker that it applied to a file it had just confirmed was
  current, reading them from the file's current bytes (C13).
  A repair in place (storage damage emptied a commit word after the checkpoint, so recovery zeroes the records from
  there on in the same file, C11) can't be noticed: the file keeps its identity. A visible reader may then return
  the removed records it had already read or buffered and continue in the new history without `Error::Discarded`;
  if its position falls inside a record of the new history it waits there (`Ok(None)`) or reports
  `Error::Corrupt` for bytes that aren't corrupt, until the application seeks to its position. It returns a record
  that isn't in the journal only if the new history holds, at that position, bytes forming a frame with a valid
  checksum and the expected sequence number. Durable-only readers are unaffected (ADR
  [0034](decisions/0034-durable-only-reads-check-segment-identity.md) → In-place repairs).
- **Missing segments.** If the checkpoint marker's segment, or a segment in the middle of the chain, is missing,
  the writer refuses to open (under every recovery policy) rather than discard history on its own. A reader that
  reaches a seal whose successor is missing while its own segment is still in place, and still finds the seal when
  it reads that slot again from the file, reports `CorruptReason::BrokenChain`
  (`tests/retention.rs::missing_successor_with_segment_in_place_is_a_broken_chain`); if its own segment is gone too,
  retention removed both and it reports `Error::Discarded` (C14). If the seal is gone from the file, a repair in
  place removed it after the reader read it, or while the safe reader held it in its buffer (storage damage emptied
  a commit word earlier in that sealed segment, after the checkpoint, so recovery zeroed from there to the end of the
  segment, seal included, and quarantined the successor; `format.md` → What readers do about repairs): the journal is
  healthy, and the reader reads the slot as it is now, waiting there or continuing in the new history
  (`tests/durable_only.rs::visible_reader_does_not_follow_a_seal_an_in_place_repair_removed`); where the repair
  began earlier in the segment than the seal, its position may fall inside a record of the new history, as for any
  reader across an in-place repair (C11). A seal that is still there was stored by a live writer only after it
  linked the successor, so the reader tries the successor once more first; one report can still be transient, while
  a recovery that replaces a successor whose header is damaged keeps the seal and creates a new successor.
- **CRC32C** detects all burst errors up to 32 bits and most other corruption, not all of it. It is not a
  cryptographic integrity check.
- **Clocks.** Timestamps are caller-supplied data. tickertape never orders or validates by timestamp.
- **`flock`** works on local filesystems. On network filesystems it may silently fail to exclude other hosts; they
  are not supported.
- **The writer lock and `fork`.** The lock is a `flock`, which belongs to the open file description of
  `writer.lock`: closing a descriptor releases it only when the last copy of that descriptor closes, and a child
  forked by any thread holds a copy until it calls `exec` (the descriptor is close-on-exec) or exits. Dropping the
  writer in the process that opened it releases the lock explicitly (after its helper threads have stopped and
  readers have been told it has gone), and so does an open that fails after taking the lock, so neither waits for
  such a child. When the writer's process exits or dies without dropping the writer, the lock stays with the last
  copy of the descriptor: a child keeps the journal locked until it execs or exits, briefly for a child that is about
  to run a program, but for its whole life for one that never execs (a pre-fork worker; Python `multiprocessing` with
  the fork start method), and the same holds for a grandchild. A new writer sees `Error::Locked` meanwhile (C3). A
  forked child must neither use nor drop an inherited writer. If it drops one, the explicit unlock is skipped and the
  lock stays with the parent, and the child closes its copy of the lock descriptor first and neither joins the
  spare-segment thread nor removes a prepared spare. The rest of the writer's clean-up still runs in the child:
  stopping the prefault helper, whose thread does not exist there, can stall it, and clearing the wake-up region
  disturbs the parent's readers.
