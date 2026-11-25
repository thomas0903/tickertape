# 0052: How much the safe reader reads at the tail

- **Status:** proposed
- **Date:** 2026-11-26
- **Relates to:** [0006](0006-reader-api.md) (the two readers), [0017](0017-reader-wake-ups.md) (blocking readers);
  issue #62

## Context

The safe reader (`Reader::open`) fetches 64 KiB per `pread` and takes records from that buffer. On replay that is one
system call per 64 KiB. A reader that has caught up finds one new record per read, and zeros after it: it copies 64 KiB
for nearly every record, about 1.3 GB/s to follow 2 MB/s of frames at 20,000 records/s (`docs/benchmarks.md`, the
profiles of 2026-11-25).

The first attempt (branch `core/safe-reader-tail-reads`, not merged) read 4 KiB after any read that reached past the
newest record, and doubled the size only when a buffer was used up to its end. It lowered the latency of readers that
wait, and made others worse. Its counts (raw data of that session, x86-64) show one thing plainly: at 1.5 million
records/s, 0.52 reads of 4 KiB and 0.53 reads of a commit word per record, so at most 0.01 of the commit words per record
were empty. **98 % of its small reads followed no empty slot**: the reader was behind the writer, not waiting for it.
A small read that finds the newest record leaves zeros behind itself just as a large one does, so that rule never took
the reader back to large reads. It then sat one to two records behind the writer (p50 0.8 instead of 3.1 µs), waited
0.0013 times per record instead of 0.0001 and cost the writer 0.011 wake-ups per record instead of 0.0022, eight per
wait; that this made the blocking reader's p99 (11 to 102 µs) is likely and not shown, and it can't explain the spinning
reader's tail, which grew too. At 200,000 records/s a blocking reader on x86-64 alternates on the base (it waits for a
record, 3 commit-word reads, and is still copying 64 KiB when the next one comes, 1 read: 2.0 commit-word reads and
0.5 waits per record); with the copy gone it waits for every record (2.9 and 0.97), hence twice the futex wakes. The
copy worked as a short spin. The loss on replay had no mechanism that the code shows (it was found on the zero-copy
reader too, whose code was the same); the change had added code to the function that reads each slot.

## Options considered

1. **The first attempt.** See above.
2. **Shrink after several empty polls, grow at the first wholly used buffer.** The first half avoids nothing: any stall
   of the writer gives empty polls. The second half never happens at 1.5 million records/s with small reads: 0.5 reads
   per record use one to two records each.
3. **A 4 KiB read for any record the reader waited for.** No memory. Every blocking reader then waits more often,
   and so wakes the writer more often; the two bad cells at 200,000 records/s stay.
4. **Read less only while the reader waits for every record even when it reads whole chunks** (chosen).

## Decision

A read of the buffer (a *fill*) **follows** if the slot at the cursor was found empty before it and what the reader used
of the last fill, at least one record, is at most 4 KiB. After 16 fills in a row that follow, the 16th included, a fill
asks for 4 KiB, as long as each fill follows, except one in 128 of them (a *probe*), which asks for 64 KiB; the first
fill that does not follow asks for 64 KiB, and the count starts over.
A fill always covers the frame at the cursor and never passes the segment's end, as before.

- **Where nothing changes.** On replay and catch-up no slot is empty before its fill, so no fill follows. A fill
  of the first slot of a segment does not follow (the last fill lies in the segment before), nor does the first after a
  seek, nor one of the same slot again (its record failed validation). A durable-only reader comes to no empty slot. A
  reader that now and then finds its next record at once while it reads whole chunks is at the limit of what it can
  follow, and stays there: a blocking reader at 200,000 records/s, every reader at 1.5 million.
- **Why the probe.** Without it, small reads can hold themselves in place. Measured on x86-64 without the probe: at
  200,000 records/s a blocking reader that a pause of the writer had brought to small reads waited for every record,
  because its reads were now fast, so every fill followed, and it stayed there (the context switches of the reader
  thread rose by 66–85 %, the waits of the first attempt), while in other runs of the same configuration it never left
  whole chunks. A probe gives such a reader the time of a whole chunk; it then finds the next record at once and goes back
  to whole chunks. A reader with real slack still waits after the probe and keeps its small reads; the probe costs it
  64 KiB in 128 fills, about 500 bytes per fill.
- **State.** One byte, and no extra work per record or per empty poll: whether the slot was empty is the existing
  `waiting_since` (set at an empty slot, cleared when the cursor moves), and what was used is the cursor less `buf_start`.
  All of it is in the function that fills the buffer. The functions on the record path (`try_next`, `advance`, `next_slot`,
  `read_slot`, `next_timeout`, `at_empty_slot`) compile to the same instructions as before on aarch64 and x86-64.
- **Cost of growing.** The buffer is zero-filled from 4 KiB to 64 KiB at a probe and when the reads grow back: at most
  once in 16 fills.
- **Not observable.** The same records and errors: the size decides only which bytes are in the buffer after the frame at
  the cursor, and bytes there are used only as before (a commit word that is zero there is read again from the file, and a
  frame that does not validate is read again, at 64 KiB). It is how the buffer always behaved at a segment's end.

Why 16: a spinning reader that meets a late record every few hundred gets back to small reads quickly, and a blocking
reader that finds about 4 of 10 records at once (aarch64, 200,000 records/s) has 16 waits in a row in about one of 3,500
places, and leaves at the next one it does not wait for. Why the limit on what was used: a reader of batches larger than
4 KiB would alternate between both sizes, with the zero-fill each time.

## Consequences

- A blocking reader at rates between "waits for every record" and "never waits" still copies 64 KiB for most records. Its
  copy is what keeps it from sleeping; reading less there trades reader CPU and the waiting records' latency against futex wakes of the
  writer, and is not decided here.
- Records larger than 4 KiB are followed with 64 KiB reads.
- Tests: `reader.rs` drives the size through shrink and grow and checks the size of each fill (single records, batches, a
  backlog, records larger than 4 KiB and than 64 KiB, rolls, seeks, marker refreshes, a slot read again).
