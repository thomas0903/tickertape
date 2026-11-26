# tickertape on-disk format, version 1

Status: **accepted** (M1, approved by the owner on 2026-10-31). This document is normative: the code
must match it. Rationale: ADRs [0002](decisions/0002-record-format.md) (record format),
[0003](decisions/0003-publication-protocol.md) (publication and memory access),
[0005](decisions/0005-segment-rolling.md) (segments and rolling),
[0007](decisions/0007-checkpoint-marker-and-recovery.md) (checkpoint marker and recovery) and
[0035](decisions/0035-recovery-preserves-a-committed-tail.md) (committed records after an empty commit word).

## Conventions

- All integers are **little-endian**, unsigned unless stated. The library refuses to compile on big-endian targets.
- Offsets are in bytes from the start of the file. `align8(x)` rounds `x` up to a multiple of 8, computed in `u64`.
- "CRC32C" is CRC-32 with the Castagnoli polynomial (`0x1EDC6F41`, reflected, init `0xFFFFFFFF`, final XOR
  `0xFFFFFFFF`). Check value: `crc32c(b"123456789") == 0xE3069283`.
- A **commit word** is the 4-byte-aligned `u32` at the start of every record or seal. The writer stores it with
  release ordering; readers load it as described in ADR 0003. All other bytes are written with ordinary stores and
  only before their commit word.

## Journal directory

A journal is one directory. Only these names have a meaning; anything else is ignored and never touched.

| Name | What it is |
|---|---|
| `{base_seq:020}.tape` | A segment. `base_seq` is the sequence number of its first record, zero-padded to 20 decimal digits so that lexicographic order equals numeric order, e.g. `00000000000001048576.tape` |
| `{base_seq:020}.tape.tmp` | A segment being created or repaired by the writer. Never read by readers |
| `next-segment.tmp` | The next segment, prepared in the background before it is needed (ADR 0022): preallocated, synced and all zeros. At roll the writer writes its header and links it to `{base_seq:020}.tape` (the next checkpoint syncs it, ADR 0038). Never read by readers |
| `{base_seq:020}.tape.{pid}.{unix_ns}.removing` | A segment that an archiver other than the writer (`retention::apply`, `tickertape archive`) is removing (ADR [0055](decisions/0055-an-archiver-takes-a-segment-out-before-it-lets-it-go.md)): renamed here out of the journal, checked, then unlinked, or renamed back if the check fails. A crash in between leaves it; a later retention run unlinks it once it is an hour old (`unix_ns`), its process (`pid`) is gone and `base_seq` is below a valid marker's segment. Never read by readers, the writer or its recovery; tickertape 1.0.0 ignores it |
| `checkpoint` | The checkpoint marker: where the last successful checkpoint ended (see "Checkpoint marker") |
| `notify` | Wake-up record (ADRs 0017, 0021): 80 bytes naming the running writer's shared-memory wake-up region (magic `TTNOTIFY`, version, kind, nonce, pid and descriptor on Linux or object name on macOS, journal directory identity, CRC32C; layout in ADR 0021). Readers only read it with `pread`; it is replaced by rename, never written in place. Carries no journal data; deleting it or damage to it only affects reader latency |
| `writer.lock` | Lock file. The writer holds an exclusive `flock` on it while open and writes its PID (decimal ASCII) into it. The PID is informational; the lock is the authority |
| `quarantine/` | Bytes and segments removed from the journal by recovery, kept so that they are not destroyed (bytes recovery zeroes in place are not kept: "Recovery (writer open)", step 4). Never read by readers, never deleted by tickertape |
| `readers/` | Optional reader beacons (ADR [0032](decisions/0032-operational-metrics.md)): 128-byte files (magic `TTREADER`) in which applications that opt in publish a reader's position for monitoring, plus hidden temporary files (`.<name>.<pid>.<n>.beacon-tmp`) while one is created. Not part of format version 1: the writer, readers, recovery and retention never read or modify it (v0.4.0 included) |
| `seek-index/` | Optional seek indexes, one file per sealed segment (ADR [0045](decisions/0045-sparse-seek-index.md), "Seek index" below), derived from the segments and safe to delete. Not part of format version 1: readers read it only with `ReaderConfig::seek_index`, the writer's open removes the files of segments that recovery may change (Recovery, step 2a), and retention removes a segment's file after the segment. Version 1.0.0 ignores it |

Segments are never removed automatically unless a writer retention policy is configured.
`tickertape archive` or `tickertape::retention` can remove a contiguous prefix of sealed segments strictly before
the checkpoint segment; without a valid marker nothing is eligible (ADR [0016](decisions/0016-retention-and-archiving.md)).
The checkpoint segment and everything after it remain. A retained journal can therefore begin above sequence 0.

Sequence numbers start at 0 and increase by exactly 1 per record across the whole journal, including across
segments. The first segment is `00000000000000000000.tape`. The largest sequence number a record can have is
`u64::MAX - 1`; an append beyond it fails.

## Segment file

A segment is a fixed-size file, preallocated to its full size when it is created, never resized, and never replaced
except by a repair (see "Recovery").

```
0                64                                                size-8     size
├────────────────┼─────────────────────────── record area ─────────────┼──────────┤
│ segment header │ record │ record │ … │ seal, or zeros                 │ trailer  │
└────────────────┴─────────────────────────────────────────────────────┴──────────┘
```

- `segment_size` is a multiple of 65,536 (64 KiB), at least 65,536 and at most 2^40. Default: 134,217,728 (128 MiB).
- **Records** start at offset 64, each at a multiple of 8, and must end (including padding) at or before
  `segment_size - 8`. The last 8 bytes, the **trailer**, never hold a record, so a seal always fits after the last one.
- A **seal** starts where the next record would have started, at some `P <= segment_size - 8`, and occupies
  `[P, P + 8)`. It may lie in the trailer.
- Every byte that holds no record or seal is zero. Preallocation guarantees that a new segment reads as zeros.

### Segment header (64 bytes, offset 0)

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 8 | `magic` | ASCII `TICKTAPE` (`54 49 43 4B 54 41 50 45`) |
| 8 | 2 | `format_version` | `1` |
| 10 | 2 | `header_len` | `64`. Records start at this offset |
| 12 | 4 | `flags` | `0`. Reserved; a version 1 reader rejects a non-zero value |
| 16 | 8 | `segment_size` | Total file size in bytes. Must equal the actual file size |
| 24 | 8 | `base_seq` | Sequence number of the first record in this segment. Must match the file name |
| 32 | 8 | `created_unix_ns` | Writer's wall-clock time at creation, nanoseconds since the Unix epoch. Informational |
| 40 | 20 | `reserved` | Zero |
| 60 | 4 | `header_crc` | CRC32C of bytes `[0, 60)` |

The header is written before the segment becomes visible (see "Rolling") and is not modified afterwards; recovery
writes it again only where it never reached the disk (crash contract C8).

A segment is **rejected** (an error, not a silent skip) if: the magic differs; `format_version` is not supported
(version 1 readers support exactly `1`); `header_len != 64`; `flags != 0`; any `reserved` byte is non-zero;
`header_crc` doesn't match; `segment_size`
breaks the constraints above or differs from the file size; or `base_seq` doesn't match the file name.

### Record (offset `P`, a multiple of 8)

| Offset | Size | Field | Meaning |
|---|---|---|---|
| P+0 | 4 | `len` | **Commit word.** Frame length: 40 (header) + payload length. `0` = no record here (yet). `0xFFFFFFFF` = seal |
| P+4 | 4 | `crc` | CRC32C of `len` (its 4 bytes) followed by bytes `[P+8, P+len)` |
| P+8 | 8 | `seq` | Sequence number |
| P+16 | 4 | `schema_id` | Caller-defined event type / schema identifier. Opaque to tickertape |
| P+20 | 4 | `flags` | `0`. Reserved; a version 1 reader treats a non-zero value as invalid |
| P+24 | 8 | `venue_ts_ns` | Caller-supplied timestamp, e.g. the exchange's event time. `0` = not set |
| P+32 | 8 | `local_ts_ns` | Caller-supplied timestamp, e.g. local receive time. `0` = not set |
| P+40 | `len-40` | `payload` | Opaque bytes |
| P+len | `align8(len)-len` | padding | Zero, not covered by the CRC |

The bytes `[P+4, P+len)` are the record's **body**. The next record or seal starts at `P + align8(len)`.

**Timestamps** are data, not ordering: unsigned nanoseconds since the Unix epoch (1970-01-01T00:00:00Z) by
convention, never interpreted by the library. The caller picks the clock; `tickertape::clock::realtime_ns()` reads
the system wall clock (`CLOCK_REALTIME` semantics: it can jump, and nanosecond units do not imply nanosecond
accuracy). **Sequence numbers are the only authority on order.** Monitoring (a reader's unread age in `status`,
`latency::RecordAge`) compares `local_ts_ns` with a clock; no read, order or validity rule depends on it.

A record at `P` in a segment of size `Z`, read while expecting sequence number `S`, is **valid** if and only if:

1. `40 <= len <= 0xFFFFFFFE`;
2. `P + align8(len) <= Z - 8`;
3. `crc` matches;
4. `seq == S`;
5. `flags == 0`.

Maximum payload length for segment size `Z`: `min(Z - 64 - 8 - 40, 0xFFFFFFFE - 40)` bytes. Larger appends fail with
an error; records never span segments.

### Seal (offset `P`, 8 bytes)

| Offset | Size | Field | Value |
|---|---|---|---|
| P+0 | 4 | commit word | `0xFFFFFFFF` |
| P+4 | 4 | `seal_crc` | CRC32C of the 4 bytes `FF FF FF FF` followed by `next_seq` as a `u64` |

`next_seq` is the sequence number of the next record, which is the `base_seq` of the next segment. A seal is
**valid** for an expected sequence number `S` if `seal_crc` matches `S` and it is not at offset 64 (an empty segment
is never sealed, so a seal there would name its own segment). A reader that finds a valid seal continues
at offset 64 of `{S:020}.tape`; an invalid seal is corruption.

## Checkpoint marker

`checkpoint` is a 131,072-byte file with two 64-byte **slots**, at offsets 0 and 65,536. All other bytes are zero.
The slots are 64 KiB apart so that they fall in different filesystem blocks for every block size up to 64 KiB (the
ext4 maximum), and a torn write of one slot is unlikely to damage the other; this assumes the storage's failure unit
is no larger than that. If both slots are damaged anyway, recovery falls back to a full scan (tested).

The writer creates the file (all zeros, so neither slot is valid), syncs it, and syncs the journal directory when it
creates the journal, before the first segment. A writer that finds an existing marker shorter than 131,072 bytes (something cut it)
leaves it so at open, writes its checkpoints' slots into it, and extends it to its full length (zeros) after the first
checkpoint whose slot and marker sync succeed. Readers accept a short marker (below). Generations start at 1. A checkpoint that would need generation
`u64::MAX` fails.

| Offset in slot | Size | Field | Value |
|---|---|---|---|
| 0 | 8 | `magic` | ASCII `TICKCKPT` |
| 8 | 2 | `format_version` | `1` |
| 10 | 6 | `reserved` | Zero |
| 16 | 8 | `generation` | Incremented by 1 on every checkpoint; the slot written is `generation % 2` |
| 24 | 8 | `next_seq` | Every record with `seq < next_seq` was durable when this slot was written |
| 32 | 8 | `segment_base_seq` | The segment that contains the position of `next_seq` |
| 40 | 8 | `offset` | Offset of `next_seq`'s record (or of a seal) in that segment |
| 48 | 12 | `reserved` | Zero |
| 60 | 4 | `slot_crc` | CRC32C of bytes `[0, 60)` of the slot |

The **current checkpoint** is the valid slot (magic, version, CRC, reserved fields zero) with the higher
`generation`; if both are valid with the same generation (which a tickertape writer never produces), the slot at
offset 65,536 wins. Each slot is checked on its own, so a file too short for one slot can still hold a valid other
slot. If neither slot is valid, or the file is missing, there is no checkpoint. A slot is written only after
everything it describes is durable (see `crash-contract.md` → What `checkpoint()` does), so recovery can trust the
journal before it. Readers ignore this file unless `ReaderConfig::durable_only(true)` is enabled (and except that seeks read it to decide
where to start: ADR [0046](decisions/0046-seeks-that-need-no-index.md), and, with a seek index, which segments a
durable-only or mapped reader may use one for: crash contract C15; a visible zero-copy reader also bounds its loads by
it, ADR [0048](decisions/0048-publication-frontier.md), which changes no record or error it returns): then only records
with `seq < next_seq` are returned, and a missing or invalid marker covers no records (ADR
[0010](decisions/0010-durable-only-reads.md), crash contract C13).

A slot describes the journal's **current** files: the segments that the segment paths named when it was written,
and, because recovery never changes a record before the marker, the files those paths name at any later time. It says
nothing about a file a reader opened earlier and that a repair has since replaced, nor about bytes after the
previous marker that a reader read before this one was written, which a repair in place may have changed. A reader
may therefore apply a marker to the segment it has open only if it has confirmed, after reading the marker, that the
segment's path still names that file, and it must then read the records the marker newly covers from the file again
(ADR [0034](decisions/0034-durable-only-reads-check-segment-identity.md); see "What readers do about repairs").

## Publication

The writer appends a record at `P` in this order:

1. Write the body `[P+4, P+len)` with ordinary stores. The bytes `[P+len, P+align8(len))` and everything after are
   already zero (an invariant the writer maintains; see "Abandoned writes").
2. **Release-store** `len` into the commit word at `P`.

A reader at `P` observes the commit word first and the body only afterwards, ordered as described in ADR 0003 (an
acquire load from the mapping, or a `pread` followed by an acquire fence). `0` means "nothing yet". Any other value
means the body is visible, so the reader reads and validates it. Committed bytes are never modified by a correctly working writer on an intact chain; after storage damage that recovery cannot tell from a torn append, it zeroes published records in place ("Recovery (writer open)", step 4).

A seal is published the same way (`seal_crc` first, then a release store of `0xFFFFFFFF`), and only after the next
segment file is fully created and in place. This orders *visibility* between processes on the same host. It says
nothing about when bytes reach the disk; see `crash-contract.md`.

### Abandoned writes

If the writer starts writing a body and then does not commit it (the caller's fill closure panics, or declines through
`Writer::try_append_with`), it zeroes every byte it may have written and makes that zeroing durable (`msync`, a data
sync and the platform barrier) before anything else is appended. If that fails, the writer refuses further appends and
checkpoints and reports why (`Writer::abandoned_write_error`). Treat that as a storage incident, like a failed
checkpoint (`crash-contract.md` → Errors are sticky): a new writer opened in the same boot accepts appends again, but
recovery may see the zeroed bytes in the page cache while the disk still holds the stale ones, so nothing is guaranteed
about the records written since the last successful checkpoint across a power loss. A body can contain bytes that look
like a valid record for the next sequence number, so leftover bytes must never remain past the end of a shorter record
that is committed later.

## Rolling

When the next record doesn't fit in the current segment's record area, the writer rolls:

1. If the current segment holds no record, the append fails with `RecordTooLarge` (rolling would need the same file
   name). This only happens when the configured segment size was raised since the segment was created.
2. Remove any existing `{next_seq:020}.tape.tmp` name (`unlink`; it may be a leftover hard link to an already
   published segment, which is unaffected), then create it with `O_CREAT | O_EXCL`, preallocate it to `segment_size`
   (Linux: `fallocate`; macOS: `fcntl(F_PREALLOCATE)` then `ftruncate`) and write the segment header with `pwrite`.
   The roll does not sync it: the next checkpoint syncs every segment linked before it is requested, before it writes
   its marker, and recovery restores a header that an OS crash kept from the disk (ADR 0038; crash contract C8). A
   segment that recovery creates is synced before it is linked. A temporary file is never truncated or reopened, so a
   published inode can't be damaged through it.
3. Publish it under its final name **without replacing anything**: `link` the temporary file to
   `{next_seq:020}.tape`, which fails if that name exists. **A successful `link` is the publication point.** Then
   `unlink` the temporary name; if that fails, the leftover name is ignored (it is removed at the next open).
4. Write the seal into the old segment at `P`.
5. Append the record at offset 64 of the new segment.

If step 2 fails, or `link` fails (for example, the disk is full), the writer removes the temporary file, the append
returns an error, the old segment stays unsealed and usable, and the next append retries the roll. After a successful
`link` the roll always continues with steps 4 and 5; it is never retried from step 2. A record larger than the
maximum payload for the configured segment size fails with `RecordTooLarge` without rolling.

## Recovery (writer open)

Readers never modify a journal. Only the writer repairs it, on open, after taking the lock and before appending.

1. **Remove stale temporary files** (`*.tape.tmp` and `next-segment.tmp`). They are never segments (at most a
   second name for a published one).
2. **Find the scan start.** With a current checkpoint (see "Checkpoint marker"): its segment must exist and
   its `offset` must be a multiple of 8 in `[64, segment_size - 8]`; otherwise the journal is **refused** with a
   corruption error under every policy, because the history the marker vouches for is missing and recreating that
   segment could make the stale marker look valid again. Recovery then starts there, expecting `next_seq`, and
   trusts everything before. Without a current checkpoint, it starts at offset 64 of the oldest segment, expecting
   that segment's `base_seq`. The **recovery domain** is the start segment and every segment with a larger
   `base_seq`.

   **2a. Remove the seek indexes of the recovery domain.** After the marker's segment and offset have been validated,
   and before anything below changes a segment: if `seek-index/` exists and is a directory (not a symlink), list it
   twice, each time through a directory stream opened for that pass, and unlink in each pass every temporary name and
   every name beginning `{base:020}.idx` whose base is at or above the start of the recovery domain (every base, if there
   is no current checkpoint). Then sync it. All of it is best effort: what fails is counted in the `RecoveryReport` and
   never stops the open. Indexes of segments older than the oldest one go after recovery. ("Seek index" below; the two
   passes, and why they suffice against a builder running meanwhile: ADR
   [0045](decisions/0045-sparse-seek-index.md).)
3. **Scan forward** through the chain, validating every record and seal: record by record within a segment; at a
   valid seal, into the segment it names. A segment the scan enters whose header never reached the disk (all zero)
   gets its header back first (crash contract C8). The scan stops at the first position `P` (in segment `X`, of size `Z`)
   that is not a valid record and not a valid seal whose successor exists.
4. **Classify what is at `P`.** At a commit word `0`, where `S` is the expected sequence number, recovery decides
   in a fixed order, stated exactly in crash contract C11 and ADR 0035 ("How recovery tells them apart", "Damage
   around a seal"). First, whether `P` is where `X`'s seal belongs, in front of a segment named `{S:020}.tape` that
   opens (second row below). Otherwise, if `[P+4, Z)` holds any non-zero byte, whether it shows **committed history**
   after `P`: in the common case a later record or final seal in `X` with a sequence number its position allows, or
   a segment with a `base_seq` greater than `S`; the search is bounded and only reads, and a stop at its bound counts
   too (ADR 0035 has the rules and their exceptions). Committed history leads to the first row below; otherwise `P`
   is the end of the chain (third row). A writer process crash usually leaves no committed history; the exception, a
   torn payload whose bytes the search counts, is in crash contract C1.

   | At `P` | Meaning | Action |
   |---|---|---|
   | Commit word `0`, committed history after it | Usually storage damage, power loss or outside modification that hid committed records behind an empty commit word (C1 has the exception) | Corruption, as in the last row, reported as `EmptyCommitWord(T)` (or `UncheckedTail` when the search stopped at its bound): `Quarantine` repairs by replacement; `Fail` refuses before anything in `X` is changed |
   | Commit word `0`, and `P` is where `X`'s seal belongs (a segment named `{S:020}.tape` exists, is not `X`, has not been scanned and opens) | Usually the previous writer died between publishing the next segment and sealing this one; or the seal was damaged | When at most the seal's CRC word is non-zero after `P`: zero it in place. Otherwise, as in the last row (`BadSeal`). Then write the seal and continue scanning in that segment (step 3), which is checked like any segment (C11 (1)) |
   | Commit word `0`, neither of the above | Usually the clean end, or a body the previous writer never committed (no reader saw it); also damage that left no evidence, and then this removes published records in place (C11 (3); "What readers do about repairs") | Zero any non-zero bytes after `P` in place, without a copy, under either policy. This is the end of the chain; segments the scan did not reach are disconnected (step 5). In front of a segment named `{S:020}.tape` that exists, is not `X`, has not been scanned and does not open, nothing is zeroed: non-zero bytes after `P` are corruption, as in the last row, reported as `BrokenChain(S)` (C11 (3); ADR 0035 → Damage around a seal) |
   | Valid seal, successor missing, no segment with a larger `base_seq` | Crash or power loss during a roll | Create the successor (as in "Rolling"). This is the end of the chain |
   | Valid seal, successor missing, a segment with a larger `base_seq` exists | A segment in the middle of the chain is missing (deleted by someone, or lost) | **Refuse** under every policy: tickertape doesn't discard a large part of a journal on its own |
   | Commit word non-zero, invalid record or seal | Not possible after a writer *process* crash. Power loss, storage corruption or outside modification | `Quarantine` (default): preserve the bytes from `P` to the last non-zero byte of `X` in `quarantine/`, then **repair by replacement**: write a copy of `X` with `[P, Z)` zeroed to `X.tmp`, sync it, and `rename` it over `X`. `Fail`: refuse (see below) |

   Bytes are zeroed in place only after an empty commit word, in the second and third rows (crash contract C8). Some
   damage cannot be told from a torn append and is zeroed like one, destroying the records it covers without a copy
   (C11 (3), ADR 0035 → What cannot be detected).

5. **Disconnected segments.** Every segment in the recovery domain that the scan did not visit (whatever its
   `base_seq`, including one between visited segments) is not part of the valid chain. `Quarantine`: preserve it
   by moving it into `quarantine/`. `Fail`: refuse. (A segment with an invalid header is usually such a segment;
   where the scan itself has to open it, the open fails or the segment is replaced: crash contract C8.)

   With `Fail`, a refusal leaves the bytes at the corruption position it reports, and everything after them in that
   segment, untouched, and moves no file. The open is not read-only, though: the steps above run in order, and what
   an open may change before it refuses is listed in crash contract C11.
   Segments below the recovery domain were covered by an earlier checkpoint; files placed in the directory by other
   means are outside the contract.
6. **Persist the repairs** before returning: every in-place zeroing, seal and new file is synced, and the journal
   directory (and `quarantine/`) is synced after any create, rename or link, with the platform barrier
   (`crash-contract.md` → What `checkpoint()` does, steps 1–3). If nothing was repaired but `seek-index/` exists, the
   barrier is applied to that directory's own descriptor, best effort, so that the removals of step 2a reach the drive
   before anything is appended. No segment is synced for it: a discarded sync error of a segment's data could hide from
   the next checkpoint that the data is lost (`crash-contract.md` → Errors are sticky).

**Preserving data in `quarantine/`.** Before replacing or moving a segment, and aborting the repair with an error if
any step fails (bytes zeroed in place are not preserved: crash contract C8):

1. If `quarantine/` doesn't exist: create it, then sync the journal directory.
2. Bytes (a tail, from the corrupt position to the last non-zero byte of its segment): write them to a new file `….partial` created with `O_EXCL`, sync it, then publish it with `link` to
   `{base_seq:020}-{offset:012}-{unix_ns}-{n}.bin` (`n` = 0, 1, … until a name is free; `link` never replaces), unlink
   the `.partial` name, and sync `quarantine/`.
   Whole segments: sync the segment file's data, `link` it to a free name in `quarantine/` (as above), sync
   `quarantine/` and apply the platform barrier, and only then unlink its old name and sync the journal directory.
3. Apply the platform barrier (macOS: `F_FULLFSYNC`).

An interrupted recovery leaves either the original or the preserved copy (or both); the next open redoes the
repair. Leftover `.partial` files are kept as evidence. Records before `P` are never modified.

Every action is reported in the writer's `RecoveryReport`. Recovery work is: listing the directory; reading every
record after the scan start (the data written since the last checkpoint); reading `[P, Z)` of the last segment to find
non-zero bytes and, up to the last of them, checking each 8-byte position for committed history (step 4); and, only
for repairs, copying the affected segment or tail.

### What readers do about repairs

A repair replaces a segment file with a new inode, or moves it into `quarantine/`. A reader checks that its
segment's path still names the file it has open (same device and inode) when it gets a validation error, when it has
waited at an empty slot for 100 ms or more, and before it follows a seal into the next segment. If the path names a
different file or no file, the reader returns `Error::Discarded` and doesn't advance; it never continues at its old
offset in the new file, because offsets after the repair point don't line up. The application calls `seek` to
continue: `seek` scans the current chain from the start of a segment, so it only lands on validated positions. (A
forward seek in the reader's own segment may scan from the reader's position instead, but only if a marker that the
reader read before it passed the records before that position covers them, the path still names the file the reader
has open, and the record before the position is still there. ADR [0046](decisions/0046-seeks-that-need-no-index.md).
With `ReaderConfig::seek_index`, a seek may start at a record a seek index names; the records before it are not read,
and the place is on the current chain except as crash contract C15 states.)

A durable-only reader also checks when a checkpoint marker it has just read moves past its cursor, after reading the
marker and before it reads any record the marker newly covers. The order matters: recovery replaces files before the
new writer's first checkpoint, so a successful check after the read shows that the open file was current after the
marker was written, and so holds exactly the records the marker covers. A check before the read could pass just
before a repair and a new checkpoint, and the reader would then apply the new marker to the old file (ADR
[0034](decisions/0034-durable-only-reads-check-segment-identity.md) has the full argument). If the check fails, the
reader returns `Error::Discarded` without applying the marker. This also applies when retention removed the segment:
a durable-only reader may learn of it there rather than at the segment's end. If the check passes, the reader reads
every record the marker newly covers from the file's current bytes: the safe reader discards whatever it had read
into its buffer before, because bytes after a marker can change in place (below).

Not every repair replaces the file. When storage damage empties the commit word of a committed record after the
checkpoint and leaves no committed history after it that recovery can find (damage indistinguishable from a torn
append, such as the newest record's commit word zeroed; crash contract C11 (3)), recovery takes it for the end of
the records and zeroes it and the non-zero bytes after it in place (step 4, third row), and the next writer appends a different history into the same file. The file keeps
its identity, so readers can't notice. A durable-only reader is unaffected: it returned only records below a marker,
which recovery keeps, and it reads the rest from the current bytes once a later marker is trusted; its position is a
record boundary of the new history, so it simply reads on. A visible reader may return the removed records it had
already read or buffered, and then continue at its old position in the new history without `Error::Discarded`. Where
the records of the two histories line up it reads the new ones; otherwise its position falls inside a record, and it
waits there (`Ok(None)`) or reports `Error::Corrupt` for bytes that aren't corrupt, until the application seeks to its
position. (Damage that also destroyed the marker, or set it back to an older one, can let recovery remove covered
records in place; a seek to the reader's position or forward in its segment then goes on from there if the new history
holds, right before it, bytes that form a record with the expected sequence number, also for a durable-only reader. A
seek to the last record the reader passed, or an earlier one, scans the segment, and so does a new reader. ADR [0046](decisions/0046-seeks-that-need-no-index.md).) It would return a record that is not in the journal only if the new history held a frame with a valid
checksum and the expected sequence number at that position (ADR
[0034](decisions/0034-durable-only-reads-check-segment-identity.md) → In-place repairs).

A seal can also disappear from a file that keeps its identity: when storage damage zeroes a whole seal and its
successor no longer opens, or zeroes a stretch from the start of a record after the checkpoint through the end of the
seal, nothing is left after the empty commit word, so recovery changes nothing in that file and moves the successor
into `quarantine/` as disconnected (crash contract C11 (3); ADR 0035 → Damage around a seal; step 5). (Had any byte
been left after the empty word in front of a successor that does not open, recovery would preserve it and replace
the file under `Quarantine`, and readers would get `Error::Discarded`.) A reader may have read the seal before, and the safe reader may
still hold it in its buffer. So a reader at a seal whose successor is missing while its own
segment is still in place reads the slot again from the file before it reports a broken chain. If the seal is gone,
it reads the slot as it is now: it waits there (`Ok(None)`) or returns the new history's record, without
`Error::Discarded`, like any reader across an in-place repair. (When the whole seal was zeroed, the chain ends at the
seal's own slot, so the reader's position starts a record of the new history. Where the damage began earlier in the
segment, at an empty commit word with no committed history found after it, the reader's position can fall inside a
new record, and it then waits or reports corruption there as described above.) If the seal is there, a
live writer stored it only after linking its successor, so the reader tries the successor once more and reports
`CorruptReason::BrokenChain` only if it is still missing. Such a report can be transient, and a later call then
follows the successor: for example while a recovery replaces a successor whose header is damaged (it keeps the
seal, moves that successor into `quarantine/` and then creates a new one), or after a power loss that kept the seal
but not the successor's directory entry, until a writer opens and creates the successor.

Until it checks, a visible reader may return records from a segment that recovery has just preserved and removed.
Such records are like records lost to a power loss that a reader had already seen (`crash-contract.md` → C8). A
durable-only reader never does (C13). A reader never moves from a removed history into the repaired one without
returning `Error::Discarded`, except across an in-place repair, which it can't notice (above).

## Versioning

`format_version` changes whenever a version 1 reader could misread a file written by the new code. A reader accepts
only versions it knows and fails clearly on others. Reserved fields and flags must be zero, so a future version can
give them a meaning. The CRC algorithm, endianness and header lengths are fixed for version 1.

## Seek index (not part of format version 1)

Optional files, one per sealed segment, that let a reader which opts in (`ReaderConfig::seek_index`) start a seek in the
middle of a segment. They are derived from the segments and may be deleted at any time; a journal means the same with and
without them. They have a version of their own (below) and `format_version` says nothing about them. What a reader
requires of one before it uses it, and what a wrong one can do: crash contract C15 and ADR
[0045](decisions/0045-sparse-seek-index.md), which also has the reasons. Conventions are those above (little-endian,
unsigned, offsets from the start of the file, CRC32C).

In the subdirectory `seek-index/` of the journal directory only these names have a meaning; anything else there is
ignored and never touched:

| Name | What it is |
|---|---|
| `{base_seq:020}.idx` | The index of segment `{base_seq:020}.tape`: exactly 20 ASCII digits, value at most `u64::MAX - 1`, then `.idx` |
| `{base_seq:020}.idx*` | Any other name that begins so is reserved for later kinds of index. Never read; removed by the writer's open like an index |
| `.{base_seq:020}.{pid}.{n}.idx-tmp` | An index of any kind, now or later, being written: exactly this form, `pid` and `n` decimal, 1 to 20 digits each (`n` starts at the builder's `CLOCK_REALTIME` in nanoseconds). Never read by readers |

**File.** A 128-byte header, then `entry_count` entries of 24 bytes. The file is exactly `128 + 24 * entry_count`
bytes long.

| Offset | Size | Field | Value in version 1 |
|---|---|---|---|
| 0 | 8 | `magic` | ASCII `TICKTIDX` (`54 49 43 4B 54 49 44 58`) |
| 8 | 2 | `index_version` | `1` |
| 10 | 2 | `header_len` | `128` |
| 12 | 2 | `entry_len` | `24` |
| 14 | 2 | `reserved` | `0` |
| 16 | 4 | `entry_count` | 1 to 65,535 |
| 20 | 4 | `spacing` | Block size in bytes, a power of two (rule below) |
| 24 | 8 | `seal_offset` | Offset of the segment's seal |
| 32 | 8 | `next_seq` | The sequence number the seal names. The segment holds `next_seq - base_seq` records |
| 40 | 20 | `reserved` | Zero |
| 60 | 4 | `file_crc` | CRC32C of bytes `[0, 60)` followed by bytes `[64, file length)` |
| 64 | 64 | `segment_header` | The segment's bytes `[0, 64)`, verbatim (its size, `base_seq`, `created_unix_ns` and CRC) |

| Offset in entry | Size | Field | Value |
|---|---|---|---|
| 0 | 8 | `seq` | Sequence number of the record |
| 8 | 8 | `offset` | Offset `P` of its commit word in the segment |
| 16 | 4 | `len` | The record's commit word, bytes `[P, P+4)` |
| 20 | 4 | `crc` | The record's stored CRC32C, bytes `[P+4, P+8)` |

**Spacing.** For a segment of size `Z`, `spacing(Z)` is the smallest power of two that is at least 65,536 and at least
`Z / 65,536`, so 64 KiB up to 4 GiB and 16 MiB at 2^40. Block `k` is `[k * spacing, (k+1) * spacing)`. There is one
entry for each `k >= 1` in which a record of the chain starts: the first that does. Block 0 needs none (its first
record is at 64). An index therefore has at most 65,535 entries (1,572,968 bytes) whatever the segment size. A segment
that would have no entry (every 64 KiB segment) gets no file. For a given spacing the content is a function of the
segment's bytes alone, so two builders of one version write the same file.

**Validity.** A reader uses an index only if all of this holds; otherwise it does what it does without one:

1. It is a regular file, owned by the segment's owner or root, and not writable by its group or others; its length is
   `128 + 24n` for `n` in 1 to 65,535, and `entry_count` is `n`.
2. The magic, `index_version` (exactly 1), lengths, reserved fields and `file_crc` are as above; `spacing` is a power of
   two from 4,096 to 2^30.
3. `segment_header` equals the first 64 bytes of the segment, byte for byte. With `Z` and `base` from it: `seal_offset`
   is a multiple of 8 in `[104, Z - 8]` and `next_seq > base`.
4. Every entry has `base < seq < next_seq`, `offset` a multiple of 8 and at least `spacing`, `40 <= len <= 0xFFFFFFFE`
   and `offset + align8(len) <= seal_offset`. Both `seq` and `offset / spacing` increase strictly. The records between
   fit: `64 + 40 * (seq - base) <= offset` for the first entry, and from each entry to the next, and from the last to
   `(next_seq, seal_offset)`, `offset_i + align8(len_i) + 40 * (seq_{i+1} - seq_i - 1) <= offset_{i+1}`.
5. The 8 bytes of the segment at `seal_offset` are the seal for `next_seq`.
6. The 8 bytes of the segment at the chosen entry's `offset` are its `len` and `crc`, and the record there is valid
   (above) with sequence number `seq`.

**Versions.** The version changes when a version 1 reader could misread the file. A reader ignores every version but its
own. Readers do not depend on `spacing` (they search `seq`), so a later builder may choose another power of two. Removal
goes by name and ignores content. A builder writes the file completely under its temporary name and renames it into
place; the files are not synced, so a power loss can leave an empty, torn or zero file, which the checks above reject.

**Test vector.** Segment `00000000000000001000.tape`: size 131,072, `base_seq` 1000, `created_unix_ns`
1700000000000000000; 1,259 records with `schema_id` 1, both timestamps equal to `seq`, payload 64 bytes of `0x5A`;
seal at 131,000 for 2259. Only record 1630, at 65,584, is indexed. The index, 152 bytes (released v1.0.0's `verify`
accepts this segment):

```
0000  54 49 43 4b 54 49 44 58 01 00 80 00 18 00 00 00
0010  01 00 00 00 00 00 01 00 b8 ff 01 00 00 00 00 00
0020  d3 08 00 00 00 00 00 00 00 00 00 00 00 00 00 00
0030  00 00 00 00 00 00 00 00 00 00 00 00 c3 ec 87 c9
0040  54 49 43 4b 54 41 50 45 01 00 40 00 00 00 00 00
0050  00 00 02 00 00 00 00 00 e8 03 00 00 00 00 00 00
0060  00 00 2a 36 fe 9c 97 17 00 00 00 00 00 00 00 00
0070  00 00 00 00 00 00 00 00 00 00 00 00 5c f1 81 ff
0080  5e 06 00 00 00 00 00 00 30 00 01 00 00 00 00 00
0090  68 00 00 00 92 7f f1 5a
```
