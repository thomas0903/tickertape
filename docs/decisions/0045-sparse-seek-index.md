# 0045: A sparse, rebuildable seek index next to sealed segments

- **Status:** proposed (design only, nothing is built; issue #59)
- **Date:** 2026-11-25
- **Follows up:** [0042](0042-replay-and-recovery-at-scale.md), option 2 (deferred there for 1.0; its decision stands
  until this record is accepted)

## Context

`Reader::seek(seq)` opens the segment whose name covers `seq` and validates every earlier record in it. Measured on an
M4 Pro with a warm cache: 16–19 ns per skipped 64-byte record, 21–25 ms to the last record of a full 128 MiB segment
(1,290,554 records). ADR 0042 left a sparse sidecar index for later. A Chronicle Queue user has asked about this cost,
and the owner wants the index. For comparison (read at tag `chronicle-queue-2026.8`): `RollCycles.DEFAULT` is
`FAST_DAILY`, which indexes every 256th entry, and `SCQIndexing.linearScan0` steps from the indexed entry over message
headers, without a checksum.

Constraints: format version 1 does not change; nothing is added to `append`; no sync on the writer's thread; the index
is a hint; released 1.0.0 software must be unaffected by the files; C ABI version 1; no weaker guarantee or changed
documented meaning in 1.x (ADR 0043). `Reader::seek`'s documentation says that every record on the way is validated,
and the type's says that readers never write. Both bind this design.

## Decision in brief

- One file per **sealed** segment, `seek-index/{base_seq:020}.idx` in the journal directory: one entry for the first
  record in each 64 KiB block of the segment. 0.04 % of the segment, derived; the files can be deleted at any time.
- Built by `tickertape index` (a timer, or after `archive`), **never by a reader**, and in the first release not by
  the writer (Deferred). Only segments **strictly before the segment of a durable checkpoint marker** are indexed,
  which no recovery from that marker changes; also every sealed segment of a directory that has no marker and no
  running writer.
- A reader uses it only with `ReaderConfig::seek_index(true)` (**off by default** in 1.x). It validates the record it
  lands on and every record from there, and falls back to today's scan on any doubt. An index never causes an error.
- The writer's open removes the index files of every segment recovery may change, before recovery changes anything,
  best effort: what it cannot remove is still checked by value.
- The active segment gets nothing.

## File format, version 1

Conventions are those of `format.md`: little-endian, unsigned, offsets from the start of the file, CRC32C as there.

**Names.** In the journal directory, the subdirectory `seek-index/`. In it, only these names have a meaning; anything
else there is ignored and never touched:

| Name | What it is |
|---|---|
| `{base_seq:020}.idx` | The index of segment `{base_seq:020}.tape`: exactly 20 ASCII digits, value at most `u64::MAX - 1`, then `.idx` |
| `{base_seq:020}.idx*` | Any other name that begins so is reserved for later kinds of index. Never read; removed with the segment's index |
| `.{base_seq:020}.{pid}.{n}.idx-tmp` | An index being written (`pid`, `n`: decimal, 1 to 20 digits each; `n` is the builder's `CLOCK_REALTIME` in nanoseconds). Never read by readers |

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

**Why bytes, why 64 KiB.** A count of records bounds nothing useful: 256 records are 26 KiB at 64 B and more than a
segment at 1 MiB. With byte blocks, the records skipped between the landing record and the target start in one block
and end before the target, so a jump **validates less than 64 KiB of skipped records** (at most 1,638 of the smallest,
40 bytes) at any record size; records above 64 KiB each have an entry. 64 KiB is also the safe reader's chunk, so a
walk normally needs one or two reads. Finer is slower, not faster, because the whole file is read and checked (Cost
and benefit).

**Why `len` and `crc` per entry.** They tie an entry to one record, not to any record with that number: a changed
history that reuses the number at that offset differs in these 8 bytes unless the frame is the same. They need no
extra read. (A digest of the whole segment would need the whole segment read to check.)

**Why no device, inode, or modification time.** Copying, restoring and archiving a journal must keep its index usable.

**Evolution.** A reader uses only `index_version == 1` with exactly the lengths and zero reserved fields above; any
other file is ignored (scan). The version changes when a version 1 reader could misread the file. Readers do not
depend on `spacing` (they search `seq`), so a later version 1 builder may choose another power of two from 4,096 to
2^30 within the entry limit, and a builder keeps any valid version 1 file it finds, whatever its spacing (Building,
step 1). Without `--rebuild`, a builder never replaces a file whose magic is right and whose version is higher than it
knows. Removal and invalidation go by name and ignore content. `seek-index/` is not part of format version 1 and has
its own version, like `readers/` (ADR 0032).

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

## What a reader does

With `seek_index(true)`, `seek(target)` does this after it has chosen and opened segment `F` as today (header read and
validated), or kept the one it has open where forward seeks continue from the cursor (the parallel change). Any
failure below, of any kind (missing file, I/O error, failed check, no memory), means **what the seek does without an
index**: scan from offset 64, or continue from the cursor where that is the rule, with the cursor and buffer put back.
It is never returned as an error.

1. **Choose whether to look.** Not for `target == base`, nor in a segment of at most 64 KiB. For a forward seek in the
   current segment, only when the estimated distance `(target - next_seq) * (offset - 64) / (next_seq - base)`,
   computed in 128 bits, is at least 256 KiB (always, when the cursor is at offset 64), and the chosen entry is used
   only if its `seq` is greater than the cursor's.
2. **Durable-only readers** (C13) read the checkpoint marker now, and go on only if it is valid and
   `marker.segment_base_seq > F.base`. Visible readers skip this step.
3. **Open.** Open `seek-index` in the journal directory with `O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC` (a
   symlink counts as absent), and in it `{base:020}.idx` with `openat` and
   `O_RDONLY | O_NONBLOCK | O_NOFOLLOW | O_CLOEXEC` (no FIFO can block it). From `fstat`: a regular file; its length
   is `128 + 24n` for `n` in 1 to 65,535 (checked before allocating); its owner is the segment's owner or root; not
   writable by group or others.
4. **Read once** into a buffer (no mapping, so no `SIGBUS`), and check: magic, version, lengths, reserved fields,
   `file_crc`, `entry_count` equal to `n`, `spacing` a power of two from 4,096 to 2^30. The copied `segment_header`
   must equal `F`'s first 64 bytes byte for byte (`created_unix_ns` is informational in `format.md`, but a segment
   never changes it). With `Z` and `base` from it: `seal_offset` is a multiple of 8 in `[104, Z - 8]`;
   `next_seq > base`; every entry has `base < seq < next_seq`, `offset` a multiple of 8, `offset >= spacing`,
   `40 <= len <= 0xFFFFFFFE`, `offset + align8(len) <= seal_offset`; both `seq` and `offset / spacing` increase
   strictly; and for consecutive positions (from `(base, 64)` through the entries to `(next_seq, seal_offset)`) the
   offset grows by at least 40 bytes per sequence number. All arithmetic is checked.
5. **Identity, after the index was read** (and, for a durable-only reader, after the marker was): the segment's path
   names `F`'s device and inode.
6. **Seal.** The 8 bytes of `F` at `seal_offset` are `FF FF FF FF` and `seal_crc(next_seq)`.
7. **Choose** the entry with the greatest `seq <= target`. If there is none, that is a failure as above.
8. **Land.**
   - **8a.** Read exactly 8 bytes of `F` at `offset` with `pread` (mapped readers too). They must equal the entry's
     `len` and `crc`. Nothing has been allocated or read for the frame yet.
   - **8b.** Set the cursor to `(offset, seq)`, drop the read buffer, and read that slot once, fresh, through the
     reader's normal slot read, given the expected commit word: the word it observes must equal `len` **before** it
     allocates or reads a body. It must be a valid record as `format.md` defines (length, bounds, CRC32C over the
     whole frame, `seq`, `flags`), and its stored CRC must equal `crc`. An empty word fails.
9. Keep the cursor; set `skip_until = target`. From here nothing is new: the next read validates the landing record
   again and every record after it, and follows seals with the identity checks of today.

The index is read again at each seek that uses it and not cached, so the only state a jump leaves in the reader is the
cursor.

## Trust: what a wrong index can and cannot do

**T1. Whatever the index holds,** every record returned is a frame in the segment file that meets all five validity
rules of `format.md` at its position, with the sequence number expected, and nothing below `target` is returned. A
jump sets only `(offset, seq)`; every record still goes through the unchanged slot read. An index cannot cause an
error, a panic or a hang. Memory: the index file (at most 1.5 MiB) and, in 8b, one frame of `len` bytes, read only
when 8 bytes of the segment equal the entry's; step 4 bounds it by the segment's data, as for a genuine record.
Without 8a and the expected word, the slot read (`fill_buf`) would size its buffer by whatever word it meets, up to
the segment size, before any comparison.

What T1 does not say is that the landing offset is a **record boundary of the current chain**, rather than bytes
inside a payload that happen to be a valid frame with the right sequence number; and the records before it are not
read ("What is given up").

**T2. An index that matches its segment points at boundaries.** The builder records an offset only after it has walked
the chain from offset 64 and validated every record and the final seal.

**T3. The segment of an index does not change while the marker it was built under stands.** Recovery starts at the
marker and changes nothing before it (`format.md` → Recovery, step 2; ADR 0034, F2 and F3), retention removes only
whole files, and a finished segment is never written again. A segment can come back into the recovery domain only if
an older marker, or none, becomes current: by damage to the marker, by an operator restoring files, or by a power loss
that drops a slot the writer had written and not yet synced. The last needs no damage; the builder excludes it by
making the slot it relies on durable before it walks, as `archive` does (`retention::durable_checkpoint`, C14).
Recovery from an older marker then changes the segment only where it does not validate.

**T4. An index-aware writer removes the index first.** At open, when the marker is known and before recovery's scan,
it unlinks the index files of every base at or above the start of the recovery domain. Where that works, an old index
is never paired with a file recovery made, nor the reverse:

- *`F` was made by recovery* (freshly opened, or held since an earlier seek): its predecessor's index had been
  unlinked before `F` existed, and the reader reads the index while it holds `F`.
- *`F` is the old file, the index belongs to its replacement* (more common once readers keep `F` open across seeks):
  at step 5, after the index was read, the path still names `F`, so `F` had not been replaced when the index was read;
  identities do not come back while `F` is open (ADR 0034, F4).
- *A repair in place keeps the file.* A reader that had read the index before the unlink may still land, but only
  where 8a and 8b find the very frame indexed: a boundary the repair kept or, as for any reader across an in-place
  repair (C11), one just before it.

This is a guarantee only where the unlink works: the writer opens anyway when it does not (Life cycle), and a 1.0.0
writer never unlinks. Therefore:

**T5. The rest is caught by value, and must suffice alone** (a 1.0.0 writer's repair, an index that could not be
removed, files mixed by an operator, a power loss while the index was written):

| The index is | Caught by |
|---|---|
| Missing, unreadable, a directory, symlink, FIFO or device; `seek-index` a symlink | Step 3 |
| Empty, truncated, longer, all zeros, partly written, bit-flipped, another version | Steps 3 and 4 (length; `file_crc`) |
| Of another segment or journal with the same base; of a deleted and recreated journal; of a segment whose header recovery wrote again | Step 4 (`segment_header`) |
| Of this segment before a repair kept the header (a tail replaced or zeroed) and the writer went on | Step 6 unless the new history ends at the same offset and sequence number; then 8a (before a byte of a frame is read), then 8b |
| Of the repaired file, while the reader holds the old one | Step 5; failing that, steps 6, 8a and 8b |
| Well formed but wrong: wrong order, offsets out of range, not 8-aligned, past the seal, a wrong `seq` | Step 4, 8a, or 8b (validity with `seq`) |
| Right, and the record it names is damaged | 8a or 8b; the scan then reports what a scan reports |
| Right for a segment that recovery replaces after step 5 | The reader holds the old file and gets `Error::Discarded` as today |

A stale index passes all of this only if the current segment holds, at exactly that offset inside a payload, a
complete valid frame with exactly the old record's sequence number, length and CRC: in practice, a payload that
contains a copy of that record of this journal's discarded history, 8-byte aligned at its old place. This is a subset
of the coincidence that `format.md` and ADR 0034 already state for a reader across an in-place repair.

**T6. Forged.** No check short of the full scan can exclude a deliberately forged index. Format version 1 has no back
pointers: if record `r`'s payload embeds a complete frame that claims sequence number `r` and ends where record `r`
ends, then the walk from the embedded frame to the seal is consistent, and `len` and `crc` can be copied from it.
Someone who controls both a payload and the index file can make an opted-in reader return the embedded frame. They
need not be able to write the segment, and nothing is wrong in it: `verify` and ordinary readers report nothing.
`tickertape index --check` does. Step 3's owner and mode check is **protection against misconfiguration, not a
security boundary** (it does not see ACLs, and whoever can write the journal directory can replace more than the
index). The option must not be enabled where someone who may not write the segments can write `seek-index/`; with it
off, no byte there is ever read.

**Durable-only (C13, ADR 0034).** The argument there needs a cursor that is a boundary of the current history and
never past an applied marker, with an identity check after the marker read. Step 2 reads a marker `M` with `F` open,
and step 5 confirms the identity afterwards, so `F` holds the covered records throughout (the segment lies wholly
before `M`) and, by F3, never changes there: the landing offset is a boundary of every later history. The jump does
not touch `durable_next_seq`. If the cursor is past it, the next read refreshes the marker, confirms the identity
again and drops the buffer before returning anything, exactly as at any checkpoint boundary. A lost marker covers no
records, so the reader waits. The exception is the one C13 already states (damage that also destroys the marker). This
covers the cursor a correct index gives; T5's last paragraph and T6 apply to durable-only readers as to others.

### What is given up, and the wording it needs

With the option on, records between offset 64 and the landing record are **not read by that seek**. Today a damaged
record or a zeroed commit word there ends the reader's progress (`Error::Corrupt`, or waiting at the empty slot) and
hides everything after it; with a jump it goes unnoticed and the records after it are returned. Not given up:
validation of each record returned or walked over, the chain from there on (across segments), identity checks, and
what `verify`, replay and a reader without the option report. Recovery, retention, `verify` and `status` never read an
index. The builder validated all skipped records once, when it wrote the index.

The default is off because this changes what `seek` documents. When this is built, the same change must add:

- `crash-contract.md`, "Readers, the writer and memory safety": "A reader with `ReaderConfig::seek_index(true)` may
  start a seek at a record named by a seek index (ADR 0045). Every record returned satisfies the five validity rules
  at its position. Records before the landing record are not read, so damage there does not stop the reader. A stale
  index can lead to a wrong record only if a payload holds, at the indexed offset, a complete frame identical in
  sequence number, length and CRC to the record indexed there before. A forged index can do so whenever a payload
  contains a suitable frame. `verify` reports neither; `tickertape index --check` does. Do not enable this where
  someone who may not write the segments can write `seek-index/`." And C11 and C13: such a reader is the exception,
  with a pointer here.
- `crash-contract.md`, **C15** (index missing, stale, torn, damaged): T1 to T6, with the tests below; C11's list of
  what an open may change before it refuses gains the removal of index files; C14 gains the removal after a segment.
- `format.md`: a row for `seek-index/` ("Not part of format version 1", as for `readers/`), and the removal as
  recovery step 2a; the file format above moves to a section outside the version 1 text. `compatibility.md`: next to
  the `readers/` sentence, and the new items.
- rustdoc of `seek`; `operations.md` (Seek time, running `tickertape index`, archive); `cli.md`; `c-api.md` and the
  binding docs.

## Building

Only `tickertape index` and `seek_index::build` build (first release). All access to `seek-index/` is through one
directory descriptor, opened with `O_DIRECTORY | O_NOFOLLOW`, and `openat`, `renameat` and `unlinkat`, in builders,
readers, retention and the writer, so a symlink there can never lead to another journal's files.

**Who may build.** The builder creates `seek-index/` only if its effective user owns the journal directory and the
journal directory's group is one of its groups; otherwise it refuses (`mkdir` the directory as the journal's owner
first). After `mkdirat` it sets the directory's group and permission bits to the journal directory's through the
descriptor. If that fails it removes the directory it just made (empty, by `unlinkat`), the only case in which
tickertape removes it. It builds a segment's index only if its effective user owns the segment and the index file can
be given the segment's group; otherwise the segment is reported `NotPermitted`. Root gets no exception (`sudo -u` the
owner), so it never leaves files the owner cannot manage. (Changing ownership as root inside a directory another user
controls invites races; refusing needs no such care.)

**One segment:**

1. **Skip.** If `{base}.idx` passes the reader's steps 3, 4 and 6 and the 8 bytes of its **last** entry match the
   segment (8a), keep it. A repair changes a suffix of a segment, so a stale file is stale at its last entry.
   `--rebuild` skips nothing and `--check` walks everything. Without `--rebuild`, a file of a higher version is kept.
2. **Fence.** Create the temporary file (`O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC`, mode 0600) **before anything
   below**.
3. **Eligibility**, read after step 2:
   - *A valid marker* that passes retention's position check (a dangling one is exit 2): made durable with
     `retention::durable_checkpoint` (one marker sync per slot, `F_FULLFSYNC` on macOS). Eligible:
     `base < marker.segment_base_seq`. No lock; runs beside a live writer.
   - *None* (an archive, a journal never checkpointed): open `writer.lock` without creating it; if it exists, take its
     `flock` without blocking and **release it at once**, writing nothing. Busy: exit 1. Eligible: every segment that
     ends in a valid seal.
4. **Walk.** Open the segment read-only and **keep the descriptor until step 6**, so its inode cannot be reused.
   Validate its header, then every record from offset 64 with `pread` in windows of at most 1 MiB. It must end in a
   valid seal. Any other end: no index for this segment.
5. **Publish.** Write the temporary file; set its group to the segment's and its mode to the segment's `& 0444` plus
   owner write (independent of the umask); `renameat` it over `{base}.idx`. If the temporary name is gone, a writer
   opened meanwhile: start again from step 2, at most three times. **The index is not synced, nor is its directory.**
6. The segment's path must still name the descriptor's device and inode; otherwise unlink `{base}.idx`.
7. On any error unlink the temporary file.

A crash at any point leaves no index, the old one, a new complete one, or a temporary file. A power loss can leave a
file of zeros, an empty or a torn file: the reader's steps 3 and 4 reject it, and the next run rewrites it. A reader
sees the old file, the new one or none.

**Why the fence is enough.** An index-aware writer's open (Life cycle, 2a) reads the marker, then lists `seek-index/`
**twice**, unlinking in both passes every temporary name and every index name in its recovery domain, then changes
segments. For a build of a segment `S` and such an open, assuming its unlinks work:

- *The temporary file existed when the first pass began.* If the pass meets it, the unlink removes it and the rename
  fails, so the build restarts; or the rename came first. If the pass met neither name (a listing concurrent with a
  rename may return neither), the rename happened during that pass, so `{base}.idx` exists before the second pass
  begins, and that pass removes it if `S` is in the domain. If it is not, recovery does not change `S`.
- *It was created later.* Then the builder read the marker after the writer did. Until the writer's first checkpoint
  nothing writes the marker, so the builder sees the marker the writer saw (then `S` is below the domain) or one
  damaged since (an older slot: `S` is below that too; none: the lock probe finds the writer). After that checkpoint,
  recovery has finished, and the walk reads the result.
- *No marker.* The probe shows that no writer is open after the fence exists, so any later open lists after it and the
  first case applies. An open that ended before the probe has finished with `S`; one killed in the middle left `S` as
  the next open finds it, and that open removes the index. Without `writer.lock`, a writer creates it before it lists.

No later marker check is needed, and one could not replace the fence: a marker can be damaged, recovered from and
valid again past `S` between two looks, with `S` repaired in place and its inode unchanged. The argument holds for
index-aware writers whose unlinks work; otherwise, and when an operator restores a marker meanwhile, T5 decides. The
probe holds the lock for an instant, so a writer opened at that moment can get `Error::Locked`; it retries.

**Readers never build:** "Readers never take locks, never write" is documented, readers are often other users or on
read-only media, and a slow reader must not create files in a journal it only reads.

## Deferred: building from the writer

A writer option (`WriterConfig::seek_index`), its helper thread, metrics and binding options are **not** in the first
release: the thread carries most of the performance and shutdown risk and nothing about it is measured yet. A timer
running `tickertape index` gives the same files, later.

For when it comes: no list of sealed bases and no work at a roll. On each checkpoint wake the helper lists the
segments in `[watermark, marker base)` itself, builds them and moves the watermark (initially the marker base at
open). That removes the lock per roll, the drop counter and the push from recovery. It must use the same build steps,
get the marker base only after the marker barrier, be joined on drop before the lock is released, be left alone in a
forked child, never report an error to an append or checkpoint, and first show with `tt-sustained` (both payloads,
ext4 and xfs) that append latency and throughput are within noise. Not at the seal: until a marker passes a segment, a
crash may replace it or zero it in place. Not on the checkpoint thread: a build (tens of milliseconds warm) would
delay the next marker (ADR 0044). Not from `append`: the hot path gains a branch and state, and the index would vouch
for what the writer remembers rather than for the file's bytes.

## Life cycle

- **Writer open** (every index-aware writer; a missing `seek-index/` costs one failed open). Step 2a, between reading
  the marker and scanning: open the directory (a symlink or a non-directory is ignored), and twice list it and unlink
  every `.idx-tmp` name and every name beginning `{base:020}.idx` whose base is at or above the recovery domain's
  start (the marker's segment, or everything without a marker). Then sync the directory. **All of it is best effort:**
  an optional cache must not stop `Writer::open` for someone who never opted in (a directory left by another user, a
  subdirectory named like an index, a read-only bind mount). What cannot be listed, unlinked or synced is counted, and
  T5 stands alone for it, as it does with a 1.0.0 writer. `RecoveryReport` (NE) gains `seek_indexes_removed` and
  `seek_index_errors`; `actions` stays what it is (repairs of the journal, empty for a cleanly closed one). With an
  intact marker normally nothing is removed. After recovery, indexes below the oldest segment are removed too.
- **Durability of the removal.** The sync at the end of 2a runs whenever the directory exists, not only when something
  was removed (an earlier open may have died between unlink and sync). Today recovery's final directory syncs and
  `full_barrier` run only when `actions` is not empty; the barrier now also runs, best effort if `actions` is empty,
  when `seek-index/` exists, so on macOS the unlink is on the drive before anything is appended to a repaired segment.
  (If a power loss before the barrier brings the index back, the segment holds no new history yet: step 6, 8a or 8b
  rejects the entry, or it points at a record recovery kept.)
- **Retention and `archive`** unlink the index after the segment's removal has been made durable, best effort; a
  failure is not a retention failure. A leftover is harmless (its segment is gone, and the numbers before a marker are
  never used again) and is swept at the next writer open. `MoveTo` does not carry the index: the move protocol and C14
  stay as they are, and `tickertape index DEST` builds it.
- **Copying, backup and restore, another machine:** a copy of the whole directory keeps working. Copying only `*.tape`
  and `checkpoint` loses only the index. Restored segments under newer indexes, or the reverse, fall under T5.
- **Read-only media:** an existing index works; nothing is written. `tickertape index` fails there with exit 1.
- **Replacement, quarantine, restored headers:** only inside the recovery domain, so step 2a has removed the index
  where it could.
- **Deleting the files in `seek-index/`** is safe at any time, with readers and writers running. Deleting the
  directory is too, except that an `export --output` of 1.0.0 walking the journal at that moment can fail with exit 1
  and must be run again. Documentation says to delete the files (`tickertape index DIR --remove`).

## The active segment

Nothing, on disk or in memory. The target of a consumer that restarts near the head usually lies in the active
segment, so **this design does not speed up that common seek** (up to one segment's scan, as today); it speeds up
seeks into sealed history. An on-disk partial index would need rewriting at each checkpoint for an unsealed, still
changing file, and the identity of "this much of it" in the format. A per-reader in-memory one helps only a second
seek by the same reader, which continuing from the cursor mostly covers. The marker already names one boundary in the
active segment (`offset`, `next_seq`) that recovery trusts: starting there, and a cursor the caller saves (option 14),
both serve the restart without any file. They should be the **next** record, with priority over building from the
writer (Open, 5).

## Compatibility with 1.0.0

**In the code** (`git diff v1.0.0` over `crates`, `python` and `bindings` is empty at `0a28ef5b`):

| 1.0.0 component | What it does with a name it does not expect |
|---|---|
| Readers' open and seek, writer's open and recovery, retention, `archive`, `inspect`, `verify`, `stats`, `tail`, `export`'s and `status`'s reading of the journal | List only through `journal::list_segments` and `find_bases`: an entry counts only if `parse_segment_file_name` accepts the whole name (20 digits + `.tape`, 25 bytes); others are skipped without `stat` or open. No recursion |
| **`export --output FILE`** (`reject_alias`, an output that is a hard link of a journal file is refused) | **Walks the whole tree below the journal directory**: lists every directory and `lstat`s every entry, opening none. Skips an entry that vanishes. **Fails (exit 1, before truncating the output) if a directory other than `readers/` cannot be listed or vanishes** |
| `status --output FILE` (`reached_from`) | For an existing output, lists the journal directory, `quarantine/` and the beacon directory, one level, following and `stat`ing each entry; it does not enter `seek-index/` |
| Recovery's removal of temporary files | `list_tmp_files`: only segment name + `.tmp`, and `next-segment.tmp` |
| Retention and `archive --to` | Remove or move the planned segment path only; the destination is not listed |
| `status` | Lists segments as above and `readers/` only; storage bytes are sums over segments |
| C, C++ and Java bindings, Python wheel | Call the Rust core; no listing of their own in their sources |
| `python/tickertape_reader.py` | `iterdir()`, then requires 25 characters, `.tape`, 20 digits; others are skipped without opening |

**By test.** Built from the `v1.0.0` tag (`git archive`), plus the published `tickertape==1.0.0` and
`tickertape-cli==1.0.0` wheels from PyPI and a C program against the 1.0.0 library. For each operation the same
journal (13 segments of 64 KiB, records of 0 to 1,499 bytes, a checkpoint before the head) was run with and without
planted files, and exit status, output, recovery report, segment list, quarantine, archive destination and every
record read afterwards were compared; the planted files' hashes and modification times were compared before and after.
Only clocks, free space and timestamps in quarantine names were masked. Two layouts: `seek-index/` (an index per
segment, an orphan, a temporary file, segment-like names `….tape` and `….tape.tmp` inside it, a subdirectory, a
dangling symlink, a FIFO), and, to learn whether it would also be safe, files beside the segments (`….tape.idx`,
`….idx`, `….tape.idx.tmp`, hidden temporaries).

- Reads: `tail` (four forms), `inspect`, `verify`, `stats`, `export`, `status` (JSON and Prometheus),
  `archive --dry-run`; safe, mapped and durable-only library readers with five seeks each; the Python decoder; the
  PyPI wheel, PyPI CLI and C library (macOS).
- Writes: open, append, roll and checkpoint; `SIGKILL` then recovery; a damaged tail under `Quarantine` and `Fail`;
  the marker deleted; zeroed; deleted with a damaged middle segment (seven files quarantined; `Fail` refuses); a
  missing middle segment; retention by deletion and by move; `archive --delete` and `--to`.
- A live 1.0.0 writer with background checkpoints and four followers while a thread created, renamed and removed files
  in both places, then 22 opens and seeks.
- Output files: `export --output` (JSONL, Parquet) and `status --output` (JSON, Prometheus), to a new, an empty and an
  earlier output file, in both layouts; then 150 runs of each of two while a thread renamed and removed index files,
  and 150 while one removed and recreated `seek-index/`; then with the directory and with an index file at mode 000.

Result with files that are present, readable and at most created, renamed and removed: **macOS 27 (APFS, arm64): 62 +
24 comparisons, 0 differences. Linux 7.2 (OrbStack VM, btrfs, aarch64): 56 + 24 comparisons, 0 differences** (without
the two PyPI packages and the C program, in both layouts); the 300 runs per platform under renaming files all
succeeded. No planted file was changed. Every follower reached the end and every seek hit. Also not run: Java and C++
1.0.0 packages (they load the same library), ext4 and xfs, a 1.0.0 writer overtaken by power loss.

**The exception, confirmed by test:** 1.0.0's `export --output` failed with exit 1 when `seek-index/` was at mode 000,
and in 21 (macOS) and 18 (Linux) of 150 runs while the directory itself was removed and recreated in a loop.
`status --output` did not. So only the journal's owner creates the directory, with the journal directory's group and
mode (mode bits alone would not do for a 0750 journal whose group is not the builder's primary group), and tickertape
does not remove it; files inside may come and go. What remains is an operator who makes the directory unlistable for
the exporting user or deletes it during an export, and the moment between `mkdirat` and the group and mode being set
(once per journal), in which another user's export can fail and must be run again. `quarantine/` has the same effect
on that command today.

**Two consequences of an index-unaware writer:** it does not remove indexes or temporary files before repairing, so
neither T4 nor the fence applies (T5 does; continuing a newer writer's journal with an older writer is not promised
anyway, `compatibility.md`), and its retention leaves orphans until a newer writer opens.

**Why a subdirectory**, when files beside the segments would avoid even that exception: the directory listing
dominates opens and seeks in large journals, and files beside the segments double it for every version. Time of
1.0.0's `seek(0)` (listing, open, header):

| Segment files | No index files | Beside the segments | In `seek-index/` |
|---|---|---|---|
| 1,000, macOS | 0.35 ms | 0.70 ms | 0.36 ms |
| 10,000, macOS | 3.7 ms | 8.6 ms | 3.5 ms |
| 10,000, Linux VM | 0.63 ms | 1.3 ms | 0.62 ms |

## Cost and benefit

Scratch prototype outside the repository: 128 MiB sealed segment, warm cache, 1,000 random targets, full record
validation. **Informal: M4 Pro running several other jobs (load average 8), and a Linux VM on the same Mac. Not
published results.** "Index" is the time to open, read and check the index, land and walk to the target; the prototype
did not have the directory descriptor, the identity `stat` and the 8-byte read of 8a (a few system calls, not
measured); a real seek also adds today's listing and open (34 µs on the Mac, 6 µs in the VM with two segments, and the
table above with many).

| Payload | Spacing | Index size | 1.0.0: last record, Mac / VM | Index, median (p99), Mac | Index, median (p99), VM | Build, Mac / VM |
|---|---|---|---|---|---|---|
| 64 B | 4 KiB | 786 KB (0.59 %) | 25.8 / 23.7 ms | 69 (134) µs | 88 (108) µs | 23 / 32 ms |
| 64 B | 16 KiB | 197 KB (0.15 %) | | 29 (36) µs | 27 (37) µs | 22 / 32 ms |
| 64 B | **64 KiB** | **49 KB (0.04 %)** | | **29 (43) µs** | **17 (25) µs** | **21 / 31 ms** |
| 64 B | 256 KiB | 12 KB | | 48 (329) µs | 37 (69) µs | 21 / 32 ms |
| 64 B | 1 MiB | 3 KB | | 113 (398) µs | 127 (247) µs | 27 / 31 ms |
| 1 KiB | 64 KiB | 49 KB | 14.0 / 11.3 ms | 22 (39) µs | 13 (20) µs | 15 / 18 ms |
| 16 KiB | 64 KiB | 49 KB | 13.6 / 11.5 ms | 22 (31) µs | 13 (21) µs | 12 / 16 ms |
| 1 MiB | any | 3 KB | 11.9 / 9.6 ms | 106 (127) µs | 117 (155) µs | 12 / 16 ms |

- **Seek:** about 25 ms becomes about 0.06 ms in a small journal. The bound is one index file, less than 64 KiB of
  skipped records, and the target; the landing record's CRC is computed twice. Cold storage was not measured: the
  reads are the index (49 KB), the seal, and the landing chunk, instead of up to 128 MiB.
- **Disk:** 24 bytes per 64 KiB. **Memory:** the index file during the seek (49 KB for 128 MiB, at most 1.5 MiB) and
  the landing frame, nothing after; the builder, one window (at most 1 MiB, or one larger record in pieces) and the
  entries. **Writer:** nothing, except the listing at open.
- **Build:** one read and CRC pass per segment in the command's process; per run over existing indexes, the index
  file, the header, the seal and one 8-byte read per segment. Comparing every entry's 8 bytes on each run would be
  about 2,000 scattered reads per 128 MiB segment (20 million for 10,000 segments), so that is `--check`'s work. The
  implementation must run the `replay` benchmark before and after (`CONTRIBUTING.md`).

## Options considered

1. **This design.**
2. **Nothing.** Smaller segments already bound the cost (`operations.md`), at more files and rolls. Rejected: the
   owner wants cheap seeks at the default size, and the index is opt-in.
3. **An in-memory index built on a reader's first scan of a segment.** No file, no trust question. Every process still
   pays the full first scan of each segment it enters, which is the cost complained about. Rejected as the answer;
   compatible with this one if repeated seeks by one reader ever matter.
4. **An index in the segment's footer.** Travels with the segment and is retained, moved and copied with it. But the
   trailer is 8 bytes and "every byte that holds no record or seal is zero": recovery, `nonzero_end`, retention's
   `seal_at_end` and every released reader rely on that, so it is format version 2. Rejected.
5. **An index beside each segment** (`….tape.idx`). Safe with 1.0.0 without the exception above, but doubles every
   listing for every version. Rejected.
6. **One index for the journal.** Needs rewriting or compaction at retention and locking between builders, and one
   damaged byte loses all of it. Rejected.
7. **Entry every N records** (256, as Chronicle). Unbounded bytes per seek at large records. Rejected.
8. **Skip over commit words without CRC** (no file). Still touches every record's cache line or page (all of a cold
   segment) and gives up the same validation. Rejected.
9. **On by default, or used without opting in.** Changes `seek`'s documented meaning in 1.x. Rejected.
10. **Readers build lazily.** Rejected ("Building").
11. **Synced index files.** A sync per segment for data that can be rebuilt; a torn file is detected. Rejected.
12. **A marker required of visible readers too.** Would need no trust in T4 for them, but makes the index useless in
    archives, which have no marker. Rejected.
13. **Mapped index.** `SIGBUS` on truncation for 49 KB of I/O. Rejected.
14. **A cursor the caller saves.** `Reader` hands out a token (segment header, offset, `seq`, `len`, `crc`) that a
    later process gives back, revalidated as in steps 6 to 9. No file, no builder, and it serves the restart near the
    head. But it reaches only positions a reader has already been at, not any sequence number, and needs a public
    token type in every binding. Deferred to the record on the marker as a jump point, where it belongs; it does not
    replace this one.
15. **Failing `Writer::open` when an index cannot be removed.** Stronger than T5 alone, but an optional cache would
    stop writers that never opted in. Rejected.
16. **Holding the writer's lock for a whole run without a marker.** A writer restart would fail with `Locked` naming a
    dead process for as long as the run. Rejected for the fence and a probe.
17. **Checking the marker again after the rename, without a fence.** Not sound (Building). Rejected.

## Public surface

All additive (ADR 0043); C ABI version 1 and the existing baseline unchanged.

- **Rust.** `ReaderConfig::seek_index(bool)`, public field, default `false`. `Reader::last_seek() -> SeekStart`, NE
  enum `SegmentStart`, `Cursor`, `Index`: where the last `seek` started (tests and diagnosis).
  `RecoveryReport::seek_indexes_removed` and `::seek_index_errors` (`u64`). Module `seek_index`: `DIR_NAME`;
  `build(dir, rebuild: bool) -> Result<BuildReport>`; `check(dir) -> Result<CheckReport>`;
  `remove(dir) -> Result<u64>`. Reports are NE; per segment: `base_seq` and an NE state (`Built`, `Valid`, `Missing`,
  `Ignored`, `Wrong`, `NotNeeded`, `NotSealed`, `NotCovered`, `NotPermitted`, `Vanished`) with a reason string. The
  byte codec stays crate-private (`#[doc(hidden)]` for the fuzz target).
- **CLI.** `tickertape index DIR [--rebuild | --check | --remove] [--json]` (`schema_version` 1). `--check` scans each
  segment, changes nothing, and compares every entry with the chain, whoever runs it: ownership and mode are reported
  separately and do not decide the state. `Ignored`: a reader would not use the content (reason given). `Wrong`: it
  would, but an entry is not on the chain; exit 2. I/O errors and `NotPermitted` exit 1; otherwise 0. `--remove`
  deletes recognised names and leaves the directory. `tail`, `export` and `stats` gain `--seek-index`, for their
  `--from`. `export --output` treats `seek-index/` in its alias walk as it treats `readers/`: it holds no journal
  data, so a part that vanished or cannot be listed is skipped. No other command changes.
- **C:** `tickertape_reader_options_set_seek_index`, shaped like `…_set_durable_only`. **C++:** the same on its reader
  options. **Python:** keyword `seek_index=False` on `Reader`. **Java:** `seekIndex(boolean)` on the reader options.
  Building, `last_seek` and the two report fields stay in Rust and the CLI for now. The pure-Python decoder does not
  use the index.
- **Metrics, `WriterConfig`:** nothing in the first release.

## Test plan

Names are suggestions; `tests/seek_index.rs` unless stated. New fault points: `SeekIndexFence` (after step 2),
`SeekIndexBeforeRename`, `SeekIndexAfterRename` (builder), `SeekIndexListed` (writer open, after each pass of 2a),
`SeekIndexRemoved` (after 2a), `ReaderSeekIndexLoaded` (after the reader's step 4), `ReaderSeekIndexMarker` (after its
step 2).

1. **Codec:** the test vector, both ways; every boundary of step 4; `spacing(Z)` at 64 KiB, 4 GiB, 4 GiB + 64 KiB,
   2^40; property test that the entries are exactly the first record of each block for random record sizes, and that
   building twice gives equal bytes; other spacings are accepted and kept. Fuzz target `decode_seek_index` (no panic,
   allocation bounded).
2. **Equivalence (property and fuzz):** on undamaged journals whose payloads hold no frames, a reader with the option
   returns exactly the records, positions and errors of one without, for every target (block edges, first, last, past
   the end, next segment), {safe, mapped} × {visible, durable-only}, and **for arbitrary bytes as the index file**,
   including valid files with each field mutated and the CRC recomputed.
3. **Bound:** with `last_seek` and a counting view, a jump reads less than one spacing plus the target's frame, at 0
   B, 64 B, 1 KiB, 100 KiB and maximal records.
4. **8a before 8b:** a resealed segment with the same seal offset and `next_seq`, an indexed offset inside a payload
   whose four bytes decode to a length near the segment size (a sparse 4 GiB segment with `0xF0000000`, and smaller):
   no read or allocation beyond the 8 bytes (counting view and allocator), safe and mapped; the commit word changed
   between 8a and 8b: no body read.
5. **Each row of T5:** missing; directory, dangling and live symlink, `seek-index` a symlink to another journal's
   (read, build, remove, writer open: nothing followed, nothing unlinked there), FIFO (with a timeout), no permission,
   I/O error; empty, every truncation length, one byte longer, zeros, each single bit flipped; versions 0 and 2;
   another segment's, another journal's, an older journal's with equal base and size; foreign owner (root test,
   ignored by default), group- and world-writable.
6. **Stale after each repair kind,** produced by the v1.0.0 writer and, with removal disabled by a fault point, the
   current one: tail replaced, zeroed in place, quarantined and created anew, header restored; then a new history with
   equal and with different record sizes, sealed at the same and at another offset. Never a removed record; every
   result equals a scan's. The same with a reader that keeps the old `F` open while an index is built from the
   repaired file.
7. **Removal (T4):** marker lost, older slot current, both with damage below the old marker; `Quarantine` and `Fail`;
   kill after 2a, during the repair, before the barrier; a reader paused at `ReaderSeekIndexLoaded` across each
   repair; `{base}.idx2` is removed too. **Best effort:** `seek-index/` of another user, mode 0555 and 0000, a
   subdirectory named like an index and like a temporary file, a regular file, unlink, listing and sync failing: the
   open succeeds, the count is right, and test 6 holds.
8. **Durable-only:** repair injected at `ReaderSeekIndexMarker` and after landing; sealed segments after the marker
   never use the index; marker lost between seek and read; cursor past the applied marker.
9. **Forged (documents T6):** a payload embedding a frame for its own sequence number and an index pointing at it:
   returned with the option on, never with it off; `verify` clean; `--check` reports `Wrong`.
10. **Damage:** a damaged record and a zeroed commit word before the landing record: found without the option, by
    `verify` and by replay; not by the jump. Damaged landing or target record: same result as the scan.
11. **Builder and fence:** for every order of {fence, marker read, walk, rename, identity check} against {marker read,
    pass 1, pass 2, repair, append, roll, checkpoint} at the fault points, with the marker damaged, recovered and
    valid again past the segment (in place and by replacement): no index of the old history survives, and at most
    three restarts; a rename inside a listing (a listing stub that returns neither name); a loom or exhaustive model
    of the two parties. Marker read before its barrier, then the older slot current (the sync is asked for before the
    walk, `RetentionMarkerSync` pattern). Inode reuse after the segment is removed during a walk. Two commands at
    once; kill at each fault point; `ENOSPC`; read-only directory; retention between walk and rename.
12. **No marker:** with and without `writer.lock`; a live writer (exit 1, lock file unchanged); a writer restart
    during a long run succeeds.
13. **Permissions:** umask 077 and 002; 0750 journal with a group that is not the builder's primary group, with and
    without setgid; builder that is not the owner, and root: refused, nothing created; readers of the same group can
    use the result.
14. **Retention and archive:** index gone after the segment; unlink failure not reported; orphans swept; `index DEST`
    on an archive; `reader_retention_stress` and `soak` with the option on, `tickertape index` in a loop, half the
    readers without, results compared.
15. **1.0.0:** `tests/compat/v1.0.0-ignores-seek-index.sh`, modelled on `v0.4.0-ignores-readers.sh`, with the
    operations above, including `export --output` under renaming index files; `generate.sh --check-old-readers` on
    journals with indexes; C ABI baseline; C, C++, Python and Java option tests.
16. **Regression workload:** `tt-scale` gains seeks with and without the index, warm and cold.

## Consequences

Seeks into sealed history cost tens of microseconds instead of tens of milliseconds, for those who run the command and
opt in. There is a new file kind, a step in `Writer::open` and a crash-contract item, and opted-in readers step over
damage that a scan reports. Until the next run, newly sealed segments have no index and seeks into them scan. The name
`seek-index/` is taken from now on (a user's own directory of that name keeps its other files). Nothing changes for
anyone who does not opt in, except two listings and a sync at writer open, and retention's unlink, when the directory
exists.

## Open decisions for the reviewers

Positions adopted after the first review (Claude), open to the other reviewers:

1. **Reader default:** off in 1.x; revisit for 2.0.
2. **T6:** accepted with the contract wording above; the owner and mode check is kept against misconfiguration, not as
   a security boundary.
3. **Failed removal at writer open:** does not fail the open; counted in `RecoveryReport`.
4. **Visible readers without a marker** may use an index (archives need it).
5. **The marker as a jump point in its own segment, and a saved cursor:** the next record, before building from the
   writer.
6. **Backfill at writer open:** no. **Building from the writer:** deferred.
7. **Carrying the index in `MoveTo`:** no; build it at the destination.
8. **Subdirectory or beside the segments,** given the `export --output` exception: the subdirectory, created only by
   the journal's owner.
9. **Root may not build.** Stricter than needed (root could `fchown`); recommended for the first release.
10. **The default run compares only the last entry's 8 bytes** of an existing index. Recommended, for its cost; a
    stale file it missed would cost scans, not correctness.
