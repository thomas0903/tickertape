# 0042: Replay and recovery at scale: what grows with segments and bytes

- **Status:** accepted on 2026-11-20 by the maintainers under the owner's delegation, after the cloud run: no sparse
  index for 1.0. The measurement tool and the reader listing change are in place; the sparse index and options 3
  and 4 are possible later work, not built
- **Date:** 2026-11-20

## Context

The roadmap item "Make replay and recovery excellent at scale" asks to measure startup, seek, catch-up, replay and
crash recovery across many segments and journals larger than RAM, to build rebuildable sparse indexes only if
measured seek costs justify them, and to address measured bottlenecks while keeping memory bounded.

`tt-scale` (`benches/README.md`) builds a journal of a given shape (segment count, segment size, payload) and times each
operation in a process of its own, with CPU time, page faults, peak RSS and the page-cache state. It was run on macOS
(APFS) and in the OrbStack Linux VM (btrfs), with 10 to 20,000 segments, segment sizes from 64 KiB to 1 GiB, and, in the
VM, a journal four times larger than a cgroup memory limit. It was then run on cloud machines with local SSD, up to a
journal four times larger than their memory (`benches/README.md`, "Replay and recovery at scale"). The informal figures
of all these runs are in pull request #33; the cloud run's results are recorded in `docs/benchmarks.md` → "2026-11-19 to
2026-11-22, sustained workloads on Google Cloud VMs" → Replay and recovery at scale, with dataset sizes and resource
use. What the code does, and what the runs showed about how each cost grows:

| Operation | Grows with | Memory |
| --- | --- | --- |
| `Reader::open`, `Reader::seek` (choosing the segment) | entries in the journal directory (one directory read) | constant (since this change; before, a sorted list of every segment's path) |
| `Reader::seek` (reaching the target) | records before the target in its segment: each is read and validated, at the replay rate | constant (safe reader) |
| Replay | bytes, plus a fixed cost per segment entered (open, header, identity) that dominates with very small segments | constant (safe reader) |
| `Writer::open` after a clean close | directory entries (two directory reads: temporary files, then segments), plus the unwritten remainder of the newest segment, which `format::nonzero_end` reads in full | that remainder is mapped and read, so RSS and page cache grow up to one segment |
| `Writer::open` after a kill | as above, plus the records written since the marker | up to one segment mapped |
| `Writer::open` without a valid marker | every byte of the journal | one segment mapped at a time, and at most four scanned segments' files held open until the first checkpoint (before ADR 0038 → Bounded open segments, one per scanned segment, which failed with `EMFILE` at about a thousand segments) |
| `inspect::verify` | bytes, plus one file open per segment | a report per segment |
| `inspect::list_segments`, `retention::plan` | segments: one open, `fstat` and header read per segment (the largest per-segment cost measured) | a list of every segment |
| Removing a segment by retention | the bytes of each removed segment (the plan scans each candidate for its seal) | constant |

Apart from recovery without a valid marker (crash contract C8, ADR 0007, by design) and retention removing segments,
open, seek and recovery read no segment's records other than those of the segments they enter. The costs that grow
with the segment count are directory reads and file opens with a header read, not scans.

## Options considered

1. **Reader listing without collecting (done).** `Reader::open` and `Reader::seek` need only the oldest base and the
   newest base at or below the target. They collected every segment's path and sorted them; `journal::find_bases`
   now picks both in one pass. Time stays proportional to the directory's entries (the directory read dominates);
   memory no longer grows with the segment count. Behaviour is unchanged: the same segment is chosen, and the
   retries when a chosen segment disappears are unchanged.
2. **A rebuildable sparse index per segment (proposed, not built).** A seek costs at most one segment's worth of
   reading and validation: with the default 128 MiB segment, a seek to the last record reads up to 128 MiB, from
   storage if the segment is cold. A sidecar per sealed segment mapping every Nth sequence number to its offset
   would bound that to N records. Conditions, from the roadmap: it is derived data, rebuilt (or ignored) whenever it
   does not match its segment (identity, size, header creation time, base); a seek through it still validates the
   record it lands on and everything after it, and lands only on an offset where a record with the expected
   sequence number and a valid CRC starts; recovery that replaces or quarantines a segment invalidates its index
   before the segment is reused; no change to format version 1 (the index is a separate file the writer may write
   and readers may ignore). Cost: a new file kind, a new crash-contract case, and a write at every seal. The
   measured seek cost is bounded and proportional to the segment size, which the user chooses; whether that bound
   is too high depends on the cold figures from local SSD, which the macOS and VM runs could not give (the Mac
   could not drop its cache; in the VM, dropping the cache leaves the host's cache warm).
3. **Bound the unwritten-remainder read at writer open (proposed).** `nonzero_end` reads the whole unwritten part of
   the newest segment at every open, also after a clean close, to find bytes after the last record (C8, C11, ADR
   0035). Segments are preallocated, so on most filesystems those reads return zeros without I/O, but they fill the
   page cache and the writer's RSS with up to one segment of zero pages. Skipping unwritten ranges with
   `SEEK_DATA`/`SEEK_HOLE` where the filesystem reports them would keep the check and bound the work to what was
   written; it needs per-filesystem tests (what `SEEK_DATA` reports for preallocated, unwritten ranges differs
   between filesystems) and a crash-contract review, so it is a separate change.
4. **Fewer per-segment opens (proposed).** `retention::plan` opens every segment's header at each run (writer open
   and after each checkpoint, when retention is configured). Sealed segments' headers do not change, so a writer
   could keep them (keyed by file identity) and read only new segments. `Writer::open` reads the directory twice
   (temporary files, then segments); one read could serve both. Both are small, but they touch retention (C14) and
   recovery, so each needs its own review.
5. **Not measured here.** Archived history (segments moved away by retention), missing and corrupt segments, catch-up
   under a live writer, and the zero-copy reader (the tool uses only the safe reader, to add no `unsafe` code; the
   two share the listing and segment-opening code, and differ in how records are read).

## Decision

Keep option 1 (no contract change). Record the growth table above as the current behaviour.

**No sparse index for 1.0** (option 2 not built). Measured on cloud machines with local SSD (`docs/benchmarks.md`,
recorded from the run of pull request #33), including a journal larger than the machine's memory, with the page cache
dropped before each operation: opening a reader did not grow with the journal's size in bytes, and a seek cost at most
one scan of the segment holding its target, proportional to the segment size the user chooses and paid once when a
reader starts or seeks. That bound does not justify a new file kind and a new crash-contract case before 1.0.
`docs/operations.md` (Choosing a segment size → Seek time, Segment count) documents the cost so users can size segments
for it. The sparse index (option 2), skipping the unwritten part of the newest segment at writer open (option 3) and
fewer file opens per retention run and writer open (option 4) stay possible later work, each with its own review; none
of them changes format version 1.

**A defect the run found.** The journals with 10,000 and 20,000 segments could not be measured: a writer held one open
file per segment after its checkpoint marker, so `Writer::open` without a valid marker failed with "Too many open
files" at about a thousand segments, as did a writer that rolled that often without a checkpoint. Fixed in pull
request #38: the writer keeps a bounded number of segments open between checkpoints, also in recovery (ADR 0038 →
Bounded open segments; crash contract → Segments the writer no longer holds).

## Consequences

- `tt-scale` is the regression workload for these costs; its JSON fields are listed in `benches/README.md`.
- `docs/operations.md` states that seek time grows with the segment size and that listing, verification and
  retention costs grow with the segment count.
- Follow-up work, each needing its own review: options 2 to 4, the cases in option 5, and measuring the journals
  with 10,000 and 20,000 segments that the cloud run could not open before #38 (`docs/benchmarks.md` reports them as
  failed).
