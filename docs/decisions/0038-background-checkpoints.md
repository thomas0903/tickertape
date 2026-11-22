# 0038: A checkpoint that does not stop appends

- **Status:** accepted on 2026-11-20 by the maintainers under the owner's delegation. Its Linux results (the prototype,
  the finish round and the sustained session with background checkpoints) are recorded in `docs/benchmarks.md` →
  "2026-11-19 to 2026-11-22, sustained workloads on Google Cloud VMs" → Checkpoints and → Sustained session (see "After
  the Linux measurement")
- **Date:** 2026-11-20

## Context

`Writer::checkpoint(&mut self)` runs every sync call of a checkpoint (`crash-contract.md` → What `checkpoint()` does)
on the caller's thread, which is the writer's thread, so nothing can be appended until they return. Functional runs of
the sustained workloads (ADR 0026) on cloud Linux machines with local SSD (ext4 and xfs, 1,024-byte records at half
the probe rate, a checkpoint every second) showed the writer stopped for a few hundred milliseconds per checkpoint, and
the readers' p99.9 latency equal to that stall. The iceoryx2 recorder (ADR 0030) has the same problem. The roadmap item
"Cheaper checkpoints with prefaulting" asks for less checkpoint cost; a checkpoint that does not block appends is the
largest improvement available to sustained workloads with durability.

Constraints: `checkpoint()` keeps its meaning exactly; the on-disk format does not change; the core stays small and
dependency-free, and a thread it starts must be named, joined, and behave across `fork` as the writer's other helper
threads do (crash contract, "The writer lock and `fork`"); `unsafe` stays in `mmap.rs`; nothing is added to `append`.

## Options considered

1. **(a) The writer owns a checkpoint thread.** `request_checkpoint()` captures the frontier, hands the dirty state to
   the thread and returns; completion is polled; `checkpoint()` stays "wait for what is in flight, then checkpoint".
   The library starts one more thread, but it controls its whole life: the thread finishes before the lock is
   released, so a marker is never written by anything but the writer that holds the lock.
2. **(b) A `Send` checkpointer object the application runs on its own thread.** No library thread, but the object can
   outlive the writer (or be run after the writer was dropped and another opened the journal), so the writer would
   have to coordinate with it on drop anyway, or two parties could write the marker. The application must also write
   the loop, its shutdown and its error handling; every binding would expose that. **Rejected**: it moves the hard
   part (who may write the marker, when) to every caller without removing the coordination.
3. **(c) No concurrency, less work per call** (start write-back early, e.g. `sync_file_range(SYNC_FILE_RANGE_WRITE)`
   on Linux, so the blocking part is short). Cheap and keeps one thread, but a checkpoint has a floor that early
   write-back does not remove: on macOS two `F_FULLFSYNC` drive-cache flushes (data barrier and marker), on ext4 the
   journal commit that `fdatasync` forces, and on both the marker's own sync. **Not chosen as the alternative to
   (a)**, but the Linux measurement showed it matters as much as (a), so it is built under (a) as early write-back
   (below).

## Decision

**(a)**, in `crates/tickertape/src/checkpoint.rs`, with three changes that followed the Linux measurement: rolls no
longer sync on the writer's thread, early write-back on Linux, and placement of the helper threads (section "After
the Linux measurement").

**API** (new public items: `Writer::request_checkpoint`, `Writer::checkpoint_progress`, `Writer::close`,
`CheckpointProgress`):

- `request_checkpoint(&mut self) -> Result<u64>` captures the frontier (active segment, offset, next sequence number),
  moves the dirty list (ranges, files, directories) into a job, and queues it; it returns the `next_seq` the job covers.
  The first call starts the thread (`tickertape-checkpoint`). The calling thread takes one lock and moves a few small
  values; no system call, except when the request leaves a dirty list more than four segments long (Bounded open
  segments, below).
- `checkpoint_progress(&mut self) -> Result<CheckpointProgress>` reports `durable` (the marker this writer last made
  durable) and `pending` (the `next_seq` of the newest request not finished). One atomic load when nothing changed.
- `checkpoint()` waits for the jobs in flight, then checkpoints on the calling thread exactly as before (same steps,
  same function: `checkpoint::run`), and runs retention as before.
- `close(self) -> Result<()>` waits for the jobs in flight, drops the writer and reports whether every checkpoint of
  this writer succeeded. **Drop waits** too (it does not cancel): a sync in progress cannot be interrupted, the lock
  must be held until the last marker is written, and a requested checkpoint is then honoured; drop cannot report, so
  `close` exists to report.

**Coverage.** A job covers exactly what was dirty when it was requested: the marker names the frontier at the request,
and its data, names and barrier are synced before the marker is written (data before marker, as before). Records
appended while it runs are dirty for the next checkpoint. Rolls during a job: the rolled-past segment's range after
the frontier, its seal, the new segment and the directory entry go into the writer's new dirty list; the job's
directory list is the one at the request, which contains every segment name its marker can name. **One job runs at a
time**, in request order; a request made while one runs is queued, and requests made while one is queued join it
(one marker, covering the newest frontier). Marker generations are assigned at the request and written in that order,
so they increase strictly; `checkpoint()` waits for the queue before it takes the next generation.

**Shared state while appends continue.** The active segment's file is shared through an `Arc<File>`; the mapping is
synced through `mmap::Flush`, a handle in `mmap.rs` that, like the prefault `Advice`, holds the mapping alive and never
touches its bytes. `msync` on one thread while the writer stores into the same pages is sound in Rust's terms: `msync`
creates no reference, so the writer's `&mut` slices remain the only access in the process; the kernel writes the pages
as they are, and the bytes stored during the call lie after the frontier, which no marker claims (the reasoning is in
`Flush`'s documentation). Rolled-past segments stay open and mapped (through their `Flush`) until their job has synced
them. Retention: the writer's retention runs on the checkpoint thread after each successful job, so it is ordered with
the jobs (amended 2026-11-22 by ADR [0044](0044-retention-without-a-backlog-wait.md): the run stops before its next
removal when a job is queued or the writer waits for the thread, and the run after the next job continues it); any
retention (the writer's or `tickertape archive`) removes only sealed segments strictly before the marker on disk, and
every segment a job syncs is at or after that marker's segment, so retention cannot remove one (and the
job holds the file open in any case). The abandoned-write path syncs on the writer's thread under the same lock that a
job holds from its first sync to its marker barrier (`Context::lost`), and a failed sync on either side marks the
writer's data lost, after which no marker is written: on Linux a failed `fdatasync` can mark pages clean without writing
them and reports the error to one caller only. The prefault thread and the spare segment are unaffected (they belong
to the writer's thread and to the segments after the frontier). The notification word: the checkpoint thread publishes
through `mmap::NotifyPublisher`, a second handle on the same region, so the writer's own handle and `append` are
unchanged. Metrics (ADR 0032): `since_checkpoint` counts from when the job's marker became durable; `metrics()` reads
the checkpoint thread's results directly (the newest marker, `poisoned` as soon as a job failed, in-flight and failed
bytes in `uncheckpointed_bytes`); the marker file's modification time, which `status` uses, is the job's marker write.
Retention's report and last error reach the writer when it collects. `fork`: the thread does not exist in a child, so a
child that drops its copy of the writer leaves the thread's handle and lock alone (it may have been locked at the
fork); the parent's writer is unaffected (`writer::tests::a_forked_child_dropping_its_copy_leaves_the_checkpoint_thread_alone`).

**Failure.** A failed job poisons the writer as a failed `checkpoint()` does. Its error is returned once, by the next
call that reports on checkpoints (`request_checkpoint`, `checkpoint_progress`, `checkpoint`, `close`), and every later
one returns `Error::DurabilityLost`; `metrics().poisoned` is true from the moment it failed. A job queued behind a
failed one is dropped without running. Nothing is reported durable after a failure: `checkpoint_progress` returns an
error from then on. The error is lost only if the caller drops the writer without asking (`close` asks).

**Crash contract.** No new item: a crash during a background checkpoint is C1 (process) or C7/C8 (power), its failure
C12. The wording of "Two levels", "What `checkpoint()` does", "Errors are sticky", C1, C7, C12, C13 and C14 names the
background path and its tests. New fault points: `fault::Point::CheckpointBeforeMarker` (after the data, names and
barrier, before the marker), and `fault::hold_background`, which holds a background thread at a point while the test
appends and rolls; `CheckpointSync` and `CheckpointMarkerSync` work on the checkpoint thread with
`fault::fail_next_background`. The loom model gained the hand-over of the frontier to another thread that stores the
marker (`protocol_model::marker_stored_by_the_checkpoint_thread_covers_only_committed_records`). Format: unchanged.

**What Linux does to the writer during another thread's sync.** Known from the kernel's write-back path: writing a
dirty page back write-protects it again (`folio_clear_dirty_for_io` → `folio_mkclean`), so the writer's next store
into that page takes a write fault and calls the filesystem's `page_mkwrite`; the fault waits for the write-back to
finish only when the device or filesystem requires stable pages (`folio_wait_stable`; not the usual case for ext4 or
xfs on NVMe). `fdatasync` writes back the whole file's dirty pages, including those the Linux prefault helper dirtied
ahead of the writer (`MADV_POPULATE_WRITE`, ADR 0025). ext4's `page_mkwrite` starts a journal handle when it does not
take the delayed-allocation path, and a handle can wait while the journal commits; xfs converts unwritten extents at
I/O completion, in a transaction of its own. The measurement below found the writer's remaining waits elsewhere.

## Decisions on the open questions

The owner decided the questions this ADR left open:

- `close()` waits for the checkpoints in flight and reports how they ended; it does not checkpoint.
- No separate public call that waits without checkpointing: polling `checkpoint_progress` does that.
- Retention stays on the checkpoint thread, after each successful background checkpoint. (Amended 2026-11-22 by ADR
  [0044](0044-retention-without-a-backlog-wait.md): `checkpoint()`, `close()` and drop no longer wait for the whole
  run on the checkpoint thread, only for the removal in progress; a run also gives way to a queued job. A blocking
  `checkpoint()` still runs retention to the end on the calling thread afterwards.)
- Coalescing (a request made while one checkpoint waits joins it) is the semantics the bindings will expose.
- The abandoned-write path may wait for a background checkpoint (it takes the same lock); it is a rare path.

## After the Linux measurement

The owner ran the sustained comparison on cloud Linux machines (x86-64 and aarch64, local NVMe, ext4 and xfs; the
figures, first posted in the pull request, are recorded in `docs/benchmarks.md` → "2026-11-19 to 2026-11-22, sustained
workloads on Google Cloud VMs" → Checkpoints, not here). Background checkpoints removed the long stop of the writer at
each checkpoint, and showed three things to fix.

### Rolls without a sync

**What the sync was for.** A roll wrote the new segment's header, then called `fdatasync` on the writer's thread, then
linked the file under its name (`format.md` → Rolling). On ext4 that `fdatasync` waits for a journal commit, at every
roll, also with no checkpoint at all, and during a background checkpoint it waits behind that checkpoint's write-back.
The sync guaranteed one thing: **if the new segment's name is on disk, so are its header and its preallocated size**.
A process crash does not need it (the header is in the page cache before the link, and the next writer reads it from
there); it matters only for an OS crash or power loss. Item by item:

- **C7 (durable after a checkpoint).** A marker vouches for every segment from the scan start to the segment it
  names, so their headers must be durable before the marker. The checkpoint now provides that: every segment linked
  before a checkpoint's request is in its job (rolled-past segments in the dirty list, the active one last), and the
  job calls `fdatasync` on each of their files, which writes the header page and the metadata needed to read the
  data (size and extents), before it syncs the directory and writes the marker. A segment linked after the request
  is not named by that marker, and belongs to the next checkpoint, which syncs it in the same way. The blocking
  checkpoint runs the same steps.
- **C1 and C2 (process crash).** Unaffected: the header is written with `pwrite` before the link, so a crash at any
  later point leaves it readable to the next writer
  (`tests/background_checkpoint.rs::writer_killed_between_a_rolls_link_and_the_sync_of_the_new_segment`, which also
  kills with a background checkpoint in flight; and every kill test that rolls).
- **C8 (power loss, records not covered).** One new crash state: a segment after the marker whose name reached the
  disk while its header did not. Its first 64 bytes then read as zeros (the filesystem returns zeros for allocated
  blocks that were never written), or the file is shorter than its size if the preallocation did not reach the disk
  either. Recovery now handles it where the scan enters a segment (by a valid seal, or as the successor at the seal's
  place): an all-zero header is written again (base from the name, size from the file, creation time at recovery)
  and synced, and a file holding only zeros whose length is not a valid segment size is replaced by a new empty
  segment (`RecoveryAction::RestoredHeader`). The replacement is prepared and synced under a temporary name and
  renamed over the old file, as a tail repair is, so the name holds a segment for that base at every moment: an
  interruption leaves the old file, which the next open restores the same way, and at most a temporary file, which it
  removes (review round 1: unlinking first could leave a missing middle segment, which no later open accepts). The restored header is the one the roll wrote (the name gives the base,
  the file its size; only the creation time differs), so the scan then sees the state a durable header would have
  given and handles it by the existing rules: usually an empty commit word at the first record slot (the first
  records share the header's page), an uncommitted end; evidence of later records, or later segments disconnected,
  as before. Records after the marker may be lost (C8), as before; nothing a checkpoint covered is involved.
- **C11 (corruption).** A header that is entirely zero, in a segment the scan enters, is now taken for the crash
  state above, also under `Fail`; a damaged header that is not all zero is handled as before. Only an all-zero file
  is ever replaced, so no non-zero byte is removed by this. The segment the scan starts in (the marker's) is never
  restored: its header was synced before the marker, and an invalid header there still makes the open fail. Under
  `Fail`, an open that refuses restores nothing (amended after the fuzzer found a refused open that had
  replaced an empty successor and sealed the segment before it, then refused at a disconnected segment): from the
  first segment whose header it would restore, it holds back that change and every change the scan decides after it
  (zeroing an uncommitted tail, a seal, a created successor), scans as if they had been made (a segment whose header
  is restored is mapped from its file as it is, since the scan does not read the header; a replacement or a created
  successor is prepared and synced under its temporary name and mapped from there), and makes them in the order
  decided once the open can no longer refuse, after the check for disconnected segments. A refused open, or one
  that fails while making them, removes the prepared temporary files it did not publish. Each held change is the one `Quarantine` makes at the same point (the scans
  differ only where `Fail` refuses), so an interruption between two of them leaves the journal that an interrupted
  `Quarantine` open leaves there, which the next open handles the same way (new fault point
  `fault::Point::BetweenHeldChanges`). What a refused open keeps from before that segment is unchanged (C11).
  `Quarantine` makes each change when it decides it, as before.
- **C9 (disk full while creating a segment).** Unchanged: preallocation happens before the link and fails there. An
  error writing the header back surfaces at the next checkpoint's `fdatasync` (C12).
- **C13 and C14.** Unaffected: durable-only readers read below the marker, and retention removes only segments
  before it.

**An archiver between a marker's write and its barrier** (review round 1; also true before this ADR). A checkpoint
writes its marker slot and then syncs it. An external `retention::apply` (`tickertape archive --delete`) that read the
new slot in between could remove the segments before it durably, and a power loss could then bring back the previous
marker, naming a removed segment (`DanglingCheckpoint`). `retention::apply` now syncs the marker file after reading the
slot that decides a removal and before removing anything (once per slot it has not synced yet); a sync covers every
write the read saw, so the slot it relies on is durable. The writer's own retention runs after its marker barrier and
does not sync again. Test: `tests/retention.rs::archiver_syncs_the_marker_before_removing` (new fault point
`fault::Point::RetentionMarkerSync`).

Until a writer has opened the journal after such a crash, a reader that follows the old segment's seal, and `verify`,
see an invalid header in that segment where a durable header would have shown an empty segment. The format is
unchanged; `format.md` → Rolling no longer lists the sync. Tests: `tests/recovery.rs::unwritten_newest_segment_is_restored_under_both_policies`,
`::empty_newest_segment_file_is_replaced`, `::unwritten_first_page_with_later_records_is_a_committed_tail`,
`::two_unwritten_segments_restore_the_first_and_disconnect_the_second`,
`::interrupted_replacement_of_an_unwritten_segment_leaves_no_gap` (fault points `fault::Point::Preallocate` and the new
`fault::Point::RestoreBeforeRename`), `::fail_refusing_before_an_unwritten_successor_changes_no_file`, and the kill
test above (fault point `fault::Point::RollAfterLink`).

The alternative, syncing on the thread that prepares the spare, would not remove the state: the header names the base
sequence number, known only at the roll, and the link must follow the header at once for readers to follow the seal.
So that thread could only sync after the link, and recovery would need the same rule.

### Early write-back

With the kernel's flusher forced to write back continuously, blocking checkpoints stopped the writer for a small
fraction of the time they took without it, and background checkpoints hardly at all. Option (c) is therefore built in
on Linux, as `WriterConfig::early_writeback` (default on; it needs `WriterConfig::prefault`, the switch for the
writer's helper threads):

- A helper thread, `tickertape-writeback`, calls `sync_file_range(SYNC_FILE_RANGE_WRITE)` over the whole pages behind
  the write position (never the page that holds it) each time the position has moved 1 MiB, and over the rest of a
  segment, up to the end of its seal, when the writer rolls past it. A new segment is followed from its first page,
  header included. The writer's thread only compares its offset with the next step after each append, and once per
  step takes a lock and wakes the helper: the call itself, which can block in the block layer's throttling, is off
  the writer's thread.
- **Step size.** 1 MiB spreads the cost of a call (a lock, a wake-up, a system call) over about a thousand 1 KiB
  appends and lets the kernel build large requests; it leaves a checkpoint at most one step of unwritten data per
  segment beyond what the kernel's own write-back left, a few milliseconds of appends at the rates this is for; and it
  starts each step's write-back long before the kernel's flusher would (it waits for `dirty_background_*` or
  `dirty_expire_centisecs`).
- **What is durable when does not change.** `SYNC_FILE_RANGE_WRITE` only starts writing: no metadata, no device cache
  flush, no promise. Records may reach the disk earlier than before, which C8 always allowed. Every checkpoint still
  runs all its steps. Without the wait flags the call neither checks nor advances the file's write-back error state,
  so a write-back failure it started is still reported to the checkpoint's `fdatasync`; and if the call itself
  returns an I/O error, the helper marks the writer's data lost (`Context::lost`, as a failed sync does) and stops.
  That error is reported like a failed background checkpoint's: once, by the next checkpoint (which writes no marker)
  or the next `request_checkpoint`, `checkpoint_progress` or `close`, whichever comes first, also when no checkpoint
  is pending; later calls return `DurabilityLost`. `close` joins the helper before it checks, so an error from a call
  the helper was making is not missed (`tests/background_checkpoint.rs::failed_early_writeback_fails_the_next_checkpoint`,
  `::failed_early_writeback_reaches_the_next_call_once`, fault point `fault::Point::EarlyWriteback`). Errors that mean
  "not supported here" (`EINVAL`, `ENOSYS`, `EOPNOTSUPP`, `ESPIPE`) stop the helper and change nothing else. A helper
  that has stopped, for either reason, is marked stopped and lets go of every file under the state's lock, and rolls
  hand it nothing more, so it keeps no descriptor or deleted segment open
  (`writer::tests::a_stopped_writeback_helper_is_handed_no_files`).
- **`fork`.** The helper does not exist in a forked child, so the child's copy of the writer neither stops, joins
  nor signals it. Between calls the helper keeps no reference of its own to a segment file (it follows the active one
  by a weak reference), so `Writer::release_after_fork` in the child drops the state's references to the files and
  closes their descriptors, provided the state's lock is free; a lock held at the fork stays held in the child, and
  the child then only tries it and leaves those files open until it exits
  (`writer::tests::a_forked_child_releasing_its_copy_closes_the_writeback_helpers_files`,
  `::a_forked_child_releasing_its_copy_leaves_a_writeback_lock_held_at_the_fork_alone`).
- Pages behind the writer are never stored into again, so starting their write-back does not make the writer take a
  write fault on them.
- **macOS:** not built. macOS has no call that starts write-back of a file range without waiting for it, and its
  checkpoint has a floor of two `F_FULLFSYNC` calls that early write-back would not remove. The option has no effect
  there.

### Helper threads and CPU affinity

A new thread inherits the CPU mask of the thread that creates it. The writer creates its helpers from the calling
thread: the prefault and write-back helpers in `Writer::open`, the checkpoint thread at the first
`request_checkpoint`, the spare thread during an append. A writer thread pinned to one CPU, as low-latency
applications pin it, therefore got its checkpoint thread on its own CPU, where the checkpoint's work delayed appends
for as long as it ran. `WriterConfig::helper_cpus(HelperCpus)` now places every helper thread:

- `HelperCpus::Process` (default): on Linux each helper sets its affinity when it starts to every CPU, minus those the
  kernel lists as isolated (`/sys/devices/system/cpu/isolated`), unless that would leave none. The kernel keeps only
  the CPUs the process's cpuset allows. A mask set with `taskset` or `sched_setaffinity` belongs to each thread and
  does not limit this; the rustdoc and `operations.md` say how to keep helpers inside one.
- `HelperCpus::Inherited`: the previous behaviour.
- `HelperCpus::List(cpus)`: those CPUs (within the cpuset); a mask the kernel refuses falls back to `Process`.
- macOS has no call to bind a thread to CPUs; the option has no effect there.

The prefault and spare threads had the same problem (the prefault helper's `MADV_POPULATE_WRITE` work ran on the
writer's CPU) and are treated the same way. `tt-sustained --helper-cpus` passes the option through, so the
measurement needs no `taskset`. Tests: `affinity::tests` and `writer::tests::helper_threads_of_a_pinned_writer_are_placed_as_configured`
(Linux; each helper's `Cpus_allowed_list` in `/proc`).

### What the benchmark records

`tt-sustained` gained `--no-early-writeback` and `--helper-cpus`, and records the system's dirty and under-write-back
page cache each second on Linux (`dirty_bytes`, `writeback_bytes` in the writer's samples; maxima in
`writer.page_cache`). A roll's cost on the writer's thread already shows as `roll_append_latency`.

## After the scale measurement

### Bounded open segments

`tt-scale` (ADR 0042) failed to open a writer on a journal of 10,000 segments without a marker: `EMFILE` at about
the thousandth segment. Recovery kept every segment after the marker in the writer's dirty list with its file open, for
the first checkpoint to sync, and so did every roll (file and mapping) until the next checkpoint, also in v0.4.0. A
writer that checkpoints rarely or never therefore held one more descriptor and one more segment mapping per roll, and
failed at the descriptor limit; the background checkpoint's job and the early write-back helper's list of rolled
segments held the same files.

Now a dirty list (`checkpoint::DirtySegments`) keeps the file and mapping handle of its four most recent segments
only; pushing a fifth releases the oldest held one, which stays in the list by absolute path, file identity (device
and inode, from an `fstat` before it lets go) and range. Rolls usually do the release; so does a request that leaves a
list longer than four, after many rolls since the previous request or when requests join a checkpoint queued behind a
slow one: that request may then drop the last reference to the released segment's file and mapping (a `close` and a
`munmap` on the calling thread), and the job reopens the segment later. Nothing is added to an append, and recovery's
inherited segments go into the same list. The background checkpoint's
job is such a list too (a queued job that absorbs later requests merges them through it), and the write-back helper
keeps at most four rolled segments, skipping the oldest one's early write-back if it falls that far behind (it is
only a head start). **Four** because a writer that checkpoints at least once per roll then does not reopen a segment (unless its
requests pile up behind a slow background checkpoint, above),
and because a few lists of four (the writer's, a running and a queued job's, the helper's) stay far below the usual
descriptor limits, whatever the segment count.

A checkpoint opens each released segment again by path, checks that it is the same file (device and inode), and
syncs it there; on macOS it also maps it again and calls `msync` on the range first. The journal directory is made
absolute at `Writer::open` for everything the writer opens later, so a writer opened with a relative path is not
redirected by a change of the working directory (`Writer::dir` still returns the path as given). Why that is enough,
and what a held descriptor gave that a reopened one does not (a write-back error the kernel forgets with an evicted
inode, or that another descriptor already observed, goes unreported), is in `docs/crash-contract.md` → Segments the
writer no longer holds. A released segment is after the marker, so retention leaves it; if it is gone at the
checkpoint, or replaced by another file under its name, the checkpoint fails as an I/O error before any marker and
poisons the writer. Under `RecoveryPolicy::Fail` the changes held back after a restored header (C11) keep only the
path and identity of their segment too, and reopen it, one at a time, when they are made. Tests: `tests/open_segments.rs` (300 rolls with a descriptor
limit of 64, without checkpoints and with background checkpoints every 25 rolls, then a durable-only reader reads
everything; recovery over more segments than the limit under both policies, and under `Fail` with a header to
restore and a seal to write in every segment; a removed released segment fails a blocking and a background checkpoint
and poisons the writer, and so does one replaced by a copy; a writer opened with a relative path checkpoints its own
segments after the process changes into a directory holding another journal with the same file names).

## Consequences

- One more thread per writer once `request_checkpoint` is used (none otherwise); drop can wait for a checkpoint.
- On Linux, one more helper thread by default (`tickertape-writeback`), started with the prefault helper; both are
  off with `prefault(false)`.
- A roll's append no longer waits for a sync; after an OS crash or power loss recovery may restore a header
  (`RecoveryAction::RestoredHeader`), and until a writer opens, readers and `verify` see that segment's header as
  invalid.
- Helper threads no longer inherit a pinned caller's CPU mask unless asked to (`HelperCpus::Inherited`).
- The abandoned-write path can wait for a background checkpoint to finish (it takes the same lock).
- Between checkpoints a writer holds at most four rolled or inherited segments open per dirty list; a checkpoint
  reopens the others, and fails if one is gone or replaced.
- The C ABI, the bindings, the CLI and the iceoryx2 recorder adopt the API once this ADR is accepted. Done for the C
  ABI, the C++, Python and Java bindings and the iceoryx2 recorder on 2026-11-20 (amendments to ADRs 0028, 0029,
  0030, 0036 and 0037): the same names, coalescing and failure reporting, and each binding's close reports as
  `close` does.
- An external `retention::apply` syncs the checkpoint marker once per new slot before it removes anything.
- Tests: `tests/background_checkpoint.rs` (C7, C12, C13, C14 with a checkpoint in flight; the roll kill test; early
  write-back failure), `tests/recovery.rs` (restored headers), `tests/retention.rs` (the archiver's marker sync), the
  fork, placement and stopped-helper tests in `writer.rs`, the loom
  model, and `benches/tests/tools.rs` for the benchmark options.
