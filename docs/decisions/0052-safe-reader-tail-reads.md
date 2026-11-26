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

A fill of the buffer **follows** if the slot at the cursor was found empty before it, and the reader used exactly one
record of the last fill. After 16 fills in a row that follow, the 16th included, a fill asks for 4 KiB, as long as each
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
- **State.** Two bytes, and no extra work per record or per empty poll: whether the slot was empty is the existing
  `waiting_since` (set at an empty slot, cleared when the cursor moves), and the one record is the commit word at the start
  of the buffer, which the last fill began with, against the cursor less `buf_start`. All of it is in the function that
  fills the buffer. The functions of the record path (`try_next`, `advance`, `next_slot`, `read_slot`, `next_timeout`)
  compile to the same instructions as before on aarch64 and x86-64; `at_empty_slot` and the seeks differ only in the
  offsets of two `bool` fields.
- **Cost of growing.** The buffer is zero-filled from 4 KiB to 64 KiB at a probe and when the reads grow back.
- **Not observable.** The same records and errors: the size decides only which bytes are in the buffer after the frame at
  the cursor, and bytes there are used as before (a commit word that is zero there is read again from the file, and a
  frame that does not validate is read again, with a whole chunk). That is how the buffer always behaved at a segment's
  end.

Why 16: a spinning reader that meets a late record every few hundred gets back to small reads quickly, and a blocking
reader that finds about four in ten records at once (aarch64, 200,000 records/s) rarely has 16 waits in a row.

## Consequences

- Measured before and after (x86-64 and aarch64 cloud VMs, `docs/benchmarks.md`, 2026-11-26): replay and the 1.5 million
  records/s cells within the noise floor; spinning readers at 20,000 and 200,000 records/s read 4.4–11.7 KB per record
  instead of 66–68 KB, with p50 halved or better; blocking readers at 20,000 records/s have 11–14 % (x86-64) and 23–26 %
  (aarch64) lower p50 and CPU, with the same waits and wakes; a blocking reader at 200,000 records/s on aarch64 is
  unchanged.
- **Blocking readers at 200,000 records/s on x86-64 still reach the small reads part of the time.** There, p50 was 25–28 %
  lower and the reader's CPU 12–14 % lower, p99 and the writer's late sends did not change, and the reader thread blocked
  38–59 % more often, so the writer woke it more often (the first attempt: 92 %). Reading less there trades the reader's
  CPU and the waiting records' latency against futex wakes by the writer; this ADR does not settle that trade.
- Records larger than 4 KiB are followed with reads of their own size once the reads are small.
- Tests: `reader.rs` drives the size through shrink and grow and checks the size of each fill (single records, batches, a
  backlog, records larger than 4 KiB and than 64 KiB, the probe, rolls, seeks, marker refreshes, a slot read again), and
  compares the records with the zero-copy reader's throughout.
