# 0059: Release finished segments off the writer's thread

- **Status:** proposed
- **Date:** 2026-11-27

## Context

A dirty list keeps the file and mapping of its four newest segments only (ADR 0038 → Bounded open segments). A writer
that rolls without checkpoints, or faster than its checkpoints take segments off its hands, pushes a fifth segment at
every roll, and the push let go of the oldest one there: an `fstat` for its identity, then the last reference to its
mapping (`munmap`) and file (`close`), all on the writer's thread, inside the `append` that rolled. With checkpoints at
least once per roll the writer's list rarely holds five, and the segments go with the job, which the checkpoint thread
drops.

What was known (issue #60): on Linux a roll's `append` took 142–184 µs with checkpoints and 0.9–1.0 ms without
(`docs/benchmarks.md` → "2026-11-25, roll and journal stalls"), and unmapping a 128 MiB segment took 0.4–0.65 ms where a
trace caught it, which was the likely rest, not confirmed; on macOS "the writer's own thread releases the oldest of four
open dirty segments: 1.7–16 ms per roll for any writer that doesn't checkpoint" (the macOS section of 2026-11-26).

### What the roll is made of

Confirmed first, with timers around each part of the roll in a local build of `main` (never committed), 1,024 B at the
steady-50 rate without checkpoints, 128 MiB segments, on one x86-64 and one aarch64 Google Cloud VM (ext4, kernel 7.0;
`docs/benchmarks.md` → "2026-11-26, finished segments released off the writer's thread"). Medians over the rolls that
released a segment: the roll took 991 µs (x86-64) and 847 µs (aarch64), of which the `munmap` of the released segment
812 and 664 µs, creating the new segment 158 and 166 µs, and the `close`, the `fstat`, the seal, the hand-overs and the
publication together less than 20 µs. With the spare segment on: 889 and 718 µs, the `munmap` 840 and 657 µs. With
checkpoints every second nothing was released at a roll, which took 171 and 172 µs. So the `munmap` is the larger part
(78–94 % of the roll), and the `close` is not worth moving on Linux. On macOS (one short pair with 8 MiB segments on a
busy laptop) the release took 150 of a 418 µs roll, and there the `close` (103 µs) was larger than the `munmap` (46 µs);
both go together, since they are the same hand-over.

## Options considered

1. **Leave it.** A writer that checkpoints every second does not pay it. Rejected: writers without checkpoints, or with
   a checkpoint interval longer than a roll, pay it at every roll: most of the roll on Linux.
2. **A new helper thread for releases.** Rejected: an existing helper fits (below), and every thread is one more to
   place (`WriterConfig::helper_cpus`), start and join.
3. **The checkpoint thread.** Rejected: it starts with the first `request_checkpoint`, so a writer without checkpoints,
   the one that pays, has none; and a release queued behind a slow checkpoint's syncs would hold its descriptors that
   long.
4. **The early write-back helper.** Rejected: off with `early_writeback(false)`, and it has write-back of its own to do.
5. **The prefault helper, bounded, with the writer as the fallback.** **Chosen.**

## Decision

- **Linux only.** On macOS the writer keeps releasing segments on its own thread (`writer::HAND_OFF`; decided by the
  orchestrator after the one macOS measurement, below). Everything that follows is about Linux.
- **Who releases.** The prefault helper (`tickertape-prefault`), which exists whenever `WriterConfig::prefault` is on
  (the switch for every helper thread) and is woken at every roll anyway: the segment a roll's push releases goes to it
  in the same lock and wake-up as the switch to the new segment (`Prefaulter::switch`), and a segment a
  `request_checkpoint` releases (its own push, or `Job::absorb`) goes to it with `Prefaulter::release`. The writer still
  reads the segment's identity (`fstat`) itself, while the file is open, as before (`checkpoint::Released`). The helper
  takes any prefault work it has before a release, so it releases only once it has caught up with prefaulting (a helper
  that is behind releases nothing, the queue fills, and the writer releases itself). The release runs where the helper
  runs (`helper_cpus`): if that can be the writer's CPU (`HelperCpus::Inherited` from a pinned thread, or a set that
  includes it), the `munmap` can delay an ordinary append by as long as the roll used to (`docs/operations.md` → Writer
  threads says so; no logic is added for it).
- **The bound.** At most two released segments wait for the helper, the one it is releasing included
  (`prefault::RELEASE_QUEUE`). Two, so that a roll finds room while the helper still unmaps the previous roll's segment,
  or a request comes right after a roll that released one. They can be segments no list holds, so the writer holds at
  most two more descriptors and two more segment mappings than before: 21 files by default, 22 with the spare segment,
  20 with early write-back off; macOS stays at 18 (ADR 0054, amended; `docs/operations.md` → Open files).
- **The fallback.** When two wait, or the helper has ended, the writer releases the segment on its own thread, as
  before; it never waits for the helper. With `prefault(false)` there is no helper and nothing changes. A helper that
  panics marks itself ended and drops whatever waits as it unwinds (`HelperEnded`), so nothing waits for it and nothing
  more is handed to it. On Linux kernels before 5.14, which reject the prefault advice, the helper stops advising but
  keeps running, to release.
- **The order kept.** A segment is released at the same push as before or later, never earlier: after its seal (at its
  own roll), after its hand-over to the early write-back helper (at its own roll, before it joins the list; that helper
  holds its own reference to the file, so it can still write it back), and after it joined the dirty list, whose push
  now returns what it let go of instead of dropping it (`DirtySegments::push`). Ownership does the rest: each handle is
  moved, never copied, so nothing is closed or unmapped twice, and on drop the writer stops and joins the helper (in the
  process that opened it), which drops what waits.
- **Durability and readers.** Unmapping and closing write nothing. A segment's data reaches the device only through a
  checkpoint: step 1 of `checkpoint::steps` (the ranged `msync` through a held mapping handle, or the reopen of a
  released segment, then `fdatasync`/`fsync`, and on macOS the reopened file's `msync` and the final `F_FULLFSYNC`),
  plus, on Linux, the early write-back helper's `sync_file_range`, which starts write-back and makes nothing durable.
  That does not move. A checkpoint that reopens a released segment may now run while the helper still holds its old
  mapping and descriptor; the data is in the file's page cache either way (crash contract → Segments the writer no
  longer holds), and the limit stated there (a write-back error the kernel forgot while no descriptor was open) is no
  wider: the descriptor is open as long as before or longer. Readers map segments themselves. Nothing in `status` or
  `Writer::metrics` reports open segments or mapped bytes.
- **Tests** (`tests/segment_release.rs`; the holds are process-wide, so they run one at a time): twenty rolls that each
  release a segment make no `munmap` or `close` of a segment on the writer's thread while the helper keeps up, and the
  helper's thread makes them (`strace -f -y`; Linux, required in CI as `tests/writer_latency.rs` is), and without
  helpers the writer's thread makes them; with the helper held at `fault::Point::SegmentRelease` holding one segment and
  a second waiting, twenty more rolls leave the descriptors and journal mappings where they were and nothing more waits
  (fallback); dropping the writer with two waiting closes and unmaps everything once; a helper made to panic there
  (`fault::Point::SegmentReleasePanic`) is not waited for, and every later segment is released by the writer. The exact
  bound: on Linux `tt-open-segments worst` now also holds the prefault helper with two segments no list holds, so
  `tests/open_segments.rs` checks 17 and 18 (with the spare) descriptors beyond those after the open (macOS 14, as
  before), that two wait, and that the segment the next roll releases is given back to the writer, so that a larger
  queue fails the test. On macOS the tests of the hand-off are skipped with a message, and one checks that nothing is
  handed over. Unit tests in `prefault.rs` cover the queue's limit and give-back. Each safety test was checked to fail
  with the property removed (an unbounded queue; no ended guard; queued segments forgotten on stop; no hand-over at all,
  which also fails the strace test and the exact count). No test measures time.

## Measurements

Interleaved, base (`main`), change, base again, one 60-second run each, steady-50, 1,024 B and 64 B, checkpoints every
second or none, spare segment on and off, on one x86-64 and one aarch64 VM (on aarch64 the two 1,024 B cells without
checkpoints twice for the change and three times for the base); one unpaced run per build; the tables are in
`docs/benchmarks.md`.

- **Without checkpoints** the roll's append fell from 0.83–1.06 ms to 169–179 µs at the median (spare off) and from
  0.71–0.94 ms to 49–60 µs (spare on), its maximum from 0.73–1.14 ms to 60–211 µs, on both architectures and payloads.
- **With checkpoints every second** nothing moved beyond the spread of the two base runs.
- **The whole distribution:** p99.9 and p99.99 within or next to the base runs, except x86-64, 1,024 B without
  checkpoints, where the change's p99.99 was 23.3 against 21.2 and 21.6 (spare off) and 25.5 against 16.7 and 17.2
  (spare on), from one run each; on aarch64, where those cells were repeated, no difference. One aarch64 change run had
  one 1.03 ms append that was not a roll; no other change run without checkpoints had an append above 211 µs. **Open.**
  One explanation fits the x86-64 result and was **not tested**: the `munmap` now runs on another CPU of the writer's
  process, so its TLB flushes interrupt the writer's CPU, where before the writer flushed its own TLB locally; x86-64
  sends them as interrupts (that VM has two threads per core), aarch64 broadcasts them in hardware. The
  `/proc/interrupts` samples of the session cannot test it (they cover the 64 B runs, and their 5-second totals per CPU
  hide a few flushes per roll among the 37,000 a second that write-back causes in every run). What would settle it:
  repeated x86-64 runs of the 1,024 B cells without checkpoints with `tlb:tlb_flush` and
  `irq_vectors:call_function_entry` traced on the writer's CPU around each roll.
- **The writer thread's page faults** (`getrusage(RUSAGE_THREAD)`): the same for both builds on aarch64; on x86-64 they
  vary from run to run, and without checkpoints with the spare segment the change's count was well above both base runs
  (65,214 against 12,002 and 11,960 at 1,024 B; 75,312 against 40,647 and 44,175 at 64 B), but one change count is
  outside its base pair even where nothing is released at a roll (64,518 against 45,719 and 47,487, 64 B with
  checkpoints). Not explained; one run each.
- **Unpaced:** the same rate (403,596 against 404,055 and 408,629 against 412,383 records/s); the tails there are device
  stalls, which one run per build cannot compare.
- **Instructions per append** (ADR 0047's gate 1): 384.000 on x86-64 and 368.000 on aarch64, the same for both builds.
- **macOS** (why the hand-off is Linux only): one run per build, 8 MiB segments (a sixteenth of the default), no
  checkpoints, under 2 GB of writes, on a laptop busy with other work. Base (instrumented) → change: roll append p50 421
  → 371 µs, p99 693 µs → 4.2 ms, maximum 1.6 → 4.2 ms; all appends p50 0.084 → 0.125 µs, p99 0.167 → 0.333 µs, p99.9 1.0
  → 3.7 µs, p99.99 393 → 356 µs, maximum 1.6 → 5.8 ms; late sends 0.35 → 1.5 %. In the base the release took 150 of a
  418 µs roll (`close` 103, `munmap` 46). A possible mechanism, **not confirmed**: macOS's `MADV_WILLNEED` maps nothing,
  so the writer soft-faults on each new 16 KiB page, and the helper's `munmap` holds the process's VM map lock
  exclusively, moving the stall from the roll to the writer's next fault. Before macOS could get the hand-off it would
  need runs with 128 MiB segments on an idle Mac, several per build, with the writer's faults counted; the 1.7–16 ms per
  roll reported for 128 MiB segments on 2026-11-26 was not measured again.

## Consequences

- On Linux two more descriptors and segment mappings at most, said in the changelog (ADR 0054's rule for raising the
  count); macOS unchanged.
- The roll's `append` without checkpoints costs what it costs with checkpoints (creating or publishing the next
  segment), on Linux; the `munmap` moves to the prefault helper, which also prefaults, so for that time (0.66–0.84 ms
  per roll on these VMs) it does not advise the window ahead: it advises the new segment's first window first, and the
  writer faults pages itself if it overtakes the window meanwhile.
- `tests/open_segments.rs::most_extra_descriptors` and `docs/operations.md` → Open files carry the new numbers; a change
  of `prefault::RELEASE_QUEUE` must change them.
- **Not measured:** xfs, steady-90, `HelperCpus::Inherited` (the helper sharing the writer's CPU), small machines, bare
  metal, macOS with default segments, more than one run per build on x86-64, and whether the x86-64 p99.99 difference at
  1,024 B without checkpoints is real (it needs repeated runs there); where the writer thread's extra faults on x86-64
  come from, if they are real; kernels before about 6.6, where a fault on a shared file page takes `mmap_lock` for
  reading and can queue behind a writer of that lock while the helper clears page tables (on the measured 7.0 kernel
  `munmap` downgrades the lock before it clears them, and such faults take the per-VMA lock); and a writer above about
  310–390 MB/s, which overtakes the 256 KiB prefault window while the helper unmaps (it advises nothing meanwhile), as
  in the unpaced runs.
