# 0051: Retention reads only what its policy needs, and lists the directory once per run

- **Status:** accepted on 2026-11-26 by the orchestrator under the owner's delegation, after one independent review
  ("Behaviour that changes" → acceptance)
- **Date:** 2026-11-26
- **Amends:** [0044](0044-retention-without-a-backlog-wait.md) (what `plan` reads, when a removal lists the directory)

## Context

Issue #80. A checkpoint with a retention policy took 29, 67 and 336 ms on journals of 10,000, 20,000 and 100,000
segments, against 0.3 ms without one, because planning read every segment's header before it looked at the policy.
Removing a segment took about 8 ms at 20,000 segments, 5 of them for a directory listing before each removal. Constraints:
the same plans and the same errors for what a policy has to consider; crash contract C14; no on-disk change; nothing a
writer remembers may be wrong after another process's retention, archive or recovery.

## What each limit needs

| Limit | Needed to decide | Read besides the listing, the marker and the header of its segment |
|---|---|---|
| `max_segments` | the number of names | nothing |
| `max_bytes` | the sum of the segment files' sizes | each file's metadata, once (a writer keeps the sizes, below), and the header of each file whose size nothing else confirms |
| `max_age` | when the oldest segments were sealed (their successors' `created_unix_ns`) | the headers up to the first segment that is young enough and its successor |
| combined | what each limit needs | the union |

Of every candidate (a segment a limit asks to remove, before the marker's segment, with all older ones candidates): its
header, its successor's header, its seal. All headers come before the first seal, so that the same error is reported
first.

**Sizes.** The sum of the sizes needs every file, so `retention::plan` asks for each one's metadata without opening it. A
writer keeps the sizes between its runs and uses one only while the run's own listing shows the same name with the same
inode number, as a regular file; it forgets the files the listing does not show. Retention and archiving by others
therefore cannot make it stale, and a recovery, the only thing that replaces a segment, needs the lock the writer holds.
A segment file is never resized (`format.md` → Segment file). **The exception:** a file changed without getting a new
inode number keeps its old size until the segment is removed or the writer opens again: a copy written over the file in
place (on any file system), or a file removed and created again where the file system gives it the old number (ext4 and
xfs do). Segment files are not edited (`operations.md`), so this is outside what a journal allows. The sum is then off by
the difference in size, which only shifts which segments `max_bytes` asks to remove: nothing that protects a segment from
removal uses a size, and the segment's header is checked against its file when the plan reaches it.

**Lengths a header confirms.** A length counted without its header could make `max_bytes` remove segments the policy
keeps: a file at or after the marker's segment that is longer than its header says (an active segment extended, or a
stray file with a segment's name), or such a file before the marker's segment beyond where a run stops early (ADR 0044).
Up to 1.0 the plan failed there. So with `max_bytes` the header of a file is read and checked against its length, once
(a writer remembers it with the size), when the file is the marker's segment or after it, when its length is not a valid
segment size, or when its length differs from the length of the file before it. Every length summed is then one a
header confirms, or equals the length before it; in a journal of one segment size only the newest segments are read. A
file that fails is an error, as in 1.0. Where reading a file's metadata, or such a header, fails, the error is the one
1.0 gave: that of the oldest invalid header up to that file (they are read for it), and for metadata that cannot be read,
the error opening the file gives.

**The listing.** One per run remains, and its cost grows with the number of segments. Reusing the last one was rejected:
other processes remove names without telling the writer, and a directory's modification time does not settle whether
one did (on ext4 and xfs it has the granularity of a timer tick).

## Removals without a listing each

Before each removal, C14 requires that the segment is the oldest one, that its successor is the next one, and that both
are there. The listing before each removal showed the first two. Now a run takes them from one listing (a writer's run
from the one its plan was made from, `retention::apply` from one it takes when it starts) and keeps it up to date with
its own removals. The argument:

1. **While a valid marker is in place, names older than its segment only disappear.** A writer links new segments after
   the newest one. A recovery from that marker creates and renames segments only in what it recovers, which starts at the
   marker's segment (`format.md` → Recovery, step 2). Archivers only remove. POSIX lets a listing miss only entries added
   or removed while it runs.
2. So a segment that was the oldest in a listing is the oldest for as long as it exists, and so is the one listed next
   once the older ones have gone; and no segment appears between two that were listed one after the other. An archiver
   removes a segment only as the oldest, with its successor in place; this holds for each of any number of them.
2a. **Without a valid marker, (1) does not hold, and safety rests on the checks instead.** A recovery without a valid
   marker starts at the oldest segment, and can replace segments and create segments before the old marker's segment.
   But no removal happens while the marker is invalid (it is read before each removal, and an archiver makes it durable
   first). A segment the recovery replaced fails the identity check. A segment it created is one that a seal names (a
   missing successor, or one put in place of an invalid successor), so it is never in front of the oldest segment, and
   never between a segment and the successor that segment's seal names; and before each removal the segment's seal must
   name the planned successor, which must open with a valid header. What a recovery moves into `quarantine/` only
   disappears.
3. What can change is that a segment goes. Both are opened before each removal: the segment (its identity must be the
   planned one) and the successor (its header must be valid). As before, what happens between these checks and the
   unlink is not seen: another archiver may remove the segment (the unlink then fails and is judged by a listing) but
   not the successor, whose turn comes only after the segment has gone.

The marker, the identity, both headers and the seal (or every record, for a move) are still checked before each removal.
The directory is listed again before anything else is removed when a check fails, when a file is missing, or when a
segment the run removed or found gone is there again. An error is reported only as that listing shows it, so a
concurrent archiver is told from a missing middle as before. One failure is reported at once: an I/O error on the marker,
since a second sync could succeed without having written anything.

**The limit.** A segment file that something other than tickertape puts in front of the oldest segment, or between two
segments, while a run goes on: before, the next removal's listing showed it and the run stopped. Now that is noticed
only for the segment removed just before (restoring an archive next to the journal's oldest segment involves it). Nothing
is lost that was not to be removed, but the file put there is then in front of a gap. `operations.md` says not to do it.

## Behaviour that changes

1. **Damage in a segment the policy does not need no longer stops retention.** Before, any invalid header, or any
   segment file that could not be opened, made every run fail (`Writer::last_retention_error`,
   `retention_failures`, an error from `plan` and `archive`), and nothing was removed. Now this is so for a candidate, a
   candidate's successor, the marker's segment, with `max_age` the first segment that stays and its successor, and with
   `max_bytes` every file whose header is read for its length ("Lengths a header confirms"). This is documented behaviour
   of 1.0.0, not only an accident: the rustdoc of `retention::plan` said "Reads every segment's header", and its errors
   named "an invalid header". ADR 0043 keeps documented meanings for 2.0; it is not a crash-contract guarantee (C14
   requires, before a removal, things of the segment, its successor and the marker). Removal still stops at the damage
   when it gets there, so the segments a damaged header would cut off from the writer are not touched. What the old
   behaviour gave was an alarm for damage anywhere in the journal; `verify` is the documented check for damage
   (`operations.md`). What is gained is that damage far from the oldest end does not fill the disk, and that checkpoints
   with a policy no longer cost a read of every header (336 ms at 100,000 segments).
2. With `max_bytes`, a file whose header is not read is one as long as the file before it, and counts with that length.
   If its header names another size, the plan counts the length the file has on disk where 1.0 failed. When the header
   names a smaller size, the limit can then ask to remove more than 1.0 would have, by that difference, and only for a
   file as long as its neighbour; every other length that does not match its header is an error, as in 1.0 ("Lengths a
   header confirms").
3. A run stopped early by a waiting checkpoint (ADR 0044) does not read the headers behind the point where it stopped.
4. A plan made while another archiver removes segments can no longer fail because one of the others was gone between the
   listing and the open (`Error::Io`), except among the segments considered, and with `max_bytes`.
5. A segment missing in the middle that is found missing after a removal's listing used to be an I/O error
   (`Error::Io`); it is now the broken chain (`Error::Corrupt`, `BrokenChain`) it is when found at the listing. A rare
   race, but a different `Error` variant.
6. An I/O error reading or syncing the marker during an archiver's run is returned even when another archiver has just
   removed that segment (before, the segment counted as already removed first): a second sync could succeed without
   having written anything.

**Acceptance.** The orchestrator accepted these changes under the owner's delegation on 2026-11-26, after one
independent review: `verify` is the documented check for damage, and a planner that reads every header is what made a
checkpoint with a policy cost 336 ms at 100,000 segments.

## Options considered

1. **The list above (chosen).**
2. **Remember which segments exist and their ages in the writer.** Faster than a listing, but stale after another
   process's retention or archive, and useless to `retention::apply`.
3. **Compare the directory's modification time before each removal and list again when it changed.** A running writer
   changes it at every roll, so the cost would return exactly when retention has to keep up.
4. **List again every N removals.** Bounds the limit above by a number, at a cost that still grows with the journal; no
   tickertape process needs it.

## Consequences

- A checkpoint with a policy costs one listing more than one without, and a few reads; a removal no longer depends on the
  number of segments. Measured: `benchmarks.md` → "2026-11-25, retention on journals of many segments".
- The crash contract's C14 says how "the oldest" and "the next" are established. Its guarantees do not change.
- Undocumented differences, for completeness: with `max_bytes` a directory named like a segment counts its metadata
  size when that size is a valid segment size and equals the one before it (otherwise its header is read, which fails as
  before); `list_bases` asks for each entry's type, which costs an `lstat` per entry on a file system that reports none
  in the listing (the supported ones report it). The errors are those of 1.0 where 1.0 failed too: a file whose metadata
  cannot be read gives the error opening it gives ("open segment"), not one of its own.
- Not fixed here: a race that predates this record, in which an archiver removes the marker's segment when a recovery
  replaces it between the checks and the removal (filed separately).
- Tests: `retention/planning_tests.rs` (the old planner is kept there; any journal, any policy; lengths that are not
  their header's; a file changed in place), `tests/retention_at_scale.rs` (what is read per checkpoint and per removal,
  at two sizes; another archiver, a missing segment, a damaged successor, a lost marker, a destination that goes away,
  at every point of a run), `tests/retention.rs::retention_kill_loop_50` (now kills inside a run of several removals).
  `fault::Access` and `Point::RetentionChecked` are new test hooks.

## Amendment (2026-11-26): the race between the checks and the removal

The race named under "Not fixed here" (issue #83) is closed by ADR
[0055](0055-an-archiver-takes-a-segment-out-before-it-lets-it-go.md). Point 2a above holds only for a replacement made
before the checks, and a recovery can also end its chain in the oldest segment in place, keeping its inode. So point 3
changes for an archiver other than the writer: it takes the segment out of the journal with a no-replace rename, checks
again that it took the file it checked and that the marker, read again, names a later segment, and only then lets the
file go; otherwise it renames it back. A writer's own run, which holds the lock, still removes by name.
