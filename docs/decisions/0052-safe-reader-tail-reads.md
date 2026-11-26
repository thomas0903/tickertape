# 0052: How much the safe reader reads at the tail

- **Status:** accepted (2026-11-26, by the orchestrator on the owner's delegation, after one independent review, two
  fix rounds and a re-check of the final code)
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
record of the last fill, and the reader did not block for this record (register as a waiter, so that the writer makes a
wake-up call). So blocking readers always read whole chunks, and only readers that do not block read less. After 16 fills in a row that follow, the 16th included, a fill asks for 4 KiB, as long as each
one follows, except one in 256 of them (a *probe*), which asks for 64 KiB. The first fill that does not follow asks for
64 KiB, and the count starts over. A fill always covers the frame at the cursor and never passes the segment's end, as
before.

- **Where nothing changes.** On replay and catch-up no slot is empty before its fill. A fill of the first slot of a
  segment does not follow (the last fill lies in the segment before), nor does the first after a seek, nor one of the
  same slot again (its record failed validation). A durable-only reader comes to no empty slot on an intact journal. A
  reader that finds its next record at once now and then, or more than one record in a fill, is at the limit of what it
  can follow and stays on whole chunks: every reader at 1.5 million records/s. A reader that blocks stays on whole chunks
  (below).
- **Why the probe.** Small reads can hold themselves in place: a blocking reader brought to them by a pause of the writer
  becomes fast enough to wait for every record, so every fill follows. Measured on x86-64 without the probe, at 200,000
  records/s: the blocking reader's context switches rose by 66–85 % in the runs that got there, while in other runs of
  the same configuration it never left whole chunks. A probe gives such a reader the time of a whole chunk; if it then
  finds the next record at once, or two records in the probe, it goes back to whole chunks. A reader with real slack still
  waits after the probe and keeps the small reads, which costs it 64 KiB in 256 fills, about 250 bytes per fill. One probe
  in 128 fills put 0.8 % of the records of a spinning reader at 20,000 records/s at the latency of a whole read, and
  raised its p99 by 22 %; one in 256 did not. The probe came before the rule for blocking readers below, which now keeps
  blocking readers on whole chunks by itself; it stays for readers that wait without blocking (the same can happen to a
  reader that spins slowly or yields), and every measurement of the rule built ran with it. Whether it still pays for
  itself there was not measured.
- **Readers that block** (decided 2026-11-26 under the owner's delegation, after two measurements of this ADR). A
  blocking reader that reads less finishes sooner and goes to sleep before the next record more often, and the writer
  pays a wake-up call for each record a reader blocks for: this library does not buy reader latency with writer time.
  - *Without any rule for them* (x86-64, blocking, 1,024 B, 200,000 records/s, 10 interleaved rounds): the writer's
    `append` p50 0.96 → 1.05 µs, +8.9 % in the median of the per-round ratios and slower in all 10 rounds (+5.5 to
    +16.7 %), against a floor of −4.5 to +6.6 % for a second copy of the base; p90 +4.2 %; the 64 B cell +0.3 % (noise).
    One wake-up costs the writer about 1.3 µs: its `append` p50 is 1.43 µs beside a blocking reader at 20,000 records/s
    and 0.108 µs beside a spinning one.
  - *A 20 µs threshold, measured and not built:* a fill for a record the reader blocked on counted if the reader had
    waited at the slot for at least 20 µs. It matched the base at 50,000 and 200,000 records/s and kept the blocking
    reader's gain at 20,000 records/s (p50 14.1 → 12.4 µs, 4.5 KB read per record instead of 67 KB), but there the
    writer's `append` p50 rose by 2.5–2.8 % (9 of 10 rounds at 64 B; floor +0.4 to +0.9 %; +1.6 % in all 10 rounds in the
    first session), with the same number of wake-ups and no known mechanism. An unexplained cost to the writer is not
    one to ship, however small.
  - *The rule built:* a fill for a record the reader blocked on never counts. Measured in the same session, at the floor
    in every blocking cell, the writer's `append` p50 at 20,000 records/s included (+1.3 % and −0.3 %). What it gives up:
    a blocking reader at 20,000 records/s keeps 14.1 µs p50 and reads about 67 KB per record. A reader that wants the
    lower latency spins: a spinning reader gets 63 % lower p50. It also removes a constant that was never swept, a read
    of the clock, and the threshold's blind spot (below).
- **State.** Behind the pointer to the reader's wait state, which `Reader` already had (its size affects replay
  speed, issue #86, so it does not grow): the count of fills (two bytes) and the slot the reader last found empty, with
  whether the wake-up region was attached then and whether the reader found the slot empty again while registered as a
  waiter. Nothing changes where the reader blocks: `block` takes the wake-up region out of the waker for the whole of
  `block_on`. Its check of the slot runs through `at_empty_slot`, which notes the region missing after it was attached at
  the slot's first empty poll; and if that check finds the record, the fill runs inside `block_on`, where the fill
  function sees the region taken out. Both count as blocked. (The threshold's build saw only the first, and counted a
  record found in the check as not blocked.) A reader without a region never counts as blocked: its writer makes no
  wake-up calls. A reader that stops blocking counts again from its first fill that follows. Whether the slot was empty
  is the existing `waiting_since`, and the one record is the commit word at the start of the buffer, which the last fill
  began with, against the cursor less `buf_start`. No clock is read for any of it. The functions of the record path
  (`try_next`, `advance`, `next_slot`, `read_slot`, `next_timeout`, `follow_seal`) compile to the same instructions as on
  `main` on aarch64 and x86-64 (`--emit asm`, compared function by function); `at_empty_slot` (once per empty poll), the
  fill function, the empty-slot check of ADR 0050 and the seeks differ.
- **Cost of growing.** The buffer is zero-filled from 4 KiB to 64 KiB at a probe and when the reads grow back.
- **Not observable.** The same records and errors: the size decides only which bytes are in the buffer after the frame at
  the cursor, and bytes there are used as before (a commit word that is zero there is read again from the file, and a
  frame that does not validate is read again, with a whole chunk). That is how the buffer always behaved at a segment's
  end.

Why 16: a spinning reader that meets a late record every few hundred gets back to small reads quickly, and a blocking
reader that finds about four in ten records at once (aarch64, 200,000 records/s) rarely has 16 waits in a row. **Neither
16 nor one probe in 256 was compared with other values in a sweep,** nor was the 20 µs of the threshold that was not
built. That one probe in 256 is what
removed the spinning reader's p99 rise at 20,000 records/s is an inference: the commit that made it (`c7bec958`, after
`81012b2c`'s one in 128) also changed what counts as following.

## Consequences

Measured on cloud VMs (`docs/benchmarks.md`, 2026-11-26):

- **Before the rule for blocking readers** (x86-64 and aarch64): replay and the 1.5 million records/s cells within the
  noise floor; spinning readers at 20,000 and 200,000 records/s read 4.4–11.7 KB per record instead of 66–68 KB, with p50
  halved or better. Blocking readers then read less too, with the costs to the writer above. On aarch64 at 200,000
  records/s only the 64 B cell could be compared (the 1,024 B runs were stalled by the storage): p50 unchanged
  (3.63 → 3.61 µs), p99 +5.6 % against a floor of −3.3 %, with overlapping per-round ranges.
- **The rule built** (x86-64; measured as the threshold's code with no limit, which differs from the merged code as
  `docs/benchmarks.md` states): blocking readers at 20,000, 50,000 and 200,000 records/s are the base's, blocks per
  record and the writer's `append` p50 and p90 included; at 1.5 million records/s p99 is within the floor; spinning
  readers keep their gains (p50 63 % lower at 20,000 and 200,000 records/s). Two medians lie above their floors with
  overlapping per-round ranges: p50 +1.5 % at 50,000 records/s, p99 +10.1 % at 20,000 records/s of 1,024 B.
- **Given up:** blocking readers do not read less. One at 20,000 records/s keeps 14.1 µs p50 and about 67 KB read per
  record (the threshold gave 12.4 µs and 4.5 KB, at a cost to the writer).
- **Known gap (found by the re-check, 2026-11-26):** a reader that finds a slot empty while it has no wake-up region,
  then attaches one (a writer started or restarted) and blocks at that same slot, has that one fill counted as following
  although it registered. It is one fill per attached region, and the next blocked fill resets the count, so it cannot
  lead to small reads by itself. No test covers that upgrade of the note.
- **Not measured:** the merged code itself (only the build described above); records larger than 4 KiB, which are
  followed with reads of their own size once the reads are small; the rule for blocking readers on aarch64; readers with
  the default backoff (spinning, yielding, then blocking), which read whole chunks for the records they block for and
  adapt where they spin.
- Tests: `reader.rs` drives the size through shrink and grow and checks the size of each fill (single records, batches, a
  backlog, records larger than 4 KiB and than 64 KiB, the probe, rolls, seeks including a wait right after one, marker
  refreshes, a slot read again, a reader that blocked in either way and one that stops blocking, and that blocking on the
  region is noted), and compares the records with the zero-copy reader's throughout.
