# Benchmarks

Measured results only, recorded following [`playbooks/run-benchmark.md`](playbooks/run-benchmark.md). Every number
below comes from the raw output of the commands listed with it. These numbers are not guarantees: they describe
specific machines on specific days. What each benchmark measures, and what it doesn't, is in
[`benches/README.md`](../benches/README.md) and, for the comparison with other tools, in
[`compare/README.md`](../compare/README.md). The methodology is in ADRs [0009](decisions/0009-benchmark-methodology.md)
and [0018](decisions/0018-competitive-benchmarks.md).

**Linux results come first** because that is where tickertape is meant to run, and **bare metal comes first** among
them. The cloud-VM and macOS results are secondary.

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
3.1–3.2 million records/s (v0.3.0: 1.9), 2.2× Chronicle Queue.

**Replay.**
- 64 B: tickertape is fastest, 22.0–23.1 against Chronicle Queue's 18.8–20.4.
- 1,024 B: 4.8–5.0 million records/s with the `safe` reader (v0.3.0: 3.0), close to Chronicle Queue's 5.0–5.3.
  Chronicle Queue checks no checksum; tickertape validates every record.

**Latency.** At 64 B, tickertape's zero-copy reader has the lowest p99.9 of all tools measured, Aeron's in-memory
IPC included:
- 2.1 µs at 10,000 records/s;
- 0.5–1.8 µs at 100,000 records/s, against 36–37 µs for Chronicle Queue and 92–133 µs for Aeron.

At 1,024 B, Aeron IPC (which persists nothing) has the lowest tails: p99.9 0.8–5 µs. tickertape comes next and well
ahead of Chronicle Queue:
- 16–26 µs against 47–48 µs at 10,000 records/s;
- 7.4–9.2 µs (`safe` reader) against 48–55 µs at 100,000 records/s.

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
  on ext4: 110–243 µs with prefaulting against 97–122 µs without. On xfs prefaulting helps there too.

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

Bare-metal Linux; Intel/AMD x86-64; ext4 or xfs on real disks; core pinning or isolated CPUs; checkpoint durability
on real Linux hardware; power-loss behaviour. CI runners only run smoke versions of these tools, and their numbers
are never recorded.
