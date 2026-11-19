# 0032: Operational metrics and reader beacons

- **Status:** proposed
- **Date:** 2026-11-19 (revised the same day after review round 1 of PR #8)

## Context

The v1.0 roadmap asks for inexpensive operational metrics for reader lag, checkpoint age, storage headroom and
retention failures, with documented sampling costs and no mandatory telemetry service. An operator running a
recorder wants to know, cheaply and from another process: how far behind each consumer is, how much a power loss
would cost now, how much room is left before the volume fills, and whether retention is failing.

Constraints: format version 1 does not change; `append` gets no new work; the reader hot path stays as it is; readers
never slow the writer down, and nothing a reader does may influence the writer or retention; `unsafe` stays in
`mmap.rs` and `crc.rs`; no background threads, global state, network or mandatory metrics dependency.

Today an outside process can see the segment files, the checkpoint marker and `writer.lock`. Readers leave no trace
on disk, and the writer keeps its retention errors in memory.

## Governing rules

These decide every case below, and the tests check them:

1. **Nothing reported may be untrue.** A value that a sample could not establish is reported as unknown (`None`,
   JSON `null`, an absent Prometheus series), with a reason where there are several. It is never guessed. Values
   that are estimates are named `estimated_*`.
2. **A journal the writer would refuse to open never looks healthy.** What a sample can check cheaply, it checks as
   the writer does; what it can't (the segments between the checkpoint and the head, which it doesn't read), it
   marks unverified.
3. **Observing never modifies a journal, blocks or fails a reader or the writer, and is never defeated by junk.**
   Every file is opened non-blocking and must be a regular file; the observer never takes a lock anybody waits for;
   an unreadable beacon or an unusable beacon directory is reported, never fails the sample.

## What is measured, and where it comes from

| Value | Source | Accuracy and failure modes |
|---|---|---|
| **Head**: next sequence number, segment, offset | A `pread` scan of the newest segment from the checkpoint position if the marker points into it, otherwise from its first record. A `Sampler` resumes from its previous head while the newest segment is the same file (device, inode, and the header's creation time and size), or from the checkpoint if that is further on | Exact for what was committed in the newest segment when the scan passed. A torn copy of a record being appended is re-read 3 times before the head is reported `invalid` (as the reader does). If the scan ends at a seal and the successor exists (the writer rolled meanwhile), the directory is listed again and the new newest segment scanned instead, up to 16 times; so the head is `sealed` only if the successor did not exist when checked. **`verified`** says whether the sample (or the sampler's earlier samples of the same file) validated every record from the checkpoint position (without a marker: from the start of the oldest segment) to the head. When the checkpoint is in an older segment, the segments in between are not read, so a missing or damaged one is not noticed, and the head's sequence number is what the newest segment's name and records imply |
| **Checkpoint**: generation, `next_seq`, position | The marker's two slots, read with up to two 64-byte `pread`s between two `fstat`s, retried while the metadata changes | Validated on every sample as `Writer::open` validates it (`docs/format.md` → Recovery, step 2): the marker's segment must exist with a valid header, and its offset must be aligned and inside the record area. Otherwise the marker has a `problem`, the writer refuses the journal, and the sample reports `checkpoint_invalid`, a corruption breach |
| **Checkpoint age** (an estimate) | The marker file's modification time, compared with the observer's wall clock read **after** the marker | The writer changes the marker only by writing a slot at a checkpoint (since this change, also when it extends a short marker at open it restores the old modification time), so the time is the checkpoint's, at filesystem timestamp resolution. Reported as **unknown** when detectably unreliable: in the future; earlier (by more than 2 s, for coarse timestamps) than the creation time in the header of the marker's segment, which a checkpoint can't predate; the marker not at its full length (cut by something else); or the metadata changing over 3 reads. Undetectable: something else writing or `touch`ing the file later, a copy that didn't keep times, a forward clock step. A sample taken while a checkpoint writes the marker may pair the new time with the previous slot (the kernel updates the time before copying the data). If the marker's own sync fails, the slot and time are visible although the writer is poisoned: the in-process `Writer::metrics().poisoned` is authoritative there |
| **Exposure** (what a power loss can remove) | Records and segment-file footprint between the checkpoint position and the head; its age is the checkpoint's estimated age if any record is uncovered, else zero | Counts follow the head (`verified` likewise). Unknown if the marker has a problem. The age is an upper bound: every uncovered record was appended after the checkpoint. The local timestamp of the oldest uncovered record is reported as well, as data in the caller's clock |
| **Reader lag** (in process) | `Reader::cursor()` against a sample: records, and segment-file footprint between the two positions | The cursor's segment must be the file the sample listed under that name (device and inode; the reader holds it open, so the inode can't be reused). Records are exact; bytes are the footprint (below), zero when no record is unread, unknown while a seek is pending. A durable-only reader's lag is measured against the checkpoint, which it can't pass |
| **Reader lag** (other processes) | Opt-in reader beacons (below) | As fresh as the beacon's last write, and only if the beacon's segment identity matches this journal's file (else unknown, with the reason). The unread age is the observer's wall clock minus the `local_ts_ns` of the first unread record, so it is meaningful only when the producer stamps `local_ts_ns` from the same host's wall clock |
| **Storage headroom** | `statvfs` on the journal directory: `f_bavail * f_frsize` available to unprivileged users, and `f_blocks * f_frsize` total | The bytes are the observed fact; rolls and time are estimates. See "Headroom" below |
| **Retention** | In process: `Writer::metrics()` (failed runs, last error), `Writer::retention_report()`. From outside: segment count, total segment bytes, oldest segment's creation time, and how many segments lie before the checkpoint segment | The writer does not persist retention errors. An outside observer can only see their effect: a journal growing past the configured limit |
| **Writer process** | The PID in `writer.lock` and `kill(pid, 0)` | Best effort, never used for health: the PID stays after a clean exit and can be reused. Probing the lock itself would make a writer that opens at that moment fail with `Error::Locked`, so the observer never touches the lock |

**Footprint.** Byte lag and exposure bytes count segment files between two positions: the rest of the first
segment, every segment in between, and the offset in the last, so headers, seals, unused tails and trailers count.
A segment rolls when the next record (aligned frame `F`) doesn't fit, so its unused tail including the seal and the
trailer is less than `F + 8` bytes; with the next segment's 64-byte header, the footprint exceeds the unread record
bytes by less than `72 + F` per segment boundary, where `F` is the first record of the next segment. For large
records that is up to a segment per boundary, which is why the value is described as a footprint, not as unread
bytes.

**Headroom.** Segments are preallocated, so on APFS, ext4 and xfs writes into the active segment need no new space
(crash contract C10). **On btrfs this does not hold**: a full volume makes writes into the active segment fail
(`SIGBUS`), so every estimate below is optimistic there. The failure a full volume causes elsewhere is the next roll,
which needs a whole new segment (C9). Headroom is reported as `available_bytes` and as **`estimated_rolls_left`**:
`available_bytes / next_segment_size`. The next segment's size is the prepared spare's size if there is one (the
writer makes it with its configured size), otherwise the newest segment's size; a writer reopened with another size
makes this wrong until it rolls or prepares a spare. A prepared spare (`next-segment.tmp`, ADR 0022) is reported
separately (`spare_segment`, only if all its blocks are allocated: a sparse file of the right length holds no
space) and **not counted** in the rolls, because publishing it at the roll can still fail on a full volume (C9,
C10). Given an append rate, the estimated time until full is the time until the writer would need roll
`estimated_rolls_left + 1`: the bytes left in the active segment (none if it is sealed) plus that many segments,
divided by the rate. A one-shot status has no rate; a `Sampler` measures one between samples whose history
continued (the previous head's segment is still the same file). Prometheus users get a better estimate from
`predict_linear` over the available bytes, which also counts other users of the volume.

## Reader positions from other processes

Options:

1. **In-process API only.** Smallest, but the CLI cannot answer "how far behind is each consumer".
2. **Leave it to the application** (each consumer exports its own lag). Works with any metrics system, but every
   application re-implements it and the operator needs one dashboard per consumer.
3. **Automatic position reporting from every reader.** Readers would write to the journal directory (they may not
   have permission, and today they never write), and the hot path would need a clock read or a counter.
4. **An opt-in beacon the application drives** (chosen).

**Decision: `status::ReaderBeacon`.** An application that wants its reader to be visible creates a beacon with a
name and calls `beacon.update(&reader)` whenever convenient (for example after each batch). Beacons live in
`<dir>/readers/<name>` by default, or in any directory the application chooses (a reader of another user's journal,
which can't write to its directory, passes its own and the observer is pointed at it with `--readers-dir`).

### Beacon file

128 bytes, little-endian, outside format version 1 (own magic and version). A file of any other length is not a
beacon.

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | magic `TTREADER` |
| 8 | 4 | version, `1` |
| 12 | 4 | flags: bit 0 durable-only reader, bit 1 seek pending; other bits zero |
| 16 | 4 | owner's pid (informational) |
| 20 | 4 | reserved, zero |
| 24 | 8 | `next_seq`: the next record the reader returns |
| 32 | 8 | the sequence number expected at the cursor's slot (differs from `next_seq` while a seek is pending) |
| 40 | 8 | the slot's segment base sequence number |
| 48 | 8 | the slot's offset |
| 56 | 8 | that segment header's `created_unix_ns` |
| 64 | 8 | that segment header's `segment_size` |
| 72 | 8 | that segment file's device number |
| 80 | 8 | that segment file's inode number |
| 88 | 8 | owner's wall-clock time of the write, ns since the Unix epoch |
| 96 | 28 | reserved, zero |
| 124 | 4 | CRC32C of bytes 0–123 |

**Identity.** An observer computes lag only if the segment named by the slot's base is, in the journal it samples,
the same file (device, inode, size) with the same header creation time. Otherwise the lag is unknown, with the
reason: `segment_gone` (retention removed it, or recovery moved it to `quarantine/`), or `other_history` (the beacon
belongs to another journal sharing the beacon directory, the journal was replaced at the same path, or recovery
repaired the segment after the reader read it: a repair writes a new file, and the old inode is still held by the
reader or kept in `quarantine/`, so it can't be reused while it matters). The header's creation time guards against
inode numbers that ext4 reuses at once. A durable-only reader's lag is measured against the checkpoint marker, since
it can never pass it; with no marker it can read nothing and its lag is zero.

**Cost.** Readers without a beacon do no work: nothing changes in `Reader`'s read path. `update` compares the
reader's position with the last one written; if it moved, it reads the monotonic clock and writes only if at least
the beacon's interval (default 1 s) has passed: one 128-byte `pwrite`, never synced (a beacon only has to survive
the reader process, not a power loss), plus an `fstat` and a 64-byte header `pread` when the reader moved to another
segment file. `flush` writes unconditionally.

### The beacon protocol

One invariant carries the protocol: **a beacon file is locked (exclusive `flock`) only by its creator, before it is
published under its name, and never again.** Three consequences follow. A published beacon is always complete. Its
contents are written only by the process that holds its lock. And once a published beacon is seen unlocked, its
owner has gone for good (died, or dropped it and is about to unlink it): it can only be stale, and nobody will write
it again.

Creators serialize among themselves on an exclusive `flock` of the beacon directory itself (an `O_DIRECTORY`
descriptor, so no lock file is needed). Observers never take it.

**Creating** a beacon named `name` in directory `D`:

1. Check the name: 1 to 64 ASCII letters, digits, `.`, `_`, `-`, not starting with `.`; not a journal file's name
   (`checkpoint`, `writer.lock`, `notify`, `next-segment.tmp`, `quarantine`, `readers`); not ending in `.tape` or
   `.tmp`. A beacon directory may be the journal directory itself, so this keeps beacons away from journal files by
   construction.
2. Read the reader's segment header (from the reader's own descriptor) and encode the beacon.
3. Lock `D` exclusively (waiting if another creator holds it).
4. Remove temporary files that creators which died before publishing left (`.<name>.<pid>.<n>.beacon-tmp`, regular,
   unlocked). Under the directory lock, an unlocked temporary file can't belong to a creator that is still working.
5. Create a new temporary file `.<name>.<pid>.<n>.beacon-tmp` with `O_CREAT | O_EXCL` (never follows a symlink,
   never opens an existing file), with the process's umask; lock it exclusively (it is new, so this can't fail for
   contention); write the complete beacon.
6. Publish: `link` the temporary file to `name`. If that succeeds, unlink the temporary name. Done.
7. If `name` exists, check it (takeover): open it `O_NONBLOCK | O_NOFOLLOW`; it must be a regular file. Probe its
   lock with a **shared** non-blocking `flock`: if that fails, a live owner holds it: refuse with
   `Error::BeaconInUse`. If it succeeds (release it at once), read it: it must be exactly 128 bytes that decode
   (magic, version, flags, reserved bytes, checksum); otherwise refuse with `Error::InvalidConfig` and leave it
   untouched. Then `lstat` the name: if it no longer names the file just checked, go back to step 6. Otherwise
   `rename` the temporary file over it, and check that the name now names the new beacon.
8. Unlock `D`.

**Updating**: `pwrite` the 128 bytes at offset 0 of the locked file.

**Removing** (dropping the beacon): `lstat` the name; if it still names this beacon's file, unlink it; then close
the file, which releases the lock.

**Observing** a beacon in `D` (no directory lock): skip names that are invalid or reserved and entries that are known
not to be regular files; open the name `O_NONBLOCK | O_NOFOLLOW` (a FIFO, socket, directory or symlink is skipped,
never opened blocking), `fstat` it (regular file; note its identity); read it (129 bytes, to see a longer file),
re-reading once on a bad checksum and up to 8 times while a live owner holds it; probe its lock with a shared
non-blocking `flock` and release it (`live` if it fails with `EWOULDBLOCK`, `stale` if it succeeds, `uncertain` on
any other error); then `lstat` the name: if it no longer names the file observed, start again (up to 3 times, then
report the last file observed). A file that can't be opened or read (for example another user's beacon created under
umask 077) is `unreadable`; one that doesn't decode is `invalid`.

**Why each interleaving is safe:**

- *Two creators of one name.* They serialize on the directory lock; the second finds the first's beacon at step 7,
  locked, and gets `BeaconInUse` (test `creators_of_one_name_serialize`).
- *A creator and the owner dropping its beacon* (the reported ghost). The creator's own file is published by its
  own `link` or `rename` while it holds its lock, and is only ever replaced by a takeover, which requires it to be
  unlocked. If the owner unlinks the old file while the creator has it open (step 7), the creator either sees the
  lock still held (refuses, which was true at that instant) or sees it free, then finds at the `lstat` that the name
  no longer names that file, and links its own beacon to the now free name. Nothing ever writes to a deleted beacon
  under the belief that it is published (`owner_drops_while_a_creator_checks_the_name`,
  `creator_during_the_owners_unlink`; across processes, `beacon_churn_across_processes` checks
  `ReaderBeacon::is_published` after every creation and before every drop).
- *Two creators taking over one stale beacon.* The directory lock orders them; the second finds the first's new,
  locked beacon (`two_creators_take_over_one_stale_beacon`).
- *The owner removing its beacon while a creator checks it.* The owner unlinks before it closes, so while the name
  still names its file, the file is locked and no takeover can happen; the owner's `lstat`-then-unlink therefore
  can't remove someone else's beacon.
- *An observer and a creator.* Observers probe with a shared lock and only on published files; creators lock only
  their own new temporary files exclusively and probe existing files with a shared lock too. A shared probe never
  conflicts with another shared probe, so an observer can't make creation fail, however long it is preempted while
  holding its probe (`takeover_while_an_observer_holds_its_probe`). An observer never sees a beacon before it is
  complete, because it is complete before it gets its name (the temporary name starts with `.`, which no beacon name
  does).
- *An observer while a beacon is replaced.* Contents, file identity and lock state are all taken from one open file,
  and a published file never changes owner: either it is the live owner's (its contents are that owner's) or its
  owner is gone (its contents are final). If a creator replaces the name meanwhile, the final `lstat` notices and the
  observer starts again, so it never pairs a dead reader's position with a new owner's lock
  (`observer_sees_the_new_owner_after_a_takeover`, paused at each observer step;
  `observer_when_the_owner_leaves_mid_observation`).
- *A torn update.* An observer that reads while the owner rewrites the beacon may see a mix; the checksum fails and
  it reads again.
- *A creator that dies.* Before step 6, it leaves a temporary file, removed by the next creator (step 4); between the
  `link` and the unlink of the temporary name, a second, harmless link to a now stale beacon, removed the same way.
  It never holds the directory lock beyond its death.

**Limits, stated in the rustdoc and in `docs/operations.md`.** The lock belongs to the open file, not the process: a
child created with `fork` that keeps the descriptor (it is opened close-on-exec, so `exec` drops it) keeps a dead
reader's beacon live until the child exits. On network filesystems `flock` may be emulated or unsupported: Linux NFS
emulates it with byte-range locks, so an observer in the owner's own process can release the owner's lock. Beacons,
like journals, are supported on local filesystems only; where the probe fails, observers report `uncertain`, and lag
thresholds apply to `uncertain` beacons as to live ones. A beacon created with umask 077 can't be read by other users
(`unreadable`); the creating process's umask is respected, never overridden.

**Never influences the writer.** The writer, readers, recovery and retention never read the beacon directory.
Retention that waits for slow readers would be a separate, explicitly designed opt-in; it is out of scope.

## API

- `status::status(dir) -> Result<Status>`: one read-only sample, from any process. Plain `#[non_exhaustive]` structs;
  unknown values are `Option`s, often with a reason (`CheckpointProblem`, `AgeUnknown`, `LagUnknown`,
  `BeaconDirState`).
- `status::Sampler`: the same, but keeps the previous head (resumes the scan there, so steady-state cost is
  proportional to the bytes appended since the last sample) and reports append rates and time until full.
  Everything it carries is keyed by the identity of the segment file it came from (device, inode, header creation
  time and size), so a replaced journal or a repaired segment is scanned afresh and rates and cached timestamps are
  dropped; a failed lookup is never cached.
- `Writer::metrics() -> WriterMetrics`: head, last checkpoint (with a monotonic age, immune to clock steps),
  uncheckpointed records and bytes (what was appended after the marker, by this writer or, before it opened, by
  earlier ones; without a marker, from the oldest retained segment), poisoned flag, retention failure count. No
  system calls.
- `Reader::cursor() -> Cursor` and `Status::lag(&cursor) -> Result<Lag, LagUnknown>`: in-process reader lag. No
  system calls.
- `status::ReaderBeacon`: as above, with `Error::BeaconInUse` for a name held by a live beacon and
  `ReaderBeacon::is_published`.
- `Status::breaches(&Thresholds) -> Vec<Breach>`; `Breach::is_corruption` for the always-checked `HeadInvalid` and
  `CheckpointInvalid`.

No metrics crate is a dependency. The docs show how to export a `Status` to Prometheus or OpenTelemetry in a few
lines of application code. An optional `metrics`-facade feature is possible later; it is left out because the
facade would fix metric names and types in the core crate before 1.0 for little gain over those few lines.

## Sampling cost

What the code does, per call (counted from `status.rs`; no file stays open after a call):

| Call | System calls | Scans records? |
|---|---|---|
| `Writer::metrics`, `Reader::cursor`, `Status::lag` | None (one monotonic clock read for `Writer::metrics`) | No |
| `ReaderBeacon::update` | A comparison if the reader has not moved; else one monotonic clock read, and at most once per interval a wall-clock read and one 128-byte `pwrite`, plus `fstat` and a 64-byte `pread` of the segment header after the reader moved to another segment file | No |
| `ReaderBeacon::create`/`create_in` | `mkdir`; `open` and `flock` of the directory; a `readdir` pass; `open` (exclusive create), `flock`, `fstat`, `pwrite`, `link`, `unlink`; a takeover adds `open`, `fstat`, two `flock`s, `pread`, `lstat`, `rename`, `lstat` | No |
| `status()` / `Sampler::sample()` | Journal directory: one `readdir` pass and one `stat` per segment. Marker: `open`, `fstat`, up to two 64-byte `pread`s, `fstat` (repeated, at most 3 times, while it changes). Newest segment: `open`, `fstat`, 64-byte header `pread`, scan `pread`s in 1 MiB windows; again after a re-listing if the writer rolled (at most 16). The checkpoint's and the oldest segment, if different: `open`, `fstat`, header `pread` each. A `Sampler` whose head moved to another segment: the same for the previous head's segment. With uncovered records the scan didn't pass: one record lookup (below). `statvfs`; `lstat` of the spare; `open` and `read` of `writer.lock`; `kill(pid, 0)`. Beacon directory: one `readdir` pass; per beacon `open`, `fstat`, one or two 129-byte `pread`s, two `flock`s, `lstat`; per beacon whose segment isn't open yet, `open`, `fstat`, header `pread`; per lagging beacon, one record lookup. One monotonic and one wall-clock read | The newest segment from the checkpoint (or its start); a `Sampler` only from its previous head. Bounded by one segment |
| Record lookup | One `pread` of 4 KiB, or of the whole record if it is larger (it is validated before its timestamp is used); following a seal adds an `open`, `fstat`, header `pread` and another lookup | One record |

## CLI

`tickertape status <DIR>` rather than an `inspect` flag: `inspect` is a full checksum scan whose cost grows with the
journal, while `status` is meant to run every few seconds. Read-only, no writer lock. Output `text` (default),
`json` (one object; JSONL with `--watch`), or `prometheus` (text exposition format 0.0.4, for a node_exporter
textfile collector or a cron job). The JSON carries `"schema_version":1`; fields may be added without a version
change, while a removal or change of meaning bumps it. The `journal` field and label are the canonical absolute
path, so `./j` and `/abs/j` are one series.

- **Exit codes:** 0 healthy; 3 a threshold is breached (`--max-checkpoint-age`, `--min-headroom`,
  `--min-headroom-segments`, `--max-reader-lag`); 2 the sample found the journal corrupt or unopenable, consistent
  with `verify`: the head at invalid bytes, a checkpoint marker the writer refuses, a damaged newest segment header,
  an unsupported format. 2 takes precedence over 3. 1 for usage and I/O errors and a missing journal.
- **`--max-checkpoint-age`** checks the exposure's estimated age: breached when records are uncovered and the
  checkpoint is older than the limit, its age is unknown, or there is no checkpoint. An idle, fully checkpointed
  journal and an empty journal are healthy.
- **`--watch <INTERVAL>`** repeats until interrupted and reports health in each sample instead of exiting. A sample
  that fails is reported (a JSON object with `error`, a text `error` line, or a Prometheus file with
  `tickertape_up 0`) and the watch continues. Every Prometheus file carries `tickertape_status_timestamp_seconds`,
  so a writer of the file that stops is visible as a timestamp that stops advancing; alert on
  `time() - tickertape_status_timestamp_seconds`. Prometheus output with `--watch` needs `--output`, because
  repeated expositions in one stream are not a valid exposition.
- **`--output`** writes each sample to a new, uniquely named temporary file in the destination directory, created
  with `O_CREAT | O_EXCL` (it never follows a symlink or opens an existing file), and renames it over the target; the
  temporary file is removed on every error. The target must be outside the journal. A one-shot sample that fails
  still replaces the file (with `tickertape_up 0`), so a collector never keeps serving a stale healthy sample.

## Compatibility

- No format change. `docs/format.md` lists `readers/` as an advisory name outside the format, like `notify`.
- **v0.4.0 and `readers/`.** Two reviewers checked by reading that v0.4.0's writer open and recovery, `verify`,
  retention and `archive` never read or touch `readers/` (they list segment names, temporary segment names,
  `next-segment.tmp`, `checkpoint`, `notify`, `writer.lock` and `quarantine/` only), and one ran them. This change
  repeated the check by running the `v0.4.0` tag's own binaries (built from the tag) against a journal whose
  `readers/` held a live beacon, a stale one (its reader aborted), a junk file, a FIFO and a subdirectory
  (2026-11-19, macOS, APFS): `tt-writer` opened and recovered the journal twice (the second time with non-zero bytes
  written after the head of the newest segment), then `tickertape verify`, `inspect`, `archive --to <dir>
  --keep-segments 2` (which moved 6 sealed segments), `verify` again and `export --format jsonl --output` all
  exited 0, and `readers/` was unchanged afterwards (names, types, sizes and checksums compared). The script is
  `tests/compat/v0.4.0-ignores-readers.sh`; it is not part of CI (it builds the tag).
- **v0.4.0's `export --output`** walks the journal directory tree to reject an output file that is a hard link to a
  journal file. It fails if an entry disappears mid-walk (a beacon dropped at that moment) or a subdirectory can't be
  listed (another user's `readers/`): a transient failure of an export to a file, which a retry fixes. This change
  makes the walk skip entries that vanish and subdirectories it cannot list; the journal directory itself must still
  be listable.

## Consequences

- The CLI's "only `archive` changes a journal" statement still holds: `status` writes nothing into the journal, and
  `--output` must be outside it.
- Left out on purpose: automatic reader reporting; reader-aware retention; persisting the writer's retention errors
  for outside observers; one-shot append rates; scanning a segment just to report the last record's time in a
  one-shot sample; verifying the whole chain in every sample (that is `verify`'s job and costs what `Writer::open`
  costs); a `metrics`/OpenTelemetry integration in the core crate; any server. Each can be added later without
  breaking this API.
- Tests: unit tests for every derived value and edge case in `status/tests.rs` (no process spawning, no
  wall-clock assumptions); cross-process tests (`tests/status.rs`); the beacon protocol's interleavings, each forced
  deterministically with test-only pause points (`tickertape::fault::on_pause`, feature `fault-injection`), and a
  multi-process churn with crashes (`tests/beacon_races.rs`); CLI tests for the JSON schema, Prometheus output, exit
  codes and watch failures.
