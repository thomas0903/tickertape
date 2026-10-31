# tickertape on-disk format, version 1

Status: **draft for M1 approval**. This document is normative: the code must match it. Design rationale is in
ADR [0002](decisions/0002-record-format.md) (record format), [0003](decisions/0003-publication-protocol.md)
(publication protocol) and [0005](decisions/0005-segment-rolling.md) (segments and rolling).

## Conventions

- All integers are **little-endian**, unsigned unless stated. The library refuses to compile on big-endian targets.
- Offsets are in bytes from the start of the segment file. `align8(x)` rounds `x` up to a multiple of 8.
- "CRC32C" is CRC-32 with the Castagnoli polynomial (`0x1EDC6F41`, reflected, init `0xFFFFFFFF`, final XOR
  `0xFFFFFFFF`), as used by iSCSI, ext4 and many storage systems. `crc32c(b"123456789") == 0xE3069283`.
- A "commit word" is a 4-byte-aligned `u32` that the writer stores with **release** ordering and readers load with
  **acquire** ordering (see "Publication" below). Every other byte is written with ordinary stores.

## Journal directory

A journal is one directory. Only the files below have a meaning; anything else is ignored.

| Name | What it is |
|---|---|
| `{base_seq:020}.tape` | A segment. `base_seq` is the sequence number of the segment's first record, zero-padded to 20 decimal digits so that lexicographic order equals numeric order. Example: `00000000000001048576.tape` |
| `{base_seq:020}.tape.tmp` | A segment being created by the writer. Never read by readers. The writer removes stale ones on open |
| `writer.lock` | Lock file. The writer holds an exclusive `flock` on it while open, and writes its PID (decimal ASCII) into it for diagnostics. The PID is informational only; the lock is the authority |
| `quarantine/` | Bytes the writer removed during recovery (see "Recovery"), kept so that nothing is ever silently destroyed |

Sequence numbers start at 0 and increase by exactly 1 per record across the whole journal, including across
segments. The first segment is `00000000000000000000.tape`.

## Segment file

A segment is a fixed-size file, preallocated to its full size when it is created, and never resized afterwards.

```
0                64                                          size-8     size
├────────────────┼──────────────────── data area ──────────────┼──────────┤
│ segment header │ record │ record │ … │ seal or zeros │ zeros  │ reserved │
└────────────────┴─────────────────────────────────────────────┴──────────┘
```

- `segment_size` is a multiple of 65,536 (64 KiB), at least 65,536 and at most 2^40. Default: 134,217,728 (128 MiB).
- The data area is `[64, segment_size - 8)`. Records are packed from offset 64, each starting at a multiple of 8.
- The last 8 bytes are reserved so that a seal (8 bytes) always fits after the last record.
- Bytes that hold no record are zero. Preallocation guarantees that a new segment reads as zeros.

### Segment header (64 bytes, offset 0)

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 8 | `magic` | ASCII `TICKTAPE` (`54 49 43 4B 54 41 50 45`) |
| 8 | 2 | `format_version` | `1` |
| 10 | 2 | `header_len` | `64`. Records start at this offset |
| 12 | 4 | `flags` | `0`. Reserved; a reader of version 1 rejects a non-zero value |
| 16 | 8 | `segment_size` | Total file size in bytes. Must equal the actual file size |
| 24 | 8 | `base_seq` | Sequence number of the first record in this segment. Must match the file name |
| 32 | 8 | `created_unix_ns` | Writer's wall-clock time at creation, nanoseconds since the Unix epoch. Informational |
| 40 | 20 | `reserved` | Zero |
| 60 | 4 | `header_crc` | CRC32C of bytes `[0, 60)` |

The header is written once, before the segment becomes visible (see "Rolling"), and never changed.

A reader **rejects** a segment (error, not a silent skip) if: the magic differs; `format_version` is not one it
supports (version 1 readers support exactly `1`); `header_len != 64`; `flags != 0`; `header_crc` doesn't match;
`segment_size` breaks the constraints above or differs from the file size; or `base_seq` doesn't match the file name.

### Record (offset `P`, a multiple of 8 inside the data area)

| Offset | Size | Field | Meaning |
|---|---|---|---|
| P+0 | 4 | `len` | **Commit word.** Frame length in bytes: 40 (header) + payload length. `0` = no record here (yet). `0xFFFFFFFF` = seal |
| P+4 | 4 | `crc` | CRC32C of `len` (its 4 bytes) followed by bytes `[P+8, P+len)` |
| P+8 | 8 | `seq` | Sequence number |
| P+16 | 4 | `schema_id` | Caller-defined event type / schema identifier. Opaque to tickertape |
| P+20 | 4 | `flags` | `0`. Reserved; a version 1 reader treats a non-zero value as invalid |
| P+24 | 8 | `venue_ts_ns` | Caller-supplied timestamp, e.g. the exchange's event time. `0` = not set |
| P+32 | 8 | `local_ts_ns` | Caller-supplied timestamp, e.g. local receive time. `0` = not set |
| P+40 | `len-40` | `payload` | Opaque bytes |
| P+len | `align8(len)-len` | padding | Zero, not covered by the CRC |

The next record (or the seal) starts at `P + align8(len)`.

**Timestamps** are data, not ordering. They are unsigned nanoseconds since the Unix epoch (1970-01-01T00:00:00Z)
by convention, and the library never interprets them. The caller picks the clock: `tickertape::clock::realtime_ns()`
reads the system wall clock (`CLOCK_REALTIME` semantics: it can jump, and nanosecond units do not imply nanosecond
accuracy). **Sequence numbers are the only authority on order.**

A record at `P`, read while expecting sequence number `S`, is **valid** if and only if all of these hold:

1. `40 <= len` and `len != 0xFFFFFFFF`;
2. `P + align8(len) <= segment_size - 8`;
3. `crc` matches;
4. `seq == S`;
5. `flags == 0`.

Maximum payload length = `segment_size - 64 - 8 - 40` bytes (the whole data area minus one record header), and the
frame length must fit in a `u32`. Larger appends fail with an error; they never split across segments.

### Seal (offset `P`, 8 bytes)

When the writer rolls to a new segment, it writes a seal at the position `P` where the next record would have gone:

| Offset | Size | Field | Value |
|---|---|---|---|
| P+0 | 4 | commit word | `0xFFFFFFFF` |
| P+4 | 4 | `seal_crc` | CRC32C of the 4 bytes `FF FF FF FF` followed by `next_seq` as a `u64` |

`next_seq` is the sequence number of the next record, which is the `base_seq` of the next segment. A reader that
expects `S` at `P` and finds a seal checks `seal_crc` against `S`; if it matches, it continues at the start of
`{S:020}.tape`. A seal whose `seal_crc` doesn't match is corruption.

## Publication

The writer appends a record at `P` in this order:

1. Write bytes `[P+4, P+len)`: CRC, sequence, schema id, flags, timestamps, payload (ordinary stores).
2. **Release-store** `len` into the commit word at `P`.

A reader at `P` **acquire-loads** the commit word. `0` means "nothing yet": it polls again later. Any other value
means everything the writer wrote before that store is visible, so the reader validates and reads the record.
Committed bytes are never modified or truncated afterwards by a correctly working writer.

The seal is published the same way: `seal_crc` first, then a release store of `0xFFFFFFFF`, and only after the next
segment file is fully created and in place.

This orders *visibility* between processes on the same host. It says nothing about when bytes reach the disk; see
`crash-contract.md`. The memory-model assumptions behind this are in ADR 0003.

## Recovery (writer open)

Readers never modify a journal. Only the writer, on open, repairs the tail. It scans the newest segment from offset 64,
validating records with expected sequence numbers starting at `base_seq`, and stops at the first position `P` that
is not a valid record:

| What is at `P` | Meaning | What the writer does |
|---|---|---|
| Commit word `0` | Clean end, or a record that was being written when the writer died (its commit word was never stored, so no reader ever saw it) | Zero any non-zero bytes from `P` to the end of the data area, then append from `P` with the next sequence number |
| Valid seal | The segment was sealed but the next segment is missing (a crash or power loss during a roll) | Create the next segment, then continue in it |
| Commit word non-zero, record invalid | Cannot result from a writer *process* crash under this protocol. Indicates power loss, storage corruption or outside modification | Default policy `Quarantine`: copy bytes `[P, last non-zero byte]` to `quarantine/{base_seq:020}-{P:012}.bin`, fsync it, then zero them and append from `P`. Policy `Fail`: return a corruption error and change nothing |

The writer also checks the segment before the newest one. If it is not sealed (a crash between creating the new
segment and writing the seal), the writer scans it, checks that it ends exactly at the new segment's `base_seq`, and
writes the seal. If the newest segment's header is invalid (possible only after power loss), the policy decides: move
the file into `quarantine/` (`Quarantine`) or fail (`Fail`).

Every action taken is reported in the writer's `RecoveryReport`. Records before `P` are never touched.

## Rolling

When a record doesn't fit in the remaining data area, the writer rolls:

1. Create `{next_seq:020}.tape.tmp`, preallocate it to `segment_size` (Linux: `fallocate`; macOS:
   `fcntl(F_PREALLOCATE)` then `ftruncate`), write the segment header with `pwrite`, and `fdatasync` it.
2. Rename it to `{next_seq:020}.tape` (atomic within a directory).
3. Write the seal into the old segment at `P`, then unmap the old segment.
4. Append the record at offset 64 of the new segment.

If step 1 or 2 fails (for example, the disk is full), the append returns an error, the old segment stays unsealed and
fully usable, and the next append retries the roll.

## Versioning

`format_version` changes whenever a version 1 reader could misread a file written by the new code. A reader accepts
only versions it knows and fails clearly on others. Reserved fields and flags must be zero, so a future version can
give them a meaning. The CRC algorithm, endianness and header length are fixed for version 1.
