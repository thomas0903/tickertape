# 0038: A checkpoint that does not stop appends

- **Status:** proposed (prototype; the design is finished after the Linux measurement below)
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
   journal commit that `fdatasync` forces, and on both the marker's own sync. Not measured on Linux yet (the local
   Linux machine is out of service); the commands to measure it are below. **Not built**; it can be added under (a)
   later (the checkpoint thread can start write-back of a request's ranges before it waits on them).

## Decision

**(a)**, as a prototype in `crates/tickertape/src/checkpoint.rs`.

**API** (new public items: `Writer::request_checkpoint`, `Writer::checkpoint_progress`, `Writer::close`,
`CheckpointProgress`):

- `request_checkpoint(&mut self) -> Result<u64>` captures the frontier (active segment, offset, next sequence number),
  moves the dirty list (ranges, files, directories) into a job, and queues it; it returns the `next_seq` the job covers.
  The first call starts the thread (`tickertape-checkpoint`). The calling thread takes one lock and moves a few small
  values; no system call.
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
the jobs; any retention (the writer's or `tickertape archive`) removes only sealed segments strictly before the marker
on disk, and every segment a job syncs is at or after that marker's segment, so retention cannot remove one (and the
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

**What Linux does to the writer during another thread's sync: known, and to measure.** Known from the kernel's
write-back path: writing a dirty page back write-protects it again (`folio_clear_dirty_for_io` → `folio_mkclean`), so
the writer's next store into that page takes a write fault and calls the filesystem's `page_mkwrite`; the fault waits
for the write-back to finish only when the device or filesystem requires stable pages (`folio_wait_stable`; not the
usual case for ext4 or xfs on NVMe). `fdatasync` writes back the whole file's dirty pages, including those the Linux
prefault helper dirtied ahead of the writer (`MADV_POPULATE_WRITE`, ADR 0025), so after every checkpoint, blocking or
not, the writer may fault again on up to a prefault window of pages. ext4's `page_mkwrite` starts a journal handle
when it does not take the delayed-allocation path (`data=journal`, delayed allocation off, or switched off as the
filesystem fills up), and a handle can wait while the journal commits (the commit `fdatasync` forces); xfs converts
unwritten extents at I/O completion, in a transaction of its own. Whether any of this makes the writer wait during a background checkpoint, for
how long, and how ext4 and xfs differ, is exactly what the prototype is for. **Nothing is claimed for Linux** until the
sustained comparison (`tt-sustained --checkpoint-mode blocking|background`, fields `checkpoint_pause`,
`append_latency`, `checkpoint_durable_latency` and the readers' latency) has been run on both.

## Consequences

- One more thread per writer once `request_checkpoint` is used (none otherwise); drop can wait for a checkpoint.
- The abandoned-write path can wait for a background checkpoint to finish (it takes the same lock); it is a panic path.
- The C ABI, the bindings, the CLI and the iceoryx2 recorder adopt the API once this ADR is accepted.
- Open questions for the owner: whether `close` should also checkpoint (today it only waits and reports, like drop);
  whether a `wait_checkpoint()` that waits without checkpointing the rest is worth a public item (polling
  `checkpoint_progress` does it now); whether retention should rather run on the writer's thread at the next poll
  (smaller change, but the writer then pays for it); whether early write-back (option c) is added under (a).
- Tests: `tests/background_checkpoint.rs` (C7, C12, C13, C14 with a checkpoint in flight), the fork test in
  `writer.rs`, the loom model, and `benches/tests/tools.rs` for the benchmark option.
