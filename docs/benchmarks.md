# Benchmarks

Measured results only, recorded following [`playbooks/run-benchmark.md`](playbooks/run-benchmark.md). Every number
below comes from the raw output of the commands listed with it. These numbers are not guarantees: they describe
specific machines on specific days. What each benchmark measures, and what it doesn't, is in
[`benches/README.md`](../benches/README.md) and, for the comparison with other tools, in
[`compare/README.md`](../compare/README.md). The methodology is in ADRs [0009](decisions/0009-benchmark-methodology.md)
and [0018](decisions/0018-competitive-benchmarks.md).

**Linux results come first** because that is where tickertape is meant to run, and **bare metal comes first** among
them. The cloud-VM and macOS results are secondary.

The sustained-workload, checkpoint, comparison, reader and scale results and the long verified runs of 2026-11-19 to
2026-11-22 ran on Google Cloud VMs
([below](#2026-11-19-to-2026-11-22-sustained-workloads-on-google-cloud-vms-linux-x86-64-and-aarch64)), after the
bare-metal sections. The profiles of v1.0.0's readers, notification and serialization paths (where the CPU time goes, and
what remains per record) are [under 2026-11-25](#2026-11-25-where-the-cpu-time-goes-profiles-of-the-readers-notification-and-serialization-google-cloud-vms).

**How Chronicle Queue and tickertape were configured in the comparisons below.** Open-source Chronicle Queue (2026.8,
the version `compare/chronicle/pom.xml` pins) ran with its default queue options, except `SyncMode.SYNC` in the
sustained SD runs (`compare/chronicle/src/main/java/compare/Main.java`):
- no page pre-touching: the pretoucher has been Enterprise-only since Chronicle Queue 5.24 (upstream commit
  `5056836a1`, first tagged `5.24ea0`; the open-source version has only the interface), and the harness did not call
  the open-source `microTouch()`. That method touches the next 4 KiB page from the calling thread
  (`MicroToucher.execute()`), so it is a smaller mechanism than a background pretoucher;
- the default `blockSize` of 64 MiB, where Chronicle's README says that in production a larger one may reduce the
  jitter of creating chunks;
- a JIT warm-up only before W1 and W3 (100,000 records through the queue, `compare/java/compare/Harness.java`). W2 and
  the sustained runs do not warm up the queue before the first send. W2 and the paced runs leave the first records out
  of the histogram instead (the unpaced probe leaves none out), so the reader first takes the branch that records a
  latency at the first measured record.

tickertape ran with its defaults: on Linux, prefaulting 256 KiB ahead in the v0.4.0 and later comparisons (it was off
on Linux in v0.3.0, ADR 0023), and early write-back in tickertape's rows of the sustained comparison (`88b3304`, ADR
0038).

In W1, tickertape's `Writer::open` preallocates the first segment before the timer starts. That segment is 128 MiB and
holds the whole run (about 104 MB of records at 64 B and 106 MB at 1,024 B). Chronicle Queue creates its first cycle
file at the first append, inside the timed part. The harness is to be corrected (issue #56) and the comparison rerun
(issue #57).

## 2026-11-02, v0.4.0 on bare-metal Linux (AWS `z1d.metal`)

The primary results for v0.4.0, recorded at `c9ec640`, the code released as v0.4.0. The environment and commands are
those of the v0.3.0 bare-metal section below, on a fresh `z1d.metal`, every process bound to NUMA node 0.

Two changes since v0.3.0 drive the differences:
- CRC32C runs three hardware chains in parallel for long records (ADR 0024);
- Linux prefaults 256 KiB ahead of the writer with `MADV_POPULATE_WRITE` (ADR 0025).

### Comparison with other tools

**W1, append throughput** (million records/s):

| Tool | 64 B | 1,024 B |
|---|---|---|
| **tickertape** | 26.68–26.92 | 3.12–3.22 |
| chronicle | 4.75–4.76 | 1.42–1.44 |
| yaque | 1.10 | 0.70–0.71 |
| queue-file | 0.31 | 0.23 |
| plain-file | 1.12 | 0.72 |

**W3, warm replay** (million records/s):

| Tool | Reader | 64 B | 1,024 B |
|---|---|---|---|
| **tickertape** | safe | 22.03–22.12 | 4.77–4.98 |
| **tickertape** | mapped | 23.04–23.10 | 4.49–4.70 |
| chronicle | default | 18.78–20.37 | 5.00–5.28 |
| yaque | default | 16.41–16.42 | 3.52–3.53 |
| queue-file | default | 0.45 | 0.42 |
| plain-file | default | 1.39 | 1.24 |

**W2, writer-to-reader latency** (µs; tickertape's `safe` reader uses `pread`, `mapped` is the zero-copy reader).

64 B at 10,000 records/s:

| Tool | Reader | p50 | p99 | p99.9 | max | late sends |
|---|---|---|---|---|---|---|
| **tickertape** | safe | 3.4–3.5 | 4 | 4.2–4.4 | 62–324 | 266–282 |
| **tickertape** | mapped | 0.3–0.4 | 0.5 | 2.1 | 258–300 | 216–240 |
| chronicle | default | 0.5 | 0.6–0.7 | 3–3.9 | 435–454 | 353–479 |
| aeron | default | 0.3 | 2.6–2.9 | 15–18 | 5,190–5,243 | 220–415 |
| yaque | default | 2 | 4–4.9 | 6.8–7.4 | 3,369–3,463 | 119–142 |
| plain-file | default | 1.8 | 3.9–4.2 | 7.3–8.8 | 3,434–3,506 | 244–531 |

1,024 B at 10,000 records/s:

| Tool | Reader | p50 | p99 | p99.9 | max | late sends |
|---|---|---|---|---|---|---|
| **tickertape** | safe | 3.8 | 4.3 | 16–26 | 622–652 | 1,536–2,410 |
| **tickertape** | mapped | 0.7 | 2.3 | 20–21 | 1,283–1,290 | 1,964–2,024 |
| chronicle | default | 0.6 | 2.5 | 47–48 | 453–454 | 680–749 |
| aeron | default | 0.4 | 2.7 | 5 | 306–351 | 251–271 |
| yaque | default | 2.5 | 8.4–8.5 | 7,725–7,782 | 20,251–20,283 | 154–156 |
| plain-file | default | 2.2–2.3 | 6.6–6.7 | 8.2–8.3 | 554–2,736 | 130–325 |

64 B at 100,000 records/s:

| Tool | Reader | p50 | p99 | p99.9 | max | late sends |
|---|---|---|---|---|---|---|
| **tickertape** | safe | 3.4–3.5 | 4–4.9 | 4.3–6.3 | 360–470 | 75–270 |
| **tickertape** | mapped | 0.3 | 0.5 | 0.5–1.8 | 24–48 | 31–191 |
| chronicle | default | 0.4 | 0.5–0.6 | 36–37 | 636–740 | 6,341–6,350 |
| aeron | default | 0.3 | 0.5–1.7 | 92–133 | 1,783–1,821 | 920–11,713 |
| yaque | default | 1.8 | 3.9–4.2 | 7.8–8 | 178–182 | 240–344 |
| plain-file | default | 1.7 | 3.9–4.2 | 7.1–7.2 | 181–182 | 214–11,224 |

1,024 B at 100,000 records/s:

| Tool | Reader | p50 | p99 | p99.9 | max | late sends |
|---|---|---|---|---|---|---|
| **tickertape** | safe | 3.8 | 4.4–5.3 | 7.4–9.2 | 678–781 | 656–1,043 |
| **tickertape** | mapped | 0.7 | 1–1.2 | 36–76 | 1,374–1,407 | 668–707 |
| chronicle | default | 0.4–0.5 | 37–41 | 48–55 | 620–642 | 49,424–51,697 |
| aeron | default | 0.3–0.4 | 0.7 | 0.8 | 170–390 | 1,687–1,731 |
| yaque | default | 2.3–2.4 | 8,241–8,278 | 19,055–19,087 | 20,398–20,447 | 3,041–3,090 |
| plain-file | default | 2.1 | 6.7 | 34–38 | 123–338 | 1,670–1,757 |

What changed against v0.3.0 on the same machine type, and where tickertape now stands:

**Append.** 26.7–26.9 million records/s at 64 B (v0.3.0: 17.1–17.3), 5.6× Chronicle Queue. At 1,024 B,
3.1–3.2 million records/s (v0.3.0: 1.9), 2.2× Chronicle Queue. This is a short burst into a segment preallocated
before the timer starts (above).

**Replay.**
- 64 B: tickertape is fastest, 22.0–23.1 against Chronicle Queue's 18.8–20.4.
- 1,024 B: 4.8–5.0 million records/s with the `safe` reader (v0.3.0: 3.0), close to Chronicle Queue's 5.0–5.3.
  Chronicle Queue checks no checksum; tickertape validates every record.

**Latency.** At 64 B, tickertape's zero-copy reader has the lowest p99.9 of all tools measured, Aeron's in-memory
IPC included:
- 2.1 µs at 10,000 records/s;
- 0.5–1.8 µs at 100,000 records/s, against 36–37 µs for Chronicle Queue and 92–133 µs for Aeron.

At 1,024 B, Aeron IPC (which persists nothing) has the lowest tails: p99.9 0.8–5 µs. Against Chronicle Queue:
- at 10,000 records/s, tickertape's p99.9 is 16–26 µs (`safe` reader) and 20–21 µs (zero-copy reader), against
  47–48 µs; the plain file's is lower, at 8.2–8.3 µs;
- at 100,000 records/s, the `safe` reader's is 7.4–9.2 µs against 48–55 µs, but the zero-copy reader's is 36–76 µs,
  around Chronicle Queue's. The comparison runs `tt-latency` for tickertape; in the separate `tt-latency` runs below
  (the same program and settings, run outside the comparison suite) it was 110–243 µs on ext4 and 10–11 µs on xfs.

Late sends at 1,024 B and 100,000 records/s fell from about 28,000 to about 700 for the zero-copy reader.

### tickertape's own benchmarks

All at 100,000 records/s; µs; ext4 / xfs; ranges over two runs. On Linux `prefault` now enables the 256 KiB
`MADV_POPULATE_WRITE` helper, so the `--no-prefault` rows show the effect directly.

| Payload | Reader, wait | p50 | p99 | p99.9 | p99.99 | max |
|---|---|---|---|---|---|---|
| 64 B | mapped, spin | 0.31–0.35 / 0.31–0.32 | 0.47–0.50 / 0.45 | 1.7–2.0 / 0.49–1.4 | 31–158 / 5.9–7.0 | 278–399 / 45–392 |
| 64 B | mapped, spin, `--no-prefault` | 0.32 / 0.32 | 0.47–1.2 / 0.46 | 31–34 / 24–28 | 69–245 / 42–154 | 328–351 / 367–387 |
| 64 B | safe, spin | 3.4 / 3.5 | 4.7–4.8 / 4.1–4.7 | 5.7–6.1 / 4.4–6.2 | 14–47 / 10–11 | 187–364 / 31–35 |
| 64 B | safe, default (blocks on the wake-up region) | 3.4 / 3.5 | 4.0 / 4.2–4.9 | 4.4–6.0 / 5.8–6.2 | 11–255 / 33–183 | 28–370 / 357–362 |
| 1,024 B | mapped, spin | 0.69–0.71 / 0.69–0.70 | 2.1–2.2 / 1.2–2.0 | 110–243 / 10–11 | 1,202–1,295 / 997–1,003 | 1,529–1,586 / 1,217–1,236 |
| 1,024 B | mapped, spin, `--no-prefault` | 0.65–0.67 / 0.67–0.68 | 31 / 23 | 97–122 / 39–41 | 1,184–1,209 / 1,032–1,034 | 1,346–1,443 / 1,230–1,248 |
| 1,024 B | safe, spin | 3.8 / 3.9 | 4.4–4.5 / 5.4–5.6 | 6.9–9.3 / 7.2–9.1 | 520–534 / 337 | 635–696 / 456–470 |
| 1,024 B | safe, default | 3.8 / 3.9 | 5.4–5.9 / 4.5–4.8 | 14–18 / 7.5–9.6 | 633–639 / 347–350 | 766–804 / 466–497 |

What these rows show:
- **Prefaulting helps at 64 B on both filesystems**, cutting the zero-copy reader's p99.9 by more than 15×.
- **At 1,024 B it cuts p99 from 23–31 µs to 1.2–2.2 µs.** The zero-copy reader's p99.9 at 1,024 B is the exception
  on ext4: 110–243 µs with prefaulting against 97–122 µs without. On xfs prefaulting helps there too. **Open:** why
  it is worse with prefaulting on ext4 has not been explained. With prefaulting on, the two runs differ by more than
  2× (110 and 243 µs), against 97 and 122 µs with it off, and the comparison suite's own `tt-latency` runs above
  measured 36–76 µs.
- **With prefaulting off, the zero-copy reader's p99.9 on ext4 is close to Chronicle Queue's only at 64 B**: 31–34 µs
  against 36–37 µs. At 1,024 B it is 97–122 µs against 48–55 µs. (Chronicle Queue's figures are the comparison's above,
  on ext4; it was not run on xfs on this machine.)

Other results, ext4 / xfs:
- **`tt-checkpoint`** (16 MiB segments, 1,000 records per checkpoint):
  - `checkpoint()` p50 is 0.84 / 0.71–0.73 ms at 64 B and 1.8 / 1.6 ms at 1,024 B. In v0.3.0 it was 0.38 / 0.34 and
    1.3 / 1.1 ms: the pages prefaulted ahead are dirty, so the file sync also writes them out.
  - Roll appends take 0.19–0.73 ms.
- **Criterion:**
  - append: 25.5 / 24.9 million records/s at 64 B and 4.15 / 4.65 at 1,024 B; with prefaulting off, 16.2 / 19.3
    and 3.27 / 4.16;
  - replay: 21.6 / 21.5 (safe) and 23.1 / 23.3 (zero-copy) at 64 B; 4.97 / 4.92 and 4.27 / 4.18 at 1,024 B.
- **Cold-cache replay:** 8.9–9.0 million records/s at 64 B; 1.79–1.83 million records/s at 1,024 B, which is
  1.84–1.87 GB/s of payload.

**Steady state.** All of these runs last about 11 seconds. In 5-minute experiments at 1,024 B and 100,000 records/s
(ADR 0025), ext4 write-back occasionally stalled the writer for milliseconds (p99.99 about 8–12 ms) with or without
prefaulting. The comparison suite does not measure steady state yet, for any tool.

## 2026-11-02, v0.3.0 on bare-metal Linux (AWS `z1d.metal`)

The primary results for v0.3.0. They were recorded at the release tag (`e81e6b8`) with the same commands as the
cloud-VM section below.

**Environment.** An AWS EC2 `z1d.metal` in us-east-1, **bare metal** (`systemd-detect-virt` reports `none`):
- two Intel Xeon Platinum 8151 (Skylake-SP) at 3.4 GHz, 2 × 12 cores × 2 threads, 384 GiB of memory;
- Ubuntu 24.04, kernel 7.0.0-1014-aws, rustc 1.99.1, OpenJDK 21.0.12.1;
- **every process, for every tool, bound to NUMA node 0** (`numactl --cpunodebind=0 --membind=0`, 24 hardware threads);
- the journal on one of the two local NVMe disks, ext4 formatted with lazy initialisation off; tickertape's own
  benchmarks also on the second disk, formatted xfs;
- a fresh instance, idle except for the benchmarks.

With enough cores, Aeron runs as intended here: its media driver and busy-spinning threads each get their own
hardware thread.

### Comparison with other tools

The same suite, workloads and caveats as in the sections below (`compare/README.md`). Two runs each.

**W1, append throughput** (million records/s):

| Tool | 64 B | 1,024 B |
|---|---|---|
| **tickertape** | 17.05–17.25 | 1.87–1.90 |
| chronicle | 4.75–4.80 | 1.42–1.44 |
| yaque | 1.08–1.09 | 0.70–0.71 |
| queue-file | 0.31 | 0.23–0.24 |
| plain-file | 1.12 | 0.72 |

**W3, warm replay** (million records/s):

| Tool | Reader | 64 B | 1,024 B |
|---|---|---|---|
| **tickertape** | safe | 23.81–23.85 | 3.03–3.06 |
| **tickertape** | mapped | 22.64–22.69 | 2.97 |
| chronicle | default | 20.15–20.99 | 5.01–5.53 |
| yaque | default | 16.67–16.76 | 3.55–3.57 |
| queue-file | default | 0.45 | 0.42 |
| plain-file | default | 1.38–1.39 | 1.24–1.25 |

**W2, writer-to-reader latency** (µs; tickertape's `safe` reader uses `pread`, `mapped` is the zero-copy reader).

64 B at 10,000 records/s:

| Tool | Reader | p50 | p99 | p99.9 | max | late sends |
|---|---|---|---|---|---|---|
| **tickertape** | safe | 3.4 | 3.9 | 19–20 | 271–285 | 269–313 |
| **tickertape** | mapped | 0.3 | 0.5 | 15–17 | 52–772 | 271–287 |
| chronicle | default | 0.5 | 0.7–2.9 | 5.6–15 | 450–452 | 335–110,387 |
| aeron | default | 0.3 | 2.6–2.7 | 16 | 5,272–5,296 | 211–280 |
| yaque | default | 2 | 4.1–4.4 | 7.3–8 | 3,428–3,480 | 127–145 |
| plain-file | default | 1.8 | 3.9–4 | 6.8–8.9 | 3,441–3,471 | 121–242 |

1,024 B at 10,000 records/s:

| Tool | Reader | p50 | p99 | p99.9 | max | late sends |
|---|---|---|---|---|---|---|
| **tickertape** | safe | 4–4.1 | 17 | 30–31 | 569–575 | 2,094–2,203 |
| **tickertape** | mapped | 1 | 9.1–13 | 41–43 | 1,295–1,310 | 1,936–2,132 |
| chronicle | default | 0.6 | 2.6 | 49–51 | 471–482 | 726–1,023 |
| aeron | default | 0.4 | 2.7–2.9 | 4.8–5.9 | 332–347 | 211–278 |
| yaque | default | 2.4–2.5 | 8.6–8.7 | 8,032–8,110 | 20,513–20,726 | 156–302 |
| plain-file | default | 2.2 | 6.6–6.7 | 8.2–8.9 | 2,650–2,777 | 114–461 |

64 B at 100,000 records/s:

| Tool | Reader | p50 | p99 | p99.9 | max | late sends |
|---|---|---|---|---|---|---|
| **tickertape** | safe | 3.4 | 4 | 20 | 46–370 | 725–788 |
| **tickertape** | mapped | 0.3 | 0.5 | 30 | 48–50 | 2,669–2,672 |
| chronicle | default | 0.4 | 0.5 | 36–37 | 632–635 | 5,475–6,038 |
| aeron | default | 0.3 | 0.4 | 28–63 | 1,613–1,627 | 732–767 |
| yaque | default | 1.8–1.9 | 4–4.1 | 7.6–8.2 | 192–201 | 258–352 |
| plain-file | default | 1.7 | 3.7–3.8 | 6.9–8.8 | 200–208 | 217–1,945 |

1,024 B at 100,000 records/s:

| Tool | Reader | p50 | p99 | p99.9 | max | late sends |
|---|---|---|---|---|---|---|
| **tickertape** | safe | 4 | 21 | 31 | 602–721 | 5,797–5,840 |
| **tickertape** | mapped | 0.9 | 32–33 | 55–237 | 1,337–1,360 | 27,800–28,284 |
| chronicle | default | 0.4–0.5 | 38–39 | 48–52 | 603–625 | 49,375–49,555 |
| aeron | default | 0.4 | 0.7 | 2.8–96 | 121–1,849 | 1,618–1,713 |
| yaque | default | 2.3–2.4 | 8,454–8,675 | 19,268–19,464 | 20,595–20,775 | 3,080–3,767 |
| plain-file | default | 2–2.1 | 6.5–6.6 | 30–32 | 119–166 | 1,535–1,633 |

What bare metal shows:

**Append.** tickertape appends 3.6× faster than Chronicle Queue at 64 B (17.1 against 4.8 million records/s) and
1.3× faster at 1,024 B (1.9 against 1.4).

**Replay.** tickertape is fastest at 64 B (23.8 against 20.2–21.0). At 1,024 B Chronicle Queue is clearly faster:
5.0–5.5 against tickertape's 3.0, and yaque does 3.6.

**Latency.** Aeron IPC, an in-memory transport that persists nothing, has the lowest tails overall:
- 1,024 B at 10,000 records/s: p99.9 of 4.8–5.9 µs;
- 1,024 B at 100,000 records/s: 2.8–96 µs.

Against Chronicle Queue, the other persistent journal, tickertape's `safe` reader has the lower p99.9 at 100,000
records/s:
- 64 B: 20 µs against 36–37 µs;
- 1,024 B: 31 µs against 48–52 µs.

At 10,000 records/s, Chronicle Queue's 64 B tail is lower (5.6–15 µs against 15–20 µs). At 1,024 B tickertape's
`safe` reader is lower (30–31 µs against 49–51 µs). Chronicle Queue and Aeron have the lowest medians (0.3–0.6 µs). tickertape's zero-copy reader matches
them at 64 B (0.3 µs); the `safe` reader pays for a `pread` call (3.4–4 µs).

### tickertape's own benchmarks

All at 100,000 records/s; µs; ext4 / xfs; ranges over two runs. `prefault` has no effect on Linux in v0.3.0
(ADR 0023).

| Payload | Reader, wait | p50 | p99 | p99.9 | p99.99 | max |
|---|---|---|---|---|---|---|
| 64 B | mapped, spin | 0.30–0.31 / 0.30 | 0.46–0.48 / 0.45–2.0 | 30–35 / 21–23 | 41–251 / 33–37 | 49–387 / 45–75 |
| 64 B | safe, spin | 3.4 / 3.5 | 4.0–5.0 / 4.1–4.6 | 20–21 / 19–28 | 30–34 / 29–248 | 38–145 / 37–358 |
| 64 B | safe, default (blocks on the wake-up region) | 3.4 / 3.5 | 5.3–5.6 / 4.1–4.8 | 20–28 / 19–28 | 30–291 / 29–257 | 47–430 / 56–368 |
| 1,024 B | mapped, spin | 0.93–0.96 / 0.94–0.96 | 32–33 / 23–24 | 67–277 / 39–43 | 1,121–1,227 / 999–1,017 | 1,332–1,452 / 1,205–1,218 |
| 1,024 B | safe, spin | 4.0 / 4.1–4.2 | 20–21 / 20 | 31–33 / 31 | 441–466 / 324–325 | 583–586 / 455–466 |
| 1,024 B | safe, default | 4.0 / 4.1 | 21 / 20 | 31–32 / 31 | 531–557 / 338–348 | 753–812 / 458–558 |

The mapped-reader rows merge the `prefault` and `--no-prefault` runs, four runs each, because they are the same
configuration on Linux.

Other results, ext4 / xfs:
- **`tt-checkpoint`** (16 MiB segments, 1,000 records per checkpoint):
  - `checkpoint()` p50 is 0.38 / 0.34 ms at 64 B and 1.3 / 1.1 ms at 1,024 B;
  - roll appends take 0.16–0.62 ms on both filesystems.
- **Criterion:**
  - append: 18.0 / 19.2–19.8 million records/s at 64 B, 2.39–2.40 / 2.53–2.54 at 1,024 B;
  - replay: 23.1 / 23.1 million records/s at 64 B (safe reader), 3.08 / 3.09 at 1,024 B.
- **Cold-cache replay** (page cache dropped): 8.9–9.0 million records/s at 64 B; 1.66–1.67 million records/s at
  1,024 B, which is 1.70–1.71 GB/s of payload from the NVMe disk.

## 2026-11-19 to 2026-11-22, sustained workloads on Google Cloud VMs (Linux, x86-64 and aarch64)

Results of the sustained sessions (ADR 0026), the checkpoint work (ADR 0038), the comparison with other tools, the
readers' per-record cost, replay and recovery at scale (ADR 0042), and long verified runs (issues #43 and #45). All of
them ran on **Google Cloud virtual machines with local NVMe SSD, not bare metal**, on other CPUs and with other commits
than the bare-metal v0.4.0 results above; the two are not compared. The raw output of every run (results, time series,
environment, commands, logs) is kept outside the repository. Latency tools schedule sends at a fixed rate and measure
from the intended send time (coordinated omission handled at the source; no skipped sends or rebasing), as in the
sections above.

### Environments

Common to every run: Ubuntu 24.04.5 LTS, kernel 7.0.0-1017-gcp (`systemd-detect-virt`: `google`), rustc 1.100.0, OpenJDK
21.0.13.1 for the comparison's Java tools; one NUMA node; the I/O scheduler `none`. Journals are on a local NVMe SSD
(375 GB): ext4 formatted with `-E lazy_itable_init=0,lazy_journal_init=0`, xfs with defaults, both mounted `noatime`;
the write-back sysctls at the distribution's defaults (`dirty_background_ratio` 10, `dirty_ratio` 20,
`dirty_expire_centisecs` 3000, `dirty_writeback_centisecs` 500) unless a run says otherwise. CRC32C uses tickertape's
hardware path on both architectures (`lscpu` lists `sse4_2` and `crc32`). Processes are pinned with
`tt-sustained --cpus` to one logical CPU per physical core, skipping core 0 (the writer first, then the readers).

| Recording | Dates (UTC) | Commit | Machine type, zone | CPU, vCPUs, memory |
|---|---|---|---|---|
| Sustained session and comparison, first session | 2026-11-20 | `279f59a` | x86-64: `c4-standard-16-lssd` (ext4 in us-east1-b, xfs in us-central1-b); aarch64: `c4a-standard-8-lssd` (us-east1-b); one VM per architecture and filesystem | Intel Xeon 6985P-C, 16 vCPUs, 63 GB; Google Axion (Neoverse-V2), 8 vCPUs, 34 GB |
| Second sustained session (both payloads) and long verified runs | 2026-11-22 | `3fea1c0` | x86-64: `c4-standard-16-lssd` (ext4 in us-east1-b, xfs in us-central1-b); aarch64: `c4a-standard-8-lssd` (ext4 in us-central1-b, xfs in us-east1-d); one VM per architecture and filesystem | as the first session |
| Confirming long runs (issue #45) | 2026-11-22 | `fbe5785` | x86-64: `c4-standard-16-lssd` (ext4 in us-east1-b); aarch64: `c4a-standard-8-lssd` (xfs in us-east1-d); two new VMs | as the first session |
| tickertape's rows of the comparison, second session | 2026-11-22 | `88b3304`, with the one-line `benches/session.py` fix of this change applied locally | the same machine types: x86-64 ext4 in us-east1-b, xfs in us-central1-b; aarch64 in us-east1-d | as above |
| Reference before the checkpoint work | 2026-11-19 | `29960a3` | `c4-standard-16-lssd` (us-central1-b), `c4a-standard-16-lssd` (us-east1-b); ext4, then xfs on the same VM | Xeon 6985P-C, 16 vCPUs, 63 GB; Axion, 16 vCPUs, 67 GB |
| Checkpoint prototype | 2026-11-19 to 20 | `552ed71` (branch of pull request #28) | as the reference | as the reference |
| Checkpoint finish round | 2026-11-20 | aarch64 `f194c42`, x86-64 `49b1e89` (both on the branch of #28) | as the reference | as the reference |
| Reader per-record cost | 2026-11-19 | `7ed7782`, `a381fe1`, `2522869` (branch of #24) | `c3-standard-8-lssd` (us-central1-a), `c4a-standard-8-lssd` (us-east1-b) | Intel Xeon Platinum 8481C, 8 vCPUs; Axion, 8 vCPUs |
| Replay and recovery at scale | 2026-11-20 | `dd450d1` | `c3-standard-4-lssd` (us-central1-a), `c4a-standard-4-lssd` (us-east1-b); ext4 | Xeon Platinum 8481C, 4 vCPUs, 16 GB; Axion, 4 vCPUs, 16 GB |

Pull request #28 was squash-merged as `279f59a` and #24 as `6ee9a0e`; the branch commits named above are not in `main`'s
history. In the sustained sessions and the long runs `--out` was on the ext4 SSD (`/data`), so on the ext4 VMs the
results were written to the filesystem under test (the playbook prefers another filesystem).

### Sustained session: tickertape with background checkpoints (`279f59a`)

Command, on each VM (`/data/tt` on ext4, `/data2/tt` on xfs), run twice (the second run resumes the first):

```sh
python3 benches/session.py --journal-dir /data/tt --out /data/results/session-ext4 \
  --scenarios steady-50,steady-90,bursty,visibility-only --checkpoint-mode background \
  --cpus 1,2,3,4,5,6 --max-run 2400 --yes
```

It runs `tt-sustained` with 10-minute runs (plus 10 s of warm-up), the probe once and every other scenario twice; for
example steady-50 at 64 B on x86-64 ext4:

```sh
tt-sustained run --dir … --out … --rate 1446707 --duration 600s --warmup 10s --payload 64 --checkpoint-every 1s \
  --retain-bytes 68719476736 --readers mapped,safe:wait=default,safe:durable:wait=block \
  --checkpoint-mode background --cpus 1,2,3,4,5,6 --json
```

Settings: 128 MiB segments; prefault and early write-back on; helper threads on every CPU (the default); a background
checkpoint (`Writer::request_checkpoint`) every second on a wall-clock grid, except visibility-only, which neither
checkpoints nor removes segments; retention of 64 GiB. Readers: `mapped` busy-polls, `safe` uses the default wait (spin,
then the wake-up region), the durable-only reader blocks. The scenarios, with R the probe's achieved rate: probe
(unpaced), steady-50 and steady-90 (0.5R and 0.9R), bursty (2R for 200 ms of every second, mean 0.4R), visibility-only
(0.5R). The lagging and beyond-RAM scenarios were not part of this session (`--scenarios`); their results are those of
the reference session below, with blocking checkpoints.

**What did not run or failed.** 64 B only: the 1,024 B probe failed on all four VMs, so no 1,024 B scenario ran
(`probe for 1024 B has no result`). On x86-64 (ext4 and xfs) the 1,024 B probe stopped with `benchmark timed out`. **On
aarch64 (ext4 and xfs) a reader of the 1,024 B probe stopped with
`corrupt journal data in …/00000000000103942656.tape at offset 14036352: record checksum mismatch`** (xfs:
`…/00000000000094355712.tape` at offset 44317792), in the active segment near the writer's position, with neither reader
lagging. It was later traced to the reader's view lagging a correctly published record (issue #43, closed;
`crash-contract.md` → Known risks and limits). The 1,024 B probes of the reference session (`29960a3`, on
`c4a-standard-16-lssd`) and of the comparison's tickertape rows (`88b3304`, on `c4a-standard-8-lssd`, without
checkpoints) completed, but neither is this scenario at this commit. Three 64 B runs failed: steady-90 repeat 1 on both xfs VMs
(`background checkpoints did not finish within 120 s of the run's end`) and steady-90 repeat 2 on aarch64 ext4
(`reader timed out`); their rows show one run. The 1,024 B scenarios ran in the second session, at `3fea1c0`
([below](#second-sustained-session-both-payloads-background-checkpoints-3fea1c0)).

Cells are ext4 / xfs, with the range over the two repeats (the probe ran once). Latencies in µs unless marked ms or s;
"to" separates two repeats that differ by more than one unit. Late sends are sends more than 1 µs after their intended
time. CPU is the fraction of one CPU over the run: the process (all threads) and the measuring thread.

**Writer schedule and CPU (64 B):**

| Scenario | Arch | Rate (records/s) | Achieved (records/s) | Late sends (%) | Max schedule delay | Writer CPU (process) | Writer CPU (measuring thread) |
|---|---|---|---|---|---|---|---|
| probe (unpaced) | x86-64 | unpaced | 2,893,414 / 3,098,194 | – | – | 1.18 / 1.00 | 0.83 / 0.73 |
| probe (unpaced) | aarch64 | unpaced | 2,902,516 / 2,919,079 | – | – | 0.91 / 0.92 | 0.70 / 0.72 |
| steady-50 | x86-64 | 1,446,707 / 1,549,097 | 1,446,707 / 1,549,097 | 9.6–9.8 / 8.0–8.8 | 1.17–4.0 ms / 0.49–2.6 ms | 1.16–1.17 / 1.18 | 1.00 / 1.00 |
| steady-50 | aarch64 | 1,451,258 / 1,459,539 | 1,451,258 / 1,459,539 | 1.9 / 1.9–2.1 | 0.75–3.4 ms / 527–628 | 1.10 / 1.10 | 1.00 / 1.00 |
| steady-90 | x86-64 | 2,604,072 / 2,788,374 | 2,399,855–2,428,409 / 2,696,320 | 82.2–83.2 / 72.4 | 44–52 s / 23 s | 1.16–1.17 / 1.06 | 0.85–0.86 / 0.78 |
| steady-90 | aarch64 | 2,612,264 / 2,627,171 | 2,498,160 / 2,489,672 | 72.3 / 73.6 | 35 s / 41 s | 0.95 / 0.95 | 0.75 / 0.77 |
| bursty | x86-64 | 1,157,365 / 1,239,277 | 1,158,477–1,158,490 / 1,240,557–1,240,596 | 100.0 / 100.0 | 209–232 ms / 186–187 ms | 1.12 / 1.12 | 1.00 / 1.00 |
| bursty | aarch64 | 1,161,006 / 1,167,631 | 1,162,273–1,162,284 / 1,168,883–1,168,894 | 100.0 / 100.0 | 178 ms / 189–190 ms | 1.06 / 1.06 | 1.00 / 1.00 |
| visibility-only | x86-64 | 1,446,707 / 1,549,097 | 1,446,707 / 1,549,097 | 6.7–7.6 / 4.4–4.6 | 2.8–235 ms / 13.9–149 ms | 1.14 / 1.14 | 1.00 / 1.00 |
| visibility-only | aarch64 | 1,451,258 / 1,459,539 | 1,451,258 / 1,459,539 | 1.3 / 1.2 | 630–671 ms / 557–666 ms | 1.07 / 1.06 | 1.00 / 1.00 |

**Append latency (64 B, every append timed, µs):**

| Scenario | Arch | p50 | p99 | p99.9 | p99.99 | max |
|---|---|---|---|---|---|---|
| probe (unpaced) | x86-64 | 0.17 / 0.12 | 1.38 / 1.25 | 2.1 / 1.96 | 6.4 / 5.7 | 137 ms / 81 ms |
| probe (unpaced) | aarch64 | 0.13 / 0.13 | 0.21 / 0.21 | 1.87 / 1.79 | 2.6 / 2.6 | 580 ms / 353 ms |
| steady-50 | x86-64 | 0.24 / 0.20 | 1.36–1.49 / 1.33–1.36 | 2.3–2.7 / 2.2–2.3 | 7.7–7.9 / 6.5–7.0 | 1.04–1.72 ms / 0.49–2.6 ms |
| steady-50 | aarch64 | 0.14 / 0.14–0.15 | 0.64 / 0.63–0.64 | 2.3 / 2.3–2.5 | 3.9 / 4.3–4.5 | 0.72–3.4 ms / 510–609 |
| steady-90 | x86-64 | 0.22 / 0.17 | 1.45 / 1.29 | 2.4–2.5 / 2.2 | 7.5 / 6.2 | 10.4–108 ms / 58 ms |
| steady-90 | aarch64 | 0.13 / 0.14 | 0.60 / 0.60 | 2.1 / 2.1 | 3.3 / 3.8 | 168 ms / 228 ms |
| bursty | x86-64 | 0.21 / 0.12 | 1.43–1.46 / 1.28–1.30 | 2.1–2.2 / 1.74–1.88 | 6.9–7.3 / 6.3–6.5 | 3.6–4.0 ms / 5.1–6.5 ms |
| bursty | aarch64 | 0.13 / 0.13 | 0.52–0.58 / 0.56–0.59 | 2.1 / 2.0–2.1 | 3.3–3.4 / 3.8 | 8.8 ms / 10.1–24 ms |
| visibility-only | x86-64 | 0.19–0.20 / 0.12 | 1.39–1.41 / 1.24–1.25 | 1.88–2.00 / 1.79–1.88 | 6.2–6.6 / 5.9–6.1 | 2.7–103 ms / 13.9–113 ms |
| visibility-only | aarch64 | 0.13 / 0.13 | 0.21 / 0.22 | 1.98–2.00 / 1.94 | 3.3 / 3.7 | 309–671 ms / 557–666 ms |

**Checkpoints and rolls (64 B).** "Pause" is, per checkpoint, the writer thread's longest stretch of work around it:
the request, and every append and progress poll while the checkpoint was in flight; "to durable" is from the request
until the checkpoint is durable (its marker synced).

| Scenario | Arch | Pause p50 | Pause p99 | Pause max | To durable p50 | To durable p99 | To durable max | Rolls | Roll p50 | Roll max |
|---|---|---|---|---|---|---|---|---|---|---|
| probe (unpaced) | x86-64 | 1.85 ms / 3.7 ms | 90 ms / 81 ms | 137 ms / 81 ms | 23 s / 57 s | 165 s / 312 s | 170 s / 316 s | 1,345 / 1,440 | 146 / 104 | 68 ms / 7.7 ms |
| probe (unpaced) | aarch64 | 125 ms / 292 | 580 ms / 353 ms | 580 ms / 353 ms | 67 s / 62 s | 282 s / 275 s | 287 s / 280 s | 1,349 / 1,357 | 196 / 163 | 140 ms / 7.3 ms |
| steady-50 | x86-64 | 84–88 / 80 | 195–200 / 154–245 | 257–527 / 324–787 | 1.72–2.5 ms / 1.85–1.86 ms | 4.6–4.9 ms / 3.3–3.6 ms | 5.4–5.5 ms / 3.9–7.2 ms | 683 / 732 | 241–343 / 114–129 | 0.81–1.38 ms / 325–787 |
| steady-50 | aarch64 | 49–51 / 41–44 | 111–115 / 190–195 | 465–481 / 510–610 | 873–881 / 752–757 | 1.16–1.18 ms / 1.01–1.03 ms | 1.21–1.28 ms / 1.08–1.09 ms | 685 / 689 | 188–254 / 155–162 | 719–723 / 510–609 |
| steady-90 | x86-64 | 1.30–1.69 ms / 366 | 10.4–108 ms / 58 ms | 10.4–108 ms / 58 ms | 9.6–12.4 s / 21 s | 135–171 s / 230 s | 141–176 s / 236 s | 1,230 / 1,317 | 418–456 / 120 | 8.9–10.4 ms / 5.8 ms |
| steady-90 | aarch64 | 3.5 ms / 195 | 168 ms / 228 ms | 168 ms / 228 ms | 12.9 s / 12.0 s | 203 s / 197 s | 209 s / 203 s | 1,234 / 1,241 | 404 / 158 | 7.0 ms / 1.78 ms |
| bursty | x86-64 | 102–121 / 69–80 | 1.90–1.91 ms / 3.1–3.5 ms | 2.6–3.2 ms / 5.1–6.5 ms | 3.1–3.4 ms / 44–49 ms | 44–48 ms / 66–67 ms | 53–54 ms / 69–70 ms | 547 / 585 | 321–338 / 111–117 | 3.6–4.0 ms / 247–709 |
| bursty | aarch64 | 1.31–1.45 ms / 60–62 | 6.9–7.3 ms / 4.0 ms | 8.8 ms / 4.9 ms | 35 ms / 37–38 ms | 53 ms / 53 ms | 56–57 ms / 54–55 ms | 548 / 551 | 385–605 / 148–153 | 4.8–5.5 ms / 0.48–10.1 ms |
| visibility-only | x86-64 | – | – | – | – | – | – | 683 / 732 | 258–302 / 97–107 | 0.62–2.7 ms / 3.9–7.2 ms |
| visibility-only | aarch64 | – | – | – | – | – | – | 685 / 689 | 254–259 / 158–160 | 2.3–2.8 ms / 595–599 |

**Readers (64 B).** Visible readers measure from the intended send time to the record's arrival after validation; the
durable-only reader to the moment the checkpoint covering the record let it through.

| Scenario | Arch | Reader | p50 | p99 | p99.9 | p99.99 | max | Lag max (records) | CPU |
|---|---|---|---|---|---|---|---|---|---|
| probe (unpaced) | x86-64 | mapped, spin | 0.34 / 0.26 | 2.5 / 1.50 | 478 / 308 | 2.1 ms / 455 | 137 ms / 81 ms | 11,967 / 1,895 | 1.00 / 1.00 |
| probe (unpaced) | x86-64 | safe, default | 4.0 / 4.0 | 9.4 / 7.0 | 56 / 11.6 | 87 ms / 775 | 137 ms / 121 ms | 442,111 / 10,807 | 0.83 / 0.73 |
| probe (unpaced) | aarch64 | mapped, spin | 0.26 / 0.27 | 0.96 / 0.85 | 416 / 398 | 655 / 656 | 580 ms / 353 ms | 3,435 / 2,255 | 1.00 / 1.00 |
| probe (unpaced) | aarch64 | safe, default | 4.7 / 4.8 | 7.5 / 8.5 | 10.2 / 11.5 | 1.97 ms / 96 | 582 ms / 354 ms | 8,623 / 12,039 | 0.70 / 0.72 |
| steady-50 | x86-64 | mapped, spin | 0.45 / 0.35 | 92–107 / 44–75 | 190–230 / 115–126 | 770–920 / 593–612 | 1.49–4.0 ms / 0.86–2.7 ms | 904–1,190 / 730–924 | 1.00 / 1.00 |
| steady-50 | x86-64 | safe, default | 3.2–3.3 / 3.1 | 94–109 / 47–78 | 168–176 / 97–119 | 321–492 / 174–187 | 1.57–4.0 ms / 0.89–2.8 ms | 13–515 / 4–16 | 1.00 / 1.00 |
| steady-50 | x86-64 | safe, durable-only, block | 583–584 ms / 585–587 ms | 994 ms / 994 ms | 1.00 s / 1.00 s | 1.00 s / 1.00 s | 1.01 s / 1.00–1.01 s | 1,453,055 / 1,553,407–1,557,503 | 1.00 / 1.00 |
| steady-50 | aarch64 | mapped, spin | 0.30 / 0.30 | 2.2–2.3 / 2.3–2.6 | 193–223 / 154–171 | 830–876 / 786–800 | 1.43–3.4 ms / 1.17–1.26 ms | 978–1,230 / 1,132–1,153 | 1.00 / 1.00 |
| steady-50 | aarch64 | safe, default | 3.7 / 3.7 | 6.7 / 7.0–7.5 | 41–42 / 32 | 217–268 / 163–171 | 0.94–3.4 ms / 580–666 | 1–4 / 4–21 | 1.00 / 1.00 |
| steady-50 | aarch64 | safe, durable-only, block | 598–599 ms / 599 ms | 993 ms / 993 ms | 1.00 s / 1.00 s | 1.00 s / 1.00 s | 1.00 s / 1.00 s | 1,453,055 / 1,461,247 | 1.00 / 1.00 |
| steady-90 | x86-64 | mapped, spin | 3.4–3.6 s / 378 ms | 43–50 s / 23 s | 44–52 s / 23 s | 44–52 s / 23 s | 44–52 s / 23 s | 828–895 / 4,963 | 1.00 / 1.00 |
| steady-90 | x86-64 | safe, default | 3.4–3.6 s / 378 ms | 43–50 s / 23 s | 44–52 s / 23 s | 44–52 s / 23 s | 44–52 s / 23 s | 204–935 / 5,403 | 0.85–0.86 / 0.78 |
| steady-90 | x86-64 | safe, durable-only, block | 21–23 s / 40 s | 177–199 s / 256 s | 181–203 s / 260 s | 181–204 s / 260 s | 181–204 s / 260 s | 287,047,679–349,437,951 / 435,683,327 | 0.88–0.91 / 0.80 |
| steady-90 | aarch64 | mapped, spin | 1.87 s / 2.6 s | 35 s / 40 s | 35 s / 41 s | 35 s / 41 s | 35 s / 41 s | 1,793 / 2,171 | 1.00 / 1.00 |
| steady-90 | aarch64 | safe, default | 1.87 s / 2.6 s | 35 s / 40 s | 35 s / 41 s | 35 s / 41 s | 35 s / 41 s | 7,515 / 5,039 | 0.75 / 0.77 |
| steady-90 | aarch64 | safe, durable-only, block | 28 s / 25 s | 243 s / 241 s | 245 s / 244 s | 245 s / 245 s | 245 s / 245 s | 437,780,479 / 444,071,935 | 0.73 / 0.76 |
| bursty | x86-64 | mapped, spin | 81–82 ms / 68–73 ms | 195–200 ms / 158–170 ms | 202–211 ms / 163–175 ms | 206–218 ms / 171–179 ms | 209–226 ms / 182–186 ms | 549–811 / 632–768 | 1.00 / 1.00 |
| bursty | x86-64 | safe, default | 81–82 ms / 68–73 ms | 195–200 ms / 158–170 ms | 202–211 ms / 163–175 ms | 206–218 ms / 171–179 ms | 209–226 ms / 182–186 ms | 34–1,138 / 17–54 | 0.39–0.40 / 0.36–0.37 |
| bursty | x86-64 | safe, durable-only, block | 972–975 ms / 1.01 s | 1.03 s / 1.06 s | 1.04 s / 1.06 s | 1.05 s / 1.06–1.07 s | 1.05 s / 1.07 s | 1,266,687–1,270,783 / 1,414,143–1,416,191 | 0.39–0.40 / 0.36–0.37 |
| bursty | aarch64 | mapped, spin | 58 ms / 61 ms | 142–143 ms / 147–148 ms | 148 ms / 151 ms | 151–152 ms / 153–163 ms | 153–154 ms / 162–175 ms | 1,762–15,159 / 2,201–2,379 | 1.00 / 1.00 |
| bursty | aarch64 | safe, default | 58 ms / 61 ms | 142–143 ms / 147–148 ms | 148 ms / 151 ms | 151–152 ms / 153–163 ms | 153–154 ms / 162–175 ms | 15–17 / 12–17 | 0.34 / 0.35 |
| bursty | aarch64 | safe, durable-only, block | 1.01 s / 1.02 s | 1.05 s / 1.05 s | 1.05 s / 1.05 s | 1.05 s / 1.05 s | 1.06 s / 1.05 s | 1,279,999–1,286,143 / 1,284,095–1,285,119 | 0.34 / 0.35 |
| visibility-only | x86-64 | mapped, spin | 0.43 / 0.35 | 24–34 / 5.0–5.1 | 154–756 / 36–394 | 0.85–202 ms / 0.60–123 ms | 3.3–235 ms / 13.9–149 ms | 586–1,424 / 772–2,036 | 1.00 / 1.00 |
| visibility-only | x86-64 | safe, default | 3.0–3.2 / 3.1 | 27–36 / 8.3–8.4 | 67–319 / 28–62 | 0.34–204 ms / 0.21–125 ms | 2.8–235 ms / 16.0–153 ms | 347–33,887 / 5,039–18,271 | 1.00 / 1.00 |
| visibility-only | aarch64 | mapped, spin | 0.29 / 0.29–0.30 | 2.1 / 1.90–1.95 | 277–328 ms / 176–326 ms | 591–635 ms / 519–642 ms | 630–672 ms / 557–666 ms | 1,051–1,137 / 1,036–1,041 | 1.00 / 1.00 |
| visibility-only | aarch64 | safe, default | 3.7 / 3.7 | 6.7–7.0 / 7.1–7.3 | 280–331 ms / 178–329 ms | 593–636 ms / 521–643 ms | 630–675 ms / 561–670 ms | 5,035–30,287 / 5,035–5,043 | 1.00 / 1.00 |

The single slow appends of visibility-only, from `series.jsonl` (UTC): x86-64 ext4 repeat 1, 103 ms at
2026-11-20T04:25:10Z; x86-64 xfs repeat 2, 113 ms at 05:26:29Z; aarch64 ext4, 671 ms at 04:12:47Z and 309 ms at
04:57:33Z; aarch64 xfs, 557 ms at 04:21:24Z and 666 ms at 05:05:55Z. At most one such append per run (x86-64 ext4 repeat
2 and x86-64 xfs repeat 1 had none above 14 ms); no roll append in these runs took longer than 7.2 ms. At `279f59a` a
writer that never checkpoints kept every rolled segment open (the defect fixed in #38). In the second session's
visibility-only runs, at `3fea1c0`, no append took longer than 13.2 ms (below).

### Second sustained session: both payloads, background checkpoints (`3fea1c0`)

The tickertape scenarios of the first session again, at `3fea1c0` (after ADR 0044, the reader's re-read window of #44
and the bounded open segments of #38), now with both payloads, on four new VMs (one per architecture and filesystem;
Environments above). The 1,024 B scenarios ran here for the first time (issues #43 and #45). The 64 B scenarios repeat
the first session's at a newer commit and at other rates: every figure in this section is from `3fea1c0`, and the first
session's tables above are from `279f59a`; the two are not merged. Command, on each VM (`/data2/tt` and
`session-xfs` on xfs), run twice by the VM's script (the first run finished every run, the second found nothing left):

```sh
python3 benches/session.py --journal-dir /data/tt --out /data/results/session-ext4 --only tickertape \
  --scenarios steady-50,steady-90,bursty,visibility-only --checkpoint-mode background \
  --cpus 1,2,3,4,5,6 --max-run 2400 --yes
```

for example steady-50 at 1,024 B on x86-64 ext4:

```sh
tt-sustained run --dir … --out … --rate 173119 --duration 600s --warmup 10s --payload 1024 --checkpoint-every 1s \
  --retain-bytes 68719476736 --readers mapped,safe:wait=default,safe:durable:wait=block \
  --checkpoint-mode background --cpus 1,2,3,4,5,6 --json
```

Settings, readers and scenarios as in the first session. Each VM's open-files limit was 1,024 and transparent huge pages
were set to `madvise`. R, the probe's achieved rate, is per VM: the aarch64 xfs VM (us-east1-d) reached 262,421
records/s at 1,024 B against 338,412 on aarch64 ext4, so its paced 1,024 B rates are lower.

**What did not run or failed.** Nothing failed: all 18 runs finished on every VM (`manifest.json`: 18 done, 0 failed;
the logs hold no error), no reader reported corruption, `read + skipped + remaining = written` held for every reader,
no reader lost a record to retention, and no checkpoint grid point was missed. **One configuration did not keep its
schedule:** steady-90 at 64 B on aarch64 xfs, where in both repeats the writer fell more than the overrun limit (60 s)
behind its schedule's end and stopped there (`stopped_by: overrun_limit`), with 479,481,119 and 419,590,582 records
unsent; its rows describe what was sent.

Cells are ext4 / xfs, with the range over the two repeats (the probe ran once); units, "to", late sends, CPU, "pause"
and "to durable" as in the first session. The final checkpoint of each run is blocking. Where the ends of a range differ
by more than about 10 %, the two repeats differed that much.

**Writer schedule and CPU (1,024 B):**

| Scenario | Arch | Rate (records/s) | Achieved (records/s) | Late sends (%) | Max schedule delay | Writer CPU (process) | Writer CPU (measuring thread) |
|---|---|---|---|---|---|---|---|
| probe (unpaced) | x86-64 | unpaced | 346,239 / 345,942 | – | – | 0.34 / 0.29 | 0.25 / 0.22 |
| probe (unpaced) | aarch64 | unpaced | 338,412 / 262,421 | – | – | 0.25 / 0.17 | 0.20 / 0.14 |
| steady-50 | x86-64 | 173,119 / 172,970 | 173,119 / 172,970 | 1.1–1.2 / 1.5–1.7 | 21–24 ms / 0.92–1.70 ms | 1.14–1.16 / 1.17 | 1.00 / 1.00 |
| steady-50 | aarch64 | 169,206 / 131,210 | 169,206 / 131,210 | 0.7 / 0.5 | 1.24–2.7 ms / 0.29–1.93 ms | 1.08 / 1.06 | 1.00 / 1.00 |
| steady-90 | x86-64 | 311,614 / 311,347 | 311,614 / 311,347 | 2.4–2.5 / 2.7–2.9 | 13.0–30 ms / 8.3–27 ms | 1.31 / 1.32–1.35 | 1.00 / 1.00 |
| steady-90 | aarch64 | 304,571 / 236,178 | 304,571 / 236,178 | 6.9 / 0.7 | 112–128 ms / 122–148 ms | 1.09 / 1.09–1.10 | 0.95 / 1.00 |
| bursty | x86-64 | 138,495 / 138,376 | 138,677 / 138,558 | 29.1–32.2 / 30.4–31.0 | 71–125 ms / 7.8–45 ms | 1.15 / 1.15–1.16 | 1.00 / 1.00 |
| bursty | aarch64 | 135,364 / 104,968 | 135,542 / 105,106 | 2.6–2.9 / 0.9 | 13.5–26 ms / 6.9–11.6 ms | 1.06 / 1.05 | 1.00 / 1.00 |
| visibility-only | x86-64 | 173,119 / 172,970 | 173,119 / 172,970 | 1.3–1.5 / 1.5–1.8 | 1.63–16.1 ms / 1.45–1.66 ms | 1.15 / 1.16–1.17 | 1.00 / 1.00 |
| visibility-only | aarch64 | 169,206 / 131,210 | 169,206 / 131,210 | 0.8 / 0.6 | 1.22–1.27 ms / 1.02–1.50 ms | 1.07–1.08 / 1.05 | 1.00 / 1.00 |

**Append latency (1,024 B, every append timed, µs):**

| Scenario | Arch | p50 | p99 | p99.9 | p99.99 | max |
|---|---|---|---|---|---|---|
| probe (unpaced) | x86-64 | 0.51 / 0.45 | 1.96 / 1.79 | 9.2 / 4.3 | 11.9 ms / 14.0 ms | 370 ms / 100 ms |
| probe (unpaced) | aarch64 | 0.33 / 0.30 | 2.0 / 1.99 | 11.2 / 3.6 | 13.6 ms / 15.6 ms | 372 ms / 86 ms |
| steady-50 | x86-64 | 0.64 / 0.54–0.55 | 2.1–2.5 / 1.96–2.2 | 6.4–7.9 / 6.0–6.1 | 25–28 / 17.6–18.8 | 7.3–24 ms / 0.92–1.15 ms |
| steady-50 | aarch64 | 0.54 / 0.52 | 2.8 / 2.5 | 7.8–8.1 / 9.2–9.6 | 31–33 / 30–31 | 1.24–1.34 ms / 0.27–1.93 ms |
| steady-90 | x86-64 | 0.59–0.61 / 0.52 | 2.4–2.6 / 2.1–2.3 | 6.0–6.2 / 5.7–5.9 | 8.8–9.3 / 8.5–9.1 | 12.9–14.6 ms / 2.8–27 ms |
| steady-90 | aarch64 | 0.39 / 0.37–0.38 | 2.4 / 2.2–2.3 | 4.2 / 4.1–5.8 | 26–28 / 25–26 | 112–128 ms / 122–149 ms |
| bursty | x86-64 | 0.60–0.61 / 0.53–0.54 | 2.4–2.8 / 2.6 | 6.4–7.0 / 7.1–7.4 | 11.1–11.7 / 41–58 | 71–125 ms / 7.6–45 ms |
| bursty | aarch64 | 0.36–0.37 / 0.34 | 2.2–2.3 / 2.2 | 2.8–3.0 / 2.7–2.8 | 5.6–6.0 / 17.0–17.1 | 13.5–26 ms / 6.9–11.5 ms |
| visibility-only | x86-64 | 0.54–0.56 / 0.43 | 2.2 / 2.0 | 6.1 / 5.7–5.9 | 22–23 / 16.3–16.5 | 1.42–7.4 ms / 1.44–1.66 ms |
| visibility-only | aarch64 | 0.33 / 0.30 | 2.4–2.5 / 2.2 | 6.5 / 8.0 | 28–31 / 28 | 1.17–1.25 ms / 1.01–1.42 ms |

**Checkpoints and rolls (1,024 B).**

| Scenario | Arch | Pause p50 | Pause p99 | Pause max | To durable p50 | To durable p99 | To durable max | Rolls | Roll p50 | Roll max |
|---|---|---|---|---|---|---|---|---|---|---|
| probe (unpaced) | x86-64 | 277 ms / 26 ms | 370 ms / 100 ms | 370 ms / 100 ms | 67 s / 73 s | 93 s / 104 s | 94 s / 107 s | 1,646 / 1,645 | 3.3 ms / 763 | 370 ms / 100 ms |
| probe (unpaced) | aarch64 | 292 ms / 28 ms | 372 ms / 86 ms | 372 ms / 86 ms | 35 s / 49 s | 49 s / 97 s | 51 s / 99 s | 1,609 / 1,248 | 2.2 ms / 613 | 287 ms / 36 ms |
| steady-50 | x86-64 | 77–80 / 90–91 | 214–243 / 245–248 | 0.67–1.41 ms / 315–644 | 1.45–1.48 ms / 1.57–1.96 ms | 3.0–4.0 ms / 3.3–3.7 ms | 4.2–16.6 ms / 3.6–4.2 ms | 837 / 836 | 226–228 / 139–148 | 0.40–2.9 ms / 315–411 |
| steady-50 | aarch64 | 42–50 / 48–53 | 92–233 / 227–235 | 256–357 / 0.27–1.93 ms | 868–919 / 0.93–4.5 ms | 1.15–1.27 ms / 3.8 ms to 1.55 s | 1.23–5.5 ms / 0.53–3.9 s | 818 / 634 | 274–278 / 151–158 | 376–470 / 267–303 |
| steady-90 | x86-64 | 93 / 102–107 | 162–294 / 178–193 | 1.17–2.1 ms / 359–905 | 1.57–1.67 ms / 2.0–2.6 ms | 3.2–4.0 ms / 3.9–4.9 ms | 4.3–166 ms / 27–225 ms | 1,506 / 1,505 | 220–221 / 131–163 | 1.17–2.6 ms / 0.36–1.05 ms |
| steady-90 | aarch64 | 3.4–3.8 ms / 60–140 | 100 ms / 232 | 112–128 ms / 0.28–18.4 ms | 331–342 ms / 0.99–274 ms | 618–719 ms / 0.84–1.74 s | 720–838 ms / 1.24–2.6 s | 1,472 / 1,142 | 1.20–1.27 ms / 152 | 35–68 ms / 1.28–10.0 ms |
| bursty | x86-64 | 70–75 / 59–66 | 1.26–1.93 ms / 199–250 | 5.8–34 ms / 0.41–1.02 ms | 2.6–3.3 ms / 2.0–2.7 ms | 27–277 ms / 6.8–7.4 ms | 408–427 ms / 8.6–9.9 ms | 669 / 669 | 0.60–1.23 ms / 141–155 | 71–125 ms / 7.6–7.8 ms |
| bursty | aarch64 | 53–59 / 73–137 | 253–272 / 181–189 | 1.89–26 ms / 1.60–11.5 ms | 1.06–1.09 ms / 12.6–25 ms | 2.6–5.3 ms / 333–396 ms | 157–255 ms / 353–406 ms | 654 / 507 | 1.08–1.24 ms / 150 | 6.0–6.2 ms / 0.38–11.5 ms |
| visibility-only | x86-64 | – | – | – | – | – | – | 837 / 836 | 1.01–1.02 ms / 0.97–1.05 ms | 1.28–1.37 ms / 1.32–1.44 ms |
| visibility-only | aarch64 | – | – | – | – | – | – | 818 / 634 | 964–970 / 822 | 1.17–1.25 ms / 1.01–1.42 ms |

**Readers (1,024 B).**

| Scenario | Arch | Reader | p50 | p99 | p99.9 | p99.99 | max | Lag max (records) | CPU |
|---|---|---|---|---|---|---|---|---|---|
| probe (unpaced) | x86-64 | mapped, spin | 0.90 / 0.82 | 218 / 245 | 461 / 493 | 12.0 ms / 14.0 ms | 370 ms / 101 ms | 1,266 / 1,568 | 1.00 / 1.00 |
| probe (unpaced) | x86-64 | safe, default | 5.6 / 5.8 | 28 / 35 | 98 / 73 | 12.0 ms / 14.0 ms | 370 ms / 100 ms | 241 / 124 | 0.25 / 0.22 |
| probe (unpaced) | aarch64 | mapped, spin | 0.52 / 0.49 | 328 / 338 | 690 / 690 | 13.6 ms / 15.6 ms | 372 ms / 86 ms | 1,424 / 1,188 | 1.00 / 1.00 |
| probe (unpaced) | aarch64 | safe, default | 5.1 / 5.2 | 14.6 / 16.1 | 31 / 32 | 13.6 ms / 15.6 ms | 372 ms / 86 ms | 45 / 118 | 0.20 / 0.14 |
| steady-50 | x86-64 | mapped, spin | 0.89 / 0.82–0.83 | 3.2–3.5 / 3.0–3.4 | 108–178 / 98–144 | 732–827 / 706–757 | 21–24 ms / 1.76–2.1 ms | 113–117 / 126–128 | 1.00 / 1.00 |
| steady-50 | x86-64 | safe, default | 2.6 / 2.6–2.7 | 5.1–6.1 / 4.9–5.0 | 24–26 / 15.9–16.8 | 285–287 / 166–179 | 21–24 ms / 1.28–1.72 ms | 1–3 / 0–3 | 1.00 / 1.00 |
| steady-50 | x86-64 | safe, durable-only, block | 522 ms / 523–524 ms | 992 ms / 992 ms | 1.00 s / 1.00 s | 1.00 s / 1.00 s | 1.00–1.02 s / 1.00 s | 173,695–174,591 / 173,439–173,567 | 1.00 / 1.00 |
| steady-50 | aarch64 | mapped, spin | 0.57 / 0.54 | 2.9 / 2.3 | 244–256 / 31 | 887–896 / 733–742 | 1.24–2.7 ms / 0.95–1.93 ms | 134–153 / 93–102 | 1.00 / 1.00 |
| steady-50 | aarch64 | safe, default | 2.8 / 2.8 | 5.8–6.1 / 5.1 | 27–28 / 21–22 | 259–274 / 123–124 | 1.27–2.7 ms / 0.31–1.95 ms | 0–4 / 0 | 1.00 / 1.00 |
| steady-50 | aarch64 | safe, durable-only, block | 522 ms / 519–530 ms | 991 ms / 0.99–2.3 s | 1.00 s / 1.00–4.3 s | 1.00 s / 1.47–4.8 s | 1.00–1.01 s / 1.52–4.8 s | 169,343–169,599 / 199,807–634,367 | 1.00 / 1.00 |
| steady-90 | x86-64 | mapped, spin | 0.89–0.91 / 0.82–0.83 | 3.9–4.0 / 3.5 | 399–436 / 307–381 | 0.76–8.2 ms / 656–744 | 13.0–30 ms / 8.3–27 ms | 197–214 / 170–214 | 1.00 / 1.00 |
| steady-90 | x86-64 | safe, default | 2.6 / 2.6–2.7 | 6.6–6.8 / 6.0–6.2 | 69–89 / 45–61 | 0.30–8.4 ms / 230–279 | 13.0–31 ms / 9.1–27 ms | 2–288 / 80–123 | 1.00 / 1.00 |
| steady-90 | x86-64 | safe, durable-only, block | 541 ms / 542–545 ms | 993 ms / 994 ms | 1.00 s / 1.00 s | 1.00–1.11 s / 1.00–1.17 s | 1.00–1.17 s / 1.03–1.22 s | 312,831–363,519 / 317,951–378,879 | 1.00 / 1.00 |
| steady-90 | aarch64 | mapped, spin | 0.57 / 0.54 | 47–50 ms / 2.2–2.3 | 80–82 ms / 439–489 | 99–109 ms / 70–96 ms | 112–128 ms / 122–149 ms | 958–985 / 149–179 | 1.00 / 1.00 |
| steady-90 | aarch64 | safe, default | 2.8 / 2.8 | 47–50 ms / 5.3 | 80–82 ms / 27 | 99–109 ms / 70–96 ms | 112–128 ms / 122–149 ms | 2,791–4,931 / 30–56 | 0.95 / 1.00 |
| steady-90 | aarch64 | safe, durable-only, block | 866–883 ms / 597–821 ms | 1.48–1.50 s / 1.57–2.4 s | 1.58–1.61 s / 1.83–3.1 s | 1.66–1.70 s / 2.2–3.5 s | 1.72 s / 2.2–3.6 s | 520,959 / 523,263–845,311 | 0.96 / 1.00 |
| bursty | x86-64 | mapped, spin | 0.98–1.00 / 0.90–0.92 | 1.38–1.44 ms / 652–679 | 17.3–26 ms / 2.8–17.5 ms | 67–118 ms / 5.7–41 ms | 72–125 ms / 8.2–45 ms | 2,457–3,021 / 3,599–3,791 | 1.00 / 1.00 |
| bursty | x86-64 | safe, default | 4.1–4.3 / 4.3–4.7 | 1.27–1.34 ms / 613–637 | 17.3–26 ms / 1.06–17.5 ms | 67–118 ms / 4.1–41 ms | 71–125 ms / 7.8–45 ms | 103–127 / 61–195 | 0.20 / 0.20 |
| bursty | x86-64 | safe, durable-only, block | 922 ms / 920–921 ms | 1.00–1.22 s / 1.00 s | 1.31–1.39 s / 1.00 s | 1.40–1.42 s / 1.01 s | 1.41–1.43 s / 1.01 s | 276,991 / 141,311–142,847 | 0.20 / 0.20 |
| bursty | aarch64 | mapped, spin | 0.57 / 0.54 | 472–665 / 2.5–2.6 | 2.9–3.3 ms / 632–646 | 8.0–19.8 ms / 1.61–3.6 ms | 13.6–26 ms / 6.9–12.2 ms | 867–1,171 / 355–380 | 1.00 / 1.00 |
| bursty | aarch64 | safe, default | 3.8 / 3.7 | 197–374 / 6.4–6.7 | 2.6–3.0 ms / 79–85 | 8.0–19.8 ms / 1.68–3.6 ms | 13.6–26 ms / 7.0–11.6 ms | 1–4 / 0–1 | 0.20 / 0.20 |
| bursty | aarch64 | safe, durable-only, block | 919 ms / 950–978 ms | 1.00 s / 1.27–1.34 s | 1.01–1.16 s / 1.33–1.39 s | 1.07–1.25 s / 1.34–1.40 s | 1.09–1.26 s / 1.35–1.41 s | 189,439–270,847 / 210,047 | 0.20 / 0.20–0.21 |
| visibility-only | x86-64 | mapped, spin | 0.93–0.95 / 0.81–0.83 | 3.3 / 2.8–3.1 | 418–441 / 372–441 | 1.01–1.03 ms / 0.96–1.04 ms | 1.63–17.1 ms / 1.46–1.66 ms | 0–142 / 0 | 1.00 / 1.00 |
| visibility-only | x86-64 | safe, default | 2.6 / 2.6–2.7 | 5.6–5.7 / 5.3–5.6 | 427–450 / 383–453 | 1.02–1.05 ms / 0.98–1.05 ms | 1.64–16.1 ms / 1.47–1.68 ms | 1–191 / 1 | 1.00 / 1.00 |
| visibility-only | aarch64 | mapped, spin | 0.57 / 0.53–0.54 | 2.8–2.9 / 2.2 | 324–335 / 31 | 940–948 / 775–779 | 1.22–1.28 ms / 1.03–1.50 ms | 0 / 0 | 1.00 / 1.00 |
| visibility-only | aarch64 | safe, default | 2.8 / 2.8 | 5.6–5.9 / 5.2 | 335–345 / 36 | 947–956 / 782–784 | 1.23–1.30 ms / 1.40–1.50 ms | 0 / 0–1 | 1.00 / 1.00 |

**Writer schedule and CPU (64 B):**

| Scenario | Arch | Rate (records/s) | Achieved (records/s) | Late sends (%) | Max schedule delay | Writer CPU (process) | Writer CPU (measuring thread) |
|---|---|---|---|---|---|---|---|
| probe (unpaced) | x86-64 | unpaced | 3,207,061 / 3,455,547 | – | – | 1.29 / 1.17 | 0.99 / 0.98 |
| probe (unpaced) | aarch64 | unpaced | 3,345,229 / 3,252,565 | – | – | 1.10 / 1.10 | 0.94 / 0.99 |
| steady-50 | x86-64 | 1,603,530 / 1,727,773 | 1,603,530 / 1,727,773 | 11.1–11.7 / 11.2–11.7 | 5.0–5.9 ms / 1.56–2.0 ms | 1.14–1.15 / 1.17 | 1.00 / 1.00 |
| steady-50 | aarch64 | 1,672,614 / 1,626,282 | 1,672,614 / 1,626,282 | 2.9–3.1 / 2.2–2.3 | 0.63–2.3 ms / 2.6–5.5 ms | 1.08–1.09 / 1.07 | 1.00 / 1.00 |
| steady-90 | x86-64 | 2,886,355 / 3,109,992 | 2,886,180–2,886,260 / 3,109,990 | 85.5–94.5 / 61.4–63.8 | 44–514 ms / 28–56 ms | 1.26–1.27 / 1.29 | 1.00 / 1.00 |
| steady-90 | aarch64 | 3,010,706 / 2,927,308 | 3,010,499–3,010,524 / 1,949,485–2,038,906 | 71.7–75.9 / 99.6–99.9 | 49–51 ms / 203–224 s | 1.13–1.14 / 0.80–0.82 | 1.00 / 0.76–0.78 |
| bursty | x86-64 | 1,282,824 / 1,382,218 | 1,283,997–1,284,050 / 1,383,566–1,383,576 | 100.0 / 100.0 | 281–288 ms / 232–243 ms | 1.12 / 1.14 | 1.00 / 1.00 |
| bursty | aarch64 | 1,338,091 / 1,301,026 | 1,339,375–1,339,379 / 1,302,332–1,302,372 | 100.0 / 100.0 | 258–260 ms / 367–408 ms | 1.06–1.07 / 1.05–1.06 | 1.00 / 1.00 |
| visibility-only | x86-64 | 1,603,530 / 1,727,773 | 1,603,530 / 1,727,773 | 9.2–9.3 / 6.5–8.8 | 16.5–20 ms / 1.27–28 ms | 1.14–1.15 / 1.16–1.17 | 1.00 / 1.00 |
| visibility-only | aarch64 | 1,672,614 / 1,626,282 | 1,672,614 / 1,626,282 | 1.7 / 1.2–1.3 | 1.14–1.17 ms / 1.17–1.21 ms | 1.07–1.08 / 1.06 | 1.00 / 1.00 |

**Append latency (64 B, every append timed, µs):**

| Scenario | Arch | p50 | p99 | p99.9 | p99.99 | max |
|---|---|---|---|---|---|---|
| probe (unpaced) | x86-64 | 0.18 / 0.12 | 1.40 / 0.54 | 1.90 / 1.85 | 6.8 / 5.7 | 132 ms / 74 ms |
| probe (unpaced) | aarch64 | 0.14 / 0.13 | 0.23 / 0.21 | 1.86 / 1.75 | 2.9 / 2.5 | 219 ms / 8.1 ms |
| steady-50 | x86-64 | 0.24–0.25 / 0.20 | 1.41–1.44 / 1.43–1.44 | 2.1 / 1.91–1.96 | 6.7–7.0 / 6.5–6.7 | 2.3–5.8 ms / 0.69–1.60 ms |
| steady-50 | aarch64 | 0.15 / 0.14 | 0.65–0.66 / 0.63–0.64 | 2.4 / 2.2 | 4.0–4.2 / 3.9–4.0 | 0.61–1.40 ms / 2.6–5.5 ms |
| steady-90 | x86-64 | 0.20–0.21 / 0.13 | 1.37–1.47 / 1.39–1.40 | 2.0–2.1 / 1.85–2.0 | 6.7–7.1 / 6.3–6.5 | 6.8–19.7 ms / 23–24 ms |
| steady-90 | aarch64 | 0.14 / 0.13 | 0.61–0.62 / 0.56–0.59 | 2.1–2.2 / 1.96–2.0 | 3.5–3.6 / 3.6–3.7 | 7.7–15.5 ms / 68–288 ms |
| bursty | x86-64 | 0.21 / 0.12 | 1.40–1.41 / 1.36 | 1.99–2.1 / 1.89–1.98 | 6.6–6.9 / 6.4–6.5 | 4.3–12.5 ms / 8.1–21 ms |
| bursty | aarch64 | 0.14 / 0.13 | 0.57–0.63 / 0.54–0.56 | 2.2 / 1.99–2.0 | 3.6 / 3.2–3.3 | 7.7–8.9 ms / 26–45 ms |
| visibility-only | x86-64 | 0.21 / 0.12 | 1.32–1.33 / 1.33–1.35 | 1.93 / 1.81–1.87 | 6.3 / 5.9–6.2 | 12.3–13.2 ms / 1.23–10.2 ms |
| visibility-only | aarch64 | 0.14 / 0.13 | 0.23 / 0.21 | 2.0–2.1 / 1.83–1.88 | 3.5 / 3.0–3.3 | 1.11–1.12 ms / 1.12–1.17 ms |

**Checkpoints and rolls (64 B).**

| Scenario | Arch | Pause p50 | Pause p99 | Pause max | To durable p50 | To durable p99 | To durable max | Rolls | Roll p50 | Roll max |
|---|---|---|---|---|---|---|---|---|---|---|
| probe (unpaced) | x86-64 | 1.26 ms / 1.85 ms | 114 ms / 74 ms | 132 ms / 74 ms | 230 ms / 27 s | 570 ms / 51 s | 592 ms / 53 s | 1,491 / 1,606 | 102 / 109 | 4.5 ms / 74 ms |
| probe (unpaced) | aarch64 | 1.09 ms / 165 | 142 ms / 8.1 ms | 219 ms / 8.1 ms | 548 ms / 1.01 s | 939 ms / 11.8 s | 1.04 s / 14.6 s | 1,555 / 1,512 | 198 / 157 | 135 ms / 8.1 ms |
| steady-50 | x86-64 | 71–78 / 85–86 | 151–229 / 230–243 | 324–453 / 268–557 | 1.41–2.4 ms / 1.83–2.0 ms | 3.6–5.9 ms / 3.7–4.4 ms | 4.2–6.8 ms / 4.0–5.2 ms | 757 / 816 | 189–215 / 123–127 | 453–503 / 447–557 |
| steady-50 | aarch64 | 49–61 / 50–52 | 113–163 / 167–229 | 453–529 / 2.6–2.9 ms | 917–978 / 0.85–1.29 ms | 1.19–1.29 ms / 0.24–1.14 s | 1.26–1.93 ms / 0.28–1.44 s | 790 / 768 | 262–275 / 150–156 | 603–607 / 2.6–2.9 ms |
| steady-90 | x86-64 | 95–837 / 94–96 | 3.5–4.4 ms / 2.0–2.9 ms | 5.9–6.8 ms / 3.4–6.6 ms | 1.76–22 ms / 6.4–7.5 ms | 40–63 ms / 32–79 ms | 60–79 ms / 103–440 ms | 1,364 / 1,469 | 189–192 / 101–106 | 4.1–4.2 ms / 833–882 |
| steady-90 | aarch64 | 1.13–1.19 ms / 24–27 ms | 6.7–6.9 ms / 68–288 ms | 7.7–8.3 ms / 68–288 ms | 37–50 ms / 58–62 s | 61–62 ms / 96–121 s | 64–66 ms / 99–127 s | 1,423 / 1,012–1,058 | 295–337 / 160–162 | 5.9–15.5 ms / 68–288 ms |
| bursty | x86-64 | 100–116 / 70–82 | 2.5–2.6 ms / 178 | 4.3–4.4 ms / 6.8–8.1 ms | 2.9–4.2 ms / 58 ms | 39–44 ms / 70–71 ms | 45–68 ms / 73–76 ms | 606 / 653 | 197–220 / 105–115 | 3.9–4.2 ms / 524–701 |
| bursty | aarch64 | 1.30–1.32 ms / 138–142 | 6.5–6.9 ms / 2.3–4.6 ms | 7.7–8.9 ms / 26–45 ms | 36–38 ms / 75–77 ms | 50–51 ms / 708–977 ms | 53 ms / 0.79–1.42 s | 632 / 614 | 1.13–1.17 ms / 147–154 | 4.2–5.3 ms / 349–430 |
| visibility-only | x86-64 | – | – | – | – | – | – | 757 / 816 | 1.03 ms / 982–996 | 1.30–5.1 ms / 1.19–1.23 ms |
| visibility-only | aarch64 | – | – | – | – | – | – | 790 / 768 | 957 / 807–821 | 1.11–1.12 ms / 1.12–1.17 ms |

**Readers (64 B).**

| Scenario | Arch | Reader | p50 | p99 | p99.9 | p99.99 | max | Lag max (records) | CPU |
|---|---|---|---|---|---|---|---|---|---|
| probe (unpaced) | x86-64 | mapped, spin | 0.34 / 0.26 | 1.77 / 1.55 | 183 / 347 | 477 / 476 | 132 ms / 75 ms | 5,775 / 8,151 | 1.00 / 1.00 |
| probe (unpaced) | x86-64 | safe, default | 4.8 / 4.3 | 9.4 / 6.7 | 51 / 11.5 | 85 / 222 | 132 ms / 74 ms | 5,663 / 9,223 | 0.99 / 0.97 |
| probe (unpaced) | aarch64 | mapped, spin | 0.28 / 0.26 | 1.28 / 0.86 | 483 / 370 | 646 / 547 | 219 ms / 8.7 ms | 2,341 / 1,941 | 1.00 / 1.00 |
| probe (unpaced) | aarch64 | safe, default | 5.3 / 5.0 | 8.4 / 7.8 | 11.4 / 10.0 | 2.2 ms / 36 | 219 ms / 8.2 ms | 13,351 / 12 | 0.94 / 0.99 |
| steady-50 | x86-64 | mapped, spin | 0.42 / 0.34 | 127–132 / 104–119 | 194–205 / 180–191 | 728–802 / 635–641 | 5.0–8.6 ms / 1.57–2.1 ms | 903–11,391 / 754–920 | 1.00 / 1.00 |
| steady-50 | x86-64 | safe, default | 3.1–3.3 / 3.2 | 129–134 / 107–121 | 176–188 / 161–177 | 271–287 / 209–285 | 6.0–6.8 ms / 1.87–2.3 ms | 3–60 / 11–1,397 | 1.00 / 1.00 |
| steady-50 | x86-64 | safe, durable-only, block | 589–596 ms / 598–599 ms | 994–995 ms / 994 ms | 1.00 s / 1.00 s | 1.00 s / 1.00 s | 1.00–1.01 s / 1.00–1.01 s | 1,608,703–1,611,775 / 1,732,607–1,734,655 | 1.00 / 1.00 |
| steady-50 | aarch64 | mapped, spin | 0.31 / 0.30 | 3.2–5.9 / 2.5–2.6 | 334–380 / 225–238 | 871–908 / 773–783 | 1.32–2.3 ms / 3.4–5.5 ms | 1,316–1,499 / 1,168–1,190 | 1.00 / 1.00 |
| steady-50 | aarch64 | safe, default | 3.7 / 3.8 | 7.6–8.7 / 7.1–7.4 | 47–63 / 39 | 267–293 / 171–181 | 0.68–2.3 ms / 2.7–5.5 ms | 5–19 / 2–4 | 1.00 / 1.00 |
| steady-50 | aarch64 | safe, durable-only, block | 613–614 ms / 632–635 ms | 994 ms / 1.08–1.78 s | 1.00 s / 1.21–2.2 s | 1.00 s / 1.25–2.4 s | 1.00 s / 1.28–2.4 s | 1,674,239 / 2,074,623–3,958,783 | 1.00 / 1.00 |
| steady-90 | x86-64 | mapped, spin | 5.6–27 ms / 41–54 | 28–454 ms / 0.83–2.1 ms | 35–494 ms / 4.7–27 ms | 40–509 ms / 24–50 ms | 44–514 ms / 29–56 ms | 901–1,890 / 669–1,626 | 1.00 / 1.00 |
| steady-90 | x86-64 | safe, default | 5.6–27 ms / 45–59 | 28–454 ms / 0.84–2.1 ms | 35–494 ms / 4.7–27 ms | 40–509 ms / 24–50 ms | 44–514 ms / 29–56 ms | 585–1,515 / 621–5,987 | 1.00 / 1.00 |
| steady-90 | x86-64 | safe, durable-only, block | 685–813 ms / 683–686 ms | 1.02–1.38 s / 1.00–1.01 s | 1.05–1.48 s / 1.02–1.17 s | 1.07–1.53 s / 1.09–1.40 s | 1.10–1.55 s / 1.13–1.44 s | 3,057,663–3,190,783 / 3,446,783–4,292,607 | 1.00 / 1.00 |
| steady-90 | aarch64 | mapped, spin | 7.9–11.4 ms / 43–122 s | 38–45 ms / 201–219 s | 43–49 ms / 203–223 s | 46–50 ms / 203–224 s | 49–51 ms / 203–224 s | 1,962–2,387 / 1,336–1,436 | 1.00 / 1.00 |
| steady-90 | aarch64 | safe, default | 7.9–11.4 ms / 43–122 s | 38–45 ms / 201–219 s | 43–49 ms / 203–223 s | 46–50 ms / 203–224 s | 49–51 ms / 203–224 s | 14–1,525 / 347–388 | 1.00 / 0.76–0.78 |
| steady-90 | aarch64 | safe, durable-only, block | 772–783 ms / 119–194 s | 1.06–1.08 s / 288–322 s | 1.08–1.09 s / 291–324 s | 1.08–1.09 s / 291–324 s | 1.09–1.10 s / 291–324 s | 3,143,679–3,149,823 / 188,088,319–308,019,199 | 1.00 / 0.61–0.77 |
| bursty | x86-64 | mapped, spin | 102–107 ms / 92–93 ms | 236–248 ms / 203–205 ms | 242–254 ms / 207–210 ms | 246–275 ms / 210–228 ms | 249–288 ms / 213–243 ms | 545–1,338 / 586–613 | 1.00 / 1.00 |
| bursty | x86-64 | safe, default | 102–107 ms / 92–93 ms | 236–248 ms / 203–205 ms | 242–254 ms / 207–210 ms | 246–275 ms / 210–228 ms | 249–288 ms / 213–243 ms | 57–551 / 15–1,571 | 0.44–0.45 / 0.41 |
| bursty | x86-64 | safe, durable-only, block | 981–983 ms / 1.03 s | 1.03 s / 1.06 s | 1.04 s / 1.07 s | 1.04–1.06 s / 1.07–1.08 s | 1.05–1.07 s / 1.07–1.08 s | 1,382,399–1,419,263 / 1,562,623–1,573,887 | 0.44–0.45 / 0.40–0.41 |
| bursty | aarch64 | mapped, spin | 88–89 ms / 100–102 ms | 212 ms / 341–343 ms | 217 ms / 359–361 ms | 219–220 ms / 365–387 ms | 222–223 ms / 367–409 ms | 2,057–2,475 / 1,419–1,858 | 1.00 / 1.00 |
| bursty | aarch64 | safe, default | 88–89 ms / 100–102 ms | 212 ms / 341–343 ms | 217 ms / 359–361 ms | 219–220 ms / 365–387 ms | 222–223 ms / 367–409 ms | 10–15 / 14–2,197 | 0.41 / 0.43 |
| bursty | aarch64 | safe, durable-only, block | 1.02–1.03 s / 1.06–1.08 s | 1.05 s / 1.70–1.96 s | 1.05 s / 1.78–2.3 s | 1.05 s / 1.79–2.4 s | 1.05 s / 1.79–2.4 s | 1,449,983–1,454,079 / 2,603,007–3,440,639 | 0.41 / 0.47–0.48 |
| visibility-only | x86-64 | mapped, spin | 0.43 / 0.33 | 68–72 / 14.5–28 | 612–618 / 559–583 | 1.06 ms / 0.99–1.07 ms | 16.5–20 ms / 1.27–28 ms | 0 / 0 | 1.00 / 1.00 |
| visibility-only | x86-64 | safe, default | 3.2 / 3.1 | 71–75 / 17.8–31 | 618–623 / 567–591 | 1.08–1.09 ms / 1.01–1.09 ms | 17.7–20 ms / 1.43–30 ms | 4–1,839 / 8–4,367 | 1.00 / 1.00 |
| visibility-only | aarch64 | mapped, spin | 0.30 / 0.30 | 2.2–2.3 / 1.71–1.81 | 540–541 / 358–366 | 944 / 796–803 | 1.15–1.32 ms / 1.18–1.25 ms | 0 / 0 | 1.00 / 1.00 |
| visibility-only | aarch64 | safe, default | 3.7 / 3.7–3.8 | 7.3–7.5 / 6.9–7.0 | 546–547 / 365–372 | 957–958 / 805–815 | 1.16–1.18 ms / 1.17–1.23 ms | 4–5 / 3 | 1.00 / 1.00 |

The longest appends of the paced runs, from `series.jsonl` (2026-11-22, UTC), every one of 100 ms or more:
- aarch64 ext4, steady-90 at 1,024 B: appends of 50 ms or more in 148 of the 600 seconds of repeat 1 (11:42:56Z to
  11:52:44Z) and 172 of repeat 2 (13:04:27Z to 13:14:34Z), up to 112 ms (11:48:19Z) and 128 ms (13:04:56Z); none was a
  roll. Their checkpoints took 331–342 ms (p50) to become durable, against 1.57–2.6 ms on x86-64. Not investigated.
- aarch64 xfs, steady-90 at 1,024 B: one append of 149 ms (11:46:01Z) and one of 122 ms (13:08:35Z), not rolls.
- aarch64 xfs, steady-90 at 64 B, repeat 1: roll appends of 288 ms (11:06:15Z) and 115 ms (11:12:48Z).
- x86-64 ext4, bursty at 1,024 B, repeat 1: a roll append of 125 ms (11:59:50Z).

What this session shows (`3fea1c0`):
- **1,024 B, half the probe rate** (steady-50, 131,210–173,119 records/s, 134–177 MB/s of payload): the writer keeps its
  schedule (every scheduled record sent, 0.5–1.7 % of sends late, at most 24 ms behind on x86-64 ext4 and 2.7 ms
  elsewhere); a checkpoint pauses the writer for 42–91 µs (p50) and at most 1.93 ms, and is durable 0.87–4.5 ms (p50)
  after its request, at most 16.6 ms, except on aarch64 xfs, where the slowest took 0.53–3.9 s; the
  visible readers see p99.9 of 15.9–256 µs and at most 24 ms; durable-only readers trail by 0.52–0.53 s (p50).
- **1,024 B, 90 % of the probe** (steady-90, 236,178–311,614 records/s, 242–319 MB/s): every scheduled record was
  sent, 0.7–6.9 % late, at most 148 ms behind. On x86-64 a checkpoint is durable 1.57–2.6 ms (p50) after its request
  (at most 225 ms) and the visible readers' p99.9 is 45–436 µs; on aarch64 the writer is held up for up to 112–149 ms
  (above), checkpoints are durable 0.99–342 ms (p50) and at most 2.6 s after the request, and the readers' p99.9 is up
  to 82 ms.
- **1,024 B bursts** at twice the probe rate for 200 ms of every second: unlike at 64 B, the writer keeps up with most
  of each burst (29–32 % of sends late on x86-64, 0.9–2.9 % on aarch64; at most 125 ms behind), and the readers' p50
  of the visible readers stays at 0.54–4.7 µs.
- **1,024 B unpaced** (the probe, 262,421–346,239 records/s, 269–355 MB/s): checkpoints fall behind. 598 of the 599
  requests found one in flight and joined it; a checkpoint became durable 35–73 s (p50) and at most 107 s after its
  request, the writer was held up for up to 372 ms (rolls on x86-64), and dirty page cache reached 10.6 GB (x86-64) and
  6.2 GB (aarch64). Retention did not keep up while the probes ran (issue #45, below): the 737–1,135 segments the
  results show as removed, for 1,248–1,646 rolls (511 fewer on every VM), were removed by the blocking checkpoint that
  ends a run, which took 0.53–1.44 s and left the journal at its 64 GiB limit. Without
  checkpoints, the comparison's 10-minute probes at `88b3304` reached 0.34–0.35 million records/s on the same machine
  types; here x86-64 and aarch64 ext4 reach 0.34–0.35 million and aarch64 xfs 0.26 million.
- **64 B** (probe 3,207,061–3,455,547 records/s, so the paced rates are 11–15 % above the first session's): at half the
  probe rate (1,603,530–1,727,773 records/s, 103–111 MB/s) the writer keeps its schedule (2.2–11.7 % late, at most
  5.9 ms behind), a checkpoint pauses it for 49–86 µs (p50) and at most 2.9 ms and is durable 0.85–2.4 ms (p50) after
  its request, at most 6.8 ms except on aarch64 xfs (up to 1.44 s); visible readers see p99.9 of 39–380 µs and at most
  8.6 ms. At 90 % of the probe, x86-64 (ext4 and xfs) and aarch64 ext4 sent every scheduled record within 0.01 % of
  the rate although 61–95 % of sends were late (at most 514 ms behind); their checkpoints were durable 1.76–50 ms
  (p50) and at most 440 ms after the request, and the final checkpoint took at most 52 ms (at `279f59a`, at lower
  rates, the same scenario fell 23–52 s behind and three runs failed). aarch64 xfs fell behind and stopped (above):
  checkpoints durable 58–62 s (p50) after the request, the writer 203–224 s behind its schedule. Bursts: every send in
  a burst is late and the backlog (at most 232–408 ms) drains before the next burst, as before.
- **Without checkpoints** (visibility-only), no append took longer than 13.2 ms at 64 B and 7.4 ms at 1,024 B: the
  single stalls of 0.10–0.67 s seen in six of the eight 64 B runs at `279f59a` did not recur.

### Long verified runs (`3fea1c0`, issues #43 and #45)

On the same four VMs, right after the session (2026-11-22, from 13:35 to 14:00 UTC), one run each: an unpaced 1,024 B
writer with a background checkpoint requested every second and retention of 64 GiB, a zero-copy and a copying reader,
memory compaction forced every second, then `tickertape verify` of the journal from the file:

```sh
( while true; do echo 1 | sudo tee /proc/sys/vm/compact_memory > /dev/null; sleep 1; done ) &
tt-sustained run --dir /data/tt/long --out /data/results/long --unpaced --duration 3600s --payload 1024 \
  --checkpoint-every 1s --retain-bytes 68719476736 --readers mapped,safe:wait=default \
  --checkpoint-mode background --cpus 1,2,3,4,5,6 --json
tickertape verify /data/tt/long
```

(`/data2/tt/long` on xfs). **Every run failed: the disk filled.** Planned for 3,600 s, each ended after 1,075–1,304 s
when the writer could not create its next segment, for example on x86-64 ext4
`preallocate segment failed for /data/tt/long/00000000000354464640.tape.tmp: No space left on device (os error 28)`.
`tt-sustained` therefore wrote no `result.json`; the figures below come from the per-second series, the output of
`tickertape verify`, the segment listing and `/proc/vmstat`. Retention had fallen behind: it removed 20 segments
(x86-64) and 35–36 (aarch64) while the writer rolled 2,809–2,972, so the journal grew to 349–368 GiB (`du -sh`) on a
375 GB SSD, against a limit of 64 GiB. The cause (issue #45): at `3fea1c0` a retention run on the checkpoint thread
stopped after one segment whenever a checkpoint was queued (ADR 0044 as first decided). Behind this writer a
checkpoint was always queued and one completed every 24–43 s, so retention removed one segment in the time the writer
filled dozens; the forced compaction played no part. The session's 10-minute probes ran the same command and had the
same shortfall while they ran: the 737–1,135 segments their results show as removed were removed by the single
blocking checkpoint that ends a run, after the journal had grown to about 200 GB, which still fitted the disk. ADR
0044 was amended after these runs (a run now removes as many segments as the checkpoint before it covered, within the
time that checkpoint took); the long runs at the amended code are recorded below (Confirming long runs, `fbe5785`).

| VM | Ran (s) | Records written | Mean rate (records/s) | MB/s of payload | Segments removed / left | Journal at the end | Append max | Reader max (mapped / safe) | `verify` | Pages migrated | Compactions |
|---|---|---|---|---|---|---|---|---|---|---|---|
| x86-64 ext4 | 1,075 | 354,464,640 | 329,735 | 338 | 20 / 2,790 | 349 GiB | 397 ms | 397 ms / 397 ms | 351,941,760 records, no problem | 2,059,684 | 593 |
| x86-64 xfs | 1,134 | 373,133,952 | 329,042 | 337 | 20 / 2,938 | 368 GiB | 239 ms | 240 ms / 239 ms | 370,611,072 records, no problem | 1,790,957 | 2,291 |
| aarch64 ext4 | 1,083 | 356,482,944 | 329,162 | 337 | 36 / 2,790 | 349 GiB | 389 ms | 390 ms / 389 ms | 351,941,760 records, no problem | 2,013,771 | 2,713 |
| aarch64 xfs | 1,304 | 375,026,112 | 287,597 | 294 | 35 / 2,938 | 368 GiB | 183 ms | 183 ms / 183 ms | 370,611,072 records, no problem | 2,135,480 | 3,904 |

"Records written" is the writer's last position; "Pages migrated" and "Compactions" are `pgmigrate_success` and
`compact_success` from `/proc/vmstat` after the run, counted since the VM booted (so they include the session before
it). Reader lag peaked at 1,406–1,726 records for the zero-copy reader and 61–2,025 for the copying reader.

What the runs show, until the disk filled:
- **No reader reported corruption**, and both readers of every run reached the writer's last position with no record
  skipped.
- **`tickertape verify`** of each journal (exit status 0, no problem listed) found every retained record valid and
  the chain intact: its last sequence number is the writer's last record, and the records it counts plus those
  retention removed are every record written.
- So, with the reader's re-read window of #44, a view that lags (`crash-contract.md` → Known risks and limits) stopped
  no reader, and no write was lost, in about 1.5 TB of payload written under forced compaction on aarch64 and x86-64.
  This is the evidence that closed issue #43; the platform mechanism stays unconfirmed.
- Retention did not keep up in these runs (cause above, since addressed and measured in the confirming runs below).

### Confirming long runs (`fbe5785`, issue #45)

After the amendment of ADR 0044 (#48), the run that filled the disk at `3fea1c0`, for the full hour, on two new VMs
(2026-11-22, from 17:36 to 18:37 UTC): x86-64 `c4-standard-16-lssd` with the journal on ext4 (us-east1-b) and
aarch64 `c4a-standard-8-lssd` with the journal on xfs (us-east1-d). They are of the machine types and in the zones of
two of the `3fea1c0` runs, but not the same VMs, and no session ran before them. The command is that of the runs above,
with the same compaction loop; in addition the journal directory was sampled every 30 s while the run lasted:

```sh
( while true; do echo 1 | sudo tee /proc/sys/vm/compact_memory > /dev/null; sleep 1; done ) &
( while true; do echo "$(date -u +%T) $(du -sb /data/tt/long | cut -f1) $(ls /data/tt/long | wc -l)"; sleep 30; done ) &
tt-sustained run --dir /data/tt/long --out /data/results/long --unpaced --duration 3600s --payload 1024 \
  --checkpoint-every 1s --retain-bytes 68719476736 --readers mapped,safe:wait=default \
  --checkpoint-mode background --cpus 1,2,3,4,5,6 --json
tickertape status --json /data/tt/long
tickertape verify /data/tt/long
```

(`/data2/tt/long` on xfs). **Both runs completed** (exit status 0, stopped by the duration after 3,600 s, with a
`result.json`) and both journals verified clean. Latencies in µs unless marked.

| | x86-64 ext4 | aarch64 xfs |
|---|---|---|
| Records written | 1,195,456,588 | 1,174,040,477 |
| Mean rate | 332,071 records/s, 340 MB/s of payload | 326,122 records/s, 334 MB/s of payload |
| Rolls / segments removed by retention / left | 9,476 / 8,965 / 512 | 9,307 / 8,796 / 512 |
| Removed before the final blocking checkpoint (`at_run_end`) | 8,899 | 8,743 |
| Retention errors | none (0 failures, no last error, in every second of the series) | none (as x86-64) |
| Journal size from the first sample above the limit to the end of the run: min / max / mean | 65.1 / 82.3 / 73.0 GiB (115 samples, from 180 s) | 64.3 / 73.1 / 68.6 GiB (114 samples, from 210 s) |
| Journal after the run (`tickertape status`) | 512 segments, 64 GiB, 511 before the checkpoint segment, health ok | as x86-64 |
| Checkpoints requested (while one was in flight) / completed before the final one | 3,599 (3,598) / 73 | 3,599 (3,598) / 133 |
| Request to durable p50 / p90 / p99 / max | 71.2 s / 91.2 s / 98.0 s / 104.4 s | 39.3 s / 50.2 s / 57.5 s / 65.3 s |
| Pause p50 / p99 / max | 320 ms / 399 ms / 399 ms | 26.4 ms / 209 ms / 209 ms |
| Append p50 / p99 / p99.9 / p99.99 / max | 0.51 / 1.97 / 14.4 / 12.8 ms / 399 ms | 0.30 / 1.92 / 3.8 / 14.0 ms / 209 ms |
| Roll p50 / max | 3.8 ms / 399 ms | 175 / 205 ms |
| Mapped reader, spin: p50 / p99 / p99.9 / p99.99 / max; lag max | 0.89 / 247 / 509 / 12.8 ms / 400 ms; 1,850 records | 0.49 / 466 / 803 / 14.0 ms / 209 ms; 1,792 records |
| Safe reader, default wait: p50 / p99 / p99.9 / p99.99 / max; lag max | 5.3 / 26.9 / 63 / 12.8 ms / 399 ms; 2,403 records | 5.3 / 18.2 / 43 / 14.0 ms / 209 ms; 614 records |
| Readers at the end | both read all 1,195,456,588 records: none skipped, none remaining | both read all 1,174,040,477 records: none skipped, none remaining |
| Dirty page cache, max | 10.5 GB | 6.4 GB |
| `tickertape verify` | 512 segments, 64,575,628 records (1,130,880,960 to 1,195,456,587), no problem | 512 segments, 64,477,853 records (1,109,562,624 to 1,174,040,476), no problem |
| Pages migrated / compactions | 2,843,730 / 731 | 3,178,390 / 2,582 |

The journal size is `du -sb` of the journal directory, sampled every 30 s from the start of the run; the statistics
start at the first sample at or above the 64 GiB limit (68,719,476,736 bytes) and end at the last sample before the
run ended. "Pause" and "to durable" are as in the sessions' tables; here a checkpoint was always in flight, for tens
of seconds, so a checkpoint's pause is the longest append in that time and its maximum is the append maximum.
"Removed before the final blocking checkpoint" is where retention stood when the writer stopped sending; the final
checkpoint removed the other 66 and 53 segments, so these runs, unlike the 10-minute probes at `3fea1c0`, kept the
journal near its limit during the run. The dirty page cache is `Dirty` from `/proc/meminfo` (whole
system), sampled every second by the writer. "Pages migrated" and "Compactions" are as in the table above, counted since
the VM booted, which here covers the build and this run. `verify`'s first sequence number is exactly the 8,965 and
8,796 removed segments of 126,144 records each, and its last is the writer's last record.

What the runs show:
- **Retention keeps level with the writer.** It began removing about 200 s in, when the journal first passed its
  limit, removed 8,743–8,899 segments while the writer ran, with no retention error, and the journal stayed between
  64.3 and 82.3 GiB for the rest of the hour. At `3fea1c0` the same command, on other VMs of the same types in the same
  zones, removed 20 (x86-64 ext4) and 35 (aarch64 xfs) segments and ended after 1,075 and 1,304 s with the disk full at
  349 and 368 GiB (above).
- **The journal exceeds its limit by what was written since the last durable checkpoint.** Retention runs after each
  completed checkpoint, which came about every 49 s on x86-64 and 27 s on aarch64; between two of them the journal grows
  by what the writer writes (up to 18.3 GiB over the limit on x86-64, 9.1 GiB on aarch64), and the next run brings it
  back.
- **Durability stays tens of seconds behind** a writer that outruns the disk: a request is durable 39–71 s after it at
  p50 and at most 104 s, as in the unpaced 10-minute probes at `3fea1c0` (35–73 s at p50); requests join the queued
  checkpoint.
- **No reader reported corruption**, both readers of each run reached the writer's last position with no record
  skipped, and `tickertape verify` found every retained record valid and the chain intact, in about 2.4 TB of payload
  written under forced compaction.
- Not measured: an archiving writer (`Action::MoveTo`) under a constant queue of checkpoints, and this run on x86-64
  xfs and aarch64 ext4.

### Reference before the checkpoint work: blocking checkpoints (`29960a3`)

The full matrix, both payloads, on one VM per architecture (ext4 first, then xfs), with `Writer::checkpoint` on the
writer's thread every second and no early write-back (it did not exist yet):

```sh
python3 benches/session.py --journal-dir /data/tt  --out /data/results/before-ext4 --only tickertape \
  --max-run 2400 --cpus 1,2,3,4,5,6 --yes
python3 benches/session.py --journal-dir /data2/tt --out /data/results/before-xfs  --only tickertape \
  --max-run 2400 --cpus 1,2,3,4,5,6 --yes
```

Every run finished. Scenarios as above, plus lagging (0.5R, 16 MiB segments, retention of 10 s of data, a reader delayed
per record to a quarter of the rate and a reader stalled 5 s every 30 s) and beyond-RAM (0.9R long enough to append 1.5
times the RAM, a reader delayed to half the rate). R was lower than in the first session (1.64–1.76 million records/s at
64 B, 0.24–0.25 million at 1,024 B), so the rates of the two sessions differ. "Checkpoint" is the duration of
`checkpoint()`, during which the writer appends nothing.

| Scenario | Payload | Arch | Rate (records/s) | Achieved (records/s) | Late sends (%) | Checkpoint p50 | Checkpoint max | Append p99.99 | Append max | Roll max | Writer CPU (process) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| probe (unpaced) | 64 B | x86-64 | unpaced | 1,663,865 / 1,638,606 | – | 1.02 s / 990 ms | 1.85 s / 1.86 s | 5.2 / 5.2 | 4.5 ms / 49 ms | 1.23 ms / 49 ms | 0.53 / 0.51 |
| probe (unpaced) | 64 B | aarch64 | unpaced | 1,756,578 / 1,714,652 | – | 996 ms / 1.03 s | 1.89 s / 1.87 s | 2.7 / 2.7 | 17.0 ms / 370 | 17.0 ms / 370 | 0.54 / 0.52 |
| steady-50 | 64 B | x86-64 | 831,932 / 819,303 | 831,932 / 819,303 | 31.6–31.7 / 31.2–31.3 | 243 ms / 240 ms | 244–245 ms / 241–242 ms | 5.6 / 5.6 | 0.92–1.43 ms / 594–637 | 0.92–1.43 ms / 594–637 | 0.79 / 0.79 |
| steady-50 | 64 B | aarch64 | 878,289 / 857,325 | 878,289 / 857,325 | 33.3–33.4 / 34.3–41.6 | 258–259 ms / 268–269 ms | 260–261 ms / 0.28–4.4 s | 2.9 / 2.9–3.2 | 478–573 / 336–359 | 478–573 / 336–359 | 0.78–0.79 / 0.72–0.77 |
| steady-90 | 64 B | x86-64 | 1,497,478 / 1,474,745 | 1,414,916–1,415,589 / 1,399,373–1,421,996 | 86.6–86.7 / 84.3–84.6 | 441 ms / 435 ms | 1.27–1.28 s / 1.24–1.54 s | 5.5–5.6 / 5.5–5.6 | 1.44–1.73 ms / 1.77–3.1 ms | 1.19–1.27 ms / 728–805 | 0.60 / 0.59–0.60 |
| steady-90 | 64 B | aarch64 | 1,580,920 / 1,543,186 | 1,446,238–1,508,404 / 1,377,568–1,446,746 | 88.6–89.0 / 89.8–99.1 | 467 ms / 488–527 ms | 1.44–1.51 s / 1.66–8.6 s | 3.0 / 2.9 | 0.65–27 ms / 374–382 | 0.65–27 ms / 374–382 | 0.59–0.61 / 0.50–0.57 |
| bursty | 64 B | x86-64 | 665,545 / 655,442 | 666,186–666,191 / 656,086–656,091 | 100.0 / 100.0 | 193 ms / 191 ms | 195 ms / 192 ms | 5.6–5.8 / 5.5–5.7 | 1.14–1.31 ms / 630–692 | 1.14–1.31 ms / 630–692 | 0.84 / 0.83 |
| bursty | 64 B | aarch64 | 702,631 / 685,860 | 703,308–703,309 / 686,514–686,518 | 100.0 / 100.0 | 206 ms / 213–214 ms | 208 ms / 0.30–1.93 s | 3.0–3.1 / 3.0 | 504–573 / 316–336 | 504–573 / 316–336 | 0.83 / 0.81–0.82 |
| lagging | 64 B | x86-64 | 831,932 / 819,303 | 831,932 / 819,303 | 37.6–37.7 / 36.1 | 291–292 ms / 283–284 ms | 304 ms / 293–294 ms | 5.3–5.4 / 5.3–5.4 | 1.04–1.30 ms / 626–746 | 1.04–1.30 ms / 626–746 | 0.79 / 0.79 |
| lagging | 64 B | aarch64 | 878,289 / 857,325 | 878,289 / 857,325 | 41.7–41.8 / 43.5–47.1 | 321–322 ms / 312–313 ms | 334–336 ms / 6.3–12.7 s | 2.7–2.8 / 2.8 | 377–928 / 81–169 ms | 377–928 / 81–169 ms | 0.79 / 0.73–0.76 |
| visibility-only | 64 B | x86-64 | 831,932 / 819,303 | 831,932 / 819,303 | 2.3–2.4 / 1.2–1.3 | – | – | 5.3–5.4 / 5.3 | 246–290 ms / 104–217 ms | 246–290 ms / 104–217 ms | 1.02 / 1.02–1.03 |
| visibility-only | 64 B | aarch64 | 878,289 / 857,325 | 878,289 / 857,325 | 1.6–1.7 / 0.7–1.1 | – | – | 2.5–2.6 / 2.5 | 263–297 ms / 0.29–2.2 s | 263–297 ms / 0.29–2.2 s | 1.01 / 1.01 |
| beyond-RAM | 64 B | x86-64 | 1,497,478 / 1,474,745 | 1,493,684–1,494,304 / 1,472,430–1,474,745 | 74.0–74.4 / 71.1–73.2 | 441 ms / 435 ms | 1.46–1.48 s / 645–733 ms | 5.3–5.4 / 5.3–5.4 | 1.94–6.4 ms / 1.30–2.8 ms | 1.09–1.25 ms / 701–757 | 0.62 / 0.61 |
| beyond-RAM | 64 B | aarch64 | 1,580,920 / 1,543,186 | 1,577,429–1,580,920 / 1,539,470–1,543,186 | 76.9–77.1 / 85.6–86.1 | 468–470 ms / 489–492 ms | 0.98–1.55 s / 4.9–7.2 s | 2.6–2.7 / 2.6–2.7 | 550–556 / 3.6–5.7 ms | 537–556 / 0.69–5.7 ms | 0.60–0.61 / 0.53 |
| probe (unpaced) | 1,024 B | x86-64 | unpaced | 254,584 / 242,907 | – | 5.3 s / 10.0 s | 23 s / 23 s | 13.8 / 13.9 | 20 ms / 5.8 ms | 1.44 ms / 641 | 0.20 / 0.16 |
| probe (unpaced) | 1,024 B | aarch64 | unpaced | 250,396 / 247,879 | – | 4.1 s / 4.4 s | 11.9 s / 8.9 s | 5.2 / 5.2 | 39 ms / 28 ms | 39 ms / 28 ms | 0.26 / 0.24 |
| steady-50 | 1,024 B | x86-64 | 127,291 / 121,453 | 127,291 / 121,453 | 44.1–44.2 / 39.3–40.9 | 383 ms / 366 ms | 813–814 ms / 580–581 ms | 9.4–12.8 / 7.1–7.4 | 1.47–5.6 ms / 0.83–37 ms | 1.22–1.47 ms / 624–828 | 0.64 / 0.66–0.68 |
| steady-50 | 1,024 B | aarch64 | 125,198 / 123,939 | 125,198 / 123,939 | 42.9–43.6 / 44.5–46.2 | 378–389 ms / 396–400 ms | 593–595 ms / 0.62–4.5 s | 5.2–5.4 / 5.1 | 7.5–165 ms / 0.47–3.3 ms | 7.5–165 ms / 346–467 | 0.65–0.67 / 0.62–0.64 |
| steady-90 | 1,024 B | x86-64 | 229,125 / 218,616 | 209,649–213,517 / 202,472–213,508 | 87.2–87.8 / 84.2–84.9 | 692 ms / 661 ms | 7.1–7.3 s / 7.2–8.1 s | 8.3–9.0 / 7.1 | 87–123 ms / 2.2–2.3 ms | 87–123 ms / 660–693 | 0.33–0.34 / 0.34 |
| steady-90 | 1,024 B | aarch64 | 225,356 / 223,091 | 223,524–225,212 / 219,662–219,876 | 85.2–86.1 / 88.7 | 683–686 ms / 720–730 ms | 7.2–7.9 s / 8.2–8.7 s | 5.0–5.6 / 4.8–5.1 | 535–851 / 0.53–23 ms | 535–646 / 0.41–23 ms | 0.37–0.39 / 0.33 |
| bursty | 1,024 B | x86-64 | 101,833 / 97,162 | 101,944–101,945 / 97,270–97,271 | 99.8 / 99.7–99.8 | 306 ms / 292 ms | 328–329 ms / 330–331 ms | 10.7–11.5 / 8.1–8.5 | 2.3–39 ms / 5.1–33 ms | 1.08–1.22 ms / 556–578 | 0.73 / 0.74 |
| bursty | 1,024 B | aarch64 | 100,158 / 99,151 | 100,264 / 99,254 | 99.8 / 99.8 | 302–303 ms / 315–321 ms | 304–307 ms / 0.33–2.1 s | 6.0–6.1 / 5.4–5.6 | 0.59–20 ms / 355–356 | 519–591 / 355–356 | 0.75 / 0.71–0.72 |
| lagging | 1,024 B | x86-64 | 127,291 / 121,453 | 127,291 / 121,453 | 45.5–45.7 / 42.6–43.3 | 429 ms / 404–405 ms | 438–439 ms / 406–407 ms | 35–39 / 33 | 1.17–1.62 ms / 595–605 | 1.17–1.62 ms / 595–605 | 0.66 / 0.67 |
| lagging | 1,024 B | aarch64 | 125,198 / 123,939 | 125,198 / 123,855 | 44.6–70.2 / 43.7–44.9 | 420–589 ms / 411 ms | 423–808 ms / 0.44–2.3 s | 29–30 / 36 | 365–427 / 317–320 | 365–427 / 317–320 | 0.44–0.68 / 0.67–0.68 |
| visibility-only | 1,024 B | x86-64 | 127,291 / 121,453 | 127,291 / 121,453 | 3.5–4.0 / 1.5–1.6 | – | – | 7.7–7.8 / 6.8–7.3 | 286–361 ms / 207–213 ms | 286–361 ms / 207–213 ms | 1.02–1.03 / 1.03–1.04 |
| visibility-only | 1,024 B | aarch64 | 125,198 / 123,939 | 125,198 / 123,939 | 1.9–2.9 / 0.5–0.7 | – | – | 4.2–4.4 / 4.0–4.1 | 172–254 ms / 187–543 ms | 172–254 ms / 187–543 ms | 1.00–1.01 / 1.01–1.02 |
| beyond-RAM | 1,024 B | x86-64 | 229,125 / 218,616 | 211,210–216,474 / 207,543–209,586 | 82.2–83.5 / 79.9–80.0 | 693–694 ms / 662 ms | 31–33 s / 13.7–22 s | 10.6–13.4 / 9.0–10.7 | 8.6–218 ms / 10.3–38 ms | 1.29–218 ms / 605–712 | 0.31 / 0.32–0.33 |
| beyond-RAM | 1,024 B | aarch64 | 225,356 / 223,091 | 205,061–208,431 / 208,722–211,442 | 83.0–84.2 / 85.4–86.0 | 685–701 ms / 719–727 ms | 12.6–12.8 s / 10.7–11.7 s | 5.1–5.5 / 4.8 | 0.55–7.7 ms / 487–609 | 0.55–7.7 ms / 413–603 | 0.33–0.35 / 0.31 |

**Readers** (the `safe, default` reader of the non-lagging scenarios follows the mapped one and is left out):

| Scenario | Payload | Arch | Reader | p50 | p99.9 | max | Lag max (records) | Records lost to retention |
|---|---|---|---|---|---|---|---|---|
| probe (unpaced) | 64 B | x86-64 | mapped, spin | 0.35 / 0.35 | 515 / 541 | 3.7 ms / 50 ms | 21,295 / 3,615 | 0 / 0 |
| probe (unpaced) | 64 B | aarch64 | mapped, spin | 0.28 / 0.28 | 464 / 476 | 17.6 ms / 1.15 ms | 1,894 / 2,097 | 0 / 0 |
| steady-50 | 64 B | x86-64 | mapped, spin | 0.48–0.49 / 0.49 | 242–243 ms / 239 ms | 244–245 ms / 241–242 ms | 1,367–1,920 / 1,974–1,984 | 0 / 0 |
| steady-50 | 64 B | x86-64 | safe, durable-only, block | 788 ms / 783–785 ms | 1.24 s / 1.24 s | 1.24 s / 1.24 s | 832,511 / 819,711 | 0 / 0 |
| steady-50 | 64 B | aarch64 | mapped, spin | 0.35 / 0.35–0.36 | 258–259 ms / 0.27–4.0 s | 260–262 ms / 0.28–4.4 s | 1,415–1,700 / 2,279–2,459 | 0 / 0 |
| steady-50 | 64 B | aarch64 | safe, durable-only, block | 813–814 ms / 823–888 ms | 1.26 s / 1.27–6.1 s | 1.26 s / 1.28–6.4 s | 878,591 / 857,599–3,110,911 | 0 / 0 |
| steady-90 | 64 B | x86-64 | mapped, spin | 259 ms / 238–243 ms | 35–36 s / 23–33 s | 35–36 s / 23–33 s | 2,907–3,011 / 2,199–2,361 | 0 / 0 |
| steady-90 | 64 B | x86-64 | safe, durable-only, block | 1.18 s / 1.15–1.17 s | 36–37 s / 25–34 s | 37 s / 25–34 s | 2,734,079–2,779,135 / 2,643,967–3,088,383 | 0 / 0 |
| steady-90 | 64 B | aarch64 | mapped, spin | 303 ms / 0.31–28 s | 29–57 s / 40–72 s | 30–57 s / 41–72 s | 1,582–2,349 / 1,624–2,447 | 0 / 0 |
| steady-90 | 64 B | aarch64 | safe, durable-only, block | 1.25 s / 1.26–29 s | 31–58 s / 41–75 s | 31–58 s / 41–76 s | 2,627,583–3,147,775 / 3,092,479–3,397,631 | 0 / 0 |
| bursty | 64 B | x86-64 | mapped, spin | 193–194 ms / 184–189 ms | 212–213 ms / 203–205 ms | 214–215 ms / 206–214 ms | 1,307–2,047 / 2,751–2,979 | 0 / 0 |
| bursty | 64 B | x86-64 | safe, durable-only, block | 1.13 s / 1.13 s | 1.19 s / 1.19 s | 1.19 s / 1.19 s | 665,599 / 655,871 | 0 / 0 |
| bursty | 64 B | aarch64 | mapped, spin | 201 ms / 206 ms | 214–215 ms / 0.30–1.93 s | 215–217 ms / 0.30–2.0 s | 1,857–2,219 / 1,720–1,835 | 0 / 0 |
| bursty | 64 B | aarch64 | safe, durable-only, block | 1.15 s / 1.16 s | 1.21 s / 1.27–3.9 s | 1.21 s / 1.30–3.9 s | 702,975 / 686,079–1,759,231 | 0 / 0 |
| lagging | 64 B | x86-64 | mapped, spin | 0.49 / 0.48 | 296 ms / 283–284 ms | 304 ms / 293–294 ms | 0–770 / 38–265 | 0 / 0 |
| lagging | 64 B | x86-64 | safe, lagging (per-record delay) | 10.9 s / 10.8 s | 11.5 s / 11.5 s | 11.6 s / 11.5–11.6 s | 9,641,983 / 9,453,567–9,461,759 | 376,193,576 / 370,224,810–370,386,128 |
| lagging | 64 B | x86-64 | safe, 5 s stalls every 30 s | 3.7–3.8 / 3.7 | 5.0 s / 5.0 s | 5.0 s / 5.0 s | 4,159,487 / 4,095,999–4,098,047 | 0 / 0 |
| lagging | 64 B | aarch64 | mapped, spin | 0.36 / 0.37–0.39 | 331 ms / 6.5–12.2 s | 334–336 ms / 6.9–12.7 s | 172–255 / 138–211 | 0 / 0 |
| lagging | 64 B | aarch64 | safe, lagging (per-record delay) | 10.9 s / 11.0 s | 11.5 s / 17.9–22 s | 11.6 s / 18.3–22 s | 10,149,887–10,158,079 / 13,066,239–13,320,191 | 397,971,506 / 388,292,426 |
| lagging | 64 B | aarch64 | safe, 5 s stalls every 30 s | 18.6–19.3 ms / 32–66 ms | 5.0 s / 11.4–17.1 s | 5.0 s / 12.0–17.7 s | 4,390,911 / 7,741,439–9,388,031 | 0 / 0 |
| visibility-only | 64 B | x86-64 | mapped, spin | 0.45 / 0.45 | 124–174 ms / 0.91–90 ms | 246–291 ms / 105–217 ms | 716–3,305 / 569–1,349 | 0 / 0 |
| visibility-only | 64 B | aarch64 | mapped, spin | 0.32 / 0.32–0.33 | 128–141 ms / 0.11–1.76 s | 263–298 ms / 0.30–2.2 s | 1,491–1,639 / 962–2,171 | 0 / 0 |
| beyond-RAM | 64 B | x86-64 | mapped, spin | 146–148 ms / 129–140 ms | 1.39–1.50 s / 0.54–1.24 s | 1.69–1.72 s / 0.65–1.41 s | 2,359–2,409 / 2,293–2,525 | 0 / 0 |
| beyond-RAM | 64 B | x86-64 | safe, lagging (per-record delay) | 178 s / 181 s | 344 s / 350 s | 344 s / 350 s | 512,753,663–513,277,951 / 514,850,815–515,375,103 | 0 / 0 |
| beyond-RAM | 64 B | aarch64 | mapped, spin | 167–169 ms / 345–348 ms | 0.81–1.60 s / 7.9–8.2 s | 1.07–1.95 s / 8.1–8.5 s | 1,498–1,970 / 1,985–2,004 | 0 / 0 |
| beyond-RAM | 64 B | aarch64 | safe, lagging (per-record delay) | 183 s / 188–189 s | 352–354 s / 363 s | 353–354 s / 364 s | 556,793,855 / 558,891,007–559,939,583 | 0 / 0 |
| probe (unpaced) | 1,024 B | x86-64 | mapped, spin | 18.9 ms / 36 ms | 106 ms / 167 ms | 109 ms / 171 ms | 470,783 / 715,775 | 0 / 0 |
| probe (unpaced) | 1,024 B | aarch64 | mapped, spin | 0.56 / 0.56 | 545 / 554 | 40 ms / 29 ms | 1,045 / 1,146 | 0 / 0 |
| steady-50 | 1,024 B | x86-64 | mapped, spin | 1.10–1.12 / 1.06–1.08 | 593 ms / 442–573 ms | 813–814 ms / 580–581 ms | 2,863–3,435 / 3,687–19,023 | 0 / 0 |
| steady-50 | 1,024 B | x86-64 | safe, durable-only, block | 897 ms / 880–881 ms | 1.38 s / 1.36 s | 1.38 s / 1.37 s | 128,319–129,087 / 137,087–138,111 | 0 / 0 |
| steady-50 | 1,024 B | aarch64 | mapped, spin | 0.68 / 0.68–0.69 | 583–584 ms / 0.60–4.1 s | 593–595 ms / 0.62–4.5 s | 906–1,083 / 1,040–1,422 | 0 / 0 |
| steady-50 | 1,024 B | aarch64 | safe, durable-only, block | 892–898 ms / 909–951 ms | 1.38–1.39 s / 1.40–6.7 s | 1.38–1.39 s / 1.41–7.0 s | 125,247 / 123,967–620,031 | 0 / 0 |
| steady-90 | 1,024 B | x86-64 | mapped, spin | 5.5–6.1 s / 0.57–2.1 s | 46–59 s / 24–50 s | 47–59 s / 25–51 s | 14,423–14,607 / 11,335–18,175 | 0 / 0 |
| steady-90 | 1,024 B | x86-64 | safe, durable-only, block | 8.9–10.2 s / 1.57–6.0 s | 49–62 s / 30–53 s | 50–62 s / 30–53 s | 1,506,303–1,589,247 / 1,495,039–1,681,407 | 0 / 0 |
| steady-90 | 1,024 B | aarch64 | mapped, spin | 532–554 ms / 604–609 ms | 8.6–9.5 s / 12.1–14.8 s | 8.9–9.9 s / 12.5–15.3 s | 1,090–1,130 / 1,215–1,308 | 0 / 0 |
| steady-90 | 1,024 B | aarch64 | safe, durable-only, block | 1.64–2.3 s / 1.69–2.4 s | 13.3–14.0 s / 16.6–21 s | 13.9–14.4 s / 17.0–21 s | 1,522,687–1,691,647 / 1,655,807–1,748,991 | 0 / 0 |
| bursty | 1,024 B | x86-64 | mapped, spin | 219–220 ms / 216–221 ms | 306 ms / 293 ms | 328–329 ms / 330–331 ms | 5,131–6,919 / 2,763–3,489 | 0 / 0 |
| bursty | 1,024 B | x86-64 | safe, durable-only, block | 1.22 s / 1.20 s | 1.30 s / 1.29 s | 1.31 s / 1.29 s | 104,895–110,399 / 97,215–97,983 | 0 / 0 |
| bursty | 1,024 B | aarch64 | mapped, spin | 224–225 ms / 238–244 ms | 303–304 ms / 0.32–2.1 s | 304–307 ms / 0.33–2.1 s | 969–1,342 / 1,128–1,321 | 0 / 0 |
| bursty | 1,024 B | aarch64 | safe, durable-only, block | 1.21 s / 1.23 s | 1.30 s / 1.32–3.9 s | 1.30–1.31 s / 1.33–4.0 s | 100,159 / 99,199–297,471 | 0 / 0 |
| lagging | 1,024 B | x86-64 | mapped, spin | 1.11–1.12 / 1.07–1.08 | 429–430 ms / 403–404 ms | 438–439 ms / 406–407 ms | 215–438 / 3–559 | 0 / 0 |
| lagging | 1,024 B | x86-64 | safe, lagging (per-record delay) | 10.9 s / 11.0 s | 11.5 s / 11.6 s | 11.6 s / 11.6 s | 1,469,439–1,470,463 / 1,414,143 | 57,076,540 / 54,443,451 |
| lagging | 1,024 B | x86-64 | safe, 5 s stalls every 30 s | 50–52 ms / 23–29 ms | 5.0 s / 5.0 s | 5.0 s / 5.0 s | 636,415 / 607,231 | 0 / 0 |
| lagging | 1,024 B | aarch64 | mapped, spin | 0.68 µs to 188 ms / 0.67–0.68 | 419–804 ms / 0.41–2.0 s | 423–808 ms / 0.44–2.3 s | 18–55 / 31–44 | 0 / 0 |
| lagging | 1,024 B | aarch64 | safe, lagging (per-record delay) | 10.9–11.2 s / 10.9 s | 11.6–11.9 s / 11.5–13.7 s | 11.6–12.0 s / 11.5–13.9 s | 1,453,055–1,472,511 / 1,429,503–1,723,391 | 56,130,520 / 55,578,675 |
| lagging | 1,024 B | aarch64 | safe, 5 s stalls every 30 s | 42–278 ms / 34–47 ms | 5.0 s / 5.0–6.7 s | 5.0 s / 5.0–7.3 s | 626,175 / 620,031–867,839 | 0 / 0 |
| visibility-only | 1,024 B | x86-64 | mapped, spin | 0.93–0.94 / 0.93 | 185–203 ms / 22–24 ms | 287–361 ms / 208–213 ms | 1,645–2,827 / 446–723 | 0 / 0 |
| visibility-only | 1,024 B | aarch64 | mapped, spin | 0.62 / 0.62 | 74–136 ms / 4.4–154 ms | 172–255 ms / 187–544 ms | 1,078–1,330 / 660–753 | 0 / 0 |
| beyond-RAM | 1,024 B | x86-64 | mapped, spin | 473–488 ms / 413–420 ms | 39–56 s / 27–43 s | 39–56 s / 28–43 s | 267,519–337,151 / 159,743–285,951 | 0 / 0 |
| beyond-RAM | 1,024 B | x86-64 | safe, lagging (per-record delay) | 171–177 s / 170–174 s | 340–349 s / 330–343 s | 340–350 s / 330–343 s | 67,436,543–70,123,519 / 66,191,359–68,091,903 | 0 / 0 |
| beyond-RAM | 1,024 B | aarch64 | mapped, spin | 430–444 ms / 457–463 ms | 50–64 s / 37–48 s | 50–65 s / 38–48 s | 1,115–1,368 / 1,440–1,470 | 0 / 0 |
| beyond-RAM | 1,024 B | aarch64 | safe, lagging (per-record delay) | 175–177 s / 173–175 s | 344–354 s / 336–344 s | 344–354 s / 336–344 s | 65,306,623–67,371,007 / 66,322,431–66,846,719 | 0 / 0 |

What this shows against the first session: a blocking checkpoint every second stopped the writer for 240–269 ms at 64 B
and 366–400 ms at 1,024 B at half the probe rate, so 31–46 % of sends were late and the visible readers' p99.9 equalled
the stop (239–259 ms at 64 B, except aarch64 xfs with stops of up to 4.4 s). With background checkpoints at `279f59a`
(steady-50, 64 B, at a rate 1.7–1.9 times higher) the writer paused 41–88 µs (p50) per checkpoint. The lagging reader
with a per-record delay fell 9.4–13.3 million records (64 B) behind and then lost 370–398 million records to retention
(54–57 million at 1,024 B), as designed: it gets `Error::Discarded`, seeks, and counts what it skipped (the tool checks
`read + skipped + remaining = written` for every reader; it held for every reader of every finished run of these
sessions). The stalled reader caught up after each stall and lost nothing. In beyond-RAM the delayed reader read records
3 minutes old from storage (journals of 96–103 GB at 64 B on 63–67 GB machines) and lost nothing.

### Checkpoints (ADR 0038)

All at 1,024 B, a checkpoint every second, at half of a 60-second probe of the same build, with the readers of
steady-50, 128 MiB segments and retention of 64 GiB; ext4 / xfs. Scripts: `ckpt/remote.sh` (prototype) and
`ckpt/remote2.sh` (finish round) of the maintainers' cloud tooling, which run `tt-sustained` as above:

```sh
tt-sustained run --dir … --out … --duration <240s|180s> --warmup 10s --payload 1024 --rate <half> \
  --checkpoint-every 1s --retain-bytes 68719476736 --readers mapped,safe:wait=default,safe:durable:wait=block \
  --cpus 1,2,3,4 --checkpoint-mode <blocking|background> --json
```

"Writer stopped" is `checkpoint()` in blocking mode and `request_checkpoint` in background mode.

**Prototype** (`552ed71`, 240-second runs, 120 s with the checkpoint thread on the writer's CPU; 146,781–156,556
records/s, about 150–160 MB/s; blocking and background twice each, the others once). The library's checkpoint thread
inherited the pinned writer's CPU at this commit; the script moved it to a CPU of its own with `taskset` except in "on
the writer's CPU". "Kernel flusher tuned" sets `vm.dirty_background_bytes=16777216`, `vm.dirty_expire_centisecs=50` and
`vm.dirty_writeback_centisecs=10`, which is what early write-back later did inside the library.

x86-64:

| Configuration | Late sends (%) | Writer stopped p50 | p99 | max | Request to durable p50 | max | Roll max | Mapped reader p99.9 | max | Durable-only reader p50 | max |
|---|---|---|---|---|---|---|---|---|---|---|---|
| blocking | 48.9–49.0 / 48.5–48.6 | 446 ms / 442 ms | 447–448 ms / 445 ms | 447–448 ms / 445 ms | 446 ms / 442 ms | 447–448 ms / 445 ms | 1.01–1.14 ms / 546–572 | 445–446 ms / 442 ms | 447–448 ms / 445 ms | 965 ms / 961 ms | 1.45 s / 1.44 s |
| background, checkpoint thread on its own CPU | 1.9–2.0 / 2.9–3.1 | 3.8–3.9 ms / 2.1–2.2 ms | 61–169 ms / 159–167 ms | 173–174 ms / 168–172 ms | 382–383 ms / 430 ms | 737–741 ms / 859–860 ms | 173–174 ms / 168–172 ms | 63–99 ms / 131–138 ms | 174 ms / 169–173 ms | 967–968 ms / 1.01 s | 1.74 s / 1.86 s |
| background, checkpoint thread on the writer's CPU | 9.2 / 9.5 | 22 ms / 22 ms | 340 ms / 356 ms | 341 ms / 359 ms | 384 ms / 385 ms | 727 ms / 737 ms | 341 ms / 359 ms | 314 ms / 326 ms | 342 ms / 360 ms | 965 ms / 963 ms | 1.73 s / 1.74 s |
| blocking, kernel flusher tuned | 5.2 / 5.4 | 31 ms / 36 ms | 49 ms / 57 ms | 53 ms / 58 ms | 31 ms / 36 ms | 53 ms / 58 ms | 47 ms / 4.1 ms | 39 ms / 43 ms | 53 ms / 58 ms | 550 ms / 555 ms | 1.05 s / 1.06 s |
| background, kernel flusher tuned | 1.8 / 1.5 | 64 / 59 | 27 ms / 4.2 ms | 39 ms / 49 ms | 59 ms / 58 ms | 94 ms / 99 ms | 52 ms / 35 ms | 12.7 ms / 2.1 ms | 52 ms / 50 ms | 579 ms / 578 ms | 1.09 s / 1.10 s |
| no checkpoints | 3.7 / 1.4 | no checkpoints | – | – | – | – | 282 ms / 9.0 ms | 249 ms / 4.1 ms | 283 ms / 9.6 ms | – | – |

aarch64:

| Configuration | Late sends (%) | Writer stopped p50 | p99 | max | Request to durable p50 | max | Roll max | Mapped reader p99.9 | max | Durable-only reader p50 | max |
|---|---|---|---|---|---|---|---|---|---|---|---|
| blocking | 50.6 / 52.6 | 472 ms / 491 ms | 473–474 ms / 498 ms | 474 ms / 499 ms | 472 ms / 491 ms | 474 ms / 499 ms | 336–378 / 317–358 | 471 ms / 491–492 ms | 474 ms / 499 ms | 991 ms / 1.01 s | 1.47 s / 1.50 s |
| background, checkpoint thread on its own CPU | 2.1–2.3 / 2.4 | 3.9 ms / 2.3 ms | 152–158 ms / 167–173 ms | 168–169 ms / 180 ms | 379–380 ms / 402–403 ms | 762–764 ms / 798–804 ms | 168–169 ms / 180 ms | 122–123 ms / 133–135 ms | 168–170 ms / 180–181 ms | 990 ms / 1.01 s | 1.76 s / 1.80 s |
| background, checkpoint thread on the writer's CPU | 12.2 / 13.9 | 51 ms / 72 ms | 290 ms / 347 ms | 309 ms / 353 ms | 380 ms / 423 ms | 764 ms / 843 ms | 309 ms / 353 ms | 258 ms / 314 ms | 309 ms / 357 ms | 991 ms / 1.03 s | 1.76 s / 1.84 s |
| blocking, kernel flusher tuned | 4.2 / 4.2 | 32 ms / 35 ms | 47 ms / 53 ms | 49 ms / 54 ms | 32 ms / 35 ms | 49 ms / 54 ms | 45 ms / 4.2 ms | 39 ms / 42 ms | 49 ms / 54 ms | 552 ms / 553 ms | 1.06 s / 1.05 s |
| background, kernel flusher tuned | 0.7 / 0.5 | 52 / 48 | 5.7 ms / 3.6 ms | 18.2 ms / 3.7 ms | 58 ms / 57 ms | 84 ms / 89 ms | 43 ms / 4.2 ms | 9.9 ms / 1.03 ms | 44 ms / 4.8 ms | 578 ms / 577 ms | 1.09 s / 1.09 s |
| no checkpoints | 1.8 / 1.0 | no checkpoints | – | – | – | – | 266 ms / 146 ms | 153 ms / 111 ms | 267 ms / 146 ms | – | – |

**Finish round** (180-second runs, 120 s for the two helper-placement rows, one each; 246,606–255,351 records/s, about
250–260 MB/s): after the changes ADR 0038 → "After the Linux measurement" describes (rolls without a sync on the
writer's thread, early write-back, helper placement). Helper threads on CPUs 5–7 (`--helper-cpus`) unless the row says
otherwise. On x86-64 xfs only the background run has results; the other xfs runs were no longer collected.

x86-64:

| Configuration | Late sends (%) | Writer stopped p50 | p99 | max | Request to durable p50 | max | Roll max | Mapped reader p99.9 | max | Durable-only reader p50 | max |
|---|---|---|---|---|---|---|---|---|---|---|---|
| background | 2.4 / 2.3 | 103 / 104 | 318 / 280 | 342 / 315 | 3.1 ms / 1.64 ms | 5.0 ms / 3.9 ms | 185 / 207 | 234 / 220 | 865 / 786 | 534 ms / 534 ms | 1.01 s / 1.00 s |
| background, early write-back off | 2.2 / not run | 282 / not run | 867 / not run | 1.08 ms / not run | 763 ms / not run | 765 ms / not run | 422 / not run | 355 / not run | 4.7 ms / not run | 1.28 s / not run | 1.76 s / not run |
| blocking | 2.4 / not run | 2.9 ms / not run | 4.1 ms / not run | 4.1 ms / not run | 2.9 ms / not run | 4.1 ms / not run | 198 / not run | 2.2 ms / not run | 4.1 ms / not run | 532 ms / not run | 1.00 s / not run |
| blocking, early write-back off | 89.1 / not run | 745 ms / not run | 746 ms / not run | 746 ms / not run | 745 ms / not run | 746 ms / not run | 174 / not run | 744 ms / not run | 746 ms / not run | 1.28 s / not run | 1.74 s / not run |
| background, helpers on every CPU (default) | 1.7 / not run | 94 / not run | 140 / not run | 215 / not run | 1.68 ms / not run | 4.2 ms / not run | 190 / not run | 225 / not run | 872 / not run | 533 ms / not run | 1.00 s / not run |
| background, helpers inherit the writer's CPU | 2.6 / not run | 73 / not run | 1.94 ms / not run | 2.0 ms / not run | 1.93 ms / not run | 4.1 ms / not run | 186 / not run | 854 / not run | 2.0 ms / not run | 533 ms / not run | 1.00 s / not run |
| no checkpoints | 2.2 / not run | no checkpoints | – | – | – | – | 3.4 ms / not run | 236 / not run | 4.1 ms / not run | – | – |
| no checkpoints, early write-back off | 2.5 / not run | no checkpoints | – | – | – | – | 65 ms / not run | 93 ms / not run | 137 ms / not run | – | – |

aarch64:

| Configuration | Late sends (%) | Writer stopped p50 | p99 | max | Request to durable p50 | max | Roll max | Mapped reader p99.9 | max | Durable-only reader p50 | max |
|---|---|---|---|---|---|---|---|---|---|---|---|
| background | 1.0 / 0.7 | 44 / 49 | 96 / 242 | 97 / 243 | 870 / 754 | 1.17 ms / 1.06 ms | 230 / 217 | 489 / 479 | 10.1 ms / 10.1 ms | 533 ms / 532 ms | 1.00 s / 1.00 s |
| background, early write-back off | 0.4 / 0.3 | 191 / 164 | 1.01 ms / 201 | 3.6 ms / 206 | 760 ms / 794 ms | 1.14 s / 1.19 s | 3.6 ms / 252 | 446 / 351 | 10.2 ms / 10.1 ms | 1.30 s / 1.33 s | 2.1 s / 2.2 s |
| blocking | 1.3 / 1.1 | 2.9 ms / 2.8 ms | 3.8 ms / 3.7 ms | 4.2 ms / 4.3 ms | 2.9 ms / 2.8 ms | 4.1 ms / 4.3 ms | 209 / 194 | 2.3 ms / 2.5 ms | 10.0 ms / 10.1 ms | 533 ms / 532 ms | 1.00 s / 1.00 s |
| blocking, early write-back off | 87.7 / 90.8 | 772 ms / 800 ms | 774 ms / 816 ms | 774 ms / 816 ms | 772 ms / 800 ms | 774 ms / 816 ms | 228 / 181 | 772 ms / 803 ms | 774 ms / 816 ms | 1.30 s / 1.33 s | 1.77 s / 1.81 s |
| background, helpers on every CPU (default) | 1.0 / 0.8 | 47 / 45 | 96 / 262 | 164 / 292 | 842 / 783 | 1.19 ms / 1.04 ms | 207 / 191 | 411 / 458 | 10.2 ms / 10.1 ms | 532 ms / 532 ms | 1.00 s / 1.00 s |
| background, helpers inherit the writer's CPU | 1.2 / 1.0 | 189 / 1.97 ms | 2.5 ms / 2.5 ms | 3.0 ms / 2.8 ms | 1.56 ms / 1.61 ms | 8.2 ms / 11.1 ms | 225 / 224 | 1.25 ms / 1.40 ms | 10.1 ms / 10.2 ms | 534 ms / 533 ms | 1.01 s / 1.01 s |
| no checkpoints | 0.9 / 0.7 | no checkpoints | – | – | – | – | 4.6 ms / 3.7 ms | 482 / 590 | 10.1 ms / 10.1 ms | – | – |
| no checkpoints, early write-back off | 0.4 / 0.3 | no checkpoints | – | – | – | – | 4.1 ms / 3.9 ms | 406 / 394 | 10.1 ms / 10.1 ms | – | – |

The 60-second unpaced probes of the finish round (records/s, early write-back on / off): x86-64 ext4 493,214 / 489,437,
xfs 496,731 / 489,288; aarch64 ext4 510,703 / 506,205, xfs 507,405 / 502,454. One run each.

What these runs show:
- Background checkpoints with early write-back stop the writer for 44–104 µs (p50) and at most 0.34 ms per checkpoint at
  about 250 MB/s, and make data durable 0.75–3.1 ms (p50) after the request. Without early write-back a background
  checkpoint still barely stops the writer (at most 3.6 ms), but takes 0.76–0.79 s to become durable.
- Early write-back is what makes blocking checkpoints short: 2.8–2.9 ms (p50) with it, 745–800 ms without.
- A helper that shares the writer's CPU still delays it: at most 2.0–3.0 ms with the finish round's helpers on the
  writer's CPU (`HelperCpus::Inherited`), 309–359 ms with the prototype's checkpoint thread there.
- Every aarch64 run of the finish round in which the writer kept its schedule has a mapped-reader maximum of
  10.0–10.2 ms, also without checkpoints (the x86-64 runs show no such constant maximum); not investigated.

**The ext4 roll stall of ADR 0025.** The prototype sampled the writer thread's kernel stack every 2 ms
(`visibility-only.stacks.txt`). On ext4 its longest waits were `fdatasync` at a roll inside `ext4_sync_file` →
`ext4_fc_commit` → `jbd2_complete_transaction` → `jbd2_log_wait_commit`, up to 254.6 ms (x86-64) and 255.7 ms (aarch64):
the roll's sync waited for the ext4 journal to commit. On xfs the same sync waited in `xfs_log_force_seq` for up to
2.6 ms (x86-64) and 70.9 ms (aarch64). Pull request #28 removed the sync from the roll (ADR 0038 → Rolls without a
sync); in the finish round's samples the writer thread waited at most 2.1 ms (aarch64, in `openat`), and without
checkpoints roll appends took at most 4.6 ms with early write-back (65 ms on x86-64 ext4 with it off).

### Comparison with other tools (sustained mode)

`compare/run.py --sustained` (`compare/README.md` → Sustained mode), 10-minute runs, both payloads: a probe (unpaced,
with a concurrent reader where the tool allows one), SV (paced at 50 % and 90 % of the tool's own probe, no sync) and SD
(SV plus the tool's closest equivalent of a checkpoint every second, on the writer's thread). Two repeats of SV and SD,
the probe once. Because rates come from each tool's own probe, the same percentage is a different rate for each tool. In
this mode tickertape uses the blocking `Writer::checkpoint`, as the other tools' sync calls block, and the mapped
reader.

**Not one session.** The other five tools ran in the first session at `279f59a` (the comparison part of the session
command above: `compare/run.py /data/results/session-<fs>/compare --sustained --keep-going`). **Every tickertape row of
that session failed** on all four VMs:
`create segment failed for …/00000000001313783972.tape.tmp: Too many open files (os error 24)` (64 B; 1,024 B at
`…/00000000000128414592.tape.tmp`), then `reader timed out`, and the SV and SD rows had no probe to derive their rates
from. The cause is the defect fixed in #38: the comparison's tickertape probe does not checkpoint, and a writer held
every rolled segment open until the next checkpoint, so it ran out of file descriptors after about a thousand rolls
(creating the 1,019th segment failed). v0.4.0 has the same defect (`CHANGELOG.md`). tickertape's rows were therefore
measured again two days later at `88b3304`, on other VMs of the same machine types and filesystems (the aarch64 VMs in
another zone), with

```sh
python3 benches/session.py --journal-dir <fs>/tt --out /data/results/session-<fs> --only compare \
  --compare-tools tickertape --cpus 1,2,3,4,5,6 --yes
```

(which runs `compare/run.py … --sustained --keep-going --tools tickertape`), with the `benches/session.py` fix of this
change applied locally. All 18 runs per VM finished. **This is a weaker comparison than one session on one machine**:
tickertape's rows come from another VM, another day and a newer commit (with early write-back and rolls without a sync,
ADR 0038) than the other tools' rows.

**Guarantees differ** (`compare/README.md` has the full table): tickertape and Chronicle Queue are persistent,
replayable journals with independent readers; Aeron IPC persists nothing and has no SD run; yaque is a consuming
single-reader queue with no sync operation and no SD run; queue-file forbids concurrent processes, so it runs writer
only (no reader latency); the plain file has no CRC and no recovery. The SD operations cover different amounts of data
and metadata: tickertape `Writer::checkpoint` (data, then the durable marker, the only one that publishes a durable
position readers can use), Chronicle Queue `ExcerptAppender.sync` with `SyncMode.SYNC` (`msync(MS_SYNC)`), queue-file
`QueueFile::sync_all` (`fsync`), plain file `File::sync_data` (`fdatasync`). None is verified for power loss by this
suite. Versions: Chronicle Queue 2026.8, Aeron 1.54.0, yaque 0.6.6, queue-file 2.0.0.

**Failed and skipped runs** (reported, not left out): SD at 90 %, 64 B, Chronicle Queue on x86-64 (ext4 and xfs, both
repeats): `java.lang.IllegalStateException: reader timed out`. SD at 90 %, 1,024 B, plain file on every VM (both
repeats): `plain-file: reader timed out`; at 64 B on aarch64 ext4 repeat 1 failed the same way (repeat 2 is in the
table). SV at 90 %, 1,024 B, yaque on both ext4 VMs, skipped before running:
`predicted … B of data exceeds 80 % of the … B free … use a larger disk or a shorter duration` (479,403,050,400 B
predicted on x86-64).

**Probe, achieved rate** (million records/s, ext4 / xfs):

| Tool | x86-64, 64 B | x86-64, 1,024 B | aarch64, 64 B | aarch64, 1,024 B |
|---|---|---|---|---|
| tickertape | 3.54 / 3.54 | 0.35 / 0.35 | 3.46 / 3.46 | 0.34 / 0.34 |
| chronicle | 3.06 / 3.30 | 0.35 / 0.35 | 2.80 / 2.96 | 0.34 / 0.34 |
| aeron | 6.80 / 7.39 | 4.10 / 4.14 | 3.55 / 3.73 | 2.90 / 2.97 |
| yaque | 1.01 / 0.44 | 0.66 / 0.28 | 0.77 / 0.33 | 0.53 / 0.27 |
| queue-file | 0.85 / 0.85 | 0.32 / 0.31 | 0.56 / 0.48 | 0.27 / 0.28 |
| plain-file | 1.65 / 0.32 | 0.35 / 0.31 | 1.15 / 0.26 | 0.34 / 0.24 |

Probe reader latency, measured from the actual send (µs unless marked; queue-file has no reader):

| Tool | Arch | 64 B: p50 | p99.9 | max | 1,024 B: p50 | p99.9 | max |
|---|---|---|---|---|---|---|---|
| tickertape | x86-64 | 0.50 / 0.52 | 10.0 / 10.6 | 332 ms / 87 ms | 0.96 / 1.01 | 28 / 24 | 351 ms / 127 ms |
| tickertape | aarch64 | 0.69 / 0.70 | 4.2 / 5.4 | 395 ms / 23 ms | 0.94 / 0.97 | 17.3 / 9.8 | 281 ms / 89 ms |
| chronicle | x86-64 | 0.50 / 0.48 | 460 / 598 | 144 ms / 12.3 ms | 0.59 / 0.62 | 705 / 681 | 172 ms / 17.7 ms |
| chronicle | aarch64 | 0.68 / 0.67 | 28 / 15.7 | 144 ms / 14.0 ms | 0.72 / 0.73 | 85 / 97 | 270 ms / 18.5 ms |
| aeron | x86-64 | 0.39 / 0.34 | 151 / 52 | 32 ms / 28 ms | 0.69 / 0.69 | 162 / 215 | 76 ms / 70 ms |
| aeron | aarch64 | 0.65 / 0.61 | 52 / 104 | 33 ms / 31 ms | 0.70 / 0.73 | 17.9 / 24 | 73 ms / 66 ms |
| yaque | x86-64 | 0.99 / 3.6 | 11.9 ms / 5.7 ms | 14.4 ms / 7.1 ms | 1.69 / 5.3 | 13.6 ms / 7.1 ms | 19.9 ms / 9.2 ms |
| yaque | aarch64 | 1.47 / 4.5 | 16.0 ms / 10.3 ms | 17.6 ms / 12.2 ms | 1.91 / 5.1 | 17.1 ms / 11.8 ms | 17.7 ms / 12.2 ms |
| plain-file | x86-64 | 1.34 / 39 ms | 7.8 / 94 ms | 141 ms / 95 ms | 1.61 / 44 | 9.1 / 1.67 ms | 279 ms / 100 ms |
| plain-file | aarch64 | 1.78 / 9.5 | 6.5 / 260 | 144 ms / 545 | 2.1 / 10.4 | 9.5 / 464 | 277 ms / 113 ms |

At 1,024 B the 10-minute probes of tickertape, Chronicle Queue and (on ext4) the plain file end at the same rate,
0.34–0.35 million records/s (about 350–360 MB/s of payload); tickertape's 60-second probes on the 16-vCPU VMs of the
checkpoint measurement reached 0.49–0.51 million, with up to 10–12 GB of dirty page cache. That is consistent with the
10-minute rate at 1,024 B being set by how fast the local SSD absorbs write-back rather than by the writer.

**SV, sustained visibility** (reader latency from the intended send time, µs unless marked; ext4 / xfs):

x86-64, 64 B:

| Tool | Fraction of the probe | Rate (records/s) | Late sends (%) | p50 | p99 | p99.9 | p99.99 | max |
|---|---|---|---|---|---|---|---|---|
| tickertape | 50 % | 1,770,295 / 1,772,303 | 6.6 / 3.5–3.7 | 0.34 / 0.33 | 6.6–6.7 / 4.0–4.2 | 519–522 / 536–538 | 973–983 / 991–992 | 1.23–1.82 ms / 1.26–1.66 ms |
| tickertape | 90 % | 3,186,531 / 3,190,147 | 24.1–24.8 / 19.6–20.5 | 0.32 / 0.30 | 0.72–1.47 ms / 21–22 ms | 35–67 ms / 23–24 ms | 101–111 ms / 25–55 ms | 114–126 ms / 39–83 ms |
| chronicle | 50 % | 1,528,514 / 1,648,691 | 7.5–8.7 / 6.0–6.1 | 0.44 / 0.43 | 28 µs to 12.7 ms / 18.0–18.3 | 17.4–109 ms / 27 | 122–130 ms / 213–250 | 139 ms / 6.1 ms |
| chronicle | 90 % | 2,751,326 / 2,967,645 | 100.0 / 100.0 | 23 s / 34–35 s | 41–49 s / 62–70 s | 41–49 s / 62–70 s | 41–49 s / 62–71 s | 41–49 s / 62–71 s |
| aeron | 50 % | 3,397,510 / 3,694,347 | 1.5–1.6 / 1.2–1.4 | 0.27–0.28 / 0.28 | 3.7–3.9 / 2.7–3.5 | 10.9–25 ms / 25–39 ms | 37–46 ms / 54–63 ms | 50–52 ms / 61–70 ms |
| aeron | 90 % | 6,115,519 / 6,649,825 | 8.8–10.6 / 31.4–100.0 | 0.29–0.31 / 0.43 µs to 32 s | 40–41 ms / 0.75–64 s | 76–79 ms / 0.87–65 s | 88–93 ms / 0.88–65 s | 91–96 ms / 0.89–65 s |
| yaque | 50 % | 506,350 / 218,124 | 3.4–3.7 / 1.3 | 0.95–0.96 / 3.7–3.8 | 6.9–7.8 / 8.0–8.2 | 10.7 ms / 163–192 | 13.3–13.4 ms / 6.5 ms | 14.6–14.7 ms / 7.6 ms |
| yaque | 90 % | 911,430 / 392,624 | 22.7–36.6 / 88.2–99.7 | 1.14–1.49 / 89 ms to 9.2 s | 0.14–4.6 ms / 2.8–25 s | 12.1–16.2 ms / 2.9–25 s | 13.6–19.7 ms / 2.9–25 s | 14.7–34 ms / 2.9–25 s |
| queue-file | 50 % | 427,214 / 425,730 | 12.1–12.2 / 7.5–7.8 | writer only | writer only | writer only | writer only | writer only |
| queue-file | 90 % | 768,985 / 766,315 | 22.0–23.0 / 37.1–38.3 | writer only | writer only | writer only | writer only | writer only |
| plain-file | 50 % | 823,623 / 158,862 | 2.7–3.3 / 0.5–0.6 | 0.85–0.87 / 3.5–3.6 | 6.8 µs to 6.9 ms / 5.6–5.9 | 53–76 ms / 9.5–9.8 | 109–115 ms / 57–73 | 125–130 ms / 385–932 |
| plain-file | 90 % | 1,482,523 / 285,951 | 29.4–33.2 / 5.7–17.9 | 0.81–0.86 / 3.5–3.8 | 71–78 ms / 14.0–201 | 121–127 ms / 0.18–1.61 ms | 133–137 ms / 0.60–2.6 ms | 141–142 ms / 1.96–3.5 ms |

x86-64, 1,024 B:

| Tool | Fraction of the probe | Rate (records/s) | Late sends (%) | p50 | p99 | p99.9 | p99.99 | max |
|---|---|---|---|---|---|---|---|---|
| tickertape | 50 % | 173,168 / 173,077 | 1.8–1.9 / 2.3–2.4 | 0.83–0.84 / 0.82 | 3.3 / 3.3 | 376–381 / 392–396 | 969–972 / 989–990 | 1.23 ms / 1.23–3.6 ms |
| tickertape | 90 % | 311,703 / 311,538 | 2.9–3.1 / 3.3–3.5 | 0.83–0.86 / 0.84 | 3.8–3.9 / 3.7–3.8 | 711–712 / 690–721 | 1.01 ms / 1.00–70 ms | 1.27–1.33 ms / 1.19–120 ms |
| chronicle | 50 % | 175,381 / 175,787 | 5.6–6.1 / 3.2 | 0.56–0.57 / 0.57–0.58 | 5.9–16.7 ms / 19.7–19.8 | 87–108 ms / 25 | 122–134 ms / 96–129 | 134–144 ms / 4.8–4.9 ms |
| chronicle | 90 % | 315,687 / 316,418 | 12.6–12.8 / 6.0 | 0.56–0.57 / 0.55–0.62 | 76–78 ms / 21 | 123–124 ms / 28 | 138–139 ms / 129–143 | 143–144 ms / 4.8–5.9 ms |
| aeron | 50 % | 2,047,724 / 2,067,805 | 2.2–2.5 / 2.2–2.7 | 0.41–0.42 / 0.41–0.42 | 0.30–8.6 ms / 3.2–50 ms | 93–103 ms / 94–145 ms | 106–117 ms / 136–257 ms | 111–121 ms / 154–277 ms |
| aeron | 90 % | 3,685,904 / 3,722,049 | 100.0 / 100.0 | 52–53 s / 35–44 s | 102–109 s / 78–87 s | 103–111 s / 79–87 s | 103–111 s / 79–87 s | 103–111 s / 79–87 s |
| yaque | 50 % | 327,789 / 138,930 | 3.5–4.1 / 1.1 | 1.39–1.41 / 4.0 | 8.1–9.4 ms / 36–44 | 11.3–12.7 ms / 6.0 ms | 11.7–13.4 ms / 6.8–6.9 ms | 12.5–14.6 ms / 7.5–9.4 ms |
| yaque | 90 % | skipped / 250,074 | skipped / 14.7–19.5 | skipped / 4.2–4.3 | skipped / 2.6–4.1 ms | skipped / 6.3–8.2 ms | skipped / 9.6–15.2 ms | skipped / 16.8–23 ms |
| queue-file | 50 % | 158,054 / 156,514 | 12.7–13.5 / 6.0 | writer only | writer only | writer only | writer only | writer only |
| queue-file | 90 % | 284,498 / 281,725 | 26.5–28.5 / 27.6–28.3 | writer only | writer only | writer only | writer only | writer only |
| plain-file | 50 % | 174,383 / 154,315 | 4.1–4.4 / 1.0 | 1.19–1.23 / 3.6 | 56–68 ms / 8.4 | 122 ms / 35–39 | 137 ms / 0.11–71 ms | 142–144 ms / 1.22–100 ms |
| plain-file | 90 % | 313,890 / 277,767 | 6.5–7.0 / 16.9–17.2 | 1.26–1.28 / 3.8 | 26–54 ms / 1.20–11.2 ms | 105–118 ms / 71–90 ms | 132–136 ms / 102–115 ms | 141–142 ms / 108–120 ms |

aarch64, 64 B:

| Tool | Fraction of the probe | Rate (records/s) | Late sends (%) | p50 | p99 | p99.9 | p99.99 | max |
|---|---|---|---|---|---|---|---|---|
| tickertape | 50 % | 1,728,604 / 1,730,510 | 1.4 / 1.3 | 0.30–0.32 / 0.32 | 1.81–1.87 / 1.80–1.83 | 470–494 / 368–385 | 919–942 / 817–830 | 1.46–1.85 ms / 1.23–2.0 ms |
| tickertape | 90 % | 3,111,488 / 3,114,919 | 3.9–4.2 / 3.3–3.4 | 0.30–0.31 / 0.31–0.33 | 28–94 / 12.5–13.0 | 1.61–1.65 ms / 702–711 | 2.0–2.1 ms / 867–871 | 3.7–4.3 ms / 2.6–9.4 ms |
| chronicle | 50 % | 1,398,050 / 1,478,695 | 6.3–6.6 / 3.1 | 0.40 / 0.39 | 35 µs to 18.7 ms / 7.5–7.7 | 67–103 ms / 17.1–17.6 | 108–131 ms / 190–193 | 123–140 ms / 909–954 |
| chronicle | 90 % | 2,516,490 / 2,661,651 | 49.2–50.1 / 22.6–25.3 | 1.14–1.78 / 0.41 | 103–106 ms / 23–40 | 126–130 ms / 140–150 | 133–141 ms / 383–483 | 136–145 ms / 1.53–1.56 ms |
| aeron | 50 % | 1,773,750 / 1,864,986 | 0.7–0.8 / 0.6–0.8 | 0.37–0.38 / 0.36–0.38 | 1.66–1.77 / 1.56–1.57 | 85–192 / 84–166 | 13.6–16.0 ms / 13.0–21 ms | 26–29 ms / 23–32 ms |
| aeron | 90 % | 3,192,751 / 3,356,975 | 3.0–6.0 / 6.8–84.3 | 0.35–0.38 / 0.38 µs to 59 ms | 3.8 µs to 2.6 ms / 5.2–314 ms | 17.8–33 ms / 43–320 ms | 33–50 ms / 52–321 ms | 34–52 ms / 53–321 ms |
| yaque | 50 % | 386,406 / 165,009 | 2.7 / 0.8 | 1.41–1.45 / 4.6 | 5.2–5.9 / 8.1–8.2 | 13.3–13.7 ms / 2.5–2.9 ms | 16.8–17.1 ms / 11.1–11.5 ms | 17.4–17.7 ms / 12.3–12.7 ms |
| yaque | 90 % | 695,532 / 297,017 | 36.5–40.1 / 69.4–73.0 | 1.85–1.99 / 8.6–11.4 | 111–150 / 140–936 ms | 15.0–15.4 ms / 179–980 ms | 16.8–17.2 ms / 186–989 ms | 17.3–17.8 ms / 200–998 ms |
| queue-file | 50 % | 281,121 / 238,651 | 12.3–12.6 / 9.2–9.3 | writer only | writer only | writer only | writer only | writer only |
| queue-file | 90 % | 506,017 / 429,572 | 35.3–41.3 / 33.3–35.1 | writer only | writer only | writer only | writer only | writer only |
| plain-file | 50 % | 574,604 / 132,358 | 1.4–1.7 / 0.4 | 1.26–1.30 / 4.0–4.2 | 3.5–4.3 / 6.6–7.0 | 14.5–87 ms / 9.8–10.0 | 129 ms / 22–28 | 142 ms / 100–110 |
| plain-file | 90 % | 1,034,287 / 238,245 | 22.6–23.5 / 2.6–6.2 | 1.32–1.34 / 4.0–4.2 | 67–81 ms / 7.9–9.7 | 123–125 ms / 17.8–35 | 134–136 ms / 48–63 | 137–141 ms / 0.42–1.31 ms |

aarch64, 1,024 B:

| Tool | Fraction of the probe | Rate (records/s) | Late sends (%) | p50 | p99 | p99.9 | p99.99 | max |
|---|---|---|---|---|---|---|---|---|
| tickertape | 50 % | 169,018 / 169,157 | 0.8 / 0.7 | 0.53–0.55 / 0.54–0.55 | 2.3 / 2.1–2.2 | 318–325 / 191–199 | 925–931 / 808–813 | 2.0–3.3 ms / 1.24–1.87 ms |
| tickertape | 90 % | 304,233 / 304,482 | 1.4 / 0.9 | 0.55 / 0.53–0.54 | 7.1–8.3 / 2.3–2.4 | 17.4–28 ms / 655–663 | 117–150 ms / 32–72 ms | 164–203 ms / 84–125 ms |
| chronicle | 50 % | 171,599 / 169,410 | 5.2–5.4 / 1.6–1.7 | 0.45–0.46 / 0.43–0.45 | 22 ms / 8.6–8.7 | 92–105 ms / 15.3–15.7 | 123–131 ms / 187–188 | 141–145 ms / 4.2–12.6 ms |
| chronicle | 90 % | 308,879 / 304,939 | 10.8–11.3 / 2.6–2.8 | 0.44–0.47 / 0.43 | 62–63 ms / 8.1–8.6 | 119 ms / 29–37 | 133–135 ms / 187–194 | 142–144 ms / 5.2–5.8 ms |
| aeron | 50 % | 1,450,193 / 1,487,346 | 0.9–1.0 / 1.1–1.3 | 0.49 / 0.51–0.53 | 1.89–1.96 / 2.2–2.7 | 24–39 ms / 52–55 ms | 76–80 ms / 67–70 ms | 83–84 ms / 78–81 ms |
| aeron | 90 % | 2,610,348 / 2,677,224 | 8.6–52.7 / 5.3–5.7 | 0.51–1.74 / 0.43–0.45 | 44–663 ms / 49–53 ms | 69–710 ms / 85–98 ms | 87–728 ms / 100–119 ms | 90–730 ms / 103–123 ms |
| yaque | 50 % | 266,310 / 136,182 | 3.9–4.2 / 1.8 | 1.71–1.77 / 4.6–4.7 | 12.7–13.0 ms / 3.0–3.3 ms | 16.7–17.0 ms / 11.1–11.4 ms | 17.1–17.4 ms / 11.9–12.2 ms | 17.7–17.8 ms / 12.3–12.6 ms |
| yaque | 90 % | skipped / 245,129 | skipped / 84.7–92.3 | skipped / 23 µs to 6.3 ms | skipped / 17.4–129 ms | skipped / 29–164 ms | skipped / 36–173 ms | skipped / 49–187 ms |
| queue-file | 50 % | 136,836 / 140,766 | 12.1–18.3 / 7.5–7.6 | writer only | writer only | writer only | writer only | writer only |
| queue-file | 90 % | 246,306 / 253,379 | 23.4–23.7 / 23.9–24.7 | writer only | writer only | writer only | writer only | writer only |
| plain-file | 50 % | 171,043 / 121,864 | 1.4–1.6 / 0.5 | 1.43–1.50 / 4.4 | 7.3–7.7 / 9.2–9.3 | 64–87 ms / 24 | 127–129 ms / 51 | 141 ms / 221–897 |
| plain-file | 90 % | 307,878 / 219,356 | 4.2–4.3 / 16.3–23.7 | 1.42–1.46 / 4.8–5.0 | 12.0–21 ms / 0.18–3.5 ms | 106–110 ms / 53–90 ms | 131–134 ms / 73–101 ms | 143–144 ms / 75–103 ms |

**SD, sustained periodic durability** (as SV, plus the sync operation's duration; Aeron and yaque have no SD run):

x86-64, 64 B:

| Tool | Fraction of the probe | Rate (records/s) | Late sends (%) | p50 | p99 | p99.9 | p99.99 | max | sync p50 | sync p99 | sync max |
|---|---|---|---|---|---|---|---|---|---|---|---|
| tickertape | 50 % | 1,770,295 / 1,772,303 | 8.8–9.1 / 7.8–7.9 | 0.33–0.34 / 0.33–0.34 | 37–38 / 17.3–21 | 1.73 ms / 1.09–1.50 ms | 2.8–2.9 ms / 1.93–2.4 ms | 3.8 ms / 2.6–3.0 ms | 2.1–2.3 ms / 1.64–2.1 ms | 3.4–3.5 ms / 2.4–3.0 ms | 3.8 ms / 2.6–3.0 ms |
| tickertape | 90 % | 3,186,531 / 3,190,147 | 41.6–42.0 / 80.5–99.7 | 0.38–0.39 / 3.8–8.3 s | 42 ms / 11.5–30 s | 49 ms / 11.6–30 s | 51–52 ms / 11.6–31 s | 53 ms / 11.6–31 s | 42–43 ms / 297–310 ms | 52 ms / 332–352 ms | 53 ms / 354–469 ms |
| chronicle | 50 % | 1,528,514 / 1,648,691 | 45.3–48.6 / 44.1–45.6 | 0.53–0.61 / 0.54–0.56 | 151 ms / 152 ms | 175–176 ms / 176 ms | 183–185 ms / 184 ms | 187–199 ms / 188 ms | 99–100 ms / 107–109 ms | 184–185 ms / 185 ms | 186–187 ms / 188 ms |
| chronicle | 90 % | failed / failed | failed / failed | failed / failed | failed / failed | failed / failed | failed / failed | failed / failed | failed / failed | failed / failed | failed / failed |
| queue-file | 50 % | 427,214 / 425,730 | 29.4–29.6 / 32.8 | writer only | writer only | writer only | writer only | writer only | 146 ms / 145 ms | 262–264 ms / 231–234 ms | 303–312 ms / 254–261 ms |
| queue-file | 90 % | 768,985 / 766,315 | 99.9 / 100.0 | writer only | writer only | writer only | writer only | writer only | 261–262 ms / 231 ms | 307–308 ms / 295–296 ms | 336–340 ms / 305–306 ms |
| plain-file | 50 % | 823,623 / 158,862 | 37.7–39.5 / 7.1–7.2 | 0.91 / 3.6 | 188 ms / 28 ms | 192–193 ms / 33 ms | 193 ms / 33–34 ms | 195 ms / 34 ms | 193 ms / 33 ms | 194 ms / 34 ms | 195–196 ms / 34 ms |
| plain-file | 90 % | 1,482,523 / 285,951 | 99.9 / 97.8–99.1 | 54–57 s / 0.18–1.49 s | 107–109 s / 1.02–2.3 s | 108–110 s / 1.08–2.3 s | 108–110 s / 1.10–2.4 s | 108–110 s / 1.10–2.4 s | 297–298 ms / 63 ms | 319–325 ms / 71–75 ms | 408–416 ms / 75–78 ms |

x86-64, 1,024 B:

| Tool | Fraction of the probe | Rate (records/s) | Late sends (%) | p50 | p99 | p99.9 | p99.99 | max | sync p50 | sync p99 | sync max |
|---|---|---|---|---|---|---|---|---|---|---|---|
| tickertape | 50 % | 173,168 / 173,077 | 2.0–2.5 / 2.1–2.2 | 0.84–0.85 / 0.83–0.87 | 3.8–4.0 / 3.5–3.6 | 1.48–1.71 ms / 0.84–1.05 ms | 2.8 ms / 1.83–12.3 ms | 3.9–6.6 ms / 2.8–57 ms | 2.4–2.6 ms / 1.64–1.68 ms | 3.4–3.5 ms / 2.4–2.5 ms | 3.9–6.6 ms / 2.8–57 ms |
| tickertape | 90 % | 311,703 / 311,538 | 2.5–3.7 / 22.1–98.2 | 0.85–0.86 / 0.87 µs to 13.3 s | 4.5–4.9 / 1.68–20 s | 2.1–2.2 ms / 2.0–21 s | 3.4 ms / 2.1–21 s | 4.5–4.9 ms / 2.2–21 s | 2.8–3.0 ms / 2.5 ms to 2.1 s | 4.0–4.1 ms / 2.00–4.2 s | 4.3–4.4 ms / 2.2–4.3 s |
| chronicle | 50 % | 175,381 / 175,787 | 23.3–23.9 / 17.0–17.1 | 0.61 / 0.57–0.62 | 133 ms / 134–135 ms | 169 ms / 172–173 ms | 181–182 ms / 226 ms | 186–202 ms / 280 ms | 117–119 ms / 126–127 ms | 184–185 ms / 187 ms | 186–202 ms / 279–280 ms |
| chronicle | 90 % | 315,687 / 316,418 | 37.2–38.4 / 36.0–36.3 | 0.64–0.69 / 0.67–0.74 | 171 ms / 388–393 ms | 183 ms / 562 ms | 218–279 ms / 684–695 ms | 244–326 ms / 732–739 ms | 179 ms / 217–223 ms | 187 ms / 499–515 ms | 244–326 ms / 585–739 ms |
| queue-file | 50 % | 158,054 / 156,514 | 60.9 / 62.0–62.1 | writer only | writer only | writer only | writer only | writer only | 485 ms / 478–479 ms | 486 ms / 480 ms | 647–648 ms / 632 ms |
| queue-file | 90 % | 284,498 / 281,725 | 99.9 / 99.9 | writer only | writer only | writer only | writer only | writer only | 1.14–1.22 s / 1.10–1.13 s | 2.3 s / 2.1 s | 2.4 s / 2.1 s |
| plain-file | 50 % | 174,383 / 154,315 | 66.6–66.7 / 90.8–91.0 | 127–128 ms / 203–204 ms | 507–508 ms / 450 ms | 514–515 ms / 454–455 ms | 516 ms / 456 ms | 518–519 ms / 457 ms | 515–516 ms / 455 ms | 517 ms / 457 ms | 518–519 ms / 457 ms |
| plain-file | 90 % | failed / failed | failed / failed | failed / failed | failed / failed | failed / failed | failed / failed | failed / failed | failed / failed | failed / failed | failed / failed |

aarch64, 64 B:

| Tool | Fraction of the probe | Rate (records/s) | Late sends (%) | p50 | p99 | p99.9 | p99.99 | max | sync p50 | sync p99 | sync max |
|---|---|---|---|---|---|---|---|---|---|---|---|
| tickertape | 50 % | 1,728,604 / 1,730,510 | 1.6–1.7 / 1.6 | 0.30 / 0.30–0.32 | 2.3–2.5 / 2.4–2.6 | 952–977 / 967–997 | 1.77–1.79 ms / 1.82–1.88 ms | 2.3–2.4 ms / 2.3–2.7 ms | 1.35–1.38 ms / 1.37–1.39 ms | 2.1–2.2 ms / 2.2 ms | 2.2–2.3 ms / 2.3–2.4 ms |
| tickertape | 90 % | 3,111,488 / 3,114,919 | 6.0 / 5.0–5.4 | 0.31 / 0.31 | 1.19–1.23 ms / 0.73–1.31 ms | 5.4–5.5 ms / 5.0–6.5 ms | 7.9–8.1 ms / 7.2–8.9 ms | 9.8–10.5 ms / 8.6–10.7 ms | 2.8 ms / 2.8–3.3 ms | 8.7–9.1 ms / 8.2–9.6 ms | 9.6–10.5 ms / 8.6–10.7 ms |
| chronicle | 50 % | 1,398,050 / 1,478,695 | 31.9–32.8 / 27.9–28.0 | 0.42 / 0.41–0.42 | 147–148 ms / 156–157 ms | 172–173 ms / 185–187 ms | 183 ms / 194–213 ms | 187–188 ms / 198–239 ms | 95 ms / 100–101 ms | 185 ms / 195–197 ms | 187–188 ms / 198–239 ms |
| chronicle | 90 % | 2,516,490 / 2,661,651 | 100.0 / 100.0 | 30 s / 36–37 s | 79–86 s / 70 s | 79–87 s / 70–71 s | 79–87 s / 70–71 s | 79–87 s / 70–71 s | 67–71 ms / 81–89 ms | 185–186 ms / 174–176 ms | 188 ms / 192–193 ms |
| queue-file | 50 % | 281,121 / 238,651 | 25.8–25.9 / 22.8–23.4 | writer only | writer only | writer only | writer only | writer only | 95 ms / 80–81 ms | 169 ms / 147–148 ms | 201 ms / 171 ms |
| queue-file | 90 % | 506,017 / 429,572 | 99.9 / 99.0–99.6 | writer only | writer only | writer only | writer only | writer only | 168–169 ms / 147 ms | 190–193 ms / 171–172 ms | 198–204 ms / 174–175 ms |
| plain-file | 50 % | 574,604 / 132,358 | 28.6–29.3 / 5.4–5.5 | 1.34 / 4.2 | 128 ms / 21–22 ms | 133 ms / 26 ms | 133 ms / 27 ms | 134 ms / 28 ms | 133 ms / 27 ms | 134 ms / 28 ms | 134 ms / 28 ms |
| plain-file | 90 % | 1,034,287 (r2 only) / 238,245 | 99.9 (r2 only) / 53.4–53.7 | 57 s (r2 only) / 1.49–2.2 ms | 111 s (r2 only) / 52–85 ms | 112 s (r2 only) / 56–114 ms | 113 s (r2 only) / 66–129 ms | 113 s (r2 only) / 73–132 ms | 204 ms (r2 only) / 52–53 ms | 232 ms (r2 only) / 53–54 ms | 251 ms (r2 only) / 54–55 ms |

aarch64, 1,024 B:

| Tool | Fraction of the probe | Rate (records/s) | Late sends (%) | p50 | p99 | p99.9 | p99.99 | max | sync p50 | sync p99 | sync max |
|---|---|---|---|---|---|---|---|---|---|---|---|
| tickertape | 50 % | 169,018 / 169,157 | 0.8–0.9 / 0.8 | 0.55–0.56 / 0.55 | 2.6–2.7 / 2.4 | 823–824 / 767–785 | 1.69–1.70 ms / 1.73–1.76 ms | 2.3 ms / 2.7–4.1 ms | 1.39–1.41 ms / 1.34–1.39 ms | 2.1–2.2 ms / 2.2–2.3 ms | 2.3 ms / 2.7–4.1 ms |
| tickertape | 90 % | 304,233 / 304,482 | 13.9–14.5 / 19.2–19.6 | 0.54–0.55 / 0.54–0.56 | 109–114 ms / 154–155 ms | 123–149 ms / 164–165 ms | 129–164 ms / 167–168 ms | 134–170 ms / 171 ms | 118–119 ms / 163–164 ms | 131–168 ms / 169–170 ms | 134–170 ms / 171 ms |
| chronicle | 50 % | 171,599 / 169,410 | 19.6–20.0 / 14.0–14.2 | 0.45–0.46 / 0.46–0.48 | 130–131 ms / 137 ms | 169 ms / 179 ms | 182 ms / 192–193 ms | 188 ms / 198–199 ms | 114 ms / 120 ms | 185–186 ms / 196 ms | 188 ms / 198–199 ms |
| chronicle | 90 % | 308,879 / 304,939 | 25.8 / 19.1–19.2 | 0.46–0.47 / 0.44–0.45 | 164–165 ms / 170–171 ms | 179 ms / 186 ms | 272–300 ms / 193–195 ms | 322–350 ms / 199–221 ms | 173–174 ms / 181–182 ms | 186–187 ms / 197 ms | 269–350 ms / 199–221 ms |
| queue-file | 50 % | 136,836 / 140,766 | 58.1–58.2 / 63.3 | writer only | writer only | writer only | writer only | writer only | 419 ms / 431 ms | 420 ms / 433 ms | 538–539 ms / 541–545 ms |
| queue-file | 90 % | 246,306 / 253,379 | 99.9 / 100.0 | writer only | writer only | writer only | writer only | writer only | 0.99–1.02 s / 1.02–1.08 s | 1.51 s / 1.33 s | 1.55 s / 1.36–1.38 s |
| plain-file | 50 % | 171,043 / 121,864 | 66.9–67.4 / 71.4–74.6 | 126–128 ms / 107–117 ms | 498 ms / 354 ms | 505 ms / 358 ms | 506 ms / 359 ms | 508 ms / 360 ms | 505–506 ms / 358 ms | 507 ms / 360 ms | 508 ms / 360 ms |
| plain-file | 90 % | failed / failed | failed / failed | failed / failed | failed / failed | failed / failed | failed / failed | failed / failed | failed / failed | failed / failed | failed / failed |

What the comparison shows, with the caveats above:
- **Visibility only, 50 % of each tool's probe.** tickertape's reader has p99.9 of 0.19–0.54 ms and a maximum of
  1.2–3.6 ms in every cell. Chronicle Queue's p99.9 ranges from 15 µs (xfs) to 109 ms (ext4), Aeron's from 84 µs to
  145 ms, the plain file's from 9.5 µs to 122 ms.
- **Visibility only, 90 %.** tickertape's reader stays below 1 ms at p99.9 in four of the eight cells (x86-64 at
  1,024 B, aarch64 xfs), at 1.6 ms on aarch64 ext4 at 64 B and at 17–67 ms in the other three. Some tools fell behind
  their schedule (every or nearly every send late, latencies of seconds): Chronicle Queue at 64 B on x86-64 (23–71 s),
  Aeron at 1,024 B on x86-64 (35–111 s) and at 64 B on x86-64 xfs in one repeat, yaque on xfs except at 1,024 B on
  x86-64 (69–100 % late).
- **Periodic durability, 50 %.** tickertape's checkpoint took 1.3–2.6 ms (p50) and at most 6.6 ms, except one x86-64 xfs
  repeat at 1,024 B (57 ms); its reader's p99.9 is 0.77–1.7 ms. Chronicle Queue's sync took 95–127 ms (p50),
  queue-file's 80–485 ms, the plain file's 27–516 ms, and their readers' p99.9 follows their syncs (26–515 ms).
- **Periodic durability, 90 %.** tickertape's backlog stayed bounded except on x86-64 xfs, with a reader maximum of at
  most 171 ms: checkpoint p50 2.8–3.3 ms and 5–6 % late sends at 64 B on aarch64, 2.8–3.0 ms and 2.5–3.7 % at 1,024 B on
  x86-64 ext4, 42–43 ms and 42 % at 64 B on x86-64 ext4, 118–164 ms and 14–20 % at 1,024 B on aarch64. On x86-64 xfs it
  fell behind at 64 B in both repeats (80–100 % late, checkpoints of 0.30–0.47 s) and at 1,024 B in repeat 1 (98 % late,
  checkpoint p50 2.1 s, 383 checkpoint grid points missed); repeat 2 had 22 % late sends, a checkpoint p50 of 2.5 ms, 44
  grid points missed and a reader maximum of 2.2 s. The two repeats differ by far more than 10 %. Chronicle Queue at
  64 B fell behind on aarch64 and failed on x86-64; queue-file fell behind everywhere (99–100 % late); the plain file
  failed at 1,024 B and fell behind at 64 B on x86-64 and on aarch64 ext4.

### Reader per-record cost

Replay of a warm journal, one record at a time with `try_next`, both readers, at three commits: `7ed7782` (before the
recovery change of ADR 0035), `a381fe1` (main with the per-record regression it caused) and `2522869` (the fix, merged
as `6ee9a0e`). Harness: a small program over the library (`readerperf/main.rs` of the maintainers' cloud tooling) that
builds journals of 1,000,000 records of 16 and 64 B and 200,000 of 1,024 B (default `WriterConfig`, checkpointed), then
replays each 11 times per reader and prints the median; 20 rounds with the three commits in a random order each round
(`readerperf/remote.sh 20 7ed7782 a381fe1 2522869`), on the local SSD (ext4). Values: the median over the 20 rounds of
each round's median, in ns per record, with the range over the rounds. The last row is appending 100,000 records of
64 B.

| Operation | Payload | x86-64 `7ed7782` | `a381fe1` | `2522869` | aarch64 `7ed7782` | `a381fe1` | `2522869` |
|---|---|---|---|---|---|---|---|
| replay, safe reader | 16 B | 36.9 (36.1–38.2) | 47.8 (46.7–49.9) | 34.9 (34.4–36.1) | 29.0 (28.6–29.4) | 37.4 (37.1–38.1) | 28.2 (27.8–28.7) |
| replay, safe reader | 64 B | 44.1 (43.5–45.0) | 53.8 (52.8–55.6) | 41.4 (40.9–42.2) | 33.6 (33.1–34.3) | 42.2 (41.6–43.0) | 33.0 (32.7–33.8) |
| replay, safe reader | 1,024 B | 207.7 (206.5–212.6) | 214.4 (212.7–218.3) | 205.7 (204.4–210.1) | 142.7 (140.2–149.7) | 153.4 (149.4–161.1) | 147.7 (142.5–150.8) |
| replay, mapped reader | 16 B | 45.2 (45.1–45.8) | 58.3 (57.9–58.4) | 36.6 (36.5–37.3) | 30.2 (29.7–31.3) | 40.2 (39.7–46.0) | 29.9 (28.5–30.2) |
| replay, mapped reader | 64 B | 51.7 (51.6–52.7) | 64.6 (64.6–64.7) | 43.6 (43.4–44.1) | 32.0 (31.4–33.1) | 42.0 (41.5–47.6) | 31.3 (30.1–31.9) |
| replay, mapped reader | 1,024 B | 137.9 (137.1–141.8) | 148.5 (147.7–151.7) | 130.1 (129.2–134.2) | 94.6 (93.7–97.1) | 104.2 (103.2–113.3) | 97.2 (96.4–98.5) |
| append | 64 B | 34.5 (34.0–46.7) | 34.7 (34.0–34.9) | 34.6 (34.3–35.0) | 86.0 (84.4–91.9) | 85.9 (84.4–89.7) | 85.7 (84.3–91.7) |

On x86-64 the fix is faster than before the recovery change for both readers (1–19 % below `7ed7782`, 4–37 % below the
regression). On aarch64 it is 1–3 % below `7ed7782` at 16 and 64 B (22–26 % below the regression) and 2.7–3.5 % above it
at 1,024 B (3.7–6.7 % below the regression), where the ranges over the rounds overlap. Append did not change. This is
replay only; the live latency of both readers on both architectures after the zero-copy reader's change (ADR 0027) is in
the sustained session above.

### Replay and recovery at scale (`tt-scale`, `dd450d1`)

The commands of `benches/README.md` → "Cloud run (larger than RAM, local SSD)", with `--repeats 2`, on ext4 on the
local SSD; 1,024 B records; cold runs drop the page cache before every operation (`cache_dropped` true in every cold
run, `cold_unavailable` false). The journals were built with the default `--fill-last 50`: **the newest segment is
half full**, so "seek last" reads about half a segment (`skipped_records`: 63,071 of the 126,144 records of a 128 MiB
segment, about 64 MiB), not a whole one. "seek middle" targets the middle record, 25 % (31,536 records of a 128 MiB
segment) or 75 % (94,608) into its segment, depending on how many records the journal held when it was measured: 75 %
cold and 25 % warm in the 64 GiB journal, 25 % cold and 75 % warm in the other journals. No recorded run measured a
seek across a full segment. The big journal is 512 segments of 128 MiB (64 GiB, 64,522,656 records) on machines with
16 GB of memory; it took 168 s to build on x86-64 (409 MB/s). Wall time per operation, each in a process of its own,
range over the two repeats (µs unless marked); peak RSS is the largest of any operation on that journal. `measure`'s
writer operations append and checkpoint, so a later measurement of the same journal can see one more segment.

**The journals of 10,000 and 20,000 segments could not be measured**: `writer-open-without-marker` failed with
`open segment failed for …/00000000000001003715.tape: Too many open files (os error 24)` on both machines, so `tt-scale`
wrote no result for them. That is the open-files defect fixed afterwards in #38; they have not been measured since.

x86-64:

| Journal | Cache | list | reader open | seek first | seek middle (25 or 75 % of a segment) | seek last (half a segment) | replay | verify | retention plan | writer open, clean | after kill | without marker | Peak RSS |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 10 × 1 MiB | cold | 2.7–3.0 ms | 921–940 | 360–382 | 1.22–1.25 ms | 1.36–1.43 ms | 19.6–22 ms | 15.3–15.5 ms | 3.1–3.4 ms | 2.1–2.8 ms | 1.59–2.4 ms | 22 ms | 4 MiB |
| 10 × 1 MiB | warm | 59–70 | 50–54 | 66–101 | 203 | 155–167 | 1.93–1.98 ms | 2.5–2.6 ms | 175–186 | 382–395 | 423–486 | 2.2 ms | 4 MiB |
| 1,000 × 1 MiB | cold | 215–226 ms | 1.86–2.00 ms | 655–658 | 1.43–1.59 ms | 1.11–1.19 ms | 1.91–1.92 s | 1.43 s | 231 ms | 4.5–6.1 ms | 4.4–4.6 ms | 1.81–1.92 s | 4 MiB |
| 1,000 × 1 MiB | warm | 2.6–3.0 ms | 350–353 | 343–349 | 493–497 | 434–440 | 219–220 ms | 221 ms | 3.1–3.5 ms | 1.12–1.17 ms | 1.14–1.24 ms | 175–177 ms | 4 MiB |
| 10 × 16 MiB | cold | 2.7 ms | 535–537 | 381–390 | 5.7–5.9 ms | 10.7–10.8 ms | 239 ms | 240–241 ms | 3.0–3.1 ms | 4.6–4.7 ms | 11.9–14.8 ms | 265 ms | 19 MiB |
| 10 × 16 MiB | warm | 58–81 | 51–57 | 63–85 | 2.4–2.5 ms | 1.66 ms | 31 ms | 32 ms | 179–229 | 1.36–1.67 ms | 7.1–10.8 ms | 29 ms | 19 MiB |
| 10 × 128 MiB | cold | 2.7–2.8 ms | 569–600 | 365–393 | 45 ms | 90–91 ms | 1.91 s | 1.93 s | 3.1 ms | 25–27 ms | 91–115 ms | 2.1 s | 131 MiB |
| 10 × 128 MiB | warm | 59–77 | 51 | 66–79 | 19.1–19.3 ms | 12.5–12.8 ms | 253–254 ms | 244–256 ms | 175–221 | 10.6–11.1 ms | 73–78 ms | 226 ms | 131 MiB |
| 10 × 1 GiB | cold | 3.1–3.3 ms | 819–821 | 309–325 | 364 ms | 727–728 ms | 15.3 s | 15.5 s | 3.9–4.0 ms | 184–185 ms | 728–915 ms | 17.0 s | 1,027 MiB |
| 10 × 1 GiB | warm | 60–78 | 53–57 | 67–78 | 152 ms | 101 ms | 2.0 s | 1.92–2.0 s | 175–221 | 79 ms | 206–722 ms | 1.77 s | 1,027 MiB |
| 512 × 128 MiB (64 GiB) | cold | 101–103 ms | 1.69–1.74 ms | 557–611 | 137 ms | 90–91 ms | 93 s | 93 s | 107–108 ms | 27–28 ms | 99–117 ms | 93 s | 131 MiB |
| 512 × 128 MiB (64 GiB) | warm | 1.26–1.59 ms | 206–221 | 207–212 | 5.6–44 ms | 12.8–12.9 ms | 84–85 s | 84 s | 1.89–80 ms | 10.6–11.3 ms | 17.2–64 ms | 94 s | 131 MiB |

aarch64:

| Journal | Cache | list | reader open | seek first | seek middle (25 or 75 % of a segment) | seek last (half a segment) | replay | verify | retention plan | writer open, clean | after kill | without marker | Peak RSS |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 10 × 1 MiB | cold | 1.10–1.64 ms | 451–904 | 224–341 | 649–683 | 618–634 | 10.9–11.9 ms | 12.4–13.1 ms | 1.43–1.50 ms | 1.39–2.4 ms | 1.20–1.81 ms | 14.2–14.4 ms | 3 MiB |
| 10 × 1 MiB | warm | 69–70 | 51–57 | 62–63 | 169–172 | 119–131 | 1.51–1.70 ms | 2.00–2.0 ms | 152–210 | 361–379 | 399–424 | 1.56–1.73 ms | 3 MiB |
| 1,000 × 1 MiB | cold | 100–102 ms | 1.69–2.0 ms | 453–492 | 799–891 | 985–996 | 1.54 s | 1.54 s | 97–103 ms | 4.4 ms | 3.1–3.8 ms | 1.54 s | 4 MiB |
| 1,000 × 1 MiB | warm | 2.9 ms | 350–369 | 314–321 | 447–478 | 387–393 | 175–181 ms | 172–175 ms | 3.3–3.9 ms | 1.03–1.07 ms | 1.04–1.08 ms | 121–123 ms | 4 MiB |
| 10 × 16 MiB | cold | 1.15 ms | 269–278 | 169–174 | 2.2 ms | 6.6–6.7 ms | 253 ms | 255 ms | 1.24–1.28 ms | 3.4–3.5 ms | 7.3–10.0 ms | 281 ms | 18 MiB |
| 10 × 16 MiB | warm | 74 | 45–52 | 55–63 | 1.90–1.93 ms | 1.02–1.17 ms | 22–24 ms | 24 ms | 166–176 | 857–976 | 7.4–16.0 ms | 18.6 ms | 18 MiB |
| 10 × 128 MiB | cold | 1.16–1.20 ms | 279–290 | 164–165 | 44 ms | 93 ms | 2.1 s | 2.1 s | 1.28–1.31 ms | 17.9–18.7 ms | 94–110 ms | 2.3 s | 130 MiB |
| 10 × 128 MiB | warm | 75–78 | 52–55 | 56–66 | 14.2–14.4 ms | 9.4–10.0 ms | 179–184 ms | 182–188 ms | 183–186 | 5.2–5.5 ms | 123–134 ms | 143–144 ms | 130 MiB |
| 10 × 1 GiB | cold | 1.86–2.0 ms | 559 | 170–171 | 388 ms | 782 ms | 16.5 s | 16.6 s | 2.2 ms | 131 ms | 783–913 ms | 18.2 s | 1,026 MiB |
| 10 × 1 GiB | warm | 73–74 | 50–58 | 55–68 | 114–118 ms | 78 ms | 1.42–1.50 s | 1.43–1.44 s | 170–188 | 40–41 ms | 135–281 ms | 1.13–1.14 s | 1,026 MiB |
| 512 × 128 MiB (64 GiB) | cold | 49 ms | 1.11–1.13 ms | 296–319 | 142 ms | 93 ms | 101 s | 101 s | 49–50 ms | 17.9–18.7 ms | 94–111 ms | 101 s | 130 MiB |
| 512 × 128 MiB (64 GiB) | warm | 1.55–1.61 ms | 188–190 | 186–193 | 4.0–44 ms | 9.4–9.5 ms | 91 s | 91 s | 1.93–38 ms | 7.3 ms | 9.1–30 ms | 101 s | 130 MiB |

Cold replay of the big journal: 0.69 million records/s (709–710 MB/s of payload) on x86-64 and 0.64 million (656 MB/s)
on aarch64, with a peak RSS of 2 MiB for the replay itself; warm, 0.76 and 0.71 million, since most of the journal is
not in memory either way. Opening a reader takes 1.1–1.7 ms cold on the big journal (1.7–2.0 ms with 1,000 segments of
1 MiB); a seek scans its segment up to the target: to the last record of the half-full newest segment it took 90–93 ms
cold with 128 MiB segments (about 64 MiB read) and 0.73–0.78 s with 1 GiB segments (about 512 MiB), and 137–142 ms to
a record 75 % into a 128 MiB segment (the 64 GiB journal); `list-segments` and `retention-plan` grow with the segment
count (49–108 ms cold for 512 segments, 97–231 ms for 1,000). Recovery without a marker reads every byte (93–101 s
cold for 64 GiB). Peak RSS stays at about one segment (130–131 MiB with 128 MiB segments, 1,026–1,027 MiB with 1 GiB
segments), whatever the journal's size.

### Operating envelope and remaining bottlenecks

From these cloud-VM results only (Google Cloud VMs with local NVMe SSD, ext4 and xfs, on the machine types and at the
commits stated above). Rates are of these machines and do not carry over to others.

- **64 B with a background checkpoint every second, at half the probe rate** (1.45–1.55 million records/s, 93–99 MB/s of
  payload, `279f59a`): the writer keeps its schedule (every scheduled record sent, at most 4.0 ms behind; 1.9–9.8 % of
  sends more than 1 µs late), appends take at most 3.4 ms, a checkpoint pauses the writer for at most 0.79 ms and is
  durable within 7.2 ms of its request, and the visible readers see p99.9 of 32–230 µs and at most 4.0 ms. Durable-only
  readers trail by up to the checkpoint interval (p50 0.58–0.60 s, at most about 1 s). At `3fea1c0` (second session,
  1.60–1.73 million records/s, 103–111 MB/s) the same holds with pauses of at most 2.9 ms, durable within 6.8 ms and
  readers' p99.9 of 39–380 µs, except that on aarch64 xfs the slowest checkpoint of each repeat took 0.28–1.44 s to
  become durable.
- **1,024 B with a background checkpoint every second** (second session, `3fea1c0`, 10-minute runs): at half the probe
  rate (131,210–173,119 records/s, 134–177 MB/s) the writer keeps its schedule (0.5–1.7 % late, at most 24 ms behind),
  a checkpoint pauses it for at most 1.93 ms and is durable within 16.6 ms of its request (on aarch64 xfs the slowest
  took 0.53–3.9 s), and the visible readers see p99.9 of 15.9–256 µs and at most 24 ms. At 90 % of the probe
  (236,178–311,614 records/s, 242–319 MB/s) every scheduled record is sent (0.7–6.9 % late, at most 148 ms behind);
  checkpoints are durable within 225 ms on x86-64 and 2.6 s on aarch64, where appends of 50–128 ms recur on ext4
  (above). The unpaced 10-minute probe reaches 262,421–346,239 records/s (269–355 MB/s), the comparison's probes
  without checkpoints 0.34–0.35 million records/s (`88b3304`). The finish round's 3-minute runs at about 250 MB/s
  (16-vCPU VMs) had pauses of at most 0.34 ms, durable within 5 ms.
- **At 90 % of the probe** (64 B, 2.60–2.79 million records/s, background checkpoints) the writer does not keep its
  schedule: it reaches 92–97 % of the rate, 72–83 % of sends are late and the backlog grows to 23–52 s by the end;
  background checkpoints fall behind (requests join the queued one; durable 9.6–21 s after the request at p50, up to
  141–236 s), the final checkpoint takes 54–103 s, and three of the eight runs failed (above). The probe therefore
  overstates what the writer sustains with periodic durability; with blocking checkpoints and early write-back (the
  comparison's SD at 90 %, `88b3304`) the writer kept up at 64 B on both aarch64 VMs and on x86-64 ext4, and fell behind
  on x86-64 xfs. At `3fea1c0` (second session, 2.89–3.11 million records/s) the writer sent every scheduled record on
  x86-64 and aarch64 ext4, 61–95 % of sends late and at most 514 ms behind, with checkpoints durable within 440 ms; on
  aarch64 xfs it fell more than 60 s behind and stopped, with checkpoints durable about a minute after their request.
  At 1,024 B the writer kept its schedule at 90 % on every VM (above).
- **Bursts** at twice the probe rate for 200 ms of every second (mean 0.4R): every send in a burst is late, the writer's
  backlog reaches 178–232 ms within a burst and drains before the next one; readers follow the writer (lag at most
  15,159 records), so their latency (p50 58–82 ms, at most 153–226 ms) is the writer's backlog. At 1,024 B
  (`3fea1c0`) the writer keeps up with most of each burst: 0.9–32 % of sends late, at most 125 ms behind.
- **Reader lag.** Readers that keep up (steady-50, bursty, visibility-only) lagged by at most 15,159 records (mapped)
  and 33,887 (safe) at `279f59a`, and by at most 11,391 and 4,367 at `3fea1c0`, and their latency is the writer's. A
  reader that reads slower than the writer appends loses records to retention once the segments it has not read are
  removed: by design, reported as skipped records (lagging scenario, `29960a3`). A reader that stalls and resumes
  catches up without loss; a reader at half the rate that falls minutes behind into data no longer in memory reads it
  from storage without loss while retention keeps it (beyond-RAM, `29960a3`).
- **Storage-bound.** At 1,024 B for 10 minutes on these VMs (see the comparison's probe and the second session's);
  unpaced with a checkpoint requested every second, checkpoints fall about a minute behind (durable 35–73 s after the
  request at p50, `3fea1c0`, and 39–71 s over the hour-long runs at `fbe5785`) with up to 10.6 GB of dirty page cache;
  with blocking checkpoints
  without early write-back the stop at each checkpoint grows with the dirty data it syncs (0.24–0.27 s at 64 B and
  0.37–0.40 s at 1,024 B per second of half-rate data at `29960a3`, 0.75–0.80 s at about 250 MB/s in the finish round).
- **Remaining, measured:** a helper thread on the writer's CPU still delays it (up to 3.0 ms with
  `HelperCpus::Inherited`; the default places helpers elsewhere); at 90 % of the 64 B probe durability fell minutes
  behind at `279f59a` and, at `3fea1c0`, on aarch64 xfs; appends of 50–128 ms in 148–172 seconds of each 1,024 B
  steady-90 run on aarch64 ext4 (`3fea1c0`), not rolls, since attributed (2026-11-25, below: the write fault's mtime
  update waits for the ext4 journal's log writes, which the device completes late; `lazytime` removes them, but not
  the rolls'); the single stalls of 0.10–0.67 s without
  checkpoints at `279f59a` did not recur at `3fea1c0` (at most 13.2 ms); the aarch64 mapped reader's 10 ms maximum in
  the finish round (16-vCPU VMs), not seen in the second session or in a repeat at that round's commit on 8-vCPU VMs
  (2026-11-25, below), not explained. A spare segment on Linux is an option from 1.1, off by default (ADR 0049): it
  makes rolls cheaper away from the disk's limit, not the stalls rarer.
- **Remaining, open:** the record checksum mismatch that stopped a reader of the 1,024 B probe on both aarch64 VMs at
  `279f59a` was a reader's view lagging a correctly published record; readers now tolerate it, and the long verified
  runs at `3fea1c0` and the confirming runs at `fbe5785` showed no false corruption and no lost write (issue #43,
  closed; the platform mechanism is not confirmed, `crash-contract.md` → Known risks and limits). Not measured: the
  10,000- and 20,000-segment journals, and the lagging and beyond-RAM scenarios with background checkpoints.
- **Retention behind an overloaded writer (issue #45, closed).** The time-out of the 1,024 B probe at `279f59a` came
  from retention: before deleting a segment it read the whole segment on the checkpoint thread, behind a writer that
  outran the disk the old segments were no longer in memory, retention fell far behind, and the final checkpoint and the
  writer's shutdown waited for it; probably also the 64 B runs at 90 % of the probe that failed because background
  checkpoints did not finish in time. Since ADR 0044 (`3fea1c0`) a deletion reads only the end of the segment back to
  its seal, and the writer's retention run gives way to a waiting checkpoint and to `close`; every run of the second
  session finished. At `3fea1c0` that run gave way after one removal, so with a checkpoint always queued, as behind an
  unpaced writer, retention removed one segment per completed checkpoint: the long verified runs ended after
  1,075–1,304 s with the disk full at 349–368 GiB (above). **Addressed and measured:** since the amendment of ADR 0044
  (`fbe5785`) a run removes as many segments as the checkpoint before it covered, within the time that checkpoint took.
  The same command for the full hour (x86-64 ext4, aarch64 xfs) completed: retention removed 8,796–8,965 of the
  9,307–9,476 segments rolled with no error, and the journal stayed at 64.3–82.3 GiB (mean 68.6–73.0 GiB) against its
  64 GiB limit (Confirming long runs, above). **What remains true:** behind a writer that outruns the disk a checkpoint
  becomes durable tens of seconds after its request (39–71 s at p50, at most 104 s, in those runs), and the journal
  exceeds its limit by what was written since the last durable checkpoint (up to 18.3 GiB there), as it would under
  any retention rule, since no retention may remove the checkpoint segment or anything after it. An archiving writer
  (`Action::MoveTo`) can fall behind for as long as checkpoints stay queued (ADR 0044 → Amendment): not measured.
  Under sustained overload watch `tickertape status` (`segments.before_checkpoint`, `segments.total_bytes`, the
  checkpoint's age and the storage headroom thresholds).

## 2026-11-25, where the CPU time goes: profiles of the readers, notification and serialization (Google Cloud VMs)

Profiles of v1.0.0's library under the sustained workloads (`tt-sustained`, ADR 0026), for the roadmap item "Finish
eliminating avoidable work" (issue #62). They show where CPU time goes and what work remains per record. **The machines
have 4 vCPUs: the latencies, rates and CPU shares here are not comparable with the published tables above**, and nothing
here is a throughput or latency result. The comparisons inside this section are between runs on the same machine in the
same session. **Current `main` was not profiled; only v1.0.0's library was.** Since then #66 changed how a durable-only
reader reads the checkpoint marker, and later merges changed other files of the library.

### Environment and method

| | x86-64 | aarch64 |
|---|---|---|
| Machine | `c4-standard-4-lssd` (us-east1-b): Intel Xeon 6985P-C, 2 cores with 2 threads each, 15 GB | `c4a-standard-4-lssd` (us-east1-b): Google Axion (Neoverse-V2), 4 cores, 16 GB |
| Kernel, OS | 7.0.0-1017-gcp, Ubuntu 24.04.5 LTS | the same |
| Filesystem | ext4 (`-E lazy_itable_init=0,lazy_journal_init=0`, mounted `noatime`) on a local NVMe SSD (375 GB), write-back sysctls at the defaults | the same |
| Writer, reader CPU | 0, 1 (two cores); helper threads unpinned | 0, 1 (two cores); helper threads unpinned |
| rustc, perf, bpftrace | 1.100.0, 7.0.14, 0.20.2 | the same |

Commit `2a9b428`: v1.0.0's `crates/tickertape/src` (no difference) with the tools of this change in `benches/`. The
candidate below is `751ee43`. Both commits are kept by the branch `core/safe-reader-tail-reads` (`2a9b428` is an
ancestor of `751ee43`); do not delete it. **Virtual machines, not bare metal.** The guests expose no CPU cycles counter
(`perf stat -e cycles` reports it as not supported), so the profiles are `cpu-clock` samples at 1999 Hz: an interrupt in
a section with interrupts off is taken when they are turned on again, which blames short kernel sections on the
instruction after them (on aarch64 `finish_task_switch`, where a thread switched in is sampled, holds most of the
blocking zero-copy reader's samples below 1.5 million records/s). The profiled binaries are built with `RUSTFLAGS="-C
force-frame-pointers=yes" CARGO_PROFILE_RELEASE_DEBUG=1`; the encoding and candidate comparisons below use the ordinary
release build.

The runs are `tt-sustained run --dir /data/j --warmup 5s --checkpoint-every 1s --checkpoint-mode background
--retain-bytes 34359738368 --cpus 0,1` with `--rate` or `--unpaced`, `--payload` and `--readers` as in the tables, and
`--duration 28s` for the CPU profiles (a 20 s sample, starting 8 s in), 36 s for the counts, 20 s for the encoding
comparison and 25 s for the candidate; `/data` is the local SSD and `/data/j` is removed after each run. The replay
benchmark ran as `TMPDIR=/data/tmp taskset -c 1 <replay benchmark binary> --bench records`.
`benches/diagnostics/profile-sustained.sh DIR perf 20 -- <run>` records the CPU profiles of the writer's and the
reader's measuring thread with frame-pointer call graphs and folds them (`fold-stacks.py`, which names inlined
functions); `... counts 8 -- <run>` is a separate run that counts system calls and bytes read (`counts.bt`), heap
allocations with their stacks (`allocs.bt`) and `perf trace -s`, each for 8 s, so that counting does not distort a
profile. The kernel-side `bpftrace` counts slow the threads somewhat at high rates, so per-record counts of a reader
that falls behind are those of the observed run. Each pass is a complete run of the list; two passes on both machines
(on aarch64 one pass for the `default` wait mode, the `owned` readers and the no-reader runs). Cells show the range over
the two passes, x86-64 first, then aarch64 after a slash. One value for a machine means that its passes are equal, and
one value without a slash that both machines are. Rows marked † have one pass on aarch64. Counting runs whose window,
measured from before `bpftrace` started, was longer than 9.5 s are left out: three, of 9.6, 17 and 41 s (the last on the
candidate below), in which `bpftrace` ran past its 8 s. Windows of 8.5 to 9.3 s were kept; they include attaching the
probes, and the counts in them can cover slightly more than 8 s, which is why a count of exactly one per record can read
1.00 to 1.03. Raw output (folded stacks, count files, `result.json` and series) is kept outside the repository.

### The readers

Share of the reader thread's samples (%), and CPU of a core the thread used (%, from `getrusage`, not from the samples).
"64 KiB reads" are the safe reader's fills of its buffer; the wait mode is that of `--readers` (`spin`: wake-ups off;
`block`: blocks at once; `default`: spins and yields, then blocks). The rest of each row is the tool's latency
recording, loop bookkeeping and registration as a waiter.

| Reader, wait | Payload | Records/s | CPU of a core | Commit-word polls | 64 KiB reads: copy | 64 KiB reads: rest | Futex wait | Clock reads | CRC and decode | Checkpoint marker |
|---|---|---|---|---|---|---|---|---|---|---|
| safe spin | 64 B | 20,000 | 100 | 72 / 73 | 3 | 0 / 1 | 0 | 15 / 16 | 0 | 0 |
| safe block | 64 B | 20,000 | 27–28 / 12 | 15–20 / 14–15 | 30–42 / 27–29 | 6–7 | 26–32 / 41 | 2 / 4 | 1 | 0 |
| mapped spin | 64 B | 20,000 | 100 | 1 / 2 | 0 | 0 | 0 | 63 / 72 | 0 | 0 |
| mapped block | 64 B | 20,000 | 21–22 / 5 | 0 | 0 | 0 | 73–85 / 99–100 | 2–4 / 0 | 1–2 / 0 | 0 |
| safe spin | 64 B | 200,000 | 100 | 51 / 46–47 | 27 / 31–32 | 4 / 7 | 0 | 9–10 / 9 | 0 / 0–1 | 0 |
| safe block | 64 B | 200,000 | 71 / 81 | 14–16 / 19 | 32–35 / 40–41 | 5–6 / 10 | 31–32 / 21–22 | 2 / 3 | 1–2 / 1 | 0 |
| mapped spin | 64 B | 200,000 | 100 | 2–3 / 3 | 0 | 0 | 0 | 60 / 70 | 2 / 1 | 0 |
| mapped block | 64 B | 200,000 | 42–43 / 50–51 | 2 / 0 | 0 | 0 | 58–64 / 97–98 | 5–7 / 1 | 4 / 0 | 0 |
| safe spin | 64 B | 1,500,000 | 100 | 14 | 58–59 / 53–54 | 9–10 / 12 | 0 | 3 / 5 | 3–4 / 3 | 0 |
| safe block | 64 B | 1,500,000 | 99 | 13 / 14 | 59 / 53 | 9 / 12 | 0 | 3 / 5 | 3 | 0 |
| mapped spin | 64 B | 1,500,000 | 100 | 13 / 10 | 0 | 0 | 0 | 41 / 56 | 14–15 / 4 | 0 |
| mapped block | 64 B | 1,500,000 | 84–87 / 95–97 | 2–3 / 2 | 0 | 0 | 43–44 / 44 | 11–12 / 19 | 10 / 5 | 0 |
| safe spin | 1024 B | 20,000 | 100 | 72 / 73 | 3 | 0 / 1 | 0 | 14 / 16 | 0 | 0 |
| safe block | 1024 B | 20,000 | 27 / 12 | 26 / 15–17 | 24 / 28–29 | 7–8 / 6–7 | 28–29 / 37–40 | 1 / 4 | 2 | 0 |
| mapped spin | 1024 B | 20,000 | 100 | 1 / 2 | 0 | 0 | 0 | 63 / 72 | 1 / 0 | 0 |
| mapped block | 1024 B | 20,000 | 21–22 / 5 | 0 | 0 | 0 | 78–81 / 99 | 3 / 0 | 3–5 / 0 | 0 |
| safe spin | 1024 B | 200,000 | 100 | 46–48 / 45 | 31–32 / 32 | 5 / 7 | 0 | 8 / 9 | 2 | 0 |
| safe block | 1024 B | 200,000 | 74 / 81 | 15 / 18 | 39 / 41–43 | 5 / 9 | 27 / 19–21 | 2 / 3 | 3 / 2–3 | 0 |
| mapped spin | 1024 B | 200,000 | 100 | 4 | 0 | 0 | 0 | 56–57 / 68 | 6 / 2 | 0 |
| mapped block | 1024 B | 200,000 | 46–47 / 48–49 | 1 / 0 | 0 | 0 | 69–70 / 97–98 | 4 / 1 | 8 / 1 | 0 |
| safe default † | 64 B | 200,000 | 99 | 50 / 46 | 27–28 / 31 | 5 / 7 | 0 | 9 | 0 | 0 |
| mapped default † | 64 B | 200,000 | 99 | 2 / 3 | 0 | 0 | 0 | 59 / 70 | 2 / 1 | 0 |
| safe durable spin | 64 B | 200,000 | 100 | 0 | 0 | 0 | 0 | 1 | 0 | 97 |
| mapped durable spin | 64 B | 200,000 | 100 | 0 | 0 | 0 | 0 | 1 | 1 | 97 |
| mapped durable block | 64 B | 200,000 | 99 | 0 | 0 | 0 | 3 | 1 | 1 | 92 / 93 |

System calls per record appended, and bytes read, over 8 s (`open` counts the wake-up record and the checkpoint marker;
a value below 0.01 is shown as `<0.01`):

| Reader, wait | Payload | Records/s | 4-byte `pread` | Larger `pread` | Bytes read | Futex wait | `open` |
|---|---|---|---|---|---|---|---|
| safe spin | 64 B | 20,000 | 106–107 / 73–74 | 1.01 | 66,700–66,900 / 66,600–67,100 | 0 | 0 |
| safe block | 64 B | 20,000 | 3.02–3.03 / 3.02 | 1.01 | 66,400 / 66,100–66,200 | 1.00–1.01 / 1.00 | <0.01 |
| mapped spin | 64 B | 20,000 | 0 | 0 | 0 | 0 | 0 |
| mapped block | 64 B | 20,000 | 0 | 0 | 0.04 | 1.01 / 1.00–1.01 | <0.01 |
| safe spin | 64 B | 200,000 | 6.16–6.63 / 4.16–4.23 | 1.00 / 1.01 | 66,000–66,200 / 66,100–66,700 | 0 | <0.01 |
| safe block | 64 B | 200,000 | 2.00–2.02 / 2.18–2.29 | 0.99–1.00 / 0.95–0.97 | 65,100–65,600 / 62,700–63,500 | 0.50 / 0.55–0.57 | <0.01 |
| mapped spin | 64 B | 200,000 | 0 | 0 | <0.01 | 0 | <0.01 |
| mapped block | 64 B | 200,000 | 0 | 0 | <0.01 | 0.98–0.99 / 0.99–1.00 | <0.01 |
| safe spin | 64 B | 1,500,000 | 0.22–0.23 / 0.19 | 0.22 / 0.19 | 14,500–14,700 / 12,400 | 0 | <0.01 |
| safe block | 64 B | 1,500,000 | 0.22 / 0.19 | 0.22 / 0.19 | 14,600 / 12,400 | <0.01 | <0.01 |
| mapped spin | 64 B | 1,500,000 | 0 | 0 | <0.01 | 0 | <0.01 |
| mapped block | 64 B | 1,500,000 | 0 | 0 | <0.01 | 0.39–0.42 / 0.35–0.39 | <0.01 |
| safe spin | 1024 B | 20,000 | 99–101 / 73–74 | 1.01 | 66,600–67,000 / 66,400–66,600 | 0 | <0.01 |
| safe block | 1024 B | 20,000 | 3.02–3.03 / 3.02 | 1.01 | 66,100–66,500 / 66,000–66,100 | 1.00–1.01 / 1.00 | <0.01 |
| mapped spin | 1024 B | 20,000 | 0 | 0 | <0.01 | 0 | <0.01 |
| mapped block | 1024 B | 20,000 | 0 | 0 | 0.04 | 1.01 / 1.00–1.01 | <0.01 |
| safe spin | 1024 B | 200,000 | 5.34–5.75 / 3.97 | 1.00 / 1.00–1.01 | 65,400–65,700 / 65,900–66,200 | 0 | <0.01 |
| safe block | 1024 B | 200,000 | 1.73–1.88 / 2.12–2.22 | 0.84–0.91 / 0.95–0.97 | 55,100–59,800 / 62,800–64,100 | 0.42–0.47 / 0.51–0.53 | <0.01 |
| mapped spin | 1024 B | 200,000 | 0 | 0 | <0.01 | 0 | <0.01 |
| mapped block | 1024 B | 200,000 | 0 | 0 | <0.01 | 0.98 / 0.97 | <0.01 |
| safe default † | 64 B | 200,000 | 6.45–6.60 / 4.15 | 1.01 | 66,200–66,400 / 66,600 | <0.01 | <0.01 |
| mapped default † | 64 B | 200,000 | 0 | 0 | <0.01 | <0.01 | <0.01 |
| safe durable spin | 64 B | 200,000 | <0.01 | <0.01 | 104 | 0 | 0.55–0.58 / 0.42–0.43 |
| mapped durable spin | 64 B | 200,000 | 0 | 0 | <0.01 | 0 | 0.56–0.57 / 0.43 |
| mapped durable block | 64 B | 200,000 | 0 | 0 | <0.01 | 0.26–0.28 / 0.20 | 0.52–0.55 / 0.41 |

What the profiles show:

- **The safe reader reads up to 64 KiB for each record it follows live.** At 20,000 and 200,000 records/s it has
  0.84–1.01 buffer fills and reads 55–67 KB per record; spinning at 20,000 records/s that is 1.3 GB/s to follow 2 MB/s
  of 104-byte records. The copy alone is 24–59 % of the reader's samples, except in the spin runs at 20,000 records/s,
  where polling is most of them (3 %). At 1.5 million records/s the reader has caught up in batches: 0.2 fills and 12–15
  KB per record. It also issues a 4-byte `pread` for every poll of an empty slot: 73–107 per record when spinning at
  20,000 records/s, and three per record when blocking (the poll, the second check after registering as a waiter that
  the wake-up protocol needs, and the poll after the wake-up).
- **The zero-copy reader copies nothing** and makes no system call while it spins. Its spin loop is mostly clock reads:
  two `Instant::now` calls per empty poll (the timeout of `next_timeout` and the 100 ms identity check) take 41–63 % of
  the samples on x86-64 and 56–72 % on aarch64. Checking a record (CRC and decode) is 4–15 % at 1.5 million records/s of
  64 B, and 0–8 % at 1,024 B.
- **Blocking readers**: the futex wait and the wake-up are 19–41 % of the samples of the safe reader (none at 1.5
  million records/s, where it hardly blocks) and 43–85 % of the zero-copy reader's on x86-64. On aarch64 they are 97–100
  % of the zero-copy reader's samples up to 200,000 records/s and 44 % at 1.5 million; 70–73 % of the samples at 200,000
  records/s are in `finish_task_switch`, where `cpu-clock` sampling lands when a thread is switched in, so those shares
  do not measure what that function costs. At 200,000 records/s the safe reader makes about one futex wait per two
  records, the zero-copy reader one per record.
- **Durable-only readers** (64 B, 200,000 records/s, a checkpoint every second): 92–97 % of the samples are in reading
  the checkpoint marker: while at the boundary, every poll opens the marker file and reads all 128 KiB of it in 13
  `read` calls (0.4–0.6 opens per record appended), with about 16 allocator calls per poll on both machines (two
  `malloc`, the rest `realloc`: mostly the read buffer growing, and two that build the path). These are counted in a
  window of their own, in which the probes slowed the reader. A blocking durable-only reader is also woken by appends
  although only a checkpoint can give it records. Since #66 a refresh reads only the marker's slots, not the whole file;
  that has not been profiled again, and the open and close remain.

Allocations: none per record on either reader. `tests/allocation.rs`, with a counting allocator, asserts zero for
records already committed, with each wait mode configured, and for a spinning wait, and bounds a reader that blocks for
every record by the re-read of the wake-up record (every 100 ms). On the live runs `allocs.bt` finds 128–385 allocator
calls in 8 s in the reader thread (128–252 of them `malloc`): the tool's once-a-second JSON series, the re-read of the
wake-up record by a blocking reader (`notify::read_record`, about 80 times, two calls each) and a few per segment
change. `Record::to_owned` (the `owned` reader option) is one allocation per record. On aarch64 it counted 1.02–1.03 per
record appended, above 1 because the count covers slightly more than 8 s; on x86-64 it counted 0.65–0.68, because in
that window the reader read only 123,000–131,000 of the 200,000 records/s (its series), so about one per record read.

### The writer, notification and serialization

Share of the writer thread's samples (%); the rest is the tool (its schedule's busy wait, clocks and histograms), which
is most of a paced run. Futex wakes include the helper thread's signals, about one per 64 KiB appended.

| Run | CRC | Payload copy | Encoding | Publish: atomic | Publish: futex wake | Page faults | Checkpoint calls | Futex wakes per record |
|---|---|---|---|---|---|---|---|---|
| mapped-wait-block-64-20000 | 0.2 / 0.0–0.1 | 0.0 | 0.0 | 0.2 | 2.5–2.7 / 3.3–3.4 | 0.0 | 0.0 | 1.00–1.03 / 1.01 |
| safe-wait-block-64-200000 | 1.4 / 0.2–0.4 | 0.1 / 0.0–0.1 | 0.0 | 0.4 / 1.6–1.8 | 10.2–10.6 / 13.8–14.4 | 0.1 / 0.0–0.1 | 0.0 | 0.51 / 0.63–0.68 |
| mapped-wait-block-64-200000 | 1.6–1.7 / 0.2–0.3 | 0.0–0.1 / 0.0 | 0.0 | 0.4–0.5 / 1.8–1.9 | 8.9 / 32.8–32.9 | 0.1 | 0.0 | 1.00 |
| mapped-wait-spin-64-1500000 | 12.1–12.3 / 1.6 | 0.3–0.4 / 0.2 | 0.0 | 2.8 / 12.3–12.4 | 0.1–0.2 / 0.2–0.4 | 0.2 / 0.4 | 0.0 | <0.01 |
| mapped-wait-block-64-1500000 | 10.7–10.9 / 1.6–1.7 | 0.3 / 0.2–0.3 | 0.0 | 3.4–4.2 / 13.5–14.4 | 33.6–33.8 / 36.6 | 0.3–0.4 / 0.4–0.5 | 0.0 | 0.48–0.50 / 0.41–0.46 |
| mapped-wait-block-1024-200000 | 5.7 / 2.3–2.4 | 0.4 / 1.4 | 0.0 | 0.9–1.2 / 1.0–1.4 | 11.1–11.2 / 32.2–32.8 | 0.1 / 0.5–0.6 | 0.0 | 1.01 |
| none-64-200000 † | 0.3–0.4 / 0.2 | 0.0 | 0.0 | 0.1 / 1.0 | 0.0 / 0.1 | 0.1 | 0.0 | — |
| none-1024-200000 † | 2.5–3.2 / 2.4 | 0.3–0.4 / 1.4 | 0.0 | 0.1 / 0.3 | 0.4 / 0.5 | 0.1 / 0.6 | 0.0 | — |
| unpaced-prepared-64 | 7.8–8.0 / 4.2–4.6 | 0.6–0.7 / 0.5 | 0.0 | 2.8–2.9 / 28.2 | 0.6–1.0 / 0.8–0.9 | 0.0–2.6 / 0.3 | 3.1–3.3 / 0.4–0.5 | <0.01 |
| unpaced-buffer-64 | 6.7–7.0 / 4.2–4.3 | 1.6–1.7 / 0.5 | 1.7–1.9 / 1.1–1.2 | 2.6–2.7 / 27.2–27.3 | 0.6 / 0.8–0.9 | 2.7–2.9 / 0.2–0.3 | 2.9–3.1 / 0.5–0.6 | <0.01 |
| unpaced-in-place-64 | 7.3–7.6 / 4.2–4.3 | 0.4 / 0.1 | 0.8–0.9 / 1.1–1.2 | 2.7–2.9 / 25.5–26.1 | 0.6–0.7 / 0.9 | 2.3–3.3 / 0.2–0.3 | 3.2–3.6 / 0.5–0.6 | <0.01 |
| unpaced-prepared-1024 | 21.3–21.7 / 19.0–19.3 | 2.7–2.8 / 8.2–8.5 | 0.0 | 1.3–1.4 / 2.9–3.0 | 0.4–0.6 / 2.2–2.3 | 25.1–25.4 / 12.6–12.9 | 4.8–4.9 / 2.2–2.3 | 0.01 |
| unpaced-buffer-1024 | 19.1–19.4 / 17.0–17.2 | 3.8–4.2 / 7.1–7.3 | 11.6–12.0 / 12.9–13.2 | 1.4–1.5 / 3.0–3.1 | 0.5–0.7 / 2.1 | 21.0–21.7 / 11.1 | 3.8–4.1 / 1.7–1.9 | 0.01 |
| unpaced-in-place-1024 | 15.8–17.2 / 16.1 | 0.2–0.3 / 0.1 | 14.5–15.1 / 19.1–19.3 | 1.2–1.6 / 2.6–2.9 | 0.9–1.0 / 1.9–2.1 | 21.6–23.1 / 10.1–11.0 | 4.3 / 2.0–2.2 | 0.01 |

- **CRC and copy.** In the unpaced runs of 1,024 B the CRC is 16–22 %, the payload copy 3 % (x86-64) to 8 % (aarch64),
  page faults 21–25 % on x86-64 and 10–13 % on aarch64, the tool 35–50 %. Encoding straight into the record
  (`append_with`) removes the copy and encodes in mapped memory; see below.
- **The wake-up's atomic increment.** `publish` is one sequentially consistent increment of the event counter and one
  load per `append` (a `lock incl` on x86-64, the only locked instruction per record in the disassembly of the append
  path: the commit word's release store is a plain store there, and the helpers' mutex is taken once per 64 KiB). It is
  1–4 % of the writer's samples on x86-64 at 1.5 million records/s and unpaced, and at most 1.4 % at 200,000 records/s
  and below. On aarch64 it is 11–14 % at 1.5 million records/s, 26–28 % unpaced at 64 B, and at most 2.0 % at 200,000
  records/s and below. Almost all the aarch64 samples are at one address in the runtime's `__aarch64_ldadd4_acq_rel`
  (called out of line because the default target selects LSE atomics at run time), which in the ordinary build's layout
  is just after the `ldaddal`; the profiled binary's own disassembly was not kept. Samples at such an instruction also
  include the time it waits for the thread's earlier stores, so these shares are not the cost of the instruction alone.
- **Wake-ups of a blocked reader.** The writer makes one futex wake per record while a reader blocks between records
  (20,000 records/s). It makes more wakes than the reader makes waits where the reader blocks often: 0.48–0.50 against
  0.39–0.42 per record for the zero-copy reader at 1.5 million records/s on x86-64 (0.41–0.46 against 0.35–0.39 on
  aarch64), and 0.63–0.68 against 0.55–0.57 for the safe reader at 200,000 records/s of 64 B on aarch64. With the
  zero-copy reader blocking, the wake-ups take 9–11 % of the writer's samples on x86-64 and 33 % on aarch64 at 200,000
  records/s, and 34 % and 37 % at 1.5 million. A reader that spins costs the writer none of this; `append_batch`
  publishes one wake-up for a batch (ADR 0041).
- **System calls on the steady path.** None per record without a blocked reader. `request_checkpoint` once a second, a
  roll about every 1.3 million records of 104 B.

Copies of the payload and allocations. Allocated once and not per record: the CRC's tables, by a process's first record
long enough to use them (ADR 0024), and on macOS, once per writer, the mutex and condition variable shared with the
helper thread:

| Path | Copies of the payload | Allocations per record | System calls per record | Locks and atomics per record |
|---|---|---|---|---|
| `append`, prepared buffer | 1 (the caller's buffer into the mapping) | 0 | 0 (a futex wake only if a reader is registered) | release store of the commit word; sequentially consistent increment and load of the wake-up words |
| `append` after encoding into a buffer | 2 (encode, then copy) | 0 | as above | as above |
| `append_with` (encode in place) | 1 (encode) | 0 | as above | as above |
| `append_batch` | 1 per record | 0 | one futex wake per batch at most | the commit word per record; the wake-up once per batch |
| safe reader | 1 in the kernel (`pread` to the reader's buffer, at the tail 64 KiB for each record) | 0 | 2 or more `pread` at the tail, one per 64 KiB when behind | one acquire fence per poll that finds a record |
| zero-copy reader | 0 | 0 | 0 | a relaxed load of the commit word and an acquire fence; relaxed loads of the record's words |
| blocking wait, reader side | as the reader | 0, apart from the wake-up record's re-read every 100 ms (two allocator calls) | one futex wait per wait, plus 4 at each re-read | increment and compare-and-swap of the waiter count, load of the event counter and of the writer's flag |
| clocks, reader side | – | 0 | 0 (vDSO) | one clock read when `next_timeout` is called with a finite timeout, one per empty poll for that timeout and one for the 100 ms identity check; per block, one in `refresh_if_due` and one in `block_on` with a timeout |
| identity check, reader side | – | 0 | one `stat` per 100 ms at an empty slot | – |
| helper signals (prefault, early write-back) | – | 0 | one futex wake per 64 KiB appended, if the helper waits | one mutex lock per 64 KiB |

Where the writer's append time goes in the three ways of making a payload, unobserved release builds, three alternated
runs each (median of each run's append p50 in ns, encoding included, range over the runs):

| Payload, rate | Machine | Prepared, `append` | Encoded into a buffer, then `append` | `append_with` |
|---|---|---|---|---|
| 64 B, 200,000/s | x86-64 | 47 (47–50) | 51 (50–51) | 51 (51–53) |
| 64 B, 200,000/s | aarch64 | 122 (122–123) | 126 (126–127) | 124 (124–124) |
| 64 B, unpaced | x86-64 | 47 (47–49) | 50 (49–51) | 52 (51–52) |
| 64 B, unpaced | aarch64 | 123 (122–123) | 126 (125–132) | 124 (123–124) |
| 1,024 B, 200,000/s | x86-64 | 114 (114–116) | 171 (167–174) | 163 (160–163) |
| 1,024 B, 200,000/s | aarch64 | 231 (231–232) | 316 (315–317) | 268 (265–268) |

`prepared` does no encoding, so it is a floor, not a competitor; a field-by-field stand-in encoder (`--encode`) is what
the other two run. At 64 B the difference in p50 between `append_with` and encoding into a buffer is −2 to +2 ns and
changes sign between configurations (the run ranges are 0–7 ns wide). At 1,024 B `append_with` is faster by 8 ns on
x86-64 (ranges 160–163 against 167–174) and 48 ns on aarch64, net; the copy alone was not isolated.

### What remains, and whether it can go

**Safe reader**

| Work | Necessary? |
|---|---|
| A `pread` per empty poll, a second one of the slot after registering as a waiter | Necessary: the safe reader does not map files (ADR 0003), and the check after registering is the wake-up protocol's (ADR 0017). The first of the three polls of a blocking reader that blocks at once could be skipped |
| A 64 KiB `pread` for every record at the tail | Avoidable: a smaller fill there. Tried below, mixed result, not changed |
| Opening, reading (128 KiB in v1.0.0) and closing the marker file at every poll of a durable-only reader at the boundary | Avoidable: less of it since #66; the descriptor could be kept open (the marker file is only created or written in place; only something outside the library replaces it) |
| Copying a record out (`Record::to_owned`) | Asked for by the caller |
| The payload's copy into the reader's buffer | Necessary for the safe reader; the zero-copy reader does without it |

**Zero-copy reader**

| Work | Necessary? |
|---|---|
| A clock read per empty poll for the 100 ms identity check, and one for the timeout of `next_timeout` (which also reads the clock once per call with a finite timeout) | Avoidable in part: share one clock read between both. The spin loop burns the core anyway, so the gain is a shorter poll, not less CPU; not measured |
| CRC of every record before it is handed out | Necessary: it is what validates a record before any reference to it exists (ADR 0027) |
| Page faults (0.0008 per record of 104 B, 0.008 of 1,064 B) | Necessary: the first touch of the mapping; the kernel maps around a fault |

**Notification**

| Work | Necessary? |
|---|---|
| The sequentially consistent increment and load per `append` | The ordering of the commit before the waiter check is necessary while wake-ups are on. The unconditional increment is how ADR 0017 does it; a fence, with the increment only when a reader waits, is untested. `notify(false)` removes both. On aarch64 the increment is an outlined call (`-C target-feature=+lse` removes the call, for a build that does not need to run on ARMv8.0); not measured |
| Futex wakes while a reader blocks between records | One wake per sleeping reader is necessary for a single `append`; `append_batch` sends one per batch (ADR 0041). Wakes sent while a reader is registered but presumably not asleep, up to about a fifth of them (more wakes than waits, above), are avoidable; not measured |
| Wake-ups of a blocking durable-only reader by `append` | Avoidable: only a checkpoint (or a roll) can give it records; it needs its own event counter or waiter count, which changes the shared notify region's layout (ADR 0021) |
| Four system calls and two small allocator calls every 100 ms per blocked reader (re-read of the `notify` record), and a `stat` per 100 ms at an empty slot (identity check) | Necessary to follow a restarted writer and to notice a replaced segment; bounded by the interval |

**Serialization and append**

| Work | Necessary? |
|---|---|
| The payload copy in `append` | Avoidable by the caller with `append_with`; the figures above give its size |
| CRC of the frame | Necessary (the record's integrity check, `format.md`) |
| Page faults of the writer at 1,024 B unpaced | Partly avoidable by a larger prefault window; Linux keeps it small on purpose (ADR 0025) |
| A mutex and a futex wake per 64 KiB for the helpers | Necessary to hand them the position; about one wake per 600 records of 104 B in total |

### Candidate measured and left out: a smaller read at the tail

Branch `core/safe-reader-tail-reads` (`751ee43`) makes the safe reader read 4 KiB when it has caught up and grow back to
64 KiB when a buffer is wholly consumed. Before/after of the release builds, three alternated runs each of 25 s after 5
s of warm-up, latency in µs from the intended send time, medians over the three runs as base → new (safe reader; late
sends are sends more than 1 µs late; CPU of the reader thread). Between the three runs of one configuration p50 varied
by at most 4 %, p99 by up to 3.8 times and p99.9 by up to 2.3 times, so only large differences in the tails say
anything.

| Wait | Payload | Records/s | p50 | p99 | p99.9 | Late sends | CPU of a core |
|---|---|---|---|---|---|---|---|
| spin, x86-64 | 64 B | 20,000 | 2.07 → 0.761 | 2.58 → 1.25 | 5.3 → 4.56 | 0.02 → 0.03 % | 100 → 100 % |
| spin, aarch64 | 64 B | 20,000 | 2.5 → 1.05 | 2.92 → 1.5 | 7.11 → 5.57 | 0.02 → 0.00 % | 100 → 100 % |
| block, x86-64 | 64 B | 20,000 | 14.8 → 12.7 | 19.1 → 15.4 | 24.1 → 20.1 | 1.53 → 0.03 % | 23 → 22 % |
| block, aarch64 | 64 B | 20,000 | 6.22 → 4.68 | 7.14 → 5.54 | 10.2 → 8.79 | 0.00 → 0.02 % | 11 → 8 % |
| spin, x86-64 | 64 B | 200,000 | 2.06 → 0.753 | 4.22 → 2.17 | 9.97 → 7.94 | 0.68 → 0.59 % | 100 → 100 % |
| spin, aarch64 | 64 B | 200,000 | 2.49 → 1.04 | 4.13 → 2.04 | 6.89 → 3.84 | 0.05 → 0.08 % | 100 → 100 % |
| block, x86-64 | 64 B | 200,000 | 4.03 → 2.98 | 7.89 → 5.68 | 17.4 → 13.2 | 0.70 → 0.67 % | 72 → 56 % |
| block, aarch64 | 64 B | 200,000 | 3.69 → 4.25 | 6.72 → 5.5 | 10.1 → 7.74 | 0.08 → 0.15 % | 80 → 56 % |
| spin, x86-64 | 64 B | 1,500,000 | 3.15 → 0.801 | 14 → 22.3 | 46.2 → 64.6 | 4.02 → 4.33 % | 100 → 100 % |
| spin, aarch64 | 64 B | 1,500,000 | 3.65 → 1.04 | 6.68 → 3.37 | 32.1 → 30 | 1.22 → 1.34 % | 100 → 100 % |
| block, x86-64 | 64 B | 1,500,000 | 3.13 → 0.837 | 11 → 102 | 52.8 → 155 | 3.56 → 8.92 % | 99 → 99 % |
| block, aarch64 | 64 B | 1,500,000 | 3.67 → 1.05 | 7.28 → 4.45 | 36 → 32.2 | 1.31 → 2.22 % | 99 → 99 % |
| spin, x86-64 | 1,024 B | 20,000 | 2.6 → 1.3 | 5.95 → 3.31 | 15.7 → 15.1 | 0.09 → 0.14 % | 100 → 100 % |
| spin, aarch64 | 1,024 B | 20,000 | 2.73 → 1.25 | 5.24 → 3.75 | 11.7 → 10.6 | 0.02 → 0.01 % | 100 → 100 % |
| block, x86-64 | 1,024 B | 20,000 | 15.4 → 13.4 | 19.3 → 16.7 | 28.7 → 23.8 | 0.10 → 0.07 % | 24 → 21 % |
| block, aarch64 | 1,024 B | 20,000 | 6.47 → 4.78 | 8.57 → 6.92 | 15 → 13.6 | 0.01 → 0.01 % | 11 → 8 % |
| spin, x86-64 | 1,024 B | 200,000 | 2.59 → 1.3 | 7.47 → 4.44 | 21.1 → 19.8 | 1.35 → 1.74 % | 100 → 100 % |
| spin, aarch64 | 1,024 B | 200,000 | 2.7 → 1.26 | 5.5 → 3.61 | 25.5 → 23.5 | 0.58 → 0.96 % | 100 → 100 % |
| block, x86-64 | 1,024 B | 200,000 | 4.5 → 3.41 | 10.6 → 9.2 | 31.6 → 22.6 | 1.72 → 1.83 % | 74 → 59 % |
| block, aarch64 | 1,024 B | 200,000 | 3.95 → 4.57 | 9.15 → 7.47 | 27.5 → 25.3 | 0.55 → 0.94 % | 82 → 56 % |

It reads about 14–18 times fewer bytes per record at the tail (3.5–4.6 KB per record of 64 B at 20,000 and 200,000
records/s, polls included, one counting pass) and lowers p50 to 25–87 % of the base, except for the blocking reader at
200,000 records/s on aarch64, where it rises to 115–116 %; the CPU of the blocking reader falls at 200,000 records/s (72
→ 56 % and 74 → 59 % of a core on x86-64, 80 → 56 % and 82 → 56 % on aarch64). The cost: on x86-64 at 1.5 million
records/s the tails grow (blocking p99 11 → 102 µs and p99.9 53 → 155 µs, spinning p99.9 46 → 65 µs). On x86-64 at
200,000 records/s of 64 B the blocking reader also costs about twice the futex wakes (0.51 → 0.98 per record, one counting pass;
aarch64 falls from 0.63–0.68 to 0.52). At 1.5 million records/s the blocking reader blocked more often (2,700–3,200 →
47,000–50,000 voluntary context switches a run on x86-64) and the writer made more futex wakes (0.002 → 0.011 per
record) and more late sends (3.6 → 8.9 %). But aarch64 shows the same rise in blocking (2,400 → 30,000–39,000 switches,
wakes 0.002 → 0.0075) and no tail growth, and the spinning tail on x86-64 grew too, so the extra wakes alone do not
explain the tail; it is not explained. The replay benchmark (`cargo bench --bench replay`, filter `records`, three
interleaved variants, 8 rounds, base, a second copy of the base binary, new; median of the rounds, ms per pass of
1,000,000 records of 64 B or 100,000 of 1,024 B):

| Machine | Case | Base | Second copy of base | New | New against base, paired by round |
|---|---|---|---|---|---|
| x86-64 | safe, 64 B | 37.7 (36.1–39.4) | 37.6 (35.9–39.1) | 38.1 (37.0–40.0) | +2.1 % median, slower in 6 of 8 rounds |
| x86-64 | safe, 1,024 B | 13.9 (13.2–15.9) | 14.4 (13.2–17.6) | 15.1 (13.3–16.6) | +7.8 %, 7 of 8 |
| x86-64 | mapped, 64 B | 50.9 (50.3–53.6) | 51.2 (49.9–56.2) | 52.1 (51.2–54.4) | +1.6 %, 7 of 8 |
| x86-64 | mapped, 1,024 B | 11.0 (10.4–12.0) | 11.3 (10.4–12.1) | 11.7 (10.5–12.2) | +4.4 %, 6 of 8 |
| aarch64 | safe, 64 B | 38.4 (38.2–38.6) | 38.2 (38.1–38.7) | 38.3 (38.1–45.6) | −0.3 % of medians |
| aarch64 | safe, 1,024 B | 14.8 (14.7–15.0) | 14.9 (14.7–15.1) | 14.9 (14.7–15.6) | +0.6 % of medians |
| aarch64 | mapped, 64 B | 36.4 (35.7–37.8) | 36.1 (35.9–36.5) | 36.3 (35.8–40.9) | −0.2 % of medians |
| aarch64 | mapped, 1,024 B | 10.5 (10.4–10.6) | 10.6 (10.5–10.7) | 10.6 (10.3–11.1) | +0.5 % of medians |

On x86-64 the second copy of the base differs from the base by −0.0 to +3.6 % (medians of the rounds; −0.3 to +2.0 %
paired), so the replay is slightly slower there for both readers, also the zero-copy one, whose code the change does not
touch, beyond what that floor explains in some cases; on aarch64 nothing differs beyond it. **Next step.** Worth
pursuing, because the p50 and CPU gains are large and the 64 KiB read is confirmed by count, but not as a fixed 4 KiB
read. Try hysteresis (shrink only after several empty polls, grow on the first fully consumed buffer) with the code of
the per-record path unchanged, gated on x86-64 p99 at 1.5 million records/s and on replay (the view of the review of
this change and of the maintainers' agent). Not changed here.

## 2026-11-25, roll and journal stalls on Google Cloud VMs (Linux, x86-64 and aarch64)

Where the stalls of the sustained runs come from (issue #60), the repeat of ADR 0023's spare-segment experiment now
that rolls do not sync, and the before/after of the change that followed (ADR 0049; an option, off by default on
Linux). All of it ran on **Google Cloud virtual machines, not bare metal**, in one day's sessions; it is not compared
with earlier sections except where the text says so. The raw output (results, time series, traces, scripts) is kept
outside the repository.

### Environment and commands

Ubuntu 24.04, kernel 7.0.0-1017-gcp (`CONFIG_HZ=1000`, `PREEMPT_LAZY`), rustc 1.100.0, ext4 on the first 375 GB local
NVMe SSD (`mkfs.ext4 -E lazy_itable_init=0,lazy_journal_init=0`, 1 GiB journal, no fast commit; mounted `rw,noatime`),
xfs with defaults on the second (`rw,noatime,inode64,logbufs=8,logbsize=32k`), I/O scheduler `none`, `wbt_lat_usec`
2000, write-back sysctls at their defaults, 4 KiB pages. Journals on the SSD, results on the boot disk.

| Machine | CPU, vCPUs, memory | Time (UTC) | Used for |
|---|---|---|---|
| `c4a-standard-8-lssd`, us-central1-b | Google Axion (Neoverse-V2), 8, 31 GiB | 12:17–17:22 | everything aarch64 |
| `c4-standard-16-lssd`, us-east1-d | Xeon 6985P-C, 16 (8 cores), 58 GiB | 12:17–16:48 | everything x86-64 but the next row |
| `c4-standard-16-lssd`, us-east1-d (a second VM) | the same | 16:50–17:39 | x86-64 without checkpoints; 10 more steady-50 pairs |

Commits: `v1.0.0` (`1e8651d1`); `85726e34` (`main`, the "before" below); `03334481`, the "after": that `main` plus the
spare segment with one thread for the writer's life, always on (later commits on the branch made it an option, added
the storage-full fallback and tests; the code the measured runs took is otherwise the same). Builds: the "before" VMs
checked out `85726e34` from the repository; the "after" binaries were built from a `git archive` of `03334481` sent
over SSH. The runs record the sha256 of the line that names the archive (`3d123edb…`, in `binaries-v.txt`); `git
archive --format=tar.gz 03334481` reproduces exactly that line (archive sha256 `2b49c851…`): the source archive's
identity is verified; that the measured binary was built from it is not. What is recorded is that its `tt-sustained`
hash differs from `main`'s on the same VM (aarch64 `d52317c4…` against `f230c344…`, x86-64 `c348e7c5…` against
`83a87b49…`), that both later scripts recorded the same hash for it, and that its traced rolls do not create or
preallocate a file on the writer. Two builds preceded that are **not** the after build and are kept for that reason:
the directories named `INVALIDmainB` (a stale Cargo cache gave the binary of `main`; rerun), and the runs of the 18
visibility-only cells per machine (`m7-vis-*`, `q2-vis-*`) stopped at once with "retention runs only after
checkpoints" because the script passed `--retain-bytes` without checkpoints (`main` and the kept build were rerun as
`v-vis-*`; the thread-per-spare build was not). Experiments used the branch `exp/roll-60` (`b22c8fe9`, `v1.0.0` with
switches, never merged), the scripts are in its `exp60/` directory.

```sh
tt-sustained run --dir … --out … --json --payload 1024 --duration 60s --warmup 10s --retain-bytes 68719476736 \
  --checkpoint-every 1s --checkpoint-mode background --readers mapped,safe:wait=default,safe:durable:wait=block \
  --cpus 1,2,3,4,5,6 --rate <rate>      # 169,206 / 304,571 aarch64, 173,119 / 311,614 x86-64
tt-latency run --dir … --rate 100000 --count 1100000 --warmup 100000 --payload 1024 --reader mapped --json
```

The rates are the published steady-50 and steady-90 rates of the second sustained session above (half and 90 % of the
10-minute probe of these machine types). Runs of 60 s, so a 10-minute run has ten times the rolls and stalls. Every
run started on a deleted journal after `sync`; configurations were interleaved. In every before/after pair the
"before" build ran first (a future run should alternate); latencies in µs unless a cell says ms; "late" is sends more
than 1 µs late; ranges are over the runs. Coordinated omission is handled as above.

### Where the writer's thread waits

From traced runs (`offcpu.bt`, `events.bt`, `stalls.bt`, block requests; bpftrace 0.20.2), each kept only if untraced
runs showed the same stalls (below). Waits of the writer's thread of 3 ms or more:

| Run | Arch | Wait | Count | Total | Max |
|---|---|---|---|---|---|
| steady-90 (v1.0.0) | aarch64 | write fault: mtime update, waiting for the ext4 journal | 56 | 5.3 s | 138 ms |
| | | roll: `link`, the same wait | 2 | 0.13 s | 121 ms |
| | | roll: `open(O_CREAT)`, the same wait | 2 | 0.04 s | 24 ms |
| steady-90 (v1.0.0) | x86-64 | none | 0 | | |
| steady-50 (v1.0.0) | both | none | 0 | | |
| unpaced (v1.0.0) | aarch64 | write fault: write-back throttling (`balance_dirty_pages`) | 3,155 | 48.6 s | 27 ms |
| | | write fault: mtime update, journal | 16 | 3.3 s | 389 ms |
| | | roll: `mmap` waiting for `mmap_lock` | 79 | 1.1 s | 27 ms |
| | | roll: `open(O_CREAT)`, journal or buffer lock | 14 | 0.79 s | 131 ms |
| unpaced (v1.0.0) | x86-64 | write fault: write-back throttling | 2,854 | 43.5 s | 27 ms |
| | | roll: `fallocate`, journal or buffer lock | 27 | 2.5 s | 329 ms |
| | | roll: `mmap` waiting for `mmap_lock` | 71 | 0.90 s | 26 ms |
| | | roll: `open(O_CREAT)`, journal or buffer lock | 11 | 0.74 s | 206 ms |
| | | write fault: mtime update, journal | 1 | 0.18 s | 182 ms |

- **Appends of 50–140 ms that are not rolls (aarch64 ext4, 90 % of the probe).** A store into a page that is not yet
  writable takes a write fault; ext4 updates the file's modification time there (`file_update_time` →
  `ext4_dirty_inode`), which needs to change the block holding the inode, and waits in `do_get_write_access` while the
  committing journal transaction writes that block to the log. In the traced run all 49 writer faults of 20 ms or more
  lay (at least 90 % of their length) inside the logging phase of a commit, which covered 10.1 of the 65 s. The
  prefault helper's faults wait in the same place, so it cannot hide them. Logging is slow there because the device
  is: commits logged for 4 ms (p50), 129 ms (p90), at most 304 ms on aarch64, and at most 1 ms on x86-64 at the same
  share of its probe, where the same workload has no such stall; on aarch64 81 of the traced log writes took 20 ms or
  more, on x86-64 none reached 1 ms. **Why that device queues at 92 % of its probe, on this shape only, is not
  explained**; nothing traced inside the guest accounts for it.
- **Test of that: `lazytime`.** With the filesystem remounted `lazytime`, which keeps modification times out of the
  journal, the same workload has 3–10 seconds with a 50 ms append per run instead of 28–30 (three 60-second runs each,
  interleaved, one VM shape), and every one of them is a roll. The roll appends get longer (max 114–130 ms, against
  0.2–59 ms without `lazytime`): the waits that remain are the roll's own journalled calls. In an earlier session of
  the day (the same machine type, a VM that has since been lost; three runs each) the figures were 2–7 against 32–39.
- **Roll appends of 100–400 ms.** Seen unpaced, where the device is overloaded: 82–304 ms (aarch64) and 120–259 ms
  (x86-64) at most per run. Each call of a roll (create, `fallocate`, `mmap`, `link`, `unlink`) is a journalled
  operation or takes `mmap_lock`, and waits for the journal, for a buffer lock, or (for `mmap`) for a thread that
  holds `mmap_lock`, most likely the prefault helper throttled in its `madvise` (not traced to the holder). Most of an
  unpaced writer's lost time is throttling in page faults, by design of the kernel.
- **Roll p50.** Without any stall a roll appends in 142–184 µs with checkpoints: creating the file, `fallocate` (about
  65–100 µs) and the link. Without checkpoints 0.9–1.0 ms: not traced here; unmapping a 128 MiB segment took 0.4–0.65
  ms where a trace caught it, which is the likely rest.
- **Helper threads on the writer's CPU** (`--helper-cpus inherited`, steady-50): append maxima of 1.0–1.8 ms against
  0.18–0.26 ms with the default placement (three runs per cell). The expected explanation, the scheduler's base slice
  (2.8 ms), **does not fit**: 1.3–1.5, 1.1–1.6 and 1.7–1.8 ms (aarch64) and 1.2–1.5, 1.0–1.6 and 1.5–1.6 ms (x86-64)
  for slices of 0.7, 2.8 and 8 ms. The 3.0 ms of the earlier session was not reached. Not explained further.
- **The aarch64 mapped reader's 10 ms maximum.** Not reproduced: the finish round's command (180 s, 250,000 records/s,
  4 CPUs, helpers on CPUs 5–7) at its own commit (`f194c42`) and at `v1.0.0` gave reader maxima of 1.07–1.12 and
  1.10–1.31 ms (two runs each) on this 8-vCPU shape. It was seen on 16-vCPU VMs, which were not run; not explained.

### Larger prefault window, ranged checkpoint sync: rejected

Both were tried as remedies at steady-90 (`exp/roll-60`; three runs per cell). A larger window does not help: 8 MiB
left 26–30 seconds with a 50 ms append and 48 MiB 21–24, against 32–39 (aarch64, the earlier session, warm-up seconds
included). Dropping the checkpoint's whole-file `fdatasync` of the active segment (its ranged `msync` covers the same
data) so that prefaulted pages are not written back changed nothing (28–37 against 28–30; with an 8 MiB window 19–28),
and with the larger window x86-64 got worse (append max 1.4–5.9 ms, checkpoints durable after 190 ms). Neither is
kept.

### The spare segment, again (ADR 0023)

ADR 0023's cell: `tt-latency`, 100,000 records/s, 1,024 B, mapped reader, 30 interleaved rounds of 11 s each per
configuration and architecture on the freshly formatted ext4 (after one 12-second smoke run). In ADR 0023 one run in
four had a p99.9 of 26 ms.

| Arch | Configuration | p99.9 | p99.99 | max | Runs with p99.9 above 100 µs |
|---|---|---|---|---|---|
| aarch64 | no spare | 24–28 | 813–849 | 978–1,035 | 0 of 30 |
| | spare, synced | 22–25 | 675–712 | 861–920 | 0 of 30 |
| | spare, not synced | 22–25 | 675–700 | 865–905 | 0 of 30 |
| x86-64 | no spare | 30–33 | 843–914 | 1,099–1,190 | 0 of 30 |
| | spare, synced | 29–35 | 726–774 | 1,024–1,085 | 0 of 30 |
| | spare, not synced | 29–31 | 719–795 | 1,028–1,106 | 0 of 30 |

These 30 rounds used the build that starts a thread per spare, which was later rejected. They do **not** show that the
failure of ADR 0023 is gone: that experiment ran on a `c6id.xlarge`, of which ADR 0025 says "The small VM cannot
sustain about 106 MB/s for minutes: every configuration stalls there for hundreds of milliseconds. That is a disk
limit", so its 100,000 records/s was near the device's limit, while it is about 30 % of the probe of the machines
here. Only the 15 rounds below used the build that was kept. At steady-90 on x86-64 one of 8 runs with the synced
spare had an append of 52 ms and a checkpoint that took 328 ms to become durable; of the 8 runs without a spare two
had appends of 3.3 and 7.9 ms, and the 8 runs with an unsynced one at most 0.69 ms. One bad run proves nothing about
the sync, but nothing needs it either, so the change does not sync.

**Unpaced** (the same build with a thread per spare, unsynced; four 60-second runs per cell, interleaved, 1,024 B,
readers mapped and safe, checkpoints every second), the mapped reader's p99.9 is worse with a spare and the rolls'
median is better:

| Arch | Configuration | Roll append p50 | Roll append max | Mapped reader p99.9 |
|---|---|---|---|---|
| aarch64 | no spare | 0.22, 2.7, 2.8, 6.6 ms | 82–304 ms | 596, 598, 599, 602 |
| | spare | 49–51 | 0.11–298 ms | 649, 656, 658, 664 |
| | no spare, `lazytime` | 0.21, 4.2, 4.2, 5.3 ms | 279–336 ms | 604, 605, 605, 606 |
| | spare, `lazytime` | 56–58 | 158–329 ms | 653, 657, 660, 663 |
| x86-64 | no spare | 0.18, 0.21, 1.4, 1.5 ms | 120–259 ms | 528, 543, 549, 762 |
| | spare | 55–60 | 0.10–303 ms | 604, 626, 628, 814 |
| | no spare, `lazytime` | 3.5, 4.0, 4.9, 9.3 ms | 240–447 ms | 540, 543, 548, 746 |
| | spare, `lazytime` | 60–63 | 14–264 ms | 605, 615, 617, 622 |

The kept build was not run unpaced.

A spare prepared by a **new thread per segment** (as on macOS) cuts the roll append to about 50 µs but makes the
writer's worst append worse at steady-50 in every run: append max 364–381 µs against 190–215 (aarch64), 571–698
against 170–346 (x86-64), and the mapped reader's p99.9 184–194 against 112–127 and 209–228 against 74–148. Traced,
starting the thread costs the writer 26–40 µs; on aarch64 the writer took 60 page faults of about 370 µs over 88
rolls, which the kept variant does not have; their cause was not traced further. Rejected. The change keeps one thread
for the writer's life.

### Before and after (`85726e34` against `03334481`: without and with the spare segment)

aarch64 (interleaved, 1,024 B; checkpoints every second unless stated):

| Configuration | Runs | Late sends (%) | Append p99.9 | Append max | Roll append p50 | Roll append max | Mapped reader p99.9 | max |
|---|---|---|---|---|---|---|---|---|
| steady-50, ext4, before | 6 | 0.7–0.8 | 8.8–9.3 | 190–211 | 170–178 | 190–211 | 116–131 | 879 µs to 1.4 ms |
| steady-50, ext4, after | 6 | 0.7–0.7 | 8.4–9.2 | 74–132 | 47–48 | 54–57 | 36–41 | 760–795 |
| steady-90, ext4, before | 6 | 9.7–14.6 | 5.1–5.8 | 138–154 ms | 179–182 | 216 µs to 126.4 ms | 129–134 ms | 138–154 ms |
| steady-90, ext4, after | 6 | 11.3–14.2 | 5.1–5.5 | 139–144 ms | 51–53 | 105 µs to 111.6 ms | 127–135 ms | 139–146 ms |
| steady-50, xfs, before | 3 | 0.6–0.7 | 9.4–9.7 | 190–232 | 152–158 | 171–188 | 123–125 | 879–906 |
| steady-50, xfs, after | 3 | 0.6–0.6 | 9.5–9.7 | 227–340 | 59 | 97–104 | 54–91 | 814–815 |
| no checkpoints, half rate, ext4, before | 6 | 0.8–0.8 | 6.1–6.4 | 1.00–1.07 ms | 882–945 | 1.00–1.07 ms | 257–291 | 1.03–1.11 ms |
| no checkpoints, half rate, ext4, after | 6 | 0.7–0.8 | 5.7–6.3 | 743–765 | 721–730 | 743–765 | 38–47 | 804–820 |

x86-64 (the last four rows on the second VM):

| Configuration | Runs | Late sends (%) | Append p99.9 | Append max | Roll append p50 | Roll append max | Mapped reader p99.9 | max |
|---|---|---|---|---|---|---|---|---|
| steady-50, ext4, before | 6 | 1.9–2.2 | 7.4–8.5 | 174–187 | 137–162 | 174–187 | 80–96 | 783–905 |
| steady-50, ext4, after | 6 | 2.0–2.2 | 7.5–8.5 | 139 µs to 2.7 ms | 46–50 | 58–68 | 51–73 | 674 µs to 2.8 ms |
| steady-90, ext4, before | 6 | 4.5–6.3 | 7.4–7.7 | 208 µs to 1.0 ms | 144–161 | 208–786 | 349–496 | 778 µs to 1.4 ms |
| steady-90, ext4, after | 6 | 3.2–5.2 | 6.2–7.5 | 144–747 | 44–50 | 94–101 | 247–309 | 657–842 |
| steady-50, xfs, before | 3 | 1.2–2.1 | 8.1–8.2 | 195–212 | 150–167 | 195–212 | 75–135 | 776–868 |
| steady-50, xfs, after | 3 | 1.7–2.1 | 7.5–8.2 | 140–174 | 64–69 | 111–118 | 50–61 | 681–799 |
| steady-50, ext4, before | 10 | 1.2–2.1 | 7.4–8.2 | 182–437 | 147–158 | 179–197 | 80–172 | 764 µs to 1.0 ms |
| steady-50, ext4, after | 10 | 1.0–2.4 | 7.7–9.2 | 147–350 | 47–54 | 59–68 | 49–66 | 661–806 |
| no checkpoints, half rate, ext4, before | 6 | 1.4–1.9 | 5.8–6.1 | 1.07–1.23 ms | 1.03–1.04 ms | 1.07–1.23 ms | 427–443 | 1.14–1.26 ms |
| no checkpoints, half rate, ext4, after | 6 | 1.7–1.9 | 5.9–6.1 | 972 µs to 1.2 ms | 917–940 | 972 µs to 1.2 ms | 285–335 | 1.00–1.20 ms |

ADR 0023's `tt-latency` cell (15 interleaved rounds each), p99.9 / p99.99 / max: aarch64 24–28 / 811–854 / 974–1,030
before, 22–25 / 674–704 / 847–887 after; x86-64 32–34 / 888–933 / 1,126–1,191 before, 29–33 / 739–799 / 1,033–1,092
after.

- Away from saturation the roll append (p50) falls from 137–182 to 44–54 µs with checkpoints on ext4 (150–167 to 59–69
  µs on xfs), and the mapped reader's p99.9 falls by roughly a quarter to 85 %, on both architectures and filesystems.
  Without checkpoints the roll stays long (0.72–0.94 ms against 0.88–1.04).
- **Costs and limits.** (1) At the saturated aarch64 steady-90 the kept build met a stall at rolls more often: 5 of 6
  runs with a roll of 20 ms or more against 2 of 6 for `main`, 10 against 3 seconds with one. Pooling the session's
  builds, spawned and kept threads alike: 16 of 22 against 12 of 32 (two-sided Fisher p = 0.014, a test chosen after
  seeing the data). What the writer loses overall does not change: 30.5 against 29.5 seconds with an append of 20 ms
  or more (medians, p = 0.87), append max 140 ms in both, late sends 12.4 against 12.3 %. (2) On aarch64 xfs the
  append max is higher (227–340 against 190–232 µs, three runs). (3) One x86-64 steady-50 run of 16 had a 2.7 ms
  append that was not a roll; it did not recur in the 10 later ones.
- **Not measured with the kept build:** unpaced, xfs at steady-90, `HelperCpus::Inherited`, 4 vCPUs, bare metal. Not
  measured at all: other filesystems, payloads other than 1,024 B, and (beyond the functional tests) a full disk. For
  that reason the spare segment is off by default on Linux (ADR 0049).

### Tracing overhead

The same runs (v1.0.0) with `offcpu.bt` (waits of 3 ms or more listed), `events.bt`, `stalls.bt` (system calls and
`handle_mm_fault` entry and return, jbd2 commits) and block issue and complete probes attached, and without,
interleaved on the same VM: four pairs at steady-90, three unpaced pairs. bpftrace 0.20.2, kernel 7.0.0-1017-gcp, on
ext4.

| Run | Arch | Tracing | Achieved rate | Append p50 | Append p99.9 | Append max | Late sends (%) | Seconds with an append ≥ 50 ms |
|---|---|---|---|---|---|---|---|---|
| steady-90 | aarch64 | off | 304,571 | 0.35 | 4.2–4.5 | 137–142 ms | 12.3–14.1 | 28, 33, 35, 35 |
| | | on | 304,571 | 0.35 | 4.7–5.2 | 141–143 ms | 11.0–12.7 | 31, 35, 35, 36 |
| steady-90 | x86-64 | off | 311,614 | 0.56 | 5.2–6.3 | 610–752 µs | 2.3–3.8 | 0 |
| | | on | 311,614 | 0.57–0.58 | 6.6–7.2 | 220–900 µs | 2.0–3.8 | 0 |
| unpaced | aarch64 | off | 403,438–407,494 | 0.30–0.31 | 10.3–10.4 | 277–371 ms | – | – |
| | | on | 402,928–406,386 | 0.30–0.31 | 11.4–11.5 | 339–347 ms | – | – |
| unpaced | x86-64 | off | 472,194–475,924 | 0.50 | 8.9 | 258–357 ms | – | – |
| | | on | 471,191–474,713 | 0.50 | 9.3–9.4 | 75–310 ms | – | – |

Tracing costs the writer's p99.9 about 0.5 µs (median difference) at steady-90 and 0.5–1.1 µs unpaced; throughput,
p50, maxima, late sends and the stalls are unchanged within the spread. The tracers' own processes used 3.4–3.5 s of
CPU per 70-second aarch64 run, 1.4–1.5 s on x86-64. The conclusion it rests on is thereby confirmed untraced: the
aarch64 stalls occur as often without any probe.

## 2026-11-02, v0.3.0 on Linux (AWS) and macOS

### Environments

**Linux (primary).** An AWS EC2 `c6id.xlarge` in us-east-1. It is a Nitro **cloud VM, not bare metal**:
- Intel Xeon Platinum 8375C (Ice Lake) at 2.9 GHz, 2 cores / 4 threads, 8 GiB of memory;
- Ubuntu 24.04, kernel 7.0.0-1014-aws;
- the journal directory on the instance's local NVMe disk, ext4, formatted with
  `-E lazy_itable_init=0,lazy_journal_init=0` and mounted `noatime`;
- rustc 1.99.1 and OpenJDK 21.0.12.1.

A fresh instance was used for each recording.

| Recording | Commit |
|---|---|
| tickertape's own benchmarks | `ce35edc` |
| comparison | `cdfd744` (same library as `ce35edc`; only the comparison runner changed) |

**macOS (secondary).** The owner's laptop:
- Apple M4 Pro, 14 cores, 24 GiB of memory;
- macOS 27.0.1, APFS on the internal SSD;
- rustc 1.99.1 and Homebrew OpenJDK 21.0.13.0.3.

| Recording | Commit |
|---|---|
| tickertape's own benchmarks | `c5be4be` |
| comparison | `98ce513` |

Both behave on macOS exactly as v0.3.0 does: the later changes affect only Linux and other non-Apple systems.
The laptop was **not idle**. A system photo-analysis process (`mediaanalysisd`) used one to two cores throughout,
and the load average was about 2.4 at the start. Run-to-run variance on the Mac is therefore much larger than on
Linux, especially for append throughput.

**Commands.**
- Linux: `TMPDIR=/data/tmp CARGO_TARGET_DIR=/data/target benches/run-recorded.sh OUTDIR`, then
  `JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64 TMPDIR=/data/tmp compare/run-all.sh OUTDIR`.
- macOS: the same commands with `TMPDIR=$HOME/tmp` and Homebrew's JDK 21.

Every configuration runs twice; ranges below are the two runs. Latency tools schedule sends at a fixed rate and
measure from the **intended** send time (coordinated omission accounted for), as described in `benches/README.md`
and `compare/README.md`.

### Comparison with other tools, Linux

What each tool promises differs, so the comparison is about mechanisms, not equivalent products.
`compare/README.md` has the full mapping. In short:
- **tickertape** and **Chronicle Queue** are persistent, replayable journals with independent readers.
- **Aeron** IPC is an in-memory transport with no persistence or replay.
- **yaque** is a consuming single-reader queue.
- **queue-file** is a FIFO file that forbids concurrent processes, so it has no live latency result.
- **plain file** is a length-prefixed file with no validation or recovery: a lower bound for "just write to a file".

**Aeron's results on this machine are not representative.** Its media driver runs dedicated busy-spinning threads.
With two client conductors and a busy-spinning writer and reader, that is more spinning threads than the four
vCPUs. Aeron's latency here measures CPU oversubscription, not Aeron.

**W1, append throughput** (million records/s; 1,000,000 × 64 B or 100,000 × 1,024 B):

| Tool | 64 B | 1,024 B |
|---|---|---|
| **tickertape** | 18.57–18.67 | 2.35–2.37 |
| chronicle | 3.49–3.76 | 0.90–0.98 |
| yaque | 2.29 | 1.09 |
| queue-file | 0.70 | 0.44 |
| plain-file | 2.49–2.51 | 1.24–1.26 |

**W3, warm replay** (million records/s, including open and validation, page cache warm):

| Tool | Reader | 64 B | 1,024 B |
|---|---|---|---|
| **tickertape** | safe | 24.79–25.11 | 3.44–3.50 |
| **tickertape** | mapped | 26.21–26.26 | 3.50–3.52 |
| chronicle | default | 12.71–12.82 | 3.48–4.00 |
| yaque | default | 19.27–19.71 | 4.18–4.27 |
| queue-file | default | 1.15 | 0.97 |
| plain-file | default | 3.83–3.84 | 2.64–2.66 |

**W2, writer-to-reader latency** (µs, p50 / p99 / p99.9 / max over 1,000,000 records after 100,000 of warm-up; "late
sends" counts sends that started more than 1 µs after their intended time). tickertape has two readers:
- `safe` reads with `pread` and is memory-safe whatever other processes do;
- `mapped` is the zero-copy reader.

64 B at 10,000 records/s:

| Tool | Reader | p50 | p99 | p99.9 | max | late sends |
|---|---|---|---|---|---|---|
| **tickertape** | safe | 2.6 | 3–3.1 | 20–21 | 581–608 | 79–164 |
| **tickertape** | mapped | 0.3 | 0.6 | 16–17 | 623–936 | 102–121 |
| chronicle | default | 0.5 | 0.7 | 2.6–7.8 | 676–5,636 | 390–666 |
| aeron | default | 0.4 | 5,218–5,263 | 6,410–6,570 | 13,984–14,991 | 219,615–226,535 |
| yaque | default | 1.4 | 3.8 | 7.6–7.7 | 2,062–3,021 | 193–239 |
| plain-file | default | 1 | 3.2–10 | 6.5–12 | 2,501–2,988 | 184–110,083 |

1,024 B at 10,000 records/s:

| Tool | Reader | p50 | p99 | p99.9 | max | late sends |
|---|---|---|---|---|---|---|
| **tickertape** | safe | 3.2–3.3 | 16–19 | 26–32 | 1,265–5,464 | 647–110,441 |
| **tickertape** | mapped | 1–1.1 | 2.6–11 | 42–45 | 1,337–1,390 | 563–110,543 |
| chronicle | default | 0.6 | 2.6–2.9 | 51 | 1,332 | 857–4,900 |
| aeron | default | 0.6 | 5,190–5,280 | 6,681–7,115 | 13,984–60,555 | 221,608–235,108 |
| yaque | default | 1.8–1.9 | 6.4–6.5 | 4,637–4,768 | 17,646–17,777 | 205–547 |
| plain-file | default | 1.4–1.5 | 4.8 | 6.4 | 617–1,411 | 128–162 |

64 B at 100,000 records/s:

| Tool | Reader | p50 | p99 | p99.9 | max | late sends |
|---|---|---|---|---|---|---|
| **tickertape** | safe | 2.6 | 9.7–12 | 20–21 | 93–142 | 13,049–15,623 |
| **tickertape** | mapped | 0.3 | 6–10 | 26–28 | 85–101 | 15,922–18,087 |
| chronicle | default | 0.4 | 11–12 | 38–47 | 783–3,795 | 38,758–39,477 |
| aeron | default | 0.4 | 5,116–5,558 | 7,176–7,193 | 9,576–10,846 | 265,712–274,628 |
| yaque | default | 1.4–1.5 | 6.8–11 | 11–16 | 214–227 | 15,106–15,591 |
| plain-file | default | 0.9 | 7.2–8.9 | 13–14 | 220–1,252 | 13,886–15,547 |

1,024 B at 100,000 records/s:

| Tool | Reader | p50 | p99 | p99.9 | max | late sends |
|---|---|---|---|---|---|---|
| **tickertape** | safe | 3.2 | 19–20 | 31–32 | 485–1,528 | 19,764–23,340 |
| **tickertape** | mapped | 1 | 29 | 62–70 | 1,280–1,467 | 37,820–39,154 |
| chronicle | default | 0.5–0.6 | 34–36 | 76–142 | 1,655–2,443 | 58,761–60,137 |
| aeron | default | 0.6 | 4,903–5,505 | 6,484–7,217 | 9,175–13,238 | 231,003–269,348 |
| yaque | default | 1.9 | 11,084–12,132 | 22,462–22,856 | 24,379–24,412 | 18,635–19,468 |
| plain-file | default | 1.3 | 11 | 17 | 111–135 | 19,730–23,728 |

What the Linux comparison shows:

**Append.** tickertape appends about 5× faster than Chronicle Queue at 64 B and about 2.5× at 1,024 B. It also beats
the plain-file baseline, which needs one `write(2)` per record.

**Replay.**
- At 64 B tickertape is fastest: 24.8–26.3 million records/s, against yaque's 19.3–19.7 and Chronicle Queue's
  12.7–12.8.
- At 1,024 B yaque is faster (4.18–4.27), and Chronicle Queue (3.48–4.00) is level with tickertape (3.44–3.52).

**Latency.**
- Chronicle Queue has the lowest medians: 0.4–0.6 µs, against 0.3–1.1 µs for tickertape's zero-copy reader and
  2.6–3.3 µs for its safe reader.
- At 100,000 records/s, tickertape's tails (p99.9) beat Chronicle Queue's for both payload sizes: at 1,024 B, 31–70 µs
  against 76–142 µs.
- At 64 B and 10,000 records/s, Chronicle Queue's tail is lower: 2.6–7.8 µs against 16–21 µs.
- The plain-file baseline and yaque have lower p99.9 in some cells. Neither validates records against corruption
  or offers multi-reader replay, and yaque's 1,024 B tails reach milliseconds.

### tickertape's own benchmarks, Linux

`tt-latency` (writer-to-reader visible latency at 100,000 records/s; µs; two runs). On Linux `WriterConfig::prefault`
has no effect (ADR 0023), so the `prefault` and `no-prefault` rows are the same configuration measured twice.

| Payload | Reader | Wait | p50 | p99 | p99.9 | p99.99 | max |
|---|---|---|---|---|---|---|---|
| 64 B | mapped | spin | 0.28 | 8.3–8.4 | 27–31 | 42–141 | 200–693 |
| 64 B | mapped | spin, `--no-prefault` | 0.28 | 4.2–7.7 | 28–30 | 46–47 | 233–302 |
| 64 B | safe | spin | 2.6 | 13 | 20–21 | 29–34 | 98–191 |
| 64 B | safe | default (blocks on the wake-up region) | 2.6 | 9.1–12 | 19–21 | 29–34 | 64–106 |
| 64 B | safe | sleep (poll with back-off) | 2.5–2.6 | 8.5–9.0 | 20–21 | 30–35 | 105–285 |
| 1,024 B | mapped | spin | 0.95–0.97 | 28–30 | 72–79 | 1,064–1,193 | 1,502–1,767 |
| 1,024 B | mapped | spin, `--no-prefault` | 0.95–0.96 | 28 | 64–67 | 1,018–1,028 | 1,346–1,451 |
| 1,024 B | safe | spin | 3.1 | 18–19 | 29 | 444–452 | 783–792 |
| 1,024 B | safe | default | 3.1 | 18 | 28–30 | 377–431 | 697–759 |
| 1,024 B | safe | sleep | 3.1 | 20–21 | 33–36 | 354–608 | 494–1,032 |

`tt-checkpoint` (16 MiB segments, 1,000 records between checkpoints, 200 checkpoints):

| Payload | `checkpoint()` p50 | p99 | Roll append | Other appends, p99.9 |
|---|---|---|---|---|
| 64 B | 0.32–0.33 ms | 0.42–0.47 ms | 0.27–0.29 ms (one roll) | 4.3–4.5 µs |
| 1,024 B | 0.64 / 7.4 ms | 8.3 ms | p50 0.23–0.31 ms, max 0.34 ms (12 rolls) | 33–35 µs |

The two 1,024 B runs differ in `checkpoint()` p50 (0.64 ms and 7.4 ms); both are shown.

Criterion (point estimates):

| Benchmark | Result |
|---|---|
| append, 64 B | 20.5 / 20.8 million records/s |
| append, 1,024 B | 2.54 / 2.76 million records/s |
| replay, 64 B, safe / zero-copy | 24.6 / 25.7 million records/s |
| replay, 1,024 B, safe / zero-copy | 3.50 / 3.28 million records/s |

The append rows are the `prefault` and `no-prefault` variants. Those are the same configuration on Linux, so their
difference shows the noise: up to 9 %.

Cold-cache replay (`tt-latency --cold`, page cache dropped with `drop_caches`, safe reader):
- 64 B: 9.1 million records/s, 581–584 MB/s of payload.
- 1,024 B: 0.54 million records/s, 553–555 MB/s.

The NVMe instance store is the limit here.

### macOS (secondary)

Comparison (same suite and workloads, `98ce513`). Aeron has enough cores here, and its tails are low.

W1 append and W3 warm replay (million records/s):

| Tool | W1, 64 B | W1, 1,024 B | W3, 64 B | W3, 1,024 B |
|---|---|---|---|---|
| **tickertape** (safe / zero-copy for W3) | 10.74–11.03 | 0.96–0.98 | 51.78–53.25 / 47.52–49.32 | 7.47–7.55 / 6.63–6.65 |
| chronicle | 10.38–10.60 | 4.02–4.12 | 28.64–38.37 | 3.26–3.62 |
| yaque | 0.32–0.33 | 0.27 | 30.88–31.21 | 6.79–6.81 |
| queue-file | 0.16 | 0.13 | 1.14–1.16 | 0.99–1.01 |
| plain-file | 0.32–0.33 | 0.26–0.27 | 3.24–3.29 | 2.74–2.83 |

W1 on this Mac varied far more between recordings than between the two runs of one recording. An earlier recording
the same day, at `fef4be5` before the background segment preparation of ADR 0022, measured tickertape at 23.5–26.0
million records/s at 64 B and 2.59–2.61 at 1,024 B. Treat the Mac W1 column as unreliable under background load.

W2 at 100,000 records/s (µs, p50 / p99 / p99.9 / max):

| Tool | 64 B | 1,024 B |
|---|---|---|
| **tickertape**, zero-copy | 0 / 0 / 5–8 / 74–77 | 0 / 2–3 / 1,922–2,255 / 3,844–4,649 |
| **tickertape**, safe | 1 / 2–7 / 13–43 / 507–522 | 2 / 12–36 / 1,288–1,309 / 3,568–3,623 |
| chronicle | 0 / 1 / 17–19 / 1,388–1,983 | 0 / 517–703 / 2,537–3,672 / 3,924–6,132 |
| aeron (IPC, no persistence) | 0 / 1 / 66–120 / 728–1,663 | 0 / 1 / 88–94 / 769–1,249 |

The Mac's `tt-latency` timer resolution is 1 µs, so "0" means below 1 µs. On APFS a roll still takes about 3.5 ms
(the header sync), which dominates the 1,024 B tails.

tickertape's own benchmarks on macOS:

| Benchmark | With background work (`prefault(true)`) | Without (`prefault(false)`) |
|---|---|---|
| append, 64 B (Criterion) | 9.96 million records/s, 776 MiB/s | 1.89 million records/s, 305 MiB/s |
| append, 1,024 B (Criterion) | 1.87 million records/s, 1.88 GiB/s | 0.65 million records/s, 612 MiB/s |
| `tt-latency`, 64 B, zero-copy, p99.9 | 4–28 µs | 149–249 µs |
| `tt-latency`, 1,024 B, zero-copy, p99.9 | 2.5–2.8 ms | 23–38 ms |

Other macOS numbers:
- Replay (Criterion): 55.4 / 50.9 million records/s at 64 B (safe / zero-copy), 8.34 / 7.21 at 1,024 B.
- `checkpoint()` p50: 8.0–9.6 ms. It ends with `F_FULLFSYNC`, which flushes the drive's cache.
- Roll appends: 3.3–3.7 ms.

## 2026-10-31, after the reader change (v0.2 work, commit `a2d4530`)

Same machines, filesystems, toolchain and command (`benches/run-recorded.sh`, plus a second Criterion invocation) as
the v0.1.0 section below; both runs at commit `a2d4530`, where readers validate records without a per-record
allocation. The writer did not change, so `checkpoint()` and roll latency are as in the v0.1.0 section (the raw
output of this run is consistent with it and not repeated here).

**Replay throughput** (warm page cache; millions of records per second, Criterion point estimates, two runs):

| Benchmark | Mac run 1 | Mac run 2 | VM run 1 | VM run 2 | v0.1.0, same machines |
|---|---|---|---|---|---|
| replay, 64 B, safe reader | 43.3 | 43.6 | 35.1 | 35.3 | Mac 24.9–25.0, VM 28.9–29.0 |
| replay, 64 B, zero-copy reader | 33.5 | 33.8 | 31.1 | 31.0 | Mac 22.1–22.4, VM 18.4–27.6 |
| replay, 1,024 B, safe reader | 8.40 | 8.48 | 7.19 | 7.39 | Mac 6.19–6.74, VM 5.94–6.34 |
| replay, 1,024 B, zero-copy reader | 7.25 | 7.25 | 6.49 | 6.49 | Mac 5.73–5.96, VM 2.57–5.43 |
| append, 64 B (unchanged code) | 5.14 | 5.05 | 13.0 | 9.56 | Mac 3.52–5.64, VM 9.52–10.95 |
| append, 1,024 B (unchanged code) | 0.53 | 0.66 | 4.78 | 4.84 | Mac 0.72–0.79, VM 4.60–4.90 |

The zero-copy reader is still slower than the safe reader at replay: it validates from 8-byte atomic loads, which
can't be vectorised, while the safe reader validates in place in its `pread` buffer. Append (unchanged code) again
varies by more than 10 % between runs.

**Cold page-cache replay, OrbStack VM** (guest page cache dropped; the host may still cache the VM disk): 64 B
17,598,485 and 17,699,993 records/s (1,126 and 1,133 MB/s payload); 1,024 B 3,618,309 and 3,799,411 records/s
(3,705 and 3,891 MB/s). Not measured on the Mac (needs root).

**Writer-to-reader visible latency**, same parameters as below (100,000 records/s, 1,000,000 measured records, µs;
"<1" = below the macOS clock's 1 µs resolution):

| Machine | Run | Payload | Reader | p50 | p90 | p99 | p99.9 | p99.99 | max | Late sends |
|---|---|---|---|---|---|---|---|---|---|---|
| Mac | 1 | 64 B | zero-copy | <1 | <1 | 47 | 264 | 1,402 | 1,833 | 14,385 |
| Mac | 1 | 64 B | safe | 1.0 | 2.0 | 25 | 159 | 238 | 285 | 152 |
| Mac | 1 | 1,024 B | zero-copy | <1 | 110 | 13,967 | 16,974 | 18,022 | 18,301 | 183,305 |
| Mac | 1 | 1,024 B | safe | 2.0 | 98 | 27,869 | 49,185 | 53,510 | 54,297 | 72,278 |
| Mac | 2 | 64 B | zero-copy | <1 | <1 | 70 | 275 | 2,443 | 3,432 | 20,129 |
| Mac | 2 | 64 B | safe | 1.0 | 2.0 | 27 | 175 | 246 | 289 | 298 |
| Mac | 2 | 1,024 B | zero-copy | <1 | 83 | 12,976 | 16,703 | 17,908 | 18,252 | 179,597 |
| Mac | 2 | 1,024 B | safe | 2.0 | 97 | 21,250 | 25,575 | 27,099 | 27,591 | 74,837 |
| VM | 1 | 64 B | zero-copy | 0.15 | 0.20 | 0.41 | 411 | 3,058 | 4,002 | 3,088 |
| VM | 1 | 64 B | safe | 1.5 | 1.7 | 2.2 | 158 | 514 | 1,445 | 2,761 |
| VM | 1 | 1,024 B | zero-copy | 0.35 | 0.48 | 647 | 39,879 | 48,529 | 49,480 | 26,325 |
| VM | 1 | 1,024 B | safe | 1.7 | 2.5 | 52,429 | 138,019 | 146,538 | 147,587 | 34,618 |
| VM | 2 | 64 B | zero-copy | 0.15 | 0.20 | 0.54 | 250 | 3,232 | 4,215 | 4,343 |
| VM | 2 | 64 B | safe | 1.5 | 1.7 | 2.5 | 40 | 396 | 515 | 3,071 |
| VM | 2 | 1,024 B | zero-copy | 0.35 | 0.41 | 136,970 | 223,478 | 232,260 | 233,177 | 37,704 |
| VM | 2 | 1,024 B | safe | 1.8 | 4.5 | 337,379 | 424,673 | 433,324 | 434,373 | 77,397 |

As before, medians stay at a few µs or below, and the tails at 1,024 B are dominated by the writer falling behind
its schedule (late sends); in the VM those tails reached hundreds of milliseconds in run 2. The reader change did not
change the medians.

## 2026-10-31 (v0.1.0, commit `ff24fd1`)

### Environments

| | Owner's Mac | OrbStack VM (Linux on macOS) |
|---|---|---|
| Hardware | MacBook Pro, Apple M4 Pro, 14 cores, 24 GB RAM, internal SSD | Same Mac. Virtualised: **not bare-metal Linux** |
| OS / kernel | macOS 27.0.1 (Darwin 27.0.0) | Ubuntu 26.04.1 LTS, Linux 7.2.12-orbstack, aarch64, 14 vCPUs (`lscpu`), 11 GB RAM |
| Journal filesystem | APFS (`$TMPDIR` on the Data volume) | btrfs (VM disk, `$HOME/tmp`) |
| Commit | `ff24fd1` | `8989c51` (differs from `ff24fd1` only in the run script) |
| Rust | 1.99.1, release profile | 1.99.1, release profile |
| Power, load | On AC power; normal desktop session, no other benchmark running | VM idle apart from the benchmark; host as on the left |

Command (both machines, from the repository root): `benches/run-recorded.sh <outdir>`, which runs `tt-latency`,
`tt-checkpoint` and the Criterion benches with the parameters shown below, twice (Criterion: two separate
invocations). No core pinning.

### Writer-to-reader visible latency (cross-process)

`tt-latency run --rate 100000 --count 1100000 --warmup 100000 --payload <64|1024> --reader <mapped|safe>`, 128 MiB
segments. One writer process sends a record every 10 µs at a scheduled time and stores that **intended** time in
the record; a separate reader process (busy-polling) records `now - intended` for the last 1,000,000 records. A writer
that falls behind keeps sending without skipping, so its backlog counts as latency (coordinated omission handled at
the source). "Late sends" counts records sent more than 1 µs after their intended time, out of 1,100,000. Both
processes use the host's wall clock.

Values in µs. On macOS the wall clock has 1 µs resolution, so "<1" means below the clock's resolution.

**Owner's Mac (APFS)**

| Run | Payload | Reader | p50 | p90 | p99 | p99.9 | p99.99 | max | Late sends |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 64 B | zero-copy | <1 | <1 | 33 | 234 | 276 | 331 | 13,897 |
| 1 | 64 B | safe | 1.0 | 2.0 | 27 | 178 | 905 | 1,215 | 2,012 |
| 1 | 1,024 B | zero-copy | <1 | 64 | 13,689 | 18,629 | 21,430 | 22,282 | 156,603 |
| 1 | 1,024 B | safe | 2.0 | 111 | 28,213 | 42,828 | 45,646 | 46,432 | 79,883 |
| 2 | 64 B | zero-copy | <1 | <1 | 127 | 1,011 | 2,376 | 3,119 | 19,547 |
| 2 | 64 B | safe | 1.0 | 2.0 | 42 | 332 | 1,427 | 1,909 | 1,421 |
| 2 | 1,024 B | zero-copy | <1 | 100 | 15,286 | 20,709 | 22,970 | 23,462 | 170,161 |
| 2 | 1,024 B | safe | 2.0 | 86 | 22,233 | 25,362 | 28,262 | 29,213 | 67,805 |

**OrbStack VM (Linux on macOS, btrfs)**

| Run | Payload | Reader | p50 | p90 | p99 | p99.9 | p99.99 | max | Late sends |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 64 B | zero-copy | 0.15 | 0.20 | 0.45 | 326 | 831 | 1,519 | 4,200 |
| 1 | 64 B | safe | 1.5 | 1.7 | 2.9 | 2,572 | 11,510 | 12,501 | 3,684 |
| 1 | 1,024 B | zero-copy | 0.40 | 0.45 | 380 | 6,881 | 8,356 | 8,765 | 21,072 |
| 1 | 1,024 B | safe | 1.8 | 2.3 | 65,765 | 151,257 | 159,908 | 160,825 | 33,108 |
| 2 | 64 B | zero-copy | 0.15 | 0.20 | 0.29 | 328 | 803 | 1,357 | 3,517 |
| 2 | 64 B | safe | 1.5 | 1.7 | 2.5 | 17 | 507 | 1,013 | 2,134 |
| 2 | 1,024 B | zero-copy | 0.40 | 0.46 | 62,980 | 149,422 | 158,073 | 158,990 | 42,837 |
| 2 | 1,024 B | safe | 1.8 | 2.4 | 301 | 6,476 | 9,159 | 10,084 | 8,257 |

Observations, from the tables only:
- Medians are at or below a few µs in every configuration. Tails vary a lot **between runs of the same
  configuration** (for example, VM 1,024 B safe reader p99: 65,765 µs in run 1, 301 µs in run 2), far beyond the 10 %
  the playbook asks us to flag. Treat the tail columns as "can reach this", not as typical.
- With 1,024 B payloads at 100,000 records/s (about 100 MB/s into fresh pages), the writer itself fell behind its
  schedule in every run (up to 170,161 late sends), and that backlog dominates the tail. The writer appends into
  never-before-touched pages of preallocated segments, so page-fault cost is part of every append.

### `checkpoint()` and roll latency

`tt-checkpoint --payload <64|1024> --batch 1000 --checkpoints 200 --segment-size 16777216`: append 1,000 records,
time one `checkpoint()`, 200 times; then one checkpoint with nothing new. Every append is timed; "roll" appends are
the ones that created a new segment. Values in µs; records/s includes the checkpoints.

| Machine | Run | Payload | Checkpoint p50 | p90 | p99 | max | Empty checkpoint | Roll appends (n, p50, max) | Other appends p50 / p99 / p99.9 / max | Records/s |
|---|---|---|---|---|---|---|---|---|---|---|
| Mac | 1 | 64 B | 7,696 | 8,352 | 8,634 | 10,330 | 4,096 | 1, 4,280, 4,280 | 0.04 / 0.17 / 191 / 2,986 | 120,092 |
| Mac | 1 | 1,024 B | 8,389 | 9,388 | 10,854 | 14,164 | 3,910 | 12, 4,321, 6,033 | 0.17 / 40 / 253 / 3,961 | 89,807 |
| Mac | 2 | 64 B | 7,778 | 8,405 | 8,864 | 9,495 | 4,004 | 1, 3,844, 3,844 | 0.04 / 0.17 / 185 / 2,402 | 119,409 |
| Mac | 2 | 1,024 B | 8,081 | 8,569 | 12,419 | 13,099 | 4,112 | 12, 4,207, 4,624 | 0.17 / 41 / 252 / 3,234 | 92,618 |
| VM | 1 | 64 B | 1,316 | 2,388 | 5,014 | 6,218 | 431 | 1, 657, 657 | 0.04 / 0.92 / 2.1 / 617 | 614,244 |
| VM | 1 | 1,024 B | 4,008 | 5,689 | 9,404 | 10,142 | 1,750 | 12, 1,950, 2,761 | 0.17 / 3.9 / 7.3 / 1,267 | 193,707 |
| VM | 2 | 64 B | 3,965 | 6,844 | 8,757 | 11,141 | 1,017 | 1, 2,092, 2,092 | 0.04 / 1.5 / 3.8 / 1,786 | 229,053 |
| VM | 2 | 1,024 B | 4,448 | 5,911 | 10,666 | 11,256 | 1,730 | 12, 1,827, 2,382 | 0.17 / 5.1 / 10 / 1,623 | 179,370 |

On macOS every checkpoint issues `F_FULLFSYNC` twice (after the data, then after the marker); an empty checkpoint
still took about 4 ms there, which is consistent with that cost, though it was not profiled. Inside the VM, `fdatasync` reaches a virtual disk; what the host does with
it was not measured, so the VM numbers say nothing about durability on real Linux hardware.

### Append and replay throughput (Criterion)

`cargo bench -p tickertape-bench --bench append` and `--bench replay`, two separate invocations per machine. Append
writes batches of 16,384 records into a fresh journal (first-touch pages, the steady state of an append-only
journal), excluding open and teardown. Replay reads a journal of 1,000,000 records (64 B) or 100,000 records (1,024 B)
with a warm page cache, including reader open and full validation. Bytes count payload only. Values are Criterion's
point estimates (middle of its interval), in millions of records per second.

| Benchmark | Mac run 1 | Mac run 2 | VM run 1 | VM run 2 |
|---|---|---|---|---|
| append, 64 B | 3.52 | 5.64 | 9.52 | 10.95 |
| append, 1,024 B | 0.72 | 0.79 | 4.60 | 4.90 |
| replay, 64 B, safe reader | 24.9 | 25.0 | 28.9 | 29.0 |
| replay, 64 B, zero-copy reader | 22.1 | 22.4 | 27.6 | 18.4 |
| replay, 1,024 B, safe reader | 6.19 | 6.74 | 6.34 | 5.94 |
| replay, 1,024 B, zero-copy reader | 5.73 | 5.96 | 5.43 | 2.57 |

In payload bytes, run 1: Mac append 286 MiB/s (64 B) and 659 MiB/s (1,024 B); VM append 647 MiB/s and 4.69 GiB/s;
Mac replay 1.46 GiB/s (64 B, safe) and 6.16 GiB/s (1,024 B, safe); VM replay 1.74 GiB/s and 5.84 GiB/s.

Observations:
- Mac append 64 B (+60 %) and VM zero-copy replay (-33 % and -53 %) differ by more than 10 % between runs; the others
  are within about 10 %.
- The zero-copy reader is not faster than the safe reader in these runs. Since the M4 review, it validates every
  record from an atomic copy before handing out a slice into the mapping, so it currently does more work per record
  than the safe reader's chunked `pread`. Its advantage is not copying large payloads into user buffers, which this
  benchmark doesn't measure.
- Appending on the Mac was slower than in the VM on the same hardware. The cause was not investigated; first-touch
  page faults on APFS versus the VM's btrfs are one candidate.

### Cold page cache replay

`tt-latency run --cold --rate 1000000 --count 1000000 --payload <64|1024> --reader safe`: the writer pre-writes and
checkpoints the journal, the tool drops the page cache, then a fresh reader process replays it.

- **Mac:** not measured. Dropping the page cache (`sudo purge`) needs root, which the benchmark doesn't use.
- **OrbStack VM:** drops the *guest's* page cache (`echo 3 > /proc/sys/vm/drop_caches`). The host may still cache
  the VM's disk, so this is not a cold-storage measurement.

| Run | Payload | Records/s | Payload MB/s |
|---|---|---|---|
| 1 | 64 B | 15,839,219 | 1,014 |
| 1 | 1,024 B | 2,973,966 | 3,045 |
| 2 | 64 B | 16,707,495 | 1,069 |
| 2 | 1,024 B | 3,185,582 | 3,262 |

### Not measured

In this v0.1.0 recording: bare-metal Linux; Intel/AMD x86-64; ext4 or xfs on real disks; core pinning or isolated
CPUs; checkpoint durability on real Linux hardware; power-loss behaviour. Since then bare-metal x86-64 Linux, ext4 and
xfs on local NVMe, and runs bound to a NUMA node or pinned to cores have been recorded (the sections above). Isolated
CPUs and whether checkpointed data survives a power loss still have not. CI runners only run smoke versions
of these tools, and their numbers are never recorded.
