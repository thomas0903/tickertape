# 0054: A stated bound on a writer's open files

- **Status:** accepted on 2026-11-26 by the orchestrator on the owner's delegation, after one independent review
- **Date:** 2026-11-26

## Context

`tests/open_segments.rs::many_rolls_with_background_checkpoints_under_a_low_descriptor_limit` failed on busy machines
(issue #73): a writer rolling 300 times with a background checkpoint requested every 25 rolls held 9 to 11 descriptors
more than at its first roll, against a bound of 8 (9 with the spare segment, whose test was ignored). The bound was what
the writer held when the test was written, when the checkpoint thread kept up. `docs/operations.md` gave only measured
figures ("no operation held more than 12 open files" in the `tt-scale` runs), and a defect fixed before 1.0 (ADR 0038 →
Bounded open segments) was exactly an unbounded number of open segments, so the question was whether the writer holds
more when its checkpoint thread falls behind, and how many.

From the code: a dirty list (`checkpoint::DirtySegments`) keeps its four newest segments open. Three lists can exist at
once: the writer's, the running background checkpoint's and the queued one's (later requests join the queued job, so
there is never a fourth). On top of them: the active segment, a released segment the running checkpoint has reopened,
the new segment during a roll, on Linux the segment the early write-back helper is working on (it can be one every list
has released, when the helper falls behind), and on Linux a spare being prepared. Nothing grows with load or with the
number of segments: a checkpoint thread that falls behind only fills the queued job's list to four. The write-back helper
keeps a queue of its own, of up to four rolled segments (`WriteBack::switch` drops the oldest beyond that), but they
are always the four segments rolled most recently, and the writer's own list, or the newest job's, keeps those open
too: the queue adds a segment no list holds only right after a request, when the writer's list is empty, so the
total does not go up. Only the segment the helper is working on can be one that no list holds, and that is the one it
adds. So the writer was
bounded, and the test's bound was tighter than the code's: under load the queued job existed more often, and timing
decided the outcome. The checkpoint held one more file than needed: it kept the previous reopened segment open (as the
candidate for the final barrier) until the next one was open.

## Options considered

1. **Back-pressure: a roll waits for, or helps, the checkpoint thread once a number of sealed segments are held.** It
   would lower the bound by at most the queued job's four, at the cost of a roll that can wait for a checkpoint's syncs
   (hundreds of milliseconds on ext4), on the latency-sensitive path. **Rejected:** the bound is already constant and
   small against the usual limits (256, 1,024).
2. **Document the bound the code gives, and test it exactly.** **Chosen.**

## Decision

- **The guarantee in 1.x is that the count is bounded**, whatever the number of segments and however far checkpoints
  fall behind. The numbers below are the counts of this release, not a promise for every 1.x release: a later 1.x
  release that raises them says so in its changelog. Users are told to set the limit on open files with headroom above
  the documented count and to read `docs/operations.md` → Open files again when they upgrade.
- `docs/operations.md` → Open files states the counts and where each file comes from: 4 always (lock, marker, wake-up
  region, active segment), 12 (three lists of four), 1 reopened by a checkpoint, 1 new segment during a roll, on Linux
  1 for the write-back helper (default on) and 1 for the spare (default off). **18** on macOS and on Linux without the
  helpers, **19** on Linux by default, **20** on Linux with the spare segment.
- A checkpoint closes a reopened segment before it opens the next one (`checkpoint::steps`). The barrier is the last
  segment of the job, which its list always holds open. Before this, a checkpoint could hold two reopened segments for
  a moment, and the bound was one higher (19, 20, 21).
- Test strategy: two hold points for the writer's background threads, `fault::Point::CheckpointReopen` (a checkpoint
  has reopened a released segment) and `fault::Point::EarlyWriteback` (the write-back helper with a segment in hand),
  with the existing `SpareMap` and `RollAfterLink`, drive a writer into the state where it holds the most (the
running checkpoint held at its second reopen, so that one that kept the first open as well would show):
  `tt-open-segments worst`. The tests check that it holds exactly the stated number there, then let it roll 300 times
  with background checkpoints and check that no sample exceeds it. Both descriptor tests run again by default. On
  Linux they count every descriptor except those under `/sys/devices/system/cpu/`: each helper thread reads
  `/sys/devices/system/cpu/isolated` once when it starts, to place itself, which made a count taken just after the open
  one too high under load. That file is open only while a helper starts (at the open, and at the first
  request for the checkpoint thread), when the lists are short, so it does not raise the bound. The journal's mappings
  are bounded, not counted exactly: the prefault helper can still hold the mapping of a segment that every list has
  released, and it has no hold point.

## Consequences

- The tests fail only if the writer holds more than the documented number, or if the worst case gets smaller, in which
  case the documented number is lowered with it.
- A change that adds a file to the writer (another list, another helper) must keep the count bounded, update
  `docs/operations.md` → Open files and `most_extra_descriptors` in `tests/open_segments.rs`, and say in the changelog
  that the count went up.
- Measured (descriptors beyond those just after the open, in the held state): macOS 14, Linux 15, Linux with the spare
  segment 16, the stated bound less the 4 files the writer always holds.

## Amendment: segments waiting for the prefault helper (2026-11-27, ADR [0059](0059-release-finished-segments-off-the-writers-thread.md))

The guarantee is unchanged: the count is bounded. The numbers of this release go up by two. A segment a dirty list
releases is no longer closed and unmapped by the thread that released it (the writer's, at a roll or a request): it is
handed to the prefault helper, and up to two (`prefault::RELEASE_QUEUE`, the one the helper is releasing included) can
wait for it; when two wait, or the helper has ended, the writer closes and unmaps the next one itself, as before. Those
two can be segments no list holds, so they add two descriptors and two mappings. The counts of this release:
**20** on macOS and on Linux without the helpers' early write-back, **21** on Linux by default, **22** on Linux with
the spare segment. With `WriterConfig::prefault(false)` there are no helper threads (no prefault helper, write-back
or spare) and nothing waits: the count stays 18 on every platform. Measured in the held state (`tt-open-segments worst`, which now also holds the prefault
helper while it releases one segment with a second one waiting): macOS 16, Linux 17, Linux with the spare segment 18
beyond the 4 files the writer always holds, and the journal's mappings of that state include the two waiting segments.
