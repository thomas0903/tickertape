# 0051: Retention reads only what its policy needs, and lists the directory once per run

- **Status:** proposed (it states a behaviour change; see "Behaviour that changes")
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
| `max_bytes` | the sum of the segment files' sizes | each file's metadata, once: a writer keeps the sizes (below) |
| `max_age` | when the oldest segments were sealed (their successors' `created_unix_ns`) | the headers up to the first segment that is young enough and its successor |
| combined | what each limit needs | the union |

Of every candidate (a segment a limit asks to remove, before the marker's segment, with all older ones candidates): its
header, its successor's header, its seal. All headers come before the first seal, so that the same error is reported
first.

**Sizes.** The sum of the sizes needs every file, so `retention::plan` asks for each one's metadata without opening it. A
writer keeps the sizes between its runs and uses one only while the run's own listing shows the same name with the same
inode number, as a regular file; it forgets the files the listing does not show. Retention and archiving by others
therefore cannot make it stale, and a recovery, the only thing that replaces a segment, needs the lock the writer holds.
A segment file is never resized (`format.md` → Segment file).

**The listing.** One per run remains, and its cost grows with the number of segments. Reusing the last one was rejected:
other processes remove names without telling the writer, and a directory's modification time does not settle whether
one did (on ext4 and xfs it has the granularity of a timer tick).

## Removals without a listing each

Before each removal, C14 requires that the segment is the oldest one, that its successor is the next one, and that both
are there. The listing before each removal showed the first two. Now a run takes them from one listing (a writer's run
from the one its plan was made from, `retention::apply` from one it takes when it starts) and keeps it up to date with
its own removals. The argument:

1. **Names older than the marker's segment only disappear while a run goes on.** A writer links new segments after the
   newest one. A recovery creates and renames segments only in what it recovers, which starts at the marker's segment
   (`format.md` → Recovery, step 2). Archivers only remove. POSIX lets a listing miss only entries added or removed while
   it runs.
2. So a segment that was the oldest in a listing is the oldest for as long as it exists, and so is the one listed next
   once the older ones have gone; and no segment appears between two that were listed one after the other. An archiver
   removes a segment only as the oldest, with its successor in place; this holds for each of any number of them.
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
   candidate's successor, the marker's segment, and with `max_age` the first segment that stays and its successor. C14
   does not promise the former: what it requires before a removal is about the segment, its successor and the marker.
   Removal still stops at the damage when it gets there, so the segments a damaged header would cut off from the writer
   are not touched. What is lost is an accidental alarm: `verify` is the check for damage, as `operations.md` says. What
   is gained is that damage far from the oldest end does not fill the disk.
2. With `max_bytes`, a segment that is not considered counts with its file's size, where its header was checked against
   it before.
3. A run stopped early by a waiting checkpoint (ADR 0044) does not read the headers behind the point where it stopped.
4. A plan made while another archiver removes segments can no longer fail because one of the others was gone between the
   listing and the open (`Error::Io`), except among the segments considered, and with `max_bytes`.
5. A segment missing in the middle that is found missing after a removal's listing used to be an I/O error; it is now
   the broken chain it is when found at the listing.

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
- Tests: `retention/planning_tests.rs` (the old planner is kept there; any journal, any policy),
  `tests/retention_at_scale.rs` (what is read per checkpoint and per removal, at two sizes; another archiver, a missing
  segment, a lost marker at every point of a run). `fault::Access` and `Point::RetentionChecked` are new test hooks.
