# tickertape on-disk format, version 1

Status: **draft for M1 approval** (revised after the cross-provider review). This document is normative: the code
must match it. Rationale: ADRs [0002](decisions/0002-record-format.md) (record format),
[0003](decisions/0003-publication-protocol.md) (publication and memory access),
[0005](decisions/0005-segment-rolling.md) (segments and rolling) and
[0007](decisions/0007-checkpoint-marker-and-recovery.md) (checkpoint marker and recovery).

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
| `checkpoint` | The checkpoint marker: where the last successful checkpoint ended (see "Checkpoint marker") |
| `writer.lock` | Lock file. The writer holds an exclusive `flock` on it while open and writes its PID (decimal ASCII) into it. The PID is informational; the lock is the authority |
| `quarantine/` | Bytes and segments removed from the journal by recovery, kept so that nothing is silently destroyed. Never read by readers, never deleted by tickertape |

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

The header is written before the segment becomes visible (see "Rolling") and is never modified.

A segment is **rejected** (an error, not a silent skip) if: the magic differs; `format_version` is not supported
(version 1 readers support exactly `1`); `header_len != 64`; `flags != 0`; `header_crc` doesn't match; `segment_size`
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
accuracy). **Sequence numbers are the only authority on order.**

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
**valid** for an expected sequence number `S` if `seal_crc` matches `S`. A reader that finds a valid seal continues
at offset 64 of `{S:020}.tape`; an invalid seal is corruption.

## Checkpoint marker

`checkpoint` is an 8,192-byte file with two 64-byte **slots**, at offsets 0 and 4,096 (separate 4 KiB blocks, so that
a torn write of one slot cannot damage the other). All other bytes are zero.

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
`generation`. If neither slot is valid, or the file is missing, there is no checkpoint. A slot is written only after
everything it describes is durable (see `crash-contract.md` → What `checkpoint()` does), so recovery can trust the
journal before it. Readers ignore this file.

## Publication

The writer appends a record at `P` in this order:

1. Write the body `[P+4, P+len)` with ordinary stores. The bytes `[P+len, P+align8(len))` and everything after are
   already zero (an invariant the writer maintains; see "Abandoned writes").
2. **Release-store** `len` into the commit word at `P`.

A reader at `P` loads the commit word with acquire semantics. `0` means "nothing yet". Any other value means the body
is visible, so the reader validates and reads it. Committed bytes are never modified by a correctly working writer.

A seal is published the same way (`seal_crc` first, then a release store of `0xFFFFFFFF`), and only after the next
segment file is fully created and in place. This orders *visibility* between processes on the same host. It says
nothing about when bytes reach the disk; see `crash-contract.md`.

### Abandoned writes

If the writer starts writing a body and then does not commit it (a panic inside the caller's fill closure, or an I/O
error), it zeroes every byte it may have written, makes that zeroing durable (`msync` + sync of the range), and only
then continues. A body can contain bytes that look like a valid record for the next sequence number, so leftover
bytes must never remain past the end of a shorter record that is committed later.

## Rolling

When the next record doesn't fit in the current segment's record area, the writer rolls:

1. If the current segment holds no record, the append fails with `RecordTooLarge` (rolling would need the same file
   name). This only happens when the configured segment size was raised since the segment was created.
2. Create `{next_seq:020}.tape.tmp` (`O_CREAT | O_TRUNC`, so a retry reinitialises it), preallocate it to
   `segment_size` (Linux: `fallocate`; macOS: `fcntl(F_PREALLOCATE)` then `ftruncate`), write the segment header with
   `pwrite`, and `fdatasync` it.
3. Publish it under its final name **without replacing anything**: `link` the temporary file to
   `{next_seq:020}.tape` (which fails if that name exists), then `unlink` the temporary name.
4. Write the seal into the old segment at `P`.
5. Append the record at offset 64 of the new segment.

If step 2 or 3 fails (for example, the disk is full), the writer removes the temporary file, the append returns an
error, the old segment stays unsealed and usable, and the next append retries the roll. A record larger than the
maximum payload for the configured segment size fails with `RecordTooLarge` without rolling.

## Recovery (writer open)

Readers never modify a journal. Only the writer repairs it, on open, after taking the lock and before appending.

1. **Remove stale temporary files** (`*.tape.tmp`). They were never segments.
2. **Find the scan start.** With a current checkpoint, start at its (`segment_base_seq`, `offset`), expecting
   `next_seq`; everything before is trusted. Without one, or if that segment is missing, start at offset 64 of the
   oldest segment, expecting its `base_seq`.
3. **Scan forward** through the chain, validating every record and seal: within a segment, record by record; at a
   valid seal, into the segment named by the seal's sequence number. The scan stops at the first position `P` (in
   segment `X`) that is not a valid record or a valid seal with an existing successor.
4. **Classify what is at `P`:**

   | At `P` | Meaning | Action |
   |---|---|---|
   | Commit word `0` | Clean end, or a body the previous writer never committed (no reader ever saw it) | Zero any non-zero bytes in `[P, Z)` in place, and sync them |
   | Valid seal, successor missing | Crash or power loss during a roll | Create the successor (as in "Rolling"), continue in it |
   | Commit word non-zero, invalid record or seal | Not possible after a writer *process* crash. Indicates power loss, storage corruption or outside modification | Policy `Quarantine` (default): copy `[P, Z)` to `quarantine/`, then **repair by replacement**: write a copy of `X` with `[P, Z)` zeroed to `X.tmp`, sync it, and `rename` it over `X`. Policy `Fail`: return a corruption error and change nothing |

5. **Check the rest of the directory.** If the scan ended in `X` with commit word `0` and a segment named
   `{next_seq:020}.tape` exists, the previous writer crashed between steps 3 and 4 of a roll: seal `X` at `P`, then
   continue scanning in that segment (back to step 3). Any *other* segment with `base_seq` greater than `X`'s is
   disconnected from the valid chain (power loss or corruption): with `Quarantine`, move each into `quarantine/`
   (rename); with `Fail`, return an error. A segment whose header is invalid is treated the same way.
6. **Persist the repairs.** Every in-place zeroing, seal and new file is synced, and the journal directory (and
   `quarantine/`) is synced after any create, rename or link, before the first new record is published.

Quarantine files are created with exclusive create (never replacing a file) under a unique name
`{base_seq:020}-{offset:012}-{unix_ns}.bin`, written as `….bin.partial`, synced, renamed to `.bin`, and the directory
synced, before any destructive step. A `.partial` file left by an interrupted recovery is kept as evidence; the repair
is simply redone.

Every action is reported in the writer's `RecoveryReport`. Records before `P` are never modified. Recovery time is
proportional to the data written since the last checkpoint.

### What readers do about repairs

A repair replaces a segment file (new inode) or moves it into `quarantine/`. A reader that gets a validation error,
or has been waiting at an empty slot for 100 ms or more, checks whether its segment's path still names the file it
has mapped (same device and inode). If the path names a different file, the reader maps the new one and retries at
the same offset; the bytes before that offset are identical. If the path no longer exists, the records from the
reader's position on were discarded by recovery, and the reader returns an error saying so.

## Versioning

`format_version` changes whenever a version 1 reader could misread a file written by the new code. A reader accepts
only versions it knows and fails clearly on others. Reserved fields and flags must be zero, so a future version can
give them a meaning. The CRC algorithm, endianness and header lengths are fixed for version 1.
