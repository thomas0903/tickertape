# 0032: Operational metrics and reader beacons

- **Status:** proposed
- **Date:** 2026-11-19 (revised the same day after review rounds 1 and 2 of PR #8)

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
2. **A journal the writer would refuse to open never looks healthy, as far as the sample can tell.** What a sample
   can check cheaply, it checks as the writer does; what it can't (the segments between the checkpoint and the head,
   which it doesn't read), it marks unverified, in the data and in the text output. The rule for breaks in the chain
   (decided in review round 2): **a break the sample confirms from what it reads anyway** (the head at invalid bytes,
   a marker the writer refuses, including a marker in a directory without segments, and a segment that the listing's
   names and sizes alone prove missing) **is a corruption breach, exit code 2, and no exposure is summed across
   it.** No extra reads are spent on confirming more. **A break it can't see leaves the sample unverified**:
   `verified: false` in the JSON (`head`, `exposure`, `health`), `health ok (unverified)` in the text, and the
   exit-code documentation and `--help` say that 0 then does not mean that the writer would open the journal.
3. **Observing never modifies a journal, blocks or fails a reader or the writer, and is never defeated by junk.**
   Every file is opened non-blocking and must be a regular file; the observer never takes a lock anybody waits for;
   an unreadable beacon or an unusable beacon directory is reported, never fails the sample.

## What is measured, and where it comes from

| Value | Source | Accuracy and failure modes |
|---|---|---|
| **Head**: next sequence number, segment, offset | A `pread` scan of the newest segment from the checkpoint position if the marker points into it, otherwise from its first record. A `Sampler` resumes from its previous head (or from the checkpoint, if that is further on) only with proof that the newest segment is the file its previous samples scanned and still holds what they saw: same device, inode, header creation time and size, with the file kept open between samples so that its inode can't be reused (ext4 reuses inode numbers at once), and the last record those samples passed still at its offset (one 4 KiB `pread`). If the checkpoint marker moved to a position the previous scans passed, the slot there must be a record with the marker's sequence number (another `pread`), as a scan from it would require. Otherwise it scans afresh | Exact for what was committed in the newest segment when the scan passed. A torn copy of a record being appended is re-read 3 times before the head is reported `invalid` (as the reader does). If the scan ends at a seal and the successor exists (the writer rolled meanwhile), the directory is listed again (and the marker read again, so that listing and marker agree) and the new newest segment scanned instead, up to 16 times; so the head is `sealed` only if the successor did not exist when checked. **`verified`** says whether the sample (or the sampler's earlier samples of the same file) validated every record from the checkpoint position (without a marker: from the start of the oldest segment) to the head. When the checkpoint is in an older segment, the segments in between are not read, so a missing or damaged one is not noticed, and the head's sequence number is what the newest segment's name and records imply |
| **Checkpoint**: generation, `next_seq`, position | The marker's two slots, read with up to two 64-byte `pread`s between two `fstat`s, retried while the metadata changes. Read right after the directory listing; if the marker names a segment that listing lacks (the writer rolled and checkpointed after the listing), the directory is listed and the marker read again, until the segment is listed or the marker didn't change across a listing (retention never removes the checkpoint's segment, so it is then really missing; at most 3 times, then an I/O error) | Validated on every sample as `Writer::open` validates it (`docs/format.md` → Recovery, step 2): the marker's segment must exist with a valid header, and its offset must be aligned and inside the record area. Otherwise the marker has a `problem`, the writer refuses the journal, and the sample reports `checkpoint_invalid`, a corruption breach. A listed checkpoint segment that is gone when opened (retention removed it after the marker moved on) makes the sample start again; it is never reported as a problem (`samples_during_retention_are_never_corrupt`) |
| **Checkpoint age** (an estimate) | The marker file's modification time, compared with the observer's wall clock read **at the end of the sample**, after the marker and the beacons | The writer changes the marker by writing a slot at a checkpoint, so the time is the checkpoint's, at filesystem timestamp resolution. **Decision (round 3):** a writer that finds the marker short at open (something cut it) leaves it short, and extends it at its next checkpoint, after the slot write and the marker's barrier have succeeded; the age of a short marker is unknown (`marker_resized`). Before, the writer extended it at open and set its time to the Unix epoch as a sentinel, which a writer killed between the two steps, a `set_modified` failing with `EPERM` (a writer running as another user with group write permission) or a power loss (`fdatasync` doesn't persist a timestamp-only change) lost, and which made the file look 56 years old. Leaving it short needs no format change: every reader of the marker (the writer, durable-only readers, `verify`, `status`, and the v0.1.0 to v0.4.0 readers in `tests/compat/`) reads each slot on its own and accepts a short file (`docs/format.md`); a slot written into a short file may need a new block, which on a full volume fails that checkpoint (the preallocation that prevents this is restored by the first checkpoint that succeeds). A marker cut and then extended by something else (a v0.4.0 writer extends it at open, keeping the cut's time or setting the extension's) is recognised by its contents: the current slot's generation is 2 or more and the other slot doesn't hold the previous one, which every checkpoint leaves there (`older_slot_lost`). Reported as **unknown** when detectably unreliable: in the future; earlier (by more than 2 s, for coarse timestamps) than the creation time in the header of the marker's segment, which a checkpoint can't predate; the marker not at its full length (cut by something else); the older slot lost; or the metadata changing over 3 reads (a writer checkpointing very often: the checkpoint is then very recent, so this unknown age doesn't breach `--max-checkpoint-age`). Undetectable: something else writing or `touch`ing the file later, a copy that didn't keep times, a forward clock step. A sample taken while a checkpoint writes the marker may pair the new time with the previous slot (the kernel updates the time before copying the data). A checkpoint that fails before its slot write (at the data barrier) leaves the marker and its time alone, and one that fails at the marker's barrier never extends a short marker, so a failed checkpoint never makes a short marker's age known. If the marker's own barrier fails on a full-length marker, the slot and time are visible although the writer is poisoned (the slot's claim is true: the data it covers was made durable first; the slot itself may not survive a power loss): the in-process `Writer::metrics().poisoned` is authoritative there, and a checkpoint can't avoid it without a sentinel time, which this design gave up |
| **Exposure** (what a power loss can remove) | Records and segment-file footprint between the checkpoint position and the head; its age is the checkpoint's estimated age if any record is uncovered, else zero | Counts follow the head (`verified` likewise). Unknown if the marker has a problem, the listing proves a segment missing in between, or the head is at invalid bytes (round 3: such a sample is never `verified`, in the library, the JSON, the text and the Prometheus output). The local timestamp of the oldest uncovered record is looked up in every sample (one record read), never carried from an earlier sample, so a replaced segment can't leave a stale one. The age is an upper bound: every uncovered record was appended after the checkpoint. The local timestamp of the oldest uncovered record is reported as well, as data in the caller's clock |
| **Reader lag** (in process) | `Reader::cursor()` against a sample: records, and segment-file footprint between the two positions | The cursor's segment must be the file the sample listed under that name (device and inode; the reader holds it open, so the inode can't be reused). Records are exact; bytes are the footprint (below), zero when no record is unread, unknown while a seek is pending. A durable-only reader's lag is measured against the checkpoint, which it can't pass |
| **Reader lag** (other processes) | Opt-in reader beacons (below) | As fresh as the beacon's last write, and only if the beacon's segment identity matches this journal's file (else unknown, with the reason). The unread age is the observer's wall clock minus the `local_ts_ns` of the first unread record, so it is meaningful only when the producer stamps `local_ts_ns` from the same host's wall clock; a timestamp later than the observer's clock gives an unknown age, not zero. The clock is read after the beacons, so a beacon written during the sample still has an update age |
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
reason: `segment_gone` (retention removed it, or recovery moved it to `quarantine/`), `ahead_of_sample` (the
segment is newer than every segment the sample listed: the writer created it after the sample), or `other_history` (the beacon
belongs to another journal sharing the beacon directory, the journal was replaced at the same path, or recovery
repaired the segment after the reader read it: a repair writes a new file, and the old inode is still held by the
reader or kept in `quarantine/`, so it can't be reused while it matters). The header's creation time guards against
inode numbers that ext4 reuses at once. A durable-only reader's lag is measured against the checkpoint marker, since
it can never pass it; with no marker it can read nothing and its lag is zero.

**Cost.** Readers without a beacon do no work: nothing changes in `Reader`'s read path. `update` compares the
reader's whole cursor (slot, pending seek target, segment file, and whether the reader is durable-only) with the last
one written, so a seek that resolves at the same sequence number, or a reader of the other mode replacing one at the
same place (round 3: its lag is measured against another position), is published too; if it moved, it reads the monotonic clock and writes only if at least
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
   `.tmp`; all compared ignoring ASCII case, since on a case-insensitive filesystem (APFS by default) `CHECKPOINT`
   names the marker. A beacon directory may be the journal directory itself, so this keeps beacons away from journal
   files by construction.
2. Read the reader's segment header (from the reader's own descriptor) and encode the beacon.
3. Lock `D` exclusively (waiting if another creator holds it).
4. Remove temporary files that creators which died before publishing left: names of the form
   `.<name>.<pid>.<n>.beacon-tmp` that are regular files, unlocked, **and whose contents prove they are beacons**
   (below), if the name still names the file checked. Under the directory lock, an unlocked temporary file can't
   belong to a creator that is still working; but a name, a file type and a free lock don't prove where a file came
   from, and the directory may be the caller's, so the contents decide (decision below).
5. Create a new temporary file `.<name>.<pid>.<n>.beacon-tmp` with `O_CREAT | O_EXCL` (never follows a symlink,
   never opens an existing file), with the process's umask; note its identity (`fstat`); lock it exclusively (it is
   new, so this can't fail for contention); write the complete beacon.
6. Publish: `link` the temporary file to `name`. If that succeeds, unlink the temporary name if it still names the
   temporary file (`lstat`). Done.
7. If `name` exists, check it (takeover): open it `O_NONBLOCK | O_NOFOLLOW`; it must be a regular file. Probe its
   lock with a **shared** non-blocking `flock`: if that fails, a live owner holds it: refuse with
   `Error::BeaconInUse`. If it succeeds (release it at once), read it: it must be exactly 128 bytes that are a beacon
   (below, possibly torn); otherwise refuse with `Error::InvalidConfig` and leave it untouched. Then `lstat` the name: if it no
   longer names the file just checked, go back to step 6. Otherwise `rename` the temporary file over it, and check
   that the name now names the new beacon.
8. Unlock `D` explicitly (`flock(LOCK_UN)`), then close it.

**What proves a file is a beacon.** Exactly 128 bytes, every one of which (the checksum aside) a beacon writer puts at
that offset in some report (round 3; before, the magic and version alone decided, so a file of the magic, the version
and `0xff` was taken over or deleted as a leftover). The owner rewrites the whole report with one `pwrite`, so an
update cut short (a partial `pwrite` followed by an error, such as `EFBIG` under a file-size limit, whose owner then
died without its destructors) leaves a prefix of one valid report and the suffix of another, cut at one point. The
rule therefore uses per-byte conditions only, which every such mix satisfies, and checks every field the format fixes:
the magic `TTREADER` and version 1; flags with only bits 0 and 1 (byte 12 at most 3, bytes 13 to 15 zero); the owner's
pid, the same in every write to one file (only its creator writes it), a positive `pid_t` (not zero, below 2^31);
reserved bytes 20 to 23 and 96 to 123 zero; the slot's offset 8-byte aligned and below 2^40, the largest segment size
(the low byte's low three bits and bytes 53 to 55 zero); the segment size a multiple of 64 KiB and at most 2^40
(bytes 64 and 65 zero, byte 69 at most 1, bytes 70 and 71 zero). Cross-field conditions (an offset inside its
segment, a size of at least 64 KiB) are left out because a mix can break them. Observers require the same rule and
the checksum (`only_what_a_beacon_writer_could_write_is_a_beacon` checks every cut point of two very different
reports). The checksum is not required for a takeover: a torn beacon fails it, and without this rule its name would
be stranded for ever (every later creator would refuse it). Observers report a torn beacon `invalid`.

**Decision (round 2): torn updates are taken over, not prevented.** The alternatives were an update protocol that
keeps a valid report at all times (two alternating slots, or write-and-rename per update), which would change the
128-byte format or cost several system calls per update instead of one `pwrite`. Recognising a torn beacon by the
bytes no write changes keeps the format and the update cost, and the lock still decides liveness: a torn file is
taken over only if no process holds it. **Temporary files** are removed by the same rule, so a leftover is removed
only if its creator wrote it completely at least once; the empty file of a creator that died between creating and
writing it (a window of one system call) stays, a harmless hidden file of the creator's name, rather than letting a
name pattern justify deleting files in a directory the caller chose.

**Updating**: `pwrite` the 128 bytes at offset 0 of the locked file, only in the process that created the beacon.

**Removing** (dropping the beacon): in the process that created the beacon only, `lstat` the name; if it still
names this beacon's file, unlink it; then close the file, which releases the lock.

**Forked children.** A child created with `fork` gets a copy of the `ReaderBeacon` and of its descriptor. The beacon
records its creator's process id: in any other process the copy never writes the file, and dropping it does nothing,
so a child that exits never unlinks its parent's live beacon. **Decision (round 3):** the process is checked where
`update` would write, not on every call. A following reader may call `update` per record, and `getpid` is a system
call on Linux (about 75 ns in an aarch64 VM, measured; 1 ns on macOS, which caches it), several times the rest of an
`update` that writes nothing (a comparison, or a comparison and a 17 ns clock read). So in a child, `update` with an
unchanged cursor or within the interval returns `Ok(false)` without writing, exactly as in the creator; it fails
only when it would write, and `flush` always fails (`a_forked_child_never_writes_or_removes_the_beacon` asserts each
case).
The directory lock is released explicitly at step 8, so a child forked while its creator held it doesn't keep it
once the creator is done (`a_forked_child_does_not_keep_the_directory_lock`). If the creator dies while holding the
directory lock, a child forked meanwhile keeps it until the child exits or execs, and creators in that directory wait
for it.

**Observing** a beacon in `D` (no directory lock): skip names that are invalid or reserved and entries that are known
not to be regular files; open the name `O_NONBLOCK | O_NOFOLLOW` (a FIFO, socket, directory or symlink is skipped,
never opened blocking), `fstat` it (regular file; note its identity); read it (129 bytes, to see a longer file),
re-reading once on a bad checksum and up to 8 times while a live owner holds it; probe its lock with a shared
non-blocking `flock` and release it (`live` if it fails with `EWOULDBLOCK`, `stale` if it succeeds, `uncertain` on
any other error); then `lstat` the name: if it no longer names the file observed, start again (up to 3 times, then
report it as `changing`, without contents or lag: what was seen belongs to files that no longer hold the name). A
file that can't be opened or read (for example another user's beacon created under umask 077) is `unreadable`; one
that doesn't decode is `invalid`.

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
- *A creator that dies.* Before step 6, it leaves a temporary file, removed by the next creator (step 4) once it was
  written; between the `link` and the unlink of the temporary name, a second, harmless link to a now stale beacon,
  removed the same way. It never holds the directory lock beyond its death, unless a child it forked meanwhile
  survives it (above).
- *An owner that dies during an update.* A `pwrite` cut short leaves a torn beacon: observers report it `invalid`,
  and the next creator of the name takes it over, since it is unlocked and still a beacon by the rule above
  (`torn_stale_beacon_is_taken_over`).

**Limits, stated in the rustdoc and in `docs/operations.md`.** The lock belongs to the open file, not the process: a
child created with `fork` that keeps the descriptor keeps a dead reader's beacon live until the child exits. The
descriptor is close-on-exec, so `exec` drops it, but every child holds it from `fork` to `exec`: a beacon dropped
while another thread spawns a process can look live for that instant (the race tests run the spawning test alone
for this reason; with it running alongside, 48 of 200 runs on macOS saw such a moment). On network filesystems `flock` may be emulated or unsupported: Linux NFS
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
  It carries only the previous head, the last record it passed and the marker it saw, and resumes only with proof
  that the newest segment is the same, unchanged file (above: identity with the file kept open, the last record
  re-read, a moved marker checked); otherwise a replaced journal, a repaired segment or one restored from a copy is
  scanned afresh and rates are dropped. Nothing else (such as the oldest uncovered record's timestamp) is carried
  from one sample to the next. It holds that one file open between samples; if retention removes it meanwhile, its
  space is freed at the next sample.
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

What the code does, per call. Counted from the code and checked with `strace` on Linux (round 2: the earlier table
listed one `fstat` per opened file where the code makes up to three). Every `open` is followed by a `close`; a
`readdir` pass is `open`, `fstat`, two `getdents64` and `close`; clock reads go through the vDSO and make no system
call. `status()` keeps no file open after the call; a `Sampler` keeps one (below).

| Call | System calls | Scans records? |
|---|---|---|
| `Writer::metrics`, `Reader::cursor`, `Status::lag` | None (one monotonic clock read for `Writer::metrics`) | No |
| `ReaderBeacon::update` | A comparison if the cursor has not moved; else one monotonic clock read, and at most once per interval a `getpid`, a wall-clock read and one 128-byte `pwrite`, plus `fstat` and a 64-byte `pread` of the segment header after the reader moved to another segment file | No |
| `ReaderBeacon::create`/`create_in` | `mkdir` (`create` only); reading the reader's segment header (`fstat`, `pread`); `open` and `flock` of the directory (a second, blocking `flock` while another creator holds it); a `readdir` pass, plus per leftover temporary name `open`, `fstat`, two `flock`s, two `pread`s, `lstat` and `unlink`; `open` (exclusive create), `fstat`, `flock`, `pwrite`, `link`, `lstat`, `unlink` of the temporary file; `flock` (unlock) and `close` of the directory; one `getpid` (round 3: the source made three, the table said two; the
redundant lookups are gone). A takeover adds `open`, `fstat`, two `flock`s, two `pread`s, `lstat`, `rename`, `lstat` | No |
| `status()` / `Sampler::sample()`, normal path | Journal directory: a `readdir` pass and one `stat` per segment. Marker: `open`, three `fstat`s, two 64-byte `pread`s. Newest segment: `open`, three `fstat`s, a 64-byte header `pread`, and the scan: one `pread` per 1 MiB window. The checkpoint's segment and the oldest segment, if not the newest: `open`, three `fstat`s, a header `pread` each. With uncovered records the scan didn't pass: one record lookup (below). `statvfs`; `lstat` of the spare; `open`, `fstat` and two `read`s of `writer.lock`; `kill(pid, 0)`. Beacon directory: a `readdir` pass; per beacon `open`, `fstat`, two `pread`s (129 bytes, then the end of file), one `flock` (held: live) or two (free: stale, probe and release), `lstat`; per beacon whose segment isn't open yet, `open`, three `fstat`s, a header `pread`; per lagging beacon, one record lookup. A `Sampler` resuming in the same segment adds one 4 KiB `pread` (the last record it passed), and one more if the marker moved behind its head; one whose head moved to another segment opens the previous head's segment (`open`, three `fstat`s, `pread`) unless already open. It keeps the newest segment open until the next sample | The newest segment from the checkpoint (or its start); a `Sampler` only from its previous head. Bounded by one segment |
| `status()` / `Sampler::sample()`, retries | Marker metadata changing under the read: `fstat`, two `pread`s, `fstat` again, at most twice more. The marker naming a segment the listing lacks: the listing and the marker again, at most 3 times. The newest segment sealed: an `lstat` of its successor; if it exists (the writer rolled), the listing and the marker again and the new newest segment opened and scanned, at most 16 times. A slot that fails validation: re-read with one `pread`, at most 3 times. A file that vanished during the sample (retention, recovery): the whole sample again, at most 3 attempts. A beacon with a bad checksum: two more `pread`s, and while its owner holds it, up to 6 more reads with a `flock` probe each; a beacon whose name is replaced meanwhile: observed again, at most 3 attempts | — |
| Record lookup | One `pread` of 4 KiB, or of the whole record if it is larger (it is validated before its timestamp is used); following a seal adds an `open`, three `fstat`s, a header `pread` and another lookup | One record |

## CLI

`tickertape status <DIR>` rather than an `inspect` flag: `inspect` is a full checksum scan whose cost grows with the
journal, while `status` is meant to run every few seconds. Read-only, no writer lock. Output `text` (default),
`json` (one object; JSONL with `--watch`), or `prometheus` (text exposition format 0.0.4, for a node_exporter
textfile collector or a cron job). The JSON carries `"schema_version":1`; fields may be added without a version
change, while a removal or change of meaning bumps it. The `journal` field and label are the canonical absolute
path, so `./j` and `/abs/j` are one series. A journal missing when the command starts is not a start-up error but a
failed sample (round 2): the path is then taken as given, made absolute, so the failure is reported like any other
(`tickertape_up 0` in a one-shot `--output` file, which is never left holding a stale healthy sample; an error line
with `--watch`, which goes on sampling and picks the journal up once it exists).

- **Exit codes:** 0 healthy; 3 a threshold is breached (`--max-checkpoint-age`, `--min-headroom`,
  `--min-headroom-segments`, `--max-reader-lag`); 2 the sample found the journal corrupt or unopenable, consistent
  with `verify`: the head at invalid bytes, a checkpoint marker the writer refuses (also one in a directory without
  segments), a segment the listing proves missing between the checkpoint and the head, a damaged newest segment
  header, an unsupported format. 2 takes precedence over 3. 1 for usage and I/O errors and a missing journal. 0 with
  `head.verified` false means healthy as far as the sample can tell, not that the writer would open the journal
  (governing rule 2).
- **`--max-checkpoint-age`** checks the exposure's estimated age: breached when records are uncovered and the
  checkpoint is older than the limit, its age is unknown (except `marker_changing`, which means a checkpoint just
  happened), or there is no checkpoint. An idle, fully checkpointed
  journal and an empty journal are healthy.
- **`--watch <INTERVAL>`** repeats until interrupted and reports health in each sample instead of exiting. A sample
  that fails is reported (a JSON object with `error`, a text `error` line, or a Prometheus file with
  `tickertape_up 0`) and the watch continues. Every Prometheus file carries `tickertape_status_timestamp_seconds`,
  so a writer of the file that stops is visible as a timestamp that stops advancing; alert on
  `time() - tickertape_status_timestamp_seconds`. Prometheus output with `--watch` needs `--output`, because
  repeated expositions in one stream are not a valid exposition.
- **`--output`** writes each sample to a new, uniquely named temporary file in the destination directory, created
  with `O_CREAT | O_EXCL` (it never follows a symlink or opens an existing file), and renames it over the target; the
  temporary file is removed on every error. Checked before every replacement, the target must not lie inside a
  directory the journal uses (the journal directory, `quarantine/`, the beacon directory), compared as given and with
  symlinks resolved on both sides (round 3: a `quarantine/` that is a symlink to another directory, which recovery
  accepts, holds evidence too), and must not be a file that an entry of the journal directory or of `quarantine/`
  reaches, following symlinks or by hard link (a segment, the marker or a quarantined file may be a symlink to a file
  outside the journal, which the rename would replace). The archive directory of `tickertape archive` is not
  recorded anywhere `status` can see, so it is not checked. A sample that fails still replaces the file (with
  `tickertape_up 0`), so a collector never keeps serving a stale healthy sample. **Decision (round 3):** when the
  journal can't be walked (unreadable, not a directory, a symlink loop), the sample has failed too; the check then
  decides from what it can establish without the walk: the target is not inside those directories by path, and is
  not the file the failed sample's error names. If so, `tickertape_up 0` is written and a watch goes on. If even that
  can't be established (a beacon directory that can't be resolved, a target that can't be `stat`ed, a successful
  sample whose journal can't be walked a moment later), nothing is written, the reason goes to standard error every
  time, and a watch goes on; a one-shot status exits 1. Before this, the check's own walk failed first and left a
  stale `tickertape_up 1` file.

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
  makes the walk skip, **inside `readers/` only**, entries that vanish and a directory it cannot list. Everywhere
  else it fails closed as v0.4.0 does: an unreadable `quarantine/` (which may hold the only copy of repaired-away
  bytes) or a segment that vanishes mid-walk is an error, never a reason to truncate the output.

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
