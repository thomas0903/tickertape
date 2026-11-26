# 0058: The page faults the writer still takes with prefaulting on, and what to change for them

- **Status:** proposed
- **Date:** 2026-11-26
- **Relates to:** issue #60; ADRs [0020](0020-background-prefault.md), [0025](0025-linux-prefault-small-window.md),
  [0038](0038-background-checkpoints.md) (early write-back), [0049](0049-linux-spare-segment-option.md); measured in
  `docs/benchmarks.md` (2026-11-26, the writer's remaining page faults with prefaulting on); the checkpoint side in the
  same file (2026-11-26, what prefaulted, unwritten pages add to a checkpoint, PR #102)

**In short.** The owner asked whether Chronicle Queue's `microTouch()`, which takes the write fault of the next 4 KiB
page on the waiting writer's thread, shows a way to make tickertape's prefaulting better. Not in the form asked:
tickertape's helper thread already removes what `microTouch()` removes for Chronicle (a writer without any pre-touching;
in the comparison rerun, `microTouch()` took Chronicle's p99.9 at 1,024 B × 10,000/s from 35–38 to 2.4–2.9 µs), and a
touch on tickertape's own writer thread changed nothing measurable. But at 1,024 B the rerun has tickertape's p99.9 2–14
µs above Chronicle with `microTouch()`, and the faults that remain with the helper on explain those cells. They are of
another kind: pages the helper made writable that a **write-back made read-only again**. About nine in ten come from
early write-back, which writes back whole page-cache folios (128 KiB on the measured kernel), including the one the
writer is filling; most of the rest from checkpoints, which also write back the 256 KiB the helper prefaulted ahead (PR
#102); a few dozen at each roll. Each costs 1–3 µs, but enough of them take 20–100 µs (a file-time update through the
filesystem journal) to set the writer's p99.9. **Stopping early write-back at the 128 KiB boundary below the writer**
removed nine in ten of the faults and cut the writer's p99.9 by 3–10 times at 1,024 B on both architectures and at 64 B
on aarch64 (aarch64: 33 → 3.2 µs at 64 B × 1,000,000/s, 18 → 2.7 µs at 1,024 B × 100,000/s; x86-64: 26 → 4.5–7.5 µs at
1,024 B × 100,000/s), with no cost found in p99.99, maxima, the reader or the checkpoint's durable time. The same
rewrite of the writer's folio is also **the 7 % of extra device bytes** that PR #105 found at steady-90: with the
boundary the device wrote 0.3 % more than appended instead of 8.6 %, and the 50–140 ms stalls of aarch64 at steady-90
(25–30 seconds per run with an append of 10 ms or more) were gone (none), with checkpoints durable after 1 ms instead of
0.3–0.4 s (early write-back off gives the same tail, at 0.9 s per checkpoint). The **ranged sync of the active segment**
(PR #102's experiment) saves the checkpoint's 256 KiB write and the faults after it; alone it moved the writer's latency
little; together with the boundary it cut the remaining faults to a third and halved the reader's p99.9 at 64 B on
aarch64. This ADR proposes both, each still off by default until the measurements listed below exist. No change to the
helper itself (window, advice, re-touching) and no writer-thread touch is proposed.

## Context

On Linux, with `WriterConfig::prefault(true)` (the default), the `tickertape-prefault` thread keeps 256 KiB ahead of the
write position populated with `MADV_POPULATE_WRITE` (ADR 0025), waking every 64 KiB. The early write-back thread
(`WriterConfig::early_writeback`, on by default) starts write-back of the whole pages behind the writer every 1 MiB
(`sync_file_range(SYNC_FILE_RANGE_WRITE)`), so that checkpoints find little left to write. Chronicle Queue's
`microTouch()` (`compare/README.md` → "No Chronicle pretoucher") does a compare-and-swap of 0 to 0 on the next page,
once per page, from the writer's busy-wait: it moves one write fault per page out of the next record, if the writer
waits.

PR #102 found that a checkpoint writes the 256 KiB window back as zeros and write-protects it, so that the writer then
takes 33–36 more faults per checkpoint. It did not look at early write-back, at where the writer's faults land relative
to its appends, or at the latency they explain.

## What was measured

Google Cloud VMs with local NVMe SSD (x86-64 `c4-standard-8-lssd`, aarch64 `c4a-standard-8-lssd`), kernel
7.0.0-1017-gcp, ext4 and xfs. A new tool, `tt-faults` (benches), appends at a fixed rate with a background checkpoint
every second and a zero-copy reader, reads the writer thread's fault count before and after each append
(`getrusage(RUSAGE_THREAD)`, outside the timed part), and logs every append that faulted or took 20 µs or more. A traced
round (`benches/diagnostics/writer-faults.bt`) classified every fault of the writer's thread: not present (the page was
not mapped) or write-protected (mapped, not writable), what it started (a journal handle, an xfs transaction) and what
had written the page back before it. Four cells: 64 B at 1,000,000 records/s, and 1,024 B at 100,000, 170,000 and
300,000 records/s (the last near what the device sustains); runs of 20 s after 3 s of warm-up; configurations
interleaved; the default twice per round as the noise floor. Experiments ran from switches that are not in `main`: they
are on the branch `exp/prefault-residual`, never merged ("Where the experiment code is", below). Figures and method are
in `docs/benchmarks.md`.

1. **Where the faults are** (default configuration, x86-64 ext4; xfs the same; aarch64 below). Faults on the writer's
   thread per MiB written: 8.1–8.4 at 64 B, 9.3–9.6 at 1,024 B × 100,000/s, 2.0–2.2 at 170,000/s, 0.55–0.62 at 300,000/s
   (with prefaulting off: 9.6–23). Of these, at the two lower rates 89–90 % were write-protect faults after early
   write-back, 8–9 % after a checkpoint, 1–2 % not-present faults in the first 256 KiB of a new segment (the writer
   starts writing before the helper has reached the new segment); at 300,000/s half were after checkpoints and half at
   rolls, because the writer has left the folio by the time early write-back cleans it. Almost none were beyond the
   window: the helper kept ahead at every rate measured, also near the device's limit. The page cache built 128 KiB
   folios (order 5) for the segments, and write-back works per folio: writing back the pages behind the writer also
   writes back, and write-protects, the rest of the folio the writer is in. On aarch64 ext4 the writer took more:
   12.3–12.6 per MiB at 64 B, 18.0–18.5 at 1,024 B × 100,000/s, 14.0–14.7 at 170,000/s and 6.5–7.7 at 300,000/s, 93–95 %
   of them after early write-back at the three lower rates (and 41–91 % at 300,000/s: there the writer is often still in
   the folio when it is cleaned).
2. **What they cost.** Write-protect faults after early write-back took 1.1 µs (median; aarch64 2.4–3.0) and 18–40 µs
   (aarch64 13–25) at the 99th percentile; those after a checkpoint 1.0–1.1 and 76–102 µs; not-present faults at a roll
   2.2–2.7 and 74–82 µs. About one in eight of the first kind started a journal handle (ext4 updating the file's
   modification time). Counting appends by the first fault of their episode (the appends delayed back to back by one
   stall), in the traced runs: at 1,024 B × 100,000/s, 53–61 % of the appends of 20 µs or more began at an early
   write-back fault, 12–22 % after a checkpoint and 14–16 % at a roll; of those of 100 µs or more, 2–4 % began at an
   early write-back fault and the rest after checkpoints and at rolls (and, in one run, in one stall that began with a
   fault outside a roll). At 300,000/s the appends of 100 µs or more were 82 % in roll episodes and 17–18 % after
   checkpoints. Appends in episodes without any writer fault were at most 3 % of those of 20 µs or more at 1,024 B, and
   5–11 % at 64 B (aarch64: none at 64 B, at most 0.3 % at 1,024 B). So the faults, the roll and the checkpoint are the
   tail; the early write-back faults set the p99.9 (26 µs on x86-64 at 1,024 B × 100,000/s; on aarch64 they began 68–90
   % of the appends of 20 µs or more at the three lower rates) and the other two the p99.99.
3. **The alternatives** (each interleaved with the default; untraced runs; x86-64 ext4 unless noted):
   - **A touch on the writer's thread**, as `microTouch()` does: while the writer waits (a `Writer::idle()`; experiment:
     `Writer::exp_touch`, once per wait, the page of the write position and the next one) it found a fault to take in
     few waits (193 and 53 faults in 20 s at 1,024 B × 100,000 and 170,000/s, against 18,400 and 6,900 faulted appends),
     and no latency figure moved outside the default's range, apart from single stalls of 1.7 and 3.0 ms in one run each
     (the default's runs have stalls of up to 5.3 ms). On aarch64 it took most of the faults off the appends (1.6–3.1
     per MiB left in them, against 12.5–18.5), and the latency did not move either: a touch waits as long as the fault
     would, and the send after it then starts late. Inside `append` (when the writer enters a page, touch the next one)
     it cost 16.4 instructions per append on x86-64 and 12.7 on aarch64 (ADR 0047's gate is 3), was slower in every
     timed pair, and changed nothing else: it only moves a fault from one append to the one before.
   - **A larger window** (1, 4, 16 MiB): worse in every cell. The checkpoint writes the whole window back and
     write-protects it, so the faults grow with the window (10–20 per MiB with 4 MiB, 34–35 with 16 MiB) and so does the
     tail (p99.9 0.6–0.9 ms at 170,000/s against 17–47 µs). A rate-adaptive window would only pick between these.
   - **`MADV_POPULATE_WRITE`** is what the helper already uses. It does not convert unwritten extents: on ext4 and xfs
     that happens when the pages are written back, on the write-back's completion, not at the fault. The faults that
     wait are those that update the file's times through the journal, not conversions.
   - **Re-touching after write-back** (experiment: at every wake-up the helper prefaults again from the writer's page):
     the faults fell to a third (1,024 B) or a sixth (64 B), but the ones left now collided with the helper on the same
     folio (median 4–32 µs instead of 1.1), and the writer's p99.9 was no better (slightly above the default's range at
     64 B and at 1,024 B × 100,000/s).
   - **Early write-back further behind.** 2–4 MiB behind: no early write-back faults, but the checkpoint then writes
     those megabytes and the writer's next fault waits for its journal commit: writer p99.9 21 µs to 0.5 ms (against
     17–100 µs), reader p99.9 0.8–1.9 ms (against 40–785 µs), and `tt-sustained`'s checkpoints durable after 12–13 ms
     instead of 1.2–2.8 ms. Without early write-back, as known, checkpoints take tens to hundreds of milliseconds. **To
     the 128 KiB boundary below the writer** (at most 128 KiB more for the checkpoint): faults 0.7–1.0 per MiB instead
     of 2.1–9.7; writer p99.9 at 1,024 B 4.5–7.5 µs against 25.5–25.8 (100,000/s) and 6.9–9.0 against 16.6–18.5
     (170,000/s), p99 at 64 B 1.6–1.8 against 3.3–3.5; p99.99, maxima and the reader within the default's range, except
     one run of three at 100,000/s with a 3.3 ms append after a checkpoint (the default runs have such outliers too). On
     aarch64 ext4 (two untraced runs each, against four of the default): faults 0.55–0.85 per MiB instead of 6.5–18.5;
     writer p99.9 3.2 against 32.5–34.0 µs (64 B), 2.7–2.8 against 18.1–18.7 (100,000/s), 2.8 against 21.8–22.3
     (170,000/s), 12.5–12.6 µs against 250 µs to 18 ms (300,000/s); the reader's p99.9 10–11 against 39–41 µs (64 B) and
     3.5–4.3 against 25–26 (1,024 B × 100,000/s); p99.99 and maxima within or below the default's range. xfs on aarch64
     the same (one run each). In the comparison's W2 cells with `tt-latency` (aarch64, three runs each, no checkpoints),
     the zero-copy reader's p99.9 fell from 8.8–9.4 to 2.3–2.8 µs at 1,024 B × 10,000/s and from 25–27 to 3.2–4.6 µs at
     100,000/s, below Chronicle with `microTouch()` in the rerun (2.7–2.9 and 18.7–29.2; all settings on 2.8–3.5 and
     14.5–16.3). `tt-sustained` at 170,000/s (aarch64): append p99.99 4.4–6.2 against 29.5–30.1 µs, checkpoints durable
     after the same 0.88–0.92 ms. To the 2 MiB boundary (x86-64) the checkpoint again had too much to write: reader
     p99.99 about 1.2 ms.
   - **The ranged sync of the active segment** (PR #102's experiment, here with appends paced and timed from the
     schedule): faults 7–28 % fewer, writer p99.9 41–57 µs against 63–72 (64 B), 22–23 against 25.5–25.8 and 9.9–10.7
     against 16.6–18.5 (1,024 B), the reader's p99.9 at 64 B 44–76 against 101–107 µs; p99.99 and maxima unchanged. On
     aarch64 alone it was within the default's range (one run at 64 B had a 12 ms stall, as one default run at 300,000/s
     had an 18 ms one); with the 128 KiB boundary it left 0.23–0.29 faults per MiB, and the reader's p99.9 at 64 B was
     4.5–5.2 µs against 10–11 with the boundary alone and the writer's at 300,000/s 5.3–6.3 against 12.5–12.6.
   - **The spare segment** (`spare_segment(true)`, an option since ADR 0049): it removes the roll's faults, and at
     300,000/s the writer's p99.9 fell from 86–172 to 8.1–9.2 µs on x86-64 ext4 and from 79–86 to 8.6 µs on xfs; but one
     of its runs at 170,000/s had an 8.8 ms stall. It is not part of this decision; ADR 0049 lists what must be measured
     before it can be the default, and these cells add to that.

4. **The extra device bytes at steady-90** (PR #105: 7 % more than appended on aarch64 with the defaults, none with
   early write-back off). Early write-back syncs `[end of the last range, start of the page that holds the position)`
   every 1 MiB with `SYNC_FILE_RANGE_WRITE`; nothing is rounded up in `writeback.rs`. The kernel writes whole folios, so
   the folio that holds the position is written partly empty and again once full: one 128 KiB folio per 1 MiB, half full
   on average, about 6 %; the checkpoint's 256 KiB window adds 0.1 % at one checkpoint a second. Measured with PR #105's
   command (aarch64, ext4, 1,024 B at 304,571 records/s, four runs each): device bytes 1.085–1.088 times the appended
   bytes with the defaults, 1.001 with early write-back off, 1.003 with the 128 KiB boundary, 1.001 with the boundary
   and the ranged sync, 1.085–1.087 with the ranged sync alone. Seconds with an append of 10 ms or more (of 71): 25–30,
   0, 0, 0–1, 26–29; append maximum 140–142 ms with the defaults and with the ranged sync alone, 0.2–1.8 ms with the
   boundary; checkpoints durable after (median) 0.32–0.42 s, 0.93 s, 1.0 ms, 0.6 ms, 0.18–0.28 s. On x86-64 (311,614
   records/s, three runs each) the defaults wrote only 0.2–0.4 % more: at that rate the writer has left the folio before
   early write-back reaches it, and the boundary changed nothing there. So the rewrite, and with it the extra bytes and
   the aarch64 stalls, depend on how far early write-back trails the writer, which depends on the machine; the boundary
   removes it wherever it occurs.

## Is the ranged sync safe?

On Linux, `msync(MS_SYNC)` on a shared file mapping calls `vfs_fsync_range(file, start, end, datasync = 1)` for the
range (Linux 7.0, `mm/msync.c`), which calls the filesystem's `fsync` with the range. Read in the Linux 7.0 sources:

- **ext4** (`ext4_sync_file`): writes back and waits for the dirty pages in the range only
  (`file_write_and_wait_range`), then commits the transaction that holds the inode's data-related metadata
  (`i_datasync_tid`: block allocation, the conversion of unwritten extents, the size; per inode, not per range), then,
  with barriers on (the default), sends a device cache flush unless that commit already sent one; then reports any
  write-back error recorded on the file since the descriptor last looked (`errseq`, for the whole file).
- **xfs** (`xfs_file_fsync`): the same range write-back and wait, then forces the log up to the inode's data-sync
  sequence (which flushes the cache), or, if there was nothing to force, flushes the data device's cache.

So against the whole-file `fdatasync`, the ranged call leaves out exactly the dirty pages outside the range, and nothing
else: the metadata needed to read the range back after a crash and the cache flush are the same. What the checkpoint
must make durable in the active segment is all inside the range: the records since the last checkpoint
(`active_dirty_from` to the write position), the segment header (in the range's first page for a new segment, which the
`msync` rounds down to), and nothing beyond the write position (a recovery's zeroing of an uncommitted tail syncs itself
at open; an abandoned record's zeroing syncs itself). The pages outside the range are the prefaulted zeros ahead of the
writer, which the format does not need on disk. The second whole-file `fdatasync` of the active segment (the barrier,
step 3) adds nothing on Linux either: the ranged call has already flushed the cache.

The VMs' disks report `write_cache` "write through", so the kernel sends them no flushes, and writing "write back" to
the local SSD's queue did not take effect. A loop device reports a write-back cache (it turns a flush into an `fsync` of
its backing file); with ext4 and xfs on one, the ranged checkpoint still sent the device 3.9–4.3 flushes per checkpoint
(the default 7.1–7.8; 11 checkpoints per run, two runs each), with one `msync` and one `fdatasync` (the marker's) per
checkpoint instead of one and about three. That shows flushes are still sent, not their order within a checkpoint; the
order follows from the sources above.

What it would change in `docs/crash-contract.md`: step 1 for Linux ("`msync(MS_SYNC)` on each dirty range of a segment
the writer still holds, then `fdatasync` on each dirty segment") would become "… then `fdatasync` on each dirty segment
other than the active one; the active segment's `msync(MS_SYNC)` is a ranged `fdatasync` and is not followed by one",
and step 3 would note that the barrier on Linux is the last of those calls (the active segment's `msync`), not a second
`fdatasync`. The guarantees (C1–C14) do not change; the claims that rest on step 1 and 3 ("Segments the writer no longer
holds", the `errseq` sentence) still hold, because a ranged call reports the file's write-back errors as a whole-file
one does.

Not covered by this reading: other filesystems (btrfs, f2fs, tmpfs, network filesystems), ext4 without a journal
(`ext4_fsync_nojournal`), and kernels older than the 7.0 sources read here (the library states no oldest supported
kernel; the helper needs 5.14). A filesystem whose `fsync` ignores the range is safe (it syncs more); one whose datasync
commit is per range would not be, and neither of the two read here is.

## Options considered

1. **A writer-thread touch (`Writer::idle()`, or in `append`).** Rejected: it found almost nothing to do with the helper
   on, changed no figure, and in `append` costs 16.4 instructions per append on x86-64 and 12.7 on aarch64 (four to five
   times ADR 0047's gate) to move a fault by one append. `microTouch()` helps Chronicle because Chronicle has no helper.
2. **A larger or rate-adaptive window.** Rejected: with today's checkpoint every byte of the window is written back and
   re-faulted. With the ranged sync it would no longer be written back at checkpoints, but early write-back never
   reaches it either; a larger window was not measured with the ranged sync, and nothing measured here says the 256 KiB
   window runs short (the helper kept ahead in every cell).
3. **Re-touching after write-back.** Rejected: fewer faults, slower ones, no change in latency; with the ranged sync and
   the 128 KiB boundary there is little left to re-touch.
4. **Early write-back to the 128 KiB boundary below the writer (proposed).** In `writeback.rs` → `follow`, the end of
   the range the helper may start becomes `position / 131_072 * 131_072` instead of `position / page * page` (the switch
   `TT_PF_WB_LAG_KIB=1 TT_PF_WB_ALIGN_KIB=128` measured exactly this). Cost: a checkpoint may find up to 128 KiB more to
   write. It does no harm whatever the folio size: with smaller folios (or none) it only lags; with larger ones (the
   page cache can build folios up to 2 MiB, for example with a larger `read_ahead_kb`) it behaves as today. The 2 MiB
   boundary, which would cover every folio size, made the checkpoints too long.
5. **The ranged sync of the active segment (proposed).** In `checkpoint.rs` → `steps`, on Linux, a segment whose mapping
   the checkpoint holds (`seg.flush`) and that is the active one gets its ranged `msync` and no whole-file `fdatasync`,
   and the barrier `fdatasync` of the active segment is skipped (the switch `TT_PF_RANGED` measured exactly this). macOS
   is unchanged (its `msync` is not a durability call). Cost: none measured; it saves the 256 KiB write and 0.16–0.43 ms
   per checkpoint (PR #102; 0.4 ms of `tt-sustained`'s durable time at 170,000/s on aarch64). It changes the crash
   contract's wording of steps 1 and 3, not its guarantees.
6. **No change.** The faults left are cheap on average; the case for options 4 and 5 is the tail at 1,024 B and the
   reader at 64 B, measured on one VM type per architecture.

## Decision

Proposed: options 4 and 5, each as a change of its own in a pull request of its own, built from the branch below and on
by default only after the measurements below. Option 4 is to be built and measured next; option 5 stays proposed until a
drive with a real volatile write cache has been tested. Neither touches the format, the API or the guarantees. They are
one story: both stop the library from writing back pages the writer has not finished (option 4 the rest of the folio it
is filling, option 5 the prefaulted window), which costs device bytes and, through write-protection, the writer's
faults. Option 4 is the one with the large effect (latency, the extra bytes, the aarch64 stalls at steady-90); option 5
saves the window's write at every checkpoint and changes the crash contract's wording, so it needs the volatile-cache
check first. If only one is taken, it should be option 4.

## What must be measured before either becomes the default

- **Option 4:** the published comparison cells (`benches/session.py`, sustained at 50 and 90 % of the probe, both
  payloads, 10 minutes) with and without, x86-64 and aarch64, ext4 and xfs; the blocking checkpoint's duration and the
  durable latency of background checkpoints (up to 128 KiB more each); unpaced (does a 128 KiB lag change the
  throttling?); a kernel or filesystem without large folios, where it must be neutral; and the reader at 64 B, where the
  gain in p99.9 was not shown.
- **Option 5:** the same cells; a drive with a real volatile write cache (bare metal or a cloud disk that reports "write
  back"), with the flushes counted per call (block `rq_issue` tracepoint with the thread: the active segment's `msync`
  must be followed by a flush before the marker's `pwrite`), and the kill and power-loss tests the crash contract lists
  for step 1 (C1–C3, C8) run with it; the sources of each filesystem the documentation names as supported, at the oldest
  kernel the project will state.
- **Both together** on x86-64 (only aarch64 ran them together).

## Where the experiment code is

The branch `exp/prefault-residual` (never merged; no tag). Each switch is compiled only with `--cfg tickertape_pf_exp`
(window, re-touch, early write-back lag and alignment, ranged sync, `Writer::exp_touch`) or, for the touch in `append`,
`--cfg tickertape_pf_exp --cfg tickertape_pf_touch_append`, and read from the environment once (`TT_PF_WINDOW_KIB`,
`TT_PF_RETOUCH`, `TT_PF_WB_LAG_KIB`, `TT_PF_WB_ALIGN_KIB`, `TT_PF_RANGED`). Its commits:

- `8e691d3b`: the source of the x86-64 matrix (42b2307e plus the switches without `TT_PF_WB_ALIGN_KIB` and
  `TT_PF_RANGED`; `git diff 42b2307e` of the tracked files hashes to the prefix `1c12916d5cc5` recorded on the VM).
- `7acd8f86`: with those two added; the source of the aarch64 runs, the x86-64 supplement, the W2 check and the
  steady-90 byte counts (prefix `b3513c036041`).
- `942bb7f7`: the same switches on `main` at `8a70d7ba`, renamed to this ADR and formatted, with this ADR's first
  documents. Option 4's change starts from here.

Each measured build, from the commit named:

```sh
git checkout <commit>
cargo build --release --locked -p tickertape-bench --bin tt-faults --bin tt-sustained --bin tt-latency --bin tt-append-cost
RUSTFLAGS="--cfg tickertape_pf_exp" CARGO_TARGET_DIR=target/exp cargo build --release --locked -p tickertape-bench …
RUSTFLAGS="--cfg tickertape_pf_exp --cfg tickertape_pf_touch_append" CARGO_TARGET_DIR=target/tapp cargo build …
```

The default configuration in every table is the first build, which compiles none of the switches.

## What was not measured

Bare metal; other VM shapes (4 or 16 vCPUs) and other disks; macOS (there the helper advises `MADV_WILLNEED` 8 MiB
ahead, which reads pages in but makes none writable, so the writer still takes a soft write fault per 16 KiB page, and
there is no early write-back; nothing here applies without measuring it); kernels other than 7.0; `lazytime`; unpaced
writers and bursts beyond the 300,000/s cell; helper threads sharing the writer's CPU (`HelperCpus::Inherited`: the
fault counts say the helper kept ahead with a CPU of its own); runs longer than 20 s (60 s at steady-90); the comparison
harness with these switches (`tt-latency` ran in the W2 cells on aarch64 only, `tt-sustained` at 170,000/s and at
steady-90).

## Consequences

- `tt-faults` and `benches/diagnostics/writer-faults.bt` are in `main`, for the measurements above; `tt-faults` there
  measures the library as built. The experiment switches are not in `main` and stay on the branch below.
- If option 4 or 5 is adopted, `docs/operations.md` (writer threads, early write-back) and, for 5, the crash contract
  change as described; the README's sentence on prefaulting at 1,024 bytes is out of date either way
  (`docs/benchmarks.md` has the proposed replacement).
