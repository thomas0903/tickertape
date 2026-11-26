# 0055: An archiver takes a segment out of the journal before it lets it go

- **Status:** accepted on 2026-11-26 by the orchestrator under the owner's delegation, after one independent review and
  a re-check (issue #83)
- **Date:** 2026-11-26
- **Amends:** [0051](0051-retention-reads-what-it-needs.md) (point 3 of "Removals without a listing each"),
  [0044](0044-retention-without-a-backlog-wait.md); `format.md` → Journal directory (a new name)

## Context

Issue #83, found by a fault-point sweep in the review of pull request #81 and present since 1.0.0. An archiver
(`retention::apply`, `tickertape archive`) holds no lock. It checks the oldest segment (marker, identity, headers, seal),
then removes it **by name**. In between, the marker can be lost and the segment damaged, and a writer can open: recovery
without a valid marker starts at the oldest segment and can end its chain there, and the writer's next marker names
it. The archiver then unlinks or moves the marker's segment: a dangling marker, which a writer refuses.

ADR 0051 (2a) said a replaced segment fails the identity check; that holds only for a replacement before the check.
And a replacement is not the only case. When the damage leaves only zeros from some record to the end of the segment,
recovery ends the chain there **in place**: the file keeps its inode, and the writer writes to it
(`tests/retention_recovery_race.rs`, `Damage::ZeroedTail`). An identity check alone does not see that.

In a test of the sweep (`::recoveries_at_every_point_of_a_run`: three ways of removing, every point of a run, five
kinds of damage, the writer closed or open), the old removal failed 60 of the 270 cases at the three points it has: 54
left a dangling marker, and 6 left in the archive a copy of the damaged file while the segment the writer had
repaired was deleted. All were at the point between the checks and the removal.

## Options considered

1. **Check the identity after a rename (the issue's direction).** Catches a replacement, not a segment ended in place.
2. **Check the identity and the marker after a rename (chosen).**
3. **Lock out recovery while an archiver removes** (for example a shared `flock` that a writer's open must take
   exclusively). An archiver would make a writer's open fail or wait, and 1.0.0 writers do not take it.
4. **Re-read the marker just before the unlink.** The window is smaller, not closed.

## Decision

A removal by an archiver other than the writer:

1. **Takes the segment out** with a no-replace rename (`renameat2(RENAME_NOREPLACE)`, `renameatx_np(RENAME_EXCL)`):
   a move by rename into the destination; a deletion, and a move by copy once the copy is durable, to a new temporary
   name in the journal directory, `{base:020}.tape.{pid}.{unix_ns}.removing`.
2. **Checks what it took:** the file is the one checked (device and inode; the run holds that file open from its
   checks on, so the inode number cannot have passed to another file, also on ext4 and xfs, which reuse them); the
   marker, read again and made durable, is valid and names a later segment; for a copy, the file still holds the bytes
   copied (length and CRC32C, read again).
3. **Lets it go** (unlinks the temporary name; for a move, syncs the destination), or, if a check fails, **renames it
   back**, never replacing a file that has appeared under the segment's name (no-replace rename; where the file system
   has none, `link` and an unlink of the temporary name), syncs the directories, and returns the error.

**Why this is sound.** Once the segment is out by name, no recovery can find it and no writer can start writing to it;
only a writer that already holds it can go on. A valid marker naming a later segment, read after that, means that the
file is a sealed segment before the marker, as the policy asked to remove: a writer's markers only move forward, so one
that held the segment has sealed it and moved past it, and a later recovery starts from that marker or, without one, from
a segment that is in the journal. If the marker names this segment or none, a writer may be writing to it, and it goes
back. (This holds for the removal that makes the check, not for a file a crash leaves behind: see "Leftovers".) For a copy, a writer that filled the segment and moved its marker past it
before the check may have written to it after the copy was read, and the CRC sees that. Before the check, the archiver
made the marker durable, so a power loss cannot bring back an earlier slot naming this segment.

**The writer's own run is unchanged.** It holds the writer lock, without which no recovery runs, so it removes by name
as before.

**Leftovers.** A crash between steps 1 and 3 leaves the segment in the destination (a move by rename: a completed move)
or under its temporary name. The writer, its recovery, readers, `verify`, `status`, and 1.0.0's programs ignore the
name: it does not parse as a segment (`{base:020}.tape`) nor as a writer's temporary file (`*.tape.tmp`). Such a file
**may hold records the journal no longer has.** In the race, a crash after the take-out (before the checks, or before
putting the segment back) leaves out of the journal a segment that a running writer already holds: the writer refuses
nothing, fills the segment through its open file, rolls and moves its marker past it. The journal then verifies, a
reader starts after that segment, and records the marker covers are only in the temporary file (found in the review).

So a retention run (any archiver's, or a writer's own) unlinks a leftover only when its file is sealed and the segment
its seal names is gone from the journal, below the oldest segment of the run's listing: retention has then removed newer
records on purpose, and removing this file is what it would have done. Which segment a seal names is told from its CRC
word, compared with the seals of the listed bases (the end of the file only); when none matches, the file is scanned
and validated for the sequence number after its last record. An unsealed leftover, or one whose seal names a segment
still in the journal, stays, and `tickertape archive` (`Report::leftovers`) and `status` (`removal_leftovers`) report
it. As extra guards, a name counts only once it is an hour old by its name and its process is gone (`kill(pid, 0)`), so
that a removal in progress is not touched; the decision itself depends on neither. Nothing is put back automatically:
a segment put back in front of the oldest one can sit in front of a gap if another archiver removes that oldest segment
from a listing taken before (the hazard C14 names). In the ordinary crash the clean-up waits one more segment's
removal; an orphan stays until the segment after it is removed on purpose. A leftover of the marker's own segment (a
crash before putting a segment back, with no writer running) makes a writer refuse the dangling marker until the file
is renamed back (`operations.md`).

## Consequences

- **Cost.** An archiver's removal makes, besides the rename that replaces its unlink, a `stat` of the file taken out
  and one read of the marker (open, the two slots, close), plus a marker sync only when the slot changed since the last
  one it synced; a move by copy also reads the segment once more. The read of the marker before the checks now opens the
  marker once instead of twice. A writer's own run makes the same calls as before. Measured: `benchmarks.md` →
  "2026-11-26, an archiver's removal checks what it took".
- **Behaviour that changes.** A new name can appear in the journal directory, as above (`format.md`). A removal that
  finds a replaced or restarted segment after its rename reports the error a check before it reports ("planned segment
  has been replaced", "no longer older than the checkpoint", "requires a valid checkpoint") and leaves the segment in
  place; a move by copy also leaves its copy, as any failed copy does. A reader that opens or seeks there meanwhile sees
  the segment gone (`SeqUnavailable`, or it starts at the next segment), as after a removal.
- **What remains.** (1) A crash, or a power loss, between taking a segment out and putting it back. With no writer
  running, a writer then refuses the dangling marker until the file is renamed back by hand. With a writer running
  that holds the segment, nothing refuses: the writer moves on and the segment is an orphan that `verify` does not
  report, whose records the journal no longer has; it is reported as a leftover and kept until the segment after it
  is removed on purpose, and an operator can put it back (`operations.md`). (2) Putting back after another archiver,
  which found the segment gone meanwhile, has removed its successor: the segment is back in front of a gap. Its records
  are kept; readers and retention stop there with a broken chain, and, without a valid marker, a writer's recovery
  refuses; remove the segment by hand. Both need a recovery or a lost marker inside the archiver's few system calls.
  (3) A reader that opens while a segment is out before it goes back starts after it, skipping records that then come
  back, or, if it was the only segment, gets `Error::NotFound` after its bounded retries (crash contract C14). (4) A
  copy's check is a CRC32C, so a change that keeps the CRC is not seen. (5) A leftover is unlinked by name once the
  checks above pass; a removal in progress is protected by the age and the process test, which another PID namespace
  or a clock step can defeat, but such a file would then have to be a sealed segment whose successor retention removed.
- **Tests.** `tests/retention_recovery_race.rs` (the reproduction; the sweep, quick and `--ignored`; a crash at each new
  step; leftovers and what removes them, the orphan of the review among them; a segment written to after it was
  copied; putting back, with and without a no-replace rename, next to a file that has
  appeared; readers meanwhile), `tests/retention_at_scale.rs` (another archiver also at the new points), and
  `tests/compat/v1.0.0-ignores-removing.sh` (1.0.0's programs beside a leftover).
