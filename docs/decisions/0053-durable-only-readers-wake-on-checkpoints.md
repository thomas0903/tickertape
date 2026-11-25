# 0053: Durable-only readers wake on checkpoints only

- **Status:** proposed
- **Date:** 2026-11-26
- **Amends:** [0017](0017-reader-wake-ups.md) and [0021](0021-notify-shared-memory.md) (the wake-up region gets three
  more words; nothing in it changes meaning). Issue #82.

**In short.** A durable-only reader that waits for a checkpoint is woken today by every append, although only a
checkpoint can give it a record. Since 1.0.0 the reader got faster at checking the marker, so it now sleeps between
appends, and the writer pays a wake-up system call for almost every record. This record adds a second event count to
the wake-up region that only checkpoints bump, and a capability word that says which writers bump it. A durable-only
reader at the checkpoint boundary waits on it if the writer has the capability, and on the old count otherwise. The
`notify` record, the region's existing words, appends and visible readers do not change, and every pairing of 1.0.0 and
new processes keeps working.

## Context

### The problem

Found by an interleaved comparison of `v1.0.0` and `main` (`dfef8cd`) on quiet Google Cloud VMs, `c4a-standard-8-lssd`
(Neoverse-V2) and `c4-standard-16-lssd` (Xeon 6985P-C), in us-east1-d: `tt-sustained`, 1,024 B records, background
checkpoints every second, readers `mapped`, `safe:wait=default` and `safe:durable:wait=block` on their own CPUs, 10
rounds per build in rotating order. "1.0.0" is the 1.0.0 library under the newer benchmark harness (`0fcdb9e5`, whose
library is identical to the tag's); a second copy of it gives the noise floor. Append latency, median of 10 runs, µs:

| Machine, rate | 1.0.0 p50 / p99 / p99.9 | its copy | `main` p50 / p99 / p99.9 |
|---|---|---|---|
| aarch64, 169,206/s | 0.54 / 2.66 / 7.35 | 0.54 / 2.64 / 7.30 | 0.75 / 4.01 / 8.92 |
| aarch64, 100,000/s | 0.61 / 2.82 / 8.38 | 0.61 / 2.82 / 8.21 | 2.02 / 4.24 / 10.0 |
| x86-64, 173,119/s | 0.56 / 2.10 / 5.96 | 0.56 / 2.07 / 5.92 | 1.20 / 3.75 / 7.10 |
| x86-64, 100,000/s | 0.62 / 2.60 / 10.5 | 0.62 / 3.23 / 11.0 | 1.16 / 3.76 / 11.7 |

p50 and p99 were slower in 10 of 10 rounds in every cell; the maximum did not change. Without the durable-only reader
(same load, the other two readers) there is no difference, and neither the append micro-benchmark nor `tt-latency` at
the README's workloads (64 and 1,024 B at 100,000/s, writer and reader pinned, either reader) shows one. The generated
code of `append`, its CRC and its wake-up step is instruction for instruction the same in both (aarch64 disassembly).

**Cause.** #66 (`eba5fa12`) made `inspect::read_checkpoint` read the marker's two 64-byte slots instead of the whole
128 KiB file. A durable-only reader at the checkpoint boundary reads the marker each time it is woken. Bisect: no commit
between 1.0.0 and `eba5fa12` changes the library, and `eba5fa12` is as slow as `main` (aarch64 0.75 / 4.04 / 8.87). Two
controls, both architectures: `main` with the 1.0.0 marker read is as fast as 1.0.0 (aarch64 0.54 / 2.64 / 7.34), and the
1.0.0 library with only the new marker read is as slow as `main` (0.75 / 4.03 / 8.83).

**Mechanism.** With the slow read, checking the marker took longer than the time between two appends, so the reader
found the event count already moved when it went to sleep and hardly ever slept: 4,250 voluntary sleeps in an 18-second
run, 99 % of a core. Now it sleeps after almost every append (994,000 sleeps, 82 % of a core), so each append finds a
sleeping waiter and makes a `FUTEX_WAKE` that has a thread to wake. Counted with a tracepoint histogram in one run each
(1.69 million appends on aarch64): 843,000 wake calls by the writer with 1.0.0, 1,499,000 with `main`; on x86-64 866,000
and 1,606,000 of 1.73 million. The writer thread's system time went from 0.69 to 2.48 s.

**1.0.0 pays part of it already.** Its appends with that reader cost 0.54 µs at p50, against 0.31 µs without it (the
same machine, the other two readers): half the appends make a wake call for a reader that is registered but not asleep,
and at lower rates, where the slow read fits between appends, the reader sleeps and 1.0.0 behaves as `main` does now.
A durable-only reader needs only checkpoints, so every one of these calls is wasted.

### The wake-up region as it is

ADR 0017 chose the protocol, ADR 0021 where it lives; `mmap.rs` (`Notify`, `NotifyPublisher`) and `reader.rs`
(`block`, `block_on`) implement it. `docs/format.md` describes only the `notify` record that names the region.

**The record.** `<dir>/notify`, 80 bytes, version 1: magic `TTNOTIFY`, version, kind (memfd or POSIX shared memory),
nonce, writer pid and descriptor or object name, the journal directory's device and inode, CRC32C. A reader decodes it
only if it is exactly 80 bytes with version 1 and a valid CRC; anything else means "poll".

**The region.** 4,096 bytes of shared memory that cannot shrink (a sealed memfd on Linux, a POSIX shared-memory object
on macOS), created zero-filled by the writer at every open, with a fresh nonce, and never reused by another writer:

| Offset | `u32` word | Written by | Read by |
|---|---|---|---|
| 0 | event count | writer (increment) | readers (load; the futex word) |
| 64 | waiter count | readers (increment, decrement) | writer (load) |
| 128 | writer flag: 1 while the writer maintains the count | writer (1 at creation, 0 when dropped) | readers |
| 192, 200 | nonce, low and high half | writer, at creation | readers, at attach |
| all others | zero | nobody | nobody |

**Who bumps the count.** The writer, after each committed record (`append`; once per batch for `append_batch`), after
the seal of a roll, and the checkpoint thread after each successful checkpoint, once the marker is durable (ADR 0038).
When dropped, the writer stores 0 in the flag and bumps once more. Each bump is `event.fetch_add(1, SeqCst)`, then
`waiters.load(SeqCst)`, and if that is non-zero, `FUTEX_WAKE` of all waiters on the event word (Linux, shared futex) or
`os_sync_wake_by_address_all` (macOS 14.4+).

**How a reader waits** (`Reader::next_timeout`, after its spin and yield phases): `waiters.fetch_add(1, SeqCst)`,
`seen = event.load(SeqCst)`, then one more look for a record (for a durable-only reader at the boundary, that look
reads the marker), and only if there is still nothing, a wait on the event word for the value `seen`, at most 100 ms if
the writer flag is set and at most `Backoff::max_sleep` otherwise; then a decrement of the waiter count that never goes
below zero. It re-reads the record at most every 100 ms and attaches to a new region when the nonce changes.

**Why no wake-up is lost.** The four accesses are sequentially consistent, so they have one order. If the writer's
increment comes before the reader's load of `seen`, the reader's look happens after the record or marker it is waiting
for (the load reads the incremented value and synchronizes with it). If it comes after, either the reader's futex call
finds the word changed and returns at once, or the reader's increment of the waiter count comes before the writer's
load of it, so the writer wakes it. The 100 ms cap bounds the damage of anything outside the protocol (a writer killed
before it cleared the flag, a lost record).

**What each side assumes.** A reader assumes that the writer bumps the count after every change it can wait for, and
clears the flag when it stops; a writer assumes nothing about readers, except that a raised waiter count may be stale
(a reader killed while waiting): it then makes a wake call per bump, which costs time, never correctness (ADR 0017).
**1.0.0 and unknown fields:** a 1.0.0 reader maps the whole 4,096 bytes, reads only the words at 0, 64, 128, 192 and
200, and never writes anything but the waiter count; a 1.0.0 writer creates the region zero-filled and writes only its
words. Neither looks at offsets 256 to 4,095.

## Options considered

1. **Do nothing and document it.** The cost falls on the writer, whose tail is what the project is used for, and it is
   avoidable. Rejected.
2. **Revert #66's marker read.** Restores 1.0.0's numbers by making the reader slow again: a durable-only reader burns
   a core polling the marker and seeks read 128 KiB for 128 bytes. And 1.0.0's numbers include the wasted wake calls.
   Rejected (owner's decision on #82).
3. **A waiter count of its own for durable-only readers, on the same event word.** The writer would skip the wake call
   when only durable-only readers wait, and a checkpoint would wake both counts' waiters. But the futex compares the
   event word, which every append moves: whenever appends come faster than the reader's check of the marker (2–3 µs
   now), the reader's wait returns at once, and it polls at 100 % of a core, as with 1.0.0. Rejected.
4. **Bump the count only when a waiter is registered** (a fence and a load instead of the unconditional increment).
   Changes the ordering of every append for every reader, needs its own model and measurements, and does not remove a
   single wake call for the durable-only reader: it is registered. A separate question; not decided here.
5. **A second region, or a second record naming one.** The `notify` record is exactly 80 bytes at version 1: 1.0.0
   readers reject any other length or version, so a changed record would take wake-ups away from every 1.0.0 reader.
   A second file and region double the lifecycle (creation, removal of a crashed writer's macOS object, the nonce
   check) for three words. Rejected.
6. **Linux `FUTEX_WAKE_BITSET`, durable-only readers in their own bit.** Linux only (macOS has no equivalent), and the
   futex word is still the event count that every append moves: as option 3. Rejected.
7. **Let a durable-only reader at the boundary sleep instead of blocking** (`Backoff::max_sleep`, 1 ms by default).
   Up to a millisecond more before a durable record arrives, and an idle reader waking 1,000 times a second. Rejected.
8. **A second event count and waiter count bumped only by checkpoints, and a capability word** (chosen).

## Decision

### The region

Three more `u32` words, each on a 64-byte line of its own, and together on a 128-byte line pair apart from the
existing words (cache lines are 128 bytes on Apple's cores, and adjacent-line prefetch pairs them elsewhere):

| Offset | `u32` word | Written by | Read by |
|---|---|---|---|
| 256 | marker event count | writer's checkpoint thread (increment) | durable-only readers (load; the futex word) |
| 320 | marker waiter count | durable-only readers (increment, decrement) | checkpoint thread (load) |
| 384 | capabilities; bit 0: the writer bumps the marker words | writer, at creation, before the writer flag | readers |

Everything else stays: the words at 0 to 200 keep their meaning, the rest of the region stays zero, the region is still
created fresh at every open, and the capability word is never cleared (the writer flag says whether anyone still
maintains either count). The `notify` record does not change at all: same 80 bytes, version 1.

### Who bumps and who waits

- **Appends and seals** bump the event count at 0, as now. Nothing else; the append path is unchanged.
- **A checkpoint**, on the checkpoint thread, once the marker is durable: `marker_event.fetch_add(1, SeqCst)`,
  `marker_waiters.load(SeqCst)`, a wake of all waiters on the marker event word if that is non-zero; then the existing
  bump of the event count at 0 (a 1.0.0 durable-only reader waits there).
- **When dropped**, the writer stores 0 in the writer flag, then bumps the marker words, then the event count.
- **A reader** that is about to block chooses the pair before it registers: the marker pair if it is durable-only, its
  cursor is at the checkpoint boundary it knows (`next_seq >= durable_next_seq`) and the capability bit is set; the
  event pair otherwise. It registers, loads that pair's event count, and looks once more, as now. If it chose the
  marker pair and the look moved it off the boundary (a new marker), it returns without waiting and chooses again.
  The wait caps are unchanged: 100 ms with the writer flag set, `Backoff::max_sleep` without.

**Orderings.** Each pair is the protocol above, unchanged: the four accesses of one pair are sequentially consistent.
The marker bytes are written with `pwrite` and made durable before the checkpoint thread's increment, and a reader reads
them with `pread` after its load of `seen`; when that load returns the incremented value it synchronizes with the
increment, so the read sees the marker. The two pairs need no order between them: no reader waits on both at once.

**Which readers wait on which count.**

| Reader, state | Waits on | Why |
|---|---|---|
| Visible reader blocked between records | event count (0) | Every append can give it a record. Unchanged. |
| Durable-only reader at the boundary, writer with the capability | marker count (256) | Only a checkpoint moves the marker. |
| Durable-only reader at the boundary, writer without it (1.0.0) | event count (0) | That writer bumps nothing else. As 1.0.0. |
| Durable-only reader inside covered history (a slot it reads again after a failed validation, an empty slot under ADR 0050) | event count (0) | Rare and bounded by those rules' windows; as now. |

No reader needs both: a roll does not move the marker (rolls do not sync, ADR 0038), so a durable-only reader at the
boundary has nothing to gain from it. `try_next` and `records()` never block; every blocking path goes through
`next_timeout`: the CLI's `tail --durable-only` and `age --durable-only --wait block`, and the C, C++, Python and Java
bindings, so they change with the library and need nothing of their own.

**When the marker is lost or set back.** The reader reads the marker after every wake-up and timeout, as now. A marker
that a damaged or restored file set back lowers what the reader knows, and it waits at its cursor (it returns nothing
it has not seen covered, as now); the next checkpoint bumps the marker count and wakes it. A marker that someone deletes
bumps nothing: the reader notices at its next timeout, within 100 ms, and has nothing to return either way. A writer
that reopens the journal creates a new region; the reader attaches to it within 100 ms (its record re-read), and the
new writer's checkpoints bump the new region's marker words. A removed or replaced segment is noticed within about
100 ms, by the throttled identity check that runs when the wait times out; with 1.0.0 and a busy writer it ran sooner,
woken by the appends; 100 ms is the bound that `leave_superseded_segment` documents and that an idle writer already gave.

### Mixed versions

The region is shared by processes of different versions. What each pairing does, checked with a small program built
once against `v1.0.0` and once against the prototype (`exp/append-marker-wake`), one process per role, on a
`c4-standard-16-lssd` (x86-64): "ping" appends one record, checkpoints it and sleeps 23–63 ms, 60 times (a lost wake-up
would show as a delivery of about 100 ms); "steady" appends 500,000 records of 1,024 B at 100,000/s with a background
checkpoint every second. Delivery is the record's age when the reader returned it; the ping checkpoints took 0.30–0.65
ms at p50 and up to 2.9 ms.

| Writer, reader | What happens | Measured |
|---|---|---|
| 1.0.0, 1.0.0 durable-only | As today: the reader waits on the event count. | ping 60/60, p50 0.50 ms, max 2.1 ms; steady: writer p50 0.43 µs, reader 136,249 sleeps |
| 1.0.0, new durable-only | Capability bit 0, so the event count, as a 1.0.0 reader. **Costs the writer more than a 1.0.0 reader does**: the new reader's marker read is fast (#66), so it sleeps between appends. | ping 60/60, p50 0.82 ms; steady: writer p50 0.98 µs, 435,623 sleeps |
| new, 1.0.0 durable-only | The 1.0.0 reader waits on the event count, which checkpoints still bump. As 1.0.0. | ping 60/60, p50 0.50 ms, max 1.9 ms; steady: writer p50 0.43 µs, 150,244 sleeps |
| new, new durable-only | The marker count. The appends make no wake call for it. | ping 60/60, p50 0.48 ms, max 2.4 ms; steady: writer p50 0.14 µs, 61 sleeps, 0.13 s of CPU in 5 s |
| either, a new durable-only reader beside a 1.0.0 visible one | Independent pairs: each is woken by what it needs. The visible reader costs what it costs today. | 60/60 each; steady: writer p50 1.61 µs (1.0.0 writer) and 1.14 µs (new), durable reader 432,406 and 60 sleeps |
| either, visible readers of either version | Unchanged. | steady: writer p50 1.20–1.21 µs in all four pairings |
| A region created by 1.0.0, attached by a new reader | Capability word zero: the event count. | as the second row |
| A region created by the new code, attached by 1.0.0 | 1.0.0 ignores offsets 256 to 447 and maps 4,096 bytes as before. | as the third row |
| Readers of both versions attached while the writer changes from 1.0.0 to new and back | Each new writer creates a new region; readers follow within the 100 ms record re-read; nothing is lost or repeated. | 120 of 120 records for each of four readers (both versions, durable-only and visible), both orders; durable max 2.9 ms |
| A durable-only reader killed while asleep | A 1.0.0 one leaves the event waiter count raised: a wake call per append (ADR 0017's documented cost). A new one leaves the marker waiter count raised: a wake call per checkpoint. | writer p50 0.28 against 0.14 µs |

The same matrix on a `c4a-standard-8-lssd` (aarch64) gave the same results: every ping and restart run complete, steady
writer p50 0.59 µs (both 1.0.0), 2.03 µs (1.0.0 writer, new durable-only reader), 0.59 µs (new writer, 1.0.0 reader) and
0.26 µs (both new, 60 sleeps); a killed 1.0.0 durable-only reader 0.51 µs, a killed new one 0.27 µs.

No pairing loses a wake-up or leaves a reader spinning; the worst delivery in every ping run is the checkpoint's own
time. The cost goes away only when both the writer and the durable-only reader are new: **upgrade the writer first**
(the second row is the one pairing that is worse than all-1.0.0).

**Compatibility (`docs/compatibility.md`, ADR 0043).** No documented behaviour of 1.0.0 changes. The region is not a
stable surface and not part of format version 1; the `notify` record, which `format.md` does describe, is byte for byte
the same, so neither `format_version` nor the record's version changes. What does change, and is documented when it is
built: ADR 0021's region table ("all others zero") gains the three words; `format.md`'s row for `notify` points to this
record too; `WriterConfig::notify` and the CLI's `age` help, which say that a waiting reader costs a wake call per
append, add that a durable-only reader waiting for a checkpoint costs one per checkpoint from 1.1.

## The prototype's two costs

Measured again with the prototype rebased onto `main` (`f1ea454d`, `de93b141`), 1.0.0 and `main` interleaved on new VMs
of the same shapes in the same zone, 10 rounds, with one line logged per checkpoint by the checkpoint thread (when it
starts the wake-ups) and by the durable-only reader (when it first sees the new marker, and when it has read up to it),
and every append over 120 µs that is not a roll.

**A higher append maximum on x86-64 (first session: 8 of 10 rounds, median 230 against 159 µs). Not reproduced.**
Second session, 173,119/s: maximum median 156 µs for the prototype against 158 µs for 1.0.0 (0 of 10 runs above 200 µs
for either); the prototype on its original base (`dfef8cd` plus the first patch) 160 µs (2 of 10 above 200 µs), 1.0.0
159 µs (1 of 10). Of the 294 non-roll appends over 120 µs in these 90 runs, 287 fall 990–1,003 ms after the previous
checkpoint started its wake-ups, that is, at the start of the next checkpoint, in every build; the rare ones over 200 µs
(up to 617 µs) appear with every build in 0–2 runs of 10. With the helper threads moved to a CPU of their own (`--helper-cpus 7`) they appear more often, with 1.0.0 (2 of
10 runs) and the prototype (5 of 10) alike. So the stall belongs to the checkpoint's start (its `msync` write-protecting
the pages the writer is about to store into is the likely source; not traced), not to this change. Keeping the
reader's CPU busy with an idle-priority spinner did not change the maximum either (median 154 µs).

**Records reach the durable-only reader later (first session: about 2 ms on aarch64, 0.4 ms on x86-64). Explained:
the woken reader waits for its CPU.** On aarch64 (169,206/s) the reader's first look at a new marker comes 373 µs (p50
of the runs' medians; at most 694 µs) after the checkpoint thread starts its wake-ups, where 1.0.0's polling reader sees
the marker as soon as it is written (11 µs before that point); reading up to the marker then takes 43.4 against 41.6 ms,
and the first record of a checkpoint arrives 1.9 ms later (43.65 against 41.75 ms). With the writer's helper threads
(checkpoint, prefault, write-back) on a CPU of their own (`--helper-cpus 7`), the first look comes after 10 µs (at most
15 µs) and the first record arrives when it does with 1.0.0 under the same placement (42.09 against 42.06 ms). So the
delay is not the wake-up: a sleeping reader leaves its CPU idle, the scheduler puts the writer's helper threads there
(1.0.0's reader never sleeps, so its CPU is never free), and when the checkpoint wakes the reader it waits until that
thread gives the CPU up, then shares it while it reads the backlog. The VMs' kernel is built with `PREEMPT_LAZY` and a
1,000 Hz tick, under which a woken thread can wait for the next tick, up to 1 ms, before it preempts another ordinary
thread; that fits the maxima (an idle-priority spinner on the reader's CPU, which the woken reader should preempt at
once, gave a first look after 788 µs, at most 1.0 ms), but the preemption mode was not recorded, so this part is
inferred. On x86-64 (16 vCPUs, more of them idle) the first look comes after 20 µs (at most 93 µs), and the first
record arrives 0.43 ms later than with 1.0.0 (42.83 against 42.40 ms), of which 0.2 ms is a slower read of the backlog.
No checkpoint in any run was noticed more than 1.0 ms after its wake-up, so none waited for the 100 ms timeout: no
wake-up was lost. The delay grows with what there is to read: on aarch64 0.10 ms with a checkpoint every 100 ms and
0.01 ms every 20 ms, on x86-64 0.13 and 0.05 ms (five runs each).

**Acceptable, and documented.** A durable-only reader gets records once per checkpoint interval and, on average, half an
interval after they were appended (522–524 ms here). A delay of 0.4 to 2 ms on a 1-second interval is 0.04–0.2 % of it.
An application that wants it back keeps the reader's CPU to the reader (the writer's helpers on other CPUs with
`WriterConfig::helper_cpus`, or the reader's CPU in `isolcpus`; `docs/operations.md` → Pinned writers), or uses a spinning durable-only reader
(`Backoff::busy_spin`), which never blocks and is not affected by this change.

## Test plan

- **Model.** A `loom` model of both event counts (`notify_model.rs` on the prototype branch; futexes modelled with a
  mutex and condition variable, so a lost wake-up is a deadlock that loom reports): a checkpoint wakes a durable-only
  reader on the marker words while an append races and never sees it as a waiter; a checkpoint wakes a 1.0.0 reader on
  the event count; a visible and a durable-only reader waiting at once each get their wake-up. Two variants must fail:
  without the single order of the `SeqCst` accesses, and a reader on the marker words of a writer that does not bump
  them (the case the capability bit prevents). All five pass on the prototype.
- **Unit and process tests.** The existing notify tests (ping-pong, deadlines, a reader started before the writer,
  truncated record) for both pairs; a test that the event waiter count stays 0 while a durable-only reader waits at the
  boundary under appends; a test that a region with the capability word 0 makes the reader use the event count; the
  drop wakes a reader on either pair.
- **Mixed versions.** The matrix above, as a script in `tests/compat/` that builds `v1.0.0` from the tag next to the
  current tree (as `generate.sh --check-old-readers` does) and runs the ping and restart cases with every pairing; ping
  deliveries must stay under 50 ms (a lost wake-up shows as about 100 ms). Linux and macOS.
- **Acceptance measurement.** The table at the top again with the implementation, on the same shapes, 10 interleaved
  rounds against 1.0.0 and a copy of it: append p50 / p99 / p99.9 / maximum at both rates and both architectures with
  the durable-only reader blocking; the same load without it, which must be unchanged; `tt-latency` at the README's
  workloads, unchanged; the durable-only reader's CPU and voluntary context switches; and the delivery delay of durable
  records against 1.0.0.

## Consequences

- Appends do not wake durable-only readers once writer and reader are both from a release with this change; the
  prototype's appends at 169,206/s on aarch64 cost 0.31 µs at p50 with the durable-only reader blocking, against 0.54
  µs with 1.0.0, and the reader uses 4 % of a core instead of 99 %.
- Durable records arrive 0.4–2 ms later on a 1-second checkpoint interval when the reader's CPU is shared with other
  threads (above); documented with `ReaderConfig::durable_only` and the operations guide.
- A new durable-only reader with a 1.0.0 writer costs that writer more than a 1.0.0 reader did; the upgrade note says to
  upgrade the writer first.
- `mmap.rs` changes (new words, a second wait target): it needs the cross-provider review its `unsafe` code requires.
- Wake calls for a durable-only reader that is registered but not yet asleep go with the rest: it registers on the
  marker words, which only a checkpoint looks at. Not addressed: the same calls for visible readers (`docs/benchmarks.md` → profiles,
  up to about a fifth of them). Registering after the last look instead of before it, just before the futex call, would
  remove most of them, relying on the futex call's full barrier; that changes the protocol for every reader and needs its
  own record and model.
