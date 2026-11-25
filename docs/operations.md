# Operating a tickertape journal

Practical guidance for running a writer in production-like settings. The guarantees themselves are in
[`crash-contract.md`](crash-contract.md); measured costs are in [`benchmarks.md`](benchmarks.md). This page only
draws conclusions from those two.

## Choosing a segment size

`WriterConfig::segment_size` (default 128 MiB; a multiple of 64 KiB) trades these against each other:

- **Rolls.** The append that fills a segment publishes the next one: write its header, then link it. On macOS with
  the default `prefault(true)` the next segment has been preallocated in the background (see "Writer threads");
  otherwise the roll also preallocates a new file. The roll does not sync the new segment; the next checkpoint does
  (ADR 0038). A roll append is still slower than other appends (see "roll appends" in `benchmarks.md`, measured
  before rolls stopped syncing). Larger segments roll less often.
- **Largest record.** A record must fit in one segment: at most `segment_size - 112` bytes of payload.
- **Disk reservation.** Every segment reserves its full size on disk when it is created, even if it is mostly empty.
- **Open time.** When the writer opens, it reads the part of the newest segment after the last record to clean up
  after a crash, so very large segments make opening slower.
- **Seek time.** `Reader::seek` opens the segment holding the target and reads every record before it in that
  segment, validating each, so larger segments make a seek slower (ADR 0042). A forward seek within the reader's
  segment reads only from the reader's position on, when an earlier seek of that reader found that position covered by
  the checkpoint (ADR 0046), so seeking forward through a checkpointed segment reads it once, not once for each seek. `Reader::seek_to_end` reads at most
  the newest segment, from the checkpoint on. For seeks into sealed segments, seek indexes make the cost independent of
  the segment size ("Seek indexes" below); they do nothing for the newest segment.
- **Segment count.** Opening a reader or a writer and each seek read the whole journal directory; `list_segments`,
  `verify` and each retention run (when retention is configured, at writer open and after checkpoints) open every
  segment's file; replay opens each segment it enters. These costs grow with the number of segments, so very small
  segments in a long-lived journal make them add up (ADR 0042). Measured on local SSD (`benchmarks.md`, 2026-11-25):
  journals of 10,000, 20,000 and 100,000 segments open, recover, replay and checkpoint, and no operation held more
  than 12 open files at 10 to 20,000 segments, so the limit on open files (`ulimit -n`) is not a constraint on the number
  of segments. The directory read is: each seek costs about 3 ms at 10,000 segments, 6 ms at 20,000 and 29 ms at 100,000,
  also with a seek index, and each retention run opens every header (29 ms at 10,000 segments, 67 ms at 20,000, 0.34 s
  at 100,000 with a warm cache; seconds when it is cold, as at the open of a writer after a reboot), which a writer with
  a retention policy pays at every checkpoint. Deleting a segment reads the directory again, about 8 ms each at 20,000
  segments. So plan for at most a few thousand segments in a journal with retention (a larger segment, or fewer kept),
  and prefer one large segment over many small ones where seeks or checkpoints matter.
- **Granularity of archiving.** You archive or delete whole segments (below).

For most uses the default is a reasonable start. Pick smaller segments (a few MiB) for low-volume journals you
archive often, larger ones if roll latency matters and disk space doesn't.

## Writer threads

With `WriterConfig::prefault(true)` (the default), the writer moves segment work off its own thread:

- **Page faults (Linux and macOS).** A helper thread, `tickertape-prefault`, keeps the pages just ahead of the write
  position faulted in, so `append` rarely waits for a page fault.
  - **Linux:** the next 256 KiB, with `MADV_POPULATE_WRITE` (ADR 0025; kernels before 5.14 stop the helper on first
    use).
  - **macOS:** the next 8 MiB, with `MADV_WILLNEED` (ADR 0020). On APFS, writing into a preallocated segment otherwise
    faults once per 16 KiB page, slowly.
- **The next segment** (`WriterConfig::spare_segment`; on by default on macOS, **off on Linux**, from 1.1). Once the
  active segment is half full, a thread, `tickertape-spare`, creates and preallocates `next-segment.tmp` (ADRs 0022
  and 0049), and the roll then only writes the header and links the file into place. Off, a roll creates its file
  itself.
  - **What it costs:** one more segment's worth of disk space from the time the active segment is half full ("Disk
    space" below), one more file descriptor and one more segment mapping. Dropping the `Writer` waits for a spare
    being prepared.
  - **Linux:** one thread for the writer's life. The spare is not synced; the checkpoint that first covers the segment
    syncs it, as it does a segment a roll creates itself (ADR 0038). A roll that finds its spare not ready creates the
    segment itself, without waiting, and the spare is used by the next roll; only if that fails for lack of space does
    it wait for the spare (with no time limit, as long as the preallocation takes) and use it, or try once more. Away
    from the disk's limit it made rolls cheaper in the recorded runs; near the limit it made them stall more often,
    and unpaced the mapped reader's p99.9 was worse with an earlier build of it (`docs/benchmarks.md` → "2026-11-25,
    roll and journal stalls"). Measure on your machine before turning it on.
  - **macOS:** a thread is started for each spare; it also syncs the file (preallocating and syncing a whole segment
    on APFS takes about 50 ms), and a roll waits for a spare that is still being prepared.
- **Early write-back (Linux).** A helper thread, `tickertape-writeback`, asks the kernel to start writing back the
  pages behind the write position (`sync_file_range(SYNC_FILE_RANGE_WRITE)`, every 1 MiB, and the rest of a segment
  when the writer rolls past it), so that a checkpoint, blocking or background, finds little dirty data left
  (`WriterConfig::early_writeback`, default on; ADR 0038). It makes nothing durable: only checkpoints do. An I/O
  error from it makes the next checkpoint fail, as a failed sync would. On macOS the option does nothing.

**ext4 under a load near what the disk takes.** A store into a page of a mapped file updates the file's modification
time through the ext4 journal and can wait while the journal commits, for as long as the disk takes to write the log
(50–140 ms appends in the recorded runs, `docs/benchmarks.md` → "2026-11-25, roll and journal stalls"). The mount
option `lazytime`, which keeps modification times in memory, removed those waits there, but the roll's own file
operations still waited and waited longer: the longest roll appends went from 0.2–59 to 114–130 ms. That is one
VM shape with three 60-second runs per setting, so it is an option to measure, not a recommendation. After a power
loss a file's modification time can then be older than its last write, and `tickertape status` takes the marker's
modification time as the checkpoint's time.

None of these threads ever writes records. Set `prefault(false)` if you want a writer that starts none (it also turns
off early write-back); `Writer::request_checkpoint` still starts the checkpoint thread.

**Pinned writers.** A new thread inherits the CPU mask of the thread that creates it, and the writer creates its
helpers from the calling thread (the checkpoint thread at the first `request_checkpoint`). So that a writer thread
pinned to one CPU does not get its helpers on the same CPU, each helper resets its own affinity on Linux when it
starts (`WriterConfig::helper_cpus`, ADR 0038):

- `HelperCpus::Process` (default): every CPU the process's cpuset allows, except the CPUs in
  `/sys/devices/system/cpu/isolated` (`isolcpus`), unless no other is allowed. A mask set with `taskset` or
  `sched_setaffinity` does not limit this, because it belongs to threads, not the process: to keep the helpers inside
  such a mask, name its CPUs or create the writer from a thread that has it and use `HelperCpus::Inherited`. A cpuset
  (cgroup, container CPU limit) does limit it.
- `HelperCpus::List(cpus)`: put the helpers on CPUs of your choice, for example a housekeeping CPU next to the
  writer's. A list the kernel refuses (no listed CPU in the cpuset) falls back to `Process`.
- `HelperCpus::Inherited`: the creating thread's mask, as before this option existed.

macOS has no call to bind threads to CPUs; the option does nothing there. `tt-sustained --helper-cpus` takes the same
choices.

**Sustained high byte rates on Linux.** Under steady write-back (minutes of writing, gigabytes of dirty pages), ext4
itself occasionally stalls the writer for milliseconds, with or without prefaulting (ADR 0025). Plan disk
throughput with headroom: a small cloud VM stalled for hundreds of milliseconds at about 100 MB/s sustained.

## Choosing a checkpoint interval

`Writer::checkpoint()` makes everything appended so far durable. Until then, records are visible and survive the
writer process crashing, but not a power loss (crash contract C1, C7, C8).

- **Cost.** A checkpoint takes milliseconds (see `benchmarks.md`; on macOS it ends with two `F_FULLFSYNC` calls).
  Don't checkpoint after every record unless you need that and can afford it.
- **Exposure.** After a power loss, everything since the last successful checkpoint may be gone, and its sequence
  numbers will be reused for new records.
- **Durable-only readers** (`ReaderConfig::durable_only`, `--durable-only`) see records only once a checkpoint
  covers them, so they lag the writer by up to one checkpoint interval.
- **Recovery time.** When the writer opens, it validates everything written since the last checkpoint; a journal
  that is never checkpointed is validated from the start every time.

A common pattern is a time- or count-based checkpoint (for example every second or every N records, whichever comes
first) plus one before a clean shutdown. Dropping a `Writer` does not checkpoint.

**Background checkpoints** (ADR 0038): `Writer::request_checkpoint()` starts a checkpoint of
everything appended so far on the writer's own checkpoint thread and returns at once, so appends go on while the sync
calls run; `Writer::checkpoint_progress()` reports when it is durable. Records appended after the request are not
covered. A failure is reported by the next of these calls (or `checkpoint()`, or `Writer::close()`), and poisons the
writer as above. Dropping the writer waits for the requested checkpoints; `close()` waits and reports how they ended
(it does not checkpoint). A request made while one checkpoint waits joins it. To wait for a checkpoint, poll
`checkpoint_progress()`. On Linux, combine background checkpoints with early write-back (the default) and keep the
checkpoint thread off a pinned writer's CPU (the default; "Pinned writers" above). The bindings have the same calls
with the same rules: in C `tickertape_writer_request_checkpoint` and `tickertape_writer_checkpoint_progress`
(`c-api.md` → Background checkpoints), in C++ `Writer::request_checkpoint` and `Writer::checkpoint_progress`, in
Python `Writer.request_checkpoint` and `Writer.checkpoint_progress`, in Java `Writer.requestCheckpoint` and
`Writer.checkpointProgress`; each binding's close reports a failure as `Writer::close` does. Early write-back and
the helper placement are writer options in each (`early_writeback` / `earlyWriteback`, `helper_cpus` /
`helperCpus`), and so is the spare segment (`spare_segment` / `spareSegment`, from 1.1).

If `checkpoint()` returns an error, the writer refuses all later checkpoints (`Error::DurabilityLost`) while appends
keep working. Treat it as an incident: see "Errors are sticky" in the crash contract.

## Disk space

Segments are never deleted automatically unless a writer retention policy is configured. Without retention or
`tickertape archive`, disk use only grows. Retention is best effort and depends on checkpoints: a writer that never
checkpoints never frees space, and the checkpoint segment and newer segments always remain.

- Monitor free space on the journal's filesystem. A failed roll returns an error for which
  `Error::is_storage_full()` is true; the journal stays consistent and the append can be retried once space is back.
- **btrfs:** on a full btrfs volume, writes into an already preallocated segment can end the writer process with
  `SIGBUS` (or block it, with copy-on-write disabled), measured in crash contract C10. Keep clear headroom there.
- **The spare segment** (`WriterConfig::spare_segment`, on by default on macOS, off on Linux) takes one more segment's
  worth of space as soon as the active segment is half full. Leave room for it: where retention keeps N segments,
  plan for N + 1 (a spare that does not fit is given up, and the roll creates its segment as usual). `storage.estimated_rolls_left` and `--min-headroom-segments` count free space, so their thresholds are
  reached one segment earlier from half full on (a prepared spare with allocated blocks shows as
  `storage.spare_segment`). A roll that lacks space for its own segment uses the spare instead of failing.
- **APFS:** a `checkpoint()` on a completely full volume fails with `ENOSPC`, which poisons the writer (C9).

## Archiving and deleting old segments

Use `tickertape archive` for an explicit run, or configure a writer retention policy for long-running recorders:

```sh
# Inspect the plan, then move old segments into an existing archive directory.
tickertape archive ./journal --to ./archive --keep-segments 10 --dry-run
tickertape archive ./journal --to ./archive --keep-segments 10
# Or permanently delete a sealed prefix older than 30 days.
tickertape archive ./journal --delete --older-than 30d
```

```rust
use tickertape::{WriterConfig, retention::{Action, Policy}};
let config = WriterConfig::default().retention(
    Policy::default().max_segments(10).max_bytes(1024 * 1024 * 1024),
    Action::Delete,
);
```

The writer runs retention once after recovery on open and after every successful checkpoint. After a background
checkpoint the run is on the checkpoint thread and gives way, after the removal in progress, to `close`, and to the
next checkpoint once it has removed as many segments as the checkpoint before it covered or has taken as long as that
checkpoint, so that it keeps level with the writer even when checkpoints take longer than the interval between them,
unless removing segments is slower than the checkpoints; the run after the next checkpoint
continues it, so a backlog shows as `segments.before_checkpoint` staying high in `tickertape status`, not as slower
checkpoints (ADR 0044). Segments at or after the checkpoint segment are never removed, so a writer whose checkpoints
fall behind (it writes faster than the disk) also holds what it wrote since the last durable marker (`exposure`).
Retention errors
never fail open, append or checkpoint and never poison the writer. Check `Writer::last_retention_error()` for the
most recent error (retained even after later successes), and `Writer::retention_report()` for cumulative counts,
including removals completed before a subsequent failure. Neither is part of the recovery report.

The same rules apply to the library and CLI, without taking the writer lock:

- Remove **from the oldest end only**, stopping at the first ineligible segment or failure. Never skip a file or
  remove a middle segment, since that leaves a gap in the chain.
- Only sealed segments **strictly older than the checkpoint segment** qualify. Without a valid checkpoint marker
  nothing is eligible; a dangling marker is an error. Never remove the marker's segment or anything newer.
- A deletion checks the segment's seal from the end of the file and does not read its records; a move reads and
  validates the whole segment first (crash contract C14). Run `tickertape verify` first to find damaged records
  before deleting them.
- Limits combine with OR: keep at most N total segments, at most B total segment file bytes, or remove segments
  sealed longer ago than D. Seal time is the successor header's creation time, not file modification time.
  Counts and bytes include the active segment. Checkpoint eligibility can prevent reaching the requested limit.
  The cost of a run grows with the number of segments left in the journal ("Segment count" above).
- Moves preserve the segment basename and require an existing destination directory. Same-filesystem moves use
  an atomic no-replace rename. `EXDEV` selects copy, sync the copy, sync the destination directory, remove the
  source, sync the journal directory. Existing files (including symlinks and identical copies) are errors.

An interrupted cross-filesystem move can leave an original, a copy, or both; a partial copy never permits source
removal. On retry, an existing destination remains a conflict. Inspect it and explicitly resolve the conflict
(or choose a fresh destination) before rerunning; tickertape never overwrites it. Completed earlier removals stay
in effect if a later segment fails. A concurrent archiver's missing segment is treated as already removed only
when it was at the oldest end. `retention::apply` revalidates plans against the current journal before removal.
Segments are ordinary files named after their first sequence number (`00000000000000001234.tape`); you can also
copy sealed segments without changing the journal.

Readers that are still inside a segment you delete keep reading it (they hold the file open), but when they reach
its end they return `Error::Discarded`, because the file is gone; seek to their position to continue. Durable-only
readers may return it earlier, when the checkpoint moves past their position (ADR 0034); seeking to that position
then fails with `Error::SeqUnavailable`, because the records there are gone, so continue at the oldest sequence
number it reports. `tickertape tail` does both automatically, and warns on standard error with the range of records
it skipped. Readers positioned before the oldest remaining segment get `Error::SeqUnavailable` from `seek`, also when
the removal happens while `seek` runs. `Reader::open` starts at the oldest segment that exists. A removal never shows
up as a corruption error in readers, `tickertape inspect` or `tickertape verify`, and shows up as an I/O "not found"
error only if the segment a reader chose vanished on each of 8 listings in a row (crash contract C14).
A `stat` that fails other than with "not found" (`EACCES`, `EIO`) is reported as an I/O error, never taken for a
removal.

Never truncate or edit a segment in place while anything uses the journal: processes that map it can receive
`SIGBUS`.

## Seek indexes

A seek reads and validates its segment from the first record to the target. With a seek index it starts less than 64 KiB
of records before the target, in any sealed segment (ADR [0045](decisions/0045-sparse-seek-index.md)). The index files
are optional and derived; building them and using them are separate decisions.

**Building.** Run `tickertape index` from a timer, as the user that owns the journal:

```sh
# every few minutes, beside the running writer; segments that already have an index cost a few small reads
tickertape index /data/journal
# in an archive, after each `tickertape archive --to`
tickertape index /data/archive
tickertape index /data/journal --check     # reads every segment and compares every entry
tickertape index /data/journal --remove    # deletes them
```

- It indexes the sealed segments before the checkpoint marker's segment, so a writer that never checkpoints gets none
  while it runs. Newly sealed segments have none until the next run, and the newest segment never has one: a consumer that
  restarts near the head gains nothing (`seek_to_end` starts at the marker).
- The size is 24 bytes per 64 KiB of segment (0.04 %); a build reads each new segment once. Nothing is added to the writer
  while it runs; its open lists `seek-index/` and removes what recovery could invalidate.
- In a journal of 10,000 to 20,000 segments a run with nothing new to index opens every segment and index: 0.1 to
  0.2 s with a warm cache, 3 to 12 s when it is cold (`benchmarks.md`, 2026-11-25). The index shortens a seek that
  crosses much of a segment (128 MiB: from 0.18 s to about 1 ms, cold), not the directory read that every seek starts
  with, so it does not help where segments are small and many.
- The directory `seek-index/` is created with the journal directory's group and mode. If the command refuses to create it
  (the journal directory belongs to another user, is writable by everyone, or its group is not one of this user's, as for
  a directory made under `/tmp` on macOS), create it yourself with the journal directory's owner, group and mode.
- A build that is killed leaves a temporary file. The next open of a writer removes it; in an archive, where no writer
  opens, `tickertape index --check` reports it and `--remove` clears it (with the indexes: run `index` again).
- Delete the files whenever you like. Do not delete the directory while a 1.0.0 `export --output` runs: that fails with
  exit 1 and must be run again. A separate mount at `seek-index/` is not supported.
- Retention and `archive` remove a segment's index with the segment. `archive --to` does not move it: run `index` there.
- A 1.0.0 writer that repairs the journal does not remove outdated indexes. Readers check every index by value all the
  same, and the next `tickertape index` replaces an outdated one.

**Using.** Readers use them only with `ReaderConfig::seek_index(true)` (`--seek-index` in the CLI, and the same option in
each binding). It is off by default because it gives something up: records between the segment's start and the place the
seek lands are not read, so damage there no longer stops the reader. Run `tickertape verify` as before to find it. Do not
enable it where someone who may not write the segments can write `seek-index/` (crash contract C15). A seek never fails
because of an index: without one, or with one that fails a check, it scans. Mapped and durable-only readers use one only
before the checkpoint marker's segment, so in an archive, which has no marker, only the safe visible reader does.

## Moving a tape to another machine

A journal is its `*.tape` files and `checkpoint`; nothing in them depends on the machine they were written on
(`format.md`). Copy those files. The other entries of the directory (`format.md` → Journal directory) are not needed
to read or continue the journal: `writer.lock` and `notify` describe a writer on the old machine, and a new writer
creates its own. `quarantine/`, if present, holds bytes that recovery removed; keep it if they matter to you. Seek
indexes (`seek-index/`) keep working in a copy that preserves owner and mode; readers ignore one that a umask of 0002 made
group-writable, or that belongs to another user than the segments, without saying so (`tickertape index --check` does).
Or leave them behind and run `tickertape index` on the copy. The
commands below
were run with tickertape 1.0.0 from macOS (aarch64, APFS) to Linux (aarch64, btrfs, a local VM) over SSH, with 1 MiB
segments; the first recipe's copy, checksum and `verify` steps also in the other direction.

### A whole journal, with the writer stopped

1. Checkpoint, close the writer, and check the source: `verify` exits with 0 and `status` reports an exposure of 0
   records. Records after the last checkpoint are copied too, but the marker does not cover them on either machine.

   ```sh
   tickertape verify journal
   tickertape status journal
   (cd journal && shasum -a 256 *.tape checkpoint) > journal.sha256
   ```

2. Copy. `sync` makes the new machine write the copy to its disk: the marker came along with the copy, so it calls
   records durable there before they are. This was run with a Linux destination; on macOS plain sync calls do not
   flush the drive's write cache (crash contract → What `checkpoint()` does), so with a macOS destination keep the
   source until the copy has been made durable some other way.

   ```sh
   ssh newhost 'mkdir journal'
   (cd journal && COPYFILE_DISABLE=1 tar --no-xattrs -cf - *.tape checkpoint) | ssh newhost 'tar -xpf - -C journal && sync'
   ```

   `COPYFILE_DISABLE` and `--no-xattrs` keep macOS metadata out of the archive; leave them out on Linux. `tar` keeps
   modification times, from which `status` estimates the checkpoint's age.

3. Check the copy against the list, then the journal:

   ```sh
   ssh newhost 'cd journal && sha256sum -c -' < journal.sha256
   ssh newhost 'tickertape verify journal && tickertape status journal'
   ```

   Both checks are needed. The records' CRC32C lets `verify` find a damaged record, but `verify` accepts an older,
   complete prefix of a journal and does not check that `checkpoint` names an existing segment (`status` does). The
   list shows that the listed files arrived unchanged; make it from all of them.

Readers then start at the oldest segment, durable-only readers see the records the marker covers, and a writer opened
there continues at the next sequence number (all three were run). **From then on use only one of the two
copies for writing.** The writer lock does not reach across machines: two writers give different records the same
sequence numbers, and the copies can't be joined again.

### Old segments, with the writer running

`tickertape archive` moves sealed segments that are strictly older than the checkpoint segment out of the journal. They
no longer change, so copy them from the archive directory (it must exist; the commands below are for a first run):

```sh
mkdir archive
tickertape archive journal --to archive --keep-segments 4
(cd archive && shasum -a 256 *.tape) > archive.sha256
ssh newhost 'mkdir archive'
(cd archive && COPYFILE_DISABLE=1 tar --no-xattrs -cf - *.tape) | ssh newhost 'tar -xpf - -C archive && sync'
ssh newhost 'cd archive && sha256sum -c -' < archive.sha256
```

What a reader on the new machine sees:

- The records of the archived segments, up to the seal of the last one. The seal names the next segment, which is not
  there, so `verify`, an unbounded `export` and `tail` report `next segment … missing or inconsistent` and exit with 2
  after the last record. That problem, at the last segment and alone, is expected; any other is not. Bounded reads
  succeed: `tickertape export archive --format jsonl --to <last sequence number>` and `stats … --to`. The last sequence
  number is one less than the sequence number in the name of the oldest segment left in the journal.
  `Reader::seek_to_end` fails the same way, and without a marker each forward seek reads its segment again (ADR 0046).
- Nothing with `--durable-only`: an archive has no marker.
- With the remaining files of the journal, copied once the writer has stopped, in the same directory, the whole journal
  again: `verify` exits with 0.

Do not open a writer on an archive copy: it creates the missing next segment and continues with sequence numbers the
original journal already uses.

### Limits

- **Do not copy a journal a writer is using as a whole.** The files change during the copy, so it is no snapshot.
  Copying the segments before `checkpoint` is dangerous: the marker can cover records the copied segments lack. If the
  marker names a segment the copy lacks, the writer refuses the copy. If it points into a segment that was copied
  earlier, `verify` and `status` can both exit with 0 and the writer opens, continues at the marker's sequence number
  and leaves a hole that readers cannot pass. A sign of this is a `verify` `last_seq` + 1 lower than the checkpoint's
  `next seq` in `status`. Copying `checkpoint` first avoids that, but the records after the marker may be incomplete,
  `verify` can fail because the next segment is missing, and retention may remove segments during the copy. Stop the
  writer, or use the archive recipe above.
- A copy that lacks a segment in the middle, or the newest one after a sealed one, fails `verify`; one that lacks only
  its oldest segments passes, like a journal after retention. The writer refuses a copy whose marker's segment is
  missing, but accepts one that lacks only older segments, and one that lacks only a newest segment after the
  marker's: in the last case it creates that segment again, empty. So run the checks before you open a writer, and do
  not rely on them for a copy of a live journal.
- A writer preallocates the segments it creates, not one it finds. Do not copy with options that make sparse files
  (`tar -S`, `rsync --sparse`) if a writer will continue on the copy: the newest segment may then have no disk
  space reserved (crash contract C10). Not tested.
- Not tested: x86-64 machines, and other filesystems than APFS and btrfs. Network filesystems are not supported, and
  tickertape does not build for big-endian machines.

## Monitoring

`tickertape status` and the `tickertape::status` module report what an operator needs to know about a running
recorder, cheaply and without a telemetry service (ADR [0032](decisions/0032-operational-metrics.md); the JSON and
Prometheus schemas are in [`cli.md`](cli.md)). Nothing they do slows the writer down, takes its lock or changes the
journal, and every file is opened non-blocking, so a FIFO or device in the journal can't hang them.

**What is reported is never guessed.** A value a sample can't establish is null (JSON), absent (Prometheus) or
`None` (Rust), usually with a reason. Values that are estimates are named `estimated_*`: the checkpoint's age, the
exposure's age, rolls left and time to full. A sample reads only the newest segment, so when the checkpoint is in an
older segment it can't see a missing or damaged segment in between: `head.verified` and `exposure.verified` are then
false, and the head and exposure are what the newest segment implies. A healthy verdict is then "healthy as far as
the sample can tell" (`health ok (unverified)`), not a promise that the writer would open the journal. A break the
sample does see (the head at invalid bytes, a marker the writer refuses, a segment the listing proves missing) is
reported as corruption. `tickertape verify` checks everything.

### What to alert on

| Signal | Why it matters | Suggested alert |
|---|---|---|
| **Corruption** (`tickertape_corrupt`; exit code 2) | The head is at bytes that fail validation (seen after a power loss or storage corruption, until the next writer repairs them; C8, C11), the checkpoint marker points to a missing segment or an impossible offset, or the segment listing proves a segment missing between the checkpoint and the head, so the writer refuses to open the journal | Page |
| **Exposure age** (`exposure.estimated_age`, `tickertape_exposure_estimated_age_seconds`; CLI `--max-checkpoint-age`) | Everything after the last successful checkpoint may be lost in a power loss, and its sequence numbers reused (crash contract C7, C8). The exposure age is an upper bound on how far back that loss reaches: zero when every record is covered, otherwise the checkpoint's estimated age. It is unknown (and `--max-checkpoint-age` is breached) when records are uncovered and there is no checkpoint or its time is unreliable | Warn above 3× your checkpoint interval, page above 10×; alert on `absent(tickertape_exposure_estimated_age_seconds)` too |
| **Checkpoint errors** (`Writer::metrics().poisoned`, or `checkpoint()` returning an error) | After a failed checkpoint, durability can't be guaranteed for this writer, and later checkpoints fail (C12, "Errors are sticky"). The marker then stops moving, so the exposure age grows too | Page on the first error. Only the writer process can see this directly |
| **Storage headroom** (`storage.available_bytes`, `estimated_rolls_left`; `--min-headroom`, `--min-headroom-segments`) | Writes into the active segment need no new space, but the next roll needs a whole segment; when it can't get one, `append` fails with a storage-full error (C9). **On btrfs** a full volume can kill the writer with `SIGBUS` even inside the active segment (C10), so the rolls estimate is optimistic there; on APFS a checkpoint on a full volume fails and poisons the writer. A prepared spare segment is reported (`spare_segment`) but not counted, since publishing it can still fail | Warn below a day's worth of segments at your peak rate (or use `predict_linear` on `tickertape_storage_available_bytes`), page below 2 rolls. Keep a larger margin on btrfs |
| **Reader lag** (`lag.records`/`bytes`, `unread_age`; `--max-reader-lag`) | A slow reader never slows the writer down (C6), so it can fall arbitrarily far behind. With retention, a reader in a segment that is removed gets `Error::Discarded`, and its beacon shows `lag_unknown_reason` `segment_gone`. A durable-only reader's lag is measured against the checkpoint, which it can't pass | Per consumer, from what the application tolerates. Compare `lag.bytes` (a segment-file footprint) with the bytes retention keeps to see how close a reader is to losing its segment |
| **Retention failures** (`Writer::metrics().retention_failures`, `Writer::last_retention_error()`) | Retention errors never fail `append` or `checkpoint`; they only show as a journal that keeps growing | Alert when the failure count increases. From outside: `tickertape_segments_bytes` above your retention limit for longer than a checkpoint interval |
| **Staleness of the metrics** (`tickertape_status_timestamp_seconds`, `tickertape_up`) | A sample that fails writes `tickertape_up 0`; a `tickertape status --watch` that stops (killed, hung storage) stops updating the file, which a textfile collector keeps serving | `tickertape_up == 0`, and `time() - tickertape_status_timestamp_seconds > 3 * <interval>` |
| **Writer process** (`writer.alive`) | Best effort: the PID in `writer.lock` can be reused | Use your process supervisor for this; treat it as a hint |

The checkpoint's age is estimated from the marker file's modification time and the observer's wall clock (the writer
changes the marker only at a checkpoint). It is reported as unknown when the time is detectably wrong: in the future
(a clock stepped back), earlier than the creation of the marker's segment (a copy that didn't keep times, a clock
step), or after the marker was cut short. A clock stepped forward, a later `touch`, or a copy made after the segment
was created can't be detected and make the estimate too large or too small. Inside the writer,
`Writer::metrics().since_checkpoint` uses the monotonic clock instead.

From C, C++, Python and Java the same values come from `tickertape_journal_status_sample`,
`tickertape_writer_get_metrics` and `tickertape_reader_get_metrics` and their wrappers (see
[`c-api.md`](c-api.md#operational-metrics), [`cpp-api.md`](cpp-api.md#operational-metrics),
[`python.md`](python.md#operational-metrics), [`java-api.md`](java-api.md#operational-metrics)); they report unknown
values the same way, and cost what `status()`, `Writer::metrics()` and `Status::lag()` cost.

### Cost of sampling

| Call | Cost |
|---|---|
| `Writer::metrics()`, `Reader::cursor()`, `Status::lag()` | No system calls |
| `ReaderBeacon::update()` | A comparison when the reader hasn't moved; otherwise a monotonic clock read, and at most once per interval (1 s by default) one 128-byte `pwrite`, plus a 64-byte header read when the reader moved to another segment |
| `status()`, `tickertape status` | One `readdir` and one `stat` per segment; two small reads of the checkpoint marker between two `fstat`s; the header of the newest segment, and of the checkpoint's and the oldest segment if different; a `pread` scan of the newest segment from the checkpoint (at most one segment; again if the writer rolled during the sample); one record read for the oldest uncovered record when the scan didn't pass it; `statvfs`; per reader beacon an `open`, an `fstat`, one or two small reads, two `flock`s and an `lstat`, plus a segment header read and one record read for a lagging beacon |
| `Sampler::sample()`, `tickertape status --watch` | As `status()`, but the scan covers only what was appended since the previous sample |

A record read is 4 KiB, or the whole record if it is larger (it is validated before its timestamp is used). The
exact counts are in the rustdoc of `tickertape::status::status` and in ADR 0032. Nothing is added to `append` or to
a reader's read path. Checkpointing regularly keeps one-shot samples cheap, since they scan only what was appended
since the checkpoint, and lets them verify the chain.

### Prometheus without a server

A node_exporter textfile collector picks up a file that `tickertape status` keeps up to date:

```sh
# Rewrites the file atomically every 15 seconds; rates and time-to-full appear from the second sample.
tickertape status /data/journal --format prometheus --watch 15s \
    --output /var/lib/node_exporter/textfile/tickertape.prom
# Or from cron, one sample at a time:
* * * * * tickertape status /data/journal --format prometheus --output /var/lib/node_exporter/textfile/tickertape.prom
```

With `--watch`, a sample that fails replaces the file with `tickertape_up 0` and the watch goes on; a one-shot run does
the same before it exits with an error. The file must not lie inside the journal, its `quarantine/` or the beacon
directory, and `status` replaces an existing file only if it is an earlier output of `status` (in any format) or an
empty regular file, and it finds no entry of the journal that links to it (when it cannot list the journal's
directories, a file that starts like a status output is still replaced: that is what lets a failed sample report
`tickertape_up 0`); anything else, including a symlink, is refused on standard error and stops a watch with exit
code 1 (remove the file, or choose another path). A write that fails (a full disk, a
directory that became read-only) stops a watch with exit code 1 too, so keep the timestamp alert below. Give `--output`
a path in a directory only the monitoring job writes to (such as the textfile directory above), and never link to
that file from inside the journal: `status` can't tell a file another process puts there between its check and its
rename from its own, and it can't see a link from a journal directory it can't list. `--watch` with Prometheus output needs
`--output` (several expositions in one stream are not valid). The `journal` label is the canonical path of the journal
directory.

Example alert expressions:

```text
tickertape_corrupt == 1
tickertape_up == 0
time() - tickertape_status_timestamp_seconds > 60
tickertape_exposure_estimated_age_seconds > 30 or (tickertape_exposure_records > 0 unless tickertape_exposure_estimated_age_seconds)
tickertape_storage_estimated_rolls_left < 2
predict_linear(tickertape_storage_available_bytes[6h], 24 * 3600) < 0
tickertape_reader_lag_records{reader="risk-engine"} > 100000 unless on(instance, job, journal, reader) tickertape_reader_live == 0
```

The last rule counts every reader beacon except a stale one (`tickertape_reader_live` is 0 only for a beacon whose
owner died, and absent when its lock state is unknown), as `--max-reader-lag` does: an `uncertain` beacon still
counts. `tickertape_reader_live == 1` instead would silently skip `uncertain` beacons. The match keeps the scrape
identity: `instance` and `job` are the labels Prometheus attaches to every scraped series by default (for a
textfile collector, the node_exporter's), so a stale beacon on one host never suppresses a live reader's alert on
another host that uses the same journal path and reader name. If your relabelling adds or replaces identifying
labels (a `host` label, say), list those in `on(...)` too, or leave out `on(...)` to match on every label.

As a health check (systemd, Kubernetes, a load balancer script), the exit code is enough:

```sh
tickertape status /data/journal --max-checkpoint-age 30s --min-headroom-segments 4 --max-reader-lag 100000
# 0 healthy; 3 a threshold breached; 2 the journal is corrupt or the writer would refuse it; 1 I/O or usage error.
```

When a one-shot run also has an `--output` it can't write, the health verdict still decides the exit code (2 or 3, or 1
for a failed sample) and the output's failure goes to standard error; a healthy sample then exits 1.

### Exporting from your own process

The library has no metrics dependency; a few lines connect it to whatever the application uses. With the
`prometheus` crate, for example:

```rust
use prometheus::{IntGauge, register_int_gauge};

let exposure = register_int_gauge!("tickertape_exposure_records", "Records not yet durable")?;
let rolls_left = register_int_gauge!("tickertape_storage_estimated_rolls_left", "Segment rolls that still fit")?;
let poisoned = register_int_gauge!("tickertape_writer_poisoned", "1 after a failed checkpoint")?;
let mut sampler = tickertape::status::Sampler::new("/data/journal");
// On each scrape or timer tick:
let status = sampler.sample()?;
if let Some(records) = status.exposure.records {
    exposure.set(records as i64);
}
rolls_left.set(status.storage.estimated_rolls_left as i64);
poisoned.set(i64::from(writer.metrics().poisoned)); // in the writer's process
```

With OpenTelemetry, register observable gauges whose callbacks read a `Status` the same way (the builder names vary
between `opentelemetry` versions):

```rust
let meter = opentelemetry::global::meter("tickertape");
let dir = std::path::PathBuf::from("/data/journal");
let _exposure = meter
    .u64_observable_gauge("tickertape.exposure.records")
    .with_callback(move |gauge| {
        if let Some(records) = tickertape::status::status(&dir).ok().and_then(|s| s.exposure.records) {
            gauge.observe(records, &[]);
        }
    })
    .build();
```

### How old records are when they are read

From 1.1 (ADR [0047](decisions/0047-latency-histograms.md)). `latency::RecordAge` in the process that reads, or
`tickertape age` from the command line, measure the **age** of each record when a reader returns it: this machine's wall
clock at that moment, minus the record's `local_ts_ns`.

```sh
tickertape age /data/journal --duration 60s            # one report, as text
tickertape age /data/journal --interval 10s --json     # one JSON object every 10 s, until interrupted
```

**What age is not.**

- **It is not the writer-to-reader latency.** It starts where the caller took `local_ts_ns`, which can be long before the
  append (the receive time of a market-data packet is the usual choice), and the time of the commit is not recorded
  anywhere. The two are equal only if the caller stamps right before `append`.
- **It is not the delay of your reader, if you use the command.** `tickertape age` measures its own reader, on the CPU it
  happens to run on and with the wait it was told to use (`--wait`, [`cli.md`](cli.md)). It says whether the writer, the
  clock and the machine are healthy. Your reader's ages need `RecordAge` around your reader.
- **It is not how long a replay takes.** Records read before the reader has caught up, or after a seek or discarded
  history, are counted apart (`before_live`, `after_discontinuity`): their age says when they were recorded. With
  `RecordAge` that rests on the application's own declaration (`set_live`); a forgotten `set_live(false)` before a seek
  backwards puts a replay into the histogram. A backlog that builds up while the reader is live does count.
- **It shows nothing about a record that has not been read**, such as one a stuck reader has not reached. `status` shows
  that (reader lag, unread age).
- **It is nothing at all** if records are appended again with their original timestamps, or stamped with something other
  than nanoseconds since 1970 (uptime nanoseconds give ages of decades, and a `mean_ns` of null). The library can't tell.

**Clocks.** Both times must come from the same clock, or from clocks that agree.

- Two processes on one machine (also in different containers) that both use `CLOCK_REALTIME`
  (`tickertape::clock::realtime_ns()`) share it. If it is stepped, the result moves by the step: a step forward shows as
  a false maximum, a step backward as records ahead of the clock. Slew the clock (chrony and ntpd do by default, after
  their start) rather than step it.
- **Timestamps ahead of the observer's clock** (`ahead`, in JSON `ahead_of_clock`) have no age and are never counted as 0.
  **When that count is not 0, the minimum and every percentile are biased upwards**, because the records left out are the
  fastest ones. A few after a backward step are harmless; a steady share means that the stamping clock runs ahead: a
  hardware timestamp (NIC, PHC), a clock on TAI (37 s ahead of UTC), or a clock a few microseconds fast.
- A clock that runs behind by a constant adds that constant to every age, and nothing shows it.
- On macOS the wall clock advances in steps of 1 µs: shorter ages are 0, and a finer timestamp can look ahead.
- A tape copied from another machine has no live records unless something appends to it, so nothing counts there.

**Exporting.** A `View` (or `RecordAgeView`) can be kept by a monitoring thread while the reading thread records. It
copies about 30 KB per snapshot and takes no lock; once a second or slower is the intended use. The histograms only grow:
subtract the previous snapshot for the values of an interval. Export quantiles and the extremes rather than the mean of a
short interval: values recorded while a snapshot is taken can be in its sum and not in its count, so the mean of a busy
recorder's interval can be off either way, even outside the extremes (the longer the interval, the less). With the `prometheus` crate, for example:

```rust
use prometheus::{Gauge, IntGauge, register_gauge, register_int_gauge};

let p99 = register_gauge!("tickertape_record_age_p99_seconds", "Age of records when read, last interval")?;
let max = register_gauge!("tickertape_record_age_max_seconds", "Age of records when read, last interval")?;
let ahead = register_int_gauge!("tickertape_records_ahead_of_clock", "If it grows, the ages are biased upwards")?;
let view = age.view(); // `age` is the reading thread's RecordAge
let mut previous = view.snapshot();
// On each scrape or timer tick, on another thread:
let now = view.snapshot();
let interval = now.age.since(&previous.age);
// An interval without records has no quantiles.
let seconds = |d: Option<std::time::Duration>| d.map_or(f64::NAN, |d| d.as_secs_f64());
p99.set(seconds(interval.value_at_quantile(0.99)));
max.set(seconds(interval.max()));
ahead.set(now.ahead as i64);
previous = now;
```

An application can time anything else with a `latency::Recorder` of its own, for example its `checkpoint()` calls, or an
event's handling from when it was due (see the module's documentation for why that differs from the call's duration). The
library records nothing by itself: no histogram of appends, rolls or checkpoints is kept in the writer.

### Making readers in other processes visible

Readers leave no trace on disk by default, so an observer can't see their position. A reader that should be
monitored opts in with a beacon:

```rust
use tickertape::status::ReaderBeacon;

let mut beacon = ReaderBeacon::create("/data/journal", "risk-engine", &reader)?;
loop {
    if let Some(record) = reader.next_timeout(Some(std::time::Duration::from_secs(1)))? {
        // ... handle the record ...
    }
    beacon.update(&reader)?; // writes at most once a second, only when the position moved
}
```

The beacon is `<dir>/readers/risk-engine` (128 bytes); it disappears when dropped. A reader that crashes leaves its
file behind, but `status` reports it as `stale` (the beacon's `flock` died with the process) and never counts it as
lag; delete the file when you no longer want to see it, or create a beacon with the same name, which takes it over.
Creating a beacon never replaces anything but a stale beacon (a valid one, or one whose last update was cut short
when its owner died): a name held by a live reader fails with `Error::BeaconInUse`, and any other file under the
name is left alone. Names that are journal files' names, in any letter case, are refused. Beacons never influence
the writer or retention.

- **Permissions.** Beacon files are created with the reader's umask (mode `0666` before the umask). An observer
  needs read access to the beacon directory and the files; a beacon it can't read is reported as `unreadable`, and a
  directory it can't list as unusable, without failing the sample. A reader of **another user's journal**, which
  can't write to its directory, creates its beacon with `ReaderBeacon::create_in` in a directory of its own, and the
  observer is pointed at it with `--readers-dir` (or `Sampler::readers_dir`).
- **Identity.** A beacon records the identity of the segment file its cursor is in, and an observer computes lag only
  against that same file. Beacons of different journals can share a directory: a beacon of another journal shows a
  `lag_unknown_reason` of `other_history`, and so does a reader still in a segment that recovery repaired or a
  journal replaced at the same path.
- **`fork`.** The lock belongs to the open file: a child created with `fork` that keeps the beacon's descriptor
  keeps the beacon `live` after its creator dies, until the child exits. The descriptor is close-on-exec, so `exec`
  drops it, but every child holds it from `fork` to `exec`: while another thread spawns a process, a beacon dropped
  at that moment can still look live (and a creator of the same name can get `BeaconInUse`) for that instant. A
  child's copy of a `ReaderBeacon` never writes or removes the file: `update` fails in the child, and dropping the
  copy leaves the parent's beacon in place.
- **Filesystems.** Beacons, like journals, are for local filesystems. On network filesystems `flock` may be
  unsupported (observers then report `uncertain`, and lag thresholds apply as for live beacons) or emulated (on Linux
  NFS, an observer in the owner's own process can release the owner's lock).

## Checking a journal

- `tickertape verify <dir>` checks every header, record and seal and exits with code 2 if anything is wrong; it is
  read-only and safe to run while the writer is active. A segment that ends in a state a running writer passes
  through (a record being written, or a segment not yet sealed whose successor exists) is scanned again with growing
  pauses, for up to 630 ms, before it is reported. Records the writer commits while `verify` looks at the end of the
  newest segment are followed, not reported (ADR 0035); a record being written at the end `verify` follows them to is
  scanned again with what is left of those 630 ms for that segment. If the writer is stalled in such a state for
  longer, `verify` can report a problem that isn't there; run it again to tell. Likewise, if the writer takes longer
  than about 50 ms to fill a single record whose payload itself contains valid tickertape frames with nearby sequence
  numbers (a journal that stores another journal's raw frames), `verify` can report that record as an empty commit
  word in front of committed records while it is being written; a second run after the record is committed reports
  nothing. With no writer running, a problem it reports is in the files, though not always storage damage: a record torn by a
  crash whose payload holds such frames is reported the same way (crash contract C1). The next open need not meet it:
  `verify` visits every retained segment and scans records in each segment whose header passes validation, while a
  writer's recovery scans records from the checkpoint onward (from the oldest retained segment when neither
  checkpoint slot decodes). Where a segment's records end at an empty
  commit word, `verify` also searches the rest of that segment: it reads backward from the segment's end to find the
  last non-zero byte, then checks the candidates up to it, and can stop early (`cli.md` → `verify`).
- `tickertape inspect <dir>` lists segments, their sequence ranges and fill level, and the checkpoint marker.
- `tickertape status <dir>` is the cheap, frequent check: head, checkpoint age, exposure, headroom and reader lag
  (see "Monitoring"). Unlike `inspect` and `verify`, it does not scan the whole journal.

## After a crash

- **Writer process crash:** just start the writer again. It cleans up a partly written record and continues with the
  next sequence number (C1–C4). Its `recovery_report()` usually lists at most a zeroed uncommitted tail, a completed
  seal or roll, and removed temporary files. The exception (crash contract C1): the record being written can hold
  bytes that recovery's search counts as committed records (raw frames copied from another journal, for example), and
  recovery then reports `EmptyCommitWord` or `UncheckedTail`. With the default `Quarantine` it preserves those bytes
  in `quarantine/` (`RepairedTail`) and continues; nothing committed is lost. With `RecoveryPolicy::Fail` the writer
  refuses to open. The bytes alone cannot tell this from storage damage: a frame at the reported file and offset that
  carries the expected sequence number, with the candidates inside its payload, is consistent with a torn append, but
  damage can leave the same bytes (ADR 0035). Decide from what the application knows it was writing when the process
  died: if it was such a payload, open the writer once with `RecoveryPolicy::Quarantine`, which preserves those bytes
  and continues; if not, treat it as the storage damage below.
- **Power loss or storage errors:** the writer validates the journal from the last checkpoint. Corrupt or
  disconnected data it detects is usually moved to `quarantine/`, not deleted, or, with `RecoveryPolicy::Fail`, the
  writer refuses to open so that a human can look first. That includes committed records hidden behind a zeroed
  commit word, when recovery finds evidence of them (`EmptyCommitWord`, or `UncheckedTail` when the bytes were too
  costly to check completely; crash contract C11): the quarantine file starts at the empty word (ADR 0035). A
  quarantine file of a tail stops at the last non-zero byte: the zeros after it are left out. To restore records from
  it, copy it back to the offset in its name and keep the rest of the segment zero (or, outside a segment, append
  zeros): a last record whose payload ends in zero bytes declares a length that reaches past the end of the copy, and
  it only validates with those zeros put back. Some bytes are zeroed in place without a copy: damage that recovery
  cannot tell from a crash (crash contract C8 and C11). Bytes after an empty commit word in front of a next segment
  that does not open (is invalid; an I/O error opening it fails the open instead) are preserved like a corrupt tail,
  reported as `BrokenChain` when the search finds no evidence of committed history after the word (with evidence, as
  `EmptyCommitWord` or `UncheckedTail`), and that next segment and the later ones are moved to `quarantine/` as
  disconnected; `Fail` refuses before changing those bytes (C11 (2) and (3)). A seal whose marker is empty, in front of a next segment that opens, is rewritten, keeping the records of its
  segment; a seal with its marker present and an invalid CRC is a corrupt tail like any other (C11 (1)). If `quarantine/` holds two files with identical bytes for the same position, a repair was interrupted
  and repeated; either copy can be deleted.
  Readers that had seen discarded records get `Error::Discarded`, except after a repair in place (damage that can't
  be told from a crash, as above, so recovery zeroes the records after the empty commit word in the same file), which
  visible readers can't notice: they may continue into the new history, wait, or report corruption, until seeked to
  their position (crash contract C11). Durable-only readers never return records that recovery discarded, unless the
  damage also destroyed the checkpoint marker (C13).
- **What `Fail` still does when it refuses:** it leaves the bytes of the corruption it reports, and everything after
  them in that segment, unchanged, and moves no file, but the open is not read-only. Before scanning it may create
  the lock file and the checkpoint file, writes its PID into the lock file (best
  effort), and removes stale temporary files (`*.tape.tmp`, `next-segment.tmp`); and it keeps changes recovery made
  earlier in the scan, including bytes zeroed in place. Crash contract C11 lists them.
- **Refused to open** (missing checkpoint segment or a gap in the chain): restore the missing segments from a copy,
  or move the affected segments aside knowingly. tickertape won't discard history on its own.

The `quarantine/` directory is never cleaned up by tickertape; inspect it and delete it yourself when you're done.
