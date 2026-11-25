# 0049: The spare segment on Linux, as an option that is off by default

- **Status:** accepted (decided by the orchestrating agent on the owner's delegation, after one independent review)
- **Date:** 2026-11-26

## Context

ADR [0022](0022-prepared-next-segment.md) prepares the next segment in the background: a roll then writes a header and
links a file instead of creating and preallocating one. ADR [0023](0023-no-prefault-advice-on-linux.md) decided against
it on Linux: in one run of four the spare's preallocation sync forced an ext4 journal commit and stalled the writer for
tens of milliseconds. Since then rolls no longer sync on the writer's thread (ADR
[0038](0038-background-checkpoints.md) → Rolls without a sync), so issue #60 asked for the experiment again. The results
are in `docs/benchmarks.md` → "2026-11-25, roll and journal stalls on Google Cloud VMs".

What was measured there:

- **Away from saturation** (half of the probe, and ext4 without checkpoints; aarch64 and x86-64) the spare makes the roll
  cheaper and the mapped reader's p99.9 lower.
- **At saturation it does not help, and the only near-saturated cell of the kept build points the wrong way:** aarch64
  ext4 at 90 % of the probe, against `main`, six runs each: runs with a roll of 20 ms or more 5 of 6 against 2 of 6, seconds
  with such a roll 10 against 3. The writer's total stall time did not change. Unpaced (an earlier build of the
  experiment, a thread per spare) the mapped reader's p99.9 was worse on both architectures.
- **The repeat of ADR 0023's cell does not cover its failure condition.** ADR
  [0025](0025-linux-prefault-small-window.md) says of that machine (`c6id.xlarge`): "The small VM cannot sustain about
  106 MB/s for minutes: every configuration stalls there for hundreds of milliseconds. That is a disk limit". The
  old cell ran near that limit; the same 100,000 records/s is about 30 % of the probe of the machines measured now. The
  30 runs per cell also used the rejected build that starts a thread per spare; only 15 per architecture used the
  one kept.
- **Not measured with the kept build:** unpaced, xfs at 90 % of the probe, `HelperCpus::Inherited`, 4 vCPUs, bare metal.

## Options considered

1. **On by default on Linux.** The gain away from saturation is real. Rejected: nothing measured near the condition under
   which it failed, and the one near-saturated cell is worse.
2. **Not at all, as ADR 0023.** Rejected: the gain away from saturation is real and reproducible, and a user with
   headroom on the disk can have it.
3. **An option, off by default on Linux (chosen).** A user measures it on their own machine; the default stays as it was.
4. **A thread per spare on Linux, as on macOS.** Rejected: it made the writer's worst append worse than without a spare.

## Decision

`WriterConfig::spare_segment`, from 1.1, separate from `prefault` (before it, refusing the spare on macOS also meant
refusing prefaulting and early write-back). Default on on macOS (unchanged), **off on Linux**, where the writer then
behaves as before: no thread, descriptor or disk space more. It applies only with `prefault` on. With it on, on Linux:

- One thread (`tickertape-spare`) for the writer's life, started in `Writer::open`, prepares a spare once the active
  segment is half full.
- **It does not sync the spare.** `fallocate` has allocated the blocks (C10); the checkpoint that first covers the
  segment syncs its size and header, as for a segment a roll creates itself (ADR 0038). A spare linked without a sync is the
  state C8 already describes; the format and the guarantees do not change.
- **A roll does not wait for a spare that is not ready:** it creates the segment itself, and the spare is the next
  roll's. The one exception is a storage-full error of that create: the spare may hold the space it lacked, so the
  roll then waits for the spare and uses it. Without the exception a disk with room for one more segment could make a
  roll fail that would have succeeded without a spare.
- A thread that panics ends the spares of that writer and nothing else. Dropping the writer waits for a
  spare being prepared.

The option is exposed as the other writer options of its kind are: C `tickertape_writer_options_set_spare_segment` (an
addition to the frozen header, as ADR 0043 allows), C++ and Python `spare_segment`, Java `spareSegment`.

**What would have to be measured to make it the default:** the kept build at 90 % of the probe and unpaced on both
architectures, and ADR 0023's cell at 75–100 % of a small device's limit, with at least 20 interleaved runs per
configuration, the build that runs first in a pair alternating, and the statistical test chosen beforehand.

## Consequences

- `docs/operations.md` says what the option costs (a descriptor, a segment mapping, a segment's worth of disk space, an
  alert that fires earlier).
- ADR 0023's conclusion (no spare on Linux) is the default; this record is the exception.
- Tests: `tests/spare_segment.rs` (including `::no_spare_by_default_on_linux` and the storage-full cases),
  `tests/disk_full.rs::a_roll_does_not_fail_because_the_spare_took_the_last_space`,
  `tests/open_segments.rs::many_rolls_with_the_spare_segment_under_a_low_descriptor_limit`.
