# 0049: The spare segment on Linux, as an option that is off by default

- **Status:** accepted (decided by the orchestrating agent on the owner's delegation, after one independent review)
- **Date:** 2026-11-26

## Context

ADR [0022](0022-prepared-next-segment.md) prepares the next segment in the background: a roll then writes a header and
links a file instead of creating and preallocating one. ADR [0023](0023-no-prefault-advice-on-linux.md) decided
against it on Linux: in one run of four the spare's preallocation sync forced an ext4 journal commit and stalled the
writer for tens of milliseconds. Since then rolls no longer sync on the writer's thread (ADR
[0038](0038-background-checkpoints.md) → Rolls without a sync), so issue #60 asked for the experiment again. The
results are in `docs/benchmarks.md` → "2026-11-25, roll and journal stalls on Google Cloud VMs".

What was measured there:

- **Away from saturation** (half of the probe, and ext4 without checkpoints; aarch64 and x86-64) the spare makes the
  roll cheaper and the mapped reader's p99.9 lower.
- **At saturation it does not help, and the only near-saturated cell of the kept build points the wrong way:** aarch64
  ext4 at 90 % of the probe, against `main`, six runs each: runs with a roll of 20 ms or more 5 of 6 against 2 of 6,
  seconds with such a roll 10 against 3. The writer's total stall time did not change. Unpaced (an earlier build of
  the experiment, a thread per spare) the mapped reader's p99.9 was worse on both architectures.
- **The repeat of ADR 0023's cell does not cover its failure condition.** ADR
  [0025](0025-linux-prefault-small-window.md) says of that machine (`c6id.xlarge`): "The small VM cannot sustain about
  106 MB/s for minutes: every configuration stalls there for hundreds of milliseconds. That is a disk limit". The old
  cell ran near that limit; the same 100,000 records/s is about 30 % of the probe of the machines measured now. The 30
  runs per cell also used the rejected build that starts a thread per spare; only 15 per architecture used the one
  kept.
- **Not measured with the kept build:** unpaced, xfs at 90 % of the probe, `HelperCpus::Inherited`, 4 vCPUs, bare
  metal.

## Options considered

1. **On by default on Linux.** The gain away from saturation is real. Rejected: nothing measured near the condition
   under which it failed, and the one near-saturated cell is worse.
2. **Not at all, as ADR 0023.** Rejected: the gain away from saturation is real and reproducible, and a user with
   headroom on the disk can have it.
3. **An option, off by default on Linux (chosen).** A user measures it on their own machine; the default stays as it
   was.
4. **A thread per spare on Linux, as on macOS.** Rejected: it made the writer's worst append worse than without a
   spare.

## Decision

`WriterConfig::spare_segment`, from 1.1, separate from `prefault` (before it, refusing the spare on macOS also meant
refusing prefaulting and early write-back). Default on on macOS (unchanged), **off on Linux**, where the writer then
behaves as before: no thread, descriptor or disk space more. It applies only with `prefault` on. With it on, on Linux:

- One thread (`tickertape-spare`) for the writer's life, started in `Writer::open`, prepares a spare once the active
  segment is half full.
- **It does not sync the spare.** `fallocate` has allocated the blocks (C10); the checkpoint that first covers the
  segment syncs its size and header, as for a segment a roll creates itself (ADR 0038). A spare linked without a sync
  is the state C8 already describes; the format and the guarantees do not change.
- **A roll does not wait for a spare that is not ready:** it creates the segment itself, and the spare is the next
  roll's. The one exception is a storage-full error of that create: the spare may hold the space it lacked, so the
  roll then waits for the spare, with no time limit (as long as the filesystem's `fallocate` takes), and uses it. If a
  spare was being prepared and came to nothing (both can run out of space together, and the preparation then gives its
  blocks back), the roll tries its create once more and returns that result. So a spare does not make a roll fail for
  lack of space that would have succeeded without it.
- A failed roll leaves the request for a spare as it was: no second preparation until a roll succeeds (as macOS).
- A thread that panics ends the spares of that writer and nothing else; the file it left is removed at the next roll
  that runs out of space, and when the writer is dropped. Dropping the writer waits for a spare being prepared.

The option is exposed as the other writer options of its kind are: C `tickertape_writer_options_set_spare_segment` (an
addition to the frozen header, as ADR 0043 allows), C++ and Python `spare_segment`, Java `spareSegment`.

**What would have to be measured to make it the default:** the kept build at 90 % of the probe and unpaced on both
architectures, and ADR 0023's cell at 75–100 % of a small device's limit, with at least 20 interleaved runs per
configuration, the build that runs first in a pair alternating, and the statistical test chosen beforehand.

## Consequences

- `docs/operations.md` says what the option costs (a descriptor, a segment mapping, a segment's worth of disk space,
  an alert that fires earlier).
- ADR 0023's conclusion (no spare on Linux) is the default; this record is the exception.
- Tests: `tests/spare_segment.rs` (including `::no_spare_by_default_on_linux` and the storage-full cases),
  `tests/disk_full.rs::a_roll_does_not_fail_because_the_spare_took_the_last_space`,
  `::a_roll_racing_the_spare_for_the_last_space_does_not_fail` and
  `::a_roll_does_not_fail_when_the_spare_gives_the_last_space_back`,
  `tests/open_segments.rs::many_rolls_with_the_spare_segment_under_a_low_descriptor_limit`.

## Amendment (2026-11-27): the blocks a spare gives back

"The preparation then gives its blocks back" was not always true. A spare that came to nothing was unlinked and closed,
and that frees nothing while another descriptor of the file is open: a child process forked at that moment holds a copy
of every descriptor until its `exec` (with `O_CLOEXEC`), or for good if it does not `exec`. The roll that had run out of
space and waited for the spare then tried again into space that was still taken, and failed with "no space". The long
test `tests/disk_full.rs::a_roll_does_not_fail_when_the_spare_gives_the_last_space_back` failed this way now and then in
CI (issue #73), beside tests that start `sudo` and writer processes.

Rule from now on: a file of an unpublished segment whose blocks are given back is truncated to zero length before its
name is removed (`journal::discard_unpublished`). This covers a spare whose preparation failed, a spare that a roll
discards (wrong size, failed header write or link), the file a panicked spare thread left, and the temporary file of a
segment a roll failed to create (whose partial preallocation ext4 keeps). It is the same code on macOS (ADR 0022). No
API, format or crash-contract change.

Tests: `tests/spare_segment.rs::a_failed_spare_gives_its_blocks_back_while_its_file_is_open_elsewhere` (both
platforms, no small filesystem needed; fails without the rule), and the long test above, which now keeps a descriptor of
the spare open while its preparation fails (fails on every run without the rule). The three long tests of the spare on a
small filesystem now run one at a time: background holds and failure points are process-wide, and run in parallel one
test's spare thread took another test's hold or failure.
