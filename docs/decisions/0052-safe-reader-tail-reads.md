# 0052: How much the safe reader reads at the tail

- **Status:** proposed
- **Date:** 2026-11-26
- **Relates to:** [0006](0006-reader-api.md) (the two readers), [0017](0017-reader-wake-ups.md) (blocking readers);
  issue #62; measured in `docs/benchmarks.md` (2026-11-26, the safe reader's read size)

## Context

The safe reader (`Reader::open`) fetches 64 KiB per `pread` and takes records from that buffer. On replay that is one
system call per 64 KiB. A reader that has caught up finds one new record per read, and zeros after it: it copies 64 KiB
for nearly every record, about 1.3 GB/s to follow 2 MB/s of frames at 20,000 records/s (`docs/benchmarks.md`, the
profiles of 2026-11-25).

The first attempt (branch `core/safe-reader-tail-reads`, not merged) read 4 KiB after any read that reached past the
newest record, and doubled the size only when a buffer was used up to its end. It lowered the latency of readers that
wait, and made others worse. Its counts (raw data of that session, x86-64) show why:

- **It shrank when the reader was not waiting.** At 1.5 million records/s it made 0.52 reads of 4 KiB and 0.53 reads of
  a commit word per record: at most 0.01 commit words per record were empty, so 98 % of its small reads followed no
  empty slot. The reader was behind the writer. A small read that finds the newest record leaves zeros after it, as a
  large one does, so the rule kept the reads small, and nothing used a 4 KiB buffer up to its end. The reader then ran
  one or two records behind the writer (p50 0.8 instead of 3.1 µs), waited 0.0013 times per record instead of 0.0001,
  and the writer made 0.011 futex wakes per record instead of 0.0022, about eight per wait. That this caused the
  blocking reader's p99 (11 to 102 µs) is likely and not shown; the spinning reader's tail grew too, without waits.
- **A fast reader waits more.** At 200,000 records/s a blocking reader on x86-64 alternates on the base: it waits for a
  record (3 commit-word reads), and is still copying 64 KiB when the next one comes (1 read): 2.0 commit-word reads and
  0.50 waits per record. With small reads it waited for nearly every record (2.9 and 0.97), hence twice the futex wakes.
  The copy had worked as a short spin.
- **Replay** was 1.6–7.8 % slower in its paired medians, the zero-copy reader too, whose code it did not change. That
  session ran eight rounds of a benchmark whose rounds differ by up to 20 % on these machines (this change measured 60
  short rounds instead). No mechanism in the code is shown; the change had added a branch to the function that reads
  each slot.

## Options considered

1. **The first attempt.** See above.
2. **Shrink after several empty polls, grow at the first wholly used buffer.** Any pause of the writer gives empty
   polls, and the reads never grow at 1.5 million records/s, where each small read is used for one or two records.
3. **A 4 KiB read for every record the reader waited for.** No memory. The two bad cells at 200,000 records/s stay.
4. **Read less only while the reader keeps showing that it waits for every record even when it reads whole chunks**
   (chosen).

## Decision

A fill of the buffer **follows** if the slot at the cursor was found empty before it, the reader used exactly one
record of the last fill, and, if the reader blocked for this record, it had waited at the slot for at least 20 µs.
After 16 fills in a row that follow, the 16th included, a fill asks for 4 KiB, as long as each
one follows, except one in 256 of them (a *probe*), which asks for 64 KiB. The first fill that does not follow asks for
64 KiB, and the count starts over. A fill always covers the frame at the cursor and never passes the segment's end, as
before.

- **Where nothing changes.** On replay and catch-up no slot is empty before its fill. A fill of the first slot of a
  segment does not follow (the last fill lies in the segment before), nor does the first after a seek, nor one of the
  same slot again (its record failed validation). A durable-only reader comes to no empty slot on an intact journal. A
  reader that finds its next record at once now and then, or more than one record in a fill, is at the limit of what it
  can follow and stays on whole chunks: every reader at 1.5 million records/s.
- **Why the probe.** Small reads can hold themselves in place: a blocking reader brought to them by a pause of the writer
  becomes fast enough to wait for every record, so every fill follows. Measured on x86-64 without the probe, at 200,000
  records/s: the blocking reader's context switches rose by 66–85 % in the runs that got there, while in other runs of
  the same configuration it never left whole chunks. A probe gives such a reader the time of a whole chunk; if it then
  finds the next record at once, or two records in the probe, it goes back to whole chunks. A reader with real slack still
  waits after the probe and keeps the small reads, which costs it 64 KiB in 256 fills, about 250 bytes per fill. One probe
  in 128 fills put 0.8 % of the records of a spinning reader at 20,000 records/s at the latency of a whole read, and
  raised its p99 by 22 %; one in 256 did not.
- **Readers that block** (decided 2026-11-26 under the owner's delegation, after the first measurement of this ADR).
  A blocking reader that reads less finishes sooner and goes to sleep before the next record more often, and the writer
  pays a wake-up call for each record a reader blocks for: this library does not buy reader latency with writer time.
  Measured without this rule on x86-64 (blocking, 1,024 B, 200,000 records/s, 10 interleaved rounds): the writer's
  `append` p50 0.96 → 1.05 µs, +8.9 % in the median of the per-round ratios and slower in all 10 rounds (+5.5 to
  +16.7 %), against a floor of −4.5 to +6.6 % for a second copy of the base; p90 +4.2 %; the 64 B cell +0.3 % (noise).
  One wake-up costs the writer about 1.3 µs: its `append` p50 is 1.43 µs beside a blocking reader at 20,000 records/s
  and 0.108 µs beside a spinning one. So a reader that registered as a waiter for a record counts that record's fill as
  following only if it had waited at the slot for at least 20 µs since it first found it empty: then it waits for the
  writer whether its reads are small or whole. Spinning readers make no wake-up calls and keep the rule above. The
  alternative kept ready was simpler: a reader that blocked for a record never counts it (blocking readers always read
  whole chunks). The threshold was built because it met the gates set before measuring it: at 50,000 and 200,000
  records/s blocks per record and the writer's `append` p50 and p90 within the floor of a second copy of the base, with
  no split of the 50,000 records/s cell between rounds; at 20,000 records/s a gain in p50 and bytes read with blocks per
  record unchanged; p99 at 1.5 million records/s no worse than the floor. Had it missed one, the simpler rule would have
  been built. See Consequences for the writer's cost at 20,000 records/s, which no gate covered.
- **State.** Behind the pointer to the reader's wait state, which `Reader` already had (its size affects replay
  speed, issue #86, so it does not grow): the count of fills (two bytes) and the slot the reader last found empty, when,
  and whether it found it empty again while registered as a waiter. That last part needs no change where the reader
  blocks: `block` takes the wake-up region out of the waker for the whole of `block_on`, whose check of the slot runs
  through `at_empty_slot`, which notes the region missing there after it was attached at the slot's first empty poll. A
  reader without a region never counts as blocked (its writer makes no wake-up calls), and one that registers and finds
  the record in that check is not counted either. Whether the slot was empty is the existing `waiting_since`, and the one
  record is the commit word at the start of the buffer, which the last fill began with, against the cursor less
  `buf_start`. The clock is read in the function that fills the buffer only for a reader that blocked, and at empty
  slots, where it was read already. The functions of the record path (`try_next`, `advance`, `next_slot`, `read_slot`,
  `next_timeout`, `follow_seal`) compile to the same instructions as on `main` on aarch64 and x86-64 (`--emit asm`,
  compared function by function); `at_empty_slot` (once per empty poll), the fill function, the empty-slot check of
  ADR 0050 and the seeks differ.
- **Cost of growing.** The buffer is zero-filled from 4 KiB to 64 KiB at a probe and when the reads grow back.
- **Not observable.** The same records and errors: the size decides only which bytes are in the buffer after the frame at
  the cursor, and bytes there are used as before (a commit word that is zero there is read again from the file, and a
  frame that does not validate is read again, with a whole chunk). That is how the buffer always behaved at a segment's
  end.

Why 16: a spinning reader that meets a late record every few hundred gets back to small reads quickly, and a blocking
reader that finds about four in ten records at once (aarch64, 200,000 records/s) rarely has 16 waits in a row. **None of
16, one probe in 256 and the 20 µs wait was compared with other values in a sweep.** That one probe in 256 is what
removed the spinning reader's p99 rise at 20,000 records/s is an inference: the commit that made it (`c7bec958`, after
`81012b2c`'s one in 128) also changed what counts as following.

## Consequences

Measured on cloud VMs (`docs/benchmarks.md`, 2026-11-26):

- **Without the rule for blocking readers** (x86-64 and aarch64): replay and the 1.5 million records/s cells within the
  noise floor; spinning readers at 20,000 and 200,000 records/s read 4.4–11.7 KB per record instead of 66–68 KB, with p50
  halved or better; blocking readers at 20,000 records/s 11–14 % (x86-64) and 23–26 % (aarch64) lower p50 and CPU, with
  the same waits and wakes. Blocking readers at 200,000 records/s on x86-64 reached small reads part of the time and
  blocked 38–59 % more often, and the writer's `append` p50 rose by 8.9 % at 1,024 B (above). On aarch64 at 200,000
  records/s only the 64 B cell could be compared (the 1,024 B runs were stalled by the storage): p50 unchanged
  (3.63 → 3.61 µs), p99 +5.6 % against a floor of −3.3 %, with overlapping per-round ranges.
- **With it** (x86-64 only; the rule was not measured on aarch64): blocking readers at 50,000 and 200,000 records/s are
  the base's, blocks per record and the writer's `append` p50 and p90 included; at 1.5 million records/s p99 is within
  the floor; spinning readers keep their gains. At 20,000 records/s the blocking reader keeps a p50 11–12 % lower and
  reads 4.5–4.8 KB per record instead of 67 KB, with the same number of blocks, **but the writer's `append` p50 there
  was 2.5–2.8 % higher (about 40 ns; p90 2.0–3.5 %), slower in 9 of 10 rounds at 64 B**, against a floor of +0.4 to
  +0.9 %; with the simpler rule +1.3 % and −0.3 %. Its cause was not measured. This is the open question for the owner:
  that small cost to the writer against the reader's gain at low rates, or the simpler rule.
- **Not measured:** records larger than 4 KiB, which are followed with reads of their own size once the reads are small;
  the rule for blocking readers on aarch64; readers with the default backoff (spinning, yielding, then blocking), which
  count as blocked only for records they registered for.
- Tests: `reader.rs` drives the size through shrink and grow and checks the size of each fill (single records, batches, a
  backlog, records larger than 4 KiB and than 64 KiB, the probe, rolls, seeks including a wait right after one, marker
  refreshes, a slot read again, readers that blocked briefly or long, and that blocking on the region is noted), and
  compares the records with the zero-copy reader's throughout.
