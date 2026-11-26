# Benchmarks

Measured results only, recorded following [`playbooks/run-benchmark.md`](playbooks/run-benchmark.md). Every number
below comes from the raw output of the commands listed with it. These numbers are not guarantees: they describe
specific machines on specific days. What each benchmark measures, and what it doesn't, is in
[`benches/README.md`](../benches/README.md) and, for the comparison with other tools, in
[`compare/README.md`](../compare/README.md). The methodology is in ADRs [0009](decisions/0009-benchmark-methodology.md)
and [0018](decisions/0018-competitive-benchmarks.md).

**Linux results come first** because that is where tickertape is meant to run. The
[comparison rerun at v1.0.0](#2026-11-25-to-2026-11-26-comparison-rerun-at-v100-google-cloud-vms-linux-x86-64-and-aarch64)
comes first of all: it is the current comparison with other tools, run with the corrected harness on two Google Cloud
VMs, and it supersedes the earlier comparison sections, which stay as recorded. Among the older Linux results bare
metal comes first; the cloud-VM and macOS results are secondary.

The sustained-workload, checkpoint, comparison, reader and scale results and the long verified runs of 2026-11-19 to
2026-11-22 ran on Google Cloud VMs
([below](#2026-11-19-to-2026-11-22-sustained-workloads-on-google-cloud-vms-linux-x86-64-and-aarch64)), after the
bare-metal sections. The scale cases that run left out (10,000 segments and more, archived history, damage, catch-up, the
zero-copy reader, seek indexes) ran on 2026-11-25 on machines of the same types
([below](#2026-11-25-replay-and-recovery-at-scale-the-gaps-of-100-google-cloud-vms)).
What the writer's latency histograms cost (ADR 0047) is
[under 2026-11-26](#2026-11-26-what-the-writers-latency-histograms-cost-adr-0047-google-cloud-vms).
The profiles of v1.0.0's readers, notification and serialization paths (where the CPU time goes, and
what remains per record) are [under 2026-11-25](#2026-11-25-where-the-cpu-time-goes-profiles-of-the-readers-notification-and-serialization-google-cloud-vms),
and the before and after of the safe reader's smaller reads at the tail (ADR 0052) [under 2026-11-26](#2026-11-26-the-safe-readers-read-size-at-the-tail-google-cloud-vms-x86-64-and-aarch64).
Seeks with and without the seek index, at both payloads and with both readers, on `main` after #74, are
[under 2026-11-26](#2026-11-26-seeks-with-and-without-the-seek-index-google-cloud-vms).
The investigation of the two aarch64 observations that 1.0.0 left open (the zero-copy reader's 10 ms maximum, the
lagging view of issue #43) is [under 2026-11-26](#2026-11-26-the-two-aarch64-observations-left-open-by-100-google-cloud-vms).
How much of the throughput ceiling and of the latency tail is the storage device (fio on the same SSDs, the journal on
tmpfs, one storage setting at a time, a payload sweep, the two local SSDs striped) is
[under 2026-11-26](#2026-11-26-how-much-of-the-limit-is-the-disk-fio-tmpfs-and-one-change-at-a-time-google-cloud-vms).
Why replay speed changed with the layout of `Reader`, and the before and after of its fixed layout (ADR 0057), are
[under 2026-11-26](#2026-11-26-the-layout-of-reader-adr-0057-issue-86-google-cloud-vms).
What the Linux prefault window adds to a checkpoint (bytes, sync time, append and reader latency) is
[under 2026-11-26](#2026-11-26-what-prefaulted-unwritten-pages-add-to-a-checkpoint-google-cloud-vms-linux).
Which page faults the writer still takes with prefaulting on, how much latency they explain, and what a writer-thread
touch (as Chronicle Queue's `microTouch()`), a larger window, re-touching, early write-back further behind and the
ranged sync do to them (ADR 0058), are
[under 2026-11-26](#2026-11-26-the-writers-remaining-page-faults-with-prefaulting-on-google-cloud-vms-linux).
The macOS results of 2026-11-26 (the sustained session on a shared Apple M4 Pro laptop, how often a roll waits for the
spare segment, and what prefaulted data adds to a checkpoint) are
[after the Linux sections](#2026-11-26-sustained-load-roll-waits-and-checkpoint-cost-on-macos-apple-m4-pro-apfs).

**How Chronicle Queue and tickertape were configured in the comparisons before the rerun** (the rerun's
configuration is in its own section). Open-source Chronicle Queue (2026.8,
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
file at the first append, inside the timed part. The harness was corrected (issue #56: W1-cold and W1-steady, the JVM
warm-up, `microTouch()`, the 1 GiB `blockSize`, Aeron's low-latency settings, pinning) and the comparison rerun at
v1.0.0 (issue #57, the first section below).

## 2026-11-25 to 2026-11-26, comparison rerun at v1.0.0 (Google Cloud VMs, Linux x86-64 and aarch64)

The whole comparison with Chronicle Queue, Aeron IPC, yaque, queue-file and a plain file, run again with the corrected
harness (issue #57; the plan is `docs/playbooks/rerun-comparison.md`, the driver `benches/rerun.py`). A reader of the
earlier comparison who knows Chronicle Queue pointed out that it handicapped Chronicle: a short JIT warm-up, no page
pre-touching, the default 64 MiB `blockSize`, no thread pinning, and tools measured in different sessions on different
machines. The harness now warms up the JVM tools through their measured loops, calls Chronicle's `microTouch()`, uses a
1 GiB `blockSize`, configures Aeron as its low-latency driver does, and pins every measured thread
([`compare/README.md`](../compare/README.md) → "Thread pinning" and "JVM warm-up, tool settings and JVM logs"). Every
tool ran in one session per machine, on one x86-64 and one aarch64 machine, for 24 hours. **Google Cloud virtual
machines, not bare metal.** The library is the one released as **v1.0.0**, not `main`. The rule of the plan applies:
every cell is published whatever its result, the failed one as failed.

**This section supersedes the earlier comparison sections** (v0.4.0 and v0.3.0 on bare metal, the sustained comparison
of 2026-11-19 to 2026-11-22, v0.3.0 on AWS and macOS), which used the old harness and settings. They stay below as they
were recorded, each with a note. The macOS comparison of 2026-11-26 (low identical rates) is not repeated here.

**In short.** The result is mixed. tickertape led the 64 B append bursts on both machines and file systems (W1-cold
19.11–30.84 against Chronicle Queue's 8.47–8.53 million records/s on x86-64). Chronicle Queue led warm replay in three
of the four pinned ext4 cells (overlapping in the fourth), had the lower W2 reader p99.9 at 10,000 records/s (in three
cells, and in three of four runs in the fourth) and at 1,024 B × 100,000 records/s, and appended 1,024 B records as fast
as tickertape on x86-64 ext4 and faster on xfs in W1 steady; Aeron IPC had the lowest observed p99.9 at 100,000
records/s; at 1,024 B × 100,000 records/s the plain file was lower than tickertape too. In the ten-minute runs at half
of each tool's own rate on ext4, tickertape's reader p99.9 (323–697 µs) was below Chronicle Queue's (89.98–102.69 ms)
and the plain file's at 1,024 B (87.43–96.93 ms), a tail that both tools without early write-back showed on ext4; on xfs
Chronicle's was lower than tickertape's in three of four cells. A tickertape checkpoint (median 1.38–2.99 ms) and
Chronicle's synchronous store flush (302.78–516.16 ms) are not the same work (below). At 90 % of its own rate with a
checkpoint every second tickertape stalled at 64 B on both machines and at 1,024 B on aarch64; Chronicle's 64 B run
there failed. The fairness fixes mattered: under the old settings Chronicle Queue's reader p99.9 at 1,024 B × 10,000
records/s was 34.8–36.8 µs, with `microTouch()` alone 2.35–2.51 µs.

### Environment and commands

| | x86-64 | aarch64 |
|---|---|---|
| Machine (us-central1-b) | `c4-standard-16-lssd`: Intel Xeon 6985P-C, 8 cores with 2 threads each, 59 GiB | `c4a-standard-8-lssd`: Google Axion (Neoverse-V2), 8 cores, 31 GiB |
| Storage | two local NVMe SSDs of 375 GB: ext4 on `/data` (`rw,noatime`), xfs on `/data2` (`rw,noatime,inode64,logbufs=8,logbsize=32k,noquota`); I/O scheduler `none`; results on the boot disk | the same |
| Session (UTC) | 2026-11-25 14:52 to 2026-11-26 14:41 | 2026-11-25 14:52 to 2026-11-26 14:43 |

Ubuntu 24.04, kernel `7.0.0-1017-gcp`, rustc 1.100.0, OpenJDK 21.0.13.1, Python 3.12.3, Maven 3.8.7. Chronicle Queue
2026.8 (open source), Aeron 1.54.0, yaque 0.6.6, queue-file 2.0.0 (`compare/` pins). Each local SSD writes at most 351
MB/s by itself ([how much of the limit is the
disk](#2026-11-26-how-much-of-the-limit-is-the-disk-fio-tmpfs-and-one-change-at-a-time-google-cloud-vms)).

**Commit.** `fb2d91b0` on the branch `rerun/v1.0.0-session`: `1e9846d` (the harness of #65 and #67) plus the driver of
#71. `crates/tickertape` is identical to the tag `v1.0.0` (the driver refuses to start otherwise). `main` has changed
the library since (durable-only readers wake on checkpoints, the safe reader's read size at the tail, the layout of
`Reader`, and others); none of that is in these figures, and the later sections that measure those changes are not
compared with this one.

**Kernel command line, both boots** (`/proc/cmdline`):

```text
# untuned boot, both machines (the PARTUUID differs)
BOOT_IMAGE=/vmlinuz-7.0.0-1017-gcp root=PARTUUID=… ro console=ttyS0,115200 panic=-1
# tuned boot, both machines
BOOT_IMAGE=/vmlinuz-7.0.0-1017-gcp root=PARTUUID=… ro isolcpus=managed_irq,domain,1,2 nohz_full=1,2 rcu_nocbs=1,2 irqaffinity=0,3,4,5,6,7 nosmt console=ttyS0,115200 panic=-1
```

On the tuned boot the kernel reported `isolated` 1-2, `nohz_full` 1-2 and CPUs 0-7 online on both machines;
`smt/control` was `off` on x86-64 (the sibling threads 8-15 offline) and `notsupported` on aarch64, which has one thread
per core anyway.

**CPUs.** Untuned boot, automatic choice: writer CPU 1, reader CPU 2, helpers `0,3-8,11-15` on x86-64 (CPUs 9 and 10,
the siblings of 1 and 2, unused) and `0,3-7` on aarch64. Tuned boot: writer 1, reader 2, helpers `0,3,4,5,6,7`, named.

**Commands** (`benches/rerun.py`, one unit per tool and configuration, each in its own directory; OUT is the unit's
directory, TMPDIR the file system under test):

```sh
# short mode, two repeats per unit
rerun.py --short-tools <tool> -- OUT                                    # pinned
rerun.py --short-tools <tool> -- OUT --no-pin                           # unpinned
rerun.py --short-tools <tool> -- OUT --no-long-warmup --no-micro-touch --chronicle-block-size default \
  --aeron-sparse-terms --no-aeron-pre-touch --no-jvm-logs               # jvm-old
rerun.py --short-tools chronicle -- OUT --no-long-warmup --chronicle-block-size default \
  --aeron-sparse-terms --no-aeron-pre-touch --no-jvm-logs               # microtouch-only
rerun.py --short-tools <tool> -- OUT --writer-cpu 1 --reader-cpu 2 --helper-cpus 0,3,4,5,6,7   # tuned
# sustained mode, ten-minute runs after a 10 s warm-up, one repeat per unit
compare/run.py OUT --sustained --keep-going --tools <tool> --workloads probe,SV,SD --rate-fractions 0.5 --repeats 1
compare/run.py OUT --sustained --keep-going --tools <tool> --workloads probe,SD --rate-fractions 0.9 --repeats 1   # 90 %
compare/run.py OUT --sustained --keep-going --tools <tool> --workloads probe,SV --rate-fractions 0.5 --repeats 1   # yaque, queue-file, plain file
# jvm-old and tuned add the same options as in the short mode
```

The raw output (every result line, `rerun.json`, both boots' environment files, `/proc/interrupts` captures, the JVM
logs, `children.log`, the driver's log) is kept outside the repository. The tables below were computed from the result
files of the units that `rerun.json` marks done, and from the completed runs of the two failed units; values are rounded
half up to three significant digits (two decimals for million records/s).

### What each row means

| Row | What ran |
|---|---|
| `pinned` | The default configuration of the corrected harness: each measured thread alone on its CPU, everything else on the helper CPUs; the JVM tools with the long warm-up (3 × 1,200,000 and 8 × 20,000 records through the measured loops, in every process), Chronicle with a 1 GiB `blockSize` and `microTouch()` in the writer's busy-wait, Aeron with non-sparse, pre-touched term buffers and dedicated busy-spinning driver threads; every reader busy-polls; the JVMs with a 256 MiB heap and ParallelGC and asynchronous JVM logging on |
| `unpinned` | The same with `--no-pin`: the kernel schedules every thread, CPU 0 included (which also runs the result pulls, at nice 19) |
| `tuned` | `pinned` on the tuned boot above, with the CPUs named. **The tuned CPUs were isolated but not interrupt-free** ([below](#interrupts-on-the-tuned-cpus)) |
| `jvm-old` | "Old settings, new harness": pinned, the new measured loops, but the old short warm-up (W1 and W3: 100,000 records; W2 and sustained: none), Chronicle's 64 MiB `blockSize`, no `microTouch()`, Aeron's sparse terms without pre-touch, no JVM logs. It is not the old harness: [`compare/README.md`](../compare/README.md) → "The old settings are not the old code" |
| `microtouch-only` | "Old settings plus `microTouch()`", Chronicle Queue only: as `jvm-old` but with `microTouch()`. It is not `microTouch()` on top of the new settings |

tickertape ran with v1.0.0's defaults (`WriterConfig::default()` with the helper CPUs: prefaulting 256 KiB ahead and
early write-back on Linux, where a helper thread starts the kernel's write-back of the pages behind the writer with
`sync_file_range(SYNC_FILE_RANGE_WRITE)` each time the writer has moved 1 MiB); in the sustained SD runs it calls the
blocking `Writer::checkpoint` on the writer's thread, as the other tools' sync calls block. Its W2 rows run
`tt-latency`; W3 and W2 have a row for each reader, the zero-copy `Reader::open_mapped` and the safe `Reader::open`; the
sustained rows use the zero-copy reader.

**What a sync is differs per tool** ([`compare/README.md`](../compare/README.md) → "Supported cases and unequal
guarantees"). A tickertape checkpoint on Linux `msync`s the mapped ranges it still holds, `fdatasync`s the segment files
and `fsync`s new directory entries, then writes the checkpoint marker and `fdatasync`s it; it publishes a durable
position readers can use. Chronicle Queue's is `ExcerptAppender.sync` with `SyncMode.SYNC`, a synchronous
`msync(MS_SYNC)` of its store (which index files it covers is Chronicle's choice); called "synchronous store flush"
below. queue-file's is `sync_all` and the plain file's `sync_data`; Aeron IPC and yaque have none (no SD rows). None of
them is tested for power loss here. The tools also promise different things: Aeron IPC writes no durable journal and
this adapter has no Archive replay, but its term buffers are memory-mapped files in the data directory on the file
system under test (not `/dev/shm`), so the kernel writes them back to that disk; yaque is a consuming single-reader
queue (its W3 is a scan of the unconsumed queue); queue-file's upstream API forbids concurrent process access (no W2,
writer-only sustained rows); and the plain-file adapter has no checksum or crash-recovery protocol.

### The plan, the order and the gaps between rounds

The short mode (W1-cold, W1-steady, W2 at 10,000 and 100,000 records/s, W3; 64 B and 1,024 B) ran every tool in
`pinned`, `unpinned` and `tuned`, Chronicle Queue and Aeron in `jvm-old` and Chronicle in `microtouch-only`, on ext4,
and every tool `pinned` on xfs. The sustained mode (ten-minute runs: the unpaced probe, SV paced at 50 % of the same
unit's probe without syncs, SD the same with a sync every second) ran tickertape, Chronicle Queue and Aeron `pinned` on
ext4 in two rounds, then: SD at 90 % of the probe (tickertape and Chronicle, ext4 and xfs), `jvm-old` (Chronicle and
Aeron), tickertape and Chronicle `pinned` on xfs, yaque, queue-file and the plain file (probe and SV), and the `tuned`
boot (tickertape, Chronicle, Aeron).

**Samples.** Short mode: two repeats per unit, minutes apart. `pinned`, `jvm-old` and `microtouch-only` ran in two
rounds on ext4 (n = 4); `unpinned`, `tuned` and the xfs rows in one (n = 2). Sustained mode: one run per cell and round
(n = 1); the two rounds of `sustained` are separate rows, because each round is paced from its own probe. Ranges are
min–max of the samples.

**Different loads.** SV and SD run at 50 % (or 90 %) of each tool's own probe in that configuration and round, so they
are **different absolute rates for different tools** (the target rate is beside every row). Only the probes answer the
same question for every tool (how fast unpaced, with a reader following); SV and SD answer how each tool behaves at half
(or 90 %) of its own rate.

**Order.** Within a round, unit after unit. Round 1 of the short part went tool by tool (tickertape, Chronicle Queue,
Aeron, yaque, queue-file, plain file); round 2 rotated the tools and reversed the configurations within a tool; the
sustained rounds went tickertape, Chronicle, Aeron and then Chronicle, Aeron, tickertape. Every other part had one round
in a fixed tool order: tickertape first, except `sustained-jvm-old` (Chronicle, then Aeron) and `sustained-others`
(yaque first). On xfs tickertape's first large writes went to a freshly made file system, and Chronicle's to one already
written.

| Part (x86-64, UTC; aarch64 within 3 minutes) | From | To |
|---|---|---|
| short, ext4 (both rounds) | 11-25 14:52 | 19:11 |
| sustained, ext4 (both rounds) | 19:11 | 11-26 00:40 |
| sustained at 90 %, ext4 | 00:40 | 02:17 |
| sustained `jvm-old`, ext4 | 02:17 | 03:58 |
| short, xfs | 03:58 | 05:08 |
| sustained, xfs (`sustained-journals`) | 05:08 | 07:12 |
| sustained at 90 %, xfs | 07:12 | 08:49 |
| sustained yaque, queue-file, plain file, ext4 | 08:49 | 10:47 |
| (reboot into the tuned boot) | | |
| tuned short, ext4 | 10:47 | 11:56 |
| tuned sustained, ext4 | 11:56 | 14:41 |

Gap between the two rounds of each short unit (end of round 1 to start of round 2), in minutes:

| Unit | x86-64 | aarch64 |
|---|---|---|
| Aeron `jvm-old` | 33 | 34 |
| plain file `pinned` | 43 | 43 |
| queue-file `pinned` | 51 | 51 |
| yaque `pinned` | 51 | 52 |
| Aeron `pinned` | 68 | 68 |
| Chronicle `microtouch-only` | 136 | 137 |
| Chronicle `jvm-old` | 153 | 154 |
| Chronicle `pinned` | 189 | 190 |
| tickertape `pinned` | 190 | 192 |

**Completion.** 54 of 56 units completed on each machine. The two failed units are the same on both machines and both
file systems ([below](#the-failed-cell)). Nothing else failed, nothing failed once and then passed, no unit was set
aside as incomplete, and no run was discarded for a wall-clock step (`retries.jsonl` empty everywhere).

### Short mode: W1 and W3 (million records/s)

W1 appends 1,000,000 records of 64 B or 100,000 of 1,024 B as fast as possible: W1-cold from an empty directory with the
open inside the timer, W1-steady into files opened and primed with 1,000 records before it
([`compare/README.md`](../compare/README.md) → "Workloads and schema"). For tickertape a W1 run takes 32–52 ms at 64 B
on x86-64, so one stall is a visible share of it. Both timers stop before the final flush, so W1 is visible, not
durable, append throughput. W3 writes the same records, warms the page cache with an untimed traversal, and times a
fresh reader process from open to the last record. Higher is better.

**W1-cold**

| FS | Config | Tool | n | x86-64, 64 B | x86-64, 1,024 B | aarch64, 64 B | aarch64, 1,024 B |
|---|---|---|---|---|---|---|---|
| ext4 | pinned | tickertape | 4 | 19.11–30.84 | 2.61–3.08 | 10.46–10.56 | 4.02–4.20 |
| ext4 | pinned | Chronicle Queue | 4 | 8.47–8.53 | 2.82–2.87 | 5.49–5.53 | 2.76–2.83 |
| ext4 | pinned | yaque | 4 | 3.48–3.50 | 1.51–1.63 | 1.94–1.96 | 1.18–1.20 |
| ext4 | pinned | queue-file | 4 | 0.94–0.95 | 0.34–0.35 | 0.58 | 0.28–0.29 |
| ext4 | pinned | plain file | 4 | 3.66–3.71 | 1.65–1.67 | 2.09–2.13 | 1.24–1.27 |
| ext4 | unpinned | tickertape | 2 | 19.58–21.99 | 2.87–2.89 | 10.19–10.32 | 3.75–3.82 |
| ext4 | unpinned | Chronicle Queue | 2 | 8.51–8.66 | 2.84–2.89 | 5.47–5.50 | 2.78–2.85 |
| ext4 | unpinned | yaque | 2 | 3.47–3.52 | 1.59–1.64 | 1.91–1.92 | 1.18 |
| ext4 | unpinned | queue-file | 2 | 0.95 | 0.35 | 0.58 | 0.28 |
| ext4 | unpinned | plain file | 2 | 3.66–3.69 | 1.68–1.69 | 2.07–2.10 | 1.26–1.27 |
| ext4 | jvm-old | Chronicle Queue | 4 | 6.73–7.36 | 2.19–2.38 | 4.66–4.78 | 1.70–1.94 |
| ext4 | microtouch-only | Chronicle Queue | 4 | 6.83–7.37 | 2.06–2.37 | 4.63–4.76 | 1.72–1.79 |
| ext4 | tuned | tickertape | 2 | 17.53–29.38 | 2.58–2.64 | 10.51–10.62 | 4.14–4.15 |
| ext4 | tuned | Chronicle Queue | 2 | 8.45–8.63 | 2.93–2.97 | 5.55–5.59 | 2.87–2.88 |
| ext4 | tuned | yaque | 2 | 2.89–2.92 | 1.54–1.56 | 1.53 | 1.03–1.04 |
| ext4 | tuned | queue-file | 2 | 0.61 | 0.16 | 0.45 | 0.25 |
| ext4 | tuned | plain file | 2 | 3.03 | 1.59–1.60 | 1.63 | 1.09–1.10 |
| xfs | pinned | tickertape | 2 | 19.30–29.61 | 2.61–2.73 | 10.47–10.51 | 3.90–3.91 |
| xfs | pinned | Chronicle Queue | 2 | 8.76–8.79 | 3.48–3.53 | 5.73–5.75 | 3.70–3.72 |
| xfs | pinned | yaque | 2 | 2.65–2.66 | 1.56–1.57 | 1.53–1.55 | 1.11 |
| xfs | pinned | queue-file | 2 | 0.81 | 0.34 | 0.50 | 0.27 |
| xfs | pinned | plain file | 2 | 2.78–2.80 | 1.57–1.65 | 1.61–1.63 | 1.15–1.16 |

**W1-steady**

| FS | Config | Tool | n | x86-64, 64 B | x86-64, 1,024 B | aarch64, 64 B | aarch64, 1,024 B |
|---|---|---|---|---|---|---|---|
| ext4 | pinned | tickertape | 4 | 19.44–24.93 | 2.92–3.06 | 10.21–10.55 | 3.77–4.02 |
| ext4 | pinned | Chronicle Queue | 4 | 8.54–8.69 | 2.93–3.04 | 5.57–5.60 | 2.97–3.07 |
| ext4 | pinned | yaque | 4 | 3.50–3.52 | 1.62–1.66 | 1.94–1.99 | 1.18–1.19 |
| ext4 | pinned | queue-file | 4 | 1.14–1.15 | 0.85 | 0.64–0.65 | 0.52 |
| ext4 | pinned | plain file | 4 | 3.68–3.75 | 1.51–1.76 | 2.04–2.07 | 1.24–1.26 |
| ext4 | unpinned | tickertape | 2 | 19.68–20.10 | 3.03–3.18 | 10.25–10.37 | 3.87 |
| ext4 | unpinned | Chronicle Queue | 2 | 8.68–8.84 | 3.01–3.04 | 5.57–5.59 | 3.07–3.10 |
| ext4 | unpinned | yaque | 2 | 3.50–3.52 | 1.62–1.65 | 1.94–1.97 | 1.17–1.18 |
| ext4 | unpinned | queue-file | 2 | 1.14–1.15 | 0.85 | 0.65 | 0.53 |
| ext4 | unpinned | plain file | 2 | 3.69–3.74 | 1.70–1.75 | 2.10 | 1.29 |
| ext4 | jvm-old | Chronicle Queue | 4 | 7.10–7.49 | 2.28–2.32 | 4.79–4.85 | 1.86–2.04 |
| ext4 | microtouch-only | Chronicle Queue | 4 | 7.26–7.38 | 2.20–2.34 | 4.66–4.81 | 1.92–2.14 |
| ext4 | tuned | tickertape | 2 | 18.92–19.13 | 2.65–2.67 | 10.19–10.50 | 3.79–4.01 |
| ext4 | tuned | Chronicle Queue | 2 | 8.73–8.80 | 3.12–3.14 | 5.64–5.69 | 3.08 |
| ext4 | tuned | yaque | 2 | 2.90–2.94 | 1.55–1.59 | 1.53 | 1.04 |
| ext4 | tuned | queue-file | 2 | 0.89 | 0.70–0.71 | 0.50 | 0.42 |
| ext4 | tuned | plain file | 2 | 3.01–3.02 | 1.63–1.65 | 1.60 | 1.07–1.08 |
| xfs | pinned | tickertape | 2 | 20.01–29.30 | 2.71–2.90 | 10.22–10.50 | 3.99–4.04 |
| xfs | pinned | Chronicle Queue | 2 | 9.03–9.05 | 3.73–3.76 | 5.79–5.80 | 4.08–4.11 |
| xfs | pinned | yaque | 2 | 2.66–2.67 | 1.58–1.59 | 1.53–1.54 | 1.10–1.11 |
| xfs | pinned | queue-file | 2 | 0.93–0.94 | 0.74–0.75 | 0.55 | 0.47 |
| xfs | pinned | plain file | 2 | 2.79–2.81 | 1.63–1.65 | 1.63 | 1.16 |

**W3, warm replay**

| FS | Config | Tool | Reader | n | x86-64, 64 B | x86-64, 1,024 B | aarch64, 64 B | aarch64, 1,024 B |
|---|---|---|---|---|---|---|---|---|
| ext4 | pinned | tickertape | zero-copy | 4 | 20.85–26.41 | 9.11–9.47 | 27.22–31.59 | 10.05–10.36 |
| ext4 | pinned | tickertape | safe | 4 | 31.01–32.77 | 6.40–6.57 | 28.12–28.48 | 6.26–6.53 |
| ext4 | pinned | Chronicle Queue |  | 4 | 35.57–36.77 | 10.28–10.59 | 28.17–29.30 | 15.31–17.12 |
| ext4 | pinned | yaque |  | 4 | 28.89–29.61 | 7.38–7.44 | 20.22–22.04 | 3.94–4.41 |
| ext4 | pinned | queue-file |  | 4 | 1.65–1.66 | 1.38–1.39 | 1.04 | 0.89–0.90 |
| ext4 | pinned | plain file |  | 4 | 5.57–5.58 | 3.46–3.57 | 3.23–3.31 | 2.30–2.44 |
| ext4 | unpinned | tickertape | zero-copy | 2 | 24.42–25.40 | 9.23–9.55 | 31.42–31.58 | 10.23–10.31 |
| ext4 | unpinned | tickertape | safe | 2 | 25.26–32.16 | 6.31–6.49 | 28.63–28.69 | 6.67–6.77 |
| ext4 | unpinned | Chronicle Queue |  | 2 | 32.86–34.78 | 9.57–10.67 | 27.65–28.29 | 16.08–16.59 |
| ext4 | unpinned | yaque |  | 2 | 29.17–29.52 | 7.52–7.67 | 21.23–21.34 | 4.37–4.40 |
| ext4 | unpinned | queue-file |  | 2 | 1.64–1.66 | 1.40 | 1.04–1.05 | 0.89 |
| ext4 | unpinned | plain file |  | 2 | 5.57–5.59 | 3.64–3.67 | 3.24–3.25 | 2.36–2.43 |
| ext4 | jvm-old | Chronicle Queue |  | 4 | 30.15–31.88 | 6.52–8.53 | 19.44–24.95 | 4.02–6.61 |
| ext4 | microtouch-only | Chronicle Queue |  | 4 | 28.72–32.75 | 5.88–7.32 | 19.68–23.94 | 4.41–7.55 |
| ext4 | tuned | tickertape | zero-copy | 2 | 27.75–28.13 | 10.06–10.74 | 30.96–31.62 | 10.01–10.13 |
| ext4 | tuned | tickertape | safe | 2 | 33.53–33.58 | 6.65–6.67 | 27.86–28.50 | 6.35–6.45 |
| ext4 | tuned | Chronicle Queue |  | 2 | 34.72–34.92 | 11.13–11.25 | 28.36–29.52 | 16.09–16.19 |
| ext4 | tuned | yaque |  | 2 | 27.19–28.78 | 6.90–6.92 | 21.10–21.52 | 3.93–4.01 |
| ext4 | tuned | queue-file |  | 2 | 1.12–1.13 | 1.03–1.05 | 0.78 | 0.70 |
| ext4 | tuned | plain file |  | 2 | 3.80 | 2.65–2.97 | 2.42 | 1.91–1.93 |
| xfs | pinned | tickertape | zero-copy | 2 | 23.18–23.96 | 8.83–8.84 | 30.93–31.62 | 9.74–9.85 |
| xfs | pinned | tickertape | safe | 2 | 32.90–33.27 | 6.32–6.33 | 26.69–26.75 | 5.88–5.91 |
| xfs | pinned | Chronicle Queue |  | 2 | 32.41–32.77 | 10.49–10.50 | 27.44–28.18 | 15.89–16.05 |
| xfs | pinned | yaque |  | 2 | 27.06–29.25 | 7.43–7.56 | 20.58–21.43 | 4.26–4.41 |
| xfs | pinned | queue-file |  | 2 | 1.55 | 1.34 | 0.93 | 0.82 |
| xfs | pinned | plain file |  | 2 | 5.18–5.22 | 3.22–3.59 | 2.90–2.96 | 2.20–2.22 |

- **Append:** tickertape ahead of every other tool at 64 B on both machines and both file systems, and at 1,024 B on
  aarch64 ext4. At 1,024 B on x86-64 ext4 its ranges overlap Chronicle Queue's (W1-cold 2.61–3.08 against 2.82–2.87,
  W1-steady 2.92–3.06 against 2.93–3.04); on x86-64 xfs Chronicle is ahead at 1,024 B (W1-cold 3.48–3.53 against
  2.61–2.73, W1-steady 3.73–3.76 against 2.71–2.90; n = 2), and on aarch64 xfs in W1-steady (4.08–4.11 against
  3.99–4.04; tickertape ahead in W1-cold, 3.90–3.91 against 3.70–3.72); on the tuned x86-64 boot Chronicle is ahead at
  1,024 B in both (2.93–2.97 against 2.58–2.64, 3.12–3.14 against 2.65–2.67). yaque, queue-file and the plain file are
  below both journals in every W1 cell.
- **Replay:** on x86-64 Chronicle Queue is faster than both of tickertape's readers at both payloads on ext4
  (35.57–36.77 against 20.85–26.41 zero-copy and 31.01–32.77 safe at 64 B; 10.28–10.59 against 9.11–9.47 and 6.40–6.57
  at 1,024 B), and yaque's queue scan is faster than tickertape's zero-copy reader at 64 B (28.89–29.61) and than its
  safe reader at 1,024 B (7.38–7.44). On aarch64 at 64 B the ranges of Chronicle and both readers overlap (28.17–29.30
  against 27.22–31.59 and 28.12–28.48); at 1,024 B Chronicle is faster (15.31–17.12 against 10.05–10.36 and 6.26–6.53).
  On xfs (n = 2) tickertape's safe reader is ahead of Chronicle at 64 B on x86-64 (32.90–33.27 against 32.41–32.77) and
  its zero-copy reader at 64 B on aarch64 (30.93–31.62 against 27.44–28.18); Chronicle is ahead in the other cells.
- **Spread.** tickertape's W1 at 64 B on x86-64 spread from 19.11 to 30.84 (W1-cold) and its zero-copy W3 from 20.85 to
  26.41; the repeats inside a unit differ as much as the rounds (for example W1-cold round 1: 30.84 and 19.11). Every
  `pinned` cell whose range is wider than 10 % is listed [below](#sample-spread).

### Short mode: W2, latency from the scheduled send to the read (µs)

One writer process and one reader process; record `i` is due at `t0 + i / rate`. A send is never skipped: when it starts
late it still runs, and its delay counts in the latency. The reader busy-polls and records `now - due` after validating
the record. 1,100,000 records per run: the first 100,000 are left out of the histogram, the next 1,000,000 measured (110
s at 10,000 records/s, 11 s at 100,000). Late sends: sends that started more than 1 µs after their time (warm-up
included). µs unless marked; min–max of the runs. Lower is better. queue-file has no W2 (no concurrent reader).

**64 B at 10,000 records/s**

| FS | Config | Tool | Reader | n | x86-64 p50 | p99 | p99.9 | max | late sends | aarch64 p50 | p99 | p99.9 | max | late sends |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| ext4 | pinned | tickertape | zero-copy | 4 | 0.31 | 0.48–0.49 | 2.17–2.35 | 86.3–265 | 98–354 | 0.30–0.31 | 0.42 | 2.87–3.46 | 118–133 | 5–164 |
| ext4 | pinned | tickertape | safe | 4 | 1.94–1.96 | 2.32–2.36 | 4.30–4.49 | 191–278 | 100–182 | 2.41–2.42 | 2.80–4.26 | 6.00–6.45 | 111–155 | 6–44,480 |
| ext4 | pinned | Chronicle Queue |  | 4 | 0.52–0.55 | 0.68–0.69 | 1.68–1.83 | 83.1 µs to 1.01 ms | 200–251 | 0.49–0.51 | 0.62–0.65 | 0.73–0.78 | 25.3–67.6 | 22–36 |
| ext4 | pinned | Aeron IPC |  | 4 | 0.26–0.27 | 2.11–2.17 | 2.56–5.65 | 114 µs to 71.30 ms | 218–1,003 | 0.39–0.43 | 4.11–4.65 | 5.56–7.34 | 36.8–152 | 12–41,142 |
| ext4 | pinned | yaque |  | 4 | 0.94–0.96 | 3.14–3.25 | 6.21–6.34 | 10.47–15.29 ms | 363–473 | 1.51–1.55 | 3.47–3.66 | 9.08–9.50 | 2.57–14.31 ms | 228–10,118 |
| ext4 | pinned | plain file |  | 4 | 0.85–0.86 | 2.72–3.13 | 5.62–6.08 | 13.90–15.96 ms | 565–782 | 1.29–1.37 | 3.42–4.02 | 9.04–9.41 | 13.74–15.33 ms | 267–45,282 |
| ext4 | unpinned | tickertape | zero-copy | 2 | 0.32 | 0.49–0.50 | 2.16–2.19 | 102 | 118–132 | 0.30–0.31 | 0.40–0.42 | 3.07–3.49 | 100–106 | 11–504 |
| ext4 | unpinned | tickertape | safe | 2 | 1.95 | 2.38–2.39 | 4.33–4.46 | 76.7–255 | 101–106 | 2.41–2.43 | 2.80–3.35 | 6.06–6.25 | 103–120 | 9–1,458 |
| ext4 | unpinned | Chronicle Queue |  | 2 | 0.54–0.56 | 0.71–0.73 | 1.10–1.97 | 120–146 | 143–561 | 0.48–0.53 | 0.68–2.62 | 0.77–4.80 | 40.0–71.9 | 35–28,793 |
| ext4 | unpinned | Aeron IPC |  | 2 | 0.28–0.29 | 2.09–6.01 | 2.56–7.39 | 38.6–166 | 237–279 | 0.39–0.43 | 3.87–4.09 | 5.26–5.31 | 39.3–85.8 | 21–142 |
| ext4 | unpinned | yaque |  | 2 | 0.96–0.99 | 3.09–3.10 | 5.90–6.26 | 2.46–3.28 ms | 152–337 | 1.50–1.56 | 3.62–4.16 | 9.02–9.39 | 2.78–3.00 ms | 253–297 |
| ext4 | unpinned | plain file |  | 2 | 0.81–0.82 | 2.99–3.01 | 5.97–6.24 | 14.98–16.06 ms | 580–847 | 1.32–1.36 | 3.37–3.61 | 8.70–8.94 | 14.29–14.78 ms | 284–405 |
| ext4 | jvm-old | Chronicle Queue |  | 4 | 0.51–0.53 | 0.68–0.69 | 2.45–3.51 | 493–591 | 257–417 | 0.49 | 0.64–0.67 | 2.13–2.24 | 512–910 | 111–132 |
| ext4 | jvm-old | Aeron IPC |  | 4 | 0.27 | 2.08–2.18 | 3.67–4.36 | 11.94–13.90 ms | 335–479 | 0.40–0.43 | 4.01–4.68 | 6.57–7.67 | 16.18–19.14 ms | 209–304 |
| ext4 | microtouch-only | Chronicle Queue |  | 4 | 0.51–0.54 | 0.67–0.69 | 1.90–1.99 | 528 µs to 1.32 ms | 299–346 | 0.47–0.51 | 0.64–2.95 | 1.59–3.89 | 517–592 | 127–16,243 |
| ext4 | tuned | tickertape | zero-copy | 2 | 0.30 | 0.46–0.72 | 2.00–2.03 | 215–250 | 188–199 | 0.30 | 0.40–0.41 | 2.67–2.78 | 117–136 | 6–13 |
| ext4 | tuned | tickertape | safe | 2 | 2.09–2.10 | 2.48–2.49 | 4.14–4.20 | 230–252 | 119–225 | 2.63–2.64 | 3.06 | 5.57–5.90 | 119 µs to 3.51 ms | 18–42 |
| ext4 | tuned | Chronicle Queue |  | 2 | 0.52 | 0.64–0.65 | 1.34–1.37 | 122–159 | 206–246 | 0.47–0.50 | 0.59–0.63 | 0.70–0.73 | 75.1–81.7 | 18–77 |
| ext4 | tuned | Aeron IPC |  | 2 | 0.26 | 1.84–1.85 | 2.69–2.77 | 70.9–126 | 369–385 | 0.40–0.44 | 3.67–4.24 | 5.81–6.52 | 29.0–54.7 | 18–22 |
| ext4 | tuned | yaque |  | 2 | 1.04–1.09 | 2.70–2.77 | 5.32–5.40 | 15.47–16.84 ms | 384–415 | 1.70–1.75 | 4.20–4.48 | 10.2–10.6 | 11.09–11.97 ms | 188–207 |
| ext4 | tuned | plain file |  | 2 | 0.96–0.98 | 2.79–2.96 | 5.31–7.22 | 18.04–30.47 ms | 542–948 | 1.48–1.52 | 3.76–3.83 | 8.82–9.03 | 11.89–11.99 ms | 181–213 |
| xfs | pinned | tickertape | zero-copy | 2 | 0.31 | 0.46–0.47 | 2.19–2.25 | 104–127 | 224–478 | 0.30–0.31 | 0.44–0.45 | 5.07–5.34 | 82.3–106 | 11–14 |
| xfs | pinned | tickertape | safe | 2 | 1.98 | 2.36–2.37 | 4.16–4.18 | 100–136 | 113–157 | 2.55–2.56 | 3.00–3.05 | 7.58–7.94 | 94.8–124 | 14–21 |
| xfs | pinned | Chronicle Queue |  | 2 | 0.52–0.53 | 0.68–0.69 | 1.74–1.98 | 109–342 | 216–309 | 0.47–0.48 | 0.62–0.66 | 0.75–0.81 | 64.4 µs to 1.41 ms | 42–44 |
| xfs | pinned | Aeron IPC |  | 2 | 0.27–0.28 | 2.09–2.13 | 2.48–2.57 | 97.5–126 | 230–363 | 0.38–0.43 | 4.46–4.72 | 5.83–6.58 | 56.0 µs to 14.14 ms | 69–170 |
| xfs | pinned | yaque |  | 2 | 3.89–3.93 | 5.15–5.16 | 6.45–6.62 | 225–269 | 57–64 | 4.59–4.74 | 6.43–6.84 | 8.21–8.74 | 252–254 | 30–34 |
| xfs | pinned | plain file |  | 2 | 3.59–3.60 | 4.57–4.60 | 6.41–6.74 | 201–220 | 161–179 | 4.42–4.46 | 6.65–6.73 | 8.81–10.3 | 248–249 | 95–100 |

**64 B at 100,000 records/s**

| FS | Config | Tool | Reader | n | x86-64 p50 | p99 | p99.9 | max | late sends | aarch64 p50 | p99 | p99.9 | max | late sends |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| ext4 | pinned | tickertape | zero-copy | 4 | 0.30 | 0.47–0.49 | 2.27–5.96 | 70.1–80.0 | 817–1,110 | 0.28 | 0.38–0.39 | 1.88–2.08 | 93.4–98.2 | 113–148 |
| ext4 | pinned | tickertape | safe | 4 | 1.93–1.94 | 2.35–6.91 | 3.83–8.35 | 76.5–105 | 890–11,854 | 2.39–2.41 | 2.80–4.94 | 4.95–6.07 | 100–143 | 122–1,443 |
| ext4 | pinned | Chronicle Queue |  | 4 | 0.44–0.54 | 0.60–5.52 | 11.8–15.3 | 123–143 | 2,171–12,985 | 0.44–0.50 | 0.61–2.94 | 7.85–13.3 | 103–138 | 2,145–23,733 |
| ext4 | pinned | Aeron IPC |  | 4 | 0.26 | 0.34–1.85 | 0.39–3.47 | 104–335 | 134–11,402 | 0.39–0.41 | 0.48–1.63 | 0.62–3.03 | 91.5–102 | 42–12,874 |
| ext4 | pinned | yaque |  | 4 | 0.91–0.92 | 2.68–6.14 | 5.76–7.92 | 170–239 | 984–11,786 | 1.39–1.49 | 3.32–3.50 | 8.74–9.02 | 248–275 | 414–6,969 |
| ext4 | pinned | plain file |  | 4 | 0.79–0.88 | 2.73–5.72 | 5.10–7.08 | 129–184 | 1,609–11,294 | 1.23–1.29 | 3.00–3.15 | 7.94–8.41 | 243–273 | 351–944 |
| ext4 | unpinned | tickertape | zero-copy | 2 | 0.31–0.32 | 0.48–0.49 | 2.27–2.31 | 70.0–77.4 | 688–829 | 0.27–0.28 | 0.38–2.49 | 2.07–3.46 | 34.2–38.8 | 112–11,094 |
| ext4 | unpinned | tickertape | safe | 2 | 1.94 | 3.33–7.01 | 4.74–8.26 | 65.3–76.5 | 747–8,954 | 2.40–2.41 | 2.79–4.22 | 4.60–5.18 | 43.4–103 | 135–3,538 |
| ext4 | unpinned | Chronicle Queue |  | 2 | 0.45–0.47 | 0.60–0.65 | 13.0–15.0 | 115–122 | 2,201–2,248 | 0.44–0.46 | 0.55–0.56 | 6.67–7.78 | 129–139 | 1,671–1,747 |
| ext4 | unpinned | Aeron IPC |  | 2 | 0.26 | 0.32–0.34 | 0.40–0.41 | 30.7–103 | 120–194 | 0.39 | 1.77–1.85 | 2.82–10.3 | 100 µs to 3.39 ms | 9,673–11,064 |
| ext4 | unpinned | yaque |  | 2 | 0.93 | 2.71–2.78 | 6.11–6.16 | 187–247 | 904–1,134 | 1.46–1.47 | 4.10–4.38 | 9.01–9.09 | 249–277 | 471–590 |
| ext4 | unpinned | plain file |  | 2 | 0.80–0.81 | 2.59–5.97 | 5.28–7.24 | 165–169 | 521–11,231 | 1.25–1.30 | 3.16–3.51 | 7.77–8.06 | 266–268 | 350–503 |
| ext4 | jvm-old | Chronicle Queue |  | 4 | 0.43–0.50 | 0.60–1.00 | 28.6–29.2 | 513–700 | 4,578–6,917 | 0.42–0.50 | 0.57–0.72 | 25.1–27.1 | 605–626 | 4,874–5,489 |
| ext4 | jvm-old | Aeron IPC |  | 4 | 0.25–0.26 | 0.34–3.25 | 0.83–4.99 | 119–169 | 711–11,548 | 0.39–0.41 | 0.55–2.47 | 3.07–5.92 | 128 µs to 1.16 ms | 908–18,875 |
| ext4 | microtouch-only | Chronicle Queue |  | 4 | 0.44–0.45 | 0.61–3.33 | 17.8–18.2 | 594–627 | 4,509–15,420 | 0.40–0.50 | 0.54–3.49 | 12.7–18.1 | 577–622 | 4,676–26,999 |
| ext4 | tuned | tickertape | zero-copy | 2 | 0.30 | 0.45–0.47 | 2.14–2.27 | 81.7–113 | 1,245–1,255 | 0.28 | 0.38 | 1.99–2.09 | 86.3–94.0 | 93–105 |
| ext4 | tuned | tickertape | safe | 2 | 2.09 | 2.49–2.51 | 3.88–3.92 | 76.0–95.3 | 1,246–1,327 | 2.63 | 3.05–4.25 | 5.24–5.30 | 106–167 | 142–199 |
| ext4 | tuned | Chronicle Queue |  | 2 | 0.45–0.50 | 0.61–0.64 | 10.9–11.0 | 108–114 | 1,908–1,940 | 0.44–0.49 | 0.55–0.60 | 9.01–9.72 | 126–137 | 1,719–1,737 |
| ext4 | tuned | Aeron IPC |  | 2 | 0.26–0.27 | 0.37–0.38 | 0.46–0.97 | 96.4–103 | 107–109 | 0.39–0.40 | 0.55–0.58 | 0.72–1.23 | 90.9–106 | 29–406 |
| ext4 | tuned | yaque |  | 2 | 1.03 | 2.64–2.68 | 5.20–5.40 | 86.7–97.5 | 407–560 | 1.61–1.63 | 3.62–3.98 | 9.18–9.40 | 227–250 | 427–8,506 |
| ext4 | tuned | plain file |  | 2 | 0.95–0.96 | 2.55–2.66 | 4.51–4.78 | 117–123 | 271–283 | 1.41–1.48 | 3.26–3.43 | 8.08–8.18 | 238–266 | 249–271 |
| xfs | pinned | tickertape | zero-copy | 2 | 0.30 | 0.46–5.09 | 2.21–6.62 | 65.5–68.7 | 806–10,839 | 0.28 | 0.47–1.84 | 2.64–2.78 | 111 | 297–528 |
| xfs | pinned | tickertape | safe | 2 | 1.97 | 3.96–6.94 | 5.26–8.31 | 63.6–84.4 | 758–11,771 | 2.51 | 4.51–4.79 | 6.51–7.07 | 109–110 | 277–11,244 |
| xfs | pinned | Chronicle Queue |  | 2 | 0.45–0.46 | 0.61–3.99 | 4.11–6.00 | 123–125 | 1,624–12,728 | 0.42–0.44 | 3.19–3.30 | 4.36–5.46 | 129–156 | 14,966–15,118 |
| xfs | pinned | Aeron IPC |  | 2 | 0.26 | 0.35–2.32 | 0.39–3.62 | 34.5–39.5 | 94–11,084 | 0.37–0.40 | 0.45–1.04 | 0.62–2.18 | 93.8–548 | 137–8,979 |
| xfs | pinned | yaque |  | 2 | 3.80–3.83 | 7.23–8.52 | 10.2–10.6 | 118–121 | 913–1,197 | 4.36–4.42 | 5.97–6.02 | 11.5–11.7 | 238–257 | 1,048–1,261 |
| xfs | pinned | plain file |  | 2 | 3.59–3.65 | 7.96–8.43 | 10.1 | 116–256 | 418–11,204 | 4.21–4.28 | 5.57–6.72 | 7.57–11.4 | 124–247 | 285–474 |

**1,024 B at 10,000 records/s**

| FS | Config | Tool | Reader | n | x86-64 p50 | p99 | p99.9 | max | late sends | aarch64 p50 | p99 | p99.9 | max | late sends |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| ext4 | pinned | tickertape | zero-copy | 4 | 0.76–0.78 | 2.65–3.06 | 4.60–6.84 | 1.24–1.32 ms | 618–39,615 | 0.54–0.55 | 2.65–2.78 | 8.73–9.10 | 1.01–1.02 ms | 49–342 |
| ext4 | pinned | tickertape | safe | 4 | 2.46–2.49 | 4.35–4.40 | 6.94–7.64 | 1.29–1.37 ms | 603–734 | 2.64–2.67 | 5.44–5.66 | 11.6–12.4 | 1.05–1.06 ms | 48–256 |
| ext4 | pinned | Chronicle Queue |  | 4 | 0.61–0.62 | 1.41–1.78 | 2.36 µs to 15.75 ms | 180 µs to 111.61 ms | 840–2,252 | 0.52–0.53 | 0.71–0.72 | 2.77–3.46 | 424 µs to 35.06 ms | 40–493 |
| ext4 | pinned | Aeron IPC |  | 4 | 0.39–0.45 | 2.38–2.40 | 3.26–3.64 | 75.3–345 | 223–274 | 0.48–0.51 | 2.85–3.45 | 4.57–5.96 | 68.1–101 | 12–6,549 |
| ext4 | pinned | yaque |  | 4 | 1.31–1.33 | 5.62–6.04 | 144–888 | 12.55–14.04 ms | 133–150 | 1.77–1.82 | 8.79–9.00 | 6.73–6.97 ms | 19.28–19.53 ms | 246–647 |
| ext4 | pinned | plain file |  | 4 | 1.20–1.21 | 4.38–4.60 | 5.89–6.02 | 356–639 | 103–284 | 1.49–1.51 | 4.49–4.92 | 8.39–8.79 | 260–750 | 56–504 |
| ext4 | unpinned | tickertape | zero-copy | 2 | 0.80–0.81 | 2.55–2.73 | 4.76–6.70 | 1.15–1.28 ms | 650–733 | 0.54–0.57 | 2.62–2.74 | 8.28–8.93 | 1.02–1.03 ms | 51–53 |
| ext4 | unpinned | tickertape | safe | 2 | 2.47–2.52 | 4.44–4.48 | 7.16–8.04 | 1.27–1.29 ms | 762–795 | 2.66–2.67 | 5.42–5.76 | 11.1–11.4 | 1.05–1.06 ms | 377–34,627 |
| ext4 | unpinned | Chronicle Queue |  | 2 | 0.62–0.65 | 1.34–1.83 | 2.36 µs to 25.74 ms | 172 µs to 124.13 ms | 818–2,161 | 0.51–0.52 | 0.70–1.32 | 2.87–3.79 | 1.25–73.60 ms | 119–1,669 |
| ext4 | unpinned | Aeron IPC |  | 2 | 0.39–0.40 | 2.37–2.38 | 3.05–3.12 | 201–349 | 177–239 | 0.49–0.50 | 3.07–3.19 | 4.72–5.02 | 40.9–62.3 | 17–154 |
| ext4 | unpinned | yaque |  | 2 | 1.36–1.41 | 5.74–7.95 | 120–260 | 11.98–13.22 ms | 124–110,131 | 1.79–1.83 | 9.04–9.26 | 6.93–7.11 ms | 19.51–19.69 ms | 260–263 |
| ext4 | unpinned | plain file |  | 2 | 1.23–1.24 | 4.22–4.61 | 5.66–6.20 | 446–617 | 87–566 | 1.55–1.56 | 4.70–4.96 | 8.50–8.82 | 515 µs to 1.92 ms | 78–85 |
| ext4 | jvm-old | Chronicle Queue |  | 4 | 0.61–0.63 | 1.78–1.84 | 34.8–36.8 | 262 µs to 16.02 ms | 361–534 | 0.52–0.54 | 1.85–3.71 | 36.4–37.6 | 446 µs to 21.63 ms | 170–10,300 |
| ext4 | jvm-old | Aeron IPC |  | 4 | 0.39–0.45 | 2.35–6.16 | 3.13–7.72 | 74.4 µs to 6.18 ms | 237–367 | 0.51–0.52 | 3.06–3.29 | 4.80–6.00 | 50.9 µs to 29.11 ms | 46–342 |
| ext4 | microtouch-only | Chronicle Queue |  | 4 | 0.61–0.63 | 1.73–1.80 | 2.35–2.51 | 350–399 | 313–424 | 0.52–0.55 | 1.70–1.73 | 2.71–2.94 | 740 µs to 1.18 ms | 181–206 |
| ext4 | tuned | tickertape | zero-copy | 2 | 0.76–0.77 | 2.35–2.66 | 4.98–5.18 | 1.06–1.07 ms | 1,103–1,172 | 0.53–0.54 | 2.94–2.95 | 9.06–10.5 | 940 µs to 1.51 ms | 60–141 |
| ext4 | tuned | tickertape | safe | 2 | 2.67–2.68 | 4.35–4.37 | 8.34–8.76 | 1.04–1.14 ms | 1,109–1,172 | 2.90–2.92 | 5.72–5.78 | 10.6–11.9 | 972 µs to 1.00 ms | 56–66 |
| ext4 | tuned | Chronicle Queue |  | 2 | 0.62–0.63 | 1.64–1.69 | 2.54–2.55 | 245 µs to 3.19 ms | 1,348–1,368 | 0.51 | 0.69–0.70 | 2.66–3.78 | 945 µs to 78.91 ms | 193–868 |
| ext4 | tuned | Aeron IPC |  | 2 | 0.42–0.44 | 2.08 | 3.13–3.44 | 77.6–206 | 348–400 | 0.53–0.57 | 3.09–3.32 | 5.04–5.62 | 39.7–49.5 | 12–161 |
| ext4 | tuned | yaque |  | 2 | 1.47 | 5.08–5.34 | 93.7–98.4 | 12.77–12.85 ms | 104–120 | 2.02 | 8.30–8.46 | 5.75–5.80 ms | 18.45–18.56 ms | 114–119 |
| ext4 | tuned | plain file |  | 2 | 1.30–1.36 | 4.41–4.45 | 5.42–5.43 | 336 µs to 1.09 ms | 79–81 | 1.73–1.76 | 5.44–5.50 | 8.46–8.72 | 398–455 | 41–46 |
| xfs | pinned | tickertape | zero-copy | 2 | 0.77–0.85 | 2.66–6.18 | 4.42–10.9 | 1.11–1.28 ms | 649–110,538 | 0.55 | 2.81–3.03 | 11.6–12.7 | 1.03–1.09 ms | 69–448 |
| xfs | pinned | tickertape | safe | 2 | 2.50–2.53 | 3.95–3.98 | 6.68–6.80 | 1.23–1.26 ms | 611–798 | 2.77 | 5.46–5.51 | 12.6–14.4 | 1.07 ms | 59–62 |
| xfs | pinned | Chronicle Queue |  | 2 | 0.61–0.62 | 1.49–1.55 | 2.39–2.53 | 164–194 | 582–852 | 0.53–0.54 | 0.75–2.79 | 2.98–4.27 | 191–197 | 55–75,918 |
| xfs | pinned | Aeron IPC |  | 2 | 0.38–0.55 | 2.33–2.35 | 2.60–2.64 | 208–226 | 207–222 | 0.52 | 3.65–4.02 | 4.91–5.33 | 5.84–7.67 ms | 27–29 |
| xfs | pinned | yaque |  | 2 | 4.18–4.19 | 6.42–6.44 | 9.64–10.0 | 7.31–7.52 ms | 101–137 | 4.94–5.02 | 8.94–10.5 | 188–208 | 12.34–12.35 ms | 132–141 |
| xfs | pinned | plain file |  | 2 | 3.77–3.78 | 5.42–5.43 | 7.02–7.23 | 843 µs to 1.04 ms | 195–364 | 4.67–4.87 | 7.96–8.81 | 12.1–12.6 | 99.9–143 | 435–6,439 |

**1,024 B at 100,000 records/s**

| FS | Config | Tool | Reader | n | x86-64 p50 | p99 | p99.9 | max | late sends | aarch64 p50 | p99 | p99.9 | max | late sends |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| ext4 | pinned | tickertape | zero-copy | 4 | 0.76–0.77 | 2.74–5.57 | 28.6–29.8 | 1.08–1.10 ms | 9,090–20,099 | 0.52–0.53 | 2.02–2.22 | 24.8–28.5 | 993 µs to 1.01 ms | 2,115–2,552 |
| ext4 | pinned | tickertape | safe | 4 | 2.49–2.51 | 4.12–7.12 | 29.0–30.2 | 1.14–1.18 ms | 12,659–20,206 | 2.63–2.66 | 5.04–5.46 | 24.3–31.1 | 1.02–1.08 ms | 2,182–13,376 |
| ext4 | pinned | Chronicle Queue |  | 4 | 0.53–0.56 | 13.0–13.4 | 23.9–24.6 | 288–550 | 23,479–34,986 | 0.45–0.52 | 3.26–5.22 | 14.5–16.3 | 178–481 | 12,099–23,741 |
| ext4 | pinned | Aeron IPC |  | 4 | 0.41–0.54 | 0.69–0.71 | 0.77–5.58 | 38.5–129 | 110–401 | 0.43–0.48 | 0.73–1.68 | 0.82–2.71 | 87.6–93.1 | 36–18,257 |
| ext4 | pinned | yaque |  | 4 | 1.27–1.29 | 50.0–55.3 | 10.15–10.22 ms | 11.83–12.11 ms | 2,311–12,971 | 1.67–1.71 | 7.16–7.43 ms | 18.02–18.30 ms | 19.32–19.73 ms | 4,789–22,244 |
| ext4 | pinned | plain file |  | 4 | 1.15–1.16 | 4.26–4.52 | 9.85–11.3 | 101–119 | 1,143–4,731 | 1.45–1.46 | 5.23–6.73 | 14.4–14.7 | 137–142 | 1,302–12,872 |
| ext4 | unpinned | tickertape | zero-copy | 2 | 0.78–0.80 | 2.79–6.38 | 28.9–29.2 | 1.08–1.09 ms | 6,778–7,971 | 0.53 | 2.00–2.01 | 23.9–24.1 | 994 µs to 1.01 ms | 2,061–2,107 |
| ext4 | unpinned | tickertape | safe | 2 | 2.48–2.50 | 4.74–5.21 | 28.0–29.4 | 1.14 ms | 15,546–19,815 | 2.64 | 4.83–4.93 | 23.9–25.2 | 1.02–1.07 ms | 2,248–5,378 |
| ext4 | unpinned | Chronicle Queue |  | 2 | 0.57–0.63 | 13.3–13.4 | 24.1–24.4 | 337–339 | 24,203–34,649 | 0.46–0.49 | 2.43–5.37 | 13.2–15.7 | 182–188 | 11,137–25,129 |
| ext4 | unpinned | Aeron IPC |  | 2 | 0.42–0.47 | 0.70–2.61 | 0.80–4.05 | 107–111 | 191–11,117 | 0.46–0.47 | 0.92–1.50 | 2.33–2.36 | 89.0 µs to 2.02 ms | 5,470–10,386 |
| ext4 | unpinned | yaque |  | 2 | 1.32–1.35 | 42.4–42.5 | 9.99 ms | 11.75–11.93 ms | 2,177–12,851 | 1.69–1.70 | 7.28–7.47 ms | 18.15–18.35 ms | 19.50–19.82 ms | 5,495–5,670 |
| ext4 | unpinned | plain file |  | 2 | 1.19–1.23 | 6.21–6.89 | 12.3–13.1 | 107–108 | 11,928–12,124 | 1.42–1.46 | 5.52–6.24 | 11.2–15.9 | 163–164 | 1,524–2,258 |
| ext4 | jvm-old | Chronicle Queue |  | 4 | 0.53–0.60 | 26.0–26.5 | 35.4–37.4 | 319–378 | 27,002–38,148 | 0.45–0.46 | 21.5–22.2 | 33.7–35.2 | 455–514 | 24,207–33,293 |
| ext4 | jvm-old | Aeron IPC |  | 4 | 0.40–0.42 | 0.69–3.58 | 0.77–4.96 | 117–159 | 1,218–12,168 | 0.48–0.57 | 0.72–1.89 | 0.81–2.90 | 129–173 | 1,095–9,993 |
| ext4 | microtouch-only | Chronicle Queue |  | 4 | 0.54–0.59 | 13.7–13.9 | 25.2–26.2 | 327–644 | 26,674–32,671 | 0.44–0.52 | 3.88–8.07 | 18.7–29.2 | 451–488 | 15,346–38,923 |
| ext4 | tuned | tickertape | zero-copy | 2 | 0.76–0.77 | 3.09–3.10 | 28.3–28.7 | 984 µs to 1.06 ms | 13,338–13,857 | 0.52 | 2.17–2.38 | 27.0–36.8 | 932–983 | 2,257–4,976 |
| ext4 | tuned | tickertape | safe | 2 | 2.67–2.68 | 4.80–4.83 | 31.3 | 1.08–1.19 ms | 13,845–14,223 | 2.86–2.87 | 5.17–5.23 | 25.9–27.4 | 960–965 | 2,303–2,418 |
| ext4 | tuned | Chronicle Queue |  | 2 | 0.60–0.61 | 12.6 | 23.1 | 175–225 | 22,327–22,406 | 0.49–0.50 | 2.95–4.90 | 14.0–15.9 | 180–197 | 13,426–17,374 |
| ext4 | tuned | Aeron IPC |  | 2 | 0.38–0.45 | 0.70–0.71 | 0.79–1.36 | 21.6–43.8 | 91–103 | 0.50–0.51 | 0.73–0.77 | 0.84–1.27 | 97.2–102 | 30–94 |
| ext4 | tuned | yaque |  | 2 | 1.42–1.43 | 8.51–8.87 | 9.34–9.40 ms | 10.98–11.16 ms | 1,389–1,479 | 1.87–1.89 | 6.00–6.04 ms | 16.86–16.91 ms | 18.27–18.38 ms | 3,012–3,907 |
| ext4 | tuned | plain file |  | 2 | 1.27–1.29 | 4.32–4.34 | 6.55–6.65 | 84.7–89.7 | 739–936 | 1.62–1.65 | 5.56–7.02 | 9.25–9.54 | 103–108 | 1,008–7,740 |
| xfs | pinned | tickertape | zero-copy | 2 | 0.77 | 2.69–3.97 | 25.5–26.4 | 1.07 ms | 8,813–19,795 | 0.51–0.52 | 2.06–2.82 | 38.8–40.8 | 1.03 ms | 2,881–13,728 |
| xfs | pinned | tickertape | safe | 2 | 2.53 | 3.84–3.88 | 25.6–25.8 | 1.12–1.13 ms | 8,728–9,908 | 2.74–2.75 | 4.76–6.58 | 33.3–33.8 | 1.04–1.05 ms | 2,817–2,866 |
| xfs | pinned | Chronicle Queue |  | 2 | 0.53 | 5.22–5.24 | 15.6–15.7 | 191–276 | 17,900–17,923 | 0.47–0.50 | 1.62–2.57 | 3.36–5.40 | 191–198 | 2,341–22,338 |
| xfs | pinned | Aeron IPC |  | 2 | 0.42–0.54 | 0.71–1.90 | 1.33–3.33 | 43.9–100 | 549–11,185 | 0.48 | 0.82–2.33 | 1.46–3.67 | 97.9–102 | 67–11,189 |
| xfs | pinned | yaque |  | 2 | 4.03–4.04 | 10.2–10.6 | 5.20–5.30 ms | 6.57–6.59 ms | 2,717–2,772 | 4.62–4.80 | 300–398 | 11.12–11.21 ms | 12.43–12.55 ms | 4,388–4,456 |
| xfs | pinned | plain file |  | 2 | 3.64–3.65 | 5.53–8.46 | 17.6–17.9 | 107–109 | 3,088–12,293 | 4.58–4.66 | 9.48–9.94 | 23.2–23.6 | 118 | 1,742–12,667 |

- **p99.9, `pinned`, ext4.** Chronicle Queue is the lowest at 10,000 records/s in three of the four payload and machine
  cells, and in three of four runs in the fourth (x86-64, 1,024 B: 2.36–2.82 µs; its fourth run, 15.75 ms, is above both
  tickertape readers; one of the two `unpinned` runs had 25.74 ms; neither sample is attributed to a setting). Aeron has
  the lowest observed p99.9 at 100,000 records/s in all four cells: its whole range is below tickertape's at 1,024 B and
  overlaps it at 64 B (0.39–3.47 against 2.27–5.96 on x86-64, 0.62–3.03 against 1.88–2.08 on aarch64); it is also lower
  than the zero-copy reader at 1,024 B × 10,000 records/s (3.26–3.64 against 4.60–6.84; 4.57–5.96 against 8.73–9.10),
  where the plain file's ranges overlap tickertape's (5.89–6.02; 8.39–8.79). tickertape's zero-copy reader is lower than
  Chronicle's at 64 B × 100,000 records/s (2.27–5.96 against 11.8–15.3 on x86-64, 1.88–2.08 against 7.85–13.3 on
  aarch64) and higher at 1,024 B × 100,000 records/s (28.6–29.8 against 23.9–24.6 on x86-64, 24.8–28.5 against 14.5–16.3
  on aarch64), where the plain file is lower than tickertape too (9.85–11.3 and 14.4–14.7).
- **p99.9, xfs** (`pinned`, n = 2). At 64 B × 100,000 records/s on x86-64 the zero-copy reader's range overlaps
  Chronicle's (2.21–6.62 against 4.11–6.00) and the safe reader's is above it (5.26–8.31). Elsewhere the gaps to
  Chronicle are larger than on ext4: aarch64 1,024 B × 100,000 records/s, tickertape 38.8–40.8 against 3.36–5.40 (the
  plain file 23.2–23.6); aarch64 1,024 B × 10,000 records/s, 11.6–12.7 against 2.98–4.27; aarch64 64 B × 10,000
  records/s, 5.07–5.34 against 0.75–0.81; x86-64 1,024 B × 100,000 records/s, 25.5–26.4 against 15.6–15.7.
- **Maxima** (`pinned`, ext4). At 1,024 B tickertape's zero-copy reader had a maximum of 993 µs to 1.32 ms in every run
  (both readers: 993 µs to 1.37 ms; the cause was not traced); at 100,000 records/s Chronicle Queue's maxima were
  178–550 µs, Aeron's 38.5–129 µs and the plain file's 101–142 µs, and over both rates Chronicle's were 178 µs to 111.61
  ms and Aeron's 38.5–345 µs. At 64 B tickertape's were 70.1–278 µs, Chronicle's 25.3 µs to 1.01 ms and Aeron's 36.8 µs
  to 71.30 ms.
- **Late sends** count the writer's sends that started late, not lost records; a late send's delay is in the latency.

### Sustained mode (ten-minute runs)

`compare/run.py --sustained` ([`compare/README.md`](../compare/README.md) → "Sustained mode"): 600 s measured after a 10
s warm-up, one reader following. **Probe:** unpaced, the reader's latency from the actual send. **SV:** paced at 50 % of
the same unit's probe, latency from the scheduled send, no sync. **SD:** SV plus the tool's sync every second on the
writer's thread; "sync" is its duration. SV stands for sustained visibility, SD for sustained durability. tickertape
runs its zero-copy reader (`Reader::open_mapped`) and, in SD, the blocking `Writer::checkpoint()`. Parts: `sustained`
(pinned, two rounds), `sustained-jvm-old`, `sustained-others` (yaque, queue-file, plain file: probe and SV only),
`tuned-sustained`, `sustained-90` (SD at 90 % of the probe), `sustained-journals` (xfs). n = 1 per row. µs unless
marked. **The target rates differ per tool and round:** read every SV and SD figure with the rate beside it.

**Probe, 64 B (unpaced; records/s and reader latency from the send)**

| Machine | FS | Part | Config | Tool | Round | Target (records/s) | Achieved (records/s) | p50 | p99 | p99.9 | max |
|---|---|---|---|---|---|---|---|---|---|---|---|
| x86-64 | ext4 | sustained | pinned | tickertape | 1 | unpaced | 3,543,632 | 0.47 | 2.90 | 8.63 | 355.47 ms |
| x86-64 | ext4 | sustained | pinned | tickertape | 2 | unpaced | 3,529,856 | 0.47 | 2.89 | 8.34 | 312.74 ms |
| x86-64 | ext4 | sustained | pinned | Chronicle Queue | 1 | unpaced | 3,061,325 | 0.50 | 0.88 | 6.53 | 143.79 ms |
| x86-64 | ext4 | sustained | pinned | Chronicle Queue | 2 | unpaced | 2,952,843 | 0.52 | 0.88 | 6.50 | 143.65 ms |
| x86-64 | ext4 | sustained | pinned | Aeron IPC | 1 | unpaced | 7,206,880 | 0.30 | 0.78 | 0.95 | 69.99 ms |
| x86-64 | ext4 | sustained | pinned | Aeron IPC | 2 | unpaced | 6,489,247 | 0.38 | 0.86 | 1.06 | 54.98 ms |
| x86-64 | ext4 | sustained-jvm-old | jvm-old | Chronicle Queue | 1 | unpaced | 2,993,947 | 0.50 | 0.88 | 7.67 | 144.97 ms |
| x86-64 | ext4 | sustained-jvm-old | jvm-old | Aeron IPC | 1 | unpaced | 6,849,910 | 0.37 | 0.83 | 0.95 | 47.09 ms |
| x86-64 | ext4 | sustained-others | pinned | yaque | 1 | unpaced | 1,020,018 | 0.98 | 2.07 ms | 10.77 ms | 12.46 ms |
| x86-64 | ext4 | sustained-others | pinned | queue-file | 1 | unpaced | 929,189 | writer only | writer only | writer only | writer only |
| x86-64 | ext4 | sustained-others | pinned | plain file | 1 | unpaced | 1,716,506 | 1.26 | 2.68 | 6.76 | 141.95 ms |
| x86-64 | ext4 | tuned-sustained | tuned | tickertape | 1 | unpaced | 2,387,955 | 0.50 | 2.97 | 6.76 | 497.81 ms |
| x86-64 | ext4 | tuned-sustained | tuned | Chronicle Queue | 1 | unpaced | 3,056,715 | 0.49 | 0.92 | 2.48 | 143.65 ms |
| x86-64 | ext4 | tuned-sustained | tuned | Aeron IPC | 1 | unpaced | 7,366,146 | 0.33 | 0.85 | 0.93 | 72.48 ms |
| x86-64 | ext4 | sustained-90 | pinned | tickertape | 1 | unpaced | 3,542,443 | 0.46 | 2.62 | 7.92 | 118.82 ms |
| x86-64 | ext4 | sustained-90 | pinned | Chronicle Queue | 1 | unpaced | 2,944,959 | 0.53 | 0.91 | 6.63 | 143.52 ms |
| x86-64 | xfs | sustained-journals | pinned | tickertape | 1 | unpaced | 3,519,690 | 0.50 | 2.75 | 9.46 | 25.82 ms |
| x86-64 | xfs | sustained-journals | pinned | Chronicle Queue | 1 | unpaced | 3,517,607 | 0.45 | 0.89 | 6.47 | 4.75 ms |
| x86-64 | xfs | sustained-90 | pinned | tickertape | 1 | unpaced | 3,543,512 | 0.48 | 2.72 | 8.34 | 33.91 ms |
| x86-64 | xfs | sustained-90 | pinned | Chronicle Queue | 1 | unpaced | 3,493,704 | 0.46 | 0.88 | 6.53 | 4.66 ms |
| aarch64 | ext4 | sustained | pinned | tickertape | 1 | unpaced | 3,458,048 | 0.67 | 1.92 | 9.82 | 322.96 ms |
| aarch64 | ext4 | sustained | pinned | tickertape | 2 | unpaced | 3,466,622 | 0.67 | 1.69 | 7.53 | 401.08 ms |
| aarch64 | ext4 | sustained | pinned | Chronicle Queue | 1 | unpaced | 2,570,572 | 0.68 | 1.26 | 8.34 | 139.33 ms |
| aarch64 | ext4 | sustained | pinned | Chronicle Queue | 2 | unpaced | 2,575,258 | 0.68 | 1.33 | 15.1 | 144.57 ms |
| aarch64 | ext4 | sustained | pinned | Aeron IPC | 1 | unpaced | 3,246,052 | 0.65 | 1.16 | 4.64 | 82.58 ms |
| aarch64 | ext4 | sustained | pinned | Aeron IPC | 2 | unpaced | 3,820,950 | 0.61 | 1.15 | 5.08 | 79.56 ms |
| aarch64 | ext4 | sustained-jvm-old | jvm-old | Chronicle Queue | 1 | unpaced | 2,641,531 | 0.63 | 1.44 | 18.7 | 3.84 s |
| aarch64 | ext4 | sustained-jvm-old | jvm-old | Aeron IPC | 1 | unpaced | 3,571,671 | 0.64 | 1.18 | 4.96 | 48.17 ms |
| aarch64 | ext4 | sustained-others | pinned | yaque | 1 | unpaced | 749,494 | 1.48 | 1.87 ms | 15.56 ms | 16.89 ms |
| aarch64 | ext4 | sustained-others | pinned | queue-file | 1 | unpaced | 562,648 | writer only | writer only | writer only | writer only |
| aarch64 | ext4 | sustained-others | pinned | plain file | 1 | unpaced | 1,133,796 | 1.76 | 3.57 | 7.98 | 142.74 ms |
| aarch64 | ext4 | tuned-sustained | tuned | tickertape | 1 | unpaced | 3,457,102 | 0.76 | 1.92 | 5.42 | 369.89 ms |
| aarch64 | ext4 | tuned-sustained | tuned | Chronicle Queue | 1 | unpaced | 2,627,714 | 0.72 | 1.29 | 5.33 | 139.98 ms |
| aarch64 | ext4 | tuned-sustained | tuned | Aeron IPC | 1 | unpaced | 3,372,575 | 0.70 | 1.20 | 4.23 | 83.23 ms |
| aarch64 | ext4 | sustained-90 | pinned | tickertape | 1 | unpaced | 3,462,723 | 0.69 | 1.71 | 8.47 | 347.60 ms |
| aarch64 | ext4 | sustained-90 | pinned | Chronicle Queue | 1 | unpaced | 2,582,124 | 0.65 | 1.25 | 10.8 | 140.90 ms |
| aarch64 | xfs | sustained-journals | pinned | tickertape | 1 | unpaced | 3,465,454 | 0.68 | 1.98 | 8.02 | 82.25 ms |
| aarch64 | xfs | sustained-journals | pinned | Chronicle Queue | 1 | unpaced | 2,772,747 | 0.70 | 1.36 | 9.21 | 1.37 ms |
| aarch64 | xfs | sustained-90 | pinned | tickertape | 1 | unpaced | 3,464,276 | 0.69 | 1.82 | 8.08 | 47.02 ms |
| aarch64 | xfs | sustained-90 | pinned | Chronicle Queue | 1 | unpaced | 3,002,149 | 0.69 | 1.50 | 9.22 | 5.36 ms |

**Probe, 1,024 B**

| Machine | FS | Part | Config | Tool | Round | Target (records/s) | Achieved (records/s) | p50 | p99 | p99.9 | max |
|---|---|---|---|---|---|---|---|---|---|---|---|
| x86-64 | ext4 | sustained | pinned | tickertape | 1 | unpaced | 346,382 | 0.94 | 9.16 | 29.1 | 331.87 ms |
| x86-64 | ext4 | sustained | pinned | tickertape | 2 | unpaced | 345,534 | 0.98 | 10.4 | 28.9 | 386.14 ms |
| x86-64 | ext4 | sustained | pinned | Chronicle Queue | 1 | unpaced | 350,374 | 0.45 | 5.58 | 33.5 | 170.66 ms |
| x86-64 | ext4 | sustained | pinned | Chronicle Queue | 2 | unpaced | 349,231 | 0.56 | 6.16 | 33.9 | 176.55 ms |
| x86-64 | ext4 | sustained | pinned | Aeron IPC | 1 | unpaced | 4,175,027 | 0.64 | 1.11 | 1.36 | 82.31 ms |
| x86-64 | ext4 | sustained | pinned | Aeron IPC | 2 | unpaced | 4,036,635 | 0.66 | 1.12 | 1.89 | 83.56 ms |
| x86-64 | ext4 | sustained-jvm-old | jvm-old | Chronicle Queue | 1 | unpaced | 350,249 | 0.61 | 6.71 | 33.7 | 170.92 ms |
| x86-64 | ext4 | sustained-jvm-old | jvm-old | Aeron IPC | 1 | unpaced | 4,148,741 | 0.64 | 1.09 | 1.34 | 58.20 ms |
| x86-64 | ext4 | sustained-others | pinned | yaque | 1 | unpaced | 685,628 | 1.62 | 10.04 ms | 10.72 ms | 11.89 ms |
| x86-64 | ext4 | sustained-others | pinned | queue-file | 1 | unpaced | 314,733 | writer only | writer only | writer only | writer only |
| x86-64 | ext4 | sustained-others | pinned | plain file | 1 | unpaced | 348,781 | 1.60 | 4.73 | 8.97 | 184.68 ms |
| x86-64 | ext4 | tuned-sustained | tuned | tickertape | 1 | unpaced | 339,496 | 0.96 | 4.90 | 28.3 | 405.27 ms |
| x86-64 | ext4 | tuned-sustained | tuned | Chronicle Queue | 1 | unpaced | 347,165 | 0.49 | 2.76 | 32.1 | 181.93 ms |
| x86-64 | ext4 | tuned-sustained | tuned | Aeron IPC | 1 | unpaced | 4,199,806 | 0.67 | 1.13 | 2.17 | 156.89 ms |
| x86-64 | ext4 | sustained-90 | pinned | tickertape | 1 | unpaced | 345,787 | 0.98 | 10.4 | 28.8 | 354.16 ms |
| x86-64 | ext4 | sustained-90 | pinned | Chronicle Queue | 1 | unpaced | 350,361 | 0.55 | 5.65 | 33.6 | 171.70 ms |
| x86-64 | xfs | sustained-journals | pinned | tickertape | 1 | unpaced | 346,525 | 1.02 | 9.95 | 23.9 | 304.61 ms |
| x86-64 | xfs | sustained-journals | pinned | Chronicle Queue | 1 | unpaced | 350,929 | 0.62 | 6.10 | 25.3 | 20.68 ms |
| x86-64 | xfs | sustained-90 | pinned | tickertape | 1 | unpaced | 345,749 | 1.04 | 10.3 | 23.3 | 249.17 ms |
| x86-64 | xfs | sustained-90 | pinned | Chronicle Queue | 1 | unpaced | 351,012 | 0.60 | 6.22 | 25.4 | 20.97 ms |
| aarch64 | ext4 | sustained | pinned | tickertape | 1 | unpaced | 338,179 | 0.91 | 2.77 | 18.4 | 374.87 ms |
| aarch64 | ext4 | sustained | pinned | tickertape | 2 | unpaced | 337,424 | 0.92 | 3.06 | 18.7 | 347.60 ms |
| aarch64 | ext4 | sustained | pinned | Chronicle Queue | 1 | unpaced | 343,736 | 0.71 | 4.52 | 25.2 | 171.84 ms |
| aarch64 | ext4 | sustained | pinned | Chronicle Queue | 2 | unpaced | 341,297 | 0.70 | 4.32 | 24.2 | 375.65 ms |
| aarch64 | ext4 | sustained | pinned | Aeron IPC | 1 | unpaced | 2,776,684 | 0.70 | 1.21 | 3.62 | 86.18 ms |
| aarch64 | ext4 | sustained | pinned | Aeron IPC | 2 | unpaced | 2,796,872 | 0.69 | 1.20 | 3.73 | 65.21 ms |
| aarch64 | ext4 | sustained-jvm-old | jvm-old | Chronicle Queue | 1 | unpaced | 343,855 | 0.66 | 7.30 | 42.1 | 174.59 ms |
| aarch64 | ext4 | sustained-jvm-old | jvm-old | Aeron IPC | 1 | unpaced | 2,451,881 | 0.80 | 1.33 | 4.21 | 90.31 ms |
| aarch64 | ext4 | sustained-others | pinned | yaque | 1 | unpaced | 496,821 | 2.29 | 15.78 ms | 16.83 ms | 17.47 ms |
| aarch64 | ext4 | sustained-others | pinned | queue-file | 1 | unpaced | 309,778 | writer only | writer only | writer only | writer only |
| aarch64 | ext4 | sustained-others | pinned | plain file | 1 | unpaced | 342,397 | 2.04 | 6.79 | 10.0 | 279.97 ms |
| aarch64 | ext4 | tuned-sustained | tuned | tickertape | 1 | unpaced | 338,590 | 0.98 | 3.06 | 19.5 | 345.24 ms |
| aarch64 | ext4 | tuned-sustained | tuned | Chronicle Queue | 1 | unpaced | 343,774 | 0.76 | 4.45 | 23.8 | 174.46 ms |
| aarch64 | ext4 | tuned-sustained | tuned | Aeron IPC | 1 | unpaced | 2,789,097 | 0.75 | 1.31 | 2.78 | 86.38 ms |
| aarch64 | ext4 | sustained-90 | pinned | tickertape | 1 | unpaced | 338,916 | 0.90 | 2.72 | 18.4 | 363.07 ms |
| aarch64 | ext4 | sustained-90 | pinned | Chronicle Queue | 1 | unpaced | 343,739 | 0.70 | 4.54 | 23.9 | 173.41 ms |
| aarch64 | xfs | sustained-journals | pinned | tickertape | 1 | unpaced | 338,555 | 0.93 | 3.32 | 11.9 | 28.25 ms |
| aarch64 | xfs | sustained-journals | pinned | Chronicle Queue | 1 | unpaced | 340,696 | 0.69 | 4.33 | 13.1 | 20.14 ms |
| aarch64 | xfs | sustained-90 | pinned | tickertape | 1 | unpaced | 338,426 | 0.92 | 2.88 | 11.3 | 117.05 ms |
| aarch64 | xfs | sustained-90 | pinned | Chronicle Queue | 1 | unpaced | 341,594 | 0.73 | 4.56 | 14.5 | 21.02 ms |

**SV, 64 B (paced at 50 % of the probe, no sync)**

| Machine | FS | Part | Config | Tool | Round | Target (records/s) | Achieved (records/s) | p50 | p99 | p99.9 | max | Late sends |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| x86-64 | ext4 | sustained | pinned | tickertape | 1 | 1,771,815 | 1,771,814 | 0.32 | 3.93 | 642 | 1.69 ms | 37,205,017 (3.4 %) |
| x86-64 | ext4 | sustained | pinned | tickertape | 2 | 1,764,927 | 1,764,926 | 0.32 | 3.86 | 654 | 1.56 ms | 36,245,831 (3.4 %) |
| x86-64 | ext4 | sustained | pinned | Chronicle Queue | 1 | 1,530,662 | 1,530,662 | 0.43 | 20.14 ms | 102.69 ms | 135.53 ms | 121,529,112 (13.0 %) |
| x86-64 | ext4 | sustained | pinned | Chronicle Queue | 2 | 1,476,421 | 1,476,421 | 0.44 | 20.19 ms | 100.01 ms | 136.05 ms | 113,607,776 (12.6 %) |
| x86-64 | ext4 | sustained | pinned | Aeron IPC | 1 | 3,603,440 | 3,603,440 | 0.26 | 2.94 | 36.24 ms | 78.38 ms | 30,359,971 (1.4 %) |
| x86-64 | ext4 | sustained | pinned | Aeron IPC | 2 | 3,244,623 | 3,244,623 | 0.25 | 1.86 | 34.96 ms | 80.41 ms | 22,103,894 (1.1 %) |
| x86-64 | ext4 | sustained-jvm-old | jvm-old | Chronicle Queue | 1 | 1,496,973 | 1,496,973 | 0.43 | 17.81 ms | 97.39 ms | 138.02 ms | 92,957,728 (10.2 %) |
| x86-64 | ext4 | sustained-jvm-old | jvm-old | Aeron IPC | 1 | 3,424,955 | 3,424,955 | 0.25 | 1.09 | 6.32 | 7.76 ms | 19,620,806 (0.9 %) |
| x86-64 | ext4 | sustained-others | pinned | yaque | 1 | 510,008 | 510,008 | 0.93 | 5.68 | 8.81 ms | 12.87 ms | 8,129,867 (2.6 %) |
| x86-64 | ext4 | sustained-others | pinned | queue-file | 1 | 464,594 | 464,594 | writer only | writer only | writer only | writer only | 27,391,684 (9.7 %) |
| x86-64 | ext4 | sustained-others | pinned | plain file | 1 | 858,252 | 858,252 | 0.81 | 3.79 | 16.69 ms | 122.09 ms | 7,911,298 (1.5 %) |
| x86-64 | ext4 | tuned-sustained | tuned | tickertape | 1 | 1,193,977 | 1,193,977 | 0.33 | 1.80 | 315 | 1.42 ms | 16,211,557 (2.2 %) |
| x86-64 | ext4 | tuned-sustained | tuned | Chronicle Queue | 1 | 1,528,357 | 1,528,357 | 0.43 | 19.10 ms | 97.78 ms | 140.64 ms | 113,619,547 (12.2 %) |
| x86-64 | ext4 | tuned-sustained | tuned | Aeron IPC | 1 | 3,683,073 | 3,683,073 | 0.27 | 1.86 ms | 161.61 ms | 239.73 ms | 27,063,341 (1.2 %) |
| x86-64 | xfs | sustained-journals | pinned | tickertape | 1 | 1,759,845 | 1,759,845 | 0.32 | 3.64 | 432 | 2.47 ms | 33,867,581 (3.2 %) |
| x86-64 | xfs | sustained-journals | pinned | Chronicle Queue | 1 | 1,758,803 | 1,758,803 | 0.44 | 17.07 ms | 33.31 ms | 38.04 ms | 101,006,375 (9.4 %) |
| aarch64 | ext4 | sustained | pinned | tickertape | 1 | 1,729,024 | 1,729,024 | 0.32 | 1.91 | 501 | 1.47 ms | 14,918,964 (1.4 %) |
| aarch64 | ext4 | sustained | pinned | tickertape | 2 | 1,733,310 | 1,733,310 | 0.32 | 2.14 | 697 | 23.81 ms | 15,786,491 (1.5 %) |
| aarch64 | ext4 | sustained | pinned | Chronicle Queue | 1 | 1,285,285 | 1,285,285 | 0.46 | 16.08 ms | 91.82 ms | 134.09 ms | 79,810,518 (10.2 %) |
| aarch64 | ext4 | sustained | pinned | Chronicle Queue | 2 | 1,287,628 | 1,287,628 | 0.44 | 15.25 ms | 89.98 ms | 133.37 ms | 72,099,671 (9.2 %) |
| aarch64 | ext4 | sustained | pinned | Aeron IPC | 1 | 1,623,026 | 1,623,026 | 0.42 | 1.80 | 26.02 ms | 167.64 ms | 8,084,373 (0.8 %) |
| aarch64 | ext4 | sustained | pinned | Aeron IPC | 2 | 1,910,474 | 1,910,474 | 0.36 | 1.38 | 14.72 ms | 78.77 ms | 9,450,923 (0.8 %) |
| aarch64 | ext4 | sustained-jvm-old | jvm-old | Chronicle Queue | 1 | 1,320,765 | 1,320,761 | 0.41 | 14.29 ms | 98.63 ms | 141.30 ms | 54,712,623 (6.8 %) |
| aarch64 | ext4 | sustained-jvm-old | jvm-old | Aeron IPC | 1 | 1,785,835 | 1,785,835 | 0.39 | 1.51 | 4.66 | 20.32 ms | 8,925,432 (0.8 %) |
| aarch64 | ext4 | sustained-others | pinned | yaque | 1 | 374,747 | 374,747 | 1.45 | 5.54 | 12.75 ms | 16.96 ms | 6,616,541 (2.9 %) |
| aarch64 | ext4 | sustained-others | pinned | queue-file | 1 | 281,324 | 281,324 | writer only | writer only | writer only | writer only | 17,067,118 (9.9 %) |
| aarch64 | ext4 | sustained-others | pinned | plain file | 1 | 566,898 | 566,898 | 1.27 | 4.05 | 18.32 ms | 139.59 ms | 5,494,720 (1.6 %) |
| aarch64 | ext4 | tuned-sustained | tuned | tickertape | 1 | 1,728,550 | 1,728,550 | 0.32 | 1.96 | 745 | 127.60 ms | 14,569,988 (1.4 %) |
| aarch64 | ext4 | tuned-sustained | tuned | Chronicle Queue | 1 | 1,313,857 | 1,313,857 | 0.44 | 14.99 ms | 83.23 ms | 135.92 ms | 142,612,709 (17.8 %) |
| aarch64 | ext4 | tuned-sustained | tuned | Aeron IPC | 1 | 1,686,287 | 1,686,287 | 0.40 | 0.56 | 13.68 ms | 83.82 ms | 3,704,881 (0.4 %) |
| aarch64 | xfs | sustained-journals | pinned | tickertape | 1 | 1,732,727 | 1,732,727 | 0.32 | 2.02 | 382 | 1.19 ms | 14,991,537 (1.4 %) |
| aarch64 | xfs | sustained-journals | pinned | Chronicle Queue | 1 | 1,386,373 | 1,386,373 | 0.43 | 5.26 | 10.6 | 4.84 ms | 32,787,002 (3.9 %) |

**SV, 1,024 B**

| Machine | FS | Part | Config | Tool | Round | Target (records/s) | Achieved (records/s) | p50 | p99 | p99.9 | max | Late sends |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| x86-64 | ext4 | sustained | pinned | tickertape | 1 | 173,190 | 173,190 | 0.77 | 3.14 | 504 | 1.40 ms | 2,288,005 (2.2 %) |
| x86-64 | ext4 | sustained | pinned | tickertape | 2 | 172,766 | 172,766 | 0.79 | 3.16 | 508 | 1.38 ms | 2,303,396 (2.2 %) |
| x86-64 | ext4 | sustained | pinned | Chronicle Queue | 1 | 175,186 | 175,186 | 0.57 | 36.14 ms | 100.27 ms | 135.40 ms | 10,087,029 (9.4 %) |
| x86-64 | ext4 | sustained | pinned | Chronicle Queue | 2 | 174,615 | 174,615 | 0.54 | 18.09 ms | 98.44 ms | 139.59 ms | 8,620,182 (8.1 %) |
| x86-64 | ext4 | sustained | pinned | Aeron IPC | 1 | 2,087,513 | 2,087,513 | 0.43 | 21.74 ms | 107.41 ms | 125.96 ms | 32,620,896 (2.6 %) |
| x86-64 | ext4 | sustained | pinned | Aeron IPC | 2 | 2,018,317 | 2,018,317 | 0.43 | 6.08 | 99.16 ms | 119.14 ms | 26,027,403 (2.1 %) |
| x86-64 | ext4 | sustained-jvm-old | jvm-old | Chronicle Queue | 1 | 175,124 | 175,124 | 0.55 | 16.38 ms | 93.72 ms | 138.02 ms | 8,002,266 (7.5 %) |
| x86-64 | ext4 | sustained-jvm-old | jvm-old | Aeron IPC | 1 | 2,074,370 | 2,074,370 | 0.42 | 2.70 ms | 99.61 ms | 159.51 ms | 28,166,634 (2.2 %) |
| x86-64 | ext4 | sustained-others | pinned | yaque | 1 | 342,814 | 342,814 | 1.35 | 7.45 ms | 10.50 ms | 12.48 ms | 6,970,001 (3.3 %) |
| x86-64 | ext4 | sustained-others | pinned | queue-file | 1 | 157,366 | 157,366 | writer only | writer only | writer only | writer only | 10,091,869 (10.5 %) |
| x86-64 | ext4 | sustained-others | pinned | plain file | 1 | 174,390 | 174,390 | 1.19 | 5.70 | 96.93 ms | 140.12 ms | 1,145,757 (1.1 %) |
| x86-64 | ext4 | tuned-sustained | tuned | tickertape | 1 | 169,748 | 169,748 | 0.80 | 3.31 | 492 | 1.69 ms | 2,695,504 (2.6 %) |
| x86-64 | ext4 | tuned-sustained | tuned | Chronicle Queue | 1 | 173,582 | 173,582 | 0.58 | 17.73 ms | 102.04 ms | 143.52 ms | 8,130,548 (7.7 %) |
| x86-64 | ext4 | tuned-sustained | tuned | Aeron IPC | 1 | 2,099,903 | 2,099,903 | 0.43 | 175.24 ms | 297.01 ms | 356.25 ms | 51,708,088 (4.0 %) |
| x86-64 | xfs | sustained-journals | pinned | tickertape | 1 | 173,262 | 173,262 | 0.78 | 3.11 | 284 | 1.78 ms | 2,244,854 (2.1 %) |
| x86-64 | xfs | sustained-journals | pinned | Chronicle Queue | 1 | 175,464 | 175,464 | 0.54 | 14.7 | 25.1 | 5.41 ms | 4,255,662 (4.0 %) |
| aarch64 | ext4 | sustained | pinned | tickertape | 1 | 169,089 | 169,089 | 0.57 | 2.33 | 323 | 1.14 ms | 762,115 (0.7 %) |
| aarch64 | ext4 | sustained | pinned | tickertape | 2 | 168,712 | 168,712 | 0.57 | 2.37 | 401 | 7.83 ms | 768,451 (0.7 %) |
| aarch64 | ext4 | sustained | pinned | Chronicle Queue | 1 | 171,867 | 171,867 | 0.47 | 34.50 ms | 91.42 ms | 162.92 ms | 8,014,347 (7.6 %) |
| aarch64 | ext4 | sustained | pinned | Chronicle Queue | 2 | 170,648 | 170,648 | 0.45 | 35.49 ms | 99.48 ms | 135.53 ms | 7,858,392 (7.5 %) |
| aarch64 | ext4 | sustained | pinned | Aeron IPC | 1 | 1,388,341 | 1,388,341 | 0.48 | 2.91 | 84.28 ms | 166.20 ms | 12,895,874 (1.5 %) |
| aarch64 | ext4 | sustained | pinned | Aeron IPC | 2 | 1,398,435 | 1,398,435 | 0.43 | 2.56 | 80.61 ms | 159.65 ms | 11,349,375 (1.3 %) |
| aarch64 | ext4 | sustained-jvm-old | jvm-old | Chronicle Queue | 1 | 171,927 | 171,927 | 0.45 | 34.83 ms | 88.08 ms | 135.92 ms | 8,334,880 (7.9 %) |
| aarch64 | ext4 | sustained-jvm-old | jvm-old | Aeron IPC | 1 | 1,225,940 | 1,225,940 | 0.49 | 2.21 | 56.52 ms | 93.59 ms | 8,699,802 (1.2 %) |
| aarch64 | ext4 | sustained-others | pinned | yaque | 1 | 248,410 | 248,410 | 1.74 | 12.05 ms | 16.34 ms | 17.30 ms | 5,776,478 (3.8 %) |
| aarch64 | ext4 | sustained-others | pinned | queue-file | 1 | 154,889 | 154,870 | writer only | writer only | writer only | writer only | 8,395,311 (8.9 %) |
| aarch64 | ext4 | sustained-others | pinned | plain file | 1 | 171,198 | 171,198 | 1.48 | 7.32 | 87.43 ms | 138.54 ms | 1,347,724 (1.3 %) |
| aarch64 | ext4 | tuned-sustained | tuned | tickertape | 1 | 169,294 | 169,294 | 0.57 | 2.24 | 318 | 1.15 ms | 562,803 (0.5 %) |
| aarch64 | ext4 | tuned-sustained | tuned | Chronicle Queue | 1 | 171,886 | 171,885 | 0.52 | 35.75 ms | 98.57 ms | 143.79 ms | 7,838,169 (7.5 %) |
| aarch64 | ext4 | tuned-sustained | tuned | Aeron IPC | 1 | 1,394,548 | 1,394,548 | 0.47 | 1.15 ms | 91.36 ms | 172.75 ms | 10,590,173 (1.2 %) |
| aarch64 | xfs | sustained-journals | pinned | tickertape | 1 | 169,277 | 169,277 | 0.56 | 2.15 | 201 | 1.40 ms | 717,943 (0.7 %) |
| aarch64 | xfs | sustained-journals | pinned | Chronicle Queue | 1 | 170,348 | 170,347 | 0.50 | 4.21 | 11.1 | 6.34 ms | 2,036,095 (2.0 %) |

**SD, 64 B (paced at 50 % or 90 % of the probe, a sync every second)**

| Machine | FS | Part | Config | Tool | Round | Target (records/s) | Achieved (records/s) | p50 | p99 | p99.9 | max | Late sends | Sync p50 | Sync p99 | Sync max |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| x86-64 | ext4 | sustained | pinned | tickertape | 1 | 1,771,815 | 1,771,814 | 0.32 | 12.2 | 2.39 ms | 4.89 ms | 89,721,732 (8.3 %) | 2.95 ms | 3.87 ms | 4.21 ms |
| x86-64 | ext4 | sustained | pinned | tickertape | 2 | 1,764,927 | 1,764,926 | 0.32 | 15.8 | 1.83 ms | 3.76 ms | 91,871,125 (8.5 %) | 2.34 ms | 3.63 ms | 3.80 ms |
| x86-64 | ext4 | sustained | pinned | Chronicle Queue | 1 | 1,530,662 | 1,530,112 | 147.46 ms | 358.09 ms | 369.62 ms | 404.49 ms | 818,418,159 (87.7 %) | 361.50 ms | 382.73 ms | 404.49 ms |
| x86-64 | ext4 | sustained | pinned | Chronicle Queue | 2 | 1,476,421 | 1,476,421 | 125.70 ms | 344.98 ms | 352.06 ms | 413.93 ms | 735,524,866 (81.7 %) | 348.65 ms | 366.22 ms | 413.66 ms |
| x86-64 | ext4 | sustained-jvm-old | jvm-old | Chronicle Queue | 1 | 1,496,973 | 1,496,973 | 0.47 | 149.68 ms | 174.72 ms | 186.52 ms | 354,007,436 (38.8 %) | 102.56 ms | 184.55 ms | 186.38 ms |
| x86-64 | ext4 | tuned-sustained | tuned | tickertape | 1 | 1,193,977 | 1,193,977 | 0.33 | 2.64 | 1.50 ms | 3.75 ms | 20,766,590 (2.9 %) | 2.20 ms | 3.51 ms | 3.74 ms |
| x86-64 | ext4 | tuned-sustained | tuned | Chronicle Queue | 1 | 1,528,357 | 1,528,357 | 141.69 ms | 357.30 ms | 362.02 ms | 390.07 ms | 800,189,839 (85.8 %) | 362.02 ms | 367.26 ms | 390.07 ms |
| x86-64 | ext4 | sustained-90 | pinned | tickertape | 1 | 3,188,198 | 3,188,133 | 0.34 | 42.66 ms | 49.64 ms | 54.20 ms | 755,766,930 (38.9 %) | 44.11 ms | 53.02 ms | 54.20 ms |
| x86-64 | ext4 | sustained-90 | pinned | Chronicle Queue | 1 | 2,650,462 | **failed** | **failed twice: `IllegalStateException: reader timed out`** | | | | | | | |
| x86-64 | xfs | sustained-journals | pinned | tickertape | 1 | 1,759,845 | 1,759,845 | 0.32 | 18.2 | 1.21 ms | 2.75 ms | 87,240,112 (8.1 %) | 1.76 ms | 2.61 ms | 2.75 ms |
| x86-64 | xfs | sustained-journals | pinned | Chronicle Queue | 1 | 1,758,803 | 1,623,487 | 24.71 s | 50.33 s | 50.94 s | 51.07 s | 1,071,257,697 (99.8 %) | 381.16 ms | 603.98 ms | 614.47 ms |
| x86-64 | xfs | sustained-90 | pinned | tickertape | 1 | 3,189,160 | 3,189,073 | 0.37 | 156.37 ms | 175.37 ms | 184.03 ms | 872,992,555 (44.9 %) | 46.92 ms | 181.27 ms | 184.03 ms |
| x86-64 | xfs | sustained-90 | pinned | Chronicle Queue | 1 | 3,144,333 | **failed** | **failed twice: `IllegalStateException: reader timed out`** | | | | | | | |
| aarch64 | ext4 | sustained | pinned | tickertape | 1 | 1,729,024 | 1,729,024 | 0.32 | 2.55 | 989 | 2.37 ms | 17,679,552 (1.7 %) | 1.38 ms | 2.18 ms | 2.36 ms |
| aarch64 | ext4 | sustained | pinned | tickertape | 2 | 1,733,310 | 1,733,310 | 0.31 | 5.09 | 1.80 ms | 42.01 ms | 19,619,039 (1.9 %) | 1.49 ms | 14.75 ms | 41.94 ms |
| aarch64 | ext4 | sustained | pinned | Chronicle Queue | 1 | 1,285,285 | 1,285,285 | 56.72 ms | 298.06 ms | 302.78 ms | 333.19 ms | 509,395,326 (65.0 %) | 302.78 ms | 307.23 ms | 325.84 ms |
| aarch64 | ext4 | sustained | pinned | Chronicle Queue | 2 | 1,287,628 | 1,287,628 | 55.51 ms | 301.20 ms | 381.68 ms | 540.02 ms | 507,528,815 (64.6 %) | 303.82 ms | 366.74 ms | 540.02 ms |
| aarch64 | ext4 | sustained-jvm-old | jvm-old | Chronicle Queue | 1 | 1,320,765 | 1,320,762 | 0.45 | 149.16 ms | 177.08 ms | 189.27 ms | 262,885,641 (32.6 %) | 97.98 ms | 187.17 ms | 188.87 ms |
| aarch64 | ext4 | tuned-sustained | tuned | tickertape | 1 | 1,728,550 | 1,728,550 | 0.31 | 12.5 | 2.36 ms | 98.76 ms | 18,070,009 (1.7 %) | 1.76 ms | 15.40 ms | 98.76 ms |
| aarch64 | ext4 | tuned-sustained | tuned | Chronicle Queue | 1 | 1,313,857 | 1,313,857 | 88.34 ms | 563.61 ms | 741.87 ms | 889.72 ms | 580,247,559 (72.4 %) | 310.12 ms | 579.86 ms | 638.06 ms |
| aarch64 | ext4 | sustained-90 | pinned | tickertape | 1 | 3,116,450 | 3,116,450 | 0.32 | 114.03 ms | 267.12 ms | 326.89 ms | 161,699,921 (8.5 %) | 2.86 ms | 210.50 ms | 326.89 ms |
| aarch64 | ext4 | sustained-90 | pinned | Chronicle Queue | 1 | 2,323,911 | **failed** | **failed twice: `IllegalStateException: reader timed out`** | | | | | | | |
| aarch64 | xfs | sustained-journals | pinned | tickertape | 1 | 1,732,727 | 1,732,727 | 0.32 | 2.77 | 1.02 ms | 2.38 ms | 17,365,838 (1.6 %) | 1.39 ms | 2.27 ms | 2.37 ms |
| aarch64 | xfs | sustained-journals | pinned | Chronicle Queue | 1 | 1,386,373 | 1,386,373 | 134.87 ms | 396.89 ms | 566.76 ms | 692.58 ms | 697,557,997 (82.5 %) | 352.32 ms | 468.98 ms | 573.05 ms |
| aarch64 | xfs | sustained-90 | pinned | tickertape | 1 | 3,117,848 | 3,117,848 | 0.33 | 939.52 ms | 1.17 s | 1.26 s | 400,830,384 (21.1 %) | 4.75 ms | 418.12 ms | 784.86 ms |
| aarch64 | xfs | sustained-90 | pinned | Chronicle Queue | 1 | 2,701,934 | **failed** | **failed twice: `IllegalStateException: reader timed out`** | | | | | | | |

**SD, 1,024 B**

| Machine | FS | Part | Config | Tool | Round | Target (records/s) | Achieved (records/s) | p50 | p99 | p99.9 | max | Late sends | Sync p50 | Sync p99 | Sync max |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| x86-64 | ext4 | sustained | pinned | tickertape | 1 | 173,190 | 173,190 | 0.78 | 3.93 | 2.11 ms | 4.12 ms | 2,526,439 (2.4 %) | 2.99 ms | 3.85 ms | 4.11 ms |
| x86-64 | ext4 | sustained | pinned | tickertape | 2 | 172,766 | 172,766 | 0.79 | 3.67 | 1.55 ms | 3.82 ms | 2,446,300 (2.3 %) | 2.29 ms | 3.63 ms | 3.81 ms |
| x86-64 | ext4 | sustained | pinned | Chronicle Queue | 1 | 175,186 | 175,186 | 34.83 ms | 505.94 ms | 515.64 ms | 592.97 ms | 60,368,500 (56.5 %) | 516.16 ms | 527.43 ms | 592.97 ms |
| x86-64 | ext4 | sustained | pinned | Chronicle Queue | 2 | 174,615 | 174,615 | 36.41 ms | 504.10 ms | 513.80 ms | 528.22 ms | 60,249,930 (56.6 %) | 514.33 ms | 516.42 ms | 527.96 ms |
| x86-64 | ext4 | sustained-jvm-old | jvm-old | Chronicle Queue | 1 | 175,124 | 175,124 | 0.56 | 133.37 ms | 168.95 ms | 186.38 ms | 23,062,100 (21.6 %) | 119.87 ms | 184.29 ms | 186.38 ms |
| x86-64 | ext4 | tuned-sustained | tuned | tickertape | 1 | 169,748 | 169,748 | 0.78 | 3.35 | 1.54 ms | 4.10 ms | 2,893,947 (2.8 %) | 2.25 ms | 3.85 ms | 4.09 ms |
| x86-64 | ext4 | tuned-sustained | tuned | Chronicle Queue | 1 | 173,582 | 173,582 | 30.06 ms | 501.74 ms | 511.71 ms | 604.50 ms | 59,042,308 (55.8 %) | 511.44 ms | 516.42 ms | 604.50 ms |
| x86-64 | ext4 | sustained-90 | pinned | tickertape | 1 | 311,208 | 311,208 | 0.79 | 4.43 | 2.13 ms | 4.71 ms | 6,972,392 (3.7 %) | 2.78 ms | 4.11 ms | 4.70 ms |
| x86-64 | ext4 | sustained-90 | pinned | Chronicle Queue | 1 | 315,324 | 307,301 | 10.58 s | 18.20 s | 18.71 s | 18.84 s | 191,305,437 (99.5 %) | 1.34 s | 5.16 s | 6.34 s |
| x86-64 | xfs | sustained-journals | pinned | tickertape | 1 | 173,262 | 173,262 | 0.78 | 3.38 | 1.17 ms | 2.86 ms | 2,278,220 (2.2 %) | 1.98 ms | 2.65 ms | 2.86 ms |
| x86-64 | xfs | sustained-journals | pinned | Chronicle Queue | 1 | 175,464 | 175,464 | 39.42 ms | 512.23 ms | 523.50 ms | 790.63 ms | 60,006,370 (56.1 %) | 521.67 ms | 524.55 ms | 730.86 ms |
| x86-64 | xfs | sustained-90 | pinned | tickertape | 1 | 311,174 | 311,174 | 0.78 | 3.98 | 1.66 ms | 3.45 ms | 6,430,685 (3.4 %) | 2.40 ms | 3.25 ms | 3.45 ms |
| x86-64 | xfs | sustained-90 | pinned | Chronicle Queue | 1 | 315,910 | 308,483 | 8.17 s | 15.89 s | 16.65 s | 16.93 s | 191,220,735 (99.2 %) | 1.82 s | 4.94 s | 5.37 s |
| aarch64 | ext4 | sustained | pinned | tickertape | 1 | 169,089 | 169,089 | 0.56 | 2.62 | 837 | 2.52 ms | 857,749 (0.8 %) | 1.39 ms | 2.21 ms | 2.39 ms |
| aarch64 | ext4 | sustained | pinned | tickertape | 2 | 168,712 | 168,712 | 0.56 | 2.73 | 1.60 ms | 76.55 ms | 915,175 (0.9 %) | 1.40 ms | 25.44 ms | 76.55 ms |
| aarch64 | ext4 | sustained | pinned | Chronicle Queue | 1 | 171,867 | 171,867 | 21.92 ms | 500.17 ms | 621.28 ms | 845.15 ms | 56,911,350 (54.3 %) | 506.99 ms | 611.32 ms | 845.15 ms |
| aarch64 | ext4 | sustained | pinned | Chronicle Queue | 2 | 170,648 | 170,648 | 19.91 ms | 492.31 ms | 502.27 ms | 514.33 ms | 56,265,494 (54.1 %) | 502.79 ms | 504.37 ms | 514.33 ms |
| aarch64 | ext4 | sustained-jvm-old | jvm-old | Chronicle Queue | 1 | 171,927 | 171,927 | 0.47 | 133.76 ms | 170.92 ms | 189.53 ms | 22,452,728 (21.4 %) | 115.87 ms | 187.17 ms | 188.74 ms |
| aarch64 | ext4 | tuned-sustained | tuned | tickertape | 1 | 169,294 | 169,294 | 0.56 | 2.39 | 1.13 ms | 15.48 ms | 681,433 (0.7 %) | 1.67 ms | 8.90 ms | 15.47 ms |
| aarch64 | ext4 | tuned-sustained | tuned | Chronicle Queue | 1 | 171,886 | 171,886 | 18.97 ms | 496.76 ms | 506.46 ms | 732.43 ms | 56,325,652 (53.7 %) | 506.46 ms | 509.08 ms | 731.91 ms |
| aarch64 | ext4 | sustained-90 | pinned | tickertape | 1 | 305,024 | 305,024 | 0.57 | 73.99 ms | 123.40 ms | 141.43 ms | 12,570,694 (6.8 %) | 36.21 ms | 138.02 ms | 141.43 ms |
| aarch64 | ext4 | sustained-90 | pinned | Chronicle Queue | 1 | 309,364 | 309,050 | 1.05 s | 2.64 s | 3.07 s | 3.28 s | 175,511,729 (93.0 %) | 2.08 s | 3.06 s | 3.28 s |
| aarch64 | xfs | sustained-journals | pinned | tickertape | 1 | 169,277 | 169,277 | 0.56 | 2.36 | 783 | 2.44 ms | 742,239 (0.7 %) | 1.38 ms | 2.25 ms | 2.43 ms |
| aarch64 | xfs | sustained-journals | pinned | Chronicle Queue | 1 | 170,348 | 170,348 | 61.70 ms | 552.08 ms | 755.50 ms | 1.12 s | 59,887,739 (57.6 %) | 556.79 ms | 664.80 ms | 1.12 s |
| aarch64 | xfs | sustained-90 | pinned | tickertape | 1 | 304,583 | 304,583 | 0.57 | 786.96 ms | 1.39 s | 1.69 s | 27,551,724 (14.8 %) | 100.66 ms | 1.19 s | 1.69 s |
| aarch64 | xfs | sustained-90 | pinned | Chronicle Queue | 1 | 307,434 | 306,863 | 1.14 s | 2.92 s | 3.40 s | 3.61 s | 172,323,325 (91.9 %) | 2.13 s | 3.29 s | 3.52 s |

- **Probes** (the one comparison at the same question). At 64 B tickertape's unpaced rate was above Chronicle Queue's on
  ext4 (pinned: 3,529,856–3,543,632 against 2,952,843–3,061,325 on x86-64, 3,458,048–3,466,622 against
  2,570,572–2,575,258 on aarch64) and on aarch64 xfs (3,464,276–3,465,454 against 2,772,747–3,002,149), level on x86-64
  xfs (3,519,690–3,543,512 against 3,493,704–3,517,607), and below it on the tuned x86-64 boot (2,387,955 against
  3,056,715). In the x86-64 64 B ext4 probes Chronicle's reader p99.9 was lower (6.50–6.53 against 8.34–8.63 µs).
  tickertape's probe reader maximum was above Chronicle's in 18 of the 20 pinned probe pairs (x86-64 64 B ext4:
  312.74–355.47 against 143.65–143.79 ms; x86-64 64 B xfs: 25.82–33.91 against 4.66–4.75 ms; the exceptions: x86-64 64 B
  in `sustained-90` ext4 and aarch64 1,024 B in round 2), while its probe p99.9 at 1,024 B was lower in all ten 1,024 B
  pairs (x86-64 ext4: 28.9–29.1 against 33.5–33.9 µs). Aeron IPC (pinned, ext4) was above both on x86-64
  (6,489,247–7,206,880); on aarch64 it was below tickertape in round 1 (3,246,052 against 3,458,048) and above in round
  2 (3,820,950 against 3,466,622). At 1,024 B Chronicle's rate was above tickertape's in all ten pinned pairs of probes
  (both machines, ext4 and xfs, every part), by 0.6–1.6 %, and the plain file's single probes too (348,781 on x86-64,
  342,397 on aarch64). yaque's 1,024 B probes (685,628 and 496,821) stopped at the record cap after 327 and 451 s, not
  at the ten minutes. At 1,024 B the probe measures the disk, not the tool: tickertape and Chronicle reached
  337,424–351,012 records/s in every part (345.5–359.4 MB/s of payload), and 337,424–350,374 in the pinned ext4 rounds
  (345.5–358.8 MB/s), against a device that writes at most 351 MB/s by itself in raw `fio` jobs; payload accepted is not
  bytes written to the device, and an unpaced run holds gigabytes of pages not yet written back in the page cache (the
  disk section). Aeron (pinned, ext4) reached 2,776,684–4,175,027 at 1,024 B, eight to twelve times as many: it writes
  no durable journal, though its file-backed term buffers are written back to the same disk.
- **SV at half of each tool's own rate, `pinned`, ext4** (targets: tickertape 1,729,024–1,771,815 records/s at 64 B and
  168,712–173,190 at 1,024 B; Chronicle 1,285,285–1,530,662 and 170,648–175,186; Aeron 1,623,026–3,603,440 and
  1,388,341–2,087,513). tickertape's reader p99.9 was 323–697 µs and its maximum 1.14–23.81 ms (the 23.81 ms in aarch64
  round 2); Chronicle Queue's p99.9 89.98–102.69 ms (maxima 133.37–162.92 ms; 88.08–98.63 ms with the old settings) and
  Aeron's 14.72–107.41 ms (maxima 78.38–167.64 ms; at 64 B 4.66–6.32 µs with the old settings; cause not traced). The
  plain file, which like Chronicle has no early write-back, showed a tail of the same size at 1,024 B (87.43–96.93 ms,
  at 174,390 and 171,198 records/s; 16.69–18.32 ms at 64 B), and on xfs Chronicle's tail was 10.6–25.1 µs in three of
  four cells (next item). That points at ext4 write-back as seen by tools without early write-back rather than at
  Chronicle's reader, as an observation; it was not traced.
- **SV on xfs** (`sustained-journals`, n = 1): **Chronicle Queue's reader p99.9 was lower than tickertape's in three of
  the four cells** (x86-64 1,024 B: 25.1 against 284 µs; aarch64 64 B: 10.6 against 382 µs; aarch64 1,024 B: 11.1
  against 201 µs) and higher in one (x86-64 64 B: 33.31 ms against 432 µs, at nearly the same targets, 1,758,803 and
  1,759,845 records/s). tickertape's maxima were lower in all four (1.19–2.47 ms against 4.84–38.04 ms).
- **SD at half of each tool's own rate** (`pinned`, ext4 and xfs; the same targets as SV). A tickertape checkpoint took
  1.38–2.99 ms (median; maxima 2.36–76.55 ms per run), its reader's p99.9 783 µs to 2.39 ms; Chronicle Queue's
  synchronous store flush took 302.78–556.79 ms (median; maxima 325.84 ms to 1.12 s), its reader's p99.9 302.78–755.50
  ms, except on x86-64 xfs at 64 B, where Chronicle did not keep its schedule (1,623,487 of 1,758,803 records/s; reader
  p99.9 50.94 s). This is not the same work. tickertape's early write-back starts writing each MiB as soon as the writer
  has passed it, so when the checkpoint starts most of the second's data is already on its way to the device;
  Chronicle's `msync` appears to write the second's data then, on the writer's thread (`bgMicroTouch()`, a background
  `msync`, was not used): one second of Chronicle's 1,024 B records at its 175,186 records/s target is at least 182 MB
  (payload and 16 B envelope), 0.52 s at 351 MB/s, consistent with its flush medians at 1,024 B on ext4 of 502.79–516.16
  ms; not traced. With its old settings, at nearly the same rate (175,124 records/s on x86-64), the flush took
  97.98–119.87 ms, which this does not explain. A tickertape checkpoint without early write-back became durable 0.93 s
  after its request at about 90 % of the device's rate in later tickertape-only runs (the disk section). These machine
  types' local SSDs report a write-through cache in the other sessions (the rerun's environment files do not record it),
  and if that held here, no sync paid for a drive-cache flush.
- **SD at 90 % of the probe.** tickertape stalled at 64 B on both machines (reader p99.9 49.64 ms on x86-64 ext4, 175.37
  ms on x86-64 xfs, 267.12 ms on aarch64 ext4, 1.17 s on aarch64 xfs; on x86-64 38.9 % and 44.9 % of the sends were
  late) and at 1,024 B on aarch64 (123.40 ms on ext4, 1.39 s on xfs), not at 1,024 B on x86-64 (2.13 ms and 1.66 ms).
  Reader p99.9 is the delay from scheduled send to read; it does not say where the delay arose. Its checkpoints kept the
  one-second grid in five of six runs (610 each); on aarch64 xfs at 1,024 B, 601 ran and 9 grid points were missed
  (checkpoint median 100.66 ms, max 1.69 s). Chronicle Queue's 64 B cell failed (below); at 1,024 B it did not hold its
  schedule either: 307,301 of 315,324 records/s on x86-64 ext4 (99.5 % of sends late, 293 flushes, 332 missed grid
  points), 308,483 of 315,910 on x86-64 xfs, 309,050 of 309,364 on aarch64 ext4 and 306,863 of 307,434 on aarch64 xfs;
  its reader's p99.9 was 3.07–18.71 s. The earlier sessions' 31 s backlog of tickertape on x86-64 xfs did not recur
  (maximum 184.03 ms).
- **Old settings in the sustained mode** (`sustained-jvm-old`, hours after `sustained`, single runs at each tool's own
  old-settings rate). Both JVM tools' paced figures were worse with the new settings than in these later runs with the
  old ones: Chronicle Queue's SD flush median 302.78–516.16 ms on ext4 against 97.98–119.87 ms, its SD reader p99.9
  302.78–621.28 ms against 168.95–177.08 ms, and Aeron's 64 B SV reader p99.9 14.72–36.24 ms against 4.66–6.32 µs (6.32
  on x86-64, 4.66 on aarch64). The two configurations differ in five settings and in JVM logging, and the runs differ in
  time and rate; which difference does it was not measured (the plan has no single-setting sustained rows). The probes,
  the rows the plan compares, moved less: Chronicle Queue's old-settings probes were within its two new-settings probes
  or up to 2.6 % above their upper end (aarch64 64 B: 2,641,531 against 2,570,572–2,575,258), Aeron's within them except
  at 1,024 B on aarch64 (2,451,881 against 2,776,684–2,796,872).

### Chronicle Queue and Aeron under the old and the new settings

Derived from the short-mode tables above (ext4, n = 4 each). `jvm-old` is "old settings, new harness", not the old
harness.

**Chronicle Queue**

| Workload | Payload, rate | x86-64: old settings (`jvm-old`) | `microtouch-only` | new settings (`pinned`) | aarch64: `jvm-old` | `microtouch-only` | `pinned` |
|---|---|---|---|---|---|---|---|
| W1-cold, million records/s | 64 B | 6.73–7.36 | 6.83–7.37 | 8.47–8.53 | 4.66–4.78 | 4.63–4.76 | 5.49–5.53 |
| W1-cold, million records/s | 1,024 B | 2.19–2.38 | 2.06–2.37 | 2.82–2.87 | 1.70–1.94 | 1.72–1.79 | 2.76–2.83 |
| W1-steady, million records/s | 64 B | 7.10–7.49 | 7.26–7.38 | 8.54–8.69 | 4.79–4.85 | 4.66–4.81 | 5.57–5.60 |
| W1-steady, million records/s | 1,024 B | 2.28–2.32 | 2.20–2.34 | 2.93–3.04 | 1.86–2.04 | 1.92–2.14 | 2.97–3.07 |
| W3, million records/s | 64 B | 30.15–31.88 | 28.72–32.75 | 35.57–36.77 | 19.44–24.95 | 19.68–23.94 | 28.17–29.30 |
| W3, million records/s | 1,024 B | 6.52–8.53 | 5.88–7.32 | 10.28–10.59 | 4.02–6.61 | 4.41–7.55 | 15.31–17.12 |
| W2 reader p99.9, µs | 64 B, 10,000/s | 2.45–3.51 | 1.90–1.99 | 1.68–1.83 | 2.13–2.24 | 1.59–3.89 | 0.73–0.78 |
| W2 reader p99.9, µs | 64 B, 100,000/s | 28.6–29.2 | 17.8–18.2 | 11.8–15.3 | 25.1–27.1 | 12.7–18.1 | 7.85–13.3 |
| W2 reader p99.9, µs | 1,024 B, 10,000/s | 34.8–36.8 | 2.35–2.51 | 2.36 µs to 15.75 ms | 36.4–37.6 | 2.71–2.94 | 2.77–3.46 |
| W2 reader p99.9, µs | 1,024 B, 100,000/s | 35.4–37.4 | 25.2–26.2 | 23.9–24.6 | 33.7–35.2 | 18.7–29.2 | 14.5–16.3 |

**Aeron IPC, W2 reader p99.9 (µs)**

| Payload, rate | x86-64: old settings (`jvm-old`) | new settings (`pinned`) | aarch64: `jvm-old` | `pinned` |
|---|---|---|---|---|
| 64 B, 10,000/s | 3.67–4.36 | 2.56–5.65 | 6.57–7.67 | 5.56–7.34 |
| 64 B, 100,000/s | 0.83–4.99 | 0.39–3.47 | 3.07–5.92 | 0.62–3.03 |
| 1,024 B, 10,000/s | 3.13–7.72 | 3.26–3.64 | 4.80–6.00 | 4.57–5.96 |
| 1,024 B, 100,000/s | 0.77–4.96 | 0.77–5.58 | 0.81–2.90 | 0.82–2.71 |

- **The reader of the earlier comparison was right that its settings handicapped Chronicle Queue.** With the new
  settings Chronicle appended faster in every W1 cell (W1-cold 1,024 B on aarch64: 2.76–2.83 against 1.70–1.94),
  replayed faster in every W3 cell (1,024 B on aarch64: 15.31–17.12 against 4.02–6.61), and its reader's p99.9 was lower
  in every W2 run but one, most at 1,024 B × 10,000 records/s (34.8–36.8 µs before, 2.36–2.82 µs in three of four runs
  on x86-64 after; the fourth had 15.75 ms). `microTouch()` alone brought none of the W1 and W3 gain (it only runs while
  the writer waits) and most of the W2 gain at 1,024 B × 10,000 records/s on both machines and at 64 B × 10,000
  records/s on x86-64 (on aarch64 its runs spread from 1.59 to 3.89 µs); the rest of the new settings (the long warm-up,
  the 1 GiB `blockSize`, the JVM logs) brought the rest, and the rows do not separate them.
- **Cells that changed sides** against tickertape's zero-copy reader (W2 p99.9, µs; tickertape's row is the same in both
  comparisons): x86-64 64 B × 10,000/s, tickertape 2.17–2.35, Chronicle 2.45–3.51 old and 1.68–1.83 new; x86-64 1,024 B
  × 10,000/s, 4.60–6.84 against 34.8–36.8 old and 2.36–2.82 new in three of four runs; x86-64 1,024 B × 100,000/s,
  28.6–29.8 against 35.4–37.4 and 23.9–24.6; aarch64 1,024 B × 10,000/s, 8.73–9.10 against 36.4–37.6 and 2.77–3.46;
  aarch64 1,024 B × 100,000/s, 24.8–28.5 against 33.7–35.2 and 14.5–16.3. Under the old settings tickertape was lower in
  all five; under the new ones Chronicle is (at x86-64 1,024 B × 10,000/s in three of its four runs).
- Aeron's W2 p99.9 did not change beyond the spread of its runs.

### The failed cell

Chronicle Queue's SD run at 90 % of its probe with 64 B records failed: in each of the four machine and file-system
combinations both attempts failed (eight attempts; the unit was run once more after its part, as the plan says). The
error, from `children.log`, the same in all eight:

```text
Exception in thread "main" java.lang.IllegalStateException: reader timed out
	at compare.Harness$Poller.poll(Harness.java:205)
	at compare.Harness.run(Harness.java:393)
	at compare.Main.main(Main.java:64)
```

This is the harness's reader reaching its deadline (`Harness.TIMEOUT`, 120 s, line 35; the deadline is set at the start
barrier to that plus the schedule's length, line 390) before it had seen every scheduled record, so the backlog at the
end exceeded about two minutes. `children.log` has no other error (the writer's result is empty because the run was
stopped); Chronicle Queue did not report a failure, and the cause was not established. No latency figure exists for the
cell. The rates it would have run at are in the SD table (2,323,911–3,144,333 records/s). The other runs of those units
completed and passed validation and are in the tables (the probes and SD at 1,024 B). This failure is what happened to
Chronicle Queue in that cell; it says nothing about tickertape, whose run at its own 90 % rate is in the same table.

### Probe drift between parts

The pinned probes of one tool and file system, in the order they ran (records/s; later probe ÷ first probe):

| Machine | FS | Tool | Payload | Probes in order | Ratios |
|---|---|---|---|---|---|
| x86-64 | ext4 | tickertape | 64 B | `sustained` r1 3,543,632; r2 3,529,856; `sustained-90` 3,542,443 | 0.996, 1.000 |
| x86-64 | ext4 | tickertape | 1,024 B | 346,382; 345,534; 345,787 | 0.998, 0.998 |
| x86-64 | ext4 | Chronicle Queue | 64 B | 3,061,325; 2,952,843; 2,944,959 | 0.965, 0.962 |
| x86-64 | ext4 | Chronicle Queue | 1,024 B | 350,374; 349,231; 350,361 | 0.997, 1.000 |
| x86-64 | ext4 | Aeron IPC | 64 B | 7,206,880; 6,489,247 | 0.900 |
| x86-64 | ext4 | Aeron IPC | 1,024 B | 4,175,027; 4,036,635 | 0.967 |
| x86-64 | xfs | tickertape | 64 B | `sustained-journals` 3,519,690; `sustained-90` 3,543,512 | 1.007 |
| x86-64 | xfs | tickertape | 1,024 B | 346,525; 345,749 | 0.998 |
| x86-64 | xfs | Chronicle Queue | 64 B | 3,517,607; 3,493,704 | 0.993 |
| x86-64 | xfs | Chronicle Queue | 1,024 B | 350,929; 351,012 | 1.000 |
| aarch64 | ext4 | tickertape | 64 B | 3,458,048; 3,466,622; 3,462,723 | 1.002, 1.001 |
| aarch64 | ext4 | tickertape | 1,024 B | 338,179; 337,424; 338,916 | 0.998, 1.002 |
| aarch64 | ext4 | Chronicle Queue | 64 B | 2,570,572; 2,575,258; 2,582,124 | 1.002, 1.004 |
| aarch64 | ext4 | Chronicle Queue | 1,024 B | 343,736; 341,297; 343,739 | 0.993, 1.000 |
| aarch64 | ext4 | Aeron IPC | 64 B | 3,246,052; 3,820,950 | 1.177 |
| aarch64 | ext4 | Aeron IPC | 1,024 B | 2,776,684; 2,796,872 | 1.007 |
| aarch64 | xfs | tickertape | 64 B | 3,465,454; 3,464,276 | 1.000 |
| aarch64 | xfs | tickertape | 1,024 B | 338,555; 338,426 | 1.000 |
| aarch64 | xfs | Chronicle Queue | 64 B | 2,772,747; 3,002,149 | 1.083 |
| aarch64 | xfs | Chronicle Queue | 1,024 B | 340,696; 341,594 | 1.003 |

- **Aeron's two rounds at 64 B differ at or beyond the playbook's 10 % threshold**: round 2 was 0.900 times round 1 on
  x86-64 and 1.177 times on aarch64. Its SV and SD rows of the two rounds are therefore paced at rates that differ by as
  much (3,603,440 and 3,244,623 records/s on x86-64; 1,623,026 and 1,910,474 on aarch64).
- Chronicle Queue's aarch64 xfs probe at 64 B rose by 8.3 % from the first part to the second; its x86-64 ext4 probe at
  64 B fell by 3.5–3.8 %. tickertape's probes moved by at most 0.7 %.
- The xfs probes of tickertape (its writes to a fresh file system first) and Chronicle (on one already written) do not
  show an order effect beyond these.

### Interrupts on the tuned CPUs

`/proc/interrupts` was captured five times during the tuned parts on each machine (x86-64 at 10:50, 11:00, 11:30, 12:30
and 13:30 UTC; aarch64 at 11:00, 11:10, 11:40, 12:40 and 13:40), with `/proc/irq/*/effective_affinity_list`. The
captures do not cover the last 70 minutes of the tuned sustained part (Aeron), and an interval covers several units, so
the counts cannot be attributed to a tool. Interrupts on CPU 1 (the writer's) between captures:

| Interval (UTC) | x86-64: TLB shootdowns | function calls | IRQ work | local timer | network queues (gVNIC) |
|---|---|---|---|---|---|
| 10:50–11:00 | 1,942,342 | 1,943,631 | 75 | 41 | 129 |
| 11:00–11:30 | 1,442,594 | 1,443,393 | 285 | 184 | 251 |
| 11:30–12:30 | 26,207,269 | 26,227,506 | 54,452 | 588 | 292 |
| 12:30–13:30 | 74,360,910 | 74,481,299 | 62,956 | 790 | 295 |

| Interval (UTC) | aarch64: function calls (IPI) | IRQ work | arch timer | NVMe queue `nvme0q2` (the data disk) | network queues (gVNIC) |
|---|---|---|---|---|---|
| 11:00–11:10 | 2,898 | 115 | 76 | 1,322 | 37 |
| 11:10–11:40 | 536 | 275 | 179 | 0 | 3,592 |
| 11:40–12:40 | 175,471 | 49,566 | 47,725 | 29,482 | 81 |
| 12:40–13:40 | 267,328 | 83,525 | 82,554 | 449,779 | 858 |

At the last capture eight interrupt lines on x86-64 (four gVNIC notification blocks each on CPU 1 and CPU 2) and nine on
aarch64 (four gVNIC blocks and five NVMe queues of the two data disks) had an effective affinity of CPU 1 or 2:
`irqaffinity` only sets the default, and managed and driver-placed lines stay where they are. The isolation itself held
(the measured threads alone on their CPUs, read back from `/proc` in every result), but **the tuned CPUs were not
interrupt-free**. These are observations over capture intervals that contain several units; they do not isolate one
tool, and their source was not traced. The kernel sends TLB shootdowns to the CPUs that run a process whose mappings
change, so they may be the tool's own: the x86-64 interval 10:50–11:00 lies inside tickertape's tuned short unit
(10:47–11:03) and brought 1,942,342 TLB shootdowns to CPU 1, against 1,442,594 in the next 30 minutes (the end of that
unit, then mostly Chronicle Queue and Aeron); the intervals with 26–74 million (11:30–13:30) contain tickertape's tuned
sustained unit (11:56–12:57) and the first hour of Chronicle's (from 12:57). On aarch64 the first capture (11:00) came
after most of tickertape's tuned short unit (10:49–11:06). The `tuned` rows are an isolated boot with these interrupts,
not the upper bound of the machine.

### Sample spread

Every `pinned` short-mode cell whose largest sample is more than 10 % above its smallest (the playbook's threshold; W1
and W3 throughput, W2 p99.9):

| Machine | FS | Tool | Cell | Range | max ÷ min |
|---|---|---|---|---|---|
| x86-64 | ext4 | tickertape | W1-cold, 64 B | 19.11–30.84 | 1.61 |
| x86-64 | ext4 | tickertape | W1-cold, 1,024 B | 2.61–3.08 | 1.18 |
| x86-64 | ext4 | tickertape | W1-steady, 64 B | 19.44–24.93 | 1.28 |
| x86-64 | ext4 | plain file | W1-steady, 1,024 B | 1.51–1.76 | 1.16 |
| x86-64 | ext4 | tickertape (zero-copy) | W3, 64 B | 20.85–26.41 | 1.27 |
| x86-64 | ext4 | Aeron IPC | W2, 64 B, 10,000/s, p99.9 | 2.56–5.65 | 2.20 |
| x86-64 | ext4 | Aeron IPC | W2, 64 B, 100,000/s, p99.9 | 0.39–3.47 | 8.91 |
| x86-64 | ext4 | Chronicle Queue | W2, 64 B, 100,000/s, p99.9 | 11.8–15.3 | 1.30 |
| x86-64 | ext4 | plain file | W2, 64 B, 100,000/s, p99.9 | 5.10–7.08 | 1.39 |
| x86-64 | ext4 | tickertape (zero-copy) | W2, 64 B, 100,000/s, p99.9 | 2.27–5.96 | 2.62 |
| x86-64 | ext4 | tickertape (safe) | W2, 64 B, 100,000/s, p99.9 | 3.83–8.35 | 2.18 |
| x86-64 | ext4 | yaque | W2, 64 B, 100,000/s, p99.9 | 5.76–7.92 | 1.37 |
| x86-64 | ext4 | Aeron IPC | W2, 1,024 B, 10,000/s, p99.9 | 3.26–3.64 | 1.12 |
| x86-64 | ext4 | Chronicle Queue | W2, 1,024 B, 10,000/s, p99.9 | 2.36 µs to 15.75 ms | 6677.92 |
| x86-64 | ext4 | tickertape (zero-copy) | W2, 1,024 B, 10,000/s, p99.9 | 4.60–6.84 | 1.49 |
| x86-64 | ext4 | tickertape (safe) | W2, 1,024 B, 10,000/s, p99.9 | 6.94–7.64 | 1.10 |
| x86-64 | ext4 | yaque | W2, 1,024 B, 10,000/s, p99.9 | 144–888 | 6.18 |
| x86-64 | ext4 | Aeron IPC | W2, 1,024 B, 100,000/s, p99.9 | 0.77–5.58 | 7.29 |
| x86-64 | ext4 | plain file | W2, 1,024 B, 100,000/s, p99.9 | 9.85–11.3 | 1.14 |
| x86-64 | xfs | tickertape | W1-cold, 64 B | 19.30–29.61 | 1.53 |
| x86-64 | xfs | tickertape | W1-steady, 64 B | 20.01–29.30 | 1.46 |
| x86-64 | xfs | plain file | W3, 1,024 B | 3.22–3.59 | 1.11 |
| x86-64 | xfs | Chronicle Queue | W2, 64 B, 10,000/s, p99.9 | 1.74–1.98 | 1.13 |
| x86-64 | xfs | Aeron IPC | W2, 64 B, 100,000/s, p99.9 | 0.39–3.62 | 9.24 |
| x86-64 | xfs | Chronicle Queue | W2, 64 B, 100,000/s, p99.9 | 4.11–6.00 | 1.46 |
| x86-64 | xfs | tickertape (zero-copy) | W2, 64 B, 100,000/s, p99.9 | 2.21–6.62 | 3.00 |
| x86-64 | xfs | tickertape (safe) | W2, 64 B, 100,000/s, p99.9 | 5.26–8.31 | 1.58 |
| x86-64 | xfs | tickertape (zero-copy) | W2, 1,024 B, 10,000/s, p99.9 | 4.42–10.9 | 2.47 |
| x86-64 | xfs | Aeron IPC | W2, 1,024 B, 100,000/s, p99.9 | 1.33–3.33 | 2.50 |
| aarch64 | ext4 | tickertape (zero-copy) | W3, 64 B | 27.22–31.59 | 1.16 |
| aarch64 | ext4 | Chronicle Queue | W3, 1,024 B | 15.31–17.12 | 1.12 |
| aarch64 | ext4 | yaque | W3, 1,024 B | 3.94–4.41 | 1.12 |
| aarch64 | ext4 | Aeron IPC | W2, 64 B, 10,000/s, p99.9 | 5.56–7.34 | 1.32 |
| aarch64 | ext4 | tickertape (zero-copy) | W2, 64 B, 10,000/s, p99.9 | 2.87–3.46 | 1.20 |
| aarch64 | ext4 | Aeron IPC | W2, 64 B, 100,000/s, p99.9 | 0.62–3.03 | 4.89 |
| aarch64 | ext4 | Chronicle Queue | W2, 64 B, 100,000/s, p99.9 | 7.85–13.3 | 1.69 |
| aarch64 | ext4 | tickertape (zero-copy) | W2, 64 B, 100,000/s, p99.9 | 1.88–2.08 | 1.10 |
| aarch64 | ext4 | tickertape (safe) | W2, 64 B, 100,000/s, p99.9 | 4.95–6.07 | 1.23 |
| aarch64 | ext4 | Aeron IPC | W2, 1,024 B, 10,000/s, p99.9 | 4.57–5.96 | 1.30 |
| aarch64 | ext4 | Chronicle Queue | W2, 1,024 B, 10,000/s, p99.9 | 2.77–3.46 | 1.25 |
| aarch64 | ext4 | Aeron IPC | W2, 1,024 B, 100,000/s, p99.9 | 0.82–2.71 | 3.30 |
| aarch64 | ext4 | Chronicle Queue | W2, 1,024 B, 100,000/s, p99.9 | 14.5–16.3 | 1.12 |
| aarch64 | ext4 | tickertape (zero-copy) | W2, 1,024 B, 100,000/s, p99.9 | 24.8–28.5 | 1.15 |
| aarch64 | ext4 | tickertape (safe) | W2, 1,024 B, 100,000/s, p99.9 | 24.3–31.1 | 1.28 |
| aarch64 | xfs | Aeron IPC | W2, 64 B, 10,000/s, p99.9 | 5.83–6.58 | 1.13 |
| aarch64 | xfs | plain file | W2, 64 B, 10,000/s, p99.9 | 8.81–10.3 | 1.17 |
| aarch64 | xfs | Aeron IPC | W2, 64 B, 100,000/s, p99.9 | 0.62–2.18 | 3.50 |
| aarch64 | xfs | Chronicle Queue | W2, 64 B, 100,000/s, p99.9 | 4.36–5.46 | 1.25 |
| aarch64 | xfs | plain file | W2, 64 B, 100,000/s, p99.9 | 7.57–11.4 | 1.51 |
| aarch64 | xfs | Chronicle Queue | W2, 1,024 B, 10,000/s, p99.9 | 2.98–4.27 | 1.44 |
| aarch64 | xfs | tickertape (safe) | W2, 1,024 B, 10,000/s, p99.9 | 12.6–14.4 | 1.14 |
| aarch64 | xfs | yaque | W2, 1,024 B, 10,000/s, p99.9 | 188–208 | 1.10 |
| aarch64 | xfs | Aeron IPC | W2, 1,024 B, 100,000/s, p99.9 | 1.46–3.67 | 2.52 |
| aarch64 | xfs | Chronicle Queue | W2, 1,024 B, 100,000/s, p99.9 | 3.36–5.40 | 1.61 |

### What these figures show and do not show

They show: two cloud machine types, one session each, at v1.0.0, with every tool configured as the corrected harness
does it, the order and the gaps stated above.

They do not show:

- **Bare metal.** These are virtual machines with a hypervisor between the threads and the hardware, and local SSDs
  behind the platform's 351 MB/s cap. The bare-metal results below (v0.4.0, v0.3.0) are older code with the old harness;
  none of them was rerun.
- **Other days and machines.** One session per machine: the variation between VMs of the same type, which the earlier
  sessions saw, is not in these ranges. The single-round parts used a fixed tool order; on xfs tickertape ran first on a
  freshly made file system and Chronicle followed on the same one.
- **`main`.** The library is v1.0.0. Changes merged after it (durable-only readers woken by checkpoints, the safe
  reader's read size at the tail, the layout of `Reader`, and the changes still being measured) can move tickertape's
  rows; they are measured in their own sections, not in this comparison.
- **Statistics.** min–max of n = 4, 2 or 1; no confidence intervals. A W1 burst lasts 32–52 ms (tickertape, 64 B,
  x86-64), and on x86-64 the first repeat of every W1-cold round ran at 29.12–30.84 million records/s (21.99 in the
  unpinned one) and the second at 17.53–20.07 (cause not traced; aarch64 shows no such split); an 11 s W2 run at 100,000
  records/s can be moved by one stall.
- **Equal loads in SV and SD.** Each tool ran at 50 % or 90 % of its own probe; the same percentage is a different rate.
- **Equal durability.** The sync operations differ in what they cover and promise; none was tested for power loss.
- **What the settings do one by one** in the sustained mode, or `microTouch()` on top of the new settings.
- **An interrupt-free tuned host** ([above](#interrupts-on-the-tuned-cpus)), unpinned sustained runs, Aeron on xfs in
  the sustained mode, Aeron Archive, macOS, Windows.

## 2026-11-02, v0.4.0 on bare-metal Linux (AWS `z1d.metal`)

The primary results for v0.4.0, recorded at `c9ec640`, the code released as v0.4.0. The environment and commands are
those of the v0.3.0 bare-metal section below, on a fresh `z1d.metal`, every process bound to NUMA node 0.

Two changes since v0.3.0 drive the differences:
- CRC32C runs three hardware chains in parallel for long records (ADR 0024);
- Linux prefaults 256 KiB ahead of the writer with `MADV_POPULATE_WRITE` (ADR 0025).

### Comparison with other tools

> **Superseded** by the [comparison rerun at v1.0.0](#2026-11-25-to-2026-11-26-comparison-rerun-at-v100-google-cloud-vms-linux-x86-64-and-aarch64),
> which corrected the harness: Chronicle Queue ran here with a short JIT warm-up, the default 64 MiB `blockSize` and no `microTouch()`, no thread was pinned, and W1 started its timer after tickertape had preallocated its segment but before Chronicle created its first file. The rerun ran on cloud VMs, not bare metal, so it does not repeat these bare-metal figures; it replaces them as the comparison. These results stay as they were recorded.

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

> **Superseded** by the [comparison rerun at v1.0.0](#2026-11-25-to-2026-11-26-comparison-rerun-at-v100-google-cloud-vms-linux-x86-64-and-aarch64),
> which corrected the harness: these runs had the old JVM settings and W1, no pinning, and v0.3.0's library. These results stay as they were recorded.

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

## 2026-11-26, what prefaulted, unwritten pages add to a checkpoint (Google Cloud VMs, Linux)

The 1.0.0 gap "how much prefaulted, unwritten data adds to a checkpoint" (issue #62), on Linux. **Cloud virtual machines
with local NVMe SSD, not bare metal**; macOS was not measured. The raw output (results, traces, scripts) is kept outside
the repository.

**Answer.** On Linux the prefault helper's `MADV_POPULATE_WRITE` does not only fault the 256 KiB ahead of the writer in:
it makes those pages dirty, and every checkpoint writes them to the device as zeros. In every checkpoint that covered
appends, at every rate measured, on both architectures and both filesystems, the active segment's whole-file `fdatasync`
(after the ranged `msync` has written the records) wrote 268 KiB on ext4 (256 KiB of data, 12 KiB of journal) and 260
KiB on xfs, and took 156–429 µs (medians per cell, early write-back on); without the advice it wrote nothing and took
2–4 µs. That moved the blocking checkpoint's median from 0.35–0.52 ms to 0.50–0.73 ms on aarch64 and from 0.36–0.44 ms
to 0.53–0.99 ms on x86-64 in the cells where its runs are steady enough to tell (200,000/s, and 1,024 B on ext4; below).
The checkpoint also write-protects those pages again, so the writer takes 33–36 more page faults after each checkpoint.
An idle writer pays once: its first checkpoint wrote 332–360 KiB instead of 16–44 KiB; from the third on, every
checkpoint wrote 8 KiB, with or without prefaulting. The bytes scale with the window: with a 4 MiB window (x86-64 only)
that `fdatasync` wrote 4.1 MiB and took 2.7–7.5 ms.

### Environment and commands

| Machine | CPU, vCPUs, memory | Zone | Time (UTC) | Runs |
|---|---|---|---|---|
| `c4-standard-8-lssd` | Intel Xeon 6985P-C, 8 (4 cores), 31.5 GB | us-east1-b | 05:35–06:49 | 150 and 25 (the ext4 traced round again, below) |
| `c4a-standard-4-lssd` | Google Axion (Neoverse-V2), 4, 16.7 GB | us-east1-b | 09:17–10:00 | 102 (configurations A, C, D, E and G) |

Ubuntu 24.04, kernel 7.0.0-1017-gcp, rustc 1.100.0, bpftrace 0.20.2, 4 KiB pages. One local NVMe SSD (375 GB), formatted
for each filesystem in turn: ext4 (`mkfs.ext4 -E lazy_itable_init=0,lazy_journal_init=0`), then xfs with defaults, both
mounted `noatime`; I/O scheduler `none`; the device reports `write_cache` "write through" (read on the x86-64 VM), so
the kernel sends it no cache flushes: `/sys/block/*/stat` counted none in any run on either VM. Write-back sysctls at
their defaults. Results on the boot disk.

Commit: `3d82592c` (`main` on 2026-11-26), for configurations A, B, C and G. Configurations D, E and F need switches
that the library does not have; they ran a build of the branch `exp/ckpt-prefault` (`5c558bc3`, `3d82592c` plus three
environment switches, never merged; the scripts as run are in its `expckpt/` directory since `3b10f049`). The
checkpoint's sync steps are the same at `3d82592c` and at the `main` this section was merged into. Binaries' sha256:
x86-64 `4aa97b7d…` (main), `31fc109d…` (exp); aarch64 `48ad1e3e…`, `a56182a3…` (the same on both aarch64 VMs).

```sh
[env TT_EXP_…=1] tt-sustained run --dir … --out … --rate <1|20000|200000> --payload <64|1024> --duration 20s \
  --warmup 3s --checkpoint-every 1s --readers mapped --cpus <writer>,<reader> --helper-cpus <helpers> --json \
  [--no-prefault | --no-early-writeback]
```

- **Configurations.** A: the defaults (prefault on, early write-back on). B: `--no-early-writeback`. C: `--no-prefault`
  (`WriterConfig::prefault(false)`, which also turns off early write-back). D: prefault off with early write-back on
  (`TT_EXP_NO_POPULATE`: the helper runs and advises nothing), the comparison that changes only the advice. E: prefault
  on, and the active segment synced by its ranged `msync` alone (`TT_EXP_RANGED`: no whole-file `fdatasync` of it,
  neither the per-segment one nor the barrier). F: a 4 MiB window (`TT_EXP_WINDOW_KIB=4096`). G: the defaults without
  checkpoints.
- **Cells.** Idle (one 64 B record per second), 20,000 and 200,000 records/s of 64 B, 20,000 records/s of 1,024 B (21
  MB/s, like 200,000 of 64 B). A checkpoint every second (the interval `operations.md` gives as the common pattern),
  blocking, on the writer's thread; 128 MiB segments, so 200,000/s and 1,024 B rolled three or four times per run. One
  zero-copy reader. The writer thread, the reader and the helper threads each on a CPU of their own core (`--cpus 1,2
  --helper-cpus 3` on both machines; core 0 for everything else).
- **Rounds.** Three rounds per filesystem, cells and configurations rotated from round to round; every journal deleted
  and `sync`ed before the next run. Rounds 1 and 2 are untraced: their two runs per cell are the noise floor, and every
  duration and latency below comes from them. Round 3 ran under `ckpt.bt` (branch `exp/ckpt-prefault`), which prints
  every `msync`, `fdatasync` and `fsync` of the writer's thread with its time and the bytes written to the device during
  it (block `rq_issue` tracepoint, every issuer, so journal and log writes are included), and counts the folios each
  thread made dirty (`writeback_dirty_folio`). Its first version matched the thread name `tt-sustained` exactly, while
  the binaries ran as `tt-sustained-main` and `-exp`: on x86-64 ext4 it recorded no calls, and that round was run again
  after the others with the filter fixed (xfs, and aarch64, used the fixed version).
- **Device bytes per checkpoint** in the second table are the device's written sectors over the whole run
  (`/sys/block/*/stat`), early write-back and the final checkpoint included, divided by the checkpoints plus one.
- **Tracing check.** Against the untraced runs of the same cell, the traced checkpoint's median was 11–21 µs longer at
  idle and otherwise within, below or up to 0.06 ms above their range on aarch64; on x86-64 it was 1.5–2.1 times as long
  in five cells (ext4 at 20,000/s D and E, 1,024 B D on both filesystems, xfs at 200,000/s A). The traced runs are used
  for bytes and for which call pays, not for durations.
- A first aarch64 VM ran the whole matrix of 150 runs, but its results were lost except 27 ext4 runs (the copy timed out
  and the VM was deleted); those agree with the figures below and are not used. The second VM ran four of the six
  configurations.

### Which call writes what

A checkpoint on Linux (`checkpoint.rs` → `steps`) syncs each dirty segment with `msync(MS_SYNC)` over its written range,
then `fdatasync` of the whole file; then `fsync` of the directories whose entries changed; then `fdatasync` of the
active segment again as the barrier; then writes and `fdatasync`s the marker. Medians over the checkpoints that cover
one segment, from the traced runs: KiB written to the device during the call / the call's time (ext4 ; xfs). The barrier
`fdatasync` is left out: in all 1,456 traced calls (910 on x86-64, 546 on aarch64) it took 0.7 and 1.3 µs (medians), at
most 172 µs, and wrote at most 4 KiB.

x86-64:

| Rate | Configuration | Ranged `msync` | Segment `fdatasync` | Marker `fdatasync` | Whole checkpoint | Checkpoints with a roll: KiB |
|---|---|---|---|---|---|---|
| 20,000/s, 64 B | default: prefault on, write-back on | 908 / 483 ; 900 / 384 | 268 / 235 ; 260 / 168 | 4 / 35 ; 4 / 33 | 1,180 / 715 ; 1,164 / 591 | – / – |
| 20,000/s, 64 B | prefault off, write-back on (exp) | 908 / 760 ; 900 / 370 | 0 / 2.1 ; 0 / 1.5 | 4 / 36 ; 4 / 33 | 912 / 812 ; 904 / 418 | – / – |
| 20,000/s, 64 B | `prefault(false)` (write-back off too) | 2,188 / 798 ; 2,180 / 581 | 0 / 2.3 ; 0 / 1.5 | 4 / 34 ; 4 / 33 | 2,192 / 854 ; 2,184 / 628 | – / – |
| 20,000/s, 64 B | prefault on, write-back off | 2,188 / 885 ; 2,180 / 712 | 268 / 427 ; 260 / 449 | 4 / 34 ; 4 / 34 | 2,460 / 1.35 ms ; 2,444 / 1.21 ms | – / – |
| 20,000/s, 64 B | prefault on, ranged sync (exp) | 908 / 757 ; 900 / 591 | – ; – | 4 / 37 ; 4 / 166 | 912 / 809 ; 904 / 784 | – / – |
| 20,000/s, 64 B | 4 MiB window (exp) | 896 / 281 ; 896 / 274 | 2,188 / 2.66 ms ; 2,180 / 2.8 ms | 4 / 42 ; 4 / 41 | 3,088 / 2.98 ms ; 3,080 / 3.12 ms | – / – |
| 200,000/s, 64 B | default: prefault on, write-back on | 524 / 397 ; 516 / 494 | 268 / 262 ; 260 / 429 | 4 / 119 ; 4 / 160 | 732 / 844 ; 716 / 1.1 ms | 1,084–1,212 / 1,040–1,168 |
| 200,000/s, 64 B | prefault off, write-back on (exp) | 474 / 378 ; 466 / 307 | 0 / 2.8 ; 0 / 2.2 | 4 / 36 ; 4 / 34 | 478 / 434 ; 470 / 359 | 816–944 / 780–908 |
| 200,000/s, 64 B | `prefault(false)` (write-back off too) | 20,460 / 54.4 ms ; 20,452 / 53.9 ms | 0 / 4.1 ; 0 / 2.5 | 4 / 112 ; 4 / 41 | 20,470 / 54.6 ms ; 20,458 / 53.9 ms | 20,384–20,400 / 20,348–20,364 |
| 200,000/s, 64 B | prefault on, write-back off | 20,492 / 54.1 ms ; 20,484 / 53.8 ms | 268 / 946 ; 260 / 490 | 4 / 44 ; 4 / 41 | 20,706 / 54.7 ms ; 20,686 / 54.3 ms | 20,668 / 20,624 |
| 200,000/s, 64 B | prefault on, ranged sync (exp) | 524 / 384 ; 516 / 320 | – ; – | 4 / 39 ; 4 / 34 | 528 / 436 ; 520 / 373 | 816–944 / 780–908 |
| 200,000/s, 64 B | 4 MiB window (exp) | 524 / 387 ; 516 / 328 | 4,108 / 7.25 ms ; 4,100 / 7.3 ms | 4 / 48 ; 4 / 46 | 4,572 / 7.95 ms ; 4,556 / 7.66 ms | 4,924–5,052 / 4,880–5,008 |
| 20,000/s, 1,024 B | default: prefault on, write-back on | 588 / 394 ; 580 / 319 | 268 / 237 ; 260 / 177 | 4 / 36 ; 4 / 33 | 860 / 695 ; 844 / 548 | 432–1,212 / 396–1,168 |
| 20,000/s, 1,024 B | prefault off, write-back on (exp) | 588 / 531 ; 580 / 481 | 0 / 3.0 ; 0 / 2.0 | 4 / 35 ; 4 / 159 | 592 / 612 ; 584 / 673 | 168–948 / 136–908 |
| 20,000/s, 1,024 B | `prefault(false)` (write-back off too) | 20,876 / 55.7 ms ; 20,868 / 55.4 ms | 0 / 3.4 ; 0 / 2.5 | 4 / 44 ; 4 / 34 | 20,880 / 55.8 ms ; 20,872 / 55.4 ms | 20,912 / 20,876 |
| 20,000/s, 1,024 B | prefault on, write-back off | 20,876 / 55.2 ms ; 20,868 / 55.0 ms | 268 / 826 ; 260 / 981 | 4 / 44 ; 4 / 42 | 21,148 / 56.1 ms ; 21,132 / 56.1 ms | 21,180 / 21,136 |
| 20,000/s, 1,024 B | prefault on, ranged sync (exp) | 588 / 421 ; 580 / 323 | – ; – | 4 / 36 ; 4 / 35 | 592 / 470 ; 584 / 379 | 164–948 / 136–908 |
| 20,000/s, 1,024 B | 4 MiB window (exp) | 588 / 391 ; 580 / 325 | 4,108 / 7.46 ms ; 4,100 / 7.37 ms | 4 / 49 ; 4 / 44 | 4,700 / 8.1 ms ; 4,684 / 7.79 ms | 4,272–5,052 / 4,236–5,008 |

aarch64:

| Rate | Configuration | Ranged `msync` | Segment `fdatasync` | Marker `fdatasync` | Whole checkpoint | Checkpoints with a roll: KiB |
|---|---|---|---|---|---|---|
| 20,000/s, 64 B | default: prefault on, write-back on | 908 / 482 ; 900 / 434 | 268 / 193 ; 260 / 156 | 4 / 24 ; 4 / 22 | 1,180 / 728 ; 1,164 / 649 | – / – |
| 20,000/s, 64 B | prefault off, write-back on (exp) | 908 / 474 ; 900 / 468 | 0 / 3.0 ; 0 / 2.2 | 4 / 25 ; 4 / 22 | 912 / 519 ; 904 / 513 | – / – |
| 20,000/s, 64 B | `prefault(false)` (write-back off too) | 2,188 / 1.03 ms ; 2,180 / 913 | 0 / 2.9 ; 0 / 2.5 | 4 / 22 ; 4 / 22 | 2,192 / 1.07 ms ; 2,184 / 971 | – / – |
| 20,000/s, 64 B | prefault on, ranged sync (exp) | 908 / 534 ; 900 / 464 | – ; – | 4 / 25 ; 4 / 23 | 912 / 574 ; 904 / 505 | – / – |
| 200,000/s, 64 B | default: prefault on, write-back on | 524 / 340 ; 516 / 322 | 268 / 195 ; 260 / 157 | 4 / 23 ; 4 / 22 | 732 / 570 ; 716 / 506 | 1,088–1,216 / 1,040–1,168 |
| 200,000/s, 64 B | prefault off, write-back on (exp) | 474 / 331 ; 466 / 288 | 0 / 3.1 ; 0 / 2.6 | 4 / 24 ; 4 / 22 | 478 / 382 ; 470 / 336 | 820–948 / 780–908 |
| 200,000/s, 64 B | `prefault(false)` (write-back off too) | 20,460 / 55.5 ms ; 20,452 / 56.9 ms | 0 / 3.2 ; 0 / 2.7 | 4 / 23 ; 4 / 22 | 20,470 / 55.5 ms ; 20,458 / 57.0 ms | 20,384–20,404 / 20,348–20,364 |
| 200,000/s, 64 B | prefault on, ranged sync (exp) | 524 / 335 ; 516 / 308 | – ; – | 4 / 25 ; 4 / 23 | 528 / 378 ; 520 / 350 | 820–948 / 780–908 |
| 20,000/s, 1,024 B | default: prefault on, write-back on | 588 / 369 ; 580 / 309 | 268 / 191 ; 260 / 159 | 4 / 23 ; 4 / 22 | 860 / 620 ; 844 / 527 | 432–1,216 / 396–1,168 |
| 20,000/s, 1,024 B | prefault off, write-back on (exp) | 588 / 348 ; 580 / 315 | 0 / 3.2 ; 0 / 2.6 | 4 / 23 ; 4 / 22 | 592 / 413 ; 584 / 362 | 164–948 / 136–908 |
| 20,000/s, 1,024 B | `prefault(false)` (write-back off too) | 20,876 / 56.8 ms ; 20,868 / 58.6 ms | 0 / 3.4 ; 0 / 2.9 | 4 / 24 ; 4 / 22 | 20,880 / 56.9 ms ; 20,872 / 58.7 ms | 20,912–20,916 / 20,876 |
| 20,000/s, 1,024 B | prefault on, ranged sync (exp) | 588 / 360 ; 580 / 339 | – ; – | 4 / 24 ; 4 / 23 | 592 / 400 ; 584 / 381 | 164–948 / 136–908 |

- With the advice (A, B, F), the whole-file `fdatasync` after the ranged `msync` writes exactly the window, as zeros:
  256 KiB (4 MiB with F) plus 12 KiB of ext4 journal or 4 KiB of xfs log, in every checkpoint, whatever the rate and
  record size. The `msync` writes the records early write-back has not yet written (about 0.5–0.9 MiB at these rates),
  or all of them without early write-back; the same with and without the advice, except 50 KiB more with it at
  200,000/s.
- The traced runs count who made the folios of the journal device dirty: the `tickertape-prefault` thread 41 at idle,
  406–458 per run at 20,000/s of 64 B and 3,796–3,932 at 21 MB/s, the writer's thread then 59–526 (A, B, E); without the
  advice (C, D) the helper none and the writer 1,138–10,766 at 20,000/s and above (x86-64). They are folios, not pages
  (the page cache uses large folios on this kernel), so they show who dirties, not how many bytes.
- **Rolls.** Checkpoints that cover a roll (two segments) wrote 1,040–1,216 KiB with the advice and 780–948 KiB without:
  the same 260–268 KiB more.
- **Idle.** The window moves only when the writer does, so an idle writer's window is written once. KiB written by each
  checkpoint in turn (the same on both architectures):

- ext4 default: prefault on, write-back on: 360, 20, 8, 8, 8, 8 …
- xfs default: prefault on, write-back on: 332, 12, 8, 8, 8, 8 …
- ext4 prefault off, write-back on (exp): 44, 20, 8, 8, 8, 8 …
- xfs prefault off, write-back on (exp): 16, 12, 8, 8, 8, 8 …
- ext4 `prefault(false)` (write-back off too): 44, 20, 8, 8, 8, 8 …
- xfs `prefault(false)` (write-back off too): 16, 12, 8, 8, 8, 8 …
- ext4 prefault on, ranged sync (exp): 44, 20, 8, 8, 8, 8 …
- xfs prefault on, ranged sync (exp): 16, 12, 8, 8, 8, 8 …

### Checkpoint duration and bytes

x86-64 (two untraced runs per cell; ranges over the runs; ext4 / xfs; µs unless a cell says ms):

| Rate | Configuration | Checkpoint p50 | Checkpoint max | Device KiB per checkpoint | Appended KiB per checkpoint |
|---|---|---|---|---|---|
| idle (1/s), 64 B | default: prefault on, write-back on | 89–321 / 103–372 | 121–388 / 135–505 | 29 / 27–29 | 0 / 0 |
| idle (1/s), 64 B | prefault off, write-back on (exp) | 81–95 / 96–195 | 113–120 / 115–373 | 15 / 9 | 0 / 0 |
| idle (1/s), 64 B | `prefault(false)` (write-back off too) | 86–88 / 86–89 | 215–265 / 97–111 | 15 / 14 | 0 / 0 |
| idle (1/s), 64 B | prefault on, write-back off | 84–100 / 88–97 | 113–121 / 104 | 29 / 27 | 0 / 0 |
| idle (1/s), 64 B | prefault on, ranged sync (exp) | 87–89 / 103–345 | 112–134 / 136–499 | 15 / 14 | 0 / 0 |
| idle (1/s), 64 B | 4 MiB window (exp) | 90–310 / 97–368 | 128–445 / 166–467 | 199 / 193 | 0 / 0 |
| 20,000/s, 64 B | default: prefault on, write-back on | 625–898 / 741–1.2 ms | 988–1.92 ms / 1.16 ms–1.45 ms | 2,410 / 2,389–2,394 | 2,031 / 2,031 |
| 20,000/s, 64 B | prefault off, write-back on (exp) | 433–435 / 426–733 | 485–537 / 594–956 | 2,157 / 2,148–2,149 | 2,031 / 2,031 |
| 20,000/s, 64 B | `prefault(false)` (write-back off too) | 729–767 / 619–1 ms | 799–1.04 ms / 658–3.77 ms | 2,157 / 2,149–2,150 | 2,031 / 2,031 |
| 20,000/s, 64 B | prefault on, write-back off | 1.47 ms–1.59 ms / 1.26 ms–1.53 ms | 1.66 ms–2.11 ms / 1.69 ms–1.74 ms | 2,410 / 2,389–2,394 | 2,031 / 2,031 |
| 20,000/s, 64 B | prefault on, ranged sync (exp) | 408–468 / 425–751 | 477–612 / 551–944 | 2,175 / 2,163–2,167 | 2,031 / 2,031 |
| 20,000/s, 64 B | 4 MiB window (exp) | 2.74 ms–3.09 ms / 2.97 ms–3.12 ms | 3.66 ms–3.79 ms / 3.59 ms–4.24 ms | 4,419 / 4,406–4,409 | 2,031 / 2,031 |
| 200,000/s, 64 B | default: prefault on, write-back on | 705–989 / 526–553 | 1.56 ms–1.97 ms / 1.5 ms–1.58 ms | 21,636 / 21,612 | 20,312 / 20,312 |
| 200,000/s, 64 B | prefault off, write-back on (exp) | 377–388 / 362–370 | 1.75 ms–1.77 ms / 1.63 ms–2.08 ms | 21,195–21,197 / 21,185–21,190 | 20,312 / 20,312 |
| 200,000/s, 64 B | `prefault(false)` (write-back off too) | 54.5 ms–54.6 ms / 53.9 ms–54.1 ms | 55.2 ms–55.3 ms / 55.0 ms–55.1 ms | 20,437–20,438 / 20,427 | 20,312 / 20,312 |
| 200,000/s, 64 B | prefault on, write-back off | 54.7 ms–54.9 ms / 54.3 ms–54.4 ms | 55.1 ms–55.5 ms / 55.1 ms–73.1 ms | 20,696 / 20,677–20,678 | 20,312 / 20,312 |
| 200,000/s, 64 B | prefault on, ranged sync (exp) | 427–446 / 369–404 | 1.33 ms–1.35 ms / 1.32 ms–1.34 ms | 21,402 / 21,389–21,390 | 20,312 / 20,312 |
| 200,000/s, 64 B | 4 MiB window (exp) | 7.78 ms–8.1 ms / 7.36 ms–7.66 ms | 9.88 ms–9.91 ms / 10.1 ms–16.7 ms | 25,476 / 25,455–25,457 | 20,312 / 20,312 |
| 20,000/s, 1,024 B | default: prefault on, write-back on | 664–674 / 552–1.09 ms | 1.47 ms–1.5 ms / 1.39 ms–1.85 ms | 23,589 / 23,570–23,571 | 20,781 / 20,781 |
| 20,000/s, 1,024 B | prefault off, write-back on (exp) | 434–435 / 362–371 | 1.34 ms–1.35 ms / 1.17 ms–8.16 ms | 23,300 / 23,287–23,289 | 20,781 / 20,781 |
| 20,000/s, 1,024 B | `prefault(false)` (write-back off too) | 55.5 ms–55.7 ms / 55.3 ms–55.4 ms | 56.2 ms–56.7 ms / 56.0 ms–56.4 ms | 20,930 / 20,919 | 20,781 / 20,781 |
| 20,000/s, 1,024 B | prefault on, write-back off | 55.9 ms–56.2 ms / 55.9 ms–56.1 ms | 56.5 ms–56.9 ms / 56.6 ms–57.2 ms | 21,169 / 21,148–21,150 | 20,781 / 20,781 |
| 20,000/s, 1,024 B | prefault on, ranged sync (exp) | 424–461 / 387–406 | 1.22 ms–1.37 ms / 1.32 ms–1.42 ms | 23,355 / 23,343 | 20,781 / 20,781 |
| 20,000/s, 1,024 B | 4 MiB window (exp) | 8.06 ms–8.16 ms / 7.78 ms–7.79 ms | 9.74 ms–9.88 ms / 9.9 ms–9.95 ms | 27,429 / 27,405 | 20,781 / 20,781 |

aarch64:

| Rate | Configuration | Checkpoint p50 | Checkpoint max | Device KiB per checkpoint | Appended KiB per checkpoint |
|---|---|---|---|---|---|
| idle (1/s), 64 B | default: prefault on, write-back on | 55–57 / 55 | 63–69 / 71–72 | 29–30 / 27 | 0 / 0 |
| idle (1/s), 64 B | prefault off, write-back on (exp) | 55–56 / 56–57 | 67–72 / 63–68 | 16 / 9–12 | 0 / 0 |
| idle (1/s), 64 B | `prefault(false)` (write-back off too) | 56–59 / 56–59 | 67–70 / 66–68 | 16 / 9–14 | 0 / 0 |
| idle (1/s), 64 B | prefault on, ranged sync (exp) | 55 / 57–59 | 67–71 / 63–73 | 16 / 9–12 | 0 / 0 |
| 20,000/s, 64 B | default: prefault on, write-back on | 708–727 / 648 | 850–983 / 797–828 | 2,410 / 2,394 | 2,031 / 2,031 |
| 20,000/s, 64 B | prefault off, write-back on (exp) | 511–521 / 455–468 | 668–714 / 546–579 | 2,157–2,158 / 2,149–2,150 | 2,031 / 2,031 |
| 20,000/s, 64 B | `prefault(false)` (write-back off too) | 1.05 ms / 924–944 | 1.19 ms–1.31 ms / 1.02 ms–1.24 ms | 2,158 / 2,145–2,151 | 2,031 / 2,031 |
| 20,000/s, 64 B | prefault on, ranged sync (exp) | 507–520 / 467–482 | 611–624 / 579–586 | 2,175–2,176 / 2,166–2,167 | 2,031 / 2,031 |
| 200,000/s, 64 B | default: prefault on, write-back on | 535–550 / 495–521 | 1.59 ms–1.69 ms / 1.49 ms–1.53 ms | 21,637 / 21,612–21,618 | 20,312 / 20,312 |
| 200,000/s, 64 B | prefault off, write-back on (exp) | 348–364 / 372–386 | 1.82 ms–1.85 ms / 1.76 ms–1.79 ms | 21,200 / 21,187 | 20,312 / 20,312 |
| 200,000/s, 64 B | `prefault(false)` (write-back off too) | 55.5 ms–55.6 ms / 56.1 ms–56.3 ms | 56.8 ms–56.9 ms / 57.1 ms–57.4 ms | 20,437 / 20,425–20,426 | 20,312 / 20,312 |
| 200,000/s, 64 B | prefault on, ranged sync (exp) | 366–372 / 339–346 | 1.4 ms / 1.3 ms–1.34 ms | 21,402 / 21,386–21,390 | 20,312 / 20,312 |
| 20,000/s, 1,024 B | default: prefault on, write-back on | 602–615 / 505–517 | 1.51 ms / 1.41 ms–1.43 ms | 23,589–23,590 / 23,565–23,570 | 20,781 / 20,781 |
| 20,000/s, 1,024 B | prefault off, write-back on (exp) | 385–403 / 384–391 | 1.4 ms–1.5 ms / 1.38 ms–1.4 ms | 23,301–23,302 / 23,285–23,289 | 20,781 / 20,781 |
| 20,000/s, 1,024 B | `prefault(false)` (write-back off too) | 56.9 ms / 58.6 ms | 57.8 ms–57.9 ms / 59.9 ms | 20,930–20,931 / 20,914–20,920 | 20,781 / 20,781 |
| 20,000/s, 1,024 B | prefault on, ranged sync (exp) | 396–443 / 353–375 | 1.42 ms–1.48 ms / 1.27 ms–1.29 ms | 23,355 / 23,343 | 20,781 / 20,781 |

- **The window's cost against the noise.** On aarch64 every non-idle cell separates: A 0.50–0.73 ms against D 0.35–0.52
  ms (median per run), runs of one configuration within 0.04 ms of each other. On x86-64 the separation is clear at
  200,000/s (ext4 0.71–0.99 against 0.38–0.39 ms, xfs 0.53–0.55 against 0.36–0.37 ms) and at 1,024 B on ext4 (0.66–0.67
  against 0.43 ms); at 20,000/s of 64 B (both filesystems) and at 1,024 B on xfs the two runs of A differ by 0.27–0.54
  ms, as much as the difference to D, so no figure is given for those cells beyond the call's own time above.
- At idle the steady checkpoints write the same 8 KiB with or without the advice; A's first checkpoint is the difference
  in the device total (27–30 against 9–16 KiB per checkpoint). With the window written in every checkpoint, the whole
  run wrote 241–440 KiB more per checkpoint with A than with D.
- **Without early write-back** (B, C) a checkpoint writes the whole second of records: 54–59 ms at 21 MB/s, 0.6–1.6 ms
  at 20,000/s of 64 B. The window adds the same 260–268 KiB, in 0.4–1.0 ms (B, x86-64; B was not run on aarch64).
- **The ranged sync (E)** wrote what D wrote in the checkpoint's calls and took as long as D. Over the whole run E still
  wrote 17–206 KiB per checkpoint more than D (most at 200,000/s), outside the checkpoint's calls; where those writes
  come from was not traced.
- With early write-back on, the whole run wrote 0.7–2.3 MiB per checkpoint more than without it at 21 MB/s (D against
  C), with or without prefaulting; that is not about prefaulting and was not investigated.

### Latency during the checkpoints

The zero-copy reader's latency is measured from each record's scheduled send, as `tt-sustained` always does, so a
blocking checkpoint that stops the writer shows as the reader's maximum. Append is the time of one `append` call;
writer-thread faults are the minor faults of the writer's thread over the run (`RUSAGE_THREAD`); "late" is sends more
than 1 µs after their scheduled time. ext4 / xfs.

x86-64:

| Rate | Configuration | Reader p50 | p99 | p99.9 | max | Append p99.9 | Append max | Writer-thread faults | Late sends (%) |
|---|---|---|---|---|---|---|---|---|---|
| idle (1/s), 64 B | default: prefault on, write-back on | 4.8–7.8 / 10–11 | 6.0–15 / 21–38 | 6.0–15 / 21–38 | 6.0–15 / 21–38 | 5.3–14 / 19–27 | 5.3–14 / 19–27 | 85–103 / 117–132 | 4.35 / 4.35 |
| idle (1/s), 64 B | prefault off, write-back on (exp) | 4.8–13 / 9.6–12 | 18–27 / 16–29 | 18–27 / 16–29 | 18–27 / 16–29 | 10–23 / 14–26 | 10–23 / 14–26 | 94–99 / 94–141 | 4.35 / 4.35 |
| idle (1/s), 64 B | `prefault(false)` (write-back off too) | 4.9–5.1 / 8.7–9.4 | 11–17 / 17–33 | 11–17 / 17–33 | 11–17 / 17–33 | 9.4–11 / 15–19 | 9.4–11 / 15–19 | 131–137 / 94–107 | 4.35 / 4.35 |
| idle (1/s), 64 B | prefault on, write-back off | 5.0–6.1 / 10–11 | 10–14 / 18–21 | 10–14 / 18–21 | 10–14 / 18–21 | 9.6–10 / 17–19 | 9.6–10 / 17–19 | 90–97 / 94–105 | 4.35 / 4.35 |
| idle (1/s), 64 B | prefault on, ranged sync (exp) | 5.0–9.9 / 11–12 | 9.5–20 / 28–33 | 9.5–20 / 28–33 | 9.5–20 / 28–33 | 8.6–19 / 20–25 | 8.6–19 / 20–25 | 98–132 / 132–135 | 4.35 / 4.35 |
| idle (1/s), 64 B | 4 MiB window (exp) | 5.9–7.7 / 9.8–13 | 14–24 / 19–40 | 14–24 / 19–40 | 14–24 / 19–40 | 11–22 / 17–30 | 11–22 / 17–30 | 98–136 / 117–132 | 4.35 / 4.35 |
| 20,000/s, 64 B | default: prefault on, write-back on | 0.3–0.4 / 0.4 | 0.5–0.6 / 0.7–4.7 | 2.6–86 / 7.8–99 | 943–1.88 ms / 1.24 ms–1.57 ms | 2.1–2.2 / 4.4–5.5 | 7.5–8.8 / 12–23 | 1,307–1,358 / 1,329–1,369 | 0.08–0.15 / 0.09–5.12 |
| 20,000/s, 64 B | prefault off, write-back on (exp) | 0.3–0.4 / 0.4 | 0.5 / 0.7–0.8 | 27–28 / 31–43 | 548–595 / 673–1.08 ms | 4.9–6.2 / 7.5–9.4 | 39–42 / 50–80 | 1,435–1,481 / 1,440–1,445 | 0.07 / 0.07–0.14 |
| 20,000/s, 64 B | `prefault(false)` (write-back off too) | 0.3–0.4 / 0.4 | 0.5–0.6 / 0.7–0.8 | 30–33 / 36–111 | 826–1.08 ms / 733–3.74 ms | 5.0–5.9 / 9.8–10 | 43–50 / 43–93 | 1,441–1,442 / 1,440–1,498 | 0.10–0.26 / 0.08–0.16 |
| 20,000/s, 64 B | prefault on, write-back off | 0.3–0.4 / 0.4 | 0.5–0.6 / 0.7–2.7 | 352–530 / 246–399 | 1.62 ms–2.19 ms / 1.65 ms–1.7 ms | 2.0–2.1 / 4.3–5.1 | 7.2–8.4 / 12–20 | 1,311–1,368 / 1,327–1,329 | 0.14–0.16 / 0.14–0.75 |
| 20,000/s, 64 B | prefault on, ranged sync (exp) | 0.4 / 0.4 | 0.5–0.6 / 0.7–4.4 | 2.5–3.7 / 7.7–9.9 | 502–682 / 612–912 | 1.4–2.0 / 3.2–3.7 | 7.8–9.2 / 17 | 435–502 / 458–498 | 0.06–0.07 / 0.09–5.05 |
| 20,000/s, 64 B | 4 MiB window (exp) | 0.4 / 0.4 | 2.3–3.5 / 5.1–5.4 | 1.93 ms–2.19 ms / 2.01 ms–2.08 ms | 3.71 ms–3.75 ms / 3.68 ms–4.34 ms | 3.0–7.0 / 7.6–9.4 | 8.3–15 / 14–20 | 11,294 / 11,294–11,356 | 0.33–0.35 / 0.36–2.10 |
| 200,000/s, 64 B | default: prefault on, write-back on | 0.3–0.4 / 0.4 | 1.6–1.7 / 1.6–2.2 | 58–244 / 39–46 | 1.57 ms–2.16 ms / 1.53 ms–1.73 ms | 1.8–2.4 / 2.2–2.4 | 178–196 / 241–245 | 6,094–6,105 / 6,107–6,130 | 0.82–0.89 / 0.86–1.17 |
| 200,000/s, 64 B | prefault off, write-back on (exp) | 0.3–0.4 / 0.4 | 4.6–5.1 / 3.0–6.4 | 37–46 / 37–39 | 1.77 ms–1.8 ms / 1.77 ms–2.24 ms | 6.5–12 / 7.3–7.5 | 174–198 / 220–222 | 14,437–14,523 / 14,466–14,480 | 1.18–1.40 / 1.15–1.63 |
| 200,000/s, 64 B | `prefault(false)` (write-back off too) | 0.4 / 0.4 | 45.0 ms / 44.5 ms–44.6 ms | 53.5 ms–53.6 ms / 53.1 ms–53.2 ms | 55.2 ms–55.3 ms / 55.0 ms–55.1 ms | 4.8–9.5 / 5.0–5.3 | 182–189 / 217–218 | 10,292–10,408 / 10,375–10,421 | 6.33–6.80 / 6.30–6.45 |
| 200,000/s, 64 B | prefault on, write-back off | 0.3–0.4 / 0.4 | 45.2 ms–45.3 ms / 45.0 ms–45.9 ms | 53.8 ms / 53.6 ms–54.8 ms | 55.1 ms–55.5 ms / 55.2 ms–73.2 ms | 1.4–1.9 / 2.0–2.1 | 161–207 / 223–240 | 1,371 / 1,370–1,376 | 5.95–5.99 / 6.08–6.38 |
| 200,000/s, 64 B | prefault on, ranged sync (exp) | 0.3–0.4 / 0.4 | 1.4–1.5 / 1.5–1.8 | 28–32 / 27–29 | 1.35 ms–1.38 ms / 1.36 ms–1.37 ms | 1.6–2.1 / 2.3 | 168–178 / 231–253 | 5,297–5,334 / 5,311–5,313 | 0.70–0.81 / 0.76–1.05 |
| 200,000/s, 64 B | 4 MiB window (exp) | 0.3–0.4 / 0.4 | 5.6–8.3 / 8.1–8.2 | 7.32 ms–7.47 ms / 7.04 ms–7.63 ms | 9.92 ms / 10.1 ms–16.7 ms | 2.2–3.8 / 2.8–2.9 | 179–187 / 217–226 | 26,371–26,396 / 26,387–26,392 | 1.51–1.67 / 1.65–1.90 |
| 200,000/s, 64 B | default, no checkpoints | 0.3–0.4 / 0.4 | 1.3–2.4 / 2.5 | 3.9–9.4 / 9.2–10 | 834–982 / 940–953 | 1.5–7.6 / 2.3–6.9 | 155–177 / 231–245 | 4,893–4,949 / 4,920–4,923 | 0.61–0.68 / 0.71–1.18 |
| 20,000/s, 1,024 B | default: prefault on, write-back on | 0.8–0.9 / 0.9 | 2.9–3.9 / 4.0–4.9 | 38–41 / 44–289 | 1.43 ms–1.46 ms / 1.36 ms–1.82 ms | 6.9–9.3 / 10–11 | 152–189 / 225–234 | 10,808–10,890 / 10,837–10,849 | 0.22–0.25 / 0.25–0.29 |
| 20,000/s, 1,024 B | prefault off, write-back on (exp) | 0.8–0.9 / 0.9 | 7.6–9.8 / 7.5–9.8 | 36–49 / 40–73 | 1.3 ms–1.32 ms / 1.14 ms–8.14 ms | 30–42 / 34–36 | 160–197 / 217–238 | 14,271–14,334 / 14,320–14,338 | 0.22 / 0.26–0.29 |
| 20,000/s, 1,024 B | `prefault(false)` (write-back off too) | 0.9 / 0.9 | 45.8 ms / 45.5 ms–45.6 ms | 54.7 ms / 54.4 ms–54.5 ms | 56.2 ms–56.6 ms / 56.1 ms–56.3 ms | 29–33 / 30 | 157–161 / 208–213 | 4,984–4,987 / 4,984 | 5.41–5.42 / 5.39 |
| 20,000/s, 1,024 B | prefault on, write-back off | 0.9 / 0.9 | 46.2 ms–46.4 ms / 46.2 ms–46.3 ms | 55.0 ms–55.2 ms / 55.0 ms–55.1 ms | 56.5 ms–56.9 ms / 56.6 ms–57.1 ms | 2.8–3.0 / 3.0–3.1 | 203–204 / 223–225 | 1,415–1,426 / 1,462–1,485 | 5.44–5.49 / 5.45–5.47 |
| 20,000/s, 1,024 B | prefault on, ranged sync (exp) | 0.9 / 0.9 | 3.1–4.0 / 4.2–5.4 | 23–30 / 36–39 | 1.19 ms–1.34 ms / 1.28 ms–1.39 ms | 7.3–8.6 / 10–12 | 184–201 / 246–247 | 10,000–10,048 / 10,036–10,056 | 0.18–0.22 / 0.22–0.23 |
| 20,000/s, 1,024 B | 4 MiB window (exp) | 0.8–0.9 / 0.8–0.9 | 7.3–11 / 7.5–8.2 | 7.19 ms–7.29 ms / 7 ms–7.09 ms | 9.71 ms–9.84 ms / 9.86 ms–9.9 ms | 7.4–12 / 11 | 179–200 / 216–228 | 30,238–30,271 / 30,256–30,317 | 0.94–0.99 / 0.95 |

aarch64:

| Rate | Configuration | Reader p50 | p99 | p99.9 | max | Append p99.9 | Append max | Writer-thread faults | Late sends (%) |
|---|---|---|---|---|---|---|---|---|---|
| idle (1/s), 64 B | default: prefault on, write-back on | 6.6 / 7.7–7.9 | 8.8–18 / 12 | 8.8–18 / 12 | 8.8–18 / 12 | 7.9–11 / 11–12 | 7.9–11 / 11–12 | 94–117 / 119 | 4.35 / 4.35 |
| idle (1/s), 64 B | prefault off, write-back on (exp) | 6.3–7.2 / 7.2–7.5 | 9.6–12 / 10–11 | 9.6–12 / 10–11 | 9.6–12 / 10–11 | 9.1–11 / 9.9 | 9.1–11 / 9.9 | 127 / 86–129 | 4.35 / 4.35 |
| idle (1/s), 64 B | `prefault(false)` (write-back off too) | 6.9–7.3 / 7.8–7.9 | 10–12 / 10–11 | 10–12 / 10–11 | 10–12 / 10–11 | 9.5–11 / 9.8–10 | 9.5–11 / 9.8–10 | 127 / 120 | 4.35 / 4.35 |
| idle (1/s), 64 B | prefault on, ranged sync (exp) | 6.6–7.1 / 7.7–8.4 | 9.8–11 / 11 | 9.8–11 / 11 | 9.8–11 / 11 | 9.1–9.6 / 10–11 | 9.1–9.6 / 10–11 | 128 / 85–119 | 4.35 / 4.35 |
| 20,000/s, 64 B | default: prefault on, write-back on | 0.3 / 0.3 | 0.4–0.5 / 0.4–2.0 | 4.2–4.3 / 4.3–5.2 | 833–1.02 ms / 755–793 | 3.4–3.5 / 3.7–3.8 | 10–11 / 9.3–10 | 1,308–1,319 / 1,319–1,327 | 0.07 / 0.06–5.06 |
| 20,000/s, 64 B | prefault off, write-back on (exp) | 0.3 / 0.3 | 0.4 / 0.4–0.9 | 22–23 / 14 | 627–673 / 574–588 | 7.3–7.6 / 4.9–5.0 | 33–39 / 35–37 | 1,441 / 1,481 | 0.05–0.06 / 0.05–0.20 |
| 20,000/s, 64 B | `prefault(false)` (write-back off too) | 0.3 / 0.3 | 0.4 / 0.4 | 60–78 / 24–28 | 1.15 ms–1.27 ms / 974–1.2 ms | 7.7–7.8 / 4.7–4.8 | 35–40 / 33–34 | 1,447–1,491 / 1,441–1,447 | 0.11 / 0.09–0.10 |
| 20,000/s, 64 B | prefault on, ranged sync (exp) | 0.3 / 0.3 | 0.5–1.9 / 0.4–0.5 | 3.3 / 3.7–3.9 | 584 / 568–594 | 2.4–2.6 / 2.5–2.7 | 10–15 / 8.8–9.2 | 454 / 454–483 | 0.05–0.25 / 0.05 |
| 200,000/s, 64 B | default: prefault on, write-back on | 0.3 / 0.3 | 0.5–1.7 / 0.5–0.8 | 17–19 / 12 | 1.68 ms–1.79 ms / 1.57 ms–1.62 ms | 2.1 / 2.0–2.1 | 189–199 / 186–192 | 6,513–6,527 / 6,512–6,532 | 0.10–1.04 / 0.24–0.61 |
| 200,000/s, 64 B | prefault off, write-back on (exp) | 0.3 / 0.3 | 1.6–2.0 / 0.5–0.8 | 23–25 / 20 | 1.91 ms–1.94 ms / 1.85 ms–1.88 ms | 7.5–7.6 / 4.4–4.5 | 185–191 / 193–207 | 14,832–14,853 / 14,874–14,904 | 0.35–0.87 / 0.28–0.32 |
| 200,000/s, 64 B | `prefault(false)` (write-back off too) | 0.3 / 0.3 | 46.2 ms–46.3 ms / 46.8 ms–46.9 ms | 54.7 ms–54.8 ms / 55.3 ms–55.5 ms | 56.8 ms–56.9 ms / 57.1 ms–57.4 ms | 4.4–4.6 / 3.6–3.9 | 182–189 / 181–189 | 10,407–10,430 / 10,407–10,419 | 5.87–5.90 / 6.08–6.74 |
| 200,000/s, 64 B | prefault on, ranged sync (exp) | 0.3 / 0.3 | 0.5–1.2 / 0.4–0.5 | 4.4–4.7 / 3.8–4.2 | 1.48 ms–1.49 ms / 1.39 ms–1.42 ms | 2.1 / 2.0 | 193–201 / 201–202 | 5,719 / 5,718–5,730 | 0.09–0.50 / 0.08–0.10 |
| 200,000/s, 64 B | default, no checkpoints | 0.3 / 0.3 | 0.5–1.6 / 1.2–1.8 | 2.6–2.9 / 2.8–3.2 | 1.03 ms–1.04 ms / 957–973 | 2.2–2.8 / 2.1–2.4 | 187–189 / 192–201 | 5,293–5,309 / 5,319–5,327 | 0.25 / 0.52–0.53 |
| 20,000/s, 1,024 B | default: prefault on, write-back on | 0.6 / 0.6 | 2.3–2.4 / 2.2–2.3 | 21 / 12–14 | 1.47 ms / 1.37 ms–1.39 ms | 6.7 / 5.7–6.1 | 191–192 / 193–196 | 11,487–11,495 / 11,496–11,503 | 0.08 / 0.06–0.07 |
| 20,000/s, 1,024 B | prefault off, write-back on (exp) | 0.6 / 0.6 | 5.9–7.1 / 5.2 | 27–28 / 18–20 | 1.36 ms–1.47 ms / 1.35 ms–1.37 ms | 23–24 / 14–15 | 183–190 / 182 | 14,931–14,935 / 14,981–14,985 | 0.07–1.72 / 0.05 |
| 20,000/s, 1,024 B | `prefault(false)` (write-back off too) | 0.6 / 0.6 | 47.0 ms / 48.7 ms | 55.8 ms–55.9 ms / 57.6 ms–57.7 ms | 57.7 ms–57.9 ms / 59.8 ms–59.9 ms | 23 / 12–13 | 188 / 205–206 | 4,970 / 4,984 | 5.50 / 5.66–5.67 |
| 20,000/s, 1,024 B | prefault on, ranged sync (exp) | 0.6 / 0.6 | 2.3 / 2.2–3.0 | 13–14 / 10.0–12 | 1.47 ms–1.52 ms / 1.24 ms–1.33 ms | 6.7–7.0 / 6.2–6.4 | 185–199 / 192 | 10,676–10,677 / 10,699–10,701 | 0.06 / 0.05–5.04 |

- The reader's maximum follows the checkpoint's longest stop: 0.76–2.2 ms with A, 0.50–1.9 ms with D or E, apart from
  one run with a checkpoint of 8.1 ms (x86-64 xfs, 1,024 B, D). At 20,000/s a blocking checkpoint of about 0.7 ms delays
  about 14 records per second, so it reaches p99.9 only in some runs: x86-64 ext4 A had a p99.9 of 2.6 µs in one run and
  86 µs in the other.
- **Page faults.** Writing the window back write-protects it, and the helper does not advise it again, so the writer
  takes those faults itself: A against E, 33–36 more faults per checkpoint on the writer's thread at every rate (for
  example 6,513–6,527 against 5,718–5,730 at 200,000/s on aarch64, and 4,893–5,309 without checkpoints, G). At 20,000/s
  of 64 B, where a checkpoint comes every 2 MB, the default writer took 1,307–1,358 faults against 1,435–1,491 with
  prefaulting off and 435–502 with the ranged sync: those checkpoints undo most of what prefaulting saves there. The
  append itself stays short: append p99.9 at 64 B was 1.8–5.5 µs with A, 4.4–12 µs with D and 1.4–3.7 µs with E; at
  1,024 B 5.7–11, 14–42 and 6.2–12 µs.
- A 4 MiB window (F, x86-64) made every checkpoint 2.7–8.2 ms (median) and the reader's p99.9 1.9–7.6 ms, and the writer
  took 11,294–30,317 faults.

### What these figures do not show

- **Background checkpoints** (`request_checkpoint`) were not run. The same `fdatasync` runs there on the checkpoint
  thread, so the window's time would move from the writer's stop to the time until the checkpoint is durable; not
  measured.
- **A device with a volatile write cache.** This SSD takes no cache flushes; on a drive that does, each extra journal
  commit also waits for a flush. Bare metal, other filesystems and other kernels were not run; macOS not at all.
- Runs of 20 seconds, two untraced per cell; the per-call figures come from one traced run per cell.
- Where E's extra bytes outside the checkpoint come from (above), and why the x86-64 checkpoints vary so much from run
  to run at 20,000/s.

### A cheaper sync, measured and not built

On Linux `msync(MS_SYNC)` is `vfs_fsync_range` over the range with `datasync` set: ext4 and xfs write and wait for the
range's dirty pages, commit the metadata needed to read it, and flush the device cache, so the whole-file `fdatasync`
that follows writes nothing for the records, only what lies outside the range: the prefaulted window. Syncing the active
segment with its ranged `msync` alone (configuration E: neither the per-segment `fdatasync` nor the barrier `fdatasync`
of that segment) wrote 256 KiB less per checkpoint, brought the checkpoint back to the time it takes without the advice
(median per run: aarch64 0.34–0.52 against 0.50–0.73 ms; x86-64 0.37–0.75 against 0.53–1.2 ms), removed the 33–36 faults
per checkpoint, and kept prefaulting's shorter appends. It changes what a checkpoint promises (C3 and the barrier step
of `crash-contract.md`), so it needs a decision and review of its own before it is built; on a drive with a volatile
write cache it would also have to be shown to flush as the barrier does. The barrier `fdatasync` itself costs almost
nothing (0.7–1.3 µs median).

## 2026-11-26, the writer's remaining page faults with prefaulting on (Google Cloud VMs, Linux)

The owner's question after the comparison's notes on Chronicle Queue's `microTouch()` (`compare/README.md` → "No
Chronicle pretoucher"): can tickertape's own prefaulting be made better? This measures which page faults the writer's
thread still takes with the defaults, where they come from, how much latency they explain, and what each alternative
does to them. Issue #60; the decision record is ADR [0058](decisions/0058-prefault-residual-faults.md) (proposed).
**Cloud virtual machines with local NVMe SSD, not bare metal**; Linux only. The raw output (results, traces, scripts) is
kept outside the repository.

**Answer.** With the helper on, the writer's thread took 8.3–10.0 page faults per MiB written on x86-64 (12.3–18.6 on
aarch64) at 64 B × 1,000,000/s and at 1,024 B × 100,000/s, fewer at higher rates (9.6–23 with prefaulting off). Almost
none of them were pages the helper had not reached: they were pages it had made writable that a **write-back had made
read-only again**. Early write-back writes back whole page-cache folios (128 KiB here), including the one the writer is
filling: 89–95 % of the faults at the lower rates. Checkpoints, which also write back the 256 KiB window (PR
#102), caused most of the rest, and the first pages of each new segment 1–2 %. They cost 1–3 µs each, but enough take
20–100 µs to set the writer's p99.9: with early write-back stopped at the 128 KiB boundary below the writer, the p99.9
fell from 15–34 to 2.7–3.2 µs on aarch64 (64 B and 1,024 B, ext4 and xfs) and from 26 to 4.5–7.5 µs on x86-64 at 1,024 B
× 100,000/s, with p99.99, maxima, the reader and the checkpoint's durable time unchanged or lower. The tail beyond that
is rolls and checkpoints. A touch on the writer's thread, the way `microTouch()` does it, changed nothing: while the
writer waits it either finds nothing to do or waits as long as the fault would. A larger window, re-touching after
write-back and early write-back further behind made things worse or no better. The ranged sync of the active segment (PR
#102's experiment) saves the checkpoint's 256 KiB write; on its own it moved the writer's latency little. The same
rewrite of the writer's folio is the 7 % of extra device bytes PR #105 found at steady-90 on aarch64: with the boundary
the device wrote 0.3 % more than appended instead of 8.6 %, and the 140 ms stalls there were gone, with checkpoints
durable after 1 ms. ADR 0058 proposes the boundary and the ranged sync, both off by default until the measurements it
lists.

### Environment and commands

| Machine | CPU, vCPUs, memory | Zone | Time (UTC) | Used for |
|---|---|---|---|---|
| `c4-standard-8-lssd` | Intel Xeon 6985P-C, 8 (4 cores), 31.5 GB | us-east1-b | 09:26–11:41 | x86-64: everything |
| `c4a-standard-8-lssd` | Google Axion (Neoverse-V2), 8, 31 GB | us-east1-b | 11:41–13:12 | aarch64 |
| `c4a-standard-8-lssd` (a second VM) | the same | us-east1-b | 13:14–13:45 | aarch64: the extra bytes at steady-90 |
| `c4-standard-8-lssd` (a second VM) | the same as the first | us-east1-b | 13:46–14:04 | x86-64: the extra bytes at steady-90 |

Ubuntu 24.04, kernel 7.0.0-1017-gcp, rustc 1.100.0, bpftrace 0.20.2, 4 KiB pages, transparent huge pages `madvise`. One
local NVMe SSD (375 GB), formatted for each filesystem in turn: ext4 (`mkfs.ext4 -E
lazy_itable_init=0,lazy_journal_init=0`, 1 GiB journal), then xfs with defaults, both mounted `noatime`; scheduler
`none`; `write_cache` "write through" on every disk of both VMs. Write-back sysctls at their defaults. Results on the
boot disk.

Source: the branch `exp/prefault-residual` (never merged; ADR 0058 → "Where the experiment code is" has the build
commands). The library switches are not in `main`; there they are compiled only with `--cfg tickertape_pf_exp`, or with
`--cfg tickertape_pf_exp --cfg tickertape_pf_touch_append` for the touch in `append`. The x86-64 matrix was built from
commit `8e691d3b` (`42b2307e` plus the switches), the aarch64 VMs, the x86-64 supplement, the W2 check and the steady-90
byte counts from `7acd8f86` (two switches more: `TT_PF_WB_ALIGN_KIB`, `TT_PF_RANGED`), each sent to the VMs as a source
archive whose `git diff 42b2307e` hash was recorded there. The default configuration ran a build without either `--cfg`,
which compiles none of the switches. The `tt-faults` in `main` is the same tool without its `--touch idle` option, which
needs one of the switches. Binaries' sha256 (x86-64): `tt-faults` `79f769e9…` (default), `559b4974…` (switches),
`6cfe3337…` (switches, with the last two), `f80156ad…` (touch in `append`); aarch64: `e4e36bca…` (default), `3c83f66b…`
(switches, all of them), `7a309a14…` (touch in `append`).

```sh
# TT_PF_… and --touch need a build of the branch exp/prefault-residual (above); the rest runs on main.
[env TT_PF_…] tt-faults --dir … --rate <rate> --payload <64|1024> --seconds 20 --warmup-seconds 3 \
  --checkpoint-ms 1000 --writer-cpu <w> --reader-cpu <r> --helper-cpus <h> --log appends.txt \
  [--no-prefault | --no-early-writeback | --spare-segment | --touch idle]
```

- **The tool.** `tt-faults` (new, `benches/src/bin/tt-faults.rs`) appends on its main thread at a fixed rate, scheduled
  on the wall clock as `tt-latency` does; an append's latency runs from its scheduled send to `Writer::append`
  returning, so a stall also counts in the appends it delays. It reads the thread's minor and major fault counts
  (`getrusage(RUSAGE_THREAD)`) before the wait and after the append (outside the timed part unless the append is late),
  requests a background checkpoint every second, runs a zero-copy reader that busy-polls on a CPU of its own and
  measures the visible latency, and logs every append that faulted or took 20 µs or more, with its offset in the
  segment. "Late sends" are sends that started more than 1 µs after their time.
- **Cells.** 64 B at 1,000,000 records/s (about 100 MB/s with headers); 1,024 B at 100,000 (the README's latency rate),
  170,000 (half of these machine types' probe, the steady-50 rate of "roll and journal stalls" above) and 300,000/s
  (about 90 % of it, near what the device sustains). 128 MiB segments: 13–45 rolls per run.
- **Configurations.** The default (prefault on, early write-back on, no spare), twice per round as the noise floor
  ("default" in the tables pools both). `prefault(false)`; `early_writeback(false)`; `spare_segment(true)`. Switches
  (branch `exp/prefault-residual`, `--cfg tickertape_pf_exp`): `TT_PF_WINDOW_KIB` (window of 1, 4 or 16 MiB);
  `TT_PF_RETOUCH` (re-touch: at every wake-up the helper prefaults again from the writer's page, so pages written back
  ahead of the writer are made writable again); `TT_PF_WB_LAG_KIB` and `TT_PF_WB_ALIGN_KIB` (early write-back stops that
  far behind the writer, rounded down to that alignment: "2–4 MiB behind" is 2,048 KiB rounded to 2 MiB, "to the 128 KiB
  boundary" is 1 KiB rounded to 128 KiB, so that it never reaches the 128 KiB block the writer is in); `TT_PF_RANGED`
  (ranged sync: the active segment is synced by its ranged `msync` alone, without the whole-file `fdatasync` and the
  barrier, as PR #102's `TT_EXP_RANGED`); `Writer::exp_touch` called once per wait (writer touch while waiting: an
  atomic `fetch_or(0)` on the commit word slot at the write position and on the first word of the next page, so what a
  `Writer::idle()` could do; `tt-faults --touch idle` on the branch). `--cfg tickertape_pf_touch_append`, also on the
  branch: the writer, when it enters a new page, touches the next one at the end of `append` (writer touch in `append`,
  as `microTouch()` but without waiting).
- **Rounds.** Configurations and cells rotated from round to round; every journal deleted and `sync`ed before the next
  run. x86-64: three rounds on ext4 and two on xfs, the last of each traced; the latency tables use the untraced rounds
  only. A supplement on x86-64 ext4 ran six configurations three times, untraced. aarch64: three rounds on ext4 and two
  on xfs (five configurations), the last of each traced.
- **Tracing.** `benches/diagnostics/writer-faults.bt` (new) prints every fault of the writer's thread with its time and
  duration, whether `filemap_fault` ran (the page was not in the page tables: "not present") or not (the page was mapped
  but not writable: "write-protected"), whether it started a journal handle (`jbd2__journal_start`) or an xfs
  transaction, and the start and duration of every `msync`, `fdatasync` and `sync_file_range` of the checkpoint and
  early write-back threads; at the end, the size of the folios the run added to the page cache and who cleaned folios
  for write-back. A write-protected fault counts as "after a checkpoint" when a checkpoint's `msync` or `fdatasync` had
  started before it and ended less than 5 ms before it, otherwise "after early write-back". Tracing raised the default's
  p99.9 by 1.2–2.6 times (157 against 60–100 µs at 64 B, 31–38 against 26–27 at 1,024 B × 100,000/s): the traced runs
  are used for counts and kinds, not for latencies.
- One untraced x86-64 run (ext4, round 1, 1,024 B × 100,000/s, prefault off) had a test trace attached for 8 s; its
  p99.9 is the higher of the two in its row.

### Where the writer's faults are

The traced default runs (two per cell): faults of the writer's thread on the segment mappings per MiB written, by kind,
with the median and the 99th percentile of their duration in µs; then the appends of 20 µs or more and of 100 µs or
more, by the first writer fault of their episode (the run of appends one stall delays back to back). "New segment"
episodes are rolls: their first fault is in the first 256 KiB of a segment, and they include the roll's own system
calls.

x86-64, ext4:

| Cell | Run | All | After early write-back | After a checkpoint | New segment (not present) | Other not present | All, prefault off (untraced) |
|---|---|---|---|---|---|---|---|
| 64 B, 1,000,000/s | 1 | 7.97 | 7.14 (1.1 / 39.5) | 0.74 (1.1 / 76.0) | 0.09 (2.7 / 82.1) | 0.00 (45.8 / 50.5) | 22.1–23.1 |
| 64 B, 1,000,000/s | 2 | 8.11 | 7.21 (1.1 / 38.9) | 0.79 (1.0 / 84.3) | 0.09 (2.5 / 82.6) | 0.01 (19.7 / 90.1) | 22.1–23.1 |
| 1,024 B, 100,000/s | 1 | 9.28 | 8.35 (1.1 / 31.4) | 0.77 (1.1 / 86.4) | 0.15 (2.4 / 73.8) | 0.00 (44.8 / 3.00 ms) | 9.7 |
| 1,024 B, 100,000/s | 2 | 9.33 | 8.40 (1.1 / 31.8) | 0.77 (1.1 / 102.4) | 0.16 (2.4 / 74.8) | 0.00 (31.5 / 46.8) | 9.7 |
| 1,024 B, 170,000/s | 1 | 1.88 | 1.13 (1.3 / 18.4) | 0.50 (1.0 / 99.5) | 0.25 (2.2 / 74.4) | 0.00 (32.3 / 140.6) | 9.7 |
| 1,024 B, 170,000/s | 2 | 1.92 | 1.17 (1.3 / 17.3) | 0.50 (1.1 / 90.8) | 0.25 (2.3 / 77.7) | 0.00 (15.2 / 27.7) | 9.7 |
| 1,024 B, 300,000/s | 1 | 0.55 | 0.01 (0.8 / 5.0) | 0.27 (1.1 / 97.0) | 0.27 (2.3 / 77.6) | 0.01 (18.0 / 178.7) | 9.6 |
| 1,024 B, 300,000/s | 2 | 0.57 | 0.01 (0.8 / 2.6) | 0.28 (1.1 / 94.4) | 0.27 (2.3 / 78.6) | 0.02 (18.6 / 50.1) | 9.6 |

Appends of at least 20 µs / at least 100 µs, by the first writer fault of their episode:

| Cell | Run | After early write-back | After a checkpoint | New segment | Other not present | No writer fault |
|---|---|---|---|---|---|---|
| 64 B, 1,000,000/s | 1 | 73,744 / 4,956 | 17,543 / 9,276 | 16,920 / 14,306 | 702 / 0 | 5,980 / 3,559 |
| 64 B, 1,000,000/s | 2 | 80,571 / 9,882 | 18,235 / 9,454 | 16,018 / 13,509 | 1,466 / 30 | 14,487 / 2,808 |
| 1,024 B, 100,000/s | 1 | 2,213 / 28 | 513 / 103 | 575 / 377 | 859 / 800 | 3 / 0 |
| 1,024 B, 100,000/s | 2 | 2,193 / 33 | 808 / 380 | 565 / 374 | 17 / 0 | 20 / 0 |
| 1,024 B, 170,000/s | 1 | 232 / 0 | 1,573 / 516 | 2,618 / 1,482 | 39 / 11 | 35 / 0 |
| 1,024 B, 170,000/s | 2 | 151 / 0 | 1,387 / 413 | 2,517 / 1,381 | 33 / 0 | 18 / 0 |
| 1,024 B, 300,000/s | 1 | 0 / 0 | 4,217 / 1,985 | 10,903 / 9,417 | 353 / 61 | 195 / 0 |
| 1,024 B, 300,000/s | 2 | 0 / 0 | 4,450 / 1,985 | 10,636 / 9,136 | 464 / 0 | 485 / 0 |

x86-64, xfs:

| Cell | Run | All | After early write-back | After a checkpoint | New segment (not present) | Other not present | All, prefault off (untraced) |
|---|---|---|---|---|---|---|---|
| 64 B, 1,000,000/s | 1 | 8.27 | 7.49 (0.8 / 36.7) | 0.70 (0.8 / 72.6) | 0.08 (2.2 / 86.3) | 0.00 (51.4 / 54.7) | 22.9–23.2 |
| 64 B, 1,000,000/s | 2 | 8.27 | 7.48 (0.8 / 36.7) | 0.71 (0.8 / 72.0) | 0.08 (2.2 / 85.8) | 0.00 (49.5 / 52.5) | 22.9–23.2 |
| 1,024 B, 100,000/s | 1 | 9.71 | 8.81 (0.9 / 31.0) | 0.74 (0.8 / 74.5) | 0.15 (2.2 / 82.0) | 0.00 (33.5 / 33.5) | 9.7 |
| 1,024 B, 100,000/s | 2 | 9.53 | 8.65 (0.9 / 31.3) | 0.73 (0.8 / 68.0) | 0.15 (2.1 / 83.9) | 0.00 (27.9 / 47.8) | 9.7 |
| 1,024 B, 170,000/s | 1 | 2.22 | 1.49 (1.1 / 14.0) | 0.49 (0.8 / 88.0) | 0.24 (2.1 / 84.9) | 0.00 (16.6 / 16.6) | 9.7 |
| 1,024 B, 170,000/s | 2 | 2.30 | 1.57 (1.0 / 14.3) | 0.49 (0.8 / 90.5) | 0.24 (2.1 / 79.0) | 0.00 (12.2 / 41.1) | 9.7 |
| 1,024 B, 300,000/s | 1 | 0.59 | 0.00 (0.9 / 1.2) | 0.26 (0.8 / 87.0) | 0.27 (2.1 / 76.4) | 0.05 (9.4 / 44.6) | 9.6–9.7 |
| 1,024 B, 300,000/s | 2 | 0.54 | 0.01 (0.8 / 2.0) | 0.26 (0.8 / 91.7) | 0.27 (2.1 / 77.9) | 0.01 (11.8 / 26.6) | 9.6–9.7 |

Appends of at least 20 µs / at least 100 µs, by the first writer fault of their episode:

| Cell | Run | After early write-back | After a checkpoint | New segment | Other not present | No writer fault |
|---|---|---|---|---|---|---|
| 64 B, 1,000,000/s | 1 | 58,822 / 2,421 | 14,014 / 6,850 | 16,578 / 14,077 | 558 / 0 | 386 / 0 |
| 64 B, 1,000,000/s | 2 | 56,659 / 2,472 | 13,150 / 6,155 | 16,859 / 14,336 | 676 / 0 | 777 / 0 |
| 1,024 B, 100,000/s | 1 | 2,011 / 0 | 505 / 120 | 545 / 351 | 49 / 37 | 9 / 0 |
| 1,024 B, 100,000/s | 2 | 1,968 / 0 | 547 / 164 | 541 / 348 | 43 / 0 | 11 / 0 |
| 1,024 B, 170,000/s | 1 | 104 / 0 | 1,231 / 461 | 2,422 / 1,305 | 8 / 0 | 13 / 0 |
| 1,024 B, 170,000/s | 2 | 104 / 0 | 1,257 / 430 | 2,429 / 1,328 | 27 / 0 | 7 / 0 |
| 1,024 B, 300,000/s | 1 | 0 / 0 | 3,717 / 1,493 | 9,742 / 8,275 | 620 / 7 | 58 / 0 |
| 1,024 B, 300,000/s | 2 | 0 / 0 | 3,720 / 1,560 | 9,769 / 8,276 | 77 / 0 | 55 / 0 |

aarch64, ext4 (the same 128 KiB folios; the writer faults on more of the pages of each):

| Cell | Run | All | After early write-back | After a checkpoint | New segment (not present) | Other not present | All, prefault off (untraced) |
|---|---|---|---|---|---|---|---|
| 64 B, 1,000,000/s | 1 | 12.36 | 11.57 (2.4 / 24.5) | 0.72 (2.4 / 25.7) | 0.07 (2.7 / 50.2) | 0.00 (33.1 / 34.0) | 22.6–23.1 |
| 64 B, 1,000,000/s | 2 | 12.31 | 11.52 (3.0 / 25.1) | 0.72 (2.8 / 27.8) | 0.06 (3.1 / 50.0) | 0.00 (31.5 / 70.1) | 22.6–23.1 |
| 1,024 B, 100,000/s | 1 | 18.00 | 17.10 (2.4 / 20.1) | 0.73 (2.3 / 26.0) | 0.11 (2.4 / 48.6) | 0.06 (20.7 / 29.1) | 9.7 |
| 1,024 B, 100,000/s | 2 | 18.07 | 17.17 (2.4 / 20.0) | 0.74 (2.4 / 30.1) | 0.11 (2.4 / 42.9) | 0.05 (19.1 / 33.5) | 9.7 |
| 1,024 B, 170,000/s | 1 | 14.01 | 13.24 (2.5 / 19.6) | 0.54 (2.5 / 29.4) | 0.16 (2.7 / 42.0) | 0.05 (12.6 / 26.5) | 9.7 |
| 1,024 B, 170,000/s | 2 | 14.16 | 13.38 (2.4 / 19.5) | 0.55 (2.4 / 28.4) | 0.17 (2.2 / 48.1) | 0.06 (11.7 / 28.0) | 9.7 |
| 1,024 B, 300,000/s | 1 | 6.48 | 5.90 (2.7 / 13.4) | 0.32 (2.6 / 25.4) | 0.17 (2.7 / 46.8) | 0.08 (11.4 / 804.8) | 9.7 |
| 1,024 B, 300,000/s | 2 | 7.14 | 2.96 (2.5 / 13.1) | 3.88 (2.5 / 13.9) | 0.18 (2.5 / 45.2) | 0.11 (12.0 / 639.6) | 9.7 |

Appends of at least 20 µs / at least 100 µs, by the first writer fault of their episode:

| Cell | Run | After early write-back | After a checkpoint | New segment | Other not present | No writer fault |
|---|---|---|---|---|---|---|
| 64 B, 1,000,000/s | 1 | 237,046 / 16,998 | 17,486 / 1,430 | 12,482 / 9,039 | 803 / 0 | 0 / 0 |
| 64 B, 1,000,000/s | 2 | 443,682 / 25,267 | 34,349 / 8,291 | 13,315 / 9,809 | 3,345 / 0 | 0 / 0 |
| 1,024 B, 100,000/s | 1 | 2,516 / 17 | 169 / 15 | 507 / 335 | 353 / 0 | 0 / 0 |
| 1,024 B, 100,000/s | 2 | 2,484 / 18 | 196 / 17 | 491 / 323 | 272 / 0 | 0 / 0 |
| 1,024 B, 170,000/s | 1 | 7,936 / 46 | 772 / 101 | 2,049 / 1,357 | 832 / 0 | 33 / 0 |
| 1,024 B, 170,000/s | 2 | 7,470 / 33 | 699 / 108 | 1,770 / 1,143 | 829 / 0 | 15 / 0 |
| 1,024 B, 300,000/s | 1 | 36,284 / 262 | 5,522 / 3,061 | 7,850 / 6,235 | 11,617 / 5,233 | 1 / 0 |
| 1,024 B, 300,000/s | 2 | 17,610 / 165 | 24,147 / 2,200 | 7,792 / 6,179 | 11,203 / 4,452 | 0 / 0 |

aarch64, xfs (prefault off was not run on xfs there):

| Cell | Run | All | After early write-back | After a checkpoint | New segment (not present) | Other not present | All, prefault off (untraced) |
|---|---|---|---|---|---|---|---|
| 64 B, 1,000,000/s | 1 | 12.38 | 11.61 (2.5 / 25.4) | 0.71 (2.5 / 25.9) | 0.06 (3.0 / 54.1) | 0.00 (28.4 / 40.7) | – |
| 64 B, 1,000,000/s | 2 | 12.43 | 11.66 (2.1 / 24.3) | 0.70 (2.1 / 28.6) | 0.06 (2.6 / 53.5) | 0.00 (32.6 / 36.5) | – |
| 1,024 B, 100,000/s | 1 | 18.06 | 17.19 (2.2 / 18.6) | 0.72 (2.2 / 24.3) | 0.10 (2.5 / 51.2) | 0.05 (21.0 / 33.9) | – |
| 1,024 B, 100,000/s | 2 | 18.22 | 17.35 (2.1 / 18.0) | 0.72 (2.0 / 27.7) | 0.10 (2.2 / 48.9) | 0.05 (19.4 / 29.0) | – |
| 1,024 B, 170,000/s | 1 | 14.69 | 13.99 (2.1 / 17.6) | 0.49 (2.0 / 25.2) | 0.15 (2.1 / 50.0) | 0.06 (17.8 / 27.5) | – |
| 1,024 B, 170,000/s | 2 | 14.37 | 13.66 (2.2 / 18.0) | 0.51 (2.1 / 31.7) | 0.15 (2.2 / 48.0) | 0.06 (11.3 / 32.0) | – |
| 1,024 B, 300,000/s | 1 | 7.85 | 4.40 (2.2 / 13.8) | 3.19 (2.2 / 14.1) | 0.20 (2.0 / 46.9) | 0.07 (7.3 / 23.0) | – |
| 1,024 B, 300,000/s | 2 | 8.05 | 3.79 (2.1 / 14.1) | 3.99 (2.1 / 13.7) | 0.19 (2.0 / 48.6) | 0.07 (7.4 / 24.3) | – |

Appends of at least 20 µs / at least 100 µs, by the first writer fault of their episode:

| Cell | Run | After early write-back | After a checkpoint | New segment | Other not present | No writer fault |
|---|---|---|---|---|---|---|
| 64 B, 1,000,000/s | 1 | 261,130 / 18,714 | 18,593 / 604 | 11,310 / 7,784 | 4,088 / 177 | 0 / 0 |
| 64 B, 1,000,000/s | 2 | 219,478 / 15,163 | 15,337 / 18 | 11,661 / 8,105 | 2,036 / 0 | 161 / 0 |
| 1,024 B, 100,000/s | 1 | 2,301 / 19 | 162 / 13 | 455 / 275 | 285 / 0 | 0 / 0 |
| 1,024 B, 100,000/s | 2 | 1,961 / 21 | 156 / 13 | 424 / 253 | 184 / 0 | 13 / 0 |
| 1,024 B, 170,000/s | 1 | 6,180 / 79 | 444 / 90 | 1,432 / 839 | 742 / 0 | 0 / 0 |
| 1,024 B, 170,000/s | 2 | 6,517 / 103 | 662 / 163 | 1,487 / 865 | 660 / 0 | 0 / 0 |
| 1,024 B, 300,000/s | 1 | 13,337 / 88 | 10,195 / 76 | 6,318 / 4,673 | 1,834 / 0 | 0 / 0 |
| 1,024 B, 300,000/s | 2 | 11,672 / 91 | 11,156 / 137 | 6,220 / 4,575 | 1,893 / 0 | 0 / 0 |

- **Large folios.** The page cache built the segments almost entirely from 128 KiB folios (order 5: 18,617 of 19,422
  folios added in a 1,024 B × 100,000/s run), and write-back cleans whole folios. The early write-back thread asks for
  the pages before the one that holds the writer's position, and so also writes back, and write-protects, the rest of
  the folio the writer is in, together with the pages the helper has prefaulted in it. The writer then takes a
  write-protect fault on each page of it that it writes to. In the same run the early write-back thread cleaned 21,464
  folios and the checkpoint thread 253.
- **Rate.** On x86-64 at 300,000/s the early write-back faults vanish: by the time a write-back of 1 MiB has been
  submitted (about 0.6 ms per call there), the writer has left the folio. At the lower rates it has not, and on aarch64
  it has not at 300,000/s either (3.0–5.9 such faults per MiB).
- **The helper kept ahead.** Not-present faults outside the first 256 KiB of a segment were 0.00–0.05 per MiB in every
  cell, also near the device's limit: the window did not run short.
- **Cost.** One write-protected fault in eight after early write-back started a journal handle (ext4 updating the file's
  modification time; xfs allocating a transaction for the same); that is where the slow ones wait (issue #60 has the
  mechanism). At 1,024 B × 100,000/s the early write-back faults began 53–61 % of the appends of 20 µs or more (that
  p99.9 is 26 µs) and 2–4 % of those of 100 µs or more; the rest are checkpoints and rolls. At 300,000/s, 82 % of the
  appends of 100 µs or more were in roll episodes. On aarch64 the early write-back faults began 68–90 % of the appends
  of 20 µs or more at the three lower rates, on both filesystems.

### The alternatives

Untraced runs. µs unless a cell says ms; ranges over the runs; the default's range is the noise floor. Where two runs of
one configuration differ by more than the difference to the default, the configuration did not change that figure.

x86-64, ext4:

64 B, 1,000,000/s:

| Configuration | Runs | Faults/MiB | p99 | p99.9 | p99.99 | max | Reader p99.9 | Late sends (%) |
|---|---|---|---|---|---|---|---|---|
| default | 4 | 8.25–8.43 | 3.33–4.17 | 59.5–100 | 246–332 | 435–685 | 87.6–133 | 1.8–2.1 |
| `prefault(false)` | 2 | 22.1–22.9 | 2.4–2.7 ms | 3.7–4.0 ms | 4.6 ms | 4.7 ms | 3.7–4.0 ms | 10.5 |
| `early_writeback(false)` | 2 | 0.21–0.22 | 1.2 ms | 2.0–2.8 ms | 2.7–3.4 ms | 2.8–3.5 ms | 2.3–2.8 ms | 4.9 |
| re-touch | 2 | 1.36–1.38 | 3.81–3.92 | 98.0–130 | 289–626 | 714 µs to 1.6 ms | 124–187 | 2.0–2.1 |
| 4 MiB window | 2 | 19.6–19.8 | 28.7–40.5 | 622–906 | 1.3–1.6 ms | 1.7–1.9 ms | 710–933 | 2.7–2.9 |
| writer touch while waiting | 2 | 8.31–8.44 | 3.38–4.92 | 78.4 µs to 1.7 ms | 255 µs to 5.5 ms | 367 µs to 5.9 ms | 111 µs to 1.7 ms | 2.0–2.4 |
| write-back 2–4 MiB behind | 2 | 1.34–1.36 | 3.24–3.25 | 485–514 | 970 µs to 1.1 ms | 1.2–1.6 ms | 1.0 ms | 1.8 |
| `spare_segment(true)` | 2 | 8.17–8.30 | 3.10–3.89 | 41.1–47.5 | 170–186 | 354–473 | 94.3–112 | 1.9–2.1 |

1,024 B, 100,000/s:

| Configuration | Runs | Faults/MiB | p99 | p99.9 | p99.99 | max | Reader p99.9 | Late sends (%) |
|---|---|---|---|---|---|---|---|---|
| default | 4 | 9.31–9.59 | 2.18–4.52 | 25.7–27.4 | 122–156 | 260–326 | 40.1–51.8 | 0.7–1.7 |
| `prefault(false)` | 2 | 9.75 | 24.0–25.6 | 140–237 | 921–999 | 1.4–1.5 ms | 321–412 | 2.8–3.4 |
| `early_writeback(false)` | 2 | 0.09–0.10 | 4.21–5.60 | 6.54–8.15 | 87.2–89.5 | 211–220 | 1.2 ms | 1.5–1.6 |
| re-touch | 2 | 3.13–3.17 | 3.27–6.25 | 28.4–29.7 | 133–161 | 281–984 | 50.3–74.0 | 0.8–1.8 |
| 4 MiB window | 2 | 20.4 | 3.98–6.58 | 368–397 | 1.1–1.3 ms | 1.4–1.8 ms | 537–590 | 1.1 |
| writer touch while waiting | 2 | 9.32–9.39 | 2.36–3.38 | 25.4–26.7 | 130–168 | 317–324 | 38.6–49.1 | 0.8–1.7 |
| write-back 2–4 MiB behind | 2 | 1.31 | 2.15–2.43 | 82.4–84.9 | 783–930 | 1.2–1.3 ms | 774 µs to 1.1 ms | 0.6–1.6 |
| `spare_segment(true)` | 2 | 8.95–9.29 | 2.33–5.75 | 23.7–25.8 | 86.9 µs to 1.6 ms | 277 µs to 3.3 ms | 40.8–75.6 | 1.6–1.7 |

1,024 B, 170,000/s:

| Configuration | Runs | Faults/MiB | p99 | p99.9 | p99.99 | max | Reader p99.9 | Late sends (%) |
|---|---|---|---|---|---|---|---|---|
| default | 4 | 2.02–2.19 | 2.21–2.48 | 16.6–47.3 | 151 µs to 1.6 ms | 228 µs to 3.3 ms | 197–416 | 1.2–1.5 |
| `prefault(false)` | 2 | 9.67–9.68 | 26.1–26.5 | 429–455 | 1.1–1.7 ms | 1.5–3.2 ms | 697–718 | 5.2–5.5 |
| `early_writeback(false)` | 2 | 0.16–0.17 | 2.31 | 26.5–37.4 | 152–922 | 493 µs to 2.5 ms | 167–411 | 1.5 |
| re-touch | 2 | 1.24–1.25 | 2.42–2.49 | 16.3–18.1 | 141–157 | 428–560 | 186–226 | 1.5 |
| 1 MiB window | 2 | 3.23–3.25 | 2.09–2.13 | 56.5–63.7 | 321–337 | 453–474 | 267–282 | 1.1–1.2 |
| 4 MiB window | 2 | 9.79–10.2 | 2.73–5.42 | 605–635 | 1.4–1.5 ms | 1.9–2.0 ms | 739 | 1.9–2.1 |
| 16 MiB window | 2 | 34.0–35.3 | 13.8–43.8 | 695–858 | 1.3–1.6 ms | 1.7–2.1 ms | 786–890 | 2.2–2.4 |
| writer touch while waiting | 2 | 2.08–2.11 | 2.44–2.46 | 14.9–21.1 | 150 µs to 1.0 ms | 236 µs to 2.7 ms | 201–339 | 1.5 |
| writer touch in `append` | 2 | 2.07–2.14 | 2.17–2.49 | 14.8–30.7 | 151–921 | 246 µs to 2.3 ms | 209–372 | 1.4–1.5 |
| write-back 2–4 MiB behind | 2 | 0.77–0.78 | 2.14–2.27 | 32.5–37.3 | 481 µs to 2.4 ms | 1.3–3.4 ms | 1.4–1.5 ms | 1.1–1.5 |
| `spare_segment(true)` | 2 | 1.88 | 2.38–2.63 | 9.37–509 | 93.7 µs to 8.8 ms | 188 µs to 11 ms | 124–846 | 1.5 |

1,024 B, 300,000/s:

| Configuration | Runs | Faults/MiB | p99 | p99.9 | p99.99 | max | Reader p99.9 | Late sends (%) |
|---|---|---|---|---|---|---|---|---|
| default | 4 | 0.58–0.62 | 2.31–3.63 | 86.4–172 | 176 µs to 5.3 ms | 240 µs to 6.7 ms | 509–785 | 2.0–2.6 |
| `prefault(false)` | 2 | 9.60–9.64 | 43.8–44.6 | 530–590 | 977 µs to 1.1 ms | 1.3 ms | 765–778 | 11.9–12.3 |
| `early_writeback(false)` | 2 | 0.84–0.85 | 22.8–24.4 | 91.5–118 | 499–886 | 910 µs to 1.9 ms | 537–542 | 3.3–3.6 |
| re-touch | 2 | 0.37–0.38 | 2.53–2.81 | 88.4–128 | 185–233 | 281–282 | 554–601 | 2.2–2.3 |
| 4 MiB window | 2 | 5.81–5.98 | 4.12–4.28 | 440–560 | 811 µs to 1.2 ms | 929 µs to 2.1 ms | 701–723 | 2.5–2.6 |
| writer touch while waiting | 2 | 0.61–0.62 | 2.73–3.94 | 136 µs to 3.0 ms | 225 µs to 7.9 ms | 276 µs to 9.0 ms | 598 µs to 3.0 ms | 2.5 |
| write-back 2–4 MiB behind | 2 | 0.96–0.97 | 30.8–30.9 | 100–104 | 447–478 | 693 µs to 1.2 ms | 1.8–1.9 ms | 3.8 |
| `spare_segment(true)` | 2 | 0.30–0.32 | 2.22–2.58 | 8.13–9.21 | 116–135 | 197–198 | 442–450 | 1.9–2.3 |

x86-64, xfs (one untraced run per configuration, two of the default):

64 B, 1,000,000/s:

| Configuration | Runs | Faults/MiB | p99 | p99.9 | p99.99 | max | Reader p99.9 | Late sends (%) |
|---|---|---|---|---|---|---|---|---|
| default | 2 | 8.43–8.45 | 2.77–3.17 | 72.6–73.5 | 236–254 | 374–472 | 107–109 | 1.7–1.8 |
| `prefault(false)` | 1 | 22.9 | 3.3 ms | 5.2 ms | 5.9 ms | 6.2 ms | 5.2 ms | 9.2 |
| `early_writeback(false)` | 1 | 0.19 | 2.2 ms | 4.2 ms | 5.2 ms | 5.4 ms | 4.2 ms | 5.2 |
| re-touch | 1 | 1.19 | 2.60 | 73.8 | 236 | 388 | 107 | 1.7 |
| 4 MiB window | 1 | 19.8 | 6.10 | 531 | 1.5 ms | 2.2 ms | 599 | 2.3 |
| writer touch while waiting | 1 | 8.44 | 3.24 | 70.5 | 275 | 699 | 101 | 1.9 |
| write-back 2–4 MiB behind | 1 | 1.39 | 1.89 | 273 | 608 | 1.1 ms | 790 | 1.5 |
| `spare_segment(true)` | 1 | 8.34 | 2.63 | 38.8 | 146 | 367 | 87.4 | 1.7 |

1,024 B, 100,000/s:

| Configuration | Runs | Faults/MiB | p99 | p99.9 | p99.99 | max | Reader p99.9 | Late sends (%) |
|---|---|---|---|---|---|---|---|---|
| default | 2 | 9.89–9.97 | 2.33–4.22 | 21.6–22.2 | 127–128 | 328–333 | 35.1–35.4 | 0.6–1.6 |
| `prefault(false)` | 1 | 9.75 | 14.6 | 30.7 | 92.7 | 364 | 42.1 | 2.0 |
| `early_writeback(false)` | 1 | 0.09 | 2.02 | 3.92 | 85.4 | 198 | 9.74 | 0.5 |
| re-touch | 1 | 2.86 | 2.29 | 24.8 | 137 | 256 | 44.3 | 0.7 |
| 4 MiB window | 1 | 20.9 | 3.92 | 235 | 1.6 ms | 2.6 ms | 410 | 1.9 |
| writer touch while waiting | 1 | 9.79 | 2.25 | 22.3 | 130 | 309 | 36.4 | 0.7 |
| write-back 2–4 MiB behind | 1 | 1.32 | 5.08 | 7.43 | 133 | 218 | 882 | 1.5 |
| `spare_segment(true)` | 1 | 9.76 | 2.36 | 19.7 | 75.5 | 317 | 33.3 | 1.2 |

1,024 B, 170,000/s:

| Configuration | Runs | Faults/MiB | p99 | p99.9 | p99.99 | max | Reader p99.9 | Late sends (%) |
|---|---|---|---|---|---|---|---|---|
| default | 2 | 2.30–2.36 | 2.19–2.47 | 12.6–13.4 | 153–165 | 248–282 | 190–227 | 1.4–1.5 |
| `prefault(false)` | 1 | 9.68 | 19.8 | 38.0 | 137 | 362 | 405 | 4.2 |
| `early_writeback(false)` | 1 | 0.14 | 2.30 | 15.9 | 208 | 1.0 ms | 243 | 1.4 |
| re-touch | 1 | 1.29 | 2.37 | 12.5 | 160 | 421 | 211 | 1.5 |
| 1 MiB window | 1 | 3.53 | 2.59 | 58.3 | 354 | 491 | 304 | 1.6 |
| 4 MiB window | 1 | 10.1 | 4.66 | 265 | 633 | 873 | 436 | 1.8 |
| 16 MiB window | 1 | 35.3 | 4.63 | 337 | 1.6 ms | 2.7 ms | 505 | 2.1 |
| writer touch while waiting | 1 | 2.34 | 2.47 | 11.9 | 148 | 219 | 184 | 1.5 |
| writer touch in `append` | 1 | 2.33 | 2.47 | 11.7 | 146 | 213 | 193 | 1.5 |
| write-back 2–4 MiB behind | 1 | 0.75 | 2.39 | 12.5 | 157 | 289 | 1.4 ms | 1.4 |
| `spare_segment(true)` | 1 | 2.04 | 2.38 | 7.79 | 146 | 1.6 ms | 207 | 1.4 |

1,024 B, 300,000/s:

| Configuration | Runs | Faults/MiB | p99 | p99.9 | p99.99 | max | Reader p99.9 | Late sends (%) |
|---|---|---|---|---|---|---|---|---|
| default | 2 | 0.57–0.59 | 2.62–2.85 | 79.2–86.3 | 179–183 | 236–268 | 523–538 | 2.0–2.3 |
| `prefault(false)` | 1 | 9.62 | 30.0 | 134 | 596 | 765 | 505 | 9.0 |
| `early_writeback(false)` | 1 | 0.93 | 8.72 | 57.4 | 780 | 1.0 ms | 451 | 2.9 |
| re-touch | 1 | 0.37 | 2.59 | 79.1 | 173 | 267 | 503 | 2.1 |
| 4 MiB window | 1 | 5.93 | 3.33 | 525 | 892 | 1.2 ms | 693 | 2.2 |
| writer touch while waiting | 1 | 0.58 | 2.43 | 78.6 | 170 | 219 | 500 | 1.9 |
| write-back 2–4 MiB behind | 1 | 0.87 | 14.1 | 76.4 | 209 | 754 | 1.7 ms | 2.9 |
| `spare_segment(true)` | 1 | 0.30 | 2.27 | 8.62 | 135 | 905 | 498 | 1.9 |

aarch64, ext4:

64 B, 1,000,000/s:

| Configuration | Runs | Faults/MiB | p99 | p99.9 | p99.99 | max | Reader p99.9 | Late sends (%) |
|---|---|---|---|---|---|---|---|---|
| default | 4 | 12.5–12.6 | 1.65–1.84 | 32.5–34.0 | 175–182 | 249–285 | 38.6–41.1 | 1.2–1.4 |
| `prefault(false)` | 2 | 22.6–22.8 | 13.0–13.2 | 666–712 | 1.3–1.4 ms | 1.5–1.8 ms | 806–822 | 3.5–3.8 |
| `early_writeback(false)` | 2 | 0.30–0.31 | 1.18–1.33 | 5.69–6.65 | 155–159 | 196–199 | 188–241 | 0.8–0.9 |
| ranged sync | 2 | 11.4–12.0 | 1.72–2.13 | 32.1 µs to 12 ms | 182 µs to 18 ms | 409 µs to 19 ms | 37.3 µs to 12 ms | 1.3–1.6 |
| write-back to the 128 KiB boundary | 2 | 0.85 | 0.98–1.05 | 3.17–3.22 | 169–173 | 224–229 | 10.1–11.1 | 0.7 |
| ranged sync + write-back to 128 KiB boundary | 2 | 0.27–0.28 | 1.07–1.10 | 3.00–3.23 | 175–178 | 228–229 | 4.49–5.17 | 0.8 |
| re-touch | 2 | 2.91–3.00 | 1.83–2.02 | 32.3–35.2 | 173–188 | 248–433 | 38.6–41.4 | 1.3–1.5 |
| writer touch while waiting | 2 | 1.56–1.64 | 1.76–1.88 | 34.6–35.4 | 179–183 | 264–354 | 40.2–41.7 | 1.3–1.4 |
| `spare_segment(true)` | 2 | 12.4 | 1.70–1.77 | 29.6–29.7 | 78.0–87.2 | 279–304 | 37.4–37.8 | 1.2–1.3 |

1,024 B, 100,000/s:

| Configuration | Runs | Faults/MiB | p99 | p99.9 | p99.99 | max | Reader p99.9 | Late sends (%) |
|---|---|---|---|---|---|---|---|---|
| default | 4 | 18.3–18.5 | 2.37–2.39 | 18.1–18.7 | 122–133 | 219–234 | 24.8–25.9 | 0.2–1.0 |
| `prefault(false)` | 2 | 9.75 | 12.2–13.2 | 262–276 | 1.1 ms | 1.7–1.8 ms | 453–468 | 1.1–2.0 |
| `early_writeback(false)` | 2 | 0.22 | 2.22–2.30 | 2.62–2.79 | 105–110 | 205–208 | 192–210 | 0.0–1.0 |
| ranged sync | 2 | 17.8–17.9 | 2.37–2.39 | 17.8–18.2 | 122–125 | 217–218 | 24.6–24.7 | 0.2–1.2 |
| write-back to the 128 KiB boundary | 2 | 0.78–0.79 | 2.18–2.22 | 2.65–2.83 | 115–116 | 208–218 | 3.54–4.32 | 0.0 |
| ranged sync + write-back to 128 KiB boundary | 2 | 0.23 | 2.25 | 2.65–2.81 | 118–119 | 209–216 | 3.23–3.50 | 0.0 |
| re-touch | 2 | 5.52–5.56 | 2.25–2.36 | 19.4–19.5 | 121–152 | 221–702 | 26.5–27.4 | 0.2 |
| writer touch while waiting | 2 | 2.05–2.07 | 2.33–2.36 | 18.8–19.0 | 120–126 | 213–217 | 25.9–26.0 | 0.2 |
| `spare_segment(true)` | 2 | 18.3 | 2.25 | 16.8–17.0 | 34.0–34.4 | 207–237 | 23.5–23.8 | 0.2 |

1,024 B, 170,000/s:

| Configuration | Runs | Faults/MiB | p99 | p99.9 | p99.99 | max | Reader p99.9 | Late sends (%) |
|---|---|---|---|---|---|---|---|---|
| default | 4 | 14.5–14.7 | 2.40–2.79 | 21.8–22.3 | 165–171 | 224–258 | 124–137 | 0.4–0.9 |
| `prefault(false)` | 2 | 9.68 | 13.1–13.9 | 294–425 | 1.0–1.1 ms | 1.3–1.4 ms | 577–626 | 2.2–2.5 |
| `early_writeback(false)` | 2 | 0.11–0.12 | 2.10 | 2.67–3.16 | 159–166 | 233–237 | 80.5–93.9 | 0.1 |
| ranged sync | 2 | 14.2–14.3 | 2.35–2.36 | 21.0–22.0 | 164–173 | 227–242 | 120–131 | 0.5–0.8 |
| write-back to the 128 KiB boundary | 2 | 0.65–0.66 | 2.14–2.31 | 2.77–2.81 | 171 | 236–242 | 124–131 | 0.1 |
| ranged sync + write-back to 128 KiB boundary | 2 | 0.26 | 2.17–2.55 | 3.17–4.38 | 163–164 | 228–231 | 116–123 | 0.6–0.9 |
| re-touch | 2 | 4.17–4.25 | 2.29–2.49 | 24.9–27.9 | 161–162 | 222–264 | 113–115 | 0.6–1.7 |
| writer touch while waiting | 2 | 2.66–2.72 | 2.33–2.68 | 22.3–24.0 | 160–174 | 220–239 | 112–132 | 0.5 |
| writer touch in `append` | 2 | 14.6–14.7 | 2.36–2.47 | 23.2–24.2 | 182 | 246–261 | 140 | 0.5–0.6 |
| `spare_segment(true)` | 2 | 13.7–14.4 | 2.28–2.29 | 19.5–21.0 | 41.0–41.3 | 228–239 | 44.8–45.2 | 0.4 |

1,024 B, 300,000/s:

| Configuration | Runs | Faults/MiB | p99 | p99.9 | p99.99 | max | Reader p99.9 | Late sends (%) |
|---|---|---|---|---|---|---|---|---|
| default | 4 | 0.52–7.67 | 2.40–3.95 | 250 µs to 18 ms | 997 µs to 31 ms | 1.3–33 ms | 613 µs to 18 ms | 0.9–1.6 |
| `prefault(false)` | 2 | 9.65–9.66 | 13.4–13.6 | 411–512 | 1.1–1.2 ms | 1.7–1.8 ms | 657–689 | 4.0–4.3 |
| `early_writeback(false)` | 2 | 0.30–0.32 | 2.10–2.43 | 23.6–27.3 | 199–211 | 421–433 | 397–417 | 0.6–0.7 |
| ranged sync | 2 | 7.40–7.73 | 2.53–2.91 | 171–178 | 803 µs to 1.1 ms | 1.2–1.9 ms | 552–594 | 1.2–1.3 |
| write-back to the 128 KiB boundary | 2 | 0.55 | 2.18–2.19 | 12.5–12.6 | 190–196 | 239–242 | 454–465 | 0.3–0.5 |
| ranged sync + write-back to 128 KiB boundary | 2 | 0.29 | 2.19–2.20 | 5.28–6.25 | 191–192 | 244–254 | 452–455 | 0.2–0.3 |
| re-touch | 2 | 3.55–3.71 | 8.90–10.2 | 184–361 | 901 µs to 1.0 ms | 1.2–1.6 ms | 588–624 | 1.4–1.5 |
| writer touch while waiting | 2 | 3.06–3.15 | 3.65–4.59 | 298–386 | 987 µs to 1.0 ms | 1.3–1.8 ms | 614–639 | 1.2–1.5 |
| `spare_segment(true)` | 2 | 1.38–7.34 | 2.34–4.14 | 374 µs to 3.9 ms | 1.1–9.8 ms | 1.7–11 ms | 575 µs to 3.9 ms | 1.1–1.4 |

aarch64, xfs (five configurations; one untraced run each, two of the default):

64 B, 1,000,000/s:

| Configuration | Runs | Faults/MiB | p99 | p99.9 | p99.99 | max | Reader p99.9 | Late sends (%) |
|---|---|---|---|---|---|---|---|---|
| default | 2 | 12.6–12.7 | 1.59–1.61 | 29.0–30.5 | 149–156 | 284–419 | 35.4–35.8 | 1.2 |
| ranged sync | 1 | 12.2 | 1.56 | 28.6 | 148 | 235 | 34.0 | 1.1 |
| write-back to the 128 KiB boundary | 1 | 0.83 | 1.10 | 3.08 | 140 | 203 | 13.9 | 0.7 |
| ranged sync + write-back to 128 KiB boundary | 1 | 0.27 | 0.96 | 2.68 | 141 | 213 | 4.73 | 0.7 |

1,024 B, 100,000/s:

| Configuration | Runs | Faults/MiB | p99 | p99.9 | p99.99 | max | Reader p99.9 | Late sends (%) |
|---|---|---|---|---|---|---|---|---|
| default | 2 | 18.4–18.6 | 2.35–2.36 | 15.1–16.0 | 94.7 | 211–235 | 22.0–23.2 | 0.4–1.0 |
| ranged sync | 1 | 18.1 | 2.31 | 15.3 | 96.6 | 227 | 22.2 | 0.2 |
| write-back to the 128 KiB boundary | 1 | 0.76 | 2.29 | 3.06 | 86.5 | 192 | 3.79 | 1.0 |
| ranged sync + write-back to 128 KiB boundary | 1 | 0.22 | 2.17 | 2.47 | 89.2 | 214 | 3.49 | 0.1 |

1,024 B, 170,000/s:

| Configuration | Runs | Faults/MiB | p99 | p99.9 | p99.99 | max | Reader p99.9 | Late sends (%) |
|---|---|---|---|---|---|---|---|---|
| default | 2 | 14.9–15.1 | 2.44–2.83 | 19.1–19.8 | 139 | 246–300 | 96.1–98.1 | 0.3–0.4 |
| ranged sync | 1 | 14.3 | 2.43 | 19.0 | 136 | 243 | 97.0 | 1.5 |
| write-back to the 128 KiB boundary | 1 | 0.60 | 2.20 | 3.08 | 126 | 193 | 73.3 | 1.2 |
| ranged sync + write-back to 128 KiB boundary | 1 | 0.25 | 2.25 | 2.61 | 133 | 196 | 76.3 | 0.1 |

1,024 B, 300,000/s:

| Configuration | Runs | Faults/MiB | p99 | p99.9 | p99.99 | max | Reader p99.9 | Late sends (%) |
|---|---|---|---|---|---|---|---|---|
| default | 2 | 8.42–8.78 | 2.45–3.00 | 22.0–22.2 | 161–167 | 257–607 | 403–413 | 0.8–1.0 |
| ranged sync | 1 | 6.93 | 2.29 | 22.1 | 158 | 236 | 400 | 0.9 |
| write-back to the 128 KiB boundary | 1 | 0.53 | 2.22 | 4.80 | 159 | 206 | 421 | 0.2 |
| ranged sync + write-back to 128 KiB boundary | 1 | 0.28 | 2.19 | 3.65 | 160 | 215 | 421 | 0.3 |

The supplement (x86-64, ext4, three untraced rounds of six configurations; its default runs are its own):

64 B, 1,000,000/s:

| Configuration | Runs | Faults/MiB | p99 | p99.9 | p99.99 | max | Reader p99.9 | Late sends (%) |
|---|---|---|---|---|---|---|---|---|
| default | 3 | 8.37–8.43 | 3.26–3.47 | 63.2–71.6 | 241–273 | 342–736 | 101–107 | 1.9 |
| ranged sync | 3 | 7.71–7.75 | 2.94–3.66 | 40.6–56.6 | 230–274 | 680–742 | 43.6–75.8 | 1.8–1.9 |
| write-back to the 128 KiB boundary | 3 | 0.94–1.03 | 1.64–1.83 | 66.5–95.4 | 255–257 | 348–462 | 92.5–132 | 1.4 |
| write-back to the 2 MiB boundary | 3 | 1.09–1.13 | 1.73–2.50 | 195–213 | 383–462 | 762–939 | 279–311 | 1.5–1.8 |
| ranged sync + write-back to 2 MiB boundary | 3 | 0.36 | 1.53–1.97 | 148–201 | 298–319 | 465–832 | 169–249 | 1.4–1.6 |
| write-back 2–4 MiB behind | 3 | 1.33–1.36 | 2.96–3.29 | 402–521 | 856 µs to 1.5 ms | 1.1–2.1 ms | 857 µs to 1.1 ms | 1.7–1.8 |

1,024 B, 100,000/s:

| Configuration | Runs | Faults/MiB | p99 | p99.9 | p99.99 | max | Reader p99.9 | Late sends (%) |
|---|---|---|---|---|---|---|---|---|
| default | 3 | 9.62–9.68 | 2.23–6.01 | 25.5–25.8 | 128–149 | 307–325 | 39.6–41.8 | 0.7–0.9 |
| ranged sync | 3 | 8.87–8.99 | 2.17–2.25 | 22.0–23.0 | 121–132 | 242–277 | 33.4–34.0 | 0.7–0.9 |
| write-back to the 128 KiB boundary | 3 | 0.90–0.95 | 2.09–5.05 | 4.50–7.50 | 127 µs to 1.6 ms | 219 µs to 3.3 ms | 38.6–126 | 0.5–1.5 |
| write-back to the 2 MiB boundary | 3 | 0.98–1.05 | 2.31–3.95 | 4.63–6.36 | 124–133 | 221–226 | 121–134 | 1.5 |
| ranged sync + write-back to 2 MiB boundary | 3 | 0.28–0.29 | 2.04–3.29 | 3.20–5.53 | 121–132 | 228–249 | 111–127 | 0.5–1.5 |
| write-back 2–4 MiB behind | 3 | 1.30–1.31 | 2.10–2.18 | 95.6–107 | 826–963 | 1.4–1.5 ms | 910 µs to 1.0 ms | 0.7–1.4 |

1,024 B, 170,000/s:

| Configuration | Runs | Faults/MiB | p99 | p99.9 | p99.99 | max | Reader p99.9 | Late sends (%) |
|---|---|---|---|---|---|---|---|---|
| default | 3 | 2.14–2.21 | 2.09–2.55 | 16.6–18.5 | 160–165 | 244–377 | 215–239 | 1.2 |
| ranged sync | 3 | 1.54–1.65 | 2.42–2.62 | 9.95–10.7 | 154–157 | 247–275 | 215–231 | 1.2–1.4 |
| write-back to the 128 KiB boundary | 3 | 0.71–0.77 | 2.17–2.40 | 6.92–9.00 | 145–165 | 210–268 | 242–265 | 1.0–1.4 |
| write-back to the 2 MiB boundary | 3 | 0.81–0.84 | 2.15–2.44 | 23.9–31.9 | 219–337 | 575–963 | 560–592 | 1.2–1.4 |
| ranged sync + write-back to 2 MiB boundary | 3 | 0.32–0.33 | 2.27–2.34 | 6.13–7.42 | 134–146 | 213–234 | 551–569 | 1.4–1.5 |
| write-back 2–4 MiB behind | 3 | 0.76–0.78 | 2.15–2.41 | 20.8–66.8 | 167 µs to 1.4 ms | 551 µs to 1.7 ms | 1.3–1.4 ms | 1.2–1.6 |

- **Writer touch, while waiting or in `append`.** Within the default's range in every cell, apart from single stalls of
  1.7 ms (64 B) and 3.0 ms (300,000/s) in one run each of the touch while waiting; the default's runs have such stalls
  too (up to 5.3 ms at 300,000/s). While waiting, it found a page to take in few waits (193 and 53 faults per run at
  1,024 B × 100,000 and 170,000/s, against 18,400 and 6,900 faulted appends): the pages that fault were write-protected
  after the writer last waited, or lie further on in the folio. On aarch64 it took most of the faults off the appends
  (1.6–3.1 per MiB left in them, against 12.5–18.5) and the latency did not move either: a touch waits as long as the
  fault would, and the send after it then starts late. In `append` it only moves a fault from one append to the one
  before. Its cost is below.
- **Larger window.** Worse in every cell and on both filesystems: each checkpoint writes the whole window back and
  write-protects it (PR #102), so the writer re-faults all of it (10–20 faults per MiB with 4 MiB, 34–35 with 16 MiB)
  and its p99.9 rises to 56–64 µs with 1 MiB and 0.2–0.9 ms with 4 and 16 MiB. A rate-adaptive window would choose among
  these rows.
- **Re-touch.** A third (1,024 B) to a sixth (64 B) of the faults, but the faults left collide with the helper on the
  same folio (median 3.6–31.5 µs instead of 1.1 in the traced runs), and the writer's p99.9 is no better (slightly above
  the default's range at 64 B and at 1,024 B × 100,000/s on ext4, within it elsewhere).
- **Early write-back.** Off, as known, checkpoints take hundreds of milliseconds (below) and the writer stalls behind
  them at 64 B. 2–4 MiB behind: the checkpoint then writes those megabytes and the writer's first fault after it waits
  for its journal commit (appends of 1.3–1.8 ms on ext4 in `tt-sustained`, below), and the zero-copy reader's p99.9
  rises to about 1 ms (why the reader waits that long was not traced). To the 2 MiB boundary: the same, smaller. **To
  the 128 KiB boundary:** on x86-64 ext4, faults 0.7–1.0 per MiB, writer p99.9 at 1,024 B 4.5–7.5 against 25.5–25.8 µs
  (100,000/s) and 6.9–9.0 against 16.6–18.5 µs (170,000/s), p99 at 64 B 1.6–1.8 against 3.3–3.5 µs (its p99.9 there did
  not move); p99.99, maxima and the reader within the default's range but for one run at 100,000/s with a single 3.3 ms
  append 2.7 ms after a checkpoint request (the default's runs have such outliers too: up to 2.0 ms in the main matrix).
  On aarch64, in every cell and on both filesystems: faults 0.5–0.9 per MiB against 6.5–18.6, writer p99.9 2.7–3.2 µs
  against 15–34 at the three lower rates and 4.8–12.6 against 22 µs to 18 ms at 300,000/s, the reader's p99.9 at 64 B
  10–14 against 35–41 µs and at 1,024 B × 100,000/s 3.5–4.3 against 22–26 µs; p99.99 and maxima within or below the
  default's range.
- **Ranged sync.** 7–28 % fewer faults (the window is no longer written back), writer p99.9 41–57 against 63–72 µs (64
  B), 22–23 against 25.5–25.8 and 10–11 against 16.6–18.5 (1,024 B), the reader's p99.9 at 64 B 44–76 against 101–107
  µs; p99.99 and maxima unchanged. With early write-back to the 2 MiB boundary as well, the two lower faults further but
  the reader's p99.99 rose to 1.2 ms, as with the 2 MiB boundary alone. On aarch64, alone, it stayed within the
  default's range (one run at 64 B had a 12 ms stall; one default run at 300,000/s had an 18 ms one); with the 128 KiB
  boundary it left 0.22–0.29 faults per MiB, the reader's p99.9 at 64 B 4.5–5.2 µs against 10–14 with the boundary
  alone, and the writer's p99.9 at 300,000/s 3.7–6.3 against 4.8–12.6.
- **Spare segment.** Removes the faults at rolls and the roll's own work: at 300,000/s the writer's p99.9 fell from
  86–172 to 8.1–9.2 µs (ext4) and from 79–86 to 8.6 µs (xfs). At 170,000/s on ext4 one of its runs had an 8.8 ms stall
  (p99.9 509 µs). On aarch64 ext4 it lowered the writer's p99.99 at 1,024 B × 100,000 and 170,000/s (34 and 41 against
  122–133 and 165–171 µs) and the reader's p99.9 at 170,000/s (45 against 124–137 µs), and at 300,000/s one of its two
  runs stalled (p99.9 3.9 ms). It is an option since ADR 0049, which says what to measure before it becomes the default.

### `tt-sustained` at 170,000/s, 1,024 B

The library's own sustained tool (`--duration 30s --warmup 5s --checkpoint-every 1s --checkpoint-mode background
--readers mapped`), three interleaved runs per configuration. Its append latency is the time of one `Writer::append`
(not from the schedule). µs unless a cell says ms.

| Arch, filesystem | Configuration | Append p99.9 | Append p99.99 | Append max | Durable p50 | Durable max | Reader p99.9 | Reader p99.99 |
|---|---|---|---|---|---|---|---|---|
| x86-64, ext4 | default | 4.8–5.6 | 22.5–23.3 | 183–452 | 2.4–2.8 ms | 2.9–3.2 ms | 61–72 | 620–653 |
| | write-back 2–4 MiB behind | 3.2–4.2 | 7.1–9.6 | 1.3–1.8 ms | 12.3–13.0 ms | 14.7–16.8 ms | 1.2 ms | 2.0 ms |
| | `early_writeback(false)` | 2.7–4.4 | 11.8–17.3 | 348–420 | 439–441 ms | 763–782 ms | 93–186 | 0.8–2.3 ms |
| x86-64, xfs | default | 4.6–6.5 | 17.8–18.7 | 211–223 | 1.15–1.19 ms | 1.46–1.52 ms | 44–52 | 613–617 |
| | write-back 2–4 MiB behind | 2.7–2.9 | 5.3–9.8 | 176–186 | 11.6–11.7 ms | 16.9–17.0 ms | 0.85–1.25 ms | 1.9 ms |
| | `early_writeback(false)` | 2.4–2.6 | 5.7–7.2 | 297–542 | 462 ms | 863 ms | 377–462 | 4.3–4.9 ms |
| aarch64, ext4 | default | 6.7–7.4 | 29.5–30.1 | 249–254 | 0.89–0.91 ms | 1.13–1.25 ms | 144–156 | 769–779 |
| | write-back to the 128 KiB boundary | 2.5–3.7 | 4.4–6.2 | 191–197 | 0.88–0.92 ms | 1.08–1.12 ms | 142–149 | 775–782 |
| | ranged sync | 5.3–6.1 | 28.7–29.4 | 239–242 | 0.50–0.51 ms | 0.73–0.91 ms | 143–158 | 766–782 |

Late sends: aarch64 0.36–0.40 % (default), 0.06 % (128 KiB boundary), 0.35–0.93 % (ranged sync); x86-64 0.9–1.7 % in
every configuration. The 128 KiB boundary left the checkpoint's durable time as it was; the ranged sync shortened it by
0.4 ms (the 256 KiB it no longer writes, PR #102). The zero-copy reader's tail in this tool did not move with either.
Neither was run with `tt-sustained` on x86-64.

### The comparison's W2 cells with the 128 KiB boundary (aarch64)

`tt-latency run --rate <rate> --count <n> --warmup <w> --payload <64|1024> --reader mapped --writer-cpu 1 --reader-cpu 2
--helper-cpus 3` (the latency tool of the comparison's W2, zero-copy reader, from the scheduled send to the read; no
checkpoints, so the ranged sync does not apply), 11 s at 100,000/s and 31 s at 10,000/s with the first second left out,
ext4, three interleaved rounds, on the aarch64 VM after its matrix. Reader p99.9 / p99.99 in µs:

| Payload, rate | Default | Write-back to the 128 KiB boundary | Rerun (above): tickertape / Chronicle with `microTouch()` alone / all on |
|---|---|---|---|
| 64 B, 10,000/s | 0.49–2.45 / 4.2–4.6 | 0.48–2.34 / 3.2–4.9 | 2.87–3.46 / 1.59–3.89 / 0.73–0.78 |
| 64 B, 100,000/s | 1.98–2.19 / 10.0–10.8 | 0.45–1.47 / 3.8–4.2 | 1.88–2.08 / 12.7–18.1 / 7.8–13.3 |
| 1,024 B, 10,000/s | 8.76–9.41 / 28–30 | 2.31–2.76 / 4.2–6.0 | 8.73–9.10 / 2.71–2.94 / 2.77–3.46 |
| 1,024 B, 100,000/s | 25.2–26.7 / 795–804 | 3.21–4.64 / 803–812 | 24.8–28.5 / 18.7–29.2 / 14.5–16.3 |

The default reproduces the rerun's tickertape rows at 1,024 B on the same machine type. With early write-back stopped at
the 128 KiB boundary, the 1,024 B p99.9 falls below Chronicle Queue's with `microTouch()`. The p99.99 at 1,024 B ×
100,000/s (about 0.8 ms in both) is the roll without checkpoints (issue #60: rolls without checkpoints still take
0.7–0.9 ms), which this does not touch. Not run on x86-64.

### What writes the extra bytes at steady-90 (PR #105's question)

PR #105 ([2026-11-26, how much of the limit is the
disk](#2026-11-26-how-much-of-the-limit-is-the-disk-fio-tmpfs-and-one-change-at-a-time-google-cloud-vms)) found the
device writing 7 % more than tickertape appended at steady-90 (1,024 B, aarch64) with the defaults, and exactly the
appended bytes with early write-back off, and the stalls of 50–144 ms gone with it. The same command (`tt-sustained run
… --payload 1024 --duration 60s --warmup 10s --cpus 1,2,3,4,5,6 --checkpoint-every 1s --checkpoint-mode background
--retain-bytes 4294967296 --readers mapped,safe:wait=default,safe:durable:wait=block --rate 304571`, `fstrim` and 3 s
before each run), on a new `c4a-standard-8-lssd` (us-east1-b, 13:14–13:45), ext4 on the first local SSD, four
interleaved runs per configuration, the last under `bytes.bt` (block `bio_queue` bytes by the thread that queued them,
and folios starting write-back). Device bytes from `/proc/diskstats` (sectors written) over the whole run, warm-up
included; appended bytes are the records sent × 1,064. "Seconds" counts the 71 seconds of each run, warm-up included.

| Configuration | Runs | Device GB per run | Device / appended | Append p99.9 (µs) | p99.99 (µs) | max (ms) | Seconds with an append ≥ 10 ms | Durable p50 (ms) | Durable max (ms) | Late sends (%) | Mapped reader p99.9 (ms) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| default | 4 | 24.62–24.68 | 1.085–1.088 | 3.8–3.9 | 28.1–28.6 | 140–142 | 25–30 | 319–423 | 665–835 | 5.64–7.84 | 117–131 |
| `early_writeback(false)` | 4 | 22.70 | 1.001 | 2.3–2.4 | 3.9–4.3 | 0.97–3.72 | 0 | 924–930 | 1145 | 0.41–0.47 | 0.45–0.60 |
| ranged sync | 4 | 24.61–24.67 | 1.085–1.087 | 3.9–4.1 | 28.0–29.1 | 140–142 | 26–29 | 175–279 | 630–790 | 5.97–9.14 | 122–136 |
| write-back to the 128 KiB boundary | 4 | 22.75 | 1.003 | 2.5–2.6 | 4.5–5.0 | 0.20–1.77 | 0 | 0.98–1.04 | 1–2 | 0.40–0.49 | 0.47–0.58 |
| ranged sync + 128 KiB boundary | 4 | 22.70 | 1.001 | 2.4–2.6 | 4.1–4.2 | 0.21–10 | 0–1 | 0.58–0.60 | 1 | 0.35–0.44 | 0.48–0.60 |

- **What writes them.** Early write-back asks for the whole pages behind the writer every 1 MiB
  (`sync_file_range(SYNC_FILE_RANGE_WRITE)` from the end of the last range to the start of the page that holds the
  position; nothing is rounded up in `writeback.rs`). The kernel writes back whole folios, and the folio that holds the
  position (128 KiB here) is then written while the writer is still filling it: once now, partly empty (records up to
  the position, zeros and prefaulted zeros after it), and again when it is full. One such folio per 1 MiB, half full on
  average, is about 6 % more bytes; the checkpoints, which also write the writer's folio and the 256 KiB window (PR
  #102), the rest. In the traced default run, the early write-back thread queued 23.05 GB and the checkpoint thread 1.38
  GB for 22.7 GB appended (with the boundary: 22.49 and 0.10 GB; with early write-back off the checkpoint thread wrote
  22.41 GB and the writer's own thread 0.27 GB, in write-back throttling). Counting folios per inode and index did not
  work here: retention deletes segments during the run and ext4 reuses their inode numbers, so the counts mix files
  (x86-64, below, kept every segment).
- **The stalls.** With early write-back stopped at the 128 KiB boundary, the device wrote 0.3 % more than appended
  (mostly the window at checkpoints: 0.1 % with the ranged sync as well), no second had an append of 10 ms or more
  (25–30 of 71 with the defaults), the maximum was 0.2–1.8 ms, late sends 0.4–0.5 % (5.6–7.8 %), and checkpoints were
  durable after 1 ms, where early write-back off gives the same tail at 0.9 s per checkpoint. With the ranged sync as
  well, 0.0 % more and durable after 0.6 ms (one run had one append of 10 ms). The ranged sync alone left the bytes and
  the stalls as they were: the window it saves is small next to the rewritten folios.
- **One story with the ranged sync.** Both write pages the writer has not finished: early write-back the rest of the
  folio it is filling, every 1 MiB; the checkpoint's whole-file `fdatasync` the prefaulted window, every checkpoint.
  Both pages are written again once they hold records, and writing them write-protects them, which is where the writer's
  faults come from. The boundary removes the first, the ranged sync the second. At steady-90 on aarch64 the first is
  what pushes the device from 324 MB/s of appends to about 350 MB/s, its limit, and the stalls follow.

x86-64 (a new `c4-standard-8-lssd`, us-east1-b, 13:46–14:04; 311,614 records/s, the x86-64 steady-90 rate of the 16-vCPU
type, 331 MB/s; `--retain-bytes 68719476736` so that no segment is deleted and folios can be counted per file; three
runs each, the last traced):

| Configuration | Runs | Device GB per run | Device / appended | Append p99.9 (µs) | p99.99 (µs) | max (ms) | Seconds with an append ≥ 10 ms | Durable p50 (ms) | Durable max (ms) | Late sends (%) | Mapped reader p99.9 (ms) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| default | 3 | 23.26–23.29 | 1.002–1.004 | 3.9–5.1 | 9.1–9.6 | 0.45–128 | 0–3 | 2.16–3.43 | 25–434 | 1.98–3.23 | 0.38–87 |
| `early_writeback(false)` | 3 | 23.22 | 1.000 | 4.4–5.8 | 8.1–8.6 | 1.53–4.74 | 0 | 913–925 | 1149–1179 | 2.47–2.73 | 0.48–0.50 |
| write-back to the 128 KiB boundary | 3 | 23.26 | 1.002 | 4.5–5.2 | 8.8–9.5 | 0.84–12.23 | 0–1 | 1.94–3.11 | 3–495 | 2.48–2.71 | 0.44–0.72 |

On x86-64 the defaults wrote only 0.2–0.4 % more than appended, and in the traced run 416 of 183,790 folios were written
back more than once (0.06 GB): 146 by early write-back and then a checkpoint, 117 twice by early write-back, 103 twice
by checkpoints. At this rate the x86-64 writer has left the folio by the time early write-back reaches it (as with
`tt-faults` at 300,000/s above), so there is little to remove: the boundary changed neither the bytes nor the latency
beyond the spread. One default run of three had a 128 ms append (and a checkpoint durable after 434 ms), one run with
the boundary a 12 ms one; PR #105's x86-64 maxima were 0.25–0.64 ms.

### A touch on the writer's thread: what it costs `append`

`tt-append-cost append --count <500,000 | 1,000,000> --payload 64 --prefault` under `perf stat -e instructions:u`, the
writer pinned; the difference of the two counts divided by 500,000. Five pairs on x86-64, three on aarch64, interleaved.

| Arch | Instructions per append, default | With the touch in `append` | ns per append, default | With the touch |
|---|---|---|---|---|
| x86-64 | 407.4 | 423.8 (+16.4) | 35.1–48.9 | 36.7–51.4 (slower in all 5 pairs) |
| aarch64 | 383.8 | 396.5 (+12.7) | 82.6–88.8 | 91.8–93.5 (slower in all 3 pairs) |

ADR 0047's gate for the append path is 3 instructions and no time difference outside the noise floor. The check itself
is a compare and a branch; the rest is code layout around it. A `Writer::idle()` would cost nothing in `append`, but it
found little to do (above).

### Does the ranged sync still flush a volatile write cache?

The VMs' disks report "write through", so the kernel sends them no cache flushes at all, and writing "write back" to the
local SSD's queue did not take effect. A loop device does report a write-back cache (it turns a flush into an `fsync` of
its backing file): ext4 and xfs on a loop device backed by a file on the local SSD, 20,000 records/s of 1,024 B, a
background checkpoint every second, 11 checkpoints per run, two runs each, flush requests issued to the loop device
counted (block `rq_issue` tracepoint):

| Filesystem | Configuration | Flush requests per checkpoint | Checkpoint thread's `blkdev_issue_flush` per run | Its `msync` / `fdatasync` calls per run |
|---|---|---|---|---|
| ext4 | default | 7.5–7.8 | 12–14 | 11 / 31 |
| ext4 | ranged sync | 4.1–4.3 | 9 | 11 / 11 |
| xfs | default | 7.1 | 14 | 11 / 31 |
| xfs | ranged sync | 3.9 | 9 | 11 / 11 |

So the ranged checkpoint still sends flushes; the order (the flush that follows the active segment's data comes before
the marker's write) follows from the kernel sources (ADR 0058 → Is the ranged sync safe?), and was not traced per call.

### Chronicle's `microTouch()` on the same machine types

The comparison rerun at v1.0.0 (issue #57; still running when this was written, its short part finished on 2026-11-25;
branch `rerun/v1.0.0-session`; published since in
[its own section](#2026-11-25-to-2026-11-26-comparison-rerun-at-v100-google-cloud-vms-linux-x86-64-and-aarch64)) has Chronicle Queue with every tool setting off (`jvm-old`: the old JVM warm-up, no
`microTouch()`, the default 64 MiB `blockSize`), with `microTouch()` alone, and with every setting on, and tickertape
with its defaults (prefault on, early write-back on, no checkpoints), in its W2 latency runs (from the scheduled send to
the read, 11 s, the first second left out; four runs per row: two rounds of two). ext4 on the first local SSD, x86-64
`c4-standard-16-lssd` (16 vCPUs, the same CPU as above) and aarch64 `c4a-standard-8-lssd` (the type above). Reader p99.9
in µs; tickertape with its zero-copy reader:

| Arch | Payload, rate | Chronicle, all off | `microTouch()` alone | Chronicle, all on | tickertape |
|---|---|---|---|---|---|
| x86-64 | 64 B, 10,000/s | 2.45–3.51 | 1.90–1.99 | 1.68–1.83 | 2.17–2.35 |
| | 64 B, 100,000/s | 28.6–29.2 | 17.8–18.2 | 11.8–15.3 | 2.27–5.96 |
| | 1,024 B, 10,000/s | 34.8–36.8 | 2.35–2.51 | 2.36–2.82 (and one run of 15.8 ms) | 4.60–6.84 |
| | 1,024 B, 100,000/s | 35.4–37.4 | 25.2–26.2 | 23.9–24.6 | 28.6–29.8 |
| aarch64 | 64 B, 10,000/s | 2.13–2.24 | 1.59–3.89 | 0.73–0.78 | 2.87–3.46 |
| | 64 B, 100,000/s | 25.1–27.1 | 12.7–18.1 | 7.8–13.3 | 1.88–2.08 |
| | 1,024 B, 10,000/s | 36.4–37.6 | 2.71–2.94 | 2.77–3.46 | 8.73–9.10 |
| | 1,024 B, 100,000/s | 33.7–35.2 | 18.7–29.2 | 14.5–16.3 | 24.8–28.5 |

- **What `microTouch()` does for Chronicle** on these machines: against nothing, it takes the p99.9 at 1,024 B ×
  10,000/s from 35–38 to 2.4–2.9 µs, and lowers it by up to about half in the other cells (not clearly at aarch64 64 B ×
  10,000/s, where its runs spread from 1.6 to 3.9 µs); the rest of "all on" adds less. That is the gain tickertape's
  helper already has against no prefaulting: in this section, prefault off against on is 3.7–4.0 ms against 60–100 µs at
  64 B × 1,000,000/s and 140–237 against 26–27 µs at 1,024 B × 100,000/s (writer p99.9, x86-64 ext4). The two are not
  the same comparison: Chronicle's gain is against no pre-touching at all, and tickertape already has a helper.
- **Where tickertape is above Chronicle with `microTouch()`:** at 1,024 B, by 2–6 µs at 10,000/s and by 2–14 µs at
  100,000/s (and at 64 B × 10,000/s, by 0.3–2.7 µs against "all on"; what sets that p99.9 was not measured here). Those
  are the cells where, in this section, early write-back's write-protect faults set tickertape's p99.9 (1,024 B ×
  100,000/s: 26–27 µs, with the 128 KiB boundary 4.5–7.5 µs, measured with `tt-faults`). Chronicle has no early
  write-back. With `tt-latency` in the W2 cells on aarch64 (above) the boundary closed that gap: 2.3–2.8 µs at 1,024 B ×
  10,000/s and 3.2–4.6 µs at 100,000/s. x86-64 was not run with `tt-latency`.

### A README sentence these figures update

`README.md` (Performance) says: "In separate tickertape-only runs on the bare-metal machine (ext4, 100,000 records/s),
turning prefaulting off raised the zero-copy reader's p99.9 from 1.7–2.0 µs to 31–34 µs at 64 bytes, close to Chronicle
Queue's 36–37 µs. At 1,024 bytes it did not raise it: 110–243 µs with prefaulting and 97–122 µs without, both above
Chronicle Queue's 48–55 µs." That was v0.4.0 on bare metal, before early write-back (ADR 0038, 2026-11-20), without
checkpoints. Proposed addition (the owner decides; this PR does not change the README): "With 1.0's defaults on cloud
VMs (ext4, 100,000 records/s of 1,024 bytes, a checkpoint every second; `docs/benchmarks.md`, 2026-11-26), prefaulting
off raised the writer's p99.9 from 26–27 µs to 140–237 µs and the zero-copy reader's from 40–52 µs to 321–412 µs."

And: "Close to its limit the writer can stall. In tickertape's own 10-minute runs at 1,024 bytes and 90 % of the unpaced
rate, appends of 50–128 ms recurred on aarch64 ext4." Proposed addition, once ADR 0058's change is decided: "There,
early write-back made the device write 8 % more than was appended, by writing back the 128 KiB the writer was filling;
stopping it at the boundary below the writer removed the extra bytes and these stalls in an experiment (not the
default)." Neither sentence is changed by this PR.

### What these figures do not show

Bare metal; other VM shapes and disks; macOS (where the helper advises `MADV_WILLNEED` 8 MiB ahead, which makes no page
writable, and there is no early write-back); kernels other than 7.0, and so other folio sizes (with smaller folios the
early write-back faults should not occur; with larger ones, the 128 KiB boundary would not prevent them); `lazytime`;
unpaced writers and bursts; `HelperCpus::Inherited`; runs longer than 20 s (30 s in `tt-sustained`); the comparison
harness with any switch, and `tt-latency` with it on x86-64; the ranged sync and the 128 KiB boundary together on
x86-64; a drive with a volatile write cache, and power loss.

## 2026-11-26, seeks with and without the seek index (Google Cloud VMs)

What a seek costs in a journal of 128 MiB segments (the default) with the seek index of ADR
[0045](decisions/0045-sparse-seek-index.md) and without it, for both readers, at 64 B and 1,024 B, and what building the
indexes costs (issue #59). **Cloud virtual machines with local NVMe SSD, not bare metal.** The 2026-11-25 section
([below](#seek-index-adr-0045)) measured the index at 1,024 B only, at `d2074c1` (before #74), on other machine types
and with other targets; this one measures `main` after #74.

| | x86-64 | aarch64 |
|---|---|---|
| Machine type, zone | `c4-standard-4-lssd`, us-central1-c | `c4a-standard-4-lssd`, us-east1-b |
| CPU, memory | Intel Xeon 6985P-C, 4 vCPUs (2 cores), 15.7 GB | Google Axion (Neoverse-V2), 4 vCPUs (4 cores), 16.7 GB |
| Date (UTC) | 2026-11-26, 05:12 to 05:16 (a first session on another VM of the type: 04:55 to 04:59, below) | 2026-11-26, 05:04 to 05:08 |

Both: Ubuntu 24.04, kernel 7.0.0-1017-gcp, rustc 1.100.0; one local NVMe SSD (375 GB), ext4 formatted with `-E
lazy_itable_init=0,lazy_journal_init=0`, mounted `rw,noatime`, I/O scheduler `none`; `vm.dirty_ratio` 20,
`vm.dirty_background_ratio` 10. Commit `8912012` (`main`, clean tree), `cargo build --release --locked -p
tickertape-bench --bin tt-scale` and `-p tickertape-cli --bin tickertape`. The result files were written to the same
filesystem. The runs started right after the build, on a VM that ran nothing else.

**Journals.** One per payload: `tt-scale build --dir <dir> --segments 9 --segment-size 134217728 --payload <64|1024>`,
eight full sealed segments (1 GiB) and a ninth, the newest, half full, with the checkpoint marker in it; then
`tickertape index <dir>`, which built an index for each of the eight sealed segments (a full segment holds 1,290,554
records of 64 B or 126,144 of 1,024 B). Each index file is 49,256 bytes (2,047 entries; 0.037 % of the segment) at both
payloads.

**Targets.** In the fifth segment (full, sealed, indexed): the record 1/64 of the way in (about 2 MiB in), the middle
record, and the last record. In the newest segment (unsealed, never indexed): the same fractions of the records it holds
(it is half full, so its last record is about 64 MiB in).

**Commands.** Every value is one process, `taskset -c 3 tt-scale op --dir <dir> --op seek-segment-end --reader
<safe|mapped> --seq <target> --segment-size 134217728 [--seek-index]`, the internal command that `tt-scale measure` runs
for each operation, given the target directly. It opens a reader (not timed) and times `Reader::seek` and the first
`try_next`, which returns the target. It reports `used_index` (`Reader::seek_used_index`) and the bytes the process read
from storage (`/proc/self/io`). The forward seeks are `--op seek-forward --seq <first record> --span-end <end>`: a seek
to the segment's first record and one record, then 16 equal forward steps, each a seek and one record, timed one by one.
The driver, a Python script kept with the raw output outside the repository, ran pinned to CPU 0; CPU 3 shares its core
with CPU 1 on x86-64 and has a core of its own on aarch64. "Cold" ran `sudo -n sh -c 'sync && echo 3 >
/proc/sys/vm/drop_caches'` before the process; the reader's open (not timed) then brings the directory and the oldest
segment's header back into the cache, but nothing of the target's segment or of `seek-index/`. "Warm": every file of
both journals was read before each round. Two rounds; in each, 25 warm repetitions of every configuration, then 8 cold,
for both payloads and readers in turn, the runs without and with `--seek-index` of one configuration back to back and
their order swapped at every repetition. So each warm cell is 50 values and each cold cell 16. The cells are the median
and, in brackets, the smallest and largest value. Every run returned the target, and `used_index` was true in every run
with the option in a sealed segment and false in every other run.

**Reproducibility.** The medians of the two rounds of a cell differ by at most 13 % on both machines, much less than
any difference between the index columns this section draws a conclusion from. The first x86-64 session, on another VM
of the same type with the same commit, ran each configuration's runs without the option before those with it: its warm
values in the newest segment, where the index plays no part, were 2 to 31 % lower with the option, and
alternating runs on that VM afterwards (20 pairs per case, each order 10 times) gave equal medians without and with the
option; whichever came first was up to 9 % slower with the zero-copy reader. That is why the order alternates here. Its
other values were within 12 % of the second session's, except the cold seeks with the index: 0.50 to 0.72 ms there
against 0.60 to 0.81 ms here.

### Seeks into a sealed segment

"1/64 in", "middle" and "last record" are the targets in the fifth segment. "Read from storage" is the median of the
cold runs' bytes read, readahead included.

| Machine | Payload | Reader | Target | warm, without index | warm, with index | cold, without index | cold, with index | read from storage, cold, KiB (without / with) |
|---|---|---|---|---|---|---|---|---|
| x86-64 | 64 B | safe | 1/64 in | 659 µs (633 µs–854 µs) | 99 µs (91 µs–121 µs) | 1.96 ms (1.90 ms–2.27 ms) | 789 µs (755 µs–892 µs) | 2,312 / 212 |
| x86-64 | 64 B | safe | middle | 18.9 ms (18.2 ms–28.2 ms) | 83 µs (76 µs–104 µs) | 93.2 ms (93.0 ms–96.5 ms) | 744 µs (697 µs–813 µs) | 65,800 / 212 |
| x86-64 | 64 B | safe | last record | 37.6 ms (36.4 ms–46.0 ms) | 95 µs (88 µs–114 µs) | 191 ms (191 ms–192 ms) | 755 µs (737 µs–884 µs) | 131,080 / 140 |
| x86-64 | 64 B | zero-copy | 1/64 in | 928 µs (834 µs–1.09 ms) | 112 µs (102 µs–144 µs) | 1.94 ms (1.83 ms–2.35 ms) | 813 µs (742 µs–978 µs) | 2,308 / 332 |
| x86-64 | 64 B | zero-copy | middle | 29.2 ms (26.2 ms–32.0 ms) | 78 µs (73 µs–111 µs) | 93.1 ms (92.9 ms–93.3 ms) | 604 µs (555 µs–724 µs) | 65,796 / 84 |
| x86-64 | 64 B | zero-copy | last record | 56.7 ms (52.0 ms–63.7 ms) | 108 µs (97 µs–137 µs) | 191 ms (191 ms–192 ms) | 786 µs (745 µs–963 µs) | 131,076 / 200 |
| x86-64 | 1,024 B | safe | 1/64 in | 398 µs (372 µs–439 µs) | 91 µs (82 µs–118 µs) | 1.74 ms (1.68 ms–2.36 ms) | 787 µs (717 µs–866 µs) | 2,312 / 212 |
| x86-64 | 1,024 B | safe | middle | 10.2 ms (9.84 ms–12.2 ms) | 94 µs (88 µs–116 µs) | 93.1 ms (93.0 ms–93.4 ms) | 803 µs (743 µs–880 µs) | 65,800 / 336 |
| x86-64 | 1,024 B | safe | last record | 20.3 ms (19.5 ms–21.8 ms) | 87 µs (81 µs–110 µs) | 192 ms (191 ms–192 ms) | 745 µs (633 µs–882 µs) | 131,080 / 140 |
| x86-64 | 1,024 B | zero-copy | 1/64 in | 277 µs (250 µs–322 µs) | 79 µs (72 µs–101 µs) | 1.64 ms (1.59 ms–2.35 ms) | 614 µs (562 µs–763 µs) | 2,308 / 84 |
| x86-64 | 1,024 B | zero-copy | middle | 7.15 ms (6.88 ms–7.48 ms) | 86 µs (77 µs–126 µs) | 93.0 ms (92.9 ms–93.1 ms) | 800 µs (733 µs–1.13 ms) | 65,796 / 332 |
| x86-64 | 1,024 B | zero-copy | last record | 14.7 ms (13.9 ms–15.7 ms) | 82 µs (76 µs–106 µs) | 191 ms (191 ms–192 ms) | 771 µs (658 µs–808 µs) | 131,076 / 200 |
| aarch64 | 64 B | safe | 1/64 in | 711 µs (668 µs–761 µs) | 109 µs (98 µs–154 µs) | 1.65 ms (1.60 ms–2.62 ms) | 622 µs (590 µs–1.46 ms) | 2,308 / 208 |
| aarch64 | 64 B | safe | middle | 20.2 ms (19.4 ms–24.9 ms) | 87 µs (80 µs–109 µs) | 93.0 ms (92.9 ms–93.7 ms) | 593 µs (566 µs–862 µs) | 65,796 / 208 |
| aarch64 | 64 B | safe | last record | 40.6 ms (38.8 ms–41.7 ms) | 107 µs (96 µs–126 µs) | 191 ms (191 ms–192 ms) | 570 µs (555 µs–711 µs) | 131,076 / 136 |
| aarch64 | 64 B | zero-copy | 1/64 in | 610 µs (581 µs–626 µs) | 111 µs (103 µs–131 µs) | 1.56 ms (1.50 ms–2.20 ms) | 628 µs (610 µs–758 µs) | 2,304 / 328 |
| aarch64 | 64 B | zero-copy | middle | 17.0 ms (16.8 ms–19.4 ms) | 87 µs (81 µs–116 µs) | 92.9 ms (92.8 ms–93.1 ms) | 464 µs (448 µs–498 µs) | 65,792 / 80 |
| aarch64 | 64 B | zero-copy | last record | 33.9 ms (33.4 ms–38.8 ms) | 109 µs (97 µs–128 µs) | 191 ms (191 ms–192 ms) | 595 µs (575 µs–741 µs) | 131,072 / 196 |
| aarch64 | 1,024 B | safe | 1/64 in | 404 µs (371 µs–439 µs) | 95 µs (85 µs–113 µs) | 1.35 ms (1.29 ms–2.63 ms) | 618 µs (601 µs–684 µs) | 2,308 / 208 |
| aarch64 | 1,024 B | safe | middle | 9.91 ms (9.27 ms–10.4 ms) | 98 µs (92 µs–138 µs) | 93.0 ms (92.9 ms–93.2 ms) | 647 µs (632 µs–1.13 ms) | 65,796 / 332 |
| aarch64 | 1,024 B | safe | last record | 20.1 ms (19.0 ms–20.4 ms) | 95 µs (87 µs–133 µs) | 192 ms (191 ms–192 ms) | 626 µs (604 µs–655 µs) | 131,076 / 136 |
| aarch64 | 1,024 B | zero-copy | 1/64 in | 256 µs (248 µs–283 µs) | 92 µs (85 µs–106 µs) | 1.25 ms (1.19 ms–2.14 ms) | 507 µs (488 µs–582 µs) | 2,304 / 80 |
| aarch64 | 1,024 B | zero-copy | middle | 5.88 ms (5.84 ms–6.14 ms) | 94 µs (89 µs–109 µs) | 92.9 ms (92.7 ms–93.6 ms) | 637 µs (627 µs–685 µs) | 65,792 / 328 |
| aarch64 | 1,024 B | zero-copy | last record | 11.6 ms (11.4 ms–11.7 ms) | 97 µs (88 µs–145 µs) | 191 ms (191 ms–192 ms) | 650 µs (629 µs–870 µs) | 131,072 / 196 |

- **Without an index** the time grows with the distance from the segment's first record, since every record before the
  target is read and validated: warm, 11.6 to 56.7 ms to the last record, depending on payload, reader and machine.
  Cold, the whole 128 MiB is read from storage: 191 to 192 ms in every case, the same on both machines and with both
  readers and payloads (about 0.7 GB/s from the local SSD, which is what these values measure).
- **With the index** every median was between 78 and 112 µs warm and between 0.46 and 0.81 ms cold, at all three
  targets, both payloads, both readers and both machines; a cold seek read 80 to 336 KiB (the index file, the seal and
  the place it landed, with readahead). The seek validates the landing record and the less than 64 KiB of records from
  there to the target.
- Each value is the first seek of a new process: its code, its allocations and, for the zero-copy reader, its mapping
  are new. A process that seeks repeatedly was not measured.

### Seeks into the newest segment

The newest segment is never indexed (it is not sealed, and `tickertape index` indexes only segments before the marker's
one). A seek there reads and validates every record from the segment's first one to the target, with or without the
option. A safe reader with the option first opens `seek-index/` and looks for the segment's file, which is not there; a
zero-copy (or durable-only) reader first reads the checkpoint marker, finds that it names this segment, and does not
look (ADR 0045, steps 2 and 3).

| Machine | Payload | Reader | Target | warm, without index | warm, with index | cold, without index | cold, with index | read from storage, cold, KiB (without / with) |
|---|---|---|---|---|---|---|---|---|
| x86-64 | 64 B | safe | 1/64 in | 351 µs (321 µs–379 µs) | 349 µs (328 µs–389 µs) | 1.27 ms (1.22 ms–1.52 ms) | 1.34 ms (1.30 ms–1.69 ms) | 1,288 / 1,292 |
| x86-64 | 64 B | safe | middle | 9.42 ms (9.18 ms–14.0 ms) | 9.39 ms (9.18 ms–9.69 ms) | 44.0 ms (43.8 ms–44.2 ms) | 44.1 ms (43.9 ms–44.3 ms) | 33,032 / 33,036 |
| x86-64 | 64 B | safe | last record | 17.8 ms (16.5 ms–18.7 ms) | 17.8 ms (16.7 ms–25.1 ms) | 93.2 ms (93.1 ms–93.4 ms) | 93.3 ms (93.2 ms–95.9 ms) | 65,800 / 65,804 |
| x86-64 | 64 B | zero-copy | 1/64 in | 472 µs (409 µs–550 µs) | 476 µs (431 µs–584 µs) | 1.15 ms (1.13 ms–1.46 ms) | 1.15 ms (1.11 ms–1.28 ms) | 1,284 / 1,284 |
| x86-64 | 64 B | zero-copy | middle | 14.4 ms (12.7 ms–15.7 ms) | 14.0 ms (12.9 ms–15.6 ms) | 43.8 ms (43.7 ms–44.1 ms) | 43.9 ms (43.7 ms–44.1 ms) | 33,028 / 33,028 |
| x86-64 | 64 B | zero-copy | last record | 26.4 ms (23.0 ms–31.1 ms) | 26.5 ms (23.7 ms–31.6 ms) | 93.1 ms (92.9 ms–94.5 ms) | 93.1 ms (92.9 ms–93.2 ms) | 65,796 / 65,796 |
| x86-64 | 1,024 B | safe | 1/64 in | 222 µs (199 µs–259 µs) | 225 µs (206 µs–254 µs) | 1.22 ms (1.13 ms–1.63 ms) | 1.30 ms (1.15 ms–1.51 ms) | 1,288 / 1,292 |
| x86-64 | 1,024 B | safe | middle | 5.11 ms (4.96 ms–5.39 ms) | 5.09 ms (4.94 ms–5.47 ms) | 44.0 ms (43.8 ms–44.2 ms) | 44.0 ms (43.8 ms–44.3 ms) | 33,032 / 33,036 |
| x86-64 | 1,024 B | safe | last record | 8.84 ms (8.12 ms–9.67 ms) | 8.81 ms (8.24 ms–9.77 ms) | 93.1 ms (93.1 ms–95.4 ms) | 93.3 ms (93.1 ms–94.1 ms) | 65,800 / 65,804 |
| x86-64 | 1,024 B | zero-copy | 1/64 in | 151 µs (140 µs–177 µs) | 155 µs (138 µs–196 µs) | 1.09 ms (1.01 ms–1.62 ms) | 1.13 ms (984 µs–1.48 ms) | 1,284 / 1,284 |
| x86-64 | 1,024 B | zero-copy | middle | 3.53 ms (3.15 ms–3.72 ms) | 3.54 ms (3.13 ms–3.79 ms) | 43.8 ms (43.7 ms–45.2 ms) | 43.9 ms (43.7 ms–44.2 ms) | 33,028 / 33,028 |
| x86-64 | 1,024 B | zero-copy | last record | 6.47 ms (6.05 ms–7.29 ms) | 6.51 ms (6.07 ms–7.13 ms) | 92.9 ms (92.7 ms–93.0 ms) | 92.9 ms (92.7 ms–93.8 ms) | 65,668 / 65,668 |
| aarch64 | 64 B | safe | 1/64 in | 385 µs (349 µs–427 µs) | 399 µs (365 µs–427 µs) | 1.01 ms (966 µs–2.02 ms) | 1.08 ms (1.01 ms–1.55 ms) | 1,284 / 1,288 |
| aarch64 | 64 B | safe | middle | 10.1 ms (9.80 ms–10.5 ms) | 10.1 ms (9.77 ms–10.4 ms) | 43.9 ms (43.6 ms–44.1 ms) | 43.9 ms (43.8 ms–44.7 ms) | 33,028 / 33,032 |
| aarch64 | 64 B | safe | last record | 18.9 ms (18.5 ms–20.2 ms) | 18.9 ms (18.5 ms–20.3 ms) | 93.0 ms (92.9 ms–93.1 ms) | 93.0 ms (92.8 ms–93.8 ms) | 65,796 / 65,800 |
| aarch64 | 64 B | zero-copy | 1/64 in | 313 µs (302 µs–350 µs) | 317 µs (303 µs–365 µs) | 907 µs (868 µs–1.02 ms) | 921 µs (879 µs–1.35 ms) | 1,280 / 1,280 |
| aarch64 | 64 B | zero-copy | middle | 8.52 ms (8.34 ms–9.71 ms) | 8.53 ms (8.34 ms–9.48 ms) | 43.9 ms (43.7 ms–44.0 ms) | 43.8 ms (43.6 ms–44.1 ms) | 33,024 / 33,024 |
| aarch64 | 64 B | zero-copy | last record | 16.8 ms (16.5 ms–19.3 ms) | 16.8 ms (16.6 ms–19.3 ms) | 92.9 ms (92.8 ms–93.0 ms) | 92.9 ms (92.8 ms–93.1 ms) | 65,792 / 65,792 |
| aarch64 | 1,024 B | safe | 1/64 in | 231 µs (203 µs–261 µs) | 238 µs (216 µs–659 µs) | 1.01 ms (982 µs–2.19 ms) | 1.08 ms (1.04 ms–1.97 ms) | 1,284 / 1,288 |
| aarch64 | 1,024 B | safe | middle | 5.05 ms (4.88 ms–5.22 ms) | 5.05 ms (4.73 ms–5.39 ms) | 43.9 ms (43.6 ms–44.6 ms) | 43.9 ms (43.8 ms–44.1 ms) | 33,028 / 33,032 |
| aarch64 | 1,024 B | safe | last record | 8.73 ms (8.42 ms–9.27 ms) | 8.73 ms (8.26 ms–9.36 ms) | 93.1 ms (92.8 ms–93.9 ms) | 93.0 ms (92.9 ms–94.2 ms) | 65,796 / 65,800 |
| aarch64 | 1,024 B | zero-copy | 1/64 in | 161 µs (145 µs–183 µs) | 158 µs (145 µs–225 µs) | 892 µs (878 µs–918 µs) | 912 µs (885 µs–2.21 ms) | 1,280 / 1,280 |
| aarch64 | 1,024 B | zero-copy | middle | 3.00 ms (2.91 ms–3.11 ms) | 2.98 ms (2.88 ms–3.09 ms) | 43.8 ms (43.6 ms–43.9 ms) | 43.8 ms (43.6 ms–44.0 ms) | 33,024 / 33,024 |
| aarch64 | 1,024 B | zero-copy | last record | 5.57 ms (5.47 ms–5.85 ms) | 5.57 ms (5.45 ms–5.76 ms) | 92.9 ms (92.7 ms–93.1 ms) | 92.8 ms (92.7 ms–93.1 ms) | 65,664 / 65,664 |

The columns without and with the option are the same within their spread, except one small cost: cold, at "1/64 in",
the safe reader's medians were 0.07 to 0.08 ms higher with the option on both machines, which is the look for an index
file in a `seek-index/` that is not cached; the zero-copy reader's differed by 0 to 0.04 ms. A seek to the last record
of this half-full segment costs half of a full segment's scan (5.6 to 26.5 ms warm, 93 ms cold).

### Forward seeks within a segment

`seek-forward` after a seek to the segment's first record: 16 equal steps through the fifth segment (8 MiB each), 16
steps of about 64 KiB from its first record (630 records of 64 B or 61 of 1,024 B each), and 16 steps through the
records of the newest segment (4 MiB each). Warm; 50 runs, so each "one step" cell is 800 values.

| Machine | Payload | Reader | Steps | one step, without index | one step, with index | 16 steps, without / with (median) |
|---|---|---|---|---|---|---|
| x86-64 | 64 B | safe | sealed, 1/16 of the segment | 2.36 ms (2.26 ms–2.99 ms) | 37 µs (24 µs–107 µs) | 37.9 ms / 647 µs |
| x86-64 | 64 B | safe | sealed, about 64 KiB | 25 µs (22 µs–42 µs) | 25 µs (23 µs–60 µs) | 406 µs / 407 µs |
| x86-64 | 64 B | safe | newest, 1/16 of its records | 1.19 ms (1.13 ms–1.71 ms) | 1.19 ms (1.13 ms–1.75 ms) | 19.0 ms / 19.1 ms |
| x86-64 | 64 B | zero-copy | sealed, 1/16 of the segment | 3.50 ms (3.03 ms–4.09 ms) | 54 µs (27 µs–106 µs) | 55.7 ms / 882 µs |
| x86-64 | 64 B | zero-copy | sealed, about 64 KiB | 34 µs (26 µs–56 µs) | 34 µs (26 µs–66 µs) | 547 µs / 554 µs |
| x86-64 | 64 B | zero-copy | newest, 1/16 of its records | 1.75 ms (1.46 ms–2.08 ms) | 1.74 ms (1.45 ms–2.02 ms) | 27.6 ms / 27.8 ms |
| x86-64 | 1,024 B | safe | sealed, 1/16 of the segment | 1.28 ms (1.21 ms–1.49 ms) | 29 µs (26 µs–82 µs) | 20.6 ms / 510 µs |
| x86-64 | 1,024 B | safe | sealed, about 64 KiB | 16 µs (14 µs–37 µs) | 17 µs (15 µs–38 µs) | 269 µs / 270 µs |
| x86-64 | 1,024 B | safe | newest, 1/16 of its records | 650 µs (599 µs–758 µs) | 651 µs (597 µs–829 µs) | 10.4 ms / 10.4 ms |
| x86-64 | 1,024 B | zero-copy | sealed, 1/16 of the segment | 920 µs (825 µs–1.13 ms) | 32 µs (29 µs–79 µs) | 15.1 ms / 558 µs |
| x86-64 | 1,024 B | zero-copy | sealed, about 64 KiB | 16 µs (13 µs–36 µs) | 16 µs (13 µs–34 µs) | 255 µs / 254 µs |
| x86-64 | 1,024 B | zero-copy | newest, 1/16 of its records | 461 µs (394 µs–589 µs) | 462 µs (391 µs–549 µs) | 7.35 ms / 7.43 ms |
| aarch64 | 64 B | safe | sealed, 1/16 of the segment | 2.45 ms (2.37 ms–2.67 ms) | 45 µs (31 µs–118 µs) | 39.2 ms / 771 µs |
| aarch64 | 64 B | safe | sealed, about 64 KiB | 29 µs (26 µs–62 µs) | 29 µs (26 µs–65 µs) | 480 µs / 479 µs |
| aarch64 | 64 B | safe | newest, 1/16 of its records | 1.26 ms (1.19 ms–1.38 ms) | 1.26 ms (1.19 ms–1.37 ms) | 20.1 ms / 20.2 ms |
| aarch64 | 64 B | zero-copy | sealed, 1/16 of the segment | 2.13 ms (2.08 ms–2.49 ms) | 51 µs (40 µs–106 µs) | 34.2 ms / 864 µs |
| aarch64 | 64 B | zero-copy | sealed, about 64 KiB | 30 µs (28 µs–73 µs) | 30 µs (28 µs–75 µs) | 489 µs / 489 µs |
| aarch64 | 64 B | zero-copy | newest, 1/16 of its records | 1.08 ms (1.05 ms–1.16 ms) | 1.08 ms (1.05 ms–1.23 ms) | 17.3 ms / 17.3 ms |
| aarch64 | 1,024 B | safe | sealed, 1/16 of the segment | 1.19 ms (1.12 ms–1.81 ms) | 35 µs (32 µs–88 µs) | 19.0 ms / 603 µs |
| aarch64 | 1,024 B | safe | sealed, about 64 KiB | 19 µs (16 µs–37 µs) | 19 µs (16 µs–56 µs) | 320 µs / 318 µs |
| aarch64 | 1,024 B | safe | newest, 1/16 of its records | 635 µs (559 µs–711 µs) | 641 µs (578 µs–710 µs) | 10.1 ms / 10.3 ms |
| aarch64 | 1,024 B | zero-copy | sealed, 1/16 of the segment | 741 µs (720 µs–801 µs) | 41 µs (38 µs–86 µs) | 11.9 ms / 698 µs |
| aarch64 | 1,024 B | zero-copy | sealed, about 64 KiB | 19 µs (17 µs–37 µs) | 19 µs (17 µs–52 µs) | 318 µs / 318 µs |
| aarch64 | 1,024 B | zero-copy | newest, 1/16 of its records | 373 µs (346 µs–458 µs) | 375 µs (352 µs–463 µs) | 6.02 ms / 6.04 ms |

A forward seek in the reader's segment continues from the cursor when the checkpoint marker covers it (ADR 0046), so a
step without the index reads only the records between the two targets: 8 MiB of them cost 0.74 to 3.5 ms. With the
option, a forward seek uses the index only when the estimated distance is at least 256 KiB (ADR 0045, step 1): at every
8 MiB step it did (29 to 54 µs a step), and after a landing through the index the next forward seek does not continue
from the cursor, so each step looked the index up again. The short steps never used it and cost the same with and
without the option. In the newest segment, which has no index, the steps continue from the cursor with or without the
option.

### Building, and the writer's open

`seek_index::build` with `rebuild` (what `tickertape index --rebuild` calls), in its own pinned process (`tt-scale op
--op index-build`), writing the eight indexes of the 1 GiB of sealed segments anew: 10 warm runs, 5 cold. The command
itself, `taskset -c 3 tickertape index <dir> --rebuild --json`, timed from outside, process start included: 5 warm, 3
cold; and `tickertape index <dir>` with every index current (it opens each segment's index, header and seal and finds
nothing to do): 5 warm.

| Machine | Payload | build, 1 GiB, warm | per segment | build, 1 GiB, cold | per segment | `tickertape index --rebuild`, warm | cold | `tickertape index`, nothing to do |
|---|---|---|---|---|---|---|---|---|
| x86-64 | 64 B | 368 ms (364 ms–436 ms) | 46.0 ms | 1.57 s (1.57 s–1.58 s) | 197 ms | 375 ms (373 ms–377 ms) | 1.59 s (1.59 s–1.60 s) | 2.31 ms (2.15 ms–2.35 ms) |
| x86-64 | 1,024 B | 179 ms (175 ms–181 ms) | 22.3 ms | 1.57 s (1.57 s–1.57 s) | 197 ms | 184 ms (183 ms–186 ms) | 1.59 s (1.59 s–1.60 s) | 2.15 ms (2.13 ms–2.30 ms) |
| aarch64 | 64 B | 483 ms (477 ms–487 ms) | 60.4 ms | 1.57 s (1.57 s–1.57 s) | 197 ms | 489 ms (485 ms–491 ms) | 1.59 s (1.59 s–1.59 s) | 1.82 ms (1.81 ms–1.86 ms) |
| aarch64 | 1,024 B | 152 ms (150 ms–153 ms) | 19.0 ms | 1.57 s (1.57 s–1.57 s) | 196 ms | 157 ms (156 ms–158 ms) | 1.59 s (1.59 s–1.59 s) | 1.78 ms (1.77 ms–1.82 ms) |

A build reads each segment once and checks every record, so warm it costs about what a seek without an index to the
segment's last record costs (37.6 and 20.3 ms against 46.0 and 22.3 ms per segment on x86-64 with the safe reader).
Cold, it is bound by reading from the local SSD: 1.57 s per GiB on both machines at both payloads, the same rate as the
cold seeks without an index. The index files take 49,256 bytes per 128 MiB segment at both payloads (394,048 bytes for
the eight).

`Writer::open` after a clean close, warm, with `tt-scale op --op writer-open-clean` (the open alone; a checkpoint and the
close follow, untimed), in three states of `seek-index/`: absent (moved out of the journal directory), with the eight
current indexes only (nothing to remove: the open lists the directory and syncs it), and with one more file named like
the index of the newest segment, the marker's (a copy of another index), which the open removed in every run (9 files
before, 8 after). The table is from a script run after the session on the same VMs, 30 rounds, the order of the three
states rotated each round. In the session itself they ran in a fixed order (absent, current, one out-of-date), and the
third had the lowest median in three of the four cases, by up to 0.3 ms (by 1.2 to 1.5 ms in the first x86-64
session); those runs are not reported.

| Machine | Payload | no `seek-index/` | current indexes only | one out-of-date index, removed |
|---|---|---|---|---|
| x86-64 | 64 B | 6.37 ms (5.97 ms–7.38 ms) | 6.66 ms (6.16 ms–7.29 ms) | 6.55 ms (6.26 ms–7.37 ms) |
| x86-64 | 1,024 B | 6.97 ms (6.63 ms–7.43 ms) | 7.14 ms (6.81 ms–7.55 ms) | 7.08 ms (6.74 ms–7.45 ms) |
| aarch64 | 64 B | 5.11 ms (4.60 ms–5.29 ms) | 5.19 ms (4.65 ms–5.53 ms) | 5.23 ms (4.81 ms–5.49 ms) |
| aarch64 | 1,024 B | 5.11 ms (4.67 ms–5.41 ms) | 5.20 ms (4.70 ms–5.53 ms) | 5.23 ms (4.82 ms–5.79 ms) |

The three states are 0.1 to 0.3 ms apart in their medians, with ranges that overlap; the position in the rotation
moved the medians by up to 0.5 ms (x86-64) and 0.4 ms (aarch64). The cost of listing and syncing a `seek-index/` of
eight files, and of removing one out-of-date index, is not resolved here beyond that. With 10,000 index files a writer's
open took 6.1 ms longer (2026-11-25, below).

### What these figures do not show

- **What the index gives up.** With `ReaderConfig::seek_index(true)` the records between the segment's first record and
  the record a seek lands on are not read, so damage there, at which a seek without the option stops, goes unnoticed;
  and a stale or forged index can lead a seek off the current chain where a payload holds suitable bytes
  (`Reader::seek`, crash contract C15). It is off by default.
- **Where it applies.** Only sealed segments before the checkpoint marker's segment have indexes, and only after
  `tickertape index` ran; the newest segment never has one. Mapped and durable-only readers use an index only for a
  segment before the marker's, so in an archive, which has no marker, only the safe visible reader does.
- Not measured: segment sizes other than 128 MiB; journals of many segments, where the directory listing that every seek
  starts with dominates (2026-11-25, below); a writer running and sealing segments meanwhile; repeated seeks in one
  process; the durable-only reader; archives; xfs or other filesystems; bare metal; machines other than these two types.

## 2026-11-25, replay and recovery at scale: the gaps of 1.0.0 (Google Cloud VMs)

(The costs of retention in this section were changed afterwards by ADR 0051: [below](#2026-11-25-retention-on-journals-of-many-segments-google-cloud-vm-linux-x86-64).)

What the scale run of 2026-11-20 ([below](#replay-and-recovery-at-scale-tt-scale-dd450d1)) left out: the journals of
10,000 and 20,000 segments (they failed before #38), archived history, missing and corrupt segments, catch-up under a
live writer, the zero-copy reader, and seeks with the seek index of ADR 0045 (issue #62). **Cloud virtual machines with
local NVMe SSD, not bare metal**, of the same machine types as that run.

| | x86-64 | aarch64 |
|---|---|---|
| Machine type, zone | `c3-standard-4-lssd`, us-east4-a (the earlier run: us-central1-a) | `c4a-standard-4-lssd`, us-east4-a (the earlier run: us-east1-b) |
| CPU, memory | Intel Xeon Platinum 8481C, 4 vCPUs (2 cores), 16.8 GB | Google Axion (Neoverse-V2), 4 vCPUs (4 cores), 16 GB |
| Commits | `e7bba3e` (core of `main` at `85726e3`) for everything but the seek index; `01c6495` (merged with `main` at `d2074c1`, which has the seek index of #72, before the follow-ups of #74) for that | `01c6495` for everything |
| Date (UTC) | 2026-11-25, 13:48 to 17:56 | 2026-11-25, 18:00 to 20:22 (the session stopped by hand at 20:14, the damage run after it) |

Both: Ubuntu 24.04, kernel 7.0.0-1017-gcp, rustc 1.100.0; one local NVMe SSD (375 GB), ext4 formatted with `-E
lazy_itable_init=0,lazy_journal_init=0`, mounted `rw,noatime`, I/O scheduler `none`; `vm.dirty_ratio` 20,
`vm.dirty_background_ratio` 10; soft limit on open files 1,024. The branch commits are those of the pull request that
adds this section. The result files were written to the same filesystem. Records are 1,024 B (1,064 B in the journal).

**Commands.** `benches/scale-session.sh /data/j <out>` with `TT_SCALE` (this core) and `TT_SCALE_BASE` (the same tool
built against the 1.0.0 core: `git checkout v1.0.0 -- crates/tickertape`, `--no-default-features`; `benches/README.md`,
"Another core") and `TT_CLI`. The stages were `counts many sizes seeks damage big` on x86-64 and `counts many sizes
seeks compare big` on aarch64 (stopped during the archive of `big`), followed there by `tt-scale damage --segments
10000 --segment-size 1048576` (warm). On x86-64 the `seeks` stage of `e7bba3e` (without an index) is not reported, and
two things ran after the session on the same VM: the stage `compare`, with the script of `20163aa` (which corrected the
order of the warm runs: in the x86-64 session each warm run of the 1.0.0 core came right after a cold run, so only its
cold figures and `compare` are used; `compare` ran the script of `20163aa` as an untracked copy in a tree at `e7bba3e`,
so not a clean tree), and the stage `seeks` and `tt-scale damage --segments 10000 --segment-size
1048576` at `01c6495`. Files: `seg-*`, `size-*`, `big-*`, `catchup-*`, `damage-*`, `compare-*`, `index-*` `.json` with
`environment.txt` and `progress.txt` per session, kept outside the repository.

**How to read the tables.** `tt-scale` runs each operation in a process of its own; values are wall times, the range
over two repeats unless a table says otherwise. "Cold" drops the page cache before every operation (`cache_dropped`
true in every cold run). A seek's time starts after the reader is open, so the open has already brought the directory
into the cache also in the cold runs (`reader open` shows what that costs). The largest journals do not fit in memory,
so their "warm" rows are not warm. `measure` appends and checkpoints, so a later measurement of the same journal can
see a few more segments. "seek middle" lands 25 % or 75 % into its segment, as in the earlier run; "seek across a full
segment" targets the last record of a full segment, the longest scan a seek can cost. Zero-copy reader peak RSS counts
the mapped journal pages.

### Journals of 10,000 segments and more

**They work now**, on both machines, with every operation of the list, and up to 100,000 segments of 64 KiB. No
operation failed. With the soft limit on open files lowered to 64, at 10 to 20,000 segments, `reader-open`, the seeks,
`status`, `verify`, a retention plan, the writer open after a clean close and without a marker, and the checkpoint loop
with retention ran, and none failed (`replay`, `list-segments`, the open after a kill and the checkpoint loop without
retention did not run under that limit). With the limit at 1,024, at 10 to 20,000 segments, the most files a process
had open, counted about every millisecond (3 of them are the process's own), was 5 for a reader or a seek, 4 for
`status`, `verify` and a retention plan, 8 for a writer, and for a writer opening without a marker 12, also at 1,000,
10,000 and 20,000 segments (10 on x86-64 and 11 on aarch64 at 10 segments). Open files were not sampled at 100,000
segments. A writer that built 20,000 segments without a checkpoint in between held 7 files; its first checkpoint, at
the end, covered 19.5 GiB and took 6.7 s (x86-64) and 7.8 s (aarch64).

What grows, and where it becomes a limit:

- **Directory reads.** Opening a reader or a writer and *each* seek read the whole directory: about 0.29 µs per segment
  when cached (2.9 ms at 10,000, 5.8 ms at 20,000, 29 ms at 100,000 on x86-64). From 10,000 segments on, a seek costs
  that and nothing else matters: 16 forward seeks (ADR 0046) cost 16 listings, and the seek index does not help either
  ("Seek index" below). With nothing cached, reading the directory took 18 ms and 35 ms (x86-64; 6 to 7 ms and 13 to 15
  ms on aarch64) at 10,000 and 20,000 segments, and 215 ms (70 to 78 ms) at 100,000.
- **Headers.** `list_segments` and a retention plan open every segment: 30 ms at 10,000 and 60 ms at 20,000 segments
  with a warm cache, but 2 s and 4 s (x86-64) or 0.9 s and 1.9 s (aarch64) when the segments' metadata is cold. `verify`
  opens them too, and reads every record: 2.2 s warm (x86-64) at 10,000 segments.
- **Retention.** With a retention policy configured, every checkpoint makes a plan: a blocking checkpoint of one record
  went from 0.3 ms to 29 ms at 10,000 segments, 67 ms at 20,000 and 336 ms at 100,000 (x86-64; aarch64 alike), and a
  cold writer open from 28 ms to 2.1 s, 50 ms to 4.0 s and 0.25 s to 21 s. Without retention a checkpoint does not
  depend on the number of segments. (On aarch64 the median checkpoint without retention was 0.04 ms in every cold
  measurement and in both warm ones of the 100,000-segment journal and one of the 20,000-segment one's two, and 0.43 to
  0.49 ms otherwise; unexplained.)
- **Removing segments** reads the directory again before every removal (`retention::apply`), so the cost of deleting a
  segment grows with the number of segments left: 0.6 ms at 1,000, 2 ms at 4,000 to 5,000, 8 ms at 20,000 (x86-64; the
  journal of 10,000 had been halved by `archive` before). Deleting a tenth of the 20,000 took 14 to 16 s.
- Without a marker, recovery reads every byte, as before (37.6 s cold for 20 GB on x86-64); its open files do not grow,
  but the first checkpoint after it opens every segment again: 0.2 s at 10,000 and 1.7 to 2.6 s at 100,000 on x86-64
  (26 ms and 0.24 s on aarch64).

Every operation on journals of every shape:

x86-64 (`85726e3`):

| Journal | Cache | read directory | `list_segments` | `status` | retention plan | `verify` | writer open, clean | after kill | without marker | checkpoint after that | Peak RSS |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 10 × 1 MiB | cold | 246–560 µs | 2.68–2.69 ms | 1.62–1.69 ms | 2.50–2.51 ms | 15.1–15.5 ms | 1.48–2.30 ms | 1.99–2.19 ms | 23.4–24.4 ms | 1.03–1.25 ms | 5 MiB |
| 10 × 1 MiB | warm | 29.8–35.7 µs | 67.6–74.6 µs | 369–489 µs | 76.4–84.6 µs | 2.40–2.59 ms | 383–446 µs | 416–497 µs | 1.97–2.08 ms | 1.05–1.09 ms | 5 MiB |
| 1,000 × 1 MiB | cold | 1.74–1.94 ms | 194–199 ms | 8.38–8.60 ms | 192–206 ms | 1.43 s | 4.52–4.77 ms | 4.45–4.96 ms | 1.87–1.90 s | 24.9–25.2 ms | 5 MiB |
| 1,000 × 1 MiB | warm | 280–304 µs | 2.66–2.99 ms | 1.70–1.91 ms | 3.01–3.37 ms | 225–226 ms | 1.13–1.19 ms | 1.14–1.34 ms | 180–182 ms | 27.3–27.7 ms | 5 MiB |
| 10,000 × 1 MiB | cold | 18.0–18.1 ms | 2.07–2.15 s | 64.8–66.2 ms | 1.97–1.98 s | 14.2 s | 27.6–27.7 ms | 26.1–27.8 ms | 17.7–18.1 s | 178–237 ms | 8 MiB |
| 10,000 × 1 MiB | warm | 2.49–2.56 ms | 29.6–31.2 ms | 14.0–15.5 ms | 34.0–34.2 ms | 2.17–2.19 s | 7.71–8.19 ms | 8.11–8.32 ms | 1.73 s | 177–206 ms | 8 MiB |
| 20,000 × 1 MiB | cold | 34.6–35.1 ms | 3.94–4.04 s | 134–135 ms | 3.94–3.97 s | 28.5 s | 49.8–50.5 ms | 44.5–45.2 ms | 37.6 s | 366–511 ms | 10 MiB |
| 20,000 × 1 MiB | warm | 4.97–5.00 ms | 59.3–61.4 ms | 28.2–71.6 ms | 66.2–877 ms | 13.7–18.4 s | 14.5–16.1 ms | 14.6–14.9 ms | 20.4–25.6 s | 418–452 ms | 10 MiB |
| 100,000 × 64 KiB | cold | 215–216 ms | 20.6–21.0 s | 677–702 ms | 21.0–21.1 s | 45.1–47.4 s | 242–254 ms | 238–242 ms | 47.0–47.9 s | 1.74–2.41 s | 33 MiB |
| 100,000 × 64 KiB | warm | 24.7–25.0 ms | 293–296 ms | 150–156 ms | 317–322 ms | 1.69–1.70 s | 77.6–78.7 ms | 78.0–78.9 ms | 1.90–1.92 s | 2.39–2.64 s | 33 MiB |
| 10 × 16 MiB | cold | 225–227 µs | 2.47–2.53 ms | 2.70–2.72 ms | 2.75–2.87 ms | 241 ms | 5.28–5.45 ms | 14.9–18.5 ms | 266 ms | 1.47–1.58 ms | 20 MiB |
| 10 × 16 MiB | warm | 30.6–37.9 µs | 70.9–75.6 µs | 618 µs–1.12 ms | 81.6–95.4 µs | 30.4–32.4 ms | 1.39–1.83 ms | 7.55–10.2 ms | 28.5–28.6 ms | 1.33–1.55 ms | 20 MiB |
| 10 × 128 MiB | cold | 281–287 µs | 2.58–2.60 ms | 2.68–2.71 ms | 3.14–3.23 ms | 1.93–1.94 s | 26.6 ms | 116–140 ms | 2.12 s | 1.40–1.47 ms | 132 MiB |
| 10 × 128 MiB | warm | 31.5–38.1 µs | 75.0–81.4 µs | 614 µs–1.06 ms | 81.8–119 µs | 245–257 ms | 10.0–11.0 ms | 64.1–77.9 ms | 228 ms | 1.23–1.24 ms | 132 MiB |
| 10 × 1 GiB | cold | 314–318 µs | 3.21–3.23 ms | 3.01–3.12 ms | 4.75–4.80 ms | 15.5 s | 186–188 ms | 914 ms–1.10 s | 17.0 s | 1.26–1.59 ms | 1,028 MiB |
| 10 × 1 GiB | warm | 29.4–55.3 µs | 71.3–79.5 µs | 613 µs–1.13 ms | 103–112 µs | 1.93–2.05 s | 79.5–81.7 ms | 187–259 ms | 1.76–1.77 s | 1.35–1.36 ms | 1,028 MiB |
| 512 × 128 MiB | cold | 714 µs–1.45 ms | 105 ms | 6.68–6.82 ms | 108–115 ms | 93.1 s | 27.4–28.9 ms | 94.2–118 ms | 93.4 s | 10.3–14.5 ms | 132 MiB |
| 512 × 128 MiB | warm | 190–218 µs | 1.19–1.25 ms | 1.31–3.85 ms | 1.69–75.9 ms | 77.2 s | 10.0–10.5 ms | 54.0–64.2 ms | 93.6 s | 14.8–15.9 ms | 132 MiB |

aarch64 (`d2074c1`):

| Journal | Cache | read directory | `list_segments` | `status` | retention plan | `verify` | writer open, clean | after kill | without marker | checkpoint after that | Peak RSS |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 10 × 1 MiB | cold | 56.3–332 µs | 1.25–1.46 ms | 1.57–1.61 ms | 1.36–1.64 ms | 14.4 ms | 1.44–1.48 ms | 1.32–2.04 ms | 16.1 ms | 1.22–1.51 ms | 4 MiB |
| 10 × 1 MiB | warm | 33.6–36.3 µs | 69.7–76.0 µs | 288–312 µs | 93.3–97.9 µs | 2.40–2.44 ms | 393–484 µs | 418–529 µs | 1.81–1.87 ms | 470–488 µs | 4 MiB |
| 1,000 × 1 MiB | cold | 1.47–1.58 ms | 93.0–94.2 ms | 6.85–7.03 ms | 94.4–94.8 ms | 1.54 s | 3.03–3.38 ms | 2.37–3.00 ms | 1.54 s | 3.07–3.13 ms | 4 MiB |
| 1,000 × 1 MiB | warm | 298–301 µs | 2.99–3.00 ms | 1.60–1.81 ms | 3.28–3.69 ms | 165–166 ms | 1.04–1.18 ms | 1.02–1.11 ms | 121–122 ms | 2.99–3.02 ms | 4 MiB |
| 10,000 × 1 MiB | cold | 5.81–7.42 ms | 936–940 ms | 39.8–41.4 ms | 947–951 ms | 15.4 s | 12.5–12.6 ms | 12.4–13.8 ms | 15.4 s | 25.9–26.3 ms | 7 MiB |
| 10,000 × 1 MiB | warm | 2.35 ms | 27.6–29.2 ms | 13.4–14.7 ms | 32.3–36.0 ms | 1.54–1.56 s | 7.04–7.05 ms | 7.00–7.06 ms | 1.20–1.21 s | 26.2–26.3 ms | 7 MiB |
| 20,000 × 1 MiB | cold | 13.3–14.8 ms | 1.87–1.88 s | 94.4–97.5 ms | 1.93 s | 30.8 s | 24.8–26.0 ms | 24.6 ms | 30.8 s | 67.6–75.7 ms | 10 MiB |
| 20,000 × 1 MiB | warm | 4.62–4.64 ms | 55.0–59.6 ms | 30.0–37.3 ms | 68.0 ms–1.36 s | 15.0–15.8 s | 13.4–14.0 ms | 13.5–13.6 ms | 19.3–20.4 s | 55.8–84.0 ms | 10 MiB |
| 100,000 × 64 KiB | cold | 70.1–77.7 ms | 8.96–8.99 s | 450–456 ms | 9.00–9.08 s | 18.6–18.8 s | 124–132 ms | 123–124 ms | 19.1–21.2 s | 240–244 ms | 33 MiB |
| 100,000 × 64 KiB | warm | 23.2–23.3 ms | 319 ms | 158–159 ms | 344–348 ms | 1.49 s | 73.9–75.3 ms | 74.0–74.1 ms | 1.64–1.65 s | 245 ms | 33 MiB |
| 10 × 16 MiB | cold | 173–502 µs | 1.47–2.33 ms | 1.63–2.21 ms | 2.12–2.33 ms | 280–281 ms | 3.25–3.92 ms | 9.22–11.6 ms | 306 ms | 570–575 µs | 19 MiB |
| 10 × 16 MiB | warm | 34.1–36.0 µs | 71.1–83.0 µs | 450–547 µs | 80.5–90.6 µs | 24.4–25.0 ms | 893 µs–1.03 ms | 3.36–7.10 ms | 18.8–18.9 ms | 403–428 µs | 19 MiB |
| 10 × 128 MiB | cold | 161–176 µs | 1.37–1.46 ms | 1.18–2.13 ms | 1.43–1.46 ms | 2.27 s | 18.7–20.4 ms | 111–129 ms | 2.48 s | 485–542 µs | 131 MiB |
| 10 × 128 MiB | warm | 34.9–36.3 µs | 68.7–69.5 µs | 496–547 µs | 98.4–106 µs | 184–189 ms | 5.52–5.87 ms | 40.5–160 ms | 144–145 ms | 415–444 µs | 131 MiB |
| 10 × 1 GiB | cold | 233–532 µs | 2.30–2.47 ms | 1.78–2.62 ms | 2.43–2.91 ms | 18.2 s | 134–135 ms | 913 ms–1.04 s | 19.8 s | 459–472 µs | 1,027 MiB |
| 10 × 1 GiB | warm | 35.4–48.9 µs | 66.4–80.3 µs | 486–543 µs | 83.5–107 µs | 1.43–1.45 s | 40.4 ms | 117–185 ms | 1.15–1.16 s | 816–836 µs | 1,027 MiB |
| 512 × 128 MiB | cold | 650–883 µs | 48.4–48.6 ms | 3.58–4.56 ms | 49.5–49.8 ms | 101 s | 18.7–19.2 ms | 94.6–111 ms | 101 s | 1.88–2.07 ms | 131 MiB |
| 512 × 128 MiB | warm | 149–163 µs | 1.42–1.45 ms | 1.21–2.13 ms | 1.83–37.1 ms | 87.3 s | 5.79–6.05 ms | 22.3–39.1 ms | 101 s | 1.91–1.98 ms | 131 MiB |

Checkpoints, retention and building (the checkpoint loops are 100 blocking checkpoints after one record each; building
checkpoints every 64 segments; the deletion plans and deletes a tenth of the segments, twice, cold):

| Machine | Journal | Build (checkpoint every 64 segments) | checkpoint p50, no retention | checkpoint p50 / max, retention configured | writer open with retention, cold / warm | delete a tenth of the segments (cold): plan + apply, per segment |
|---|---|---|---|---|---|---|
| x86-64 (`85726e3`) | 10 × 1 MiB | 25.1 ms (417 MB/s) | 297–299 µs | 305–393 µs / 957 µs–2.15 ms | 3.10–3.56 ms / 439–444 µs | 1 of 16: 257 µs + 573 µs (573 µs); 1 of 15: 371 µs + 644 µs (644 µs) |
| x86-64 (`85726e3`) | 1,000 × 1 MiB | 2.57 s (408 MB/s) | 298–347 µs | 2.79–2.80 ms / 3.45–4.11 ms | 191–204 ms / 3.66–4.41 ms | 100 of 1,006: 16.8 ms + 67.3 ms (673 µs); 100 of 906: 16.7 ms + 63.9 ms (639 µs) |
| x86-64 (`85726e3`) | 10,000 × 1 MiB | 25.7 s (408 MB/s) | 281–282 µs | 29.1–30.3 ms / 35.6 ms | 2.10–2.20 s / 38.0–42.4 ms | 1,000 of 5,000: 139 ms + 2.12 s (2.12 ms); 1,000 of 4,000: 146 ms + 1.72 s (1.72 ms) |
| x86-64 (`85726e3`) | 20,000 × 1 MiB | 51.6 s (406 MB/s) | 289–290 µs | 66.7–67.6 ms / 69.8–70.9 ms | 3.90–4.07 s / 83.6 ms–2.87 s | 2,000 of 20,006: 338 ms + 16.0 s (8.01 ms); 2,000 of 18,006: 305 ms + 14.5 s (7.25 ms) |
| x86-64 (`85726e3`) | 100,000 × 64 KiB | 19.4 s (339 MB/s) | 217–222 µs | 335–336 ms / 342–352 ms | 21.1–21.6 s / 414–417 ms | – |
| aarch64 (`d2074c1`) | 10 × 1 MiB | 25.2 ms (416 MB/s) | 433–494 µs | 375–485 µs / 506–557 µs | 2.03–2.56 ms / 405–431 µs | 1 of 16: 221 µs + 231 µs (231 µs); 1 of 15: 205 µs + 158 µs (158 µs) |
| aarch64 (`d2074c1`) | 1,000 × 1 MiB | 2.99 s (350 MB/s) | 431–493 µs | 2.83–2.86 ms / 3.22–3.36 ms | 94.4–95.2 ms / 4.16–4.90 ms | 100 of 1,006: 10.3 ms + 44.1 ms (441 µs); 100 of 906: 9.85 ms + 40.1 ms (401 µs) |
| aarch64 (`d2074c1`) | 10,000 × 1 MiB | 30.1 s (349 MB/s) | 430–447 µs | 31.4–31.6 ms / 37.0–37.1 ms | 960 ms–1.42 s / 39.3–43.9 ms | 1,000 of 5,000: 87.7 ms + 1.79 s (1.79 ms); 1,000 of 4,000: 84.1 ms + 1.39 s (1.39 ms) |
| aarch64 (`d2074c1`) | 20,000 × 1 MiB | 60.4 s (347 MB/s) | 39.7–430 µs | 62.9–66.9 ms / 72.0 ms–1.93 s | 1.88–2.20 s / 81.4 ms–1.84 s | 2,000 of 20,006: 210 ms + 14.9 s (7.44 ms); 2,000 of 18,006: 207 ms + 13.3 s (6.66 ms) |
| aarch64 (`d2074c1`) | 100,000 × 64 KiB | 21.1 s (310 MB/s) | 40.9–41.7 µs | 359–363 ms / 365–367 ms | 9.21–10.2 s / 432–437 ms | – |

### Readers: opening, seeking, replay

Both readers. `16 forward seeks` is a seek through a full segment in 16 equal steps (after a seek to its first record),
the total of the 16 seeks, each with the record after it.

x86-64 (`85726e3`):

| Journal | Cache | Reader | open | seek first | seek middle | seek across a full segment | seek last | 16 forward seeks | `seek_to_end` | replay | replay rate (M records/s) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 10 × 1 MiB | cold | safe | 798–934 µs | 554–589 µs | 1.58–1.62 ms | 2.06–2.22 ms | 2.22–2.23 ms | 1.31–1.43 ms | 615–641 µs | 20.4–21.9 ms | 0.49–0.53 |
| 10 × 1 MiB | cold | zero-copy | 876–983 µs | 278–319 µs | 1.54–1.55 ms | 2.00 ms | 2.17–2.19 ms | 1.88–1.90 ms | 721–866 µs | 20.4–22.0 ms | 0.49–0.53 |
| 10 × 1 MiB | warm | safe | 55.6–59.0 µs | 77.5–86.6 µs | 178–234 µs | 238–261 µs | 151–177 µs | 318 µs | 36.8–42.1 µs | 1.69–2.15 ms | 4.36–5.52 |
| 10 × 1 MiB | warm | zero-copy | 58.4–59.3 µs | 55.6–57.3 µs | 144 µs | 168–195 µs | 114–123 µs | 286–290 µs | 38.7–38.9 µs | 1.41–1.46 ms | 6.39–6.64 |
| 1,000 × 1 MiB | cold | safe | 2.08–2.13 ms | 806–857 µs | 2.10–2.12 ms | 2.69–2.70 ms | 2.27–2.33 ms | 5.04–5.09 ms | 931–986 µs | 1.92–1.99 s | 0.50–0.51 |
| 1,000 × 1 MiB | cold | zero-copy | 2.07–2.44 ms | 477–498 µs | 2.09–2.14 ms | 2.57–2.71 ms | 2.29–2.40 ms | 5.45–5.49 ms | 1.11–1.16 ms | 1.84–2.00 s | 0.49–0.54 |
| 1,000 × 1 MiB | warm | safe | 353–358 µs | 348–375 µs | 487–559 µs | 510–532 µs | 457–508 µs | 4.54–4.59 ms | 307–312 µs | 220 ms | 4.47 |
| 1,000 × 1 MiB | warm | zero-copy | 358–378 µs | 330–336 µs | 415–420 µs | 447–450 µs | 390–397 µs | 4.50–4.54 ms | 323–358 µs | 155–159 ms | 6.18–6.37 |
| 10,000 × 1 MiB | cold | safe | 18.5–18.6 ms | 3.44–3.48 ms | 4.62–4.72 ms | 5.42–5.70 ms | 5.04–5.05 ms | 46.3 ms | 3.49–3.53 ms | 17.9 s | 0.55 |
| 10,000 × 1 MiB | cold | zero-copy | 18.6–18.9 ms | 3.14–3.19 ms | 4.56–4.80 ms | 5.33–5.41 ms | 4.69–4.80 ms | 46.3–46.5 ms | 3.73–3.75 ms | 18.0–18.3 s | 0.54–0.55 |
| 10,000 × 1 MiB | warm | safe | 2.93–2.96 ms | 2.93–2.98 ms | 3.08–3.11 ms | 3.08–3.09 ms | 3.02–3.04 ms | 45.3–45.5 ms | 2.89–2.91 ms | 2.15–2.28 s | 4.33–4.59 |
| 10,000 × 1 MiB | warm | zero-copy | 2.95–2.96 ms | 2.93–2.95 ms | 3.01–3.03 ms | 3.04 ms | 2.97–3.00 ms | 45.2–45.4 ms | 2.87–2.92 ms | 1.51–1.55 s | 6.37–6.52 |
| 20,000 × 1 MiB | cold | safe | 36.0–41.5 ms | 6.47–6.52 ms | 7.79–7.81 ms | 8.06–8.08 ms | 7.66–7.77 ms | 91.6 ms | 6.54–6.55 ms | 37.4–37.6 s | 0.52–0.53 |
| 20,000 × 1 MiB | cold | zero-copy | 41.2–41.3 ms | 6.01–6.15 ms | 7.52–7.64 ms | 8.19–8.30 ms | 7.71–7.72 ms | 92.1–93.2 ms | 6.66–6.72 ms | 37.7–38.0 s | 0.52 |
| 20,000 × 1 MiB | warm | safe | 5.79–5.89 ms | 5.78–5.79 ms | 6.01–6.35 ms | 5.98–6.05 ms | 5.94–5.95 ms | 91.4–91.5 ms | 5.77 ms | 18.5–22.8 s | 0.86–1.06 |
| 20,000 × 1 MiB | warm | zero-copy | 5.83–5.88 ms | 5.78–5.79 ms | 5.93–5.94 ms | 5.92–5.95 ms | 5.84–5.86 ms | 90.8–91.4 ms | 5.81–5.83 ms | 21.0–28.5 s | 0.69–0.94 |
| 100,000 × 64 KiB | cold | safe | 194–195 ms | 29.5 ms | 30.0–30.2 ms | 29.9–30.5 ms | 29.5–29.6 ms | 458 ms | 29.4–29.5 ms | 46.2–46.8 s | 0.13 |
| 100,000 × 64 KiB | cold | zero-copy | 195–221 ms | 29.3–29.4 ms | 30.1–30.3 ms | 29.8–29.9 ms | 29.3–29.4 ms | 459–460 ms | 29.4–29.7 ms | 47.6–47.7 s | 0.13 |
| 100,000 × 64 KiB | warm | safe | 29.1–29.6 ms | 28.8–28.9 ms | 28.9–29.0 ms | 28.6–29.0 ms | 28.7–28.8 ms | 458 ms | 28.8–28.9 ms | 1.91–1.92 s | 3.18–3.19 |
| 100,000 × 64 KiB | warm | zero-copy | 29.0–29.1 ms | 28.8–28.9 ms | 28.8–28.9 ms | 28.7–28.8 ms | 28.6–28.8 ms | 458–462 ms | 28.9–29.0 ms | 1.87–1.88 s | 3.25–3.26 |
| 10 × 16 MiB | cold | safe | 575–594 µs | 609–614 µs | 6.30–6.31 ms | 22.4–22.6 ms | 11.3–11.7 ms | 21.5 ms | 456–488 µs | 239 ms | 0.69 |
| 10 × 16 MiB | cold | zero-copy | 581–676 µs | 215–226 µs | 5.93–6.03 ms | 22.4–22.7 ms | 11.2–11.5 ms | 21.6–21.7 ms | 618–659 µs | 240 ms | 0.69 |
| 10 × 16 MiB | warm | safe | 58.4–63.1 µs | 77.0–97.6 µs | 1.74–2.56 ms | 2.25–2.51 ms | 1.25–1.72 ms | 2.29–2.44 ms | 34.9–36.3 µs | 30.5 ms | 4.91–4.92 |
| 10 × 16 MiB | warm | zero-copy | 61.5–61.9 µs | 56.3–56.7 µs | 1.44–1.63 ms | 1.87–1.89 ms | 954 µs–1.06 ms | 1.96–2.06 ms | 40.3–42.2 µs | 20.7 ms | 7.23 |
| 10 × 128 MiB | cold | safe | 548–565 µs | 528–553 µs | 45.3–45.4 ms | 181 ms | 91.0–91.3 ms | 180 ms | 674–689 µs | 1.91 s | 0.69 |
| 10 × 128 MiB | cold | zero-copy | 556–571 µs | 191–197 µs | 45.1–45.4 ms | 181 ms | 91.2 ms | 181 ms | 774–842 µs | 1.91 s | 0.69 |
| 10 × 128 MiB | warm | safe | 58.1–63.0 µs | 87.9–121 µs | 18.8–19.3 ms | 24.8–25.1 ms | 12.2–12.8 ms | 25.6–31.1 ms | 44.8–50.7 µs | 253 ms | 4.74 |
| 10 × 128 MiB | warm | zero-copy | 65.7–77.9 µs | 54.7–56.0 µs | 12.1–12.4 ms | 16.2 ms | 7.81–8.10 ms | 16.6 ms | 40.7–42.0 µs | 168 ms | 7.12–7.13 |
| 10 × 1 GiB | cold | safe | 798–802 µs | 455–461 µs | 364–365 ms | 1.46 s | 728–729 ms | 1.45 s | 557–576 µs | 15.3 s | 0.69 |
| 10 × 1 GiB | cold | zero-copy | 715–739 µs | 164–193 µs | 364 ms | 1.46 s | 728 ms | 1.46 s | 708–745 µs | 15.3 s | 0.69 |
| 10 × 1 GiB | warm | safe | 58.3–63.9 µs | 79.2–90.9 µs | 150–151 ms | 200–201 ms | 100–101 ms | 199–200 ms | 36.7–40.6 µs | 2.00 s | 4.79–4.80 |
| 10 × 1 GiB | warm | zero-copy | 65.2–68.9 µs | 56.2–58.3 µs | 96.4–96.7 ms | 129 ms | 64.5–64.6 ms | 128–129 ms | 42.4–44.7 µs | 1.31 s | 7.33 |
| 512 × 128 MiB | cold | safe | 1.73–1.86 ms | 708–777 µs | 137 ms | 182 ms | 91.0–91.4 ms | 181 ms | 799–880 µs | 93.1 s | 0.69 |
| 512 × 128 MiB | cold | zero-copy | 1.68–1.98 ms | 420–426 µs | 136 ms | 182 ms | 90.4–90.8 ms | 181 ms | 978–991 µs | 93.2 s | 0.69 |
| 512 × 128 MiB | warm | safe | 213–214 µs | 213–219 µs | 5.51–45.1 ms | 24.7–140 ms | 12.2–90.4 ms | 27.5–28.1 ms | 177–185 µs | 83.5–93.2 s | 0.69–0.77 |
| 512 × 128 MiB | warm | zero-copy | 213–216 µs | 196–257 µs | 3.89–4.00 ms | 16.1–16.2 ms | 8.09–8.29 ms | 18.6–18.9 ms | 180–185 µs | 93.1 s | 0.69 |

aarch64 (`d2074c1`):

| Journal | Cache | Reader | open | seek first | seek middle | seek across a full segment | seek last | 16 forward seeks | `seek_to_end` | replay | replay rate (M records/s) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 10 × 1 MiB | cold | safe | 338–434 µs | 338–343 µs | 610–615 µs | 1.05–1.08 ms | 492–696 µs | 520–636 µs | 289–300 µs | 14.0 ms | 0.86–0.87 |
| 10 × 1 MiB | cold | zero-copy | 384–452 µs | 204–211 µs | 611–693 µs | 1.05–1.12 ms | 482–526 µs | 492–578 µs | 323–402 µs | 13.9–14.3 ms | 0.85–0.87 |
| 10 × 1 MiB | warm | safe | 57.2–72.0 µs | 76.0–86.1 µs | 188–204 µs | 214–245 µs | 136–170 µs | 311–338 µs | 35.4–35.5 µs | 1.62–1.94 ms | 4.83–5.76 |
| 10 × 1 MiB | warm | zero-copy | 54.6–73.1 µs | 56.5–61.9 µs | 132–135 µs | 148–161 µs | 109–125 µs | 275–277 µs | 38.6–38.8 µs | 1.32–1.34 ms | 6.98–7.08 |
| 1,000 × 1 MiB | cold | safe | 1.45–2.62 ms | 563–993 µs | 1.20–1.27 ms | 1.16–1.35 ms | 919 µs–1.56 ms | 4.52 ms | 470–508 µs | 1.54 s | 0.64 |
| 1,000 × 1 MiB | cold | zero-copy | 1.70–1.98 ms | 408–487 µs | 924 µs–1.30 ms | 1.36–1.50 ms | 1.12–1.22 ms | 4.63–4.65 ms | 863–960 µs | 1.54 s | 0.64 |
| 1,000 × 1 MiB | warm | safe | 331–346 µs | 319–339 µs | 442–465 µs | 452–456 µs | 386–411 µs | 4.36–4.40 ms | 296–307 µs | 167–170 ms | 5.79–5.91 |
| 1,000 × 1 MiB | warm | zero-copy | 340–345 µs | 321–324 µs | 388–394 µs | 405–415 µs | 367–375 µs | 4.36–4.37 ms | 292–330 µs | 119 ms | 8.27–8.30 |
| 10,000 × 1 MiB | cold | safe | 6.38–6.91 ms | 2.90–2.97 ms | 3.21–3.30 ms | 3.51–3.55 ms | 3.18–3.29 ms | 42.7–42.8 ms | 2.88–2.90 ms | 15.4–15.5 s | 0.64 |
| 10,000 × 1 MiB | cold | zero-copy | 6.29–6.30 ms | 2.78 ms | 3.20–3.24 ms | 3.52–3.54 ms | 3.23 ms | 42.9–43.0 ms | 3.02–3.03 ms | 15.4–15.5 s | 0.63–0.64 |
| 10,000 × 1 MiB | warm | safe | 2.75 ms | 2.74–2.76 ms | 2.84–2.86 ms | 2.86 ms | 2.79–2.81 ms | 42.6 ms | 2.70–2.71 ms | 1.56–1.58 s | 6.22–6.31 |
| 10,000 × 1 MiB | warm | zero-copy | 2.77–2.79 ms | 2.72–2.75 ms | 2.79–2.80 ms | 2.81 ms | 2.77–2.79 ms | 42.7–42.8 ms | 2.72–2.74 ms | 1.15–1.17 s | 8.40–8.55 |
| 20,000 × 1 MiB | cold | safe | 14.8–16.1 ms | 5.57–5.59 ms | 5.99–6.17 ms | 6.29–6.40 ms | 5.97–6.11 ms | 85.2–85.3 ms | 5.63–5.72 ms | 30.8 s | 0.64 |
| 20,000 × 1 MiB | cold | zero-copy | 14.5 ms | 5.44–5.48 ms | 5.97–5.99 ms | 6.29 ms | 5.99–6.01 ms | 85.4–85.6 ms | 5.71–5.90 ms | 30.8 s | 0.64 |
| 20,000 × 1 MiB | warm | safe | 5.46–5.49 ms | 5.37–5.41 ms | 5.49–5.66 ms | 5.53–5.54 ms | 5.47–5.48 ms | 84.8–85.3 ms | 5.37–5.38 ms | 11.9–16.8 s | 1.17–1.66 |
| 20,000 × 1 MiB | warm | zero-copy | 5.46–5.48 ms | 5.40 ms | 5.45–5.47 ms | 5.48 ms | 5.44 ms | 85.2–85.4 ms | 5.37 ms | 11.9–18.8 s | 1.05–1.65 |
| 100,000 × 64 KiB | cold | safe | 76.9–82.7 ms | 27.1–27.4 ms | 27.7 ms | 27.7–28.1 ms | 28.0–28.4 ms | 427–430 ms | 27.3–27.4 ms | 18.7–18.9 s | 0.32–0.33 |
| 100,000 × 64 KiB | cold | zero-copy | 76.9–79.6 ms | 27.3–27.5 ms | 27.8 ms | 27.8–28.1 ms | 27.7–28.0 ms | 430–432 ms | 27.3–27.8 ms | 19.0–19.3 s | 0.32 |
| 100,000 × 64 KiB | warm | safe | 27.6 ms | 27.4 ms | 27.2–27.4 ms | 27.3 ms | 27.3 ms | 434 ms | 27.4–28.1 ms | 1.55–1.56 s | 3.90–3.94 |
| 100,000 × 64 KiB | warm | zero-copy | 27.4–27.5 ms | 27.3–27.4 ms | 27.3 ms | 27.2–27.3 ms | 27.2–27.3 ms | 434–436 ms | 27.2–27.4 ms | 1.71–1.72 s | 3.55–3.56 |
| 10 × 16 MiB | cold | safe | 420–538 µs | 271–631 µs | 13.7–14.0 ms | 18.9–19.5 ms | 7.82–8.06 ms | 18.4–18.6 ms | 240–677 µs | 279–280 ms | 0.65 |
| 10 × 16 MiB | cold | zero-copy | 406–885 µs | 374–391 µs | 13.7 ms | 19.2–19.4 ms | 8.31–8.34 ms | 18.5–18.8 ms | 449–653 µs | 279–280 ms | 0.65 |
| 10 × 16 MiB | warm | safe | 47.3–52.0 µs | 82.3–90.0 µs | 1.63–2.05 ms | 2.10–2.29 ms | 1.01–1.38 ms | 2.10–2.55 ms | 37.0–61.9 µs | 22.9–23.3 ms | 6.42–6.53 |
| 10 × 16 MiB | warm | zero-copy | 57.0–90.2 µs | 58.2–101 µs | 1.07–1.18 ms | 1.40–1.44 ms | 725–826 µs | 1.53–1.66 ms | 43.9–58.8 µs | 15.3 ms | 9.77–9.80 |
| 10 × 128 MiB | cold | safe | 276–343 µs | 254–289 µs | 143 ms | 191–192 ms | 94.2 ms | 191 ms | 289–762 µs | 2.26 s | 0.64 |
| 10 × 128 MiB | cold | zero-copy | 270–286 µs | 135–182 µs | 143 ms | 191–192 ms | 94.4–94.5 ms | 191 ms | 928 µs–1.06 ms | 2.26 s | 0.64 |
| 10 × 128 MiB | warm | safe | 47.7–57.1 µs | 71.1–76.5 µs | 14.5–15.0 ms | 18.8–18.9 ms | 9.51–9.57 ms | 18.8–18.9 ms | 38.7–39.8 µs | 183–186 ms | 6.45–6.55 |
| 10 × 128 MiB | warm | zero-copy | 51.2–71.1 µs | 54.0–54.2 µs | 8.85–8.95 ms | 11.6–11.7 ms | 5.91–5.95 ms | 11.8–11.9 ms | 39.8–46.1 µs | 119–121 ms | 9.92–10.06 |
| 10 × 1 GiB | cold | safe | 959 µs–1.22 ms | 306–639 µs | 1.18 s | 1.57 s | 784 ms | 1.57 s | 276–806 µs | 18.1 s | 0.64 |
| 10 × 1 GiB | cold | zero-copy | 736–951 µs | 137–343 µs | 1.18 s | 1.57 s | 784 ms | 1.57 s | 344–817 µs | 18.1 s | 0.64 |
| 10 × 1 GiB | warm | safe | 48.7–50.4 µs | 71.6–104 µs | 111–112 ms | 149–152 ms | 74.5–75.8 ms | 150–152 ms | 38.3–61.8 µs | 1.43–1.51 s | 6.35–6.71 |
| 10 × 1 GiB | warm | zero-copy | 62.3–72.5 µs | 56.0–58.7 µs | 68.4–68.8 ms | 91.2–92.2 ms | 45.7–46.3 ms | 91.9–92.3 ms | 41.5–42.4 µs | 972–976 ms | 9.83–9.86 |
| 512 × 128 MiB | cold | safe | 802–956 µs | 377–386 µs | 143 ms | 192 ms | 93.4 ms | 191 ms | 795–881 µs | 101 s | 0.64 |
| 512 × 128 MiB | cold | zero-copy | 925–989 µs | 290–640 µs | 143 ms | 192 ms | 93.0–93.3 ms | 191 ms | 865 µs–1.24 ms | 101 s | 0.64 |
| 512 × 128 MiB | warm | safe | 196–198 µs | 191–210 µs | 4.02–43.7 ms | 19.5–145 ms | 8.82–94.0 ms | 20.8–21.4 ms | 164–172 µs | 93.9–101 s | 0.64–0.69 |
| 512 × 128 MiB | warm | zero-copy | 197–203 µs | 183–197 µs | 2.82–2.86 ms | 11.6–11.8 ms | 5.67–5.79 ms | 13.6–14.0 ms | 170–175 µs | 96.5–101 s | 0.64–0.67 |

With the journal in the page cache the zero-copy reader replays about 1.5 times as fast as the safe reader; from cold
storage both are limited by the SSD (0.69 million records/s, 707 MB/s of payload, on x86-64 and 0.64 million on aarch64
with 128 MiB segments; 0.5 to 0.55 million with 1 MiB segments on x86-64) and equal. The 128 MiB journal's zero-copy
reader maps one segment at a time (peak RSS about one segment).

Compared with the 1.0.0 core, on the same warm journals, median of 5 runs (range): the core of this section (`85726e3`
on x86-64, `d2074c1` on aarch64), then the 1.0.0 core, then the first again, to show drift. Only the forward seeks (ADR
0046) and the retention plan changed: with segments of 16 MiB and more the plan of ten segments takes about half the
time, since reading the marker now reads its two 64-byte slots instead of the whole file (128 KiB).

x86-64 (`85726e3`):

| Journal (warm) | Reader | Operation | 85726e3 | 1.0.0 core | 85726e3 again |
|---|---|---|---|---|---|
| 1,000 × 1 MiB | safe | seek across a full segment | 494 µs (482–516 µs) | 512 µs (482–535 µs) | 476 µs (467–493 µs) |
| 1,000 × 1 MiB | safe | 16 forward seeks | 4.56 ms (4.52–4.61 ms) | 5.41 ms (5.37–5.44 ms) | 4.58 ms (4.52–4.60 ms) |
| 1,000 × 1 MiB | safe | replay | 216 ms (215–217 ms) | 216 ms (216 ms) | 216 ms (215–216 ms) |
| 1,000 × 1 MiB | zero-copy | seek across a full segment | 443 µs (441–446 µs) | 448 µs (443–493 µs) | 460 µs (455–471 µs) |
| 1,000 × 1 MiB | zero-copy | 16 forward seeks | 4.50 ms (4.48–4.53 ms) | 5.50 ms (5.49–5.54 ms) | 4.55 ms (4.51–4.75 ms) |
| 1,000 × 1 MiB | zero-copy | replay | 152 ms (151–161 ms) | 152 ms (152–153 ms) | 155 ms (152–157 ms) |
| 1,000 × 1 MiB | – | retention plan | 2.52 ms (2.51–2.65 ms) | 2.63 ms (2.58–2.90 ms) | 2.50 ms (2.46–2.55 ms) |
| 10,000 × 1 MiB | safe | seek across a full segment | 3.07 ms (3.05–3.09 ms) | 3.06 ms (3.03–3.11 ms) | 3.09 ms (3.04–3.17 ms) |
| 10,000 × 1 MiB | safe | 16 forward seeks | 45.9 ms (45.8–46.0 ms) | 46.7 ms (46.4–46.9 ms) | 45.5 ms (45.5–45.8 ms) |
| 10,000 × 1 MiB | safe | replay | 2.14 s (2.14 s) | 2.14 s (2.13–2.15 s) | 2.15 s (2.14–2.18 s) |
| 10,000 × 1 MiB | zero-copy | seek across a full segment | 3.05 ms (3.03–3.05 ms) | 3.03 ms (3.01–3.06 ms) | 3.03 ms (3.02–3.04 ms) |
| 10,000 × 1 MiB | zero-copy | 16 forward seeks | 45.9 ms (45.9–46.2 ms) | 46.6 ms (46.5–46.7 ms) | 45.5 ms (45.4–45.6 ms) |
| 10,000 × 1 MiB | zero-copy | replay | 1.54 s (1.51–1.55 s) | 1.53 s (1.50–1.55 s) | 1.54 s (1.50–1.55 s) |
| 10,000 × 1 MiB | – | retention plan | 29.5 ms (28.8–30.6 ms) | 27.5 ms (26.7–27.7 ms) | 28.1 ms (26.9–28.5 ms) |
| 10 × 16 MiB | safe | seek across a full segment | 2.20 ms (2.14–2.49 ms) | 2.26 ms (2.21–2.47 ms) | 2.22 ms (2.17–2.44 ms) |
| 10 × 16 MiB | safe | 16 forward seeks | 2.33 ms (2.29–2.56 ms) | 18.1 ms (18.0–19.8 ms) | 2.33 ms (2.29–2.85 ms) |
| 10 × 16 MiB | safe | replay | 30.9 ms (30.8–31.2 ms) | 31.0 ms (30.9–31.4 ms) | 30.9 ms (30.6–31.2 ms) |
| 10 × 16 MiB | zero-copy | seek across a full segment | 1.84 ms (1.84–1.92 ms) | 1.85 ms (1.84–1.91 ms) | 1.84 ms (1.83–1.92 ms) |
| 10 × 16 MiB | zero-copy | 16 forward seeks | 1.97 ms (1.96–2.01 ms) | 16.0 ms (15.9–16.0 ms) | 1.99 ms (1.95–2.05 ms) |
| 10 × 16 MiB | zero-copy | replay | 20.8 ms (20.7–21.0 ms) | 20.8 ms (20.7–21.2 ms) | 20.8 ms (20.6–21.1 ms) |
| 10 × 16 MiB | – | retention plan | 80.6 µs (76.7–83.7 µs) | 182 µs (172–207 µs) | 79.8 µs (77.5–114 µs) |
| 10 × 128 MiB | safe | seek across a full segment | 24.0 ms (24.0–24.1 ms) | 24.4 ms (24.3–24.7 ms) | 23.9 ms (23.9–24.0 ms) |
| 10 × 128 MiB | safe | 16 forward seeks | 25.2 ms (24.7–25.7 ms) | 190 ms (189–198 ms) | 24.9 ms (24.5–25.7 ms) |
| 10 × 128 MiB | safe | replay | 251 ms (250–251 ms) | 252 ms (251–253 ms) | 250 ms (250–251 ms) |
| 10 × 128 MiB | zero-copy | seek across a full segment | 15.5 ms (15.5–17.1 ms) | 15.7 ms (15.5–15.9 ms) | 15.4 ms (15.4–15.5 ms) |
| 10 × 128 MiB | zero-copy | 16 forward seeks | 16.4 ms (15.9–16.8 ms) | 133 ms (131–135 ms) | 16.1 ms (15.9–16.6 ms) |
| 10 × 128 MiB | zero-copy | replay | 165 ms (164–165 ms) | 165 ms (165–166 ms) | 165 ms (164–165 ms) |
| 10 × 128 MiB | – | retention plan | 79.2 µs (78.6–84.7 µs) | 179 µs (176–234 µs) | 80.4 µs (77.9–81.0 µs) |
| 10 × 1 GiB | safe | seek across a full segment | 200 ms (199–200 ms) | 203 ms (202–203 ms) | 200 ms (200–201 ms) |
| 10 × 1 GiB | safe | 16 forward seeks | 201 ms (200–201 ms) | 1.70 s (1.70–1.72 s) | 200 ms (200–201 ms) |
| 10 × 1 GiB | safe | replay | 2.01 s (2.01 s) | 2.01 s (2.00–2.01 s) | 2.00 s (2.00–2.01 s) |
| 10 × 1 GiB | zero-copy | seek across a full segment | 129 ms (128–129 ms) | 129 ms (129–130 ms) | 128 ms (128–130 ms) |
| 10 × 1 GiB | zero-copy | 16 forward seeks | 129 ms (129–130 ms) | 1.12 s (1.12–1.13 s) | 129 ms (129 ms) |
| 10 × 1 GiB | zero-copy | replay | 1.31 s (1.31–1.32 s) | 1.31 s (1.31–1.32 s) | 1.31 s (1.30–1.31 s) |
| 10 × 1 GiB | – | retention plan | 82.2 µs (79.6–119 µs) | 183 µs (176–205 µs) | 78.9 µs (78.1–81.5 µs) |

aarch64 (`d2074c1`):

| Journal (warm) | Reader | Operation | d2074c1 | 1.0.0 core | d2074c1 again |
|---|---|---|---|---|---|
| 1,000 × 1 MiB | safe | seek across a full segment | 467 µs (444–496 µs) | 454 µs (444–461 µs) | 448 µs (440–466 µs) |
| 1,000 × 1 MiB | safe | 16 forward seeks | 4.35 ms (4.33–4.41 ms) | 5.25 ms (5.20–5.35 ms) | 4.37 ms (4.33–4.41 ms) |
| 1,000 × 1 MiB | safe | replay | 162 ms (156–163 ms) | 163 ms (162–166 ms) | 162 ms (160–164 ms) |
| 1,000 × 1 MiB | zero-copy | seek across a full segment | 410 µs (403–433 µs) | 400 µs (395–415 µs) | 409 µs (401–437 µs) |
| 1,000 × 1 MiB | zero-copy | 16 forward seeks | 4.32 ms (4.31–4.35 ms) | 5.11 ms (5.04–5.14 ms) | 4.32 ms (4.31–4.33 ms) |
| 1,000 × 1 MiB | zero-copy | replay | 117 ms (117–118 ms) | 118 ms (117–120 ms) | 119 ms (116–122 ms) |
| 1,000 × 1 MiB | – | retention plan | 3.42 ms (3.36–3.46 ms) | 3.27 ms (3.23–3.37 ms) | 3.28 ms (3.17–3.40 ms) |
| 10,000 × 1 MiB | safe | seek across a full segment | 2.86 ms (2.84–2.87 ms) | 2.89 ms (2.88–2.90 ms) | 2.88 ms (2.86–2.92 ms) |
| 10,000 × 1 MiB | safe | 16 forward seeks | 42.8 ms (42.5–42.9 ms) | 43.8 ms (43.6–44.1 ms) | 42.7 ms (42.6–44.2 ms) |
| 10,000 × 1 MiB | safe | replay | 1.62 s (1.54–1.63 s) | 1.64 s (1.60–1.65 s) | 1.65 s (1.54–1.67 s) |
| 10,000 × 1 MiB | zero-copy | seek across a full segment | 2.83 ms (2.82–2.84 ms) | 2.85 ms (2.83–2.87 ms) | 2.82 ms (2.80–2.87 ms) |
| 10,000 × 1 MiB | zero-copy | 16 forward seeks | 42.8 ms (42.8–43.1 ms) | 43.7 ms (43.3–43.8 ms) | 42.7 ms (42.7–42.9 ms) |
| 10,000 × 1 MiB | zero-copy | replay | 1.19 s (1.14–1.21 s) | 1.18 s (1.16–1.20 s) | 1.21 s (1.19–1.22 s) |
| 10,000 × 1 MiB | – | retention plan | 37.7 ms (37.4–38.3 ms) | 36.1 ms (35.9–36.3 ms) | 31.3 ms (30.4–32.5 ms) |
| 10 × 16 MiB | safe | seek across a full segment | 2.03 ms (1.94–2.14 ms) | 2.00 ms (1.92–2.16 ms) | 1.97 ms (1.92–2.15 ms) |
| 10 × 16 MiB | safe | 16 forward seeks | 2.10 ms (2.06–2.64 ms) | 15.6 ms (15.5–16.9 ms) | 2.10 ms (2.06–2.59 ms) |
| 10 × 16 MiB | safe | replay | 23.1 ms (21.9–24.0 ms) | 23.0 ms (22.6–23.4 ms) | 23.1 ms (22.4–24.0 ms) |
| 10 × 16 MiB | zero-copy | seek across a full segment | 1.38 ms (1.35–1.42 ms) | 1.38 ms (1.35–1.40 ms) | 1.35 ms (1.34–1.40 ms) |
| 10 × 16 MiB | zero-copy | 16 forward seeks | 1.51 ms (1.48–1.60 ms) | 11.5 ms (11.5–11.7 ms) | 1.49 ms (1.48–1.64 ms) |
| 10 × 16 MiB | zero-copy | replay | 15.1 ms (15.1–15.6 ms) | 15.3 ms (15.2–15.4 ms) | 15.3 ms (15.1–15.5 ms) |
| 10 × 16 MiB | – | retention plan | 73.7 µs (70.7–77.3 µs) | 141 µs (138–177 µs) | 73.3 µs (70.7–76.2 µs) |
| 10 × 128 MiB | safe | seek across a full segment | 18.6 ms (18.0–19.2 ms) | 18.9 ms (18.7–19.5 ms) | 19.0 ms (18.7–19.5 ms) |
| 10 × 128 MiB | safe | 16 forward seeks | 18.8 ms (18.5–19.2 ms) | 148 ms (145–151 ms) | 19.7 ms (19.6–19.9 ms) |
| 10 × 128 MiB | safe | replay | 183 ms (183–184 ms) | 187 ms (179–190 ms) | 187 ms (183–188 ms) |
| 10 × 128 MiB | zero-copy | seek across a full segment | 11.5 ms (11.4–11.7 ms) | 11.6 ms (11.4–11.7 ms) | 11.7 ms (11.7–11.8 ms) |
| 10 × 128 MiB | zero-copy | 16 forward seeks | 11.8 ms (11.7–11.8 ms) | 98.0 ms (97.6–98.7 ms) | 12.0 ms (11.9–12.1 ms) |
| 10 × 128 MiB | zero-copy | replay | 119 ms (118–121 ms) | 122 ms (122–123 ms) | 122 ms (120–123 ms) |
| 10 × 128 MiB | – | retention plan | 80.1 µs (73.0–91.8 µs) | 143 µs (139–178 µs) | 75.3 µs (71.6–78.6 µs) |
| 10 × 1 GiB | safe | seek across a full segment | 150 ms (147–152 ms) | 151 ms (147–152 ms) | 149 ms (145–150 ms) |
| 10 × 1 GiB | safe | 16 forward seeks | 148 ms (144–150 ms) | 1.27 s (1.22–1.27 s) | 150 ms (149–151 ms) |
| 10 × 1 GiB | safe | replay | 1.46 s (1.44–1.49 s) | 1.45 s (1.44–1.48 s) | 1.42 s (1.41–1.47 s) |
| 10 × 1 GiB | zero-copy | seek across a full segment | 92.5 ms (92.2–93.2 ms) | 92.0 ms (91.0–95.3 ms) | 92.2 ms (91.9–92.3 ms) |
| 10 × 1 GiB | zero-copy | 16 forward seeks | 92.1 ms (91.2–92.5 ms) | 825 ms (812–830 ms) | 92.4 ms (92.2–92.8 ms) |
| 10 × 1 GiB | zero-copy | replay | 962 ms (960–982 ms) | 969 ms (955–986 ms) | 961 ms (956–964 ms) |
| 10 × 1 GiB | – | retention plan | 74.6 µs (74.4–79.8 µs) | 145 µs (140–165 µs) | 75.6 µs (73.5–78.6 µs) |

### Seek index (ADR 0045)

(Measured again on `main` after #74, at 64 B and 1,024 B, with targets inside one segment:
[above](#2026-11-26-seeks-with-and-without-the-seek-index-google-cloud-vms).)

| Machine | Journal | Indexes built | Size of the index files | Build all, cold | warm | Refresh (nothing to do), cold | warm | Check, warm |
|---|---|---|---|---|---|---|---|---|
| x86-64 (`d2074c1`) | 10,000 × 1 MiB | 10,000 | 4.88 MB | 14.3–14.4 s | 3.11–3.15 s | 5.68–5.69 s | 88.1–90.4 ms | 2.52–2.53 s |
| x86-64 (`d2074c1`) | 20,000 × 1 MiB | 20,000 | 9.76 MB | 28.6–28.8 s | 25.5–25.6 s | 11.6–12.1 s | 181 ms–2.01 s | 25.4–25.5 s |
| x86-64 (`d2074c1`) | 160 × 128 MiB | 159 | 7.83 MB | 28.9–29.0 s | 25.5 s | 109–120 ms | 3.56–21.6 ms | 25.5 s |
| x86-64 (`d2074c1`) | 10 × 1 GiB | 9 | 3.54 MB | 13.1 s | 1.93 s | 11.9–12.8 ms | 1.47–1.53 ms | 1.94 s |
| aarch64 (`d2074c1`) | 10,000 × 1 MiB | 10,000 | 4.88 MB | 15.4–15.5 s | 2.32–2.35 s | 3.07–3.19 s | 94.2–94.9 ms | 1.69–1.71 s |
| aarch64 (`d2074c1`) | 20,000 × 1 MiB | 20,000 | 9.76 MB | 30.9–31.1 s | 27.2–27.3 s | 6.14–6.38 s | 177 ms–1.15 s | 26.8–26.9 s |
| aarch64 (`d2074c1`) | 160 × 128 MiB | 159 | 7.83 MB | 31.3 s | 27.1–27.2 s | 62.2–63.4 ms | 3.56–13.9 ms | 27.2 s |
| aarch64 (`d2074c1`) | 10 × 1 GiB | 9 | 3.54 MB | 14.2 s | 1.37–1.38 s | 7.68–7.82 ms | 1.09–1.10 ms | 1.38–1.40 s |

x86-64 (`d2074c1`), median of 5 (range):

| Journal | Cache | Reader | Index | seek across a full segment | seek to the last sealed record | 16 forward seeks | seek last (newest segment) | read from storage, full segment |
|---|---|---|---|---|---|---|---|---|
| 10,000 × 1 MiB | cold | safe | off | 5.23 ms (5.14–5.32 ms) | 5.36 ms (5.27–5.52 ms) | 46.3 ms (46.2–46.6 ms) | 5.05 ms (4.95–5.28 ms) | 1,164 KiB |
| 10,000 × 1 MiB | cold | safe | on | 4.97 ms (4.96–5.34 ms) * | 4.70 ms (4.63–5.02 ms) * | 46.0 ms (46.0–46.7 ms) | 4.43 ms (4.36–4.60 ms) | 492 KiB |
| 10,000 × 1 MiB | cold | zero-copy | off | 5.16 ms (5.13–5.58 ms) | 5.32 ms (5.22–5.45 ms) | 46.6 ms (46.4–46.7 ms) | 4.84 ms (4.75–5.09 ms) | 1,164 KiB |
| 10,000 × 1 MiB | cold | zero-copy | on | 5.26 ms (5.12–5.46 ms) * | 4.94 ms (4.87–5.11 ms) * | 46.5 ms (46.1–47.1 ms) | 3.73 ms (3.60–3.83 ms) | 560 KiB |
| 10,000 × 1 MiB | warm | safe | off | 3.06 ms (3.05–3.08 ms) | 3.05 ms (3.03–3.17 ms) | 45.7 ms (45.5–45.9 ms) | 3.02 ms (2.98–3.04 ms) | – |
| 10,000 × 1 MiB | warm | safe | on | 2.99 ms (2.94–3.32 ms) * | 2.94 ms (2.93–3.20 ms) * | 45.5 ms (45.5–46.2 ms) | 2.94 ms (2.92–3.26 ms) | – |
| 10,000 × 1 MiB | warm | zero-copy | off | 3.03 ms (3.02–3.03 ms) | 3.04 ms (3.02–3.06 ms) | 45.5 ms (45.5–45.6 ms) | 2.98 ms (2.96–3.03 ms) | – |
| 10,000 × 1 MiB | warm | zero-copy | on | 2.96 ms (2.93–3.00 ms) * | 2.97 ms (2.94–3.09 ms) * | 45.5 ms (45.4–46.4 ms) | 2.94 ms (2.92–2.97 ms) | – |
| 20,000 × 1 MiB | cold | safe | off | 8.30 ms (8.28–8.38 ms) | 8.07 ms (8.03–8.10 ms) | 92.2 ms (91.5–92.4 ms) | 7.72 ms (7.70–7.97 ms) | 1,164 KiB |
| 20,000 × 1 MiB | cold | safe | on | 7.86 ms (7.85–8.10 ms) * | 7.75 ms (7.70–7.81 ms) * | 91.6 ms (91.5–92.2 ms) | 7.57 ms (7.39–7.63 ms) | 492 KiB |
| 20,000 × 1 MiB | cold | zero-copy | off | 8.32 ms (8.26–8.38 ms) | 8.11 ms (8.01–8.36 ms) | 92.6 ms (91.9–92.8 ms) | 7.52 ms (7.45–7.64 ms) | 1,164 KiB |
| 20,000 × 1 MiB | cold | zero-copy | on | 8.16 ms (8.11–8.16 ms) * | 8.08 ms (7.91–8.18 ms) * | 92.5 ms (92.4–92.8 ms) | 6.77 ms (6.57–6.81 ms) | 560 KiB |
| 20,000 × 1 MiB | warm | safe | off | 5.98 ms (5.96–6.03 ms) | 6.03 ms (5.99–6.09 ms) | 91.8 ms (91.4–92.6 ms) | 5.92 ms (5.91–5.98 ms) | – |
| 20,000 × 1 MiB | warm | safe | on | 5.87 ms (5.84–6.17 ms) * | 5.87 ms (5.84–6.23 ms) * | 91.4 ms (91.1–92.0 ms) | 5.86 ms (5.81–6.27 ms) | – |
| 20,000 × 1 MiB | warm | zero-copy | off | 5.97 ms (5.96–5.99 ms) | 5.98 ms (5.96–6.07 ms) | 91.7 ms (91.4–92.2 ms) | 5.90 ms (5.87–5.94 ms) | – |
| 20,000 × 1 MiB | warm | zero-copy | on | 5.87 ms (5.84–5.95 ms) * | 5.86 ms (5.83–5.90 ms) * | 91.1 ms (90.6–91.9 ms) | 5.85 ms (5.82–5.90 ms) | – |
| 160 × 128 MiB | cold | safe | off | 182 ms (181–182 ms) | 182 ms (182 ms) | 181 ms (181 ms) | 90.8 ms (90.5–90.9 ms) | 131,080 KiB |
| 160 × 128 MiB | cold | safe | on | 1.30 ms (1.29–1.46 ms) * | 1.46 ms (1.30–1.47 ms) * | 9.13 ms (8.66–9.44 ms) * | 92.0 ms (91.8–92.1 ms) | 140 KiB |
| 160 × 128 MiB | cold | zero-copy | off | 182 ms (182 ms) | 182 ms (182 ms) | 181 ms (181 ms) | 90.7 ms (90.3–90.9 ms) | 131,080 KiB |
| 160 × 128 MiB | cold | zero-copy | on | 1.63 ms (1.63–1.85 ms) * | 1.76 ms (1.60–1.85 ms) * | 8.59 ms (8.27–9.79 ms) * | 91.6 ms (91.4–91.8 ms) | 208 KiB |
| 160 × 128 MiB | warm | safe | off | 24.1 ms (24.0–24.7 ms) | 24.8 ms (24.7–25.4 ms) | 26.0 ms (25.2–27.9 ms) | 12.1 ms (11.9–12.7 ms) | – |
| 160 × 128 MiB | warm | safe | on | 142 µs (137–375 µs) * | 140 µs (139–465 µs) * | 1.23 ms (1.21–7.55 ms) * | 11.6 ms (11.2–90.7 ms) | – |
| 160 × 128 MiB | warm | zero-copy | off | 15.6 ms (15.5–16.6 ms) | 16.0 ms (15.9–16.2 ms) | 16.8 ms (16.5–17.2 ms) | 7.85 ms (7.62–8.15 ms) | – |
| 160 × 128 MiB | warm | zero-copy | on | 137 µs (132–200 µs) * | 134 µs (134–139 µs) * | 1.32 ms (1.29–1.34 ms) * | 7.70 ms (7.58–7.97 ms) | – |
| 10 × 1 GiB | cold | safe | off | 1.46 s (1.46 s) | 1.46 s (1.46 s) | 1.46 s (1.45–1.46 s) | 728 ms (728 ms) | 1,048,584 KiB |
| 10 × 1 GiB | cold | safe | on | 1.96 ms (1.73–2.04 ms) * | 1.83 ms (1.71–1.89 ms) * | 9.52 ms (9.39–9.55 ms) * | 729 ms (729 ms) | 476 KiB |
| 10 × 1 GiB | cold | zero-copy | off | 1.46 s (1.46 s) | 1.46 s (1.46 s) | 1.46 s (1.46 s) | 728 ms (728 ms) | 1,048,584 KiB |
| 10 × 1 GiB | cold | zero-copy | on | 2.28 ms (2.27–2.40 ms) * | 2.15 ms (2.02–2.15 ms) * | 9.54 ms (9.44–10.5 ms) * | 729 ms (729 ms) | 544 KiB |
| 10 × 1 GiB | warm | safe | off | 201 ms (200–201 ms) | 202 ms (201–202 ms) | 201 ms (200–201 ms) | 100 ms (100–101 ms) | – |
| 10 × 1 GiB | warm | safe | on | 322 µs (320–634 µs) * | 339 µs (316–773 µs) * | 2.04 ms (2.03–9.50 ms) * | 101 ms (100–728 ms) | – |
| 10 × 1 GiB | warm | zero-copy | off | 128 ms (128–136 ms) | 129 ms (129–130 ms) | 129 ms (129 ms) | 63.9 ms (63.9–64.8 ms) | – |
| 10 × 1 GiB | warm | zero-copy | on | 305 µs (303–387 µs) * | 318 µs (307–369 µs) * | 2.16 ms (2.12–2.19 ms) * | 64.8 ms (64.5–65.2 ms) | – |

aarch64 (`d2074c1`), median of 5 (range):

| Journal | Cache | Reader | Index | seek across a full segment | seek to the last sealed record | 16 forward seeks | seek last (newest segment) | read from storage, full segment |
|---|---|---|---|---|---|---|---|---|
| 10,000 × 1 MiB | cold | safe | off | 3.69 ms (3.67–3.72 ms) | 3.74 ms (3.70–3.84 ms) | 42.8 ms (42.6–42.9 ms) | 3.53 ms (3.49–3.56 ms) | 1,164 KiB |
| 10,000 × 1 MiB | cold | safe | on | 3.77 ms (3.71–3.89 ms) * | 3.60 ms (3.53–3.63 ms) * | 42.7 ms (42.6–42.7 ms) | 3.55 ms (3.39–3.63 ms) | 492 KiB |
| 10,000 × 1 MiB | cold | zero-copy | off | 3.70 ms (3.63–3.73 ms) | 3.72 ms (3.67–3.75 ms) | 42.9 ms (42.7–43.0 ms) | 3.49 ms (3.41–4.46 ms) | 1,164 KiB |
| 10,000 × 1 MiB | cold | zero-copy | on | 3.95 ms (3.87–4.04 ms) * | 3.80 ms (3.70–4.03 ms) * | 43.1 ms (42.8–44.2 ms) | 3.14 ms (3.09–3.16 ms) | 560 KiB |
| 10,000 × 1 MiB | warm | safe | off | 2.93 ms (2.88–3.05 ms) | 2.87 ms (2.84–2.92 ms) | 42.7 ms (42.5–43.0 ms) | 2.82 ms (2.82–2.83 ms) | – |
| 10,000 × 1 MiB | warm | safe | on | 2.74 ms (2.74–2.96 ms) * | 2.75 ms (2.74–2.91 ms) * | 42.6 ms (42.4–43.3 ms) | 2.78 ms (2.73–2.93 ms) | – |
| 10,000 × 1 MiB | warm | zero-copy | off | 2.84 ms (2.81–2.85 ms) | 2.82 ms (2.80–2.84 ms) | 42.7 ms (42.6–42.7 ms) | 2.78 ms (2.77–2.78 ms) | – |
| 10,000 × 1 MiB | warm | zero-copy | on | 2.76 ms (2.72–2.85 ms) * | 2.74 ms (2.74–2.76 ms) * | 42.8 ms (42.7–42.9 ms) | 2.73 ms (2.72–2.74 ms) | – |
| 20,000 × 1 MiB | cold | safe | off | 6.38 ms (6.28–6.43 ms) | 6.44 ms (6.37–6.53 ms) | 85.6 ms (85.5–86.1 ms) | 6.16 ms (6.11–6.24 ms) | 1,164 KiB |
| 20,000 × 1 MiB | cold | safe | on | 6.57 ms (6.49–6.63 ms) * | 6.49 ms (6.33–6.58 ms) * | 85.3 ms (85.1–85.5 ms) | 6.14 ms (6.12–6.36 ms) | 492 KiB |
| 20,000 × 1 MiB | cold | zero-copy | off | 6.32 ms (6.26–6.39 ms) | 6.48 ms (6.40–7.01 ms) | 86.0 ms (85.8–86.2 ms) | 6.08 ms (6.06–6.13 ms) | 1,164 KiB |
| 20,000 × 1 MiB | cold | zero-copy | on | 6.66 ms (6.62–7.59 ms) * | 6.42 ms (6.39–6.74 ms) * | 85.9 ms (85.5–86.0 ms) | 5.82 ms (5.78–5.95 ms) | 560 KiB |
| 20,000 × 1 MiB | warm | safe | off | 5.55 ms (5.50–5.63 ms) | 5.54 ms (5.52–5.57 ms) | 85.1 ms (85.0–85.4 ms) | 5.50 ms (5.47–5.52 ms) | – |
| 20,000 × 1 MiB | warm | safe | on | 5.46 ms (5.41–5.57 ms) * | 5.43 ms (5.41–5.53 ms) * | 85.0 ms (84.7–85.4 ms) | 5.42 ms (5.40–5.77 ms) | – |
| 20,000 × 1 MiB | warm | zero-copy | off | 5.50 ms (5.48–5.52 ms) | 5.48 ms (5.45–5.51 ms) | 85.3 ms (85.2–85.5 ms) | 5.44 ms (5.42–5.49 ms) | – |
| 20,000 × 1 MiB | warm | zero-copy | on | 5.41 ms (5.40–5.42 ms) * | 5.43 ms (5.39–5.48 ms) * | 85.3 ms (84.8–85.3 ms) | 5.42 ms (5.38–5.47 ms) | – |
| 160 × 128 MiB | cold | safe | off | 192 ms (191–192 ms) | 192 ms (191–192 ms) | 191 ms (191 ms) | 93.1 ms (93.1–93.3 ms) | 131,196 KiB |
| 160 × 128 MiB | cold | safe | on | 718 µs (693–762 µs) * | 687 µs (663–864 µs) * | 4.91 ms (4.68–6.07 ms) * | 94.0 ms (93.9–94.4 ms) | 140 KiB |
| 160 × 128 MiB | cold | zero-copy | off | 192 ms (191–192 ms) | 191 ms (191–192 ms) | 191 ms (191 ms) | 93.0 ms (92.8–93.8 ms) | 131,196 KiB |
| 160 × 128 MiB | cold | zero-copy | on | 872 µs (860 µs–1.18 ms) * | 895 µs (842–985 µs) * | 5.23 ms (5.10–5.54 ms) * | 94.1 ms (93.9–94.3 ms) | 208 KiB |
| 160 × 128 MiB | warm | safe | off | 19.5 ms (19.2–19.7 ms) | 19.2 ms (19.1–19.4 ms) | 20.4 ms (19.4–20.5 ms) | 9.76 ms (9.71–9.96 ms) | – |
| 160 × 128 MiB | warm | safe | on | 138 µs (132–270 µs) * | 135 µs (123–282 µs) * | 1.24 ms (1.22–4.16 ms) * | 10.1 ms (9.82–94.1 ms) | – |
| 160 × 128 MiB | warm | zero-copy | off | 12.0 ms (11.9–12.1 ms) | 11.9 ms (11.9–12.0 ms) | 12.8 ms (12.6–12.8 ms) | 6.10 ms (6.01–6.37 ms) | – |
| 160 × 128 MiB | warm | zero-copy | on | 128 µs (122–129 µs) * | 135 µs (121–148 µs) * | 1.30 ms (1.28–1.34 ms) * | 6.16 ms (6.04–6.24 ms) | – |
| 10 × 1 GiB | cold | safe | off | 1.57 s (1.57 s) | 1.57 s (1.57 s) | 1.57 s (1.57 s) | 783 ms (782–783 ms) | 1,048,584 KiB |
| 10 × 1 GiB | cold | safe | on | 1.24 ms (1.14–1.45 ms) * | 1.14 ms (1.09–1.35 ms) * | 5.86 ms (5.36–6.73 ms) * | 783 ms (783–784 ms) | 476 KiB |
| 10 × 1 GiB | cold | zero-copy | off | 1.57 s (1.57 s) | 1.57 s (1.57 s) | 1.57 s (1.57 s) | 782 ms (782–783 ms) | 1,048,584 KiB |
| 10 × 1 GiB | cold | zero-copy | on | 1.34 ms (1.26–1.43 ms) * | 1.37 ms (1.37–1.43 ms) * | 6.37 ms (5.99–6.74 ms) * | 783 ms (783–784 ms) | 544 KiB |
| 10 × 1 GiB | warm | safe | off | 150 ms (147–152 ms) | 153 ms (150–154 ms) | 148 ms (147–153 ms) | 75.4 ms (73.8–76.1 ms) | – |
| 10 × 1 GiB | warm | safe | on | 275 µs (266–676 µs) * | 272 µs (265–505 µs) * | 1.83 ms (1.81–4.93 ms) * | 76.8 ms (76.4–783 ms) | – |
| 10 × 1 GiB | warm | zero-copy | off | 93.2 ms (92.3–94.1 ms) | 92.4 ms (91.9–92.8 ms) | 92.7 ms (92.3–93.2 ms) | 46.4 ms (45.8–46.5 ms) | – |
| 10 × 1 GiB | warm | zero-copy | on | 267 µs (240–285 µs) * | 289 µs (247–314 µs) * | 1.93 ms (1.91–1.97 ms) * | 46.4 ms (46.2–46.7 ms) | – |

\* All five runs used the index (`seek_used_index`). At 1 MiB the index gains nothing (the directory listing is the
cost, and the index lowers the read from 1.1 MiB to about 0.5 MiB, not the time), and the cold zero-copy reader on
aarch64 is slower with it across a full segment (3.70 to 3.95 ms at 10,000 segments, 6.32 to 6.66 ms at 20,000, ranges
apart); at 128 MiB a seek across a full segment falls from 182 to 1.3 ms cold and at 1 GiB from 1.46 s to about 2 ms
(x86-64), and forward seeks through a segment from 181 ms to 9 ms. A seek to the last record of the journal lands in
the newest segment, which has no index, and does not change. The index files take 488 B for a segment of 1 MiB and 49
KB for one of 128 MiB (0.04 to 0.05 %). `tickertape index` built every index anew in about the time of `verify`, which
also reads every segment (cold, 14.3 s for 10,000 segments; warm, 3.1 s against 2.2 s); a run with nothing left to do
(`index-refresh`) cost 0.09 to 0.18 s when cached, but 6 to 12 s cold with 10,000 and 20,000 segments (x86-64; 3 to 6 s
on aarch64), since it opens every segment and index. With index files, a writer open took 13.6 ms instead of 7.5 ms at
10,000 segments and 26.2 ms instead of 14.6 ms at 20,000 (x86-64; aarch64 12.8 for 7.0 and 25.0 for 13.5), since it
lists `seek-index/`; with 160 segments or fewer there was no difference. Checkpoints with retention: 27.2 ms without
and 26.9 ms with index files at 10,000 segments and 61.3 ms both at 20,000 on x86-64; 30.0 and 35.6 ms, and 69.9 and
69.3 ms, on aarch64. Deleting 2,000 of 20,001 segments with their indexes took 15.5 and 13.9 s on x86-64 (`d2074c1`;
16.0 and 14.5 s without indexes at `85726e3`, another core) and 13.9 and 12.4 s on aarch64 (14.9 and 13.3 s without,
the same core); 1,000 of 10,001 took 4.0 and 3.6 s (x86-64), with no run without indexes at that count.

**`tickertape index` on the journals below (missing and corrupt segments)**, 10,000 segments, warm, at `01c6495`: in
every case but two it built an index for every sealed segment it could (2.8 to 5.3 s) and `index --check` found them
all right. A segment whose records or header are damaged was reported `not_sealed` with the reason and has no index;
the others were built. The two cases where it refused and wrote nothing are a marker that names a missing segment and a
newest segment, named by the marker, with a damaged header (the same error as `Writer::open`). Afterwards, every seek
with `seek_index(true)` had the same outcome as without (for the five seeks of each case and both readers); the
`seek-middle` target was the only one that reached far enough into a segment to use an index.

### Archived history

`tt-scale archive` moved the oldest half of a journal of 10,000 segments of 1 MiB (both machines), and on x86-64 the
oldest 256 of the 1,554 segments of 128 MiB of the journal larger than memory (its size after the catch-up runs). The
move validates every byte of what it moves. The second journal was not measured on aarch64: the session there was
stopped by hand during that step, 11 minutes into the step (20:03:39 to 20:14:39). The "archive then journal" columns
read the whole history in two parts, checking every sequence number; the safe reader ran first, and the zero-copy one
after it found the smaller journal in the page cache.

| Machine | Journal | Moved | plan (cold) | move (cold) | per segment | Peak RSS | archive then journal, safe | zero-copy |
|---|---|---|---|---|---|---|---|---|
| x86-64 (`85726e3`) | 1 MiB segments | 5,006 of 10,006 (4.9 GiB) | 2.82 s | 20.2 s | 4.04 ms | 6 MiB | 9,855,835 records in 8.09 s | 9,855,835 records in 1.53 s |
| x86-64 (`85726e3`) | 128 MiB segments | 256 of 1,554 (32.0 GiB) | 345 ms | 46.6 s | 182 ms | 5 MiB | 196,000,450 records in 279 s | 196,000,450 records in 281 s |
| aarch64 (`d2074c1`) | 1 MiB segments | 5,006 of 10,006 (4.9 GiB) | 1.30 s | 17.6 s | 3.51 ms | 6 MiB | 9,855,835 records in 8.42 s | 9,855,835 records in 1.16 s |

Reading the archive directory up to its last record (`measure --end`), the reader operations and the checks:

| Machine | Archive | Cache | Reader | open | seek first | seek middle | seek across a full segment | seek last | 16 forward seeks | replay | replay rate (M records/s) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| x86-64 (`85726e3`) | 1 MiB segments | cold | safe | 9.52–10.3 ms | 1.83–1.85 ms | 2.46–2.54 ms | 3.65–3.84 ms | 3.61–3.62 ms | 25.1–25.2 ms | 9.45–9.56 s | 0.52 |
| x86-64 (`85726e3`) | 1 MiB segments | cold | zero-copy | 9.39–11.1 ms | 1.53–1.56 ms | 2.08 ms | 3.67–3.71 ms | 3.62–3.65 ms | 25.4–25.7 ms | 9.29–9.66 s | 0.51–0.53 |
| x86-64 (`85726e3`) | 1 MiB segments | warm | safe | 1.52–1.54 ms | 1.56–1.59 ms | 1.51–1.52 ms | 1.70–1.75 ms | 1.70–1.74 ms | 23.9 ms | 1.08–1.09 s | 4.54–4.58 |
| x86-64 (`85726e3`) | 1 MiB segments | warm | zero-copy | 1.54 ms | 1.49–1.55 ms | 1.49–1.50 ms | 1.63–1.64 ms | 1.63–1.64 ms | 23.9–24.2 ms | 800–816 ms | 6.04–6.16 |
| x86-64 (`85726e3`) | 128 MiB segments | cold | safe | 1.76–1.94 ms | 374–401 µs | 796–823 µs | 181 ms | 181–182 ms | 331–339 ms | 46.6 s | 0.69 |
| x86-64 (`85726e3`) | 128 MiB segments | cold | zero-copy | 1.89–1.95 ms | 129–160 µs | 355–361 µs | 181 ms | 182 ms | 284–285 ms | 46.6 s | 0.69 |
| x86-64 (`85726e3`) | 128 MiB segments | warm | safe | 126 µs | 139–144 µs | 135–424 µs | 25.0–180 ms | 25.2–25.4 ms | 189–199 ms | 35.4–46.3 s | 0.70–0.91 |
| x86-64 (`85726e3`) | 128 MiB segments | warm | zero-copy | 131–133 µs | 122–126 µs | 122–129 µs | 16.1–16.3 ms | 16.2–16.4 ms | 134 ms | 46.6 s | 0.69 |
| aarch64 (`d2074c1`) | 1 MiB segments | cold | safe | 4.28–4.37 ms | 1.52–1.53 ms | 1.74–1.83 ms | 2.23–2.29 ms | 2.23–2.26 ms | 22.8 ms | 7.70 s | 0.64 |
| aarch64 (`d2074c1`) | 1 MiB segments | cold | zero-copy | 4.49–5.17 ms | 1.40–1.41 ms | 1.58–1.59 ms | 2.18–2.23 ms | 2.22–2.24 ms | 22.8 ms | 7.70 s | 0.64 |
| aarch64 (`d2074c1`) | 1 MiB segments | warm | safe | 1.41 ms | 1.41–1.44 ms | 1.41 ms | 1.52–1.56 ms | 1.54–1.56 ms | 22.3–22.4 ms | 795–836 ms | 5.90–6.21 |
| aarch64 (`d2074c1`) | 1 MiB segments | warm | zero-copy | 1.43 ms | 1.40–1.42 ms | 1.41 ms | 1.49–1.50 ms | 1.51–1.52 ms | 22.1 ms | 574–585 ms | 8.42–8.59 |

`tickertape` on the archive directory, one run each, bounded by `--to` except for `verify`, which exits with 2 because
of the one problem:

| Machine | Archive | Cache | `list_segments` | `verify` (library) | `tickertape verify` | `tickertape stats --to` | `tickertape export --format jsonl --to` | Peak RSS |
|---|---|---|---|---|---|---|---|---|
| x86-64 (`85726e3`) | 1 MiB segments | cold | 1.07–1.13 s | 7.13 s | 7.14 s | 10.6 s | 21.2 s | 6 MiB |
| x86-64 (`85726e3`) | 1 MiB segments | warm | 15.5–16.1 ms | 1.10 s | 1.10 s | 2.38 s | 13.2 s | 7 MiB |
| x86-64 (`85726e3`) | 128 MiB segments | cold | 53.6–53.7 ms | 46.6 s | 46.6 s | 56.9 s | 127 s | 5 MiB |
| x86-64 (`85726e3`) | 128 MiB segments | warm | 695–765 µs | 31.8 s | 31.8 s | 42.7 s | 113 s | 5 MiB |
| aarch64 (`d2074c1`) | 1 MiB segments | cold | 467–472 ms | 7.70 s | 7.71 s | 8.79 s | 18.4 s | 6 MiB |
| aarch64 (`d2074c1`) | 1 MiB segments | warm | 13.3–14.1 ms | 770–786 ms | 791 ms | 1.99 s | 11.9 s | 6 MiB |

What a reader does at the boundary, the same for both readers in all three runs:

- A reader opened before the move in a segment that is moved returns the rest of that segment's records and then
  `Error::Discarded`; a seek to its position returns `Error::SeqUnavailable`, with the sequence number of the oldest
  remaining segment as the first available.
- The journal opens at the oldest remaining segment; a seek to an archived record returns `Error::SeqUnavailable`.
- Reading the archive directory to its last record, then the journal from the next sequence number, gives every record
  without a gap. Reading on past the archive's last record, or seeking to the sequence number after it, reports
  `Error::Corrupt` (`next segment … missing or inconsistent`), so bound the read; `Reader::seek_to_end` on an archive
  directory fails the same way. `verify` on the archive reports that one problem.
- A durable-only reader returns nothing in an archive directory (it has no marker). The rejoined directory (the
  archive's segments, the journal's and its marker) verified with no problem and read complete.
- Forward seeks in an archive did not continue from the cursor (ADR 0046): an archive has no marker, so no position is
  known to be covered, and 16 forward seeks through a full segment of 128 MiB took 331 to 339 ms cold (safe reader; 181
  ms in the journal).

### Missing and corrupt segments

`tt-scale damage`: a journal of 9,849,507 records in 10,000 segments of 1 MiB (the newest half full), or of 8,010,144
records in 64 segments of 128 MiB, damaged in one way per row. For the first group the checkpoint marker is a quarter
into the newest segment, so a quarter of it is covered and the rest is not; for the last group ("marker two back") it
is a quarter into the third-newest segment. Seek targets: the first record, the middle record of the journal, the first
record of the segment after the damaged one (or the last record), and the last record. Both readers returned the same
in every cell, and so did the five recordings (10,000 segments warm on x86-64 twice, cold once, and on aarch64, and 64
segments of 128 MiB cold on x86-64; in the last only the counts differ). The same 21 cases with
64 segments of 1 MiB gave the same outcomes, with nothing lost, on APFS (macOS, aarch64) and on btrfs in the OrbStack
VM (Linux on macOS); their times are not recorded.

| Damage | replay from the oldest record | seek to first / middle / after the damage / last | `seek_to_end` | `verify` problems | `status` | `Writer::open`, `Quarantine` | `Writer::open`, `Fail` | Records in quarantine / newly in neither after the open (`Quarantine`) |
|---|---|---|---|---|---|---|---|---|
| nothing (marker in the newest segment) | 9,849,507, then end | ok / ok / ok / ok | ok | 0 | no breach | opens | opens | 0 / 0 |
| oldest segment missing | 9,848,522, then end | SeqUnavailable / ok / ok / ok | ok | 0 | no breach | opens | opens | 0 / 0 |
| middle segment missing | 4,925,000, then `Corrupt` | ok / ok / ok / ok | ok | 1 | no breach | opens | opens | 0 / 0 |
| newest segment missing (the marker names it) | 9,849,015, then `Corrupt` | ok / ok / Corrupt / Corrupt | `Corrupt` | 1 | corrupt (`checkpoint_invalid`), unverified | refused: `Corrupt` | refused: `Corrupt` | 0 / 0 |
| a payload byte flipped in the middle segment | 4,925,492, then `Corrupt` | ok / ok / ok / ok | ok | 2 | no breach | opens | opens | 0 / 0 |
| a payload byte flipped in the newest segment, before the marker | 9,849,138, then `Corrupt` | ok / ok / Corrupt / Corrupt | ok | 1 | no breach | opens | opens | 0 / 0 |
| a payload byte flipped in the newest segment, after the marker | 9,849,384, then `Corrupt` | ok / ok / Corrupt / Corrupt | `Corrupt` | 1 | corrupt (`head_invalid`), unverified | opens at 9,849,384, RepairedTail | refused: `Corrupt` | 122 / 0 |
| a commit word zeroed in the middle segment | 4,925,492, then end | ok / ok / ok / ok | ok | 2 | no breach | opens | opens | 0 / 0 |
| a commit word zeroed in the newest segment, after the marker | 9,849,384, then end | ok / ok / end / end | ok | 1 | no breach | opens at 9,849,384, RepairedTail | refused: `Corrupt` | 122 / 0 |
| header of the oldest segment | 0, then `Corrupt` | Corrupt / Corrupt / Corrupt / Corrupt | `Corrupt` | 2 | no breach | opens | opens | 0 / 0 |
| header of the middle segment | 4,925,000, then `Corrupt` | ok / ok / ok / ok | ok | 2 | no breach | opens | opens | 0 / 0 |
| header of the newest segment (the marker names it) | 9,849,015, then `Corrupt` | ok / ok / Corrupt / Corrupt | `Corrupt` | 1 | error: `Corrupt` | refused: `Corrupt` | refused: `Corrupt` | 0 / 0 |
| marker removed, nothing else | 9,849,507, then end | ok / ok / ok / ok | ok | 0 | no breach, unverified | opens | opens | 0 / 0 |
| marker removed, a payload byte flipped in the middle segment | 4,925,492, then `Corrupt` | ok / ok / ok / ok | ok | 2 | no breach, unverified | opens at 4,925,492, QuarantinedSegment × 4,999, RepairedTail | refused: `Corrupt` | 4,924,014 / 0 |
| marker removed, middle segment missing | 4,925,000, then `Corrupt` | ok / ok / ok / ok | ok | 1 | no breach, unverified | refused: `Corrupt` | refused: `Corrupt` | 0 / 0 |
| nothing (marker two segments back) | 9,849,507, then end | ok / ok / ok / ok | ok | 0 | no breach, unverified | opens | opens | 0 / 0 |
| marker two back, newest segment missing | 9,849,015, then `Corrupt` | ok / ok / Corrupt / Corrupt | `Corrupt` | 1 | no breach, unverified | opens at 9,849,015, CompletedRoll | opens at 9,849,015, CompletedRoll | 0 / 0 |
| marker two back, second-newest segment missing | 9,848,030, then `Corrupt` | ok / ok / ok / ok | ok | 1 | no breach, unverified | refused: `Corrupt` | refused: `Corrupt` | 0 / 0 |
| marker two back, a payload byte flipped in the second-newest | 9,848,522, then `Corrupt` | ok / ok / ok / ok | ok | 2 | no breach, unverified | opens at 9,848,522, QuarantinedSegment, RepairedTail | refused: `Corrupt` | 984 / 0 |
| marker two back, header of the second-newest | 9,848,030, then `Corrupt` | ok / ok / ok / ok | ok | 2 | no breach, unverified | refused: `Corrupt` | refused: `Corrupt` | 0 / 0 |
| marker two back, header of the newest | 9,849,015, then `Corrupt` | ok / ok / Corrupt / Corrupt | `Corrupt` | 1 | error: `Corrupt` | opens at 9,849,015, CompletedRoll, QuarantinedSegment | refused: `Corrupt` | 492 / 0 |

In every case, for both policies, every record that was in a segment before the writer opened was afterwards byte for
byte in a segment or in `quarantine/` ("newly in neither" is 0 in all 21 × 2 × 5 cells, also after the writer had
appended a record and checkpointed). Records the damage itself removed (a missing segment, a damaged record) are not
counted. "In a segment" does not mean readable: records behind damage are counted too. A quarantined record is out of
the journal and its sequence number is used again. **In these 21 cases recovery discarded nothing that was still in a
segment when the writer opened.** Damage that recovery cannot tell from a torn append (C11 (3)) is not among them: it
is zeroed without a copy under either policy. Run by hand on a small journal on macOS (2026-11-26): with the newest
record's commit word zeroed, after the marker, an open under `Fail`, and with the newest two under `Quarantine`,
reported `ZeroedUncommitted`, created no `quarantine/` and used the sequence numbers again; `verify` before the open
had reported no problem. What the table also shows:

- **Damage that the open does not look at is not reported.** With the marker past it, damage in an older segment (or a
  missing one) does not stop `Writer::open` under either policy, `seek_to_end` (the header of the oldest segment is the
  exception, below) or `status` ("no breach"), as documented (`operations.md` → Monitoring); `verify` reports every
  case except a missing oldest segment, which looks like retention, and a removed marker. Readers stop at the damage,
  and a reader can pass it with a seek to a record in a later segment.
- **A zeroed commit word in an older segment makes a reader stop without an error** (`Ok(None)`, the sign of the end of
  the journal), here at half of the journal, while the writer continues beyond it; `verify` finds it. No document says
  this yet: C11 and `try_next` describe an empty slot only as "no new record yet". It is a defect, issue #79
  (fixed after these runs: readers report it, ADR 0050).
- **A damaged header of the oldest segment** makes `Reader::open` fail, and with it every seek and `seek_to_end` of a
  new reader, although no other segment is damaged. The writer opens. By the code (not run), retention cannot remove
  that segment either: `retention::plan` reads every header and fails.
- **A missing newest segment after the marker** is not refused by either policy: the writer creates it again, empty
  (reported as `CompletedRoll`, the same report as after a crash during a roll), and gives the 492 records it held
  (never covered by a checkpoint, but readable until then) their sequence numbers again. With the marker in that
  segment the open is refused. By design: `format.md` (Recovery, step 4), C8 and C11 allow a missing successor to be
  created, and no record the marker covers is dropped.
- **No marker and one damaged record in the middle:** `Quarantine` keeps the records before it in the journal and moves
  every later segment into `quarantine/` (4,999 segments; 3.0 s warm, 11.3 s cold), because without a marker the chain
  ends there. `Fail` refuses. With the marker past the damage the open does not touch it.

Times on x86-64 (`85726e3`), 10,000 segments warm / 10,000 segments cold / 64 segments of 128 MiB cold, for one run
each. Where damage cut the replay short, it stopped there; a `Corrupt` result waits 50 ms before it is returned (the reader reads
the record again), which shows in seeks of a few milliseconds. The warm replays of the first row are not from a warm
cache: the journals of both groups together are larger than memory. In the 10,000-segment cold runs the directory had
just been written, so the cold writer opens are faster than in the table above.

| Damage | replay, safe | replay, zero-copy | `verify` | `status` | `seek_to_end` | writer open, `Quarantine` | writer open, `Fail` |
|---|---|---|---|---|---|---|---|
| nothing (marker in the newest segment) | 18.8 s / 18.7 s / 11.6 s | 11.3 s / 19.1 s / 11.6 s | 8.44 s / 14.3 s / 11.7 s | 15.7 ms / 72.5 ms / 49.1 ms | 5.93 ms / 24.8 ms / 46.7 ms | 8.53 ms / 11.6 ms / 258 ms | 8.29 ms / 11.2 ms / 255 ms |
| oldest segment missing | 11.3 s / 18.5 s / 11.4 s | 10.8 s / 18.4 s / 11.4 s | 4.42 s / 14.3 s / 11.5 s | 15.7 ms / 70.1 ms / 47.9 ms | 5.96 ms / 28.4 ms / 45.1 ms | 8.28 ms / 11.4 ms / 252 ms | 8.35 ms / 11.2 ms / 279 ms |
| middle segment missing | 1.08 s / 9.15 s / 5.82 s | 758 ms / 9.39 s / 5.83 s | 2.24 s / 14.2 s / 11.5 s | 15.4 ms / 71.0 ms / 46.0 ms | 5.90 ms / 24.0 ms / 46.0 ms | 8.26 ms / 11.6 ms / 255 ms | 8.32 ms / 11.8 ms / 250 ms |
| newest segment missing (the marker names it) | 2.14 s / 18.7 s / 11.5 s | 1.50 s / 18.9 s / 11.5 s | 2.22 s / 14.3 s / 11.5 s | 29.7 ms / 81.2 ms / 181 ms | 6.08 ms / 24.5 ms / 182 ms | 7.76 ms / 8.84 ms / 994 µs | 7.86 ms / 8.65 ms / 1.27 ms |
| a payload byte flipped in the middle segment | 1.12 s / 9.01 s / 5.97 s | 824 ms / 9.16 s / 5.97 s | 2.18 s / 14.2 s / 11.6 s | 15.5 ms / 71.4 ms / 47.8 ms | 5.89 ms / 24.5 ms / 45.9 ms | 8.35 ms / 11.0 ms / 243 ms | 8.35 ms / 10.1 ms / 285 ms |
| a payload byte flipped in the newest segment, before the marker | 2.19 s / 18.6 s / 11.5 s | 1.55 s / 18.8 s / 11.5 s | 2.83 s / 14.9 s / 12.1 s | 16.0 ms / 69.1 ms / 45.5 ms | 5.98 ms / 23.8 ms / 45.4 ms | 8.38 ms / 11.5 ms / 238 ms | 8.43 ms / 11.1 ms / 264 ms |
| a payload byte flipped in the newest segment, after the marker | 2.19 s / 18.5 s / 11.6 s | 1.55 s / 18.6 s / 11.6 s | 2.83 s / 14.9 s / 12.2 s | 15.9 ms / 74.6 ms / 24.8 ms | 55.9 ms / 75.9 ms / 73.0 ms | 11.2 ms / 14.9 ms / 503 ms | 7.96 ms / 9.60 ms / 22.6 ms |
| a commit word zeroed in the middle segment | 1.07 s / 9.26 s / 5.92 s | 769 ms / 9.52 s / 5.92 s | 2.82 s / 14.9 s / 12.3 s | 15.5 ms / 78.2 ms / 47.7 ms | 5.93 ms / 24.9 ms / 45.7 ms | 8.34 ms / 11.4 ms / 282 ms | 8.36 ms / 10.8 ms / 253 ms |
| a commit word zeroed in the newest segment, after the marker | 2.13 s / 18.5 s / 11.5 s | 1.50 s / 18.2 s / 11.5 s | 2.24 s / 14.3 s / 11.7 s | 15.7 ms / 73.2 ms / 23.9 ms | 5.96 ms / 24.0 ms / 22.8 ms | 11.0 ms / 15.5 ms / 509 ms | 8.02 ms / 11.5 ms / 246 ms |
| header of the oldest segment | 2.94 ms / 20.9 ms / 546 µs | 2.94 ms / 21.2 ms / 542 µs | 2.18 s / 14.3 s / 11.5 s | 15.7 ms / 69.2 ms / 47.7 ms | 2.99 ms / 22.5 ms / 544 µs | 8.29 ms / 11.4 ms / 264 ms | 8.40 ms / 10.8 ms / 259 ms |
| header of the middle segment | 1.08 s / 8.81 s / 5.83 s | 755 ms / 9.25 s / 5.83 s | 2.19 s / 14.2 s / 11.5 s | 15.7 ms / 70.8 ms / 47.7 ms | 5.89 ms / 23.1 ms / 46.0 ms | 8.32 ms / 11.6 ms / 247 ms | 8.33 ms / 11.6 ms / 256 ms |
| header of the newest segment (the marker names it) | 2.16 s / 18.4 s / 11.5 s | 1.50 s / 18.6 s / 11.5 s | 2.19 s / 14.3 s / 11.5 s | 14.9 ms / 67.2 ms / 1.25 ms | 5.83 ms / 26.0 ms / 1.03 ms | 7.80 ms / 9.07 ms / 1.09 ms | 7.89 ms / 9.07 ms / 1.09 ms |
| marker removed, nothing else | 2.15 s / 18.2 s / 11.6 s | 1.54 s / 18.6 s / 11.6 s | 2.22 s / 14.3 s / 11.6 s | 16.0 ms / 69.7 ms / 92.9 ms | 5.89 ms / 28.2 ms / 90.9 ms | 1.75 s / 19.0 s / 11.8 s | 1.74 s / 18.5 s / 11.8 s |
| marker removed, a payload byte flipped in the middle segment | 1.12 s / 8.98 s / 5.97 s | 801 ms / 9.25 s / 5.97 s | 2.19 s / 14.2 s / 11.6 s | 15.6 ms / 68.1 ms / 93.3 ms | 6.04 ms / 26.3 ms / 90.9 ms | 3.05 s / 11.3 s / 6.39 s | 872 ms / 9.04 s / 5.92 s |
| marker removed, middle segment missing | 1.08 s / 9.18 s / 5.82 s | 782 ms / 8.81 s / 5.83 s | 2.22 s / 14.2 s / 11.5 s | 15.6 ms / 68.2 ms / 92.9 ms | 5.95 ms / 23.1 ms / 90.9 ms | 874 ms / 9.05 s / 5.83 s | 879 ms / 9.24 s / 5.83 s |
| nothing (marker two segments back) | 17.6 s / 19.4 s / 11.6 s | 17.1 s / 19.1 s / 11.6 s | 2.61 s / 14.3 s / 11.6 s | 16.2 ms / 72.4 ms / 93.4 ms | 5.95 ms / 27.4 ms / 91.0 ms | 8.92 ms / 14.7 ms / 634 ms | 8.66 ms / 13.8 ms / 610 ms |
| marker two back, newest segment missing | 2.12 s / 19.0 s / 11.5 s | 1.48 s / 19.2 s / 11.5 s | 2.21 s / 14.2 s / 11.5 s | 15.5 ms / 68.8 ms / 181 ms | 6.04 ms / 28.2 ms / 181 ms | 14.8 ms / 13.3 ms / 321 ms | 14.5 ms / 12.7 ms / 321 ms |
| marker two back, second-newest segment missing | 2.15 s / 18.9 s / 11.3 s | 1.49 s / 19.1 s / 11.3 s | 2.22 s / 14.2 s / 11.5 s | 16.2 ms / 67.3 ms / 94.1 ms | 5.93 ms / 27.4 ms / 91.2 ms | 7.94 ms / 9.78 ms / 137 ms | 8.01 ms / 9.99 ms / 137 ms |
| marker two back, a payload byte flipped in the second-newest | 2.18 s / 19.5 s / 11.4 s | 1.53 s / 20.0 s / 11.4 s | 2.18 s / 14.3 s / 11.6 s | 15.6 ms / 69.7 ms / 93.8 ms | 5.92 ms / 29.6 ms / 91.8 ms | 12.1 ms / 15.8 ms / 651 ms | 8.19 ms / 11.0 ms / 228 ms |
| marker two back, header of the second-newest | 2.13 s / 19.1 s / 11.3 s | 1.52 s / 19.3 s / 11.3 s | 2.19 s / 14.3 s / 11.5 s | 15.6 ms / 68.7 ms / 93.4 ms | 5.98 ms / 26.1 ms / 91.4 ms | 8.09 ms / 10.2 ms / 137 ms | 8.09 ms / 10.6 ms / 138 ms |
| marker two back, header of the newest | 2.13 s / 18.9 s / 11.5 s | 1.51 s / 19.6 s / 11.5 s | 2.18 s / 14.3 s / 11.5 s | 14.8 ms / 66.5 ms / 1.18 ms | 5.85 ms / 24.7 ms / 1.24 ms | 9.84 ms / 13.6 ms / 321 ms | 8.23 ms / 11.8 ms / 320 ms |

### Catch-up under a live writer

A reader process that starts behind a writer appending 1,024 B records at 100,000/s (106 MB/s of journal bytes, a
background checkpoint every second, `tt-scale catch-up`), on the 64 GiB journal above (which the writer extends).
Writer and reader pinned to one logical CPU per core. 5 s warm-up, 20 s before the reader starts (baseline), the
catch-up, then 20 s of live following. Two runs each; every record's sequence number was checked by the reader. "Cold":
the page cache was dropped before the run, so the backlog comes from the SSD; "in the page cache": the records had just
been written. Times are ranges over the two runs; append latency is measured from the intended time of each record (a
late schedule counts, no skipped sends), in µs or ms; "caught up" is the first time the reader found no record.

| Machine | Backlog | Reader | Wait | Time to catch up | Records read | Rate while catching up | Read from storage | Reader CPU | Append p99 / p99.9 / max: before | while catching up | after (live) | Visible latency live, p50 / p99 / p99.9 / max |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| x86-64 (`85726e3`) | 32 M records (31.7 GiB), cold | safe | spin | 67.4–67.6 s | 41.2 M | 625–626 MB/s | 31.8 GiB | 14.5 s | 2.55–4.05 µs / 25.7–28.9 µs / 480–961 µs | 2.94–5.08 µs / 36.7–46.3 µs / 1.94–2.56 ms | 3.27–5.32 µs / 31.0–31.2 µs / 621 µs–2.06 ms | 3.44–3.45 µs / 6.72–8.46 µs / 35.5–36.2 µs / 653 µs–2.06 ms |
| x86-64 (`85726e3`) | 32 M records (31.7 GiB), cold | safe | block | 67.4 s | 41.2 M | 626–627 MB/s | 31.8 GiB | 14.5 s | 1.81–2.39 µs / 28.5–30.1 µs / 1.56–2.17 ms | 2.14–2.27 µs / 34.8–39.8 µs / 1.65–2.49 ms | 5.78–7.93 µs / 29.2–31.9 µs / 1.07–1.99 ms | 8.18–8.20 µs / 14.8–14.9 µs / 38.7–96.4 µs / 1.10–3.05 ms |
| x86-64 (`85726e3`) | 32 M records (31.7 GiB), cold | zero-copy | spin | 78.9 s | 42.4 M | 550 MB/s | 38.0 GiB | 14.5 s | 3.21–5.11 µs / 24.7–27.0 µs / 593–657 µs | 4.33–6.17 µs / 34.3–39.8 µs / 2.33–2.53 ms | 4.37–6.73 µs / 27.1–27.5 µs / 636 µs–1.52 ms | 0.91–0.92 µs / 4.88–7.28 µs / 119–283 µs / 2.83–5.37 ms |
| x86-64 (`85726e3`) | 32 M records (31.7 GiB), cold | zero-copy | block | 79.0 s | 42.4 M | 550 MB/s | 38.1 GiB | 14.4 s | 2.06–2.23 µs / 26.6–28.8 µs / 588 µs–1.94 ms | 2.08–2.46 µs / 36.2–61.0 µs / 2.56–2.70 ms | 6.00–6.08 µs / 29.5–31.5 µs / 772 µs–1.69 ms | 5.33–5.42 µs / 11.5 µs / 474–605 µs / 6.08–6.25 ms |
| x86-64 (`85726e3`) | 2 M records (2.0 GiB), in the page cache | safe | spin | 1.01–1.03 s | 4.6 M | 4,596–4,669 MB/s | 0.0 GiB | 1.0 s | 1.85–5.05 µs / 24.8–29.7 µs / 476–582 µs | 1.96–6.22 µs / 28.9–34.7 µs / 410–463 µs | 2.93–6.34 µs / 29.3–31.6 µs / 680–760 µs | 3.44–3.45 µs / 6.76–10.1 µs / 34.0–36.3 µs / 716–770 µs |
| x86-64 (`85726e3`) | 2 M records (2.0 GiB), in the page cache | safe | block | 1.00–1.02 s | 4.6 M | 4,636–4,706 MB/s | 0.0 GiB | 1.0 s | 4.22–4.85 µs / 26.5–28.5 µs / 757 µs–1.93 ms | 4.51–6.12 µs / 23.7–25.6 µs / 94.3–233 µs | 6.80–7.23 µs / 28.1–29.1 µs / 498–536 µs | 8.21–8.23 µs / 14.2–14.5 µs / 38.4–52.4 µs / 530 µs–3.58 ms |
| x86-64 (`85726e3`) | 2 M records (2.0 GiB), in the page cache | zero-copy | spin | 649–664 ms | 4.6 M | 7,043–7,202 MB/s | 0.0 GiB | 0.6 s | 1.88–3.72 µs / 25.8–27.2 µs / 485–700 µs | 2.56–5.03 µs / 38.0–40.8 µs / 450–488 µs | 2.87–5.33 µs / 26.8–26.9 µs / 503–514 µs | 0.91 µs / 3.15–5.80 µs / 64.4–65.8 µs / 1.37 ms |
| x86-64 (`85726e3`) | 2 M records (2.0 GiB), in the page cache | zero-copy | block | 659–672 ms | 4.6 M | 6,956–7,091 MB/s | 0.0 GiB | 0.7 s | 2.02–2.85 µs / 26.9–27.4 µs / 586 µs–1.89 ms | 2.74–3.30 µs / 16.8 µs–1.27 ms / 52.2 µs–1.88 ms | 6.26–7.56 µs / 27.6–28.5 µs / 600–667 µs | 5.29–5.35 µs / 10.5–12.1 µs / 179–278 µs / 1.38–3.58 ms |
| aarch64 (`d2074c1`) | 32 M records (31.7 GiB), cold | safe | spin | 77.4–77.7 s | 42.3 M | 557–559 MB/s | 31.8 GiB | 7.0 s | 2.56–2.67 µs / 17.5–17.6 µs / 289–323 µs | 2.63–2.73 µs / 205–223 µs / 2.10–3.51 ms | 2.79–2.90 µs / 19.8–23.3 µs / 309–315 µs | 2.77–2.79 µs / 5.34–5.95 µs / 22.8–26.5 µs / 337–342 µs |
| aarch64 (`d2074c1`) | 32 M records (31.7 GiB), cold | safe | block | 77.4–77.5 s | 42.2 M | 558–559 MB/s | 31.8 GiB | 6.8 s | 2.53–3.05 µs / 18.9–19.6 µs / 292–298 µs | 2.55–3.16 µs / 151–231 µs / 3.01–3.53 ms | 4.43–5.33 µs / 19.8–21.6 µs / 299–335 µs | 6.61–6.64 µs / 9.46–10.2 µs / 24.8–26.9 µs / 331–556 µs |
| aarch64 (`d2074c1`) | 32 M records (31.7 GiB), cold | zero-copy | spin | 93.3–94.6 s | 44.0 M | 476–481 MB/s | 39.4 GiB | 6.1 s | 2.47–2.57 µs / 17.5–20.3 µs / 294–338 µs | 2.53–2.71 µs / 190–285 µs / 3.53–3.99 ms | 2.53–3.01 µs / 18.0–23.2 µs / 303–318 µs | 0.57–0.58 µs / 2.19–3.04 µs / 26.3–33.2 µs / 1.09–1.10 ms |
| aarch64 (`d2074c1`) | 32 M records (31.7 GiB), cold | zero-copy | block | 77.2 s | 42.2 M | 560 MB/s | 31.9 GiB | 5.6 s | 2.54–2.62 µs / 20.8–24.4 µs / 306–411 µs | 2.52–2.64 µs / 140–294 µs / 3.39–11.6 ms | 4.20–4.24 µs / 26.8–29.6 µs / 303–720 µs | 3.93–4.08 µs / 5.78–7.28 µs / 48.4–91.6 µs / 1.29–1.51 ms |
| aarch64 (`d2074c1`) | 2 M records (2.0 GiB), in the page cache | safe | spin | 737–882 ms | 4.6 M | 5,328–6,358 MB/s | 0.0 GiB | 0.7 s | 2.46–2.50 µs / 17.5–19.7 µs / 300–319 µs | 2.50–2.52 µs / 18.5–23.5 µs / 216 µs | 2.78–2.80 µs / 19.4–20.9 µs / 318 µs | 2.75–2.76 µs / 5.16–6.72 µs / 22.4–24.2 µs / 354–477 µs |
| aarch64 (`d2074c1`) | 2 M records (2.0 GiB), in the page cache | safe | block | 721–737 ms | 4.6 M | 6,352–6,496 MB/s | 0.0 GiB | 0.7 s | 2.52–2.64 µs / 17.3–17.7 µs / 311–318 µs | 2.50–2.60 µs / 18.8–24.3 µs / 46.5–296 µs | 4.23–4.25 µs / 19.9–20.6 µs / 303–449 µs | 6.46–6.54 µs / 9.14–9.75 µs / 25.1–35.4 µs / 610 µs–4.02 ms |
| aarch64 (`d2074c1`) | 2 M records (2.0 GiB), in the page cache | zero-copy | spin | 465–476 ms | 4.5 M | 9,790–10,015 MB/s | 0.0 GiB | 0.5 s | 2.51–2.60 µs / 18.0–23.9 µs / 301–323 µs | 2.46–2.66 µs / 23.0–24.5 µs / 45.4–196 µs | 2.57–2.63 µs / 18.8–20.8 µs / 288–314 µs | 0.57–0.58 µs / 2.21–2.40 µs / 28.5–32.1 µs / 1.06–1.14 ms |
| aarch64 (`d2074c1`) | 2 M records (2.0 GiB), in the page cache | zero-copy | block | 466–487 ms | 4.5 M | 9,565–9,983 MB/s | 0.0 GiB | 0.5 s | 2.39–2.66 µs / 19.6–20.5 µs / 362–365 µs | 2.49–2.62 µs / 19.7–26.7 µs / 61.1–230 µs | 4.50–5.36 µs / 21.5–28.7 µs / 311–336 µs | 3.93–4.05 µs / 6.52–6.76 µs / 56.2–888 µs / 1.13–4.01 ms |

- Catching up took 67 to 79 s (x86-64) and 77 to 95 s (aarch64) for the 31.7 GiB on the SSD and 0.5 to 1.0 s for the 2
  GiB in the page cache. The writer kept its rate: 0.2 to 0.6 % of the sends were more than 1 µs late, in every phase.
- The writer while a reader caught up from a cold backlog: the median of `append` (0.15 µs on x86-64, 0.24 to 0.27 µs
  on aarch64) and the p99 from the intended time (1.8 to 6.2 µs) did not change. **The p99.9 rose**, from 25 to 30 µs
  before the reader started to 34 to 61 µs on x86-64 and from 17 to 24 µs to 140 to 294 µs on aarch64, and so did the
  maximum there (0.5 to 2.2 ms before, 1.6 to 2.7 ms on x86-64; 0.3 to 0.4 ms before, 2.1 to 11.6 ms on aarch64), while
  the reader competed for the SSD. From the page cache, the phase is 0.46 to 1.03 s and 46,000 to 103,000 appends:
  p99.9 was 17 to 41 µs, except for one run (x86-64, zero-copy, blocking: 1.3 ms).
- After the catch-up, a reader that waits costs the writer: the median of `append` was 0.43 to 0.46 µs with a spinning
  reader and 1.4 to 1.5 µs with a blocking one on x86-64 (0.29 to 0.35 and 2.1 to 2.2 µs on aarch64). With a blocking
  reader the writer makes the wake-up call; why a spinning one costs it 0.3 µs on x86-64 is not established. In the live
  phase the p99.9 is that of the baseline.
- The zero-copy reader caught up more slowly than the safe one from the cold backlog, reading 38 GiB (x86-64) or 39 GiB
  (aarch64) for 31.7 GiB, in every run except the two with a blocking reader on aarch64, which read 31.9 GiB and were
  as fast as the safe reader. The cause is not established. From the page cache the zero-copy reader is about 1.5 times
  as fast (7,000 to 7,200 against 4,600 to 4,700 MB/s on x86-64; 9,600 to 10,000 against 5,300 to 6,500 on aarch64).

### Still not measured

Archiving under a live writer at this scale, the archive of the 128 MiB journal on aarch64, retention by age or bytes
with a large backlog, catch-up of a durable-only reader, a reader of a journal of more than 100,000 segments, other
filesystems than ext4 (xfs in particular; APFS and btrfs only for the damage outcomes), open files at 100,000 segments,
and a seek index on a journal a writer is sealing segments of. Nothing here was measured with #74 to #77; #76 changes
the zero-copy reader. Damage that recovery zeroes in place (C11 (3)) is not among the damage cases. Power loss is not
tested (`crash-contract.md`). The costs that grow with the number of segments are issue #80.

## 2026-11-26, an archiver's removal checks what it took (OrbStack VM and macOS, informal)

What ADR 0055 (issue #83) adds to a removal by an archiver (`retention::apply`, `tickertape archive`); a writer's own
retention run makes the same calls as before. **Informal: an OrbStack VM (Linux on macOS) and a loaded Mac, not bare
metal.** Mac: Apple M4 Pro, Darwin 27.2, APFS. VM: OrbStack Ubuntu (aarch64, 13 vCPUs), the journal on ext4 in a
1.5 GB loop image. Before: `main` at `9d74a89`; after: the branch of the pull request that adds this section.

**System calls per removal** (a deletion; `strace -f -c` of `tt-scale measure --ops retention-delete` removing 100 and
300 segments of a journal of 1,500 × 64 KiB, the difference divided by 200): 35 before, 40 after. Added: `renameat2`
(the unlink stays, of the temporary name), `statx` (the identity of the file taken out), two `pread64` (the marker's
slots, read again) and `getpid` (the temporary name). The `openat`/`close` count is the same: the marker read before
the checks now opens the marker once instead of twice, and the one after the rename opens it once. Syncs: the same (the
journal directory and the segment per removal, the marker once per slot).

**Time.** Those calls alone, without the syncs, in a loop that runs the three variants in turn (800 removals each;
`unlink` / `rename`+`unlink` / the whole addition): ext4 in the VM 1.3 / 4.1 / 5.6 µs; APFS 28 / 103 / 120 µs (a rename
is costly on APFS). With the directory sync and the barrier after each, as a removal makes them, the medians were within
each other's noise (ext4 about 2.4 ms, APFS about 5 ms). `tt-scale measure --ops retention-delete --count 500` on the
VM's ext4, 30 rounds alternating which build goes first: median 1.169 ms per removal before and 1.207 ms after (+3 %;
smallest 1.031 and 1.040 ms). On the Mac, the same comparison varied from 6 to 22 ms per removal between rounds (other
work was syncing the same disk) and settles nothing.

## 2026-11-25, retention on journals of many segments (Google Cloud VM, Linux x86-64)

Before and after ADR 0051 (issue #80) on one machine of the type and zone the costs were found on
([above](#2026-11-25-replay-and-recovery-at-scale-the-gaps-of-100-google-cloud-vms)). **A cloud virtual machine with local
NVMe SSD, not bare metal.**

- **Machine.** `c3-standard-4-lssd`, us-east4-a: Intel Xeon Platinum 8481C, 4 vCPUs (2 cores), 16.8 GB; Ubuntu 24.04,
  kernel 7.0.0-1017-gcp, rustc 1.100.0; one local NVMe SSD (375 GB), ext4 formatted with `-E
  lazy_itable_init=0,lazy_journal_init=0`, mounted `rw,noatime`; `vm.dirty_ratio` 20, `vm.dirty_background_ratio` 10. The
  result files were written to the same filesystem. 2026-11-25, 21:00 to 21:23 UTC.
- **Commits.** After: `9a3a6ed` (clean tree; the branch of the pull request that adds this section). Before: the same tree
  with the core of `main` at `f1ea454` (`git checkout f1ea454 -- crates/tickertape`), so the tool is the same. The raw
  output (`environment.txt`) records only `9a3a6ed`: the "before" core is stated here, not in it. The branch later added,
  after review, the header checks of `max_bytes` (ADR 0051 → Lengths a header confirms), which read the header of each new
  segment once; that code was not run on this machine (on macOS, informally, the checkpoint loop with `max_bytes` did not
  change within its noise).
- **Command.** `benches/scale-session.sh /data/j <out> retention` with `TT_SCALE` and `TT_SCALE_BASE` (`benches/README.md`).
- **How it ran.** One journal per shape (`tt-scale build`, 1,024 B records), both cores in turn on it, five rounds, which of
  the two goes first alternating; each operation in a process of its own. The cells are the median over the five rounds
  and, in brackets, the smallest and largest. A checkpoint's value in a round is the p50 of 20 blocking checkpoints
  (`Writer::checkpoint` alone, after one record; where one segment is removed by count, after the records that fill a
  segment, so those also sync a segment's data). Every run was checked: no error, and the loops that remove removed
  exactly one segment per checkpoint. There is no schedule to fall behind, so coordinated omission does not arise. The
  removals shrink the journals by less than 2,000 segments over a shape's run, the same for both. "Cold" drops the page
  cache before the process starts, but the process lists the journal before it opens the writer, so the directory itself
  is cached by then (which is why the open without a policy is shorter than above).

Checkpoints, p50:

| | | 10,000 × 1 MiB | 20,000 × 1 MiB | 100,000 × 64 KiB |
|---|---|---|---|---|
| checkpoint, no policy | before | 287 µs (248 µs–336 µs) | 298 µs (289 µs–348 µs) | 184 µs (179 µs–216 µs) |
|  | after | 291 µs (191 µs–295 µs) | 321 µs (216 µs–399 µs) | 188 µs (183 µs–209 µs) |
| checkpoint, `max_segments`, nothing to remove | before | 27.7 ms (26.6 ms–29.5 ms) | 64.3 ms (62.9 ms–66.0 ms) | 330 ms (327 ms–334 ms) |
|  | after | 3.20 ms (3.10 ms–3.34 ms) | 6.36 ms (6.29 ms–6.64 ms) | 31.3 ms (31.3 ms–31.8 ms) |
| checkpoint, `max_age`, nothing to remove | before | 27.5 ms (27.1 ms–28.4 ms) | 64.3 ms (62.5 ms–66.6 ms) | 329 ms (328 ms–334 ms) |
|  | after | 3.21 ms (3.10 ms–3.35 ms) | 6.38 ms (6.30 ms–6.67 ms) | 31.4 ms (31.3 ms–31.8 ms) |
| checkpoint, `max_bytes`, nothing to remove | before | 27.5 ms (26.5 ms–29.5 ms) | 63.9 ms (62.5 ms–67.2 ms) | 330 ms (327 ms–336 ms) |
|  | after | 3.29 ms (3.18 ms–3.44 ms) | 6.48 ms (6.41 ms–6.75 ms) | 32.2 ms (32.0 ms–32.6 ms) |
| checkpoint, `max_segments`, removes one | before | 33.8 ms (33.3 ms–36.5 ms) | 75.3 ms (73.7 ms–78.1 ms) | 373 ms (370 ms–375 ms) |
|  | after | 5.32 ms (5.03 ms–5.48 ms) | 8.51 ms (8.16 ms–8.86 ms) | 32.8 ms (32.3 ms–33.1 ms) |
| checkpoint, `max_age`, removes one | before | 38.5 ms (36.9 ms–40.8 ms) | 77.7 ms (76.0 ms–79.1 ms) | 372 ms (371 ms–375 ms) |
|  | after | 4.22 ms (4.16 ms–4.44 ms) | 7.63 ms (7.48 ms–7.82 ms) | 33.4 ms (32.9 ms–33.4 ms) |

Checkpoints, p99 and p99.9 (with 20 checkpoints per round, both are the largest of the 20, as printed):

| | | 10,000 × 1 MiB | 20,000 × 1 MiB | 100,000 × 64 KiB |
|---|---|---|---|---|
| checkpoint, no policy | before | 926 µs (785 µs–971 µs) | 1.05 ms (867 µs–2.25 ms) | 846 µs (770 µs–2.18 ms) |
|  | after | 978 µs (866 µs–2.47 ms) | 998 µs (954 µs–1.67 ms) | 875 µs (819 µs–907 µs) |
| checkpoint, `max_segments`, nothing to remove | before | 32.3 ms (30.5 ms–32.8 ms) | 66.4 ms (65.3 ms–67.4 ms) | 333 ms (330 ms–339 ms) |
|  | after | 3.96 ms (3.90 ms–5.35 ms) | 7.06 ms (6.88 ms–7.30 ms) | 32.0 ms (32.0 ms–32.5 ms) |
| checkpoint, `max_age`, nothing to remove | before | 28.6 ms (28.1 ms–30.5 ms) | 65.9 ms (65.5 ms–68.5 ms) | 332 ms (331 ms–341 ms) |
|  | after | 3.96 ms (3.83 ms–4.19 ms) | 7.11 ms (6.90 ms–7.30 ms) | 32.1 ms (32.0 ms–32.7 ms) |
| checkpoint, `max_bytes`, nothing to remove | before | 29.6 ms (28.4 ms–31.7 ms) | 65.2 ms (64.1 ms–68.1 ms) | 333 ms (330 ms–340 ms) |
|  | after | 4.12 ms (3.95 ms–6.18 ms) | 7.51 ms (7.26 ms–7.90 ms) | 34.1 ms (34.0 ms–34.5 ms) |
| checkpoint, `max_segments`, removes one | before | 36.3 ms (35.2 ms–38.7 ms) | 77.7 ms (74.8 ms–85.1 ms) | 379 ms (375 ms–399 ms) |
|  | after | 6.99 ms (6.46 ms–7.59 ms) | 10.1 ms (9.44 ms–10.8 ms) | 33.4 ms (33.4 ms–38.1 ms) |
| checkpoint, `max_age`, removes one | before | 39.2 ms (37.8 ms–41.3 ms) | 78.8 ms (76.9 ms–80.1 ms) | 379 ms (374 ms–383 ms) |
|  | after | 4.58 ms (4.46 ms–4.84 ms) | 8.34 ms (7.66 ms–9.49 ms) | 33.7 ms (33.3 ms–34.2 ms) |

`Writer::open`:

| | | 10,000 × 1 MiB | 20,000 × 1 MiB | 100,000 × 64 KiB |
|---|---|---|---|---|
| cold, no policy | before | 8.67 ms (8.46 ms–8.85 ms) | 15.8 ms (15.7 ms–16.1 ms) | 72.9 ms (72.5 ms–73.4 ms) |
|  | after | 8.26 ms (8.19 ms–8.65 ms) | 15.9 ms (15.6 ms–17.1 ms) | 73.0 ms (71.9 ms–73.6 ms) |
| cold, `max_segments` | before | 1.6 s (1.5 s–1.7 s) | 3.9 s (3.9 s–4.0 s) | 20.3 s (20.1 s–20.9 s) |
|  | after | 11.5 ms (11.3 ms–11.7 ms) | 22.3 ms (21.8 ms–22.6 ms) | 108 ms (107 ms–109 ms) |
| cold, `max_age` | before | 1.6 s (1.6 s–1.7 s) | 3.9 s (3.9 s–4.1 s) | 20.1 s (19.9 s–20.8 s) |
|  | after | 11.7 ms (11.4 ms–12.4 ms) | 22.5 ms (22.0 ms–23.0 ms) | 108 ms (108 ms–109 ms) |
| cold, `max_bytes` | before | 1.6 s (1.6 s–1.7 s) | 4.0 s (3.9 s–4.1 s) | 20.1 s (20.0 s–20.5 s) |
|  | after | 52.8 ms (52.3 ms–53.4 ms) | 111 ms (110 ms–113 ms) | 575 ms (574 ms–580 ms) |
| warm, no policy | before | 6.99 ms (6.80 ms–7.32 ms) | 13.9 ms (13.7 ms–15.1 ms) | 71.5 ms (71.0 ms–73.3 ms) |
|  | after | 7.11 ms (6.77 ms–7.40 ms) | 14.3 ms (13.9 ms–15.1 ms) | 71.6 ms (71.3 ms–72.0 ms) |
| warm, `max_segments` | before | 39.5 ms (37.3 ms–40.3 ms) | 80.2 ms (78.2 ms–82.6 ms) | 408 ms (404 ms–411 ms) |
|  | after | 10.4 ms (9.63 ms–10.6 ms) | 20.7 ms (20.2 ms–21.3 ms) | 106 ms (106 ms–107 ms) |
| warm, `max_age` | before | 35.5 ms (34.9 ms–37.4 ms) | 79.9 ms (79.4 ms–82.0 ms) | 409 ms (407 ms–412 ms) |
|  | after | 10.4 ms (9.89 ms–10.7 ms) | 20.9 ms (20.5 ms–21.4 ms) | 107 ms (107 ms–108 ms) |
| warm, `max_bytes` | before | 35.8 ms (35.0 ms–38.9 ms) | 78.7 ms (78.4 ms–83.5 ms) | 415 ms (405 ms–416 ms) |
|  | after | 21.2 ms (20.0 ms–22.2 ms) | 43.6 ms (42.7 ms–45.0 ms) | 222 ms (221 ms–223 ms) |

Stand-alone planning and removal (warm; `retention::apply` includes its one listing, spread over 100 removals):

| | | 10,000 × 1 MiB | 20,000 × 1 MiB | 100,000 × 64 KiB |
|---|---|---|---|---|
| `retention::plan`, nothing to remove | before | 31.4 ms (30.0 ms–32.6 ms) | 65.6 ms (63.8 ms–66.6 ms) | 319 ms (317 ms–322 ms) |
|  | after | 3.32 ms (3.15 ms–3.40 ms) | 6.56 ms (6.39 ms–7.20 ms) | 33.0 ms (33.0 ms–33.1 ms) |
| `retention::plan` of 100 removals | before | 30.1 ms (29.5 ms–32.7 ms) | 77.2 ms (75.8 ms–79.4 ms) | 322 ms (319 ms–323 ms) |
|  | after | 3.72 ms (3.62 ms–3.94 ms) | 20.3 ms (19.5 ms–21.7 ms) | 36.4 ms (36.0 ms–37.3 ms) |
| `retention::apply`, per segment (100 deleted) | before | 4.02 ms (3.91 ms–4.18 ms) | 7.87 ms (7.76 ms–8.14 ms) | 40.9 ms (40.7 ms–41.3 ms) |
|  | after | 367 µs (351 µs–406 µs) | 371 µs (366 µs–377 µs) | 590 µs (578 µs–638 µs) |

What remains is one directory listing per retention run, which grows with the number of segments (about 0.3 µs each): 3,
6 and 31 ms here against 0.3 ms without a policy. With `max_bytes` an open and a stand-alone plan also ask for every
file's size. Not measured: aarch64, xfs, background checkpoints, moves.

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

> **Superseded** by the [comparison rerun at v1.0.0](#2026-11-25-to-2026-11-26-comparison-rerun-at-v100-google-cloud-vms-linux-x86-64-and-aarch64),
> which corrected the harness: Chronicle Queue and Aeron ran here without a JVM warm-up and with their default settings, no thread was pinned, and tickertape's rows were measured two days after the other tools', on other VMs of the same types and at a newer commit. In the rerun every tool ran in one session per machine. These results stay as they were recorded.

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
wrote no result for them. That is the open-files defect fixed afterwards in #38; they were measured on 2026-11-25
([above](#2026-11-25-replay-and-recovery-at-scale-the-gaps-of-100-google-cloud-vms)), with the cases this run left out.

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
  closed; the platform mechanism is not confirmed, `crash-contract.md` → Known risks and limits). Not measured:
  the lagging and beyond-RAM scenarios with background checkpoints. (The 10,000- and 20,000-segment journals were
  measured on 2026-11-25.)
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

## 2026-11-26, what the writer's latency histograms cost (ADR 0047, Google Cloud VMs)

The measurements that ADR [0047](decisions/0047-latency-histograms.md) requires before the writer's histograms
(`WriterConfig::latency`) may be described by their cost: the instructions and time an append gains with the option
off, what `LatencyMode::All` adds, the tail of the visible latency, and the replay benchmark (issue #61). **4 vCPUs,
virtual machines: these are comparisons between builds on one machine in one session, not throughput or latency
results.** Base is `main` at `12e76d8`, with a second build of it ("base 2"); v1.0.0 is the released code; the change
is `9ff23da`.

### Environment and method

| | x86-64 | aarch64 |
|---|---|---|
| Machine | `c4-standard-4-lssd` (us-central1-c): Intel Xeon 6985P-C, 2 cores with 2 threads each, 15 GB | `c4a-standard-4-lssd` (us-east1-b): Google Axion (Neoverse-V2), 4 cores, 16 GB |
| Kernel, OS | 7.0.0-1017-gcp, Ubuntu 24.04 | the same |
| Clock source | `tsc` | `arch_sys_counter` |
| Filesystem | ext4 (`-E lazy_itable_init=0,lazy_journal_init=0`, `noatime`) on a local NVMe SSD; `TMPDIR` on it | the same |
| CPUs | benchmarks on CPU 1, whose SMT sibling (CPU 3) was offline for the append and replay runs; helper threads on the others | benchmarks on CPU 1 |
| rustc, Valgrind | 1.100.0, 3.22.0 | the same |

- **Instructions** (gate 1): `tt-append-cost append --payload 64 --count N` (and 1,024 B), no helper threads
  (`prefault(false)`), in one 128 MiB segment, under `valgrind --tool=callgrind`; the difference of the totals for
  N = 200,000 and 400,000 (50,000 and 100,000 at 1,024 B) divided by the difference of the counts. Exact to a fraction
  of an instruction and the same in every session.
- **Time** (gate 2, and the cost of `All`): the `append` benchmark, `append/records/prefault/{64,1024}` and
  `append/latency/{RollsAndCheckpoints,All}/{64,1024}`, each in a process of its own,
  `taskset -c 1 <bench> --bench --warm-up-time 1 --measurement-time 1 '^<id>$'`, as 40 rounds of pairs: in each round
  every build once, in an order that alternates direction and starts at a different build each round. The figure per run
  is Criterion's mean for a batch of 16,384 appends, divided by 16,384. The tables give the mean of the paired
  differences against base with its 95 % confidence interval (Student's t). A run is valid only if base 2 against base
  lies within the bound (1 ns at 64 B, 2 % at 1,024 B).
- **Parts:** `tt-append-cost parts`, three times (each the smallest of nine rounds of 5 million calls).
- **Tail:** `tt-latency run --rate 100000 --count 3100000 --warmup 100000 --payload 64 --reader mapped --json`, with
  `--writer-latency` and `--snapshot-ms` as in the tables, the writer's main thread pinned to CPU 1 and the reader's to
  CPU 0 from outside (`taskset -p`, since v1.0.0's tool has no pinning options), helpers unpinned, 3 runs per build,
  interleaved; on x86-64 with all four CPUs online.
- **Replay:** `taskset -c 1 <replay bench> --bench --warm-up-time 1 --measurement-time 2 records`, 8 rounds, interleaved.

Raw output (JSON lines per run, environment, scripts) is kept outside the repository.

### Off: gate 1, instructions per append

Gate 1 as amended on 2026-11-26 (ADR 0047): the change with the option off against **the same commit with the latency
test compiled out** (`RUSTFLAGS="--cfg tickertape_latency_check_out"`, `benches/README.md`), at most 3 more. The
difference to the base commit is reported as drift and not gated. Counted on the change as merged with `main` at
`3d82592` (the code of `9ff23da` and the review fixes, which do not touch the append path).

| | x86-64, 64 B | x86-64, 1,024 B | aarch64, 64 B | aarch64, 1,024 B |
|---|---|---|---|---|
| change, `Off` | **384** | 899.9 | **369** | 997.0 |
| change, latency test compiled out | 393 | 908.9 | 370 | 998.0 |
| **gate 1: `Off` − compiled out** | **−9** | −9 | **−1** | −1 |
| change, `RollsAndCheckpoints` | 384 | 899.9 | 369 | 997.0 |
| change, `All` | 600 (+216 over `Off`) | 1,115.9 | 576 (+207) | 1,204.0 |
| *drift:* base (`3d82592`; `12e76d8` and `8912012` the same) | 383 | 898.9 | 366 | 994.0 |
| *drift:* v1.0.0 | 381 | 896.9 | 363 | 991.0 |

x86-64 on the cloud machine; aarch64 in the OrbStack VM on an M4 Pro with the same rustc (1.100.0), which gave the cloud
machine's counts exactly for `e08c364` and, for the base and the change, the same counts as the cloud machine had on
`12e76d8`.

**Gate 1 is met on both.** The test itself costs nothing measurable in instructions: compiling it out made the append
*longer*, by 9 on x86-64 and 1 on aarch64. What the counts measure is mostly how the compiler lays out and inlines the
append path, which changes whenever that path is compiled differently. The append path exists twice in the change (the
timed copy calls the same code), and the CRC helpers' inlining into `Writer::append` (`format::finish_record_body`,
`format::record_crc`, `crc::crc32c`, `crc::crc32c_append`, the hardware `append`) moved with unrelated code:

| The same change without its three forced inlines, on | x86-64, 64 B (base) | aarch64, 64 B (base) |
|---|---|---|
| `a80f912` | 390 (383) | 366 (366)³ |
| `e08c364` | 363 (383) | 355 (366) |
| `12e76d8` | 390 (383) | 366 (366)² |

² The OrbStack VM, rustc 1.100.0. ³ The OrbStack VM, rustc 1.99.1.

With `finish_record_body`, `record_crc` and `crc32c` forced inline the drift was +1 on x86-64 and +3 on aarch64 on each of
`e08c364`, `12e76d8` and `8912012`, which is why the change keeps them: they reduced the movement in these builds. They
do not make the count independent of the rest of the crate. Variants counted on aarch64 (OrbStack, rustc 1.100.0; the
first four by the independent review of the pull request, against the base `8912012` at 366 and the change at 369):

| Variant | aarch64, 64 B |
|---|---|
| the change with the latency tests removed | 370 |
| the change without `#[cold]` on the roll's timing wrapper (`roll_measured`) | 370 |
| the base with the same three helpers forced inline | 356 |
| the change with `crc32c_append` forced inline too | 380 |
| the change with `crc32c` calling the hardware `append` directly (`#[inline]`): `Off` / test compiled out | 372 / 359 |
| the change without the three forced inlines: `Off` / test compiled out | 366 / 369 |

The change calls `crc32c_append` twice per append out of line (for the length and for the fields); the base with the
helpers forced inline calls it once. The one simple way tried to remove the second call (the fifth row) made the off path
13 instructions longer than with the test compiled out, so it was left out. The time gate below decides.

### Off: gate 2, time per append (ns)

| | x86-64, 64 B | x86-64, 1,024 B | aarch64, 64 B | aarch64, 1,024 B |
|---|---|---|---|---|
| base, mean of its 40 runs | 37.84 | 298.1 | 42.07 | 399.2 |
| **base 2 − base (the noise floor)** | −0.23 [−0.76, +0.31] | −0.43 % [−1.24, +0.37] | −0.24 [−0.51, +0.02] | +0.25 % [−0.35, +0.86] |
| v1.0.0 − base | −0.21 [−0.86, +0.44] | −0.44 % [−1.17, +0.30] | −0.19 [−0.58, +0.20] | −0.16 % [−1.15, +0.83] |
| **`Off` − base** | +0.23 [−0.33, +0.78] | +0.53 % [−0.32, +1.39] | −0.80 [−1.09, −0.51] | +0.09 % [−0.84, +1.02] |
| **`RollsAndCheckpoints` − base** | +0.20 [−0.32, +0.73] | +0.62 % [−0.60, +1.84] | −0.98 [−1.21, −0.76] | +0.07 % [−0.79, +0.94] |
| Bound | 1 ns | 2 % | 1 ns | 2 % |
| Run valid; gate met | yes; yes | yes; yes | yes; yes | yes; yes |

**Gate 2 is met on both, in valid runs, in both modes.** On aarch64 the change is faster at 64 B by 0.8 to 1 ns, with 3
more instructions: code layout rather than work. v1.0.0 and the base are within the noise floor of each other here.

Earlier runs, of code that was not merged (the change on older bases, or with other inlining), all on the same machine
types: on `a80f912`, with the writer's SMT sibling online, 40 pairs on x86-64 gave an **invalid** run at 1,024 B (base 2
− base [−0.49, +2.43] %; the change +3.13 % [+1.02, +5.25]), which is why the sibling was taken offline afterwards, and
that made base 2 − base about three times tighter there. Every valid run met both gates: without forced inlining on
`e08c364` (x86-64, 125 pairs at 64 B: `Off` +0.37 ns [+0.01, +0.74], `RollsAndCheckpoints` +0.22 [−0.09, +0.52]; 26 at
1,024 B; aarch64, 36 pairs: −0.70 and −0.78 ns, +0.13 % and −0.16 %), and with two of the three helpers forced inline
(x86-64, 70 pairs; aarch64, 50 pairs).

### `All`: the cost per append

| | x86-64 | aarch64 |
|---|---|---|
| Added to a 64-byte append (40 pairs, 95 % interval) | **+57.4 ns** [+56.8, +57.9] (37.8 → 95.2 ns) | **+83.0 ns** [+82.6, +83.5] (42.1 → 125.1 ns) |
| Added to a 1,024-byte append | +53.5 ns [+50.0, +57.0] (+18 %) | +58.4 ns [+53.3, +63.5] (+15 %) |
| Clock pair alone (`Instant::now()` and `elapsed()`) | 44.3–44.4 ns | 71.9–72.3 ns |
| Histogram update alone (the update with its value, less the value alone) | about 1.8 ns (2.04–2.05 less 0.27–0.31) | about 2.1 ns (2.54 less 0.42) |
| Median of an empty timed interval (not a floor: individual values can be lower) | 20–21 ns | 33 ns |
| Appends/s with `All` (from the paired means, 64 B, one thread) | 10.5 million | 8.0 million |

**The published budget (60 ns added to a 64-byte append, of which at most 5 ns for the histogram) is met on x86-64
(57.4 ns) and missed on aarch64 (83.0 ns).** Earlier sessions on x86-64 measured 58.7 to 60.5 ns for slightly different
code. The histogram's part (about 2 ns) is within its share on both. Almost all of the cost is the two clock reads,
through the vDSO: about 22 ns a read on x86-64 and 36 ns on aarch64 here. Since the budget is not met everywhere, the
documentation gives these figures and does not call the option cheap. With `All` an append can cost more than the
append itself.

### Tail of the visible latency (µs), not gated

Median of the runs per build [lowest..highest]. "Service" is the writer's own `append` histogram with `All`
(`WriterLatency::append`), recorded in the same runs.

| x86-64 (3 runs each) | p50 | p99 | p99.9 | max | service p50 | service p99.9 | service max |
|---|---|---|---|---|---|---|---|
| base | 0.31 | 0.49 [0.48..3.66] | 2.21 [2.18..5.19] | 871 [865..905] | | | |
| base 2 | 0.31 | 4.07 [0.47..4.91] | 5.59 [2.11..6.65] | 859 [850..901] | | | |
| v1.0.0 | 0.31 | 0.91 [0.48..2.35] | 2.50 [2.18..4.16] | 891 [772..909] | | | |
| `Off` | 0.31 | 0.54 [0.54..3.67] | 2.33 [2.19..5.29] | 854 [851..884] | | | |
| `RollsAndCheckpoints` | 0.31 | 4.19 [1.33..4.35] | 5.54 [2.73..5.92] | 868 [767..922] | | | |
| `All` | 0.33 | 0.63 [0.57..1.91] | 2.26 [2.19..3.50] | 870 [831..913] | 0.107 | 1.18 | 180 [180..250] |
| `All`, snapshot every 1 s | 0.33 | 1.86 [0.59..3.63] | 3.58 [2.30..5.37] | 921 [878..934] | 0.107 | 1.25 | 179 [172..234] |
| `All`, snapshot every 10 ms | 0.33 | 2.69 [2.62..3.45] | 5.09 [4.37..6.49] | 876 [862..909] | 0.107 | 1.31 | 188 [172..225] |

| aarch64 (3 runs each) | p50 | p99 | p99.9 | max | service p50 | service p99.9 | service max |
|---|---|---|---|---|---|---|---|
| base | 0.26 | 2.12 [1.38..2.41] | 3.10 [2.48..3.48] | 927 [905..931] | | | |
| base 2 | 0.26 | 0.36 [0.35..1.66] | 1.91 [1.87..2.27] | 935 [882..953] | | | |
| v1.0.0 | 0.26 | 0.35 [0.35..0.36] | 1.91 [1.87..2.15] | 926 [924..943] | | | |
| `Off` | 0.26 | 2.07 [0.36..2.12] | 2.98 [2.10..3.20] | 942 [913..958] | | | |
| `RollsAndCheckpoints` | 0.26 | 0.37 [0.36..2.01] | 2.41 [2.25..3.08] | 942 [924..947] | | | |
| `All` | 0.29 | 2.07 [0.40..2.52] | 2.95 [1.91..3.58] | 927 [882..951] | 0.121 | 2.03 | 195 [190..302] |
| `All`, snapshot every 1 s | 0.29 | 0.58 [0.40..1.17] | 2.34 [2.02..2.34] | 931 [916..956] | 0.121 | 2.02 | 188 [178..193] |
| `All`, snapshot every 10 ms | 0.29 | 0.99 [0.42..1.17] | 2.36 [2.29..2.72] | 918 [898..919] | 0.122 | 2.02 | 186 [174..786] |

The maximum is about 0.9 ms in every build on both machines, with the option and without it, so it is not the option's.
p99 and p99.9 are bimodal from run to run (base 2 against base, two builds of the same code, differ as much as any two
builds), so three runs show no effect of the option on the tail; earlier sessions with five runs per build (of the
change on older bases) did not either. `All` raises p50 by 0.02 to 0.03 µs, about half of what a clock pair costs alone; snapshots,
even every 10 ms, did not visibly change the writer's own service times (one run's service maximum of 786 µs on aarch64
is a single run). The service maximum, about 0.2 ms, is far below the visible maximum: most of the stall behind the
visible maximum is outside the `append` calls, which is what `WriterLatency::append` warns of.

### Replay (the reader side did not move)

`replay/records/{safe,mapped}/{64,1024}`, mean time per run against base in % with the 95 % interval, 8 rounds, in the
same sessions (x86-64 with the SMT sibling offline):

| | x86-64: base 2 | x86-64: change | aarch64: base 2 | aarch64: change |
|---|---|---|---|---|
| safe, 64 B | −3.36 [−10.95, +4.23] | −2.42 [−10.56, +5.72] | −0.37 [−3.12, +2.38] | −0.35 [−2.51, +1.82] |
| safe, 1,024 B | +0.36 [−3.11, +3.84] | +1.95 [−2.22, +6.11] | +0.34 [−0.87, +1.55] | +0.18 [−0.81, +1.17] |
| mapped, 64 B | −1.88 [−8.37, +4.62] | −1.82 [−9.89, +6.25] | −0.42 [−2.67, +1.83] | −2.24 [−4.08, −0.40] |
| mapped, 1,024 B | −0.88 [−4.60, +2.84] | −0.57 [−4.83, +3.68] | −0.35 [−1.55, +0.85] | −0.72 [−1.75, +0.31] |

Every interval of the change includes 0 or lies on the faster side, and each is within what base 2 shows on its
machine. The change touches the readers' code only through `format::record_crc` and `crc::crc32c`, which the safe reader
also calls, now always inlined. The x86-64 intervals are wide (this machine's replay varies by several percent from run
to run): they rule out a large slowdown there, not one of 2 %; aarch64's rule out more than about 1 % for the safe
reader.

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
| Wake-ups of a blocking durable-only reader by `append` | Avoidable: only a checkpoint can give it records. Removed from 1.1 by its own event and waiter counts in the region (ADR 0053, issue #82) |
| Four system calls and two small allocator calls every 100 ms per blocked reader (re-read of the `notify` record), and a `stat` per 100 ms at an empty slot (identity check) | Necessary to follow a restarted writer and to notice a replaced segment; bounded by the interval |

**Serialization and append**

| Work | Necessary? |
|---|---|
| The payload copy in `append` | Avoidable by the caller with `append_with`; the figures above give its size |
| CRC of the frame | Necessary (the record's integrity check, `format.md`) |
| Page faults of the writer at 1,024 B unpaced | Partly avoidable by a larger prefault window; Linux keeps it small on purpose (ADR 0025) |
| A mutex and a futex wake per 64 KiB for the helpers | Necessary to hand them the position; about one wake per 600 records of 104 B in total |

### The first candidate for a smaller read at the tail (not merged)

What was merged instead, ADR [0052](decisions/0052-safe-reader-tail-reads.md), is measured
[under 2026-11-26](#2026-11-26-the-safe-readers-read-size-at-the-tail-google-cloud-vms-x86-64-and-aarch64); the ADR
explains this candidate's losses from its counts. Branch `core/safe-reader-tail-reads` (`751ee43`) makes the safe reader read 4 KiB when it has caught up and grow back to
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
touch, beyond what that floor explains in some cases; on aarch64 nothing differs beyond it. The view then: worth
pursuing, but not as a fixed 4 KiB read; with hysteresis and with the code of the per-record path unchanged, gated on
x86-64 p99 at 1.5 million records/s and on replay. That is ADR 0052.

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
  explained**; nothing traced inside the guest accounts for it. (Measured since: both shapes' local SSDs are capped at
  351 MB/s, and with its defaults tickertape made the aarch64 device write 7 % more than it appended; 2026-11-26, "how
  much of the limit is the disk", below.)
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

**Note (2026-11-26).** The ranged-sync switch of that experiment (`TT_EXP_RANGED` on `exp/roll-60`) skipped only the
per-segment `fdatasync` of the active segment and kept the barrier step, which on Linux is another `fdatasync` of the
same file. That call writes the prefaulted pages back, so the experiment did not do what this paragraph says: it did not
keep prefaulted pages from being written back. Measured with both calls skipped, the checkpoint wrote 256 KiB less and
was 0.15–0.6 ms shorter
([2026-11-26](#2026-11-26-what-prefaulted-unwritten-pages-add-to-a-checkpoint-google-cloud-vms-linux)); whether that
changes the stalls of steady-90 was not measured.

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

## 2026-11-26, the safe reader's read size at the tail (Google Cloud VMs, x86-64 and aarch64)

Before and after ADR [0052](decisions/0052-safe-reader-tail-reads.md): the safe reader (`Reader::open`) reads 4 KiB
instead of 64 KiB per fill while it keeps showing that it waits for every record (issue #62; the profiles of
2026-11-25 above found it copying 64 KiB per record it followed). The tables up to
["Readers that block"](#readers-that-block-the-rule-added-after-review-x86-64) measure the change without its rule for
readers that block; that section measures the rule that merged, and an alternative that did not, on x86-64.
**Google Cloud virtual machines, not bare metal**; nothing here is compared with earlier sections.

### Environment and method

| | x86-64 | aarch64 |
|---|---|---|
| Machine | `c4-standard-4-lssd` (us-east1-b): Intel Xeon 6985P-C, 2 cores with 2 threads each, 15 GB | `c4a-standard-4-lssd` (us-east1-b): Google Axion (Neoverse-V2), 4 cores, 16 GB |
| Time (UTC) | 2026-11-26 00:56–02:52 | 2026-11-26 02:57–04:41 |
| Kernel, OS | 7.0.0-1017-gcp, Ubuntu 24.04.5 LTS | the same |
| Filesystem | ext4 (`mkfs.ext4 -E lazy_itable_init=0,lazy_journal_init=0`, mounted `rw,noatime`) on the local NVMe SSD (375 GB), I/O scheduler `none`, write-back sysctls at the defaults | the same |
| Writer, reader CPU | 0, 1; helper threads unpinned | the same |
| rustc | 1.100.0 | the same |

Commits: base `e08c364e` (`main` then), new `c7bec958` (the change on that base). Both are kept by the branch
`core/safe-reader-adaptive-read-measured`; do not delete it. The merged change adds the rule for readers that block,
measured [below](#readers-that-block-the-rule-added-after-review-x86-64) against a later `main`. Each machine built both from the public repository; the base binaries were copied to a second path to
give a second copy of the base, the noise floor (identical files, sha256 recorded). Release builds, no frame pointers.

- **Replay**: `taskset -c 1 <replay benchmark binary> --bench records --warm-up-time 0.5 --measurement-time 1` with
  `TMPDIR` on the SSD, 60 rounds, each round running base, the second copy and new in a rotating order; ms per pass of
  1,000,000 records of 64 B or 100,000 of 1,024 B. The rounds of one variant differ by up to 40 % on x86-64 (by process
  and over time, alike for all three), so the comparison is the median of the per-round ratios, beside the same for
  the second copy of the base. A first session with 10 rounds of the default measurement (5 s) showed the new build
  1.6–4.2 % slower in paired medians; a session of 60 short rounds of the same builds on the same machine then showed
  −0.8 to +0.5 % (second copy −0.3 to +0.7 %). Those are an earlier version of this change (`e12a0022`).
- **Live**: `tt-sustained run --dir /data/j --out … --warmup 3s --duration 9s --checkpoint-every 1s --checkpoint-mode
  background --retain-bytes 34359738368 --cpus 0,1 --rate R --payload P --readers safe:wait=W` (`--warmup 4s --duration
  20s` at 1.5 million records/s), 10 rounds, the three variants rotating within each round, `/data/j` removed and
  `sync` before each run. Latency in µs from the intended send time (coordinated omission: intended timestamps, no
  skipped sends). Cells: median over the rounds base → new, then the median of the per-round ratios, and the same ratio
  of the second copy against the base (the floor). "Blocks per record": the reader thread's voluntary context switches
  per record, each a futex wait. Late sends: sends more than 1 µs late.
- **Counts**: `benches/diagnostics/profile-sustained.sh DIR counts 4 -- tt-sustained run … --duration 24s --warmup 4s`
  (`SETTLE=7`, otherwise as above), base then new, one run each: system calls and bytes read of the reader thread
  (`counts.bt`) and futex wakes of the writer thread over a 4 s window (4.5–4.9 s measured), per record appended.

Raw output (results, series, count files, logs, the environment capture and the scripts) is kept outside the
repository.

### Replay

x86-64:

| Case | Base | Second copy of base | New | Second copy against base | New against base |
|---|---|---|---|---|---|
| safe, 64 B | 38.9 (32.3–50.8) | 38.8 (32.6–45.1) | 38.5 (32.6–48.3) | -0.6 % | -1.3 % (24 of 60 slower) |
| safe, 1,024 B | 16.4 (11.1–20.5) | 15.2 (11–22.3) | 15.2 (11.1–21.1) | -1.7 % | -0.3 % (28 of 60 slower) |
| mapped, 64 B | 53.1 (39.2–68.9) | 51.6 (39.9–69.2) | 50.5 (38.8–72.8) | -4.0 % | -4.2 % (17 of 60 slower) |
| mapped, 1,024 B | 12.4 (9.36–18.4) | 11.8 (9.5–18.9) | 11.8 (9.39–18.8) | -4.1 % | -1.1 % (25 of 60 slower) |

aarch64:

| Case | Base | Second copy of base | New | Second copy against base | New against base |
|---|---|---|---|---|---|
| safe, 64 B | 39.3 (38.5–42.9) | 39.1 (38.4–45.3) | 38.9 (38.4–42.4) | -0.4 % | -0.6 % (20 of 60 slower) |
| safe, 1,024 B | 15.2 (15–15.8) | 15.2 (14.9–15.9) | 15.2 (14.9–15.4) | +0.1 % | -0.2 % (23 of 60 slower) |
| mapped, 64 B | 37.1 (36.2–40.3) | 37 (36.3–41.9) | 36.4 (36–39.9) | -0.1 % | -1.6 % (7 of 60 slower) |
| mapped, 1,024 B | 10.5 (10.5–11) | 10.5 (10.5–10.9) | 10.5 (10.4–11) | -0.1 % | -0.2 % (19 of 60 slower) |

Nothing differs beyond the floor on either machine.

### Following live

x86-64:

| Wait | Payload | Records/s | p50 | p99 | p99.9 | CPU of a core | Blocks per record | Late sends |
|---|---|---|---|---|---|---|---|---|
| block | 64 B | 20,000 | 14 → 12.1 (-13.9 %; floor -0.1 %) | 19.5 → 15.4 (-17.4 %; floor +0.6 %) | 23 → 19.9 (-14.6 %; floor -0.2 %) | 22 % → 20 % (-10.8 %; floor +4.2 %) | 1.00 → 1.00 (+0.0 %; floor +0.0 %) | 0.02 % → 0.03 % (+15.6 %; floor -6.6 %) |
| spin | 64 B | 20,000 | 2.07 → 0.759 (-63.3 %; floor +0.1 %) | 2.57 → 1.26 (-51.4 %; floor +0.2 %) | 5.17 → 4.63 (-7.4 %; floor +3.2 %) | 100 % → 100 % (-0.0 %; floor -0.0 %) | 0.00 → 0.00 | 0.03 % → 0.02 % (-21.5 %; floor -2.9 %) |
| block | 1,024 B | 20,000 | 14.7 → 12.9 (-12.2 %; floor -0.2 %) | 19.7 → 16.6 (-14.8 %; floor -2.7 %) | 27.8 → 24.8 (-13.5 %; floor -3.7 %) | 22 % → 20 % (-12.2 %; floor +0.3 %) | 1.00 → 1.00 (+0.0 %; floor +0.0 %) | 0.09 % → 0.07 % (-12.3 %; floor -22.1 %) |
| spin | 1,024 B | 20,000 | 2.6 → 1.29 (-50.1 %; floor +0.0 %) | 4.57 → 3.59 (-18.4 %; floor -1.7 %) | 10.5 → 12.3 (-2.5 %; floor +9.3 %) | 100 % → 100 % (-0.0 %; floor -0.0 %) | 0.00 → 0.00 (+0.0 %; floor +0.0 %) | 0.13 % → 0.10 % (-44.7 %; floor -47.2 %) |
| block | 64 B | 200,000 | 4.02 → 2.93 (-28.0 %; floor -0.2 %) | 7.8 → 7.67 (-0.1 %; floor -0.4 %) | 16.7 → 17.1 (+2.0 %; floor +1.8 %) | 70 % → 62 % (-11.7 %; floor +0.2 %) | 0.56 → 0.76 (+38.5 %; floor +2.4 %) | 0.63 % → 0.64 % (-0.8 %; floor +5.3 %) |
| spin | 64 B | 200,000 | 2.05 → 0.758 (-62.9 %; floor -0.1 %) | 3.94 → 3.46 (-12.8 %; floor +2.7 %) | 9.52 → 8.22 (-9.2 %; floor +5.6 %) | 100 % → 100 % (-0.0 %; floor +0.0 %) | 0.00 → 0.00 (+0.0 %; floor +0.0 %) | 0.64 % → 0.61 % (-0.6 %; floor +0.5 %) |
| block | 1,024 B | 200,000 | 4.4 → 3.34 (-24.6 %; floor -0.2 %) | 10.2 → 10.3 (+0.8 %; floor +2.5 %) | 20.9 → 21.6 (+0.9 %; floor +2.2 %) | 75 % → 64 % (-14.1 %; floor -0.2 %) | 0.49 → 0.77 (+59.4 %; floor +0.2 %) | 1.45 % → 1.40 % (-2.2 %; floor -2.6 %) |
| spin | 1,024 B | 200,000 | 2.57 → 1.3 (-49.9 %; floor +0.0 %) | 7.69 → 5.53 (-31.9 %; floor -4.0 %) | 21.6 → 19.1 (-5.5 %; floor -4.0 %) | 100 % → 100 % (+0.0 %; floor +0.0 %) | 0.00 → 0.00 (-7.9 %; floor -2.5 %) | 1.08 % → 1.06 % (+8.5 %; floor +6.4 %) |
| block | 64 B | 1,500,000 | 3.1 → 3.11 (-0.0 %; floor +0.0 %) | 11.1 → 10.8 (-1.3 %; floor +0.5 %) | 45.5 → 49.7 (-0.3 %; floor +5.8 %) | 99 % → 99 % (+0.0 %; floor -0.0 %) | 0.00 → 0.00 (-2.5 %; floor +1.2 %) | 3.26 % → 3.16 % (-3.2 %; floor +0.5 %) |
| spin | 64 B | 1,500,000 | 3.09 → 3.11 (+0.1 %; floor +0.1 %) | 11 → 11.8 (+5.4 %; floor +5.8 %) | 42 → 41.6 (-0.5 %; floor -2.8 %) | 100 % → 100 % (+0.0 %; floor +0.0 %) | 0.00 → 0.00 (+8.1 %; floor +0.2 %) | 3.56 % → 3.70 % (+2.2 %; floor +5.7 %) |

aarch64 (at 1.5 million records/s only the blocking reader ran; † the runs of 1,024 B at 200,000 records/s were stalled
by the storage in about half the rounds of every variant, with p99 of 0.1–0.3 s and up to 35 % late sends, so those
cells compare nothing; for the blocking reader that includes p50 and CPU, whose medians and paired ratios disagree):

| Wait | Payload | Records/s | p50 | p99 | p99.9 | CPU of a core | Blocks per record | Late sends |
|---|---|---|---|---|---|---|---|---|
| block | 64 B | 20,000 | 5.92 → 4.45 (-24.7 %; floor +0.5 %) | 7.1 → 5.81 (-20.1 %; floor +2.3 %) | 10.7 → 9.56 (-9.8 %; floor +2.9 %) | 11 % → 8 % (-26.4 %; floor +1.9 %) | 1.00 → 1.00 (-0.0 %; floor +0.0 %) | 0.02 % → 0.02 % (+3.6 %; floor +85.9 %) |
| spin | 64 B | 20,000 | 2.49 → 1.03 (-58.4 %; floor +0.0 %) | 2.9 → 1.48 (-49.2 %; floor +0.1 %) | 6.62 → 5.1 (-25.4 %; floor -1.8 %) | 100 % → 100 % (+0.0 %; floor +0.0 %) | 0.00 → 0.00 | 0.01 % → 0.01 % (-11.1 %; floor -16.6 %) |
| block | 1,024 B | 20,000 | 6.11 → 4.78 (-23.3 %; floor +0.4 %) | 8.3 → 7.14 (-13.8 %; floor +0.5 %) | 15.7 → 15 (-5.5 %; floor +1.8 %) | 11 % → 8 % (-26.2 %; floor +0.4 %) | 1.00 → 1.00 (-0.0 %; floor -0.0 %) | 0.02 % → 0.02 % (-4.1 %; floor -10.8 %) |
| spin | 1,024 B | 20,000 | 2.72 → 1.27 (-53.4 %; floor +0.2 %) | 5.23 → 3.95 (-24.0 %; floor -0.5 %) | 12.7 → 11.4 (-10.3 %; floor +2.6 %) | 100 % → 100 % (+0.0 %; floor -0.0 %) | 0.00 → 0.00 (+0.0 %; floor +0.0 %) | 0.03 % → 0.03 % (+14.6 %; floor +5.1 %) |
| block | 64 B | 200,000 | 3.63 → 3.61 (-0.4 %; floor -1.9 %) | 7.05 → 7.27 (+5.6 %; floor -3.3 %) | 10.5 → 11.5 (+2.3 %; floor +0.9 %) | 79 % → 79 % (-0.4 %; floor +0.3 %) | 0.43 → 0.45 (+2.5 %; floor +0.4 %) | 0.15 % → 0.33 % (-1.9 %; floor +5.2 %) |
| spin | 64 B | 200,000 | 2.48 → 1.03 (-58.3 %; floor +0.0 %) | 4.35 → 3.34 (-31.2 %; floor -4.4 %) | 7.56 → 5.98 (-22.3 %; floor -0.9 %) | 100 % → 100 % (-0.0 %; floor -0.0 %) | 0.00 → 0.00 (+0.0 %; floor +0.0 %) | 0.11 % → 0.51 % (+144.9 %; floor +0.7 %) |
| block | 1,024 B | 200,000 | † | † | † | † | 0.37 → 0.34 (-3.9 %; floor -2.2 %) | † |
| spin | 1,024 B | 200,000 | 2.73 → 1.28 (-53.3 %; floor +0.3 %) | † | † | 100 % → 100 % (-0.0 %; floor -0.0 %) | 0.00 → 0.00 (+16.7 %; floor -7.1 %) | † |
| block | 64 B | 1,500,000 | 3.66 → 3.67 (+0.1 %; floor -0.1 %) | 7.62 → 7.52 (-1.2 %; floor -1.9 %) | 40.6 → 40.7 (+1.1 %; floor -1.4 %) | 99 % → 99 % (-0.0 %; floor +0.0 %) | 0.00 → 0.00 (-0.1 %; floor -1.1 %) | 1.47 % → 1.58 % (+10.0 %; floor +2.0 %) |

On aarch64 the late sends of the spinning reader at 200,000 records/s of 64 B are bimodal in all three variants (runs at
0.05–0.2 % and at 0.4–0.9 %; rounds above 0.3 %: 2 of base, 5 of the second copy, 6 of new), which the paired ratio
does not handle.

### System calls per record (one run of each)

x86-64:

| Wait | Payload | Records/s | Bytes read per record | Reads larger than 4 bytes per record | Reader futex waits per record | Writer futex wakes per record |
|---|---|---|---|---|---|---|
| block | 64 B | 20,000 | 67,464 → 4,446 | 1.02 → 1.01 | 1.02 → 1.00 | 1.0069 → 1.0135 |
| spin | 64 B | 20,000 | 67,106 → 4,942 | 1.01 → 1.02 | 0.00 → 0.00 | 0.0017 → 0.0018 |
| block | 1,024 B | 20,000 | 67,063 → 4,574 | 1.02 → 1.02 | 1.01 → 1.02 | 1.0360 → 1.0212 |
| spin | 1,024 B | 20,000 | 67,805 → 5,034 | 1.02 → 1.03 | 0.00 → 0.00 | 0.0173 → 0.0173 |
| block | 64 B | 200,000 | 66,430 → 63,346 | 1.00 → 0.98 | 0.50 → 0.50 | 0.5101 → 0.5288 |
| spin | 64 B | 200,000 | 66,497 → 9,406 | 1.01 → 1.01 | 0.00 → 0.00 | 0.0017 → 0.0017 |
| block | 1,024 B | 200,000 | 54,501 → 59,810 | 0.82 → 0.91 | 0.38 → 0.45 | 0.4417 → 0.4958 |
| spin | 1,024 B | 200,000 | 66,845 → 11,075 | 1.01 → 1.01 | 0.00 → 0.00 | 0.0172 → 0.0174 |
| block | 64 B | 1,500,000 | 14,561 → 14,555 | 0.22 → 0.22 | 0.00 → 0.00 | 0.0023 → 0.0021 |
| spin | 64 B | 1,500,000 | 14,572 → 14,777 | 0.22 → 0.22 | 0.00 → 0.00 | 0.0017 → 0.0017 |

aarch64:

| Wait | Payload | Records/s | Bytes read per record | Reads larger than 4 bytes per record | Reader futex waits per record | Writer futex wakes per record |
|---|---|---|---|---|---|---|
| block | 64 B | 20,000 | 66,677 → 4,499 | 1.00 → 1.02 | 1.00 → 1.02 | 1.0151 → 1.0065 |
| spin | 64 B | 20,000 | 67,453 → 4,770 | 1.02 → 1.02 | 0.00 → 0.00 | 0.0017 → 0.0017 |
| block | 1,024 B | 20,000 | 67,538 → 4,430 | 1.02 → 1.01 | 1.01 → 1.00 | 1.0217 → 1.0272 |
| spin | 1,024 B | 20,000 | 67,356 → 4,744 | 1.01 → 1.01 | 0.00 → 0.00 | 0.0173 → 0.0174 |
| block | 64 B | 200,000 | 66,507 → 64,936 | 1.01 → 1.00 | 0.64 → 0.60 | 0.7984 → 0.7496 |
| spin | 64 B | 200,000 | 67,484 → 5,519 | 1.02 → 1.02 | 0.00 → 0.00 | 0.0017 → 0.0017 |
| block | 1,024 B | 200,000 | 65,458 → 63,917 | 0.99 → 0.99 | 0.56 → 0.56 | 0.7393 → 0.7519 |
| spin | 1,024 B | 200,000 | 66,676 → 11,739 | 1.01 → 0.98 | 0.00 → 0.00 | 0.0172 → 0.0169 |
| block | 64 B | 1,500,000 | 12,484 → 12,412 | 0.19 → 0.19 | 0.00 → 0.00 | 0.0021 → 0.0023 |

### What it shows

- At 1.5 million records/s and on replay, no difference beyond the floor: the reader never reads less there (0.22 and
  0.19 reads per record, as before).
- Spinning readers at 20,000 and 200,000 records/s read 4.4–11.7 KB per record instead of 66–68 KB, with p50 halved
  or better (x86-64 0.76 instead of 2.05–2.07 µs at 64 B) and lower p99. Blocking readers at 20,000 records/s, in this
  version without the rule for readers that block (the merged code gives them whole chunks and no gain): p50 and
  CPU 11–14 % lower on x86-64, 23–26 % lower on aarch64, the same number of waits and wakes.
- Blocking readers at 200,000 records/s, where a reader that copies 64 KiB per record finds about half the records
  without waiting: on aarch64 the 64 B cell does not change (p50 3.63 → 3.61 µs; p99 +5.6 % against a floor of
  −3.3 %, with overlapping per-round ranges); the 1,024 B cell could not be compared (storage stalls). On x86-64 the
  reader reaches small reads part of the time: p50 25–28 % and CPU 12–14 % lower, p99, p99.9 and late sends unchanged,
  and the reader blocked 38–59 % more often, so the writer woke it more often. **The writer paid for it:** its `append`
  p50 in the 1,024 B cell went from 0.96 to 1.05 µs (+8.9 % in the median of the per-round ratios, slower in all 10
  rounds, +5.5 to +16.7 %, against −4.5 to +6.6 % for the second copy of the base), p90 +4.2 %; in the 64 B cell +0.3 %
  (noise). One wake-up costs the writer about 1.3 µs: its `append` p50 is 1.43 µs beside a blocking reader at 20,000
  records/s and 0.108 µs beside a spinning one. The late sends, measured against 1 µs, did not show it. The counting
  runs of the same cells, a few seconds later in their runs, were on whole chunks almost throughout (0.02 and 0.001 small
  reads per record) with waits of 0.50 → 0.50 and 0.38 → 0.45 per record. What decides when a run reaches small reads
  there was not measured. This is why the merged change reads whole chunks for readers that block (next section).

### Readers that block: the rule added after review (x86-64)

The merged change reads whole chunks for every record the reader blocked for (registered as a waiter, so that the writer
made a wake-up call): blocking readers always read whole chunks, and only readers that do not block read less (ADR 0052,
decided after the measurements above). This session measured it beside the alternative that was measured and **not
built**: a reader that blocked for a record counts it towards small reads if it waited at the slot for at least 20 µs.

`c4-standard-4-lssd` (us-east1-b, Xeon 6985P-C, 2 cores with 2 threads each), 2026-11-26 05:43–07:21 UTC, otherwise as
in the environment above. Commits: base `43deb9f3` (`main` then); the 20 µs threshold `51ad6777` (on the branch, not
merged as code); the rule that merged, measured as `823d4650` (a temporary commit, not kept): `51ad6777` with the 20 µs
replaced by no limit, so that a blocked fill never counts. The merged code differs from that only as stated after the
tables. Four variants per round in a rotating order (base, a second copy of base as the floor, threshold, merged rule),
10 rounds, `--warmup 3s --duration 9s` (1.5 million records/s: `--warmup 4s --duration 20s`), otherwise the
`tt-sustained run` command above; then one counting run per variant and cell, as above. The writer's `append` latency is
that of `result.json` (time of the `append` call). In brackets: the median of the per-round ratios, and the same for the
second copy of the base.

**The rule that merged** (base → merged rule):

| Wait | Payload | Records/s | p50 (µs) | p99 (µs) | Reader CPU | Blocks per record | Writer `append` p50 (µs) | Writer `append` p90 (µs) |
|---|---|---|---|---|---|---|---|---|
| block | 64 B | 20,000 | 14.1 → 14.2 (+0.6 %; floor +0.1 %) | 16.7 → 16.8 (-0.1 %; floor +15.0 %) | 23 % → 22 % (-0.1 %; floor +1.7 %) | 1.00 → 1.00 (-0.0 %; floor +0.0 %) | 1.5 → 1.52 (+1.3 %; floor +0.9 %) | 1.67 → 1.72 (+1.3 %; floor +1.1 %) |
| block | 1,024 B | 20,000 | 14.8 → 14.8 (+0.4 %; floor +0.1 %) | 19.6 → 21.6 (+10.1 %; floor +4.5 %) | 23 % → 23 % (+0.1 %; floor -0.9 %) | 1.00 → 1.00 (+0.0 %; floor +0.0 %) | 2.03 → 2.02 (-0.3 %; floor +0.4 %) | 2.35 → 2.36 (+0.6 %; floor +0.9 %) |
| block | 64 B | 50,000 | 10.4 → 10.5 (+1.5 %; floor -0.2 %) | 23 → 22 (-4.5 %; floor +3.1 %) | 51 % → 51 % (+1.0 %; floor -0.2 %) | 0.92 → 0.91 (-1.5 %; floor -0.1 %) | 1.28 → 1.24 (-3.2 %; floor -0.8 %) | 1.8 → 1.78 (-0.5 %; floor -1.1 %) |
| block | 1,024 B | 50,000 | 9.7 → 9.85 (+1.6 %; floor -0.2 %) | 23.7 → 24.3 (+2.0 %; floor +8.6 %) | 50 % → 51 % (+0.6 %; floor +0.5 %) | 0.92 → 0.91 (-0.9 %; floor +0.3 %) | 1.67 → 1.62 (-2.5 %; floor +0.1 %) | 2.35 → 2.39 (+0.6 %; floor +0.0 %) |
| block | 64 B | 200,000 | 4.06 → 4.06 (+0.3 %; floor +0.0 %) | 7.84 → 7.74 (-0.7 %; floor +3.6 %) | 71 % → 72 % (+1.4 %; floor +0.4 %) | 0.51 → 0.50 (-2.1 %; floor -0.2 %) | 0.772 → 0.76 (-0.9 %; floor -1.5 %) | 1.03 → 1.02 (-1.4 %; floor -1.5 %) |
| block | 1,024 B | 200,000 | 4.44 → 4.52 (+1.6 %; floor +0.1 %) | 9.84 → 10.2 (+1.6 %; floor +3.9 %) | 73 % → 74 % (-0.2 %; floor -0.0 %) | 0.49 → 0.48 (-1.0 %; floor -0.2 %) | 0.968 → 0.96 (-0.8 %; floor -0.6 %) | 1.38 → 1.37 (-1.2 %; floor +0.4 %) |
| block | 64 B | 1,500,000 | 3.17 → 3.15 (-0.7 %; floor -0.4 %) | 14.2 → 13.5 (-6.2 %; floor -2.3 %) | 99 % → 99 % (+0.0 %; floor +0.0 %) | 0.00 → 0.00 (-10.4 %; floor -7.3 %) | 0.115 → 0.114 (+0.0 %; floor +0.0 %) | 0.133 → 0.133 (-0.4 %; floor +0.0 %) |
| spin | 64 B | 20,000 | 2.11 → 0.778 (-63.0 %; floor -0.1 %) | 2.89 → 1.53 (-45.4 %; floor -6.4 %) | 100 % → 100 % (-0.0 %; floor -0.0 %) | 0 | 0.117 → 0.117 (-0.4 %; floor +0.0 %) | 0.173 → 0.17 (-3.0 %; floor -3.6 %) |
| spin | 64 B | 200,000 | 2.06 → 0.776 (-62.5 %; floor -0.0 %) | 4.61 → 3.63 (-22.8 %; floor -2.9 %) | 100 % → 100 % (+0.0 %; floor -0.0 %) | 0.00 → 0.00 (+0.0 %; floor +0.0 %) | 0.116 → 0.115 (-0.9 %; floor -0.4 %) | 0.145 → 0.14 (-2.7 %; floor -0.7 %) |

**The 20 µs threshold, measured and not built** (base → threshold):

| Wait | Payload | Records/s | p50 (µs) | p99 (µs) | Reader CPU | Blocks per record | Writer `append` p50 (µs) | Writer `append` p90 (µs) |
|---|---|---|---|---|---|---|---|---|
| block | 64 B | 20,000 | 14.1 → 12.4 (-12.4 %; floor +0.1 %) | 16.7 → 16.3 (-2.4 %; floor +15.0 %) | 23 % → 20 % (-12.8 %; floor +1.7 %) | 1.00 → 1.00 (+0.0 %; floor +0.0 %) | 1.5 → 1.54 (+2.5 %; floor +0.9 %) | 1.67 → 1.74 (+3.5 %; floor +1.1 %) |
| block | 1,024 B | 20,000 | 14.8 → 13.2 (-11.0 %; floor +0.1 %) | 19.6 → 19.5 (-7.0 %; floor +4.5 %) | 23 % → 20 % (-11.1 %; floor -0.9 %) | 1.00 → 1.00 (+0.0 %; floor +0.0 %) | 2.03 → 2.08 (+2.8 %; floor +0.4 %) | 2.35 → 2.39 (+2.0 %; floor +0.9 %) |
| block | 64 B | 50,000 | 10.4 → 10.4 (+1.6 %; floor -0.2 %) | 23 → 24 (+2.6 %; floor +3.1 %) | 51 % → 51 % (+0.1 %; floor -0.2 %) | 0.92 → 0.92 (-1.2 %; floor -0.1 %) | 1.28 → 1.23 (-3.8 %; floor -0.8 %) | 1.8 → 1.77 (-0.2 %; floor -1.1 %) |
| block | 1,024 B | 50,000 | 9.7 → 9.84 (+1.9 %; floor -0.2 %) | 23.7 → 24.8 (+4.5 %; floor +8.6 %) | 50 % → 51 % (+1.0 %; floor +0.5 %) | 0.92 → 0.91 (-0.5 %; floor +0.3 %) | 1.67 → 1.63 (-2.0 %; floor +0.1 %) | 2.35 → 2.39 (+1.2 %; floor +0.0 %) |
| block | 64 B | 200,000 | 4.06 → 4.06 (+0.1 %; floor +0.0 %) | 7.84 → 7.86 (-0.1 %; floor +3.6 %) | 71 % → 72 % (+1.1 %; floor +0.4 %) | 0.51 → 0.50 (-0.7 %; floor -0.2 %) | 0.772 → 0.772 (+1.7 %; floor -1.5 %) | 1.03 → 1.03 (-2.0 %; floor -1.5 %) |
| block | 1,024 B | 200,000 | 4.44 → 4.49 (+0.7 %; floor +0.1 %) | 9.84 → 10.2 (+0.6 %; floor +3.9 %) | 73 % → 74 % (+0.4 %; floor -0.0 %) | 0.49 → 0.48 (-0.7 %; floor -0.2 %) | 0.968 → 0.97 (-0.5 %; floor -0.6 %) | 1.38 → 1.37 (-1.0 %; floor +0.4 %) |
| block | 64 B | 1,500,000 | 3.17 → 3.16 (-0.3 %; floor -0.4 %) | 14.2 → 14.4 (+0.0 %; floor -2.3 %) | 99 % → 99 % (+0.0 %; floor +0.0 %) | 0.00 → 0.00 (-9.6 %; floor -7.3 %) | 0.115 → 0.114 (-0.4 %; floor +0.0 %) | 0.133 → 0.133 (+0.0 %; floor +0.0 %) |
| spin | 64 B | 20,000 | 2.11 → 0.781 (-62.9 %; floor -0.1 %) | 2.89 → 1.45 (-47.2 %; floor -6.4 %) | 100 % → 100 % (-0.0 %; floor -0.0 %) | 0 | 0.117 → 0.117 (-0.4 %; floor +0.0 %) | 0.173 → 0.169 (-3.2 %; floor -3.6 %) |
| spin | 64 B | 200,000 | 2.06 → 0.772 (-62.6 %; floor -0.0 %) | 4.61 → 3.75 (-23.7 %; floor -2.9 %) | 100 % → 100 % (+0.0 %; floor -0.0 %) | 0.00 → 0.00 (+0.0 %; floor +0.0 %) | 0.116 → 0.116 (+0.0 %; floor -0.4 %) | 0.145 → 0.143 (+2.0 %; floor -0.7 %) |

Bytes read per record (one counting run each): with the merged rule 51,000–67,000 in every blocking cell, as the base
(no small reads); with the threshold, at 20,000 records/s 67,390 → 4,524 (64 B) and 67,019 → 4,762 (1,024 B). Reader waits
and writer futex wakes per record at 50,000 and 200,000 records/s: 0.39–0.91 and 0.46–0.91 in all three variants, with
run-to-run differences of up to 0.11 in the single counting runs (1,024 B at 200,000: wakes 0.459 base, 0.525 threshold,
0.570 merged rule, which reads only whole chunks there, as the base does).

- **The merged rule** matches the base in every blocking cell: blocks per record −2.1 to +0.0 % against a floor of −0.2
  to +0.3 %; the writer's `append` p50 −3.2 to +1.3 % and p90 −1.4 to +1.3 % (20,000 records/s: +1.3 % and −0.3 %, p90
  +1.3 % and +0.6 %, against a floor of +0.9 % and +0.4 %); p99 at 1.5 million records/s −6.2 % against a floor of
  −2.3 %. Two medians lie above their floors with overlapping per-round ranges: p50 +1.5 % and +1.6 % at 50,000
  records/s (floor −0.2 %; ranges −1.2 to +7.9 % and −3.0 to +9.6 %), and p99 +10.1 % at 20,000 records/s of 1,024 B
  (floor +4.5 %; ranges −15.5 to +18.7 % and −3.2 to +18.9 %). Spinning readers: p50 63 % lower at 20,000 and 200,000
  records/s.
- **The threshold** matched the base at 50,000 and 200,000 records/s too, and at 20,000 records/s gave the blocking reader
  a p50 11–12 % and CPU 11–13 % lower, with 4.5–4.8 KB read per record instead of 67 KB and the same number of blocks.
  **But the writer's `append` p50 there was 2.5 % (64 B, slower in 9 of 10 rounds, −1.4 to +10.2 %) and 2.8 % (1,024 B,
  6 of 10) higher, p90 3.5 % and 2.0 %**, against a floor of +0.9 % and +0.4 %. The first measurement showed the same at
  64 B (+1.6 %, slower in all 10 rounds, floor +0.3 %). That is about 40 ns per `append`, with the same number of wake-up
  calls; why was not measured. That is why it was not built.
- In one round of the 50,000 records/s cells (round 6), every variant at 64 B, and the threshold's run at 1,024 B, ran
  with a p50 of 5.4–7.1 µs instead of about 10 µs; the other nine rounds agree, so this is the machine at that time.

**How the merged code differs from the measured rule.** The measured build kept the threshold's code with no limit: it
stored when the reader first found a slot empty and read the clock at each fill of a record it had blocked for. The merged
code has neither (the stored state is 16 bytes smaller, inside the boxed wait state). It also counts as blocked a record
that the reader found in `block_on`'s own check after registering as a waiter, which the measured build did not see; in
the measured blocking cells that case could only turn a fill that the build counted as following into one that does not,
and those cells had no small reads anyway. And a fill of a reader that did not block now also checks whether the wake-up
region is taken out of the waker (one load), which the spinning cells above did not run. All three are on the paths the
cells exercised (blocking and spinning fills and empty polls); none was measured.

### Earlier versions of the change, on the same kind of x86-64 machine

Three sessions on x86-64 preceded the aarch64 one, each on a new VM of the same type and zone, with the commands above
(the first used 10 rounds of the default replay measurement, then 60 short rounds).

- `e12a0022`, without the probe (2026-11-25 20:31–22:52 UTC): blocking readers at 200,000 records/s blocked 85 % (64 B)
  and 66 % (1,024 B) more often (second copy −0.2 %), p50 27 % and 22 % lower, CPU 22 % and 13 % lower; its counting
  runs were on whole chunks (0.014 and 0.009 small reads per record). Small reads that had made such a reader fast
  enough to wait for every record kept themselves in place.
- `81012b2c`, a probe in every 128 fills and the rule "used at most 4 KiB" for a fill that follows (2026-11-25
  22:53–00:52 UTC): blocking at 200,000 records/s 19 % (64 B) and 53 % (1,024 B) more blocks; the spinning reader at
  20,000 records/s of 64 B had p99 2.49 → 3.11 µs (+21.7 %, slower in 8 of 10 rounds; floor −1.1 %), the probes' 0.8 %
  of records at the latency of a whole read. Replay (60 short rounds) within the floor.
- `c7bec958` (the tables above) counts a fill as following only if the last fill brought exactly one record, and probes
  once in 256 fills.

## 2026-11-26, durable-only readers wake on checkpoints only (ADR 0053, issue #82; Google Cloud VMs)

The acceptance measurement of ADR 0053 against 1.0.0 and the `main` it fixes. All of it ran on **Google Cloud virtual
machines, not bare metal**, on one day, and is not compared with other sections.

### Environment and commands

Ubuntu 24.04, kernel 7.0.0-1017-gcp (`CONFIG_PREEMPT_LAZY=y` without `PREEMPT_DYNAMIC`, `CONFIG_HZ=1000`), rustc
1.100.0, journals on ext4 on the first local NVMe SSD (`mkfs.ext4 -E lazy_itable_init=0,lazy_journal_init=0`, `noatime`),
results on the boot disk. `c4-standard-16-lssd` (Xeon 6985P-C, 16 vCPUs) and `c4a-standard-8-lssd` (Neoverse-V2, 8
vCPUs) in us-east1-d, each shape on separate VMs as marked.

Builds, all at the same path: **1.0.0** is the 1.0.0 library under the harness of `0fcdb9e5` (whose library is the tag's),
**1.0.0 copy** the same binary copied (the noise floor), **`main`** is `f1ea454d`, **new** is this change
(`core/marker-wake` at `d08ad0a1`, whose library code is that of this change as merged). Interleaved in rotating order, 10 rounds per VM (15 on the third x86-64 VM):

```sh
tt-sustained run --dir … --out … --json --payload 1024 --duration 15s --warmup 3s --cpus 1,2,3,4,5,6 \
  --rate <rate> --checkpoint-every 1s --checkpoint-mode background --retain-bytes 68719476736 \
  --readers mapped,safe:wait=default,safe:durable:wait=block      # without the last reader for "no durable-only reader"
```

Rates: 169,206/s (aarch64) and 173,119/s (x86-64), the published half rates of the sustained session, and 100,000/s.
Latencies in µs, medians of the rounds with their range; coordinated omission is handled as in the sustained
session (intended send times).

### Append latency with a blocking durable-only reader

| Machine, rate | Build | p50 | p99 | p99.9 | Durable-only reader: CPU, voluntary context switches | Writer system time (s) |
|---|---|---|---|---|---|---|
| x86-64, VM 1, 173,119/s | 1.0.0 | 0.56 (0.54–0.57) | 2.23 (2.05–2.29) | 6.25 (6.03–6.40) | 0.99, 3260 (2935–3390) | 0.46 (0.37–0.48) |
| x86-64, VM 1, 173,119/s | 1.0.0 copy | 0.56 (0.55–0.57) | 2.23 (2.01–2.31) | 6.28 (5.96–6.42) | 0.99, 3226 (2794–3387) | 0.46 (0.40–0.50) |
| x86-64, VM 1, 173,119/s | `main` | 1.18 (1.15–1.19) | 3.80 (3.62–3.86) | 7.79 (6.89–8.05) | 0.68 (0.67–0.69), 2741304 (2598737–2783731) | 2.37 (2.18–2.40) |
| x86-64, VM 1, 173,119/s | new | 0.44 (0.42–0.45) | 2.04 (1.80–2.19) | 5.96 (3.98–7.10) | 0.04, 2516 (2513–2521) | 0.11 (0.07–0.15) |
| x86-64, VM 1, 100,000/s | 1.0.0 | 0.62 (0.61–0.64) | 2.97 (2.54–5.85) | 9.86 (7.99–11.7) | 0.95 (0.94–0.96), 318554 (245868–349744) | 0.05 (0.04–1.69) |
| x86-64, VM 1, 100,000/s | 1.0.0 copy | 0.62 (0.60–0.69) | 3.13 (2.29–3.54) | 10.1 (8.47–11.2) | 0.94 (0.93–0.95), 323020 (250585–404956) | 0.17 (0.04–0.49) |
| x86-64, VM 1, 100,000/s | `main` | 1.16 (1.13–1.18) | 3.79 (3.75–6.32) | 11.4 (10.7–12.1) | 0.49 (0.47–0.50), 1530676 (1504705–1605423) | 0.81 (0.04–14.9) |
| x86-64, VM 1, 100,000/s | new | 0.44 (0.43–0.46) | 2.12 (2.05–2.15) | 8.65 (7.85–10.9) | 0.02 (0.02–0.03), 2510 (2501–2522) | 0.06 (0.04–0.71) |
| aarch64, VM 1, 169,206/s | 1.0.0 | 0.51 (0.51–0.52) | 2.64 (2.56–2.67) | 7.19 (6.82–7.43) | 0.99, 4320 (4265–4366) | 0.67 (0.63–0.70) |
| aarch64, VM 1, 169,206/s | 1.0.0 copy | 0.51 (0.51–0.52) | 2.64 (2.58–2.69) | 7.20 (6.96–7.46) | 0.99, 4323 (4267–4392) | 0.68 (0.64–0.74) |
| aarch64, VM 1, 169,206/s | `main` | 0.73 (0.71–0.76) | 3.89 (3.84–3.93) | 8.88 (8.50–9.54) | 0.83 (0.83–0.84), 980648 (945046–1072035) | 2.46 (1.95–3.38) |
| aarch64, VM 1, 169,206/s | new | 0.30 | 2.31 (2.27–2.37) | 6.91 (6.63–7.32) | 0.04, 2517 (2506–2528) | 0.21 (0.20–0.22) |
| aarch64, VM 1, 100,000/s | 1.0.0 | 0.60 (0.59–0.61) | 2.80 (2.74–2.93) | 8.46 (7.64–9.97) | 0.99, 7919 (5590–10169) | 0.04 (0.03–0.15) |
| aarch64, VM 1, 100,000/s | 1.0.0 copy | 0.60 (0.59–0.61) | 2.79 (2.76–2.84) | 8.74 (7.73–10.2) | 0.99, 7586 (6111–9249) | 0.09 (0.04–0.95) |
| aarch64, VM 1, 100,000/s | `main` | 2.01 (1.98–2.04) | 4.11 (4.05–4.47) | 10.5 (8.94–11.5) | 0.61 (0.60–0.63), 1715633 (1699216–1728304) | 0.05 (0.04–17.6) |
| aarch64, VM 1, 100,000/s | new | 0.30 (0.30–0.32) | 2.39 (2.33–2.48) | 7.72 (6.73–9.16) | 0.03, 2513 (2495–2522) | 0.04 (0.02–0.93) |

`main` slows appends by 0.2–1.4 µs at p50 against 1.0.0 in every round; the change makes them faster than 1.0.0 in every
round (p50 0.12–0.29 µs lower), and the durable-only reader spends 2–4 % of a core instead of 99 %, with one voluntary
context switch per checkpoint plus its 100 ms timeouts (about 2,500 in 18 s) instead of one or more per append.

### Without a durable-only reader (must be unchanged)

Readers `mapped,safe:wait=default`, the same VMs and rounds:

| Machine, rate | Build | p50 | p99 | p99.9 | max |
|---|---|---|---|---|---|
| x86-64, VM 1, 173,119/s | 1.0.0 | 0.43 (0.42–0.43) | 2.02 (1.89–2.13) | 5.97 (5.71–7.05) | 151 (135–185) |
| x86-64, VM 1, 173,119/s | 1.0.0 copy | 0.43 (0.41–0.43) | 2.05 (1.96–2.15) | 5.99 (5.05–7.01) | 171 (140–216) |
| x86-64, VM 1, 173,119/s | new | 0.44 (0.41–0.45) | 2.01 (1.78–2.08) | 5.94 (3.81–6.05) | 157 (149–484) |
| x86-64, VM 1, 100,000/s | 1.0.0 | 0.42 (0.41–0.43) | 2.14 (2.09–3.31) | 8.25 (7.04–10.1) | 172 (145–273) |
| x86-64, VM 1, 100,000/s | 1.0.0 copy | 0.42 (0.41–0.43) | 2.11 (1.88–2.23) | 7.98 (6.73–9.83) | 246 (147–393) |
| x86-64, VM 1, 100,000/s | new | 0.43 (0.41–0.44) | 2.13 (2.08–2.14) | 7.92 (7.07–10.0) | 214 (145–450) |
| aarch64, VM 1, 169,206/s | 1.0.0 | 0.30 | 2.36 (2.31–2.42) | 6.90 (6.74–7.11) | 184 (174–195) |
| aarch64, VM 1, 169,206/s | 1.0.0 copy | 0.30 (0.29–0.30) | 2.36 (2.29–2.41) | 6.88 (6.66–7.32) | 178 (168–195) |
| aarch64, VM 1, 169,206/s | new | 0.30 (0.29–0.30) | 2.29 (2.26–2.33) | 6.88 (6.50–7.06) | 183 (175–192) |
| aarch64, VM 1, 100,000/s | 1.0.0 | 0.30 (0.30–0.31) | 2.45 (2.40–2.73) | 7.85 (7.32–10.8) | 183 (175–247) |
| aarch64, VM 1, 100,000/s | 1.0.0 copy | 0.30 (0.30–0.31) | 2.43 (2.37–2.91) | 8.11 (6.94–9.41) | 181 (175–257) |
| aarch64, VM 1, 100,000/s | new | 0.30 (0.29–0.31) | 2.40 (2.37–2.46) | 8.04 (6.83–8.84) | 185 (176–250) |

### The maximum: runs (of 10, 15 on the third x86-64 VM) with an append of 200 µs or more

Rule (ADR 0053): no more runs for the new build than for 1.0.0 plus the difference between the two 1.0.0 copies, on each
VM.

| VM | Runs | 1.0.0 | 1.0.0 copy | `main` | new | max median: 1.0.0 / copy / new | Rule met |
|---|---|---|---|---|---|---|---|
| x86-64, VM 1 | 10 | 0 | 0 | 0 | 2 | 151 / 149 / 162 | **no** |
| x86-64, VM 2 | 10 | 2 | 1 | 1 | 2 | 155 / 155 / 165 | yes |
| x86-64, VM 3 | 15 | 5 | 2 | 2 | 2 | 156 / 152 / 153 | yes |
| aarch64, VM 1 | 10 | 0 | 1 | 0 | 0 | 183 / 186 / 182 | yes |
| aarch64, VM 2 | 10 | 1 | 1 | 0 | 0 | 189 / 190 / 186 | yes |

Not met on the first x86-64 VM (2 runs against 0 and 0), met on the other four. Pooled over the three x86-64 VMs: 6 of 35
runs for the new build, 7 and 3 of 35 for the two 1.0.0 copies, 3 of 35 for `main`. On the first VM the cell without a
durable-only reader, where the change runs no different code on the writer's side, had 3 runs for the new build against
0 and 2; the slow non-roll appends fall at the start of a checkpoint in every build (ADR 0053).

### Delivery to the durable-only reader

From "checkpoint complete" (the checkpoint thread starts its wake-ups, once the marker is durable) to the reader
returning the first record the new marker covers, and the last; five rounds, logged by an instrumented build (one line
per checkpoint on each side; append latencies of these builds shown for reference):

| Machine | Helper threads | Build | First record after "checkpoint complete": median of the runs' medians (largest), µs | Last record (ms) | Append p50 |
|---|---|---|---|---|---|
| x86-64, VM 1 | any (default) | 1.0.0 | before the wake-ups (polls the marker: −111 µs) | 41.2 (40.5–41.8) | 0.56 (0.55–0.56) |
| x86-64, VM 1 | any (default) | new | 145 (1200) | 41.5 (40.5–41.7) | 0.42 (0.41–0.43) |
| x86-64, VM 1 | CPU 7 | 1.0.0 | before the wake-ups (polls the marker: −29.4 µs) | 40.8 (40.4–41.4) | 0.56 (0.56–0.57) |
| x86-64, VM 1 | CPU 7 | new | 33.1 (233) | 40.8 (40.3–41.4) | 0.43 (0.42–0.49) |
| aarch64, VM 1 | any (default) | 1.0.0 | before the wake-ups (polls the marker: −10.4 µs) | 41.6 (41.3–41.7) | 0.52 |
| aarch64, VM 1 | any (default) | new | 14.7 (1455) | 43.4 (42.8–44.4) | 0.30 (0.29–0.30) |
| aarch64, VM 1 | CPU 7 | 1.0.0 | before the wake-ups (polls the marker: −10.4 µs) | 41.5 (41.3–41.8) | 0.52 |
| aarch64, VM 1 | CPU 7 | new | 10.7 (17.6) | 42.1 (41.7–43.3) | 0.30 |

1.0.0's reader polls the marker on a full core, so it starts reading as soon as the marker is written. The new reader
is woken: with the writer's helper threads free to run on its CPU it starts 15–145 µs later at the median and up to
1.2–1.5 ms later in the worst checkpoint, and the last record of the 1-second backlog arrives 0.3 ms (x86-64) and 1.8 ms
(aarch64) later; with the helpers on CPU 7, 11–33 µs (at most 0.23 ms) and 0.0–0.6 ms. ADR 0053 has the mechanism.

### Mixed versions (`wakemix`, one process per role, x86-64 VM 1 and aarch64 VM 1)

Writer appends 500,000 records of 1,024 B at 100,000/s with a background checkpoint every second; one durable-only
reader that blocks at once. Writer append p50, and the reader's voluntary context switches:

| Writer, durable-only reader | x86-64: writer p50 (µs), reader switches | aarch64: writer p50 (µs), reader switches |
|---|---|---|
| 1.0.0, 1.0.0 | 0.45, 141,059 | 0.59, 13,936 |
| 1.0.0, new | 0.14, 5,257 | 0.27, 5,306 |
| new, 1.0.0 | 0.45, 147,208 | 0.62, 15,037 |
| new, new | 0.14, 61 | 0.27, 60 |

With one record per checkpoint (60 records, pauses of 23–63 ms), every pairing delivered every record; a new
durable-only reader beside a 1.0.0 writer got them 0.65 ms (x86-64) and 0.63 ms (aarch64) after the append at the
median (checkpoints took 0.29 and 0.05 ms), at most 2.2 and 1.1 ms. Readers of both versions attached while the writer
changed from 1.0.0 to new and back got 120 of 120 records each.

## 2026-11-26, the two aarch64 observations left open by 1.0.0 (Google Cloud VMs)

Two things seen on Google Axion (`c4a`) and recorded without an explanation (issues #60 and #62, roadmap "ARM server
measurements"): the zero-copy reader's maximum of about 10 ms in the checkpoint runs of 2026-11-20, and the platform
mechanism behind the lagging view of issue #43. All of it ran on **Google Cloud virtual machines, not bare metal**, in
one morning. The raw output (results, traces, counters, scripts, the reproducer) is kept outside the repository.

### Environment and commands

Ubuntu 24.04, kernel 7.0.0-1017-gcp on both architectures: `CONFIG_HZ=1000`, `PREEMPT_LAZY`, `NO_HZ_FULL` built but not
enabled (no `nohz_full=` on the command line), `CONFIG_ARCH_WANT_BATCHED_UNMAP_TLB_FLUSH=y` (reclaim and migration batch
their TLB invalidations), `CONFIG_ARM64_CONTPTE=y` on aarch64, multi-generational LRU enabled
(`/sys/kernel/mm/lru_gen/enabled` 0x0003), transparent huge pages `madvise`, 4 KiB pages, `vm.compaction_proactiveness`
20, write-back sysctls at their defaults. The guests report no steal time (`/proc/stat` column 8 stayed 0 on every
CPU). Journals on ext4 on the first local NVMe SSD (`mkfs.ext4 -E lazy_itable_init=0,lazy_journal_init=0`, mounted
`noatime`). rustc 1.100.0, bpftrace 0.20.2.

| Machine (us-east1-b) | CPU, vCPUs, memory | Time (UTC) | Used for |
|---|---|---|---|
| `c4a-standard-16-lssd` | Google Axion (Neoverse-V2), 16, 62 GiB | 05:52–06:10 | the 10 ms maximum |
| `c4a-standard-8-lssd` | Axion, 8, 31 GiB | 05:52–07:56 | the lagging view |
| `c4-standard-4-lssd` | Xeon 6985P-C, 4, 14 GiB | 07:14–07:38 | the lagging view on x86-64 |

Commits: `main` at `43deb9f3` with a local patch that is not in the repository: counters in the reader's two re-read
paths (a slot that failed validation: how often its first read failed, how often the back-to-back re-reads cleared it,
how often the 50 ms window was entered and after how long a re-read validated; and the empty-slot confirmation of ADR
0050), printed by `tt-sustained` at the end of each reader; `f194c42`, the commit of the checkpoint finish round, and
`279f59a`, the commit at which issue #43 was found, both built from `git archive`. `tt-lagreader` (local, not in the
repository) is a zero-copy reader that follows a journal from its first record with `Backoff::busy_spin()` and prints
the counters, with the time each one moved; built against `main` and against `279f59a`, the second counts the false
corruption reports that version makes (it reported after three back-to-back re-reads) and carries on.

### The zero-copy reader's 10 ms maximum

What was recorded (2026-11-20, one `c4a-standard-16-lssd` in us-east1-b, `f194c42`): every run of the finish round in
which the writer kept its schedule had a mapped-reader maximum of 10.04–10.17 ms, with and without checkpoints, with
and without early write-back, on ext4 and on xfs (15 runs of 120–190 s, 1 to 11 seconds per run with such a maximum, 53
in all). Read again from the raw series for this section: in those seconds the writer's appends took at most 0.2 ms and
the safe reader's maximum was 0.2 ms; in all 3,322 seconds of those runs neither the writer (busy-waiting for its
schedule on CPU 1) nor the safe reader (CPU 3) had a second with a delay of 9.5–10.5 ms. Only the reader on CPU 2, the
spinning zero-copy reader, had them. The maximum showed up nowhere else: in all other aarch64 sessions of 8 and 16
vCPUs, a second with a 10 ms mapped-reader maximum is rare and inside runs dominated by longer stalls.

Reproduction, with the finish round's command without checkpoints (its visibility-only run), on a new VM of the same
type in the same zone:

```sh
tt-sustained run --dir /data/tt/run --out … --duration 180s --warmup 10s --payload 1024 --rate 250000 \
  --readers mapped,safe:wait=default --cpus 1,2,3,4 --helper-cpus 5,6,7 --json
```

| Run | Commit | Mapped reader max | p99.99 | p99.9 | Seconds with a max ≥ 9.5 ms |
|---|---|---|---|---|---|
| 2026-11-20, visibility-only, ext4 / xfs | `f194c42` | 10.05 ms / 10.15 ms | 4.6 ms / 7.9 ms | 482 / 590 | 3 / 8 of 191 |
| 2026-11-26 | `main` + counters | 911 | 841 | 442 | 0 of 191 |
| 2026-11-26 | `f194c42` | 4.18 ms | 807 | 380 | 0 of 191 |
| 2026-11-26, traced (`reader-pauses.bt`) | `main` + counters | 925 | 852 | 458 | 0 of 191 |

Latencies in µs unless a cell says ms. **Not reproduced:** no 10 ms maximum in 573 s on the new VM, at either commit. At
the rate of 2026-11-20 (3.5 a run) three runs without one would happen by chance less than once in 10,000. Earlier, on 8
vCPUs, the same command at `f194c42` and at `v1.0.0` did not reproduce it either (2026-11-25, roll and journal stalls).
It belonged to that VM (or its host), not to the machine type or the commit.

What the reader's maximum is where the 10 ms is absent (the traced run, `benches/diagnostics/reader-pauses.bt` on the
reader's thread and CPU, events of 300 µs or more): at each roll the reader unmaps the segment it finished, and that
`munmap` of 128 MiB took 0.61–0.76 ms (375 calls, p50 0.64 ms); that is the maximum. The thread was off its CPU 374
times, for at most 64 µs (preempted by kernel threads); no page fault took 300 µs, no interrupt or softirq on its CPU
100 µs, and the CPU's timer never stopped for more than 2.0 ms.

What the 10 ms was not, from the recorded data and the code: the reader's own waiting (a `wait=spin` reader never sleeps
or yields, and at `f194c42` the record path had no sleep at all: the 50 ms re-read window, whose pauses of 10 µs
doubling up to 5 ms add up to about 10 ms by the tenth, came later with #44); the timer tick (`CONFIG_HZ=1000`);
checkpoints, early write-back, the filesystem and the writer (it happened without checkpoints and without early
write-back, on ext4 and xfs, while the writer kept its schedule); helper threads on the reader's CPU (they were on CPUs
5–7). Not ruled out: a pause of vCPU 2 by the hypervisor, which the guest cannot see in its accounting, and
`tt-sustained`'s lag sampler, an unpinned thread of the reader process that wakes every 10 ms (`--lag-interval`), the
same value. **Not explained.** What would settle it if it comes back: `reader-pauses.bt` during the run (a 10 ms `OFF`
line names what ran instead; a 10 ms `GAP` line on the reader's CPU with no `OFF` means the vCPU stopped; neither means
the reader spun without seeing the data), a run with `--lag-interval 25ms` (the maximum moves to 25 ms if the sampler is
the cause), and the zero-copy and safe readers' CPUs swapped.

### The lagging view of issue #43

Command: issue #43's, with memory compaction forced every second and the journal larger than RAM (64 GiB of retention on
31 GiB). In the runs with `tt-lagreader`, `tt-sustained` ran only its copying reader, and the two `tt-lagreader`s
(zero-copy, built against `main` and against `279f59a`) followed the same journal on CPUs 4 and 5, so that one run
measures both reader versions on the same records.

```sh
( while true; do echo 1 | sudo tee /proc/sys/vm/compact_memory > /dev/null; sleep 1; done ) &
tt-lagreader /data/q2/long <s> & tt-lagreader-279f59a /data/q2/long <s> &      # pinned with taskset -c 4, -c 5
tt-sustained run --dir /data/q2/long --out … --unpaced --duration <s>s --payload 1024 --checkpoint-every 1s \
  --retain-bytes 68719476736 --readers safe:wait=default --checkpoint-mode background --cpus 1,2,3,4,5,6 --json
sudo bpftrace benches/diagnostics/frontier-evictions.bt      # the traced runs
```

A "lag event" is a slot whose first read failed validation in a zero-copy reader of `main` (the record's commit word
was visible, its bytes did not validate yet); "window" means the back-to-back re-reads failed too and the reader entered
its 50 ms window. "False reports" are the `279f59a` reader's corruption reports, each one a record the writer had
published correctly. "Removals at the frontier" are page-cache folios of the active segment removed within 128 pages of
the highest page added (the writer is 64 pages behind it), from the trace.

| Run | Writer | Traced | Length | Records | Lag events (window; longest window) | False reports (`279f59a`) | Removals at the frontier | Events within 2 ms of a removal |
|---|---|---|---|---|---|---|---|---|
| aarch64 | `main`, its zero-copy reader in `tt-sustained` | no | 600 s | 157,259,192 | 0 | – | not traced | – |
| aarch64 | `279f59a`, its zero-copy reader in `tt-sustained` | no | 227 s | – | – | 1 (the run stopped) | not traced | – |
| aarch64 | `279f59a` | no | 600 s | 155,543,436 | 1 (0) | 1 | not traced | – |
| aarch64 | `279f59a` | removals | 1,000 s | 245,710,404 | 1 (1; 69 µs) | 1 | 17,713 | 1 of 2 (the false report; the lag event in the same second as a burst) |
| aarch64 | `main` | removals | 1,000 s | 294,238,395 | 5 (4; 69 µs) | 2 (two of the same five records) | 3,749 | 1 of 5 |
| aarch64 | `main` | removals, migrations | 1,000 s | 290,063,082 | 3 (2; 69 µs) | 0 | 4,275 (and 173 migrations) | 1 of 3 |
| aarch64, journal within RAM (8 GiB of retention) | `main` | removals, migrations | 1,000 s | 288,763,696 | 0 | 0 | none by reclaim (kswapd reclaimed no page in the run); 54 migrations | – |
| x86-64 | `main` | removals | 900 s | 298,747,427 | 0 | – | 8,382 | – |

The independent pair (`viewlag.c`, a writer and a reader written without tickertape code: payload words, then a
sequence word, then the commit word with a release store, through `MAP_SHARED` mappings of 128 MiB files; the writer's
helper populates 4 MiB ahead with `MADV_POPULATE_WRITE`, starts write-back of each written MiB and `fdatasync`s the
active file every second; old files are unlinked; the reader checks every payload word after an acquire load of the
commit word, and `pread`s the file when its view disagrees) ran 600 s on the aarch64 VM under the same pressure (64 GiB
of files, compaction forced; 192,607,390 frames; 161 GiB reclaimed by kswapd, 911,158 pages migrated): **no lag, and no
commit word that the file held and the mapping did not**. Like the independent pair of issue #43's investigation, it
does not reproduce the effect: its rate is too low, or tickertape's writer does something to the page cache that it
does not. Its reader also reports a lag of the commit word itself, which a zero-copy reader cannot tell from an empty
slot; none occurred.

What these runs show:

- **The lagging view still happens on `main`, on aarch64, under memory pressure:** 9 lag events in 3,000 s of the three
  traced aarch64 runs with the journal larger than RAM, about one every 5.5 minutes, as at `279f59a` (one false report
  after 227 s, 600 s and 1,000 s). With `main`'s writer the `279f59a` reader made two false reports in 1,000 s: the
  writer's changes since then do not avoid it. That the `main` run with `tt-sustained`'s own zero-copy reader had none
  in 600 s is within chance at this rate.
- **The 50 ms window is far longer than these events needed:** in every event that entered it, the first re-read after
  a pause validated, 63–69 µs after the window began (about what one 10 µs sleep takes with the default 50 µs timer
  slack). None ran out; no
  reader of `main` reported corruption, and the copying readers never entered the window (their counters show only
  ordinary back-to-back re-reads of a record copied while it was written).
- **Some events coincide with reclaim removing the frame's pages under the readers.** At the two false reports whose
  offsets are known and traced, kswapd had removed **both pages of that very frame** from the page cache 251 to 47 µs
  before (`279f59a` writer: pages 10244 and 10245 of the active segment, seven removals in 204 µs, inside a burst of 350
  removals around the writer's position) and 83 and 33 µs before (`main` writer: pages 28790 and 28791). A third event
  fell inside such a burst, and a fourth in the same second as one. Bursts cover 196–259 milliseconds of each 1,000 s
  run, so three of the ten distinct events (the nine of `main`'s reader and one that only the `279f59a` reader saw)
  within 2 ms of one is not chance (about 0.06 % for each event).
- **Others are not.** Six events had no removal of the active segment within 0.5 s; the two of them in the run that also
  traced migrations had no migration there either.
- **Without memory pressure it did not occur:** with the journal within RAM (8 GiB of retention), kswapd reclaimed
  nothing, compaction still migrated 1,024,628 pages (54 of them at the frontier), and no reader saw a lag in 1,000 s.
  At the rate with pressure, none in 1,000 s would happen by chance about 5 % of the time. This includes the six events
  without a removal: they need memory pressure too, so they may be reclaim by a path the trace does not show, or
  another effect of it; migration alone did not cause one.
- **x86-64 did not show it:** no lag event in 900 s, with more removals at the frontier (8,382) than any aarch64 run.
  At the aarch64 rate, none in 900 s would happen by chance about 7 % of the time. (The migration probe did not fire on
  this x86-64 kernel, although compaction ran; migrations there are not counted.)
- **The pages removed are ones the writer is about to write or is writing:** clean pages of the active segment, 1 to
  128 pages below the prefault frontier, removed by kswapd in bursts and faulted back in by the writer (the
  `279f59a` run refaulted 21.9 million file pages in 1,000 s, the `main` runs 106,000–113,000). Reclaim takes only
  clean pages; what wrote these back was not traced. The likely candidate is the checkpoint's whole-file `fdatasync` of
  the active segment, which writes back the prefaulted, still empty pages ahead of the writer (2026-11-25, roll and
  journal stalls: "ranged checkpoint sync").

The mechanism consistent with the first kind of event, in upstream Linux 7.0's code (Ubuntu's 7.0.0-1017-gcp builds
on it): reclaim unmaps a clean, mapped page-cache
folio from every process with the TLB invalidation deferred (`try_to_unmap` with `TTU_BATCH_FLUSH`), removes it from the
page cache (`__remove_mapping`, `mm/vmscan.c` line 1525) and only then waits for the invalidation (`try_to_unmap_flush`,
line 1609); migration, by contrast, waits before it copies (`mm/migrate.c` line 1968). In that window the writer faults
the page back in as a new page and fills it, while a reader that still has a TLB entry for the old page reads its
zeros. On arm64 the invalidation is broadcast when the page is unmapped and only its completion is deferred
(`arch_tlbbatch_add_pending`, then a `DSB` in `arch_tlbbatch_flush`); on x86-64 the whole flush is deferred to the end
of the batch, so this account alone would predict the effect on x86-64 too, where it was not seen. **Status:
consistent with, not confirmed** for the events that coincide with a removal (turning reclaim at the frontier on and
off was done only as a whole: one 1,000 s run without reclaim and without the effect, which tells at about 95 % and
switches all of reclaim, not the deferred invalidation), and **not explained** for the others. What would settle
it: the kernel's own view of the reader's translation at an event (a probe on the reader's page fault for that address
right after the event, or on `arch_tlbbatch_flush` timing), runs with the multi-generational LRU off
(`/sys/kernel/mm/lru_gen/enabled` 0) and with the checkpoint's whole-file `fdatasync` dropped (so the pages ahead of the
writer stay dirty and cannot be reclaimed), each long enough to expect ten events, and the same on another arm64
implementation.

Not measured: xfs; other machine sizes; runs long enough to compare rates between writers or readers; tracing overhead
on these runs (the untraced runs had events at a similar rate: 1 in 227 s and 1 in 600 s); the independent pair with the
trace. The migration probe was attached in the last two aarch64 runs only.

## 2026-11-26, how much of the limit is the disk: fio, tmpfs and one change at a time (Google Cloud VMs)

The owner's question after the stall findings above: "we are kind of limited by disk speed, right? How can we test
further?" (issue #60). This section measures the storage device by itself with `fio`, runs the sustained scenarios
with the journal on `tmpfs` (no disk at all) and on the local SSD, changes one storage setting at a time on the SSD, and
sweeps the payload size unpaced. All of it ran on **Google Cloud virtual machines, not bare metal**, one VM per
architecture, one after the other, on one day. The raw output (fio JSON and bandwidth logs, `tt-sustained` results and
series, traces, the scripts) is kept outside the repository.

### Environment and commands

| Machine (us-east1-b) | CPU, vCPUs, memory | Time (UTC) |
|---|---|---|
| `c4a-standard-8-lssd` | Google Axion (Neoverse-V2), 8, 31 GiB | 09:29–11:15 |
| `c4-standard-16-lssd` | Xeon 6985P-C, 16 (8 cores), 59 GiB | 11:18–12:43 |
| `c4a-standard-8-lssd` (a second VM) | Axion, 8, 31 GiB | 12:44–13:00: device bytes, fio with range syncs |

Ubuntu 24.04, kernel 7.0.0-1017-gcp, rustc 1.100.0, fio 3.36, bpftrace 0.20.2. `main` at `42b2307e` on both
(`tt-sustained` built on each VM). Each VM has two local NVMe SSDs of 375 GB (`nvme_card0`, `nvme_card1`): the first
with ext4 (`mkfs.ext4 -E lazy_itable_init=0,lazy_journal_init=0`, mounted `rw,noatime`), the second with xfs (defaults,
`rw,noatime,inode64,logbufs=8,logbsize=32k`) after the raw-device jobs below had run on it. The local SSDs report
`write_cache` "write through" (the kernel sends them no cache flushes), I/O scheduler `none`, `max_sectors_kb` 128,
`nr_requests` 1023, `wbt_lat_usec` 2000. Write-back sysctls at their defaults (`dirty_background_ratio` 10,
`dirty_ratio` 20, `dirty_expire_centisecs` 3000, `dirty_writeback_centisecs` 500) except in the rows that change them.
`tmpfs` was a dedicated mount (`mount -t tmpfs -o size=16g tmpfs /mnt/tmpfs`, default `huge=never`). Results on the
boot disk. Before every run: the journal deleted, `sync`, `fstrim` of the SSD's file system (close to 1 TB was written
to each device during the session), 3 s pause.

**tickertape** (`tt-sustained`; 60 s measured after 10 s of warm-up, as in the roll section above):

```sh
tt-sustained run --dir <journal> --out … --json --payload <64|256|1024|4096> --duration 60s --warmup 10s \
  --cpus 1,2,3,4,5,6 --checkpoint-every 1s --checkpoint-mode background --retain-bytes 4294967296 \
  --readers mapped,safe:wait=default,safe:durable:wait=block (--unpaced | --rate <rate>) [--no-disk-check on tmpfs]
```

The rates are the published steady-50 and steady-90 rates of these machine types (second sustained session above):
x86-64 173,119 / 311,614 records/s at 1,024 B and 1,603,530 / 2,886,355 at 64 B; aarch64 169,206 / 304,571 and
1,672,614 / 3,010,706. A record of payload P takes P + 40 bytes rounded up to 8 in the journal (1,064 B at 1,024 B, 104
B at 64 B); "MB/s" below is records/s × that size, the bytes the journal writes. Retention is 4 GiB (not the sessions'
64 GiB) so that the tmpfs runs never fill memory; on the SSD it deletes segments only after the data is written back.
"Unpaced" is the probe's command for 60 s instead of 10 minutes. Changes from the default: `--no-early-writeback`,
`--spare-segment`; without checkpoints the run has no retention and no durable-only reader
(`--readers mapped,safe:wait=default`); `lazytime` by `mount -o remount,noatime,lazytime`; the dirty limits by
`sysctl vm.dirty_background_bytes=… vm.dirty_bytes=…` (small: 64 MiB / 256 MiB on both; large: 8 / 16 GiB on aarch64,
16 / 32 GiB on x86-64), with `dirty_background_ratio=10 dirty_ratio=20` restored after each run. "Writer off-CPU" is
the run's length minus the CPU time of the writer's measuring thread: time it waited (throttling, journal, I/O).
"Seconds ≥ 10 ms" counts the measured seconds whose longest append took 10 ms or more.

**fio, the file system** (`journal.fio`, one job file, parameters from the environment):

```ini
; Sequential buffered writes into one preallocated file, the way a journal segment is written.
[global]
directory=${DIR}
filename=${NAME}
ioengine=${ENGINE}          ; mmap: stores into a shared mapping; sync: write(2)
rw=write
bs=${BS}                    ; 4k or 128k
size=${SIZE}                ; 64g unthrottled, 24g rate-limited
fallocate=native
rate=${RATE}                ; bytes/s: 0, or the 1,024 B steady-50 / steady-90 rate x 1,064
fdatasync=${SYNC}           ; writes between two syncs (msync(MS_SYNC) for the mmap engine): 1 s of the rate, or 256 MiB unthrottled
time_based=1
runtime=45
unlink=1
percentile_list=50:99:99.9:99.99
write_bw_log=${LOG}
log_avg_msec=1000
[journal]
```

The file was made with `fallocate -l <size>` before each job, so fio writes into unwritten extents as the journal does
(without it fio's mmap engine first writes the whole file; two jobs that did were discarded and rerun). fio syncs on the
thread that writes, so it never writes while a sync runs; to compare with tickertape, whose checkpoint thread syncs
beside the writer, three more jobs ran the mmap engine without its own syncs while a separate process called
`sync -d <file>` (`fdatasync`) once a second (`ext4x-…` below). One more job: 4 KiB buffered writes with an `fdatasync`
after each (`ENGINE=sync BS=4k SYNC=1`, 20 s), the latency of an ext4 commit on this device.

**fio, the raw device** (`raw.fio`, on the second SSD before its xfs was made): `filename=/dev/disk/by-id/google-local-nvme-ssd-1`,
`direct=1`, `ioengine=libaio`, `rw=write`, 30 s, with `bs`/`iodepth` 128k/1, 128k/32, 1m/32, 4k/1, and 4k/1 with an
`fdatasync` after every write.

### The answer in short

- **Throughput: yes, the disk is the limit on these VMs.** Each local SSD writes at most **351 MB/s**, a flat cap
  that does not move with block size or queue depth (every second of every raw job between 351 and 354 MB/s, on both
  machine types). Unpaced, tickertape on the SSD gets the device's 351 MB/s plus whatever the page cache takes in before
  write-back throttling starts (428–434 MB/s over 70 s on aarch64 with 5.1–5.4 GiB left dirty, 506–507 MB/s on x86-64
  with about 10 GiB); its writer thread spends 54 (aarch64) and 44–45 (x86-64) of the 70 seconds off the CPU, waiting.
  With the journal on tmpfs the same command writes 1,795–1,808 MB/s (aarch64) and 1,407–1,764 MB/s (x86-64) at 1,024
  B, three to four times as much. With the two local SSDs striped (RAID 0, 703 MB/s raw) the x86-64 writer reached 850–851 MB/s:
  it follows the device.
- **Payload.** From 256 B up the SSD rate is the same in MB/s (430–434 and 506–507) while tmpfs keeps growing (853
  MB/s at 256 B to 1,972–2,299 at 4,096 B): the device is the limit. At 64 B the two limits nearly meet: on tmpfs the
  writer does 3.8–4.0 million records/s (394–420 MB/s), on the SSD 3.34–3.37 million (348–351 MB/s) while staying on
  the CPU (off-CPU 0.1 s on x86-64, 4–6 s on aarch64) and the page cache stays small, with checkpoints durable 0.44–0.61
  s after the request: CPU-bound, with the device nearly saturated.
- **Latency: most of the tail at steady-90 on aarch64 is the storage stack, and on that VM most of it went away with
  early write-back off.** Without a disk (tmpfs with the spare segment, so that rolls do not zero 128 MiB on the writer's
  thread) the writer's append at steady-90 has p99.9 2.4–2.7 µs, p99.99 4.3–7.0 µs and a maximum of 48 µs (aarch64) and
  100 µs (x86-64), one run each. On the SSD, x86-64 has p99.99 9.3–11.2 µs and a maximum of 0.25–0.64 ms; aarch64 has
  p99.99 27–29 µs and maxima of 83–144 ms, with 19–36 seconds per minute containing an append of 10 ms or more (first
  VM). On aarch64, turning early write-back off brought p99.99 down to 4.1–4.4 µs (tmpfs's level) and the maximum to
  1.0–2.3 ms in 5 of 5 runs on two VMs, at the price of checkpoints becoming durable 0.93 s after the request instead
  of 0.05–0.39 s. On x86-64 it changed nothing in the tail and made checkpoints 0.93 s late instead of 2 ms.
- **The yardstick.** `fio` writing at steady-90's byte rate with a sync every second, and also with tickertape's
  write-back pattern, did not stall the way tickertape's default did on aarch64 (its writes took at most 0.07–1.7 ms,
  except one write of 122 ms in a `write` job without range syncs).
  On a second aarch64 VM tickertape with its defaults made the device write **7 % more bytes** than it appended (24.4
  GB against 22.7 GB per run; with early write-back off exactly the appended bytes; fio with the same pattern wrote only
  its data). At steady-90 that is about 348 MB/s for a device that takes 351. What writes the extra bytes was not
  traced here; it is the first thing to look at (below). (Traced since, in [2026-11-26, the writer's remaining page faults → What writes the extra bytes](#what-writes-the-extra-bytes-at-steady-90-pr-105s-question): early write-back writes back
  the 128 KiB folio the writer is still filling, and it is written again once full; stopping early write-back at the
  128 KiB boundary below the writer removed the extra bytes and these stalls in an experiment.)

### The device by itself (fio)

Selected jobs; MB/s is fio's mean over the job, latencies in µs unless stated, sync latency over all syncs of the job.

| Job | aarch64: MB/s | write p99.9 / max | syncs: n, p50, max | x86-64: MB/s | write p99.9 / max | syncs: n, p50, max |
|---|---|---|---|---|---|---|
| raw device, 128 KiB, QD 1 | 351 | 506 / 574 | – | 351 | 586 / 718 | – |
| raw device, 128 KiB, QD 32 | 351 | 13 ms / 23 ms | – | 351 | 19 ms / 24 ms | – |
| raw device, 1 MiB, QD 32 | 351 | 154 ms / 185 ms | – | 351 | 156 ms / 185 ms | – |
| raw device, 4 KiB, QD 1 | 235 | 24.4 / 122 | – | 128 | 255 / 462 | – |
| raw device, 4 KiB, QD 1, `fdatasync` after each | 233 | 26.2 / 112 | 1,703,331, 16.3, 113 | 125 | 272 / 513 | 918,935, 29.8, 518 |
| ext4, 4 KiB write + `fdatasync` after each | 53 | 8.5 / 101 | 257,887, 73.2, 10 ms | 27 | 16.3 / 175 | 132,638, 140, 1.2 ms |
| ext4 mmap 4 KiB, unthrottled, sync every 256 MiB | 334 | 27.0 / 72 | 56, 759 ms, 768 ms | 328 | 54.5 / 154 | 55, 759 ms, 765 ms |
| ext4 write 128 KiB, unthrottled, sync every 256 MiB | 342 | 29.8 / 96 | 58, 759 ms, 765 ms | 334 | 46.8 / 79 | 56, 759 ms, 765 ms |
| ext4 mmap 4 KiB at steady-50's rate, sync every 1 s | 178 | 27.8 / 68 | 45, 510 ms, 510 ms | 182 | 52.5 / 119 | 45, 518 ms, 520 ms |
| ext4 mmap 4 KiB at steady-90's rate, sync every 1 s | 318 | 27.3 / 68 | 45, 919 ms, 920 ms | 320 | 54.0 / 91 | 44, 935 ms, 942 ms |
| ext4 write 128 KiB at steady-90's rate, sync every 1 s | 318 | 27.0 / 100 | 45, 919 ms, 919 ms | 325 | 45.8 / 70 | 45, 935 ms, 941 ms |
| ext4 mmap 4 KiB at steady-90's rate, `fdatasync` every 1 s from another process | 324 | 30.1 / 1.7 ms | 5, 4.0 s, 5.2 s | 332 | 105 / 792 | 6, 4.9 s, 10.9 s |
| ext4 mmap 4 KiB at steady-50's rate, the same | 180 | 29.3 / 927 | 13, 1.08 s, 1.42 s | 184 | 102 / 1.2 ms | 20, 1.13 s, 1.92 s |
| ext4 mmap 4 KiB unthrottled, the same | 528 | 31.6 / 21 ms | 0 finished in 30 s | 576 | 94.7 / 21 ms | 0 finished in 45 s |
| xfs mmap 4 KiB at steady-90's rate, sync every 1 s | 315 | 11.3 / 78 | 44, 969 ms, 986 ms | 322 | 40.2 / 165 | 44, 944 ms, 947 ms |
| xfs write 128 KiB, unthrottled, sync every 256 MiB | 346 | 22.7 / 83 | 59, 759 ms, 761 ms | 337 | 41.7 / 68 | 57, 759 ms, 760 ms |
| RAID 0 of both SSDs, raw, 128 KiB / 1 MiB, QD 32 | | | | 703 / 703 | 8.3 ms / 63 ms (p99.9) | – |
| RAID 0, ext4 write 128 KiB, unthrottled, sync every 256 MiB | | | | 636 | 45.3 / 141 | 107, 375 ms, 383 ms |
| RAID 0, ext4 mmap 4 KiB at steady-90's rate, sync every 1 s | | | | 328 | 106 / 1.5 ms | 45, 468 ms, 473 ms |

On the second aarch64 VM (below), at steady-90's byte rate (324 MB/s), 45 s, `fdatasync` every second from another
process: `write` 128 KiB with `sync_file_range(SYNC_FILE_RANGE_WRITE)` every 1 MiB (tickertape's write-back step)
wrote with p99.99 68 µs and a maximum of 104 µs, its 42 syncs took 9.8 ms (p50) and at most 15 ms; mmap 4 KiB with the
same (fio's mmap engine turns the range sync into an `msync`, 0.5 ms each) at most 136 µs, syncs 10 ms and at most 16
ms; `write` 128 KiB without the range syncs had one write of 122 ms and 6 syncs of 4.6–10.3 s.

- The cap is the platform's, not the flash's: a flat 351 MB/s whatever the request size and depth, on both machine
  types; queueing deeper only adds latency (12–96 ms at QD 32). 4 KiB writes at QD 1 took 16 µs on aarch64 and 29 µs
  on x86-64. The devices report a write-through cache, so `fdatasync` sends no flush and costs nothing beyond the
  writes (16 and 30 µs); a drive with a volatile cache would add a flush to every checkpoint (further tests, below).
- Buffered writers get the cap too: unthrottled fio in the page cache wrote 310–346 MB/s, and one second of
  steady-90's data took 0.92–0.94 s to sync (one second of steady-50's 0.51–0.52 s), the time the device needs for it
  at 351 MB/s. That is also what a tickertape checkpoint costs without early write-back (0.93 s, below).
- fio's own writes waited at most 0.07–0.14 ms at steady-90's rate when it synced on its writing thread (where it
  never writes while a sync runs) or used tickertape's write-back pattern, and at most 0.8–1.7 ms with an `fdatasync`
  from another process and no range syncs, except the one 122 ms write above. An `fdatasync` from another process while the writer keeps going takes much longer than one second's
  data (4–11 s at steady-90; none finished unthrottled), unlike tickertape's checkpoint, which syncs up to a recorded
  position with a ranged `msync` before its whole-file `fdatasync`.

### Without a disk (tmpfs) against the SSD

On tmpfs `fdatasync` and `msync` return at once and `sync_file_range` does nothing: **this row has no durability at
all; it is a ceiling, not a mode to recommend.** What remains on tmpfs is the library's and the kernel's own cost:
appends, rolls, unmapping, scheduling, the helper threads.

| Payload | Arch | tmpfs: runs, records/s | MB/s | SSD (ext4): runs, records/s | MB/s | Writer off-CPU on the SSD (s of 70) | Page cache dirty at most (GiB) |
|---|---|---|---|---|---|---|---|
| 64 | aarch64 | 2: 4,003,436–4,021,481 | 416–418 | 2: 3,341,813–3,349,920 | 348 | 4.4–6.1 | 0.1–0.2 |
| 64 | x86-64 | 2: 3,787,930–4,035,725 | 394–420 | 2: 3,362,791–3,372,591 | 350–351 | 0.1 | 0.0–0.1 |
| 256 | aarch64 | 1: 3,249,920 | 962 | 1: 1,453,768 | 430 | 32.6 | 5.1 |
| 256 | x86-64 | 1: 2,883,171 | 853 | 1: 1,712,425 | 507 | 26.7 | 10.6 |
| 1,024 | aarch64 | 2: 1,686,719–1,699,041 | 1,795–1,808 | 3: 402,668–407,959 | 428–434 | 53.9–54.1 | 5.1–5.4 |
| 1,024 | x86-64 | 2: 1,322,402–1,657,601 | 1,407–1,764 | 3: 475,943–476,579 | 506–507 | 43.8–45.1 | 10.1–10.2 |
| 4,096 | aarch64 | 1: 555,930 | 2,299 | 1: 105,009 | 434 | 62.8 | 5.4 |
| 4,096 | x86-64 | 1: 476,872 | 1,972 | 1: 122,381 | 506 | 58.8 | 10.1 |
| 1,024 | x86-64, RAID 0 of both SSDs | | | 2: 798,645–799,730 | 850–851 | 24.5–25.2 | 9.7 |

The two x86-64 tmpfs runs at 1,024 B differ by 25 % (1,407 and 1,764 MB/s; append p50 0.47 and 0.12 µs), more than
some effects in this section; the x86-64 tmpfs figures are a range, not a rate. Subtracting what was still dirty at the
end, about 347 MB/s (aarch64) and 350 MB/s (x86-64) reached the device in the 1,024 B SSD runs: its cap.

At the same rate (unpaced rows above; "Seconds ≥ 10 ms" lists each run):

| Scenario | Payload | Arch | Storage | Runs | Late (%) | Append p99.9 | p99.99 | max | Roll append p50 | Seconds ≥ 10 ms | Mapped reader p99.9 | Safe reader p99.9 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| steady-50 | 64 | aarch64 | SSD (ext4) | 1 | 1.5 | 2.02 | 3.54 | 211 | 175 | 0 | 266 | 39.8 |
| steady-50 | 64 | aarch64 | tmpfs | 1 | 4.7 | 1.90 | 2.60 | 20 ms | 19 ms | 59 | 19 ms | 18 ms |
| steady-50 | 64 | x86-64 | SSD (ext4) | 1 | 8.5 | 2.07 | 6.72 | 381 | 133 | 0 | 114 | 83.8 |
| steady-50 | 64 | x86-64 | tmpfs | 1 | 5.5 | 1.80 | 6.07 | 31 ms | 27 ms | 59 | 28 ms | 27 ms |
| steady-50 | 1,024 | aarch64 | SSD (ext4) | 1 | 0.8 | 7.59 | 32.0 | 203 | 175 | 0 | 269 | 27.8 |
| steady-50 | 1,024 | aarch64 | tmpfs | 1 | 2.9 | 2.46 | 4.05 | 20 ms | 19 ms | 60 | 19 ms | 18 ms |
| steady-50 | 1,024 | x86-64 | SSD (ext4) | 1 | 1.4 | 5.97 | 18.6 | 205 | 149 | 0 | 76.0 | 24.0 |
| steady-50 | 1,024 | x86-64 | tmpfs | 1 | 4.4 | 2.83 | 6.56 | 29 ms | 27 ms | 60 | 28 ms | 27 ms |
| steady-90 | 64 | aarch64 | SSD (ext4) | 2 | 5.1–5.2 | 1.90–1.94 | 3.19–3.27 | 1.0–10 ms | 171–249 | 1, 0 | 744 to 1.5 ms | 263 to 1.5 ms |
| steady-90 | 64 | aarch64 | tmpfs | 2 | 17.9 | 1.83 | 2.66–2.91 | 20–21 ms | 19 ms | 60, 60 | 20 ms | 19 ms |
| steady-90 | 64 | x86-64 | SSD (ext4) | 1 | 33.7 | 1.90 | 6.85 | 176 | 119 | 0 | 548 | 503 |
| steady-90 | 64 | x86-64 | tmpfs | 1 | 18.9 | 1.81 | 6.00 | 29 ms | 28 ms | 60 | 29 ms | 27 ms |
| steady-90 | 1,024 | aarch64 | SSD (ext4) | 4 | 3.8–9.9 | 3.78–4.09 | 27.1–28.6 | 83–144 ms | 175–912 | 36, 19, 32, 25 | 48–129 ms | 48–129 ms |
| steady-90 | 1,024 | aarch64 | tmpfs | 2 | 5.3–5.4 | 2.39–2.41 | 3.89–4.16 | 20–21 ms | 19 ms | 60, 60 | 20 ms | 18 ms |
| steady-90 | 1,024 | aarch64 | tmpfs, spare segment | 1 | 0.4 | 2.43 | 4.27 | 48.2 | 38.2 | 0 | 1.2 ms | 11.4 |
| steady-90 | 1,024 | x86-64 | SSD (ext4) | 3 | 2.5–3.5 | 5.46–6.55 | 9.32–11.2 | 245–638 | 147–154 | 0, 0, 0 | 342–368 | 43.3–53.7 |
| steady-90 | 1,024 | x86-64 | tmpfs | 2 | 7.9 | 2.85–2.87 | 6.99–7.04 | 31–33 ms | 27–28 ms | 59, 59 | 28–29 ms | 27 ms |
| steady-90 | 1,024 | x86-64 | tmpfs, spare segment | 1 | 0.6 | 2.67 | 6.95 | 100 | 25.6 | 0 | 968 | 10.9 |

- **tmpfs has a cost of its own at every roll.** A roll appends in 19 ms (aarch64) and 27–30 ms (x86-64) on tmpfs
  against 0.12–0.25 ms on ext4: `fallocate` on tmpfs allocates and zeroes the whole 128 MiB segment, on the writer's
  thread. The traced runs (`offcpu.bt`, waits of 1 ms or more; unpaced and steady-90 at 1,024 B on each machine) show
  no wait of the writer's thread of 1 ms or more during the measured minute (only preemptions of 2–3 ms one second
  after the start, in the warm-up, and one sleep of 10 ms after the aarch64 steady-90 run ended): those roll appends are CPU time in
  the kernel (the writer's system time is 22.6 s for 801 rolls on aarch64, 22–32 s for 631–803 rolls on x86-64). The
  same cost makes the readers' p99.9 on tmpfs 18–29 ms. With the spare segment the zeroing moves to the spare's thread:
  rolls append in 26–38 µs and the writer's maximum at steady-90 is 48 µs (aarch64) and 100 µs (x86-64). Unpaced at
  2.1–2.4 GB/s a spare is sometimes not ready (roll maxima 4.5 ms on aarch64 and 33 ms on x86-64).
- **What the storage stack costs per append** (steady-90, 1,024 B, against tmpfs with a spare): p99.9 1.4–1.7 µs
  (aarch64) and 2.8–3.9 µs (x86-64) more, p99.99 23–24 µs (aarch64) and 2.4–4.3 µs (x86-64) more; at 64 B the p99.9
  and p99.99 of SSD and tmpfs are within 0.1–1 µs of each other (roll appends aside). These are the store faults on a
  file-backed mapping (ext4's `page_mkwrite`) and, on aarch64, the waits below.
- **What stays without a disk:** in every paced tmpfs run p99 of 0.2–0.4 µs (64 B) and 2.0–2.2 µs (1,024 B), p99.9
  1.8–2.9 µs and p99.99 2.6–7.0 µs, and the readers' wake-ups: the mapped reader's p99.9 on tmpfs with a spare was 1.2 ms (aarch64)
  and 0.97 ms (x86-64), not better than on x86-64's SSD (0.34–0.37 ms); one run each, not looked into.

### One change at a time on the SSD (1,024 B)

Rows in the order run, the base interleaved (two base runs in the first pass of the payload sweep and the change
block, the others after it; the aarch64 base runs `-r2` and the extra early-write-back-off runs after the session's
end, interleaved). Large dirty limits: 8 / 16 GiB on aarch64, 16 / 32 GiB on x86-64.

aarch64, steady-90 (304,571 records/s, 324 MB/s, 92 % of the device):

| Configuration | Runs | Late sends (%) | Seconds with an append ≥ 10 ms | ≥ 50 ms | Append p99.99 | Append max | Roll append p50 | Roll append max | Mapped reader p99.9 | To durable p50 | To durable max |
|---|---|---|---|---|---|---|---|---|---|---|---|
| ext4, defaults (base) | 4 | 3.8–9.9 | 36, 19, 32, 25 | 26, 5, 28, 2 | 27.1–28.6 | 83–144 ms | 175–912 | 3.3–92 ms | 48–129 ms | 273–391 ms | 496–836 ms |
| no checkpoints (and no durable-only reader) | 1 | 1.2 | 0 | 0 | 26.7 | 1.3 ms | 887 | 1.1 ms | 714 | – | – |
| early write-back off | 3 | 0.4–0.7 | 0, 0, 0 | 0, 0, 0 | 4.20–4.42 | 1.1–2.3 ms | 177 to 1.1 ms | 358 to 2.3 ms | 537 to 1.5 ms | 926 ms | 1.15 s |
| ext4 `lazytime` | 1 | 1.3 | 3 | 2 | 27.5 | 82 ms | 186 | 82 ms | 52 ms | 332 ms | 674 ms |
| spare segment on | 1 | 6.1 | 23 | 21 | 27.1 | 138 ms | 48.7 | 59.0 | 125 ms | 379 ms | 721 ms |
| dirty limits 64 / 256 MiB | 1 | 10.0 | 29 | 22 | 28.3 | 142 ms | 176 | 1.6 ms | 132 ms | 360 ms | 963 ms |
| dirty limits large (below) | 1 | 5.5 | 20 | 19 | 28.1 | 141 ms | 175 | 201 | 126 ms | 409 ms | 722 ms |
| xfs (second local SSD) | 1 | 0.9 | 1 | 1 | 21.6 | 123 ms | 145 | 123 ms | 71 ms | 409 ms | 827 ms |
| tmpfs | 2 | 5.3–5.4 | 60, 60 | 0, 0 | 3.89–4.16 | 20–21 ms | 19 ms | 20–21 ms | 20 ms | 41.6–43.0 | 50.4–53.3 |
| tmpfs, spare segment on | 1 | 0.4 | 0 | 0 | 4.27 | 48.2 | 38.2 | 48.2 | 1.2 ms | 39.3 | 52.6 |

aarch64, unpaced:

| Configuration | Runs | MB/s | Writer off-CPU (s of 70) | Seconds with an append ≥ 50 ms | Append p99.99 | Append max | Roll append p50 | Roll append max | Mapped reader p99.9 | Dirty max (GiB) | To durable p50 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| ext4, defaults (base) | 3 | 428–434 | 53.9–54.1 | 29, 36, 30 | 13–15 ms | 303–376 ms | 195 to 3.9 ms | 101–224 ms | 607–631 | 5.1–5.4 | 31.37–33.00 s |
| no checkpoints (and no durable-only reader) | 1 | 430 | 56.8 | 5 | 14 ms | 363 ms | 1.4 ms | 101 ms | 12.3 | 5.1 | – |
| early write-back off | 1 | 426 | 54.0 | 10 | 14 ms | 271 ms | 184 | 25 ms | 622 | 5.2 | 23.12 s |
| ext4 `lazytime` | 1 | 434 | 53.9 | 19 | 15 ms | 305 ms | 7.8 ms | 305 ms | 618 | 5.4 | 33.64 s |
| spare segment on | 1 | 433 | 53.8 | 25 | 15 ms | 347 ms | 48.4 | 99 ms | 667 | 5.5 | 34.12 s |
| dirty limits 64 / 256 MiB | 1 | 352 | 57.3 | 51 | 8.5 ms | 517 ms | 187 | 339 ms | 55 ms | 0.2 | 1.16 s |
| dirty limits large (below) | 1 | 569 | 48.3 | 25 | 15 ms | 337 ms | 3.3 ms | 188 ms | 630 | 14.4 | 70.73 s |
| xfs (second local SSD) | 1 | 430 | 54.8 | 0 | 14 ms | 26 ms | 161 | 25 ms | 642 | 5.1 | 32.40 s |
| tmpfs | 2 | 1,795–1,808 | 0.0 | 0, 0 | 4.68–4.82 | 22–23 ms | 21 ms | 22–23 ms | 1.6–2.3 ms | 0.0 | 2.5 ms |
| tmpfs, spare segment on | 1 | 2,375 | 0.0 | 0 | 4.85 | 10 ms | 2.5 ms | 4.5 ms | 2.2 ms | 0.0 | 2.5 ms |

x86-64, steady-90 (311,614 records/s, 332 MB/s, 94 % of the device):

| Configuration | Runs | Late sends (%) | Seconds with an append ≥ 10 ms | ≥ 50 ms | Append p99.99 | Append max | Roll append p50 | Roll append max | Mapped reader p99.9 | To durable p50 | To durable max |
|---|---|---|---|---|---|---|---|---|---|---|---|
| ext4, defaults (base) | 3 | 2.5–3.5 | 0, 0, 0 | 0, 0, 0 | 9.32–11.2 | 245–638 | 147–154 | 183–638 | 342–368 | 1.7–2.3 ms | 3.6–5.1 ms |
| no checkpoints (and no durable-only reader) | 1 | 2.8 | 0 | 0 | 8.77 | 1.1 ms | 999 | 1.1 ms | 711 | – | – |
| early write-back off | 1 | 1.5 | 0 | 0 | 9.03 | 557 | 180 | 557 | 437 | 933 ms | 1.15 s |
| ext4 `lazytime` | 1 | 3.2 | 0 | 0 | 11.4 | 691 | 155 | 691 | 366 | 2.0 ms | 4.5 ms |
| spare segment on | 1 | 2.8 | 0 | 0 | 9.92 | 293 | 46.9 | 66.2 | 247 | 1.6 ms | 3.7 ms |
| dirty limits 64 / 256 MiB | 1 | 3.0 | 0 | 0 | 11.1 | 353 | 149 | 196 | 347 | 1.8 ms | 4.0 ms |
| dirty limits large (below) | 1 | 2.6 | 0 | 0 | 9.53 | 865 | 147 | 865 | 344 | 1.7 ms | 3.7 ms |
| xfs (second local SSD) | 1 | 2.5 | 0 | 0 | 8.92 | 576 | 140 | 576 | 296 | 1.4 ms | 1.9 ms |
| two local SSDs in RAID 0, ext4 | 2 | 3.2–3.4 | 0, 0 | 0, 0 | 89.3–90.9 | 2.4–2.6 ms | 139–142 | 161–172 | 620–635 | 116–190 ms | 288–312 ms |
| tmpfs | 2 | 7.9 | 59, 59 | 0, 0 | 6.99–7.04 | 31–33 ms | 27–28 ms | 31–33 ms | 28–29 ms | 49.2–170 | 245–256 |
| tmpfs, spare segment on | 1 | 0.6 | 0 | 0 | 6.95 | 100 | 25.6 | 43.3 | 968 | 38.4 | 55.1 |

x86-64, unpaced:

| Configuration | Runs | MB/s | Writer off-CPU (s of 70) | Seconds with an append ≥ 50 ms | Append p99.99 | Append max | Roll append p50 | Roll append max | Mapped reader p99.9 | Dirty max (GiB) | To durable p50 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| ext4, defaults (base) | 3 | 506–507 | 43.8–45.1 | 29, 15, 1 | 13–14 ms | 273–437 ms | 173 to 11 ms | 273–437 ms | 481–793 | 10.1–10.2 | 51.98–52.41 s |
| no checkpoints (and no durable-only reader) | 1 | 506 | 47.6 | 4 | 14 ms | 73 ms | 2.5 ms | 73 ms | 12.3 | 10.1 | – |
| early write-back off | 1 | 502 | 45.1 | 18 | 13 ms | 351 ms | 8.2 ms | 351 ms | 567 | 10.2 | 44.33 s |
| ext4 `lazytime` | 1 | 507 | 44.2 | 25 | 13 ms | 253 ms | 8.7 ms | 253 ms | 532 | 10.2 | 51.61 s |
| spare segment on | 1 | 507 | 44.6 | 0 | 13 ms | 27 ms | 39.6 | 10 ms | 575 | 10.1 | 52.04 s |
| dirty limits 64 / 256 MiB | 1 | 354 | 52.1 | 33 | 7.5 ms | 358 ms | 339 | 220 ms | 879 | 0.2 | 1.11 s |
| dirty limits large (below) | 1 | 783 | 30.4 | 13 | 17.1 | 348 ms | 2.3 ms | 348 ms | 456 | 28.5 | 106.43 s |
| xfs (second local SSD) | 1 | 503 | 45.5 | 1 | 13 ms | 56 ms | 168 | 56 ms | 540 | 9.8 | 50.33 s |
| two local SSDs in RAID 0, ext4 | 2 | 850–851 | 24.5–25.2 | 4, 13 | 102–105 | 212–227 ms | 144–149 | 125–155 ms | 8.46–8.54 | 9.7 | 28.12–28.92 s |
| tmpfs | 2 | 1,407–1,764 | 0.0–1.2 | 0, 0 | 7.18–13.9 | 32–35 ms | 29–30 ms | 32–35 ms | 1.7–3.6 ms | 0.0 | 1.8–2.0 ms |
| tmpfs, spare segment on | 1 | 2,073 | 0.0 | 0 | 7.55 | 33 ms | 1.7 ms | 33 ms | 2.1 ms | 0.0 | 1.9 ms |

The second aarch64 VM (`c4a-standard-8-lssd`, us-east1-b, 12:44–13:00, the same build; steady-90, 1,024 B, two runs
each, interleaved; device sectors written from `/proc/diskstats` around each run):

| Configuration | Runs | Seconds with an append ≥ 10 ms | Append p99.99 | Append max | Mapped reader p99.9 | To durable p50 | Device writes per run |
|---|---|---|---|---|---|---|---|
| ext4, defaults | 2 | 2, 1 | 28.8–29.3 | 10–16 ms | 3.3–6.4 ms | 46–73 ms | 24.36–24.39 GB |
| early write-back off | 2 | 0, 0 | 4.10–4.28 | 1.0–1.1 ms | 453–465 | 928–929 ms | 22.70 GB |

The records of a run (70 s at 324 MB/s) are 22.68 GB; fio's jobs at the same rate wrote 14.58 GB (`write` with range
syncs, its data exactly) and 14.82 GB (mmap) in 45 s. The extra 1.7 GB per run (about 24 MB/s) is far more than the
prefaulted window that each checkpoint writes as zeros (about 268 KiB a second, 2026-11-26, "what prefaulted,
unwritten pages add to a checkpoint", above); that section also found more device writes with early write-back on
(0.7–2.3 MiB per checkpoint at 21 MB/s), not investigated there either.

- **Noise.** The base at aarch64 steady-90 drifted during the first session: 26 and 28 seconds with an append of 50
  ms or more in the first two runs, 2 and 5 in the last two; on the second VM 0 and 0 (maxima 10–16 ms). From 10:52
  on, every SSD run on that VM had a roll append p50 of 0.8–1.3 ms instead of 171–195 µs, with the same command; not
  explained. On x86-64 the unpaced base had 29, 15 and 1 seconds with an append of 50 ms or more: no unpaced change
  can be told from the base by that count. Every other row is one run.
- **Early write-back off is the only change that removed the aarch64 stalls in every run** (5 of 5 on two VMs, against
  every base run having appends of 10 ms or more, 10–144 ms at most), and with them the extra 7 % of device writes; its
  p99.99 (4.1–4.4 µs) is tmpfs's. Its cost is the checkpoint: durable 0.93 s after the request (p50; at most 1.15 s)
  instead of 0.05–0.39 s on aarch64 and 1.7–2.3 ms on x86-64, where the tail did not change (no stall either way). The
  mechanism consistent with these figures, not traced here: with the device near its cap the queue holds write-back
  that the ext4 journal's commit must wait behind (2026-11-25, "Where the writer's thread waits"), and the default
  configuration both keeps that queue full and adds 7 % to it.
- No checkpoints (one run): no stall either, at the cost of having no durability point. `lazytime` (one run): 3
  seconds with an append of 10 ms or more, the longest a roll (82 ms), as in the 2026-11-25 runs. xfs (one run): one
  roll of 123 ms, nothing else above 10 ms. The spare segment and either dirty limit: no change at steady-90.
- **Unpaced**, nothing changed the rate except the dirty limits, which only change how much the page cache takes in
  before the writer is throttled: small limits give the device's rate (352–354 MB/s) with checkpoints durable in about
  1.1 s but the longest appends (358–517 ms, 33–51 seconds with 50 ms or more); large ones give 569–783 MB/s over 70 s
  by holding 14–29 GiB dirty, with checkpoints a minute or more behind. xfs (aarch64 26 ms, x86-64 56 ms) and, on
  x86-64, the spare segment (27 ms) had far lower append maxima than every base run on their machine (303–376 and
  273–437 ms); one run each.
- **RAID 0** (x86-64, two runs each): unpaced, 850–851 MB/s, the writer off-CPU 25 s instead of 44–45. At steady-90 it
  made the tail worse: p99.99 89–91 µs against 9.3–11.2, maximum 2.4–2.6 ms against 0.25–0.64, checkpoints durable
  after 116–190 ms against 2 ms. md's RAID 0 with 512 KiB chunks was not tuned or traced.

### What would test it further

Ranked by what it would tell against what it costs (cloud prices of this session):

1. **Find the extra 7 % of device writes** (done: [2026-11-26, the writer's remaining page faults → What writes the extra bytes](#what-writes-the-extra-bytes-at-steady-90-pr-105s-question)) (`blktrace` or a `block_rq_issue` probe by sector and file offset, default
   against early write-back off, on aarch64 and x86-64, steady-90 and steady-50): whether pages are written twice and
   why (ext4's conversion of unwritten extents, the journal; the prefaulted window that every checkpoint writes as
   zeros is only about 268 KiB a second, 2026-11-26 "what prefaulted, unwritten pages add to a checkpoint", above, which
   also saw 0.7–2.3 MiB more per checkpoint with early write-back on at 21 MB/s and did not look further). It is the only finding here that may be the library's to fix, and it may also explain why aarch64 falls
   behind at 92 % of the cap while x86-64 keeps up at 94 % (x86-64's device bytes were not counted). About 30 minutes
   on each shape: under A$1.
2. **Map the knee:** steady rates at 80, 85, 90 and 95 % of the device's 351 MB/s, both payloads, 10-minute runs, with
   and without early write-back, on the aarch64 shape (64 B at 89 % did not stall, 1,024 B at 92 % did). It would turn
   "keep headroom" into a number for these devices. About 1.5 hours of `c4a-standard-8-lssd`: about A$1.1.
3. **Network block storage**, which many users have instead of local NVMe: Hyperdisk Balanced or Extreme with
   provisioned throughput (for example 500 MB/s) and IOPS, the same fio yardstick and scenarios. Higher per-request
   latency (hundreds of µs), a different queue and possibly flushes. A session like this one on a `c4-standard-8` with
   a provisioned 1 TB volume: about A$3–5 (the volume's provisioned throughput and IOPS cost by the hour).
4. **Faster local storage**: the same shape family with four local SSDs striped (`c4a-standard-16-lssd`, about A$1.5
   an hour) or a Z3 shape: shows whether tickertape keeps following the device past 1 GB/s and where the CPU limit
   (tmpfs: 1.4–2.3 GB/s at 1–4 KiB on these CPUs) takes over, and whether md RAID 0 always costs this much latency at
   paced rates.
5. **One short bare-metal session with a known NVMe drive** (not run; the owner has skipped bare metal for now, and the
   AWS credits could pay for it: `i4i.metal` or `z1d.metal` for one to two hours, about US$5–25 of credits). What it
   would add that these VMs cannot: a real drive without the platform's flat rate cap, so its own behaviour shows
   (write cache exhaustion, garbage collection, sustained rate after minutes, latency spikes that no limiter smooths);
   real flush cost if the drive reports a volatile cache; no hypervisor in the I/O path; the fio yardstick against a
   datasheet. The comparison: the same fio jobs and tickertape scenarios as here, 10-minute runs.
6. **A drive with power-loss protection against one without**, for the checkpoint's sync: these cloud SSDs report a
   write-through cache, so `fdatasync` sent no flush and checkpoints paid only for the writes. A consumer NVMe drive
   with a volatile cache adds a FLUSH to every `fdatasync` (often milliseconds, more under load); a drive with
   power-loss protection acknowledges it at once. Needs physical hardware (or a bare-metal instance whose drive model
   is known); the fio job `4k, QD 1, fdatasync after each` above is the measure.

### What these figures do not show

One VM per architecture for the main session (and a second aarch64 VM for 16 minutes); runs of 60 s, so a 10-minute
run has ten times the rolls and stalls, and an unpaced 60 s run is still filling the page cache (70 s runs flatter it
by about 80 MB/s on aarch64 and 155 MB/s on x86-64). Most changes ran once. One device type (Google's local NVMe behind its
rate cap), no network storage, no bare metal. The `fio` jobs approximate a journal: one thread, no readers, no prefault
helper, no checkpoint thread except the `sync -d` loop, which syncs the whole file. Device bytes were counted only on
the second aarch64 VM. Retention was 4 GiB, not the sessions' 64 GiB. The traced runs were the tmpfs runs only.

## 2026-11-26, the layout of `Reader` (ADR 0057, issue #86; Google Cloud VMs)

Why replay speed changed with the layout of `Reader` (PR #85 saw twice the time for the zero-copy reader at 64 B after
a 40-byte change, on an Apple M4 Pro), and the before and after of the fixed layout of ADR
[0057](decisions/0057-reader-layout.md). **Google Cloud virtual machines, not bare metal**; nothing here is compared
with earlier sections. The Mac was not measured. Raw output (results, layouts, disassembly, counters, the probe and the
scripts) is kept outside the repository.

### Environment and method

| | x86-64 | aarch64 |
|---|---|---|
| Machine | `c4-standard-4-lssd` (us-east1-b): Intel Xeon 6985P-C, 2 cores with 2 threads each, 15 GB | `c4a-standard-4-lssd` (us-east1-b): Google Axion (Neoverse-V2), 4 cores, 16 GB; the final comparison on a second VM of the same type |
| Time (UTC) | 2026-11-26 09:04–11:05 | 09:04–10:17 and 10:35–10:53 |
| Kernel, OS | 7.0.0-1017-gcp, Ubuntu 24.04 LTS, `randomize_va_space` 2 | the same |
| Counters | the VM's PMU (`--performance-monitoring-unit=standard`), `perf stat` | the same |
| rustc | 1.100.0, release profile as in the repository | the same |

Journals of 1,000,000 records of 64 B and 100,000 of 1,024 B (written once, page cache warm) on ext4 on the local SSD.
Two harnesses: the repository's `replay` benchmark (criterion, `--bench 'replay/records/' --warm-up-time 1
--measurement-time 2 or 3`), and a probe (not in the repository) that replays the same journals with the `Reader`
either on the stack of the replay function, as the benchmark has it (Linux puts it at a new offset within a page in
each process), or at a chosen address modulo 4096, and prints the median ns per record of 9 passes (5 in the sweeps)
after one untimed pass. Builds of one comparison ran interleaved in a rotating order; the noise floor is the same
binary under a second name. Ratios are medians of the per-round ratios against the reference build. Layouts were
read from `std::mem::offset_of!` in each build.

### The padding of PR #85 does not reproduce on Linux

Builds of `main` (`b88c6d5f`, where rustc puts `found` at offset 400): padding of 8, 16, 24, 32, 40, 48 and 64 bytes
left to rustc (it puts it after `found`), the same as `[u64; n]`, `#[repr(C)]` with rustc's order (`cmain`) and 8 to 64
bytes before every field or after every field, `segments_listed` unboxed as in PR #85, PR #85's own commits
(`07a0ac09c2`, field unboxed; `47b7d8a1c0`, boxed), and four builds with every function and block aligned (`-C
llvm-args=-align-all-functions=6 -C llvm-args=-align-all-nofallthru-blocks=5`). Reader on the stack, 8 rounds,
ratio of the median ns per record against `main`:

| Case | x86-64: 26 builds | x86-64: `cmain` | aarch64: 26 builds | aarch64: `cmain` |
|---|---|---|---|---|
| mapped, 64 B | 0.981–1.017 | 1.005 | 0.979–1.014 | 1.001 |
| safe, 64 B | 0.979–1.001 | 0.989 | 0.991–1.010 | 1.005 |
| mapped, 1,024 B | 0.991–1.005 | 0.991 | 0.997–1.009 | 1.001 |
| safe, 1,024 B | 0.981–1.023 | 0.983 | 0.996–1.026 | 1.002 |

PR #85's commits, mapped 64 B: 36.91 ns (boxed) and 37.09 ns (unboxed) per record on aarch64, 45.50 and 44.79 ns on
x86-64 (whose rounds were noisy in that hour, 40 to 51 ns for `main`). The criterion benchmark of the main padding
builds stayed within 2 % of `main` on both machines.

### What it depends on: a page boundary inside `found`

The reader at 80 addresses (every 8 bytes of a cache line, every 64 bytes of a page) showed one reproducible outlier,
on both machines: 40 bytes before every field, at 3648 modulo 4096 (x86-64 50.0 / 49.4 ns against a median of 41.1;
aarch64 41.2 / 41.5 against 36.5). There the page boundary falls 8 bytes into `found`. The **page sweep** then put the
boundary at every offset `j` of the struct (reader at `4096 − j`, steps of the struct's alignment), one timed round and
one under `perf stat`, both 64-byte cases. Offsets more than 5 % above the median of all offsets, in both rounds where
there were two:

| Build (`found` at) | x86-64 | aarch64 |
|---|---|---|
| `main` `b88c6d5f` (400) | `j` 408: +16 % mapped, +28 % safe; 424: +9 % / +15 % | 408: +16 % / +15 %; 440: +16 % / +16 % |
| 40 bytes before every field (440) | 448: +19 % / +23 %; 464: +10 % / +6 % | 448: +15 % / +14 %; 480: +16 % / +17 % |
| `main` `42b2307e` (400), three sessions | 408: +14 to +24 % / +26 to +30 %; 424: +8 to +17 % / +11 to +16 % | 408: +15 to +16 % / +14 to +21 %; 440: +15 to +16 % / +16 % |
| ADR 0057's layout (400, aligned to 16), two sessions | none | none |
| the first candidate (`found` at 0) without its alignment | 8: +16 % / +27 %; 24: +10 % / +14 % | 8: +16 % / +17 %; 40: +7 % / +9 % |

Single offsets were sometimes slow in one round and not in the other (noise; more often on x86-64); none of those is
listed. Counters per record at the slow offsets against all others:

| | x86-64 | aarch64 |
|---|---|---|
| `found` + 8 | `ld_blocks.store_forward` 8.98 against 7.94 (mapped), 5.01 against 4.02 (safe); `mem_inst_retired.split_loads`/`split_stores` unchanged | `st_align_lat` 1.0 against 0.00; `ld_align_lat` +1 to +2 |
| `found` + 24 (x86-64), + 40 (aarch64) | none of the measured counters moves (`ld_blocks.store_forward` 7.97–8.17) | `st_align_lat` 1.0 against 0.00 |

The record path writes `found.header` with two 16-byte stores and reads it with 16-byte loads (aarch64 also stores
`payload_at` and `payload_len` as one pair). A store that straddles two pages is slow on aarch64; on x86-64 the load
after it at `found` + 8 is not forwarded, and what makes `found` + 24 slow was not identified by these counters. Of
1,802 ordinary probe processes with `main`'s layout (reader on the stack), 9 had a boundary at a hot offset of `found`;
those at `found` + 8 replayed 64-byte records 14 to 21 % slower than the median of their build, 1,024-byte records 3 to
16 %.

**Ruled out on these machines**: 4K aliasing (x86-64 `ld_blocks.address_alias` reaches 7.3 per record at some of 64
addresses, with no change in time); cache-line splits (`found` + 8 at a line boundary inside a page: within 1 %); the
size of the struct as the cause (padding after every field; but see the 40-byte tail padding below); code alignment (the
aligned builds, and the slowdown follows the address with identical code); the stack against a boxed reader.

### Before and after (`42b2307e` against ADR 0057's layout)

See ADR 0057 → "Before and after" for the tables: probe, 30 rounds, median of per-round ratios to `main` 0.978–0.993
on x86-64 (noise floor 0.994–1.003) and 0.972–0.997 on aarch64 (0.999–1.007); criterion, 6 rounds, within the noise on
both. Two other orders were measured on x86-64 and rejected: the hot fields first with `found` at 0 (mapped 64 B 1.013
to 1.021 in three sessions, 1.019 without the alignment, 1.031 with aligned code) and the scalar fields first with
`found` at 224 (1.030); the other cases level. On aarch64 the first was 0.977. Padding at the end of ADR 0057's layout:
64 bytes −0.3 to +1.3 % on both machines; 40 bytes −0.4 to +0.2 % on aarch64, and on x86-64 +3.9 % and +1.9 % for the
zero-copy reader at 64 B in two sessions (+2.1 % with aligned code), the other cases within noise.

### What these figures do not show

The Apple M4 Pro where #85 saw the effect (16 KiB pages; not measured, see ADR 0057); other microarchitectures; readers
held by the bindings; 32-bit targets. Why the hot-fields-first orders and the 40-byte tail padding cost a few per cent on
x86-64.

## 2026-11-26, sustained load, roll waits and checkpoint cost on macOS (Apple M4 Pro, APFS)

The macOS half of three open items: the sustained session on macOS (roadmap, sustained-load comparison: "Missing: macOS
results"), how often and how long a roll waits for the spare segment (issue #60), and what prefaulted, unwritten data
adds to a checkpoint (roadmap, cheaper checkpoints: "a macOS result"). **One laptop, shared and not idle**: other
agents built and tested other branches during the runs, macOS background services (`mediaanalysisd` above one core for
most of the evening, Spotlight, `fileproviderd`) came and went, and nothing was pinned. Read the figures as this Mac on
this evening, secondary to the Linux results above. The raw output of every run (results, per-second series, traces,
the load before, during and after each run, the scripts) is kept outside the repository.

### Environment and method

| | |
|---|---|
| Machine | MacBook Pro (`Mac16,8`), Apple M4 Pro: 10 performance and 4 efficiency cores, 24 GB; on AC power |
| OS | macOS 27.2.1 (Darwin 27.2.0) |
| Journal | APFS on the internal SSD (`APPLE SSD AP1024Z`, FileVault on), the Data volume, `$HOME/tmp/tt-macos` |
| Build | `main` at `b88c6d5f`, rustc 1.99.1, release; plus `tt-sustained --count-clock-backwards` (this change) |
| Comparison | Homebrew OpenJDK 21.0.13.0.3, Chronicle Queue 2026.8, Aeron 1.54.0 (`compare/` pins) |

**What replaces CPU pinning: nothing.** macOS has no hard affinity (`tt-sustained --cpus` and `compare/` print that
pinning is unsupported); every thread ran at the default QoS and the scheduler chose performance or efficiency cores.
A paced writer and each spinning reader keep a core busy, so a run occupied two to four cores of fourteen.

**Durability in each row.** Every run that checkpoints uses a background checkpoint (`Writer::request_checkpoint`)
every second on a wall-clock grid, and a blocking one at the end. On macOS a checkpoint makes, on its own thread:
`msync(MS_SYNC)` of the written range of each segment, `fsync` of each segment, `fsync` of the journal directory when
a segment was created since the last one, `fcntl(F_FULLFSYNC)` on the newest segment (the drive's cache), then the
marker's `pwrite` and a second `F_FULLFSYNC` (`checkpoint::steps`, `mmap::full_barrier`). There is no early write-back
on macOS (`WriterConfig::early_writeback` has no effect there), so a checkpoint writes everything appended since the
last one. Visibility-only rows checkpoint never. The spare thread (macOS default) runs `fsync` once per spare.

**Shared machine rules.** Before each run the driver recorded `uptime` and the top CPU consumers; if another process
used more than half a core it waited up to 10 minutes, then ran anyway and marked the run *disturbed*; a process that
had already outlasted one full wait (`mediaanalysisd`, from 23:40 local time) was not waited for again, and runs that
started or ran while it was busy are marked disturbed all the same. During each run it sampled the load every 10 s and
the top consumers every 60 s. Journal data stayed below 20 GB at any moment (the driver would have killed the run),
which caps retention and run lengths below. **Runs marked disturbed:** most of them (in the tables: the item 2 rows
marked `*`; in item 1, every steady run but steady-90 at 1,024 B repeat 1, both 1,024 B visibility-only runs, the 1,024
B probe and bursty at 1,024 B repeat 2; the 1-minute load average during item 1's runs was 2.5–12.4). Two repeats of one
configuration differing more than the effect discussed are called out where it matters.

**The wall clock is not monotonic across processes here.** `tt-sustained`'s readers subtract the writer's send time
(wall clock) from their own receive time; on Linux that difference is never negative. Here three attempts of the 64 B
probe failed in the first 12–63 s with `wall clock moved backwards` (the reader's clock 7 µs behind the writer's stamp,
and still 3 µs behind when read again 4 µs later). Neither one thread (12.5 billion reads in 5 minutes), nor two
threads, nor two forked processes, nor a stamping process moved to the efficiency cores showed a backward step in
isolated tests of 20–300 s (`clock_gettime(CLOCK_REALTIME)` and `gettimeofday`; both have 1 µs resolution here), so the
cause was not found. A run that cannot finish cannot be measured, so `tt-sustained` has a new option,
`--count-clock-backwards`: such a sample is recorded as 0 µs and counted (`clock_backwards_samples`,
`clock_backwards_max_ns` per reader); without it a run still fails as before. Every macOS run below used it. Over all
of item 1 two samples were counted: one of 4.8 ms (steady-50, 1,024 B, repeat 2, the mapped reader) and one of 990 ms
(steady-90, 64 B, repeat 1, the safe reader), so the wall clock also stepped back as a whole at least once; a step of
990 ms also delays a paced writer (it waits for intended times that have moved away) and shifts the latency of the
records in flight.

### Sustained session (tickertape)

The scenarios of the Linux sessions (above), with `tt-sustained` run directly rather than through `benches/session.py`,
whose retention (40 % of free space, up to 64 GiB) does not fit the 20 GB limit. Arguments as `session.py` makes them,
except retention (8 GiB; the 1,024 B probe ran with 12 GiB), no `--cpus`, and the lengths:

| Scenario | Length | Repeats | Rate |
|---|---|---|---|
| probe (unpaced) | 300 s | 1 | – |
| steady-50, steady-90 | **600 s plus 10 s warm-up, as on Linux** | 2 (interleaved: both payloads, then again) | 50 % and 90 % of the probe |
| bursty | 120 s plus 10 s | 2 | 2 × probe for 200 ms of every second |
| visibility-only | 60 s plus 10 s, no checkpoints, no retention | 2 | capped so that a run writes at most 12 GB: 1,648,351 (64 B) and 161,117 (1,024 B) records/s, against half the probe |

for example steady-50 at 64 B:

```sh
tt-sustained run --dir $HOME/tmp/tt-macos/tt-steady-50-64-r1 --out <results>/tt-steady-50-64-r1 --rate 3686264 \
  --duration 600s --warmup 10s --payload 64 --checkpoint-every 1s --checkpoint-mode background \
  --retain-bytes 8589934592 --readers mapped,safe:wait=default,safe:durable:wait=block --count-clock-backwards --json
```

Not run: lagging, and beyond-RAM, which needs more than 1.5 × RAM (36 GB) retained. The whole item appended about 4 TB
to the laptop's SSD. Every run finished; no reader reported corruption, `read + skipped + remaining = written` held for
every reader and no reader lost a record to retention. Cells are the range over the two repeats (the probe ran once);
units, late sends, "pause" and "to durable" as in the Linux sessions; µs unless marked.

**Writer schedule and CPU (64 B):**

| Scenario | Rate (records/s) | Achieved (records/s) | Late sends (%) | Max schedule delay | Unsent | Writer CPU (process) |
|---|---|---|---|---|---|---|
| probe | unpaced | 7,372,529 | – | – | – | 1.01 |
| steady-50 | 3,686,264 | 3,686,278–3,686,280 | 4.7–4.9 | 1.3–1.5 s | 0 | 1.05–1.06 |
| steady-90 | 6,635,275 | 6,634,952–6,635,180 | 66.1–69.1 | 2.0–2.8 s | 0 | 1.04–1.05 |
| bursty | 2,949,011 | 2,962,705–2,962,731 | 100.0 | 536 ms to 1.0 s | 0 | 1.02 |
| visibility-only | 1,648,351 | 1,648,359 | 4.1–16.7 | 117–509 ms | 0 | 0.91–1.00 |

**Append latency (64 B, every append timed, µs):**

| Scenario | p50 | p99 | p99.9 | p99.99 | max |
|---|---|---|---|---|---|
| probe | 0.04 | 0.12 | 0.17 | 1.25 | 1.9 s |
| steady-50 | 0.04 | 0.12–0.17 | 0.21 | 3.42–3.54 | 184 ms to 1.1 s |
| steady-90 | 0.04 | 0.12 | 0.21 | 1.58–2.00 | 1.6–1.8 s |
| bursty | 0.04 | 0.12 | 0.21 | 2.33–3.17 | 113–370 ms |
| visibility-only | 0.04 | 0.12–0.17 | 0.21–0.25 | 1.12–1.75 | 117–510 ms |

**Checkpoints and rolls (64 B):**

| Scenario | Pause p50 | Pause p99 | Pause max | To durable p50 | To durable p99 | To durable max | Rolls | Roll p50 | Roll p99 | Roll max |
|---|---|---|---|---|---|---|---|---|---|---|
| probe | 33.2 ms | 932 ms | 1.2 s | 277 ms | 1.3 s | 1.5 s | 1,713 | 358 | 145 ms | 1.9 s |
| steady-50 | 686–718 | 104–126 ms | 184–385 ms | 188–189 ms | 290–303 ms | 401–559 ms | 1,742 | 329–335 | 95.7–103 ms | 184 ms to 1.1 s |
| steady-90 | 16.9–18.9 ms | 243–283 ms | 781 ms to 1.5 s | 221–244 ms | 606–660 ms | 1.7–1.8 s | 3,136 | 351–354 | 163–167 ms | 1.6–1.8 s |
| bursty | 5.05–7.07 ms | 91.0–370 ms | 113–370 ms | 165–166 ms | 547 ms to 1.6 s | 747 ms to 2.1 s | 297 | 344–357 | 86.6–139 ms | 113–370 ms |
| visibility-only | – | – | – | – | – | – | 89 | 13.0–80.0 ms | 117–510 ms | 117–510 ms |

**Readers (64 B):**

| Scenario | Reader | p50 | p99 | p99.9 | p99.99 | max | Lag max (records) | CPU |
|---|---|---|---|---|---|---|---|---|
| probe | mapped, spin | 0.00 | 679 | 2.94 ms | 8.91 ms | 1.9 s | 85,759 | 0.96 |
| probe | safe, default | 6.00 | 465 | 6.11 ms | 141 ms | 1.9 s | 236,927 | 0.86 |
| steady-50 | mapped, spin | 0.62–0.63 | 57.7–64.1 ms | 949 ms to 1.0 s | 1.3 s | 1.3 s | 6,871–49,599 | 0.95 |
| steady-50 | safe, default | 3.56–3.62 | 58.4–64.9 ms | 949 ms to 1.0 s | 1.3 s | 1.3 s | 75,583–80,255 | 0.98 |
| steady-50 | safe, durable-only, block | 822–828 ms | 1.2 s | 1.9–2.0 s | 2.3 s | 2.4 s | 8,302,591–8,527,871 | 0.26 |
| steady-90 | mapped, spin | 18.0–22.5 ms | 1.5–2.3 s | 1.9–2.7 s | 2.0–2.8 s | 2.0–2.8 s | 34,975–36,255 | 0.96 |
| steady-90 | safe, default | 18.0–22.5 ms | 1.5–2.3 s | 1.9–2.7 s | 2.0–2.8 s | 2.0–2.8 s | 177,791–355,583 | 0.90 |
| steady-90 | safe, durable-only, block | 1.1 s | 2.6–3.2 s | 3.1–3.9 s | 3.2–4.1 s | 3.2–4.1 s | 11,001,855–11,378,687 | 0.47–0.48 |
| bursty | mapped, spin | 137 ms | 337–351 ms | 493–589 ms | 557 ms to 1.0 s | 557 ms to 1.0 s | 7,767–371,199 | 0.97–0.98 |
| bursty | safe, default | 136–137 ms | 337–349 ms | 501–589 ms | 535 ms to 1.0 s | 536 ms to 1.0 s | 271,615–335,615 | 0.39 |
| bursty | safe, durable-only, block | 1.2 s | 1.6–2.6 s | 1.8–3.1 s | 1.8–3.1 s | 1.8–3.1 s | 5,898,239–8,511,487 | 0.21 |
| visibility-only | mapped, spin | 0.68–0.74 | 49.1–338 ms | 89.7–476 ms | 112–505 ms | 117–510 ms | 135–42,495 | 0.90–0.93 |
| visibility-only | safe, default | 2.84–3.13 | 49.1–359 ms | 89.7–497 ms | 112–527 ms | 117–532 ms | 280–323,327 | 0.86–0.96 |


**Writer schedule and CPU (1,024 B):**

| Scenario | Rate (records/s) | Achieved (records/s) | Late sends (%) | Max schedule delay | Unsent | Writer CPU (process) |
|---|---|---|---|---|---|---|
| probe | unpaced | 1,161,895 | – | – | – | 0.52 |
| steady-50 | 580,947 | 580,949–580,950 | 8.9–9.7 | 1.0–1.3 s | 0 | 1.05 |
| steady-90 | 1,045,705 | 768,232–816,707 | 94.3–97.2 | 149–177 s | 91,506,438–123,086,385 | 0.35–0.38 |
| bursty | 464,758 | 467,494–467,591 | 78.9–85.1 | 503 ms to 3.1 s | 0 | 0.79–0.86 |
| visibility-only | 161,117 | 161,118 | 2.0–2.5 | 77.7–81.8 ms | 0 | 1.02 |

**Append latency (1,024 B, every append timed, µs):**

| Scenario | p50 | p99 | p99.9 | p99.99 | max |
|---|---|---|---|---|---|
| probe | 0.08 | 0.17 | 0.79 | 9.71 | 3.3 s |
| steady-50 | 0.08 | 0.25 | 1.54 | 22.5–28.3 | 606 ms to 1.2 s |
| steady-90 | 0.08 | 0.17 | 1.33–1.79 | 1.57–1.62 ms | 3.7–4.4 s |
| bursty | 0.08 | 0.17–0.21 | 1.33–1.83 | 1.52–1.63 ms | 206–218 ms |
| visibility-only | 0.12 | 0.21–0.25 | 1.21–1.25 | 5.21–5.42 | 77.8–81.8 ms |

**Checkpoints and rolls (1,024 B):**

| Scenario | Pause p50 | Pause p99 | Pause max | To durable p50 | To durable p99 | To durable max | Rolls | Roll p50 | Roll p99 | Roll max |
|---|---|---|---|---|---|---|---|---|---|---|
| probe | 266 ms | 4.0 s | 5.2 s | 1.1 s | 7.4 s | 8.4 s | 2,764 | 17.3 ms | 482 ms | 3.3 s |
| steady-50 | 13.4–16.3 ms | 194–266 ms | 606 ms to 1.2 s | 212–249 ms | 558–763 ms | 1.3–1.8 s | 2,809 | 333–339 | 122–145 ms | 606 ms to 1.2 s |
| steady-90 | 253–261 ms | 3.4–3.5 s | 4.1–4.9 s | 811–817 ms | 6.9 s | 7.8–9.0 s | 4,081–4,331 | 22.3–24.9 ms | 503–647 ms | 3.7–4.4 s |
| bursty | 62.7–108 ms | 186–834 ms | 218 ms to 2.6 s | 237–412 ms | 330 ms to 4.3 s | 339 ms to 5.3 s | 478 | 10.5–20.2 ms | 163–168 ms | 206–218 ms |
| visibility-only | – | – | – | – | – | – | 89 | 13.0–13.3 ms | 77.8–81.8 ms | 77.8–81.8 ms |

**Readers (1,024 B):**

| Scenario | Reader | p50 | p99 | p99.9 | p99.99 | max | Lag max (records) | CPU |
|---|---|---|---|---|---|---|---|---|
| probe | mapped, spin | 603 | 16.1 ms | 23.5 ms | 68.6 ms | 3.3 s | 126,207 | 0.93 |
| probe | safe, default | 96.1 | 15.1 ms | 23.7 ms | 68.3 ms | 3.3 s | 126,207 | 0.23 |
| steady-50 | mapped, spin | 1.75–5.31 | 105–133 ms | 789–946 ms | 984 ms to 1.2 s | 1.0–1.3 s | 16,399–24,527 | 0.77–0.80 |
| steady-50 | safe, default | 3.21–3.26 | 105–134 ms | 787–946 ms | 984 ms to 1.2 s | 1.0–1.3 s | 20,111–69,631 | 0.91–0.92 |
| steady-50 | safe, durable-only, block | 792–810 ms | 1.4–1.5 s | 2.0–2.5 s | 2.2–2.8 s | 2.3–2.8 s | 1,324,031–1,451,007 | 0.11 |
| steady-90 | mapped, spin | 48–74 s | 148–175 s | 149–177 s | 149–178 s | 149–178 s | 80,127–126,207 | 0.93–0.94 |
| steady-90 | safe, default | 48–74 s | 148–175 s | 149–177 s | 149–178 s | 149–178 s | 80,255–126,207 | 0.17–0.20 |
| steady-90 | safe, durable-only, block | 51–76 s | 150–178 s | 153–181 s | 153–181 s | 153–181 s | 3,989,503–4,796,415 | 0.14–0.15 |
| bursty | mapped, spin | 50.3–73.9 ms | 364 ms to 2.7 s | 471 ms to 3.1 s | 494 ms to 3.1 s | 503 ms to 3.1 s | 16,559–16,783 | 0.98 |
| bursty | safe, default | 50.3–73.8 ms | 364 ms to 2.7 s | 471 ms to 3.1 s | 494 ms to 3.1 s | 503 ms to 3.1 s | 16,031–16,831 | 0.11 |
| bursty | safe, durable-only, block | 1.2–1.4 s | 1.3–5.4 s | 1.3–6.3 s | 1.3–6.3 s | 1.3–6.3 s | 929,791–2,018,303 | 0.08 |
| visibility-only | mapped, spin | 0.53 | 8.23–11.6 ms | 42.6–60.6 ms | 74.8–76.0 ms | 77.8–81.8 ms | 0–1 | 1.00 |
| visibility-only | safe, default | 2.42 | 8.27–11.6 ms | 42.6–60.6 ms | 74.8–76.0 ms | 77.8–81.8 ms | 3–168 | 0.97–0.98 |


What the session shows:
- **The writer is fast and the device absorbs a lot.** The unpaced 5-minute probe reached 7.37 million records/s at
  64 B (767 MB/s of payload) and 1.16 million at 1,024 B (1.19 GB/s) with a checkpoint requested every second, two to
  three times the Linux VMs' probes; append p99.9 is 0.17–1.8 µs in every scenario.
- **But the tail is set by rolls and checkpoints, not by the append.** At half the probe rate (64 B: 3.69 million
  records/s, 384 MB/s; 1,024 B: 581,000, 618 MB/s) every scheduled record was sent and 4.7–9.7 % of sends were late,
  yet the writer fell up to 1.0–1.5 s behind its schedule, roll appends had a p99 of 96–145 ms and a maximum of 0.18–1.2
  s, and the visible readers' p99.9 was 0.79–1.0 s (Linux at half the probe: at most 24 ms behind, p99.9 below 0.4 ms).
  In the 64 B steady-50 run of repeat 1, 91 of the 600 seconds had the writer more than 50 ms behind, and in each of
  them the longest append was a roll append of 50–184 ms. Why the rolls take that long is item 2 below.
- **Checkpoints take about 0.2 s, not milliseconds.** At half the probe rate a checkpoint became durable 188–249 ms
  (p50) after its request (Linux with early write-back: 0.85–4.5 ms): with no early write-back on macOS each checkpoint
  writes the whole second's data (380–620 MB) and then waits for two `F_FULLFSYNC` (item 3). The writer's pause per
  checkpoint was 0.69–0.72 ms (p50) at 64 B and 13–16 ms at 1,024 B; durable-only readers trailed by 0.79–0.83 s (p50).
- **90 % of the probe:** at 64 B (6.64 million records/s, 690 MB/s) every scheduled record was sent within 0.01 % of
  the rate, 66–69 % late, up to 2.0–2.8 s behind; at 1,024 B (1.05 million, 1.11 GB/s) **the writer did not keep its
  schedule**: it fell more than the 60 s overrun limit behind and stopped (`stopped_by: overrun_limit`) with 91 and 123
  million of 638 million records unsent, readers 48–74 s behind at p50. The 5-minute probe overstates what the
  writer sustains with periodic durability here, as it did on Linux.
- **Bursts** at twice the probe rate: at 64 B every send of a burst is late (as on Linux) and readers' p50 is 137 ms;
  at 1,024 B 79–85 % of sends are late and the backlog reached 0.5–3.1 s.
- **Without checkpoints** (visibility-only, at capped rates) appends were *not* faster in the tail: roll appends took
  13–80 ms (p50) and up to 0.51 s, because a writer that never checkpoints releases the oldest of its open dirty
  segments on its own thread at every roll (item 2).
- **Noise.** The two repeats agree within about 10 % on the medians, the late fractions and the checkpoint figures;
  the maxima do not (64 B steady-50 roll maximum 184 ms against 1.1 s; 1,024 B bursty schedule delay 0.5 against 3.1
  s; 64 B visibility-only late sends 4.1 against 16.7 %). Single maxima on this machine are not reproducible.

**Comparison with other tools** (reduced).

`compare/run.py --sustained` with Homebrew's JDK 21, for tickertape, Chronicle Queue and Aeron. The Linux comparison's
matrix could not run within the 20 GB limit: its probe is sized to 80 % of free space (about 60 GB here) and keeps
everything it writes, and paced runs at 50 % or 90 % of each tool's probe would write tens of GB within a minute. What
ran instead: the same rate for every tool (`--rates`), 100,000 records/s at 64 B and 10,000 at 1,024 B, SV and SD, 120 s
plus 10 s of warm-up, two repeats (Aeron has no SD run), the harness's default settings at `main` (`compare/README.md`
→ "JVM warm-up, tool settings and JVM logs": Chronicle Queue with `microTouch()` and a 1 GiB `blockSize`, the long JVM
warm-up, Aeron's term buffers not sparse and pre-touched), which are **not** the settings of the README's Linux
comparison (Chronicle's defaults, issue #56). tickertape uses the mapped reader and, in SD, the blocking
`Writer::checkpoint`, as in the Linux comparison. Every run finished (no failure, retry or skip); the 1,024 B half was
interrupted after 7 of its 10 runs and resumed with `--resume`. tickertape's adapter was built from the working tree
that also held item 2's instrumentation, inactive (no `TT_TRACE`): one check per roll and per checkpoint step, nothing
on the append path. All runs were disturbed by other processes at some point.

```sh
JAVA_HOME=/opt/homebrew/opt/openjdk@21 TMPDIR=$HOME/tmp/tt-macos/compare-data python3 compare/run.py <out> \
  --sustained --keep-going --tools tickertape,chronicle,aeron --workloads SV,SD --payloads 64 --rates 100000 \
  --duration 120
```

Reader latency from the intended send time, µs unless marked; "0.00" is below the 1 µs resolution of the Mac's wall
clock. Sync is each tool's SD operation, timed on the writer's thread.

| Payload, rate | Tool | Workload | Achieved (records/s) | Late (%) | Max late | Reader p50 | p99 | p99.9 | p99.99 | max | Sync p50 | Sync max |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 64 B, 100,000/s | tickertape | SV | 100,000 | 0.15–0.16 | 1.08–1.23 ms | 0.00 | 0.00 | 7.00 | 339–426 | 1.82–2.36 ms | – | – |
| 64 B, 100,000/s | tickertape | SD | 100,000 | 1.72–1.78 | 36.9–39.0 ms | 0.00 | 5.52–6.02 ms | 15.1–16.0 ms | 25.2–27.1 ms | 36.9–39.0 ms | 14.9–15.3 ms | 36.9–39.0 ms |
| 64 B, 100,000/s | chronicle | SV | 100,000 | 0.20–0.24 | 11.9–18.8 ms | 0.00 | 0.00 | 8.00–12.0 | 4.20–9.31 ms | 11.9–18.8 ms | – | – |
| 64 B, 100,000/s | chronicle | SD | 100,000 | 0.22–0.24 | 2.49–11.8 ms | 0.00 | 0.00 | 9.01–10.0 | 255 to 4.02 ms | 2.50–11.8 ms | 268–287 | 643–660 |
| 64 B, 100,000/s | aeron | SV | 100,000 | 0.13–0.17 | 165 to 12.7 ms | 0.00 | 0.00 | 6.00–9.01 | 14.0 to 11.8 ms | 344 to 22.7 ms | – | – |
| 1,024 B, 10,000/s | tickertape | SV | 10,000 | 0.14–0.17 | 1.16–10.1 ms | 0.00 | 3.00 | 7.00–13.0 | 328 to 11.6 ms | 1.90–20.6 ms | – | – |
| 1,024 B, 10,000/s | tickertape | SD | 10,000 | 1.67–1.69 | 22.1–24.2 ms | 0.00 | 5.41–5.46 ms | 14.8–15.0 ms | 19.1–19.3 ms | 22.2–24.2 ms | 15.0–15.2 ms | 22.2–24.3 ms |
| 1,024 B, 10,000/s | chronicle | SV | 10,000 | 0.14–0.17 | 5.76–14.8 ms | 0.00 | 2.00–3.00 | 8.00–14.0 | 2.30–7.17 ms | 5.77–14.9 ms | – | – |
| 1,024 B, 10,000/s | chronicle | SD | 10,000 | 0.15–0.21 | 7.71–12.9 ms | 0.00 | 1.00–3.00 | 11.0–23.0 | 3.34–4.30 ms | 7.82–13.0 ms | 309–571 | 959–992 |
| 1,024 B, 10,000/s | aeron | SV | 10,000 | 0.09–0.14 | 1.05–2.33 ms | 0.00 | 1.00–2.00 | 6.00–8.00 | 15.0–25.0 | 92.0 to 2.33 ms | – | – |

At these rates the three tools keep the schedule (at most 1.8 % of sends late), and without a sync their readers'
p99.9 is 6–14 µs; the tails differ. Without a sync (SV) tickertape's p99.99 was 0.34–0.43 ms at 64 B, Chronicle Queue's
4.2–9.3 ms and Aeron's 14 µs to 11.8 ms; at 1,024 B the two repeats of tickertape (0.33 and 11.6 ms) and of Aeron
differ by more than the tools do. **With a sync every second the guarantees are far apart on macOS**: tickertape's
checkpoint ends with two `F_FULLFSYNC`, which flush the drive's cache, and took 15 ms (p50) on the writer's thread,
so its reader's p99 rose to 5.4–6.0 ms (the records sent during the checkpoint wait for it); Chronicle Queue's
`msync(MS_SYNC)` took 0.27–0.57 ms because on macOS `msync` and `fsync` do not flush the drive's cache, and its reader
was not affected. The same sync with `F_FULLFSYNC` in Chronicle Queue was not measured. A comparison of the tools near
their sustainable rates on macOS is not in this record.

### The roll wait (issue #60)

On macOS the spare segment is prepared by a thread started when the active segment is half full, and a roll joins
that thread (`Spares::take`), so a roll waits for a spare still being prepared. The library has no metric for it
(`Writer::metrics` has none; the latency histograms of ADR 0047 time the roll, not its parts), so this item ran a
**local instrumented build** (a patch not in the repository): timestamps (monotonic clock, kept in memory, written by a
helper thread) at the writer's request for a spare, at each step of the spare thread (start; removing the old name and
creating the file; `F_PREALLOCATE` and `ftruncate`; `fsync`; `mmap`; `MADV_WILLNEED` of the first 8 MiB), at the start
and end of the join (and whether the thread had already finished), at each step of publishing the spare (header
`pwrite`, `link`, `unlink`), and at the start and end of the roll. The roll append times of `tt-sustained` are its own,
as in the stock build (the instrumented build's roll p50 of 0.30–0.35 ms at the lower rates is that of the stock
build's paced runs in item 1, 0.33–0.36 ms). `fs_usage`, `sample` and `dtrace` were not needed.

Cells: 60-second runs plus 5 s of warm-up, one mapped reader, three rates per payload (the highest at 90 % of item 1's
probe), the default 128 MiB segments and a smaller size chosen so that segments roll every 0.3–0.4 s (8 MiB, 32 MiB,
64 MiB); at the highest rates 128 MiB segments already roll every 0.12–0.19 s and the smaller size is 32 MiB (rolls
every 30–50 ms). Background checkpoints every second with 4 GiB of retention, or none (then no retention, and the
highest-rate cells shorter, 8–9 s, so that a run writes at most 10 GB). Two passes over all cells, about an hour apart;
cells are ranges over the two, and the counts are "first / second".

```sh
TT_TRACE=<trace file> tt-sustained run --dir … --out … --rate 6635275 --warmup 5s --duration 60s --payload 64 \
  --segment-size 33554432 --readers mapped --checkpoint-every 1s --checkpoint-mode background \
  --retain-bytes 4294967296 --count-clock-backwards --json
```

The last column classifies every roll of 1 ms or more by the part that took more than half of it: the wait for the
spare, the `link` that publishes it, or what follows the link (sealing the old segment and pushing it to the list of
dirty segments, which releases the oldest one's file and mapping when more than four are open,
`checkpoint::OPEN_SEGMENTS`); counts per pass, "wait–link–release".

| Payload | Rate (records/s) | Segment | Checkpoints | Achieved (records/s) | Late (%) | Max schedule delay | Rolls | Rolls that waited | Wait p50 | Wait max | Roll append p50 | p99 | max | Rolls ≥ 1 ms: mostly wait / link / release |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 64 B | 200,000 | 128 MiB | every 1 s | 199,987–200,001 | 0.2–0.3 | 1.26–9.40 ms | 10 | 0 / 0 | – | – | 314–350 | 365–382 | 365–382 | 0–0–0 / 0–0–0 |
| 64 B | 200,000 | 128 MiB | none | 200,000–200,001 | 0.2–0.3 | 907 to 1.14 ms | 10 | 0 / 0 | – | – | 817–831 | 877 to 1.12 ms | 877 to 1.12 ms | 0–0–0 / 0–0–1 |
| 64 B | 200,000 | 8 MiB | every 1 s | 200,001 | 0.3–0.4 | 3.26–6.92 ms | 161 | 0 / 0 | – | – | 317–330 | 2.76–3.71 ms | 3.26–6.93 ms | 0–3–0 / 0–3–0 |
| 64 B | 200,000 | 8 MiB | none | 200,001 | 0.3 | 1.52–5.29 ms | 161 | 0 / 0 | – | – | 684–987 | 1.35–5.10 ms | 1.39–5.29 ms | 0–3–17 / 0–0–71* |
| 64 B | 1,000,000 | 128 MiB | every 1 s | 1,000,003–1,000,005 | 0.2–0.3 | 10.6–11.3 ms | 50 | 0 / 0 | – | – | 304–333 | 10.6–11.3 ms | 10.6–11.3 ms | 0–3–0 / 0–1–0* |
| 64 B | 1,000,000 | 128 MiB | none | 1,000,003–1,000,005 | 1.0–1.7 | 15.0–49.6 ms | 50 | 0 / 0 | – | – | 12.0–21.5 ms | 15.0–49.6 ms | 15.0–49.6 ms | 0–0–38 / 0–0–40* |
| 64 B | 1,000,000 | 32 MiB | every 1 s | 1,000,003–1,000,005 | 0.3–0.4 | 12.5–16.3 ms | 201 | 0 / 0 | – | – | 311–323 | 8.47–12.6 ms | 12.5–16.3 ms | 0–10–0 / 0–7–0* |
| 64 B | 1,000,000 | 32 MiB | none | 1,000,003–1,000,005 | 1.3–1.6 | 4.90–12.0 ms | 201 | 0 / 0 | – | – | 3.22–4.30 ms | 4.87–10.1 ms | 4.90–12.0 ms | 0–1–188 / 0–1–189* |
| 64 B | 6,635,275 | 128 MiB | every 1 s | 6,632,363–6,632,465 | 38.4–39.4 | 104–217 ms | 334 | 3 / 4 | 23.9–31.9 ms | 34.3–45.7 ms | 332–334 | 50.1–139 ms | 59.5–176 ms | 3–34–37 / 4–31–34* |
| 64 B | 6,635,275 | 128 MiB | none | 6,635,257–6,635,309 | 53.6–57.9 | 37.4–46.7 ms | 71 | 0 / 0 | – | – | 13.0–16.2 ms | 29.7–32.8 ms | 29.7–32.8 ms | 0–0–67 / 0–0–67* |
| 64 B | 6,635,275 | 32 MiB | every 1 s | 6,617,561–6,634,641 | 70.4–97.3 | 151–487 ms | 1,336 | 522 / 378 | 1.28–1.33 ms | 99.6–172 ms | 2.22–3.74 ms | 81.3–108 ms | 147–176 ms | 221–66–945 / 115–108–983* |
| 64 B | 6,635,275 | 32 MiB | none | 6,635,289–6,635,308 | 28.6–38.3 | 13.1–64.4 ms | 287 | 21 / 0 | 1.24 ms | 1.61 ms | 2.13–3.80 ms | 3.98–5.96 ms | 5.63–13.1 ms | 0–0–277 / 0–1–278* |
| 1,024 B | 20,000 | 128 MiB | every 1 s | 20,000 | 0.1–0.2 | 1.06–1.43 ms | 10 | 0 / 0 | – | – | 333–338 | 366–390 | 366–390 | 0–0–0 / 0–0–0* |
| 1,024 B | 20,000 | 128 MiB | none | 20,000 | 0.1 | 1.60–6.18 ms | 10 | 0 / 0 | – | – | 797–836 | 1.65–6.21 ms | 1.65–6.21 ms | 0–0–1 / 0–0–1* |
| 1,024 B | 20,000 | 8 MiB | every 1 s | 20,000 | 0.2 | 4.91–7.06 ms | 164 | 0 / 0 | – | – | 305–310 | 637 to 2.69 ms | 4.93–7.09 ms | 0–2–0 / 0–1–0* |
| 1,024 B | 20,000 | 8 MiB | none | 20,000 | 0.3 | 1.56 ms | 164 | 0 / 0 | – | – | 673–983 | 1.16–1.47 ms | 1.47–1.61 ms | 0–0–7 / 0–0–75* |
| 1,024 B | 200,000 | 128 MiB | every 1 s | 200,000–200,001 | 0.3–0.4 | 34.3–35.9 ms | 103 | 0 / 0 | – | – | 314–319 | 34.3–35.9 ms | 34.3–35.9 ms | 0–6–0 / 0–6–0* |
| 1,024 B | 200,000 | 128 MiB | none | 200,000–200,001 | 2.6–3.1 | 48.1–51.8 ms | 72 | 0 / 0 | – | – | 16.7–17.2 ms | 42.1–51.8 ms | 42.1–51.8 ms | 0–0–64 / 0–0–64* |
| 1,024 B | 200,000 | 64 MiB | every 1 s | 200,000–200,001 | 0.7–0.8 | 54.6–57.3 ms | 206 | 0 / 0 | – | – | 311–319 | 45.7–48.0 ms | 54.6–57.3 ms | 0–14–0 / 0–15–0* |
| 1,024 B | 200,000 | 64 MiB | none | 200,000–200,001 | 2.2–2.6 | 13.6–15.3 ms | 145 | 0 / 0 | – | – | 6.40–8.45 ms | 11.3–11.8 ms | 13.6–15.3 ms | 0–0–137 / 0–0–137 |
| 1,024 B | 1,045,705 | 128 MiB | every 1 s | 1,045,707–1,045,710 | 35.5–35.6 | 174–248 ms | 538 | 42 / 30 | 10.6–11.8 ms | 124–241 ms | 3.57–5.09 ms | 107–122 ms | 174–248 ms | 29–49–209 / 23–51–208* |
| 1,024 B | 1,045,705 | 128 MiB | none | 914,108–1,045,527 | 52.6–56.1 | 161 ms to 2.0 s | 107 | 0 / 1 | 8.71 ms | 8.71 ms | 13.1–18.0 ms | 139 ms to 1.9 s | 139 ms to 1.9 s | 0–0–100 / 0–0–99* |
| 1,024 B | 1,045,705 | 32 MiB | every 1 s | 850,830–914,416 | 99.8 | 9.3–15 s | 2,155 | 1795 / 1617 | 5.43–8.11 ms | 172 ms to 1.4 s | 6.51–9.18 ms | 147–178 ms | 212 ms to 2.5 s | 901–46–969 / 896–42–887* |
| 1,024 B | 1,045,705 | 32 MiB | none | 1,044,454–1,044,975 | 54.7–69.3 | 427–767 ms | 431 | 112 / 161 | 8.39–9.16 ms | 14.3–15.7 ms | 4.46–6.57 ms | 21.6–31.5 ms | 22.9–37.5 ms | 83–1–343 / 106–0–325* |

`*` at least one of the two runs was disturbed.

What bounds the wait, from the timestamps:
- **A roll that finds the spare ready** costs 0.30–0.35 ms (p50), and most of it is the `link` (0.23–0.27 ms p50 on
  APFS; the header `pwrite`, the join of a finished thread and the `unlink` take 10–40 µs each). The ready spare took
  20–30 ms (p50) to prepare with 128 MiB segments (`fsync` 5–12 ms, the 8 MiB `MADV_WILLNEED` 16–17 ms, the rest
  below 0.1 ms) and 8–20 ms with 8–32 MiB ones.
- **No roll waited at 200,000 or 1 million records/s** (either payload, either segment size, with or without
  checkpoints): the spare has half a segment's time (0.2 s to 3 s here) and needs 8–30 ms.
- **Near the device's limit it waits** (64 B at 6.6 million records/s, 1,024 B at 1.05 million): with 128 MiB segments
  3–4 of 334 rolls (64 B) and 30–42 of 538 (1,024 B) waited, 11–32 ms at p50 and up to 46 ms and 241 ms; with 32 MiB
  segments 378–522 of 1,336 and 1,617–1,795 of 2,155, and at 1,024 B the writer fell 9–15 s behind its schedule.
- **What was running when a roll began to wait** (`waits.py` in the raw data): most often the 8 MiB `MADV_WILLNEED`
  (for example 1,020 and 1,117 of the 1,795 and 1,617 waits at 1,024 B with 32 MiB segments), which bounds those waits
  to its own length (at most 22 ms). At 1.1 GB/s half of a 32 MiB segment lasts 15 ms, less than that prefault alone.
  The **long** waits were the spare's `fsync` (up to 241 ms with 128 MiB segments and 1.37 s with 32 MiB) and the
  removal of the old name and creation of the file (up to 202 ms), and only with background checkpoints: without
  them the longest `fsync` that a roll waited for took 4.1 ms. The spare's sync and its directory operations queue
  behind the checkpoint thread's `msync`, `fsync` and `F_FULLFSYNC` of the same volume. The single 1.37 s wait was in
  one pass only (the other pass's longest in that cell was 166 ms).
- **The wait is not the main cause of slow rolls.** Of the rolls of 1 ms or more, a wait for the spare dominated
  only near the device's limit (at most about half of them, with 32 MiB segments). Two other parts did more:
  - the **`link`**, with background checkpoints: up to 33–57 ms at 200,000 records/s of 1,024 B and up to 175–220 ms
    near the limit, against at most 6.3 ms without checkpoints. Like the spare's sync, publishing the spare's name
    waits for the checkpoint thread's syncs. These links, not waits, are the 34–57 ms roll maxima of the 1,024 B cells
    at 200,000 records/s where no roll waited;
  - the **release of the oldest open segment** when more than four dirty segments are open: every roll of a writer that
    does not checkpoint (1.7–16 ms p50 after the link with 32–128 MiB segments; 0.36–0.50 ms with 8 MiB), and with
    background checkpoints near the limit, when more than four segments roll between two checkpoints. It is the `munmap`
    (and `close`) of a dirty mapping on the writer's thread; the same code with a clean list (with checkpoints at lower
    rates) takes 2–3 µs after the link.

What the figures do not show: the spare's thread start, `fsync` and prefault were not separated into CPU and I/O
time; why APFS makes a `link` wait for another thread's syncs was inferred from the timing, not traced in the kernel;
nothing here is Linux, where the spare is optional and a roll never waits (ADR 0049).

**Cheap improvements the data suggest** (not built here; each needs its own measurement and, for the second, a look
at the crash contract):
1. Hand the spare over before its prefault, or skip the spare's own `MADV_WILLNEED` (the prefault helper advises the
   new segment after the roll anyway, `Prefaulter::switch`): removes the most frequent wait (the 8 MiB advice, at
   most 22 ms), and with it the 8 MiB of zeros each prefault adds to a checkpoint (item 3).
2. Do not `fsync` the spare on macOS, as on Linux since ADR 0038: the checkpoint that first covers the segment syncs it
   and the directory. The longest waits were this `fsync` behind a checkpoint. Its cost moves to that checkpoint (item
   3: the first `fsync` of a preallocated APFS file writes about half of it).
3. Request the spare earlier than at half a segment. It would help only the waits shorter than the extra lead: with
   128 MiB segments near the limit half a segment is 60–100 ms, while the waits behind a checkpoint reached 241 ms.
4. Release the oldest open segment off the writer's thread (or keep more open between checkpoints): the most common
   slow roll near the limit and for writers that do not checkpoint.

### Checkpoint cost of prefaulted, unwritten data

The Linux half is recorded above ("what prefaulted, unwritten pages add to a checkpoint", merged while these runs went
on; on Linux the 256 KiB `MADV_POPULATE_WRITE` window is written as zeros by every checkpoint). The cells here follow
the roadmap entry: idle (one record a second), 20,000 and 200,000 records/s at 64 B and 20,000 at 1,024 B,
`WriterConfig::prefault` on (the default) and off (`--no-prefault`), 128 MiB segments, a background checkpoint every
second, 4 GiB of retention, one mapped reader; 120 s plus 5 s of warm-up (200 s at 64 B and 20,000 records/s, for three
rolls), two passes with the order of on and off swapped in the second. The same instrumented build as item 2 timed each
checkpoint's steps on the checkpoint thread (`msync` of each segment, `fsync` of each segment, the directory `fsync`,
the first `F_FULLFSYNC`, the marker's `pwrite` and second `F_FULLFSYNC`) and read the process's bytes written to disk
before the first step and after the last (`proc_pid_rusage`, `ri_diskio_byteswritten`).

**How far the bytes can be trusted.** `ri_diskio_byteswritten` counts the I/O the process issues, so a background
checkpoint's synchronous `msync` and `fsync` are in it, and so is anything else the process writes at the same moment
(the spare thread's `fsync`, which item 2 shows writes, falls into a checkpoint's window only if the two overlap).
Write-back that the kernel starts on its own would be missed; with checkpoints every second nothing was left for it,
since in every cell the bytes a checkpoint wrote were never below the bytes it covered (21.33 MB written for 21.28 MB
covered, and so on). It does not say what reached the flash, and it is not `iostat` (the whole disk) or `fs_usage`.
Two direct checks with a 40-line C program on the same volume, outside the runs: the first `fsync` of a file just
preallocated with `F_PREALLOCATE` and `ftruncate` writes 8.4 MB for 8 MiB, 34 MB for 32 MiB and 55–62 MB for 128 MiB
(about half of it, then `F_FULLFSYNC` takes 7–26 ms); and after that first `fsync`, `MADV_WILLNEED` on 8 MiB of the
file's shared mapping makes the next `fsync` write 8.3 MB, the same as storing into each page (8.4 MB), while reading
the same pages makes it write nothing. **On this macOS and APFS the prefault advice dirties the pages it advises**,
although the helper never stores into them.

Cells are ranges over the two passes; per checkpoint, p50 except where marked; µs unless marked. The first checkpoint
of each run and the final blocking one are left out; the first table leaves out the checkpoints that follow a roll,
which the second table shows. `*` a run of the cell was disturbed.

| Payload, rate | Prefault | Checkpoints | Covered (MB) | Written (MB) | `msync` | `fsync` | dir `fsync` | `F_FULLFSYNC` | marker + `F_FULLFSYNC` | Total p50 | Total p99 | Writer pause p50 / max | Append max |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 64 B, idle (1 record/s) | off | 123 × 2 | 0.00 | 0.03 | 108–312 | 6.54–8.42 | 1.33 | 4.71–4.78 ms | 3.94–3.95 ms | 8.78–9.03 ms | 12.3–13.7 ms | 6.21–7.38 / 13.0 | 12.3–12.4* |
| 64 B, idle (1 record/s) | on | 123 × 2 | 0.00 | 0.03 | 101–298 | 6.58–7.12 | 1.17–1.38 | 4.85–4.87 ms | 4.01–4.02 ms | 9.10–9.18 ms | 13.0–27.5 ms | 6.50–7.58 / 12.3–71.7 | 11.8–71.4* |
| 64 B, 20,000 records/s | off | 200 × 2 | 2.08 | 2.15 | 797–872 | 41.0–47.3 | 1.75 | 5.46–5.55 ms | 4.05–4.08 ms | 10.6 ms | 14.8–18.2 ms | 19.2–25.1 / 1.56–1.60 ms | 2.43–2.90 ms* |
| 64 B, 20,000 records/s | on | 200 × 2 | 2.08 | 4.21 | 892–951 | 498–544 | 1.58–1.71 | 5.00–5.50 ms | 4.02–4.07 ms | 10.6–11.0 ms | 16.5–16.7 ms | 469–518 / 1.52–1.70 ms | 1.52–1.70 ms* |
| 64 B, 200,000 records/s | off | 104 × 2 | 20.80 | 20.86 | 4.46–4.73 ms | 97.6–104 | 1.75–1.92 | 7.28–7.56 ms | 4.05–4.15 ms | 16.5–16.7 ms | 24.8 ms | 147–294 / 3.45–4.20 ms | 3.45–12.7 ms* |
| 64 B, 200,000 records/s | on | 104 × 2 | 20.80 | 28.75–28.79 | 4.30–4.56 ms | 1.97–2.11 ms | 1.75–1.79 | 8.50–8.81 ms | 4.07–4.08 ms | 19.2–19.8 ms | 25.8–28.8 ms | 27.4–27.8 / 1.31–1.78 ms | 1.78 ms |
| 1,024 B, 20,000 records/s | off | 104 × 2 | 21.28 | 21.33 | 4.73–4.90 ms | 99.0–128 | 1.58–1.83 | 7.18–8.00 ms | 4.09–4.58 ms | 16.6–18.5 ms | 25.0–145 ms | 192–391 / 4.44–32.6 ms | 4.44–32.6 ms |
| 1,024 B, 20,000 records/s | on | 104 × 2 | 21.28 | 29.21 | 4.34–4.59 ms | 1.94–1.95 ms | 1.71–1.75 | 8.99–9.28 ms | 4.08–4.17 ms | 19.6 ms | 29.0–33.0 ms | 9.92–10.7 / 1.33–1.45 ms | 1.45–3.43 ms* |

| Payload, rate | Prefault | Checkpoint | n (per pass) | Covered (MB) | Written (MB) | `fsync` | dir `fsync` | `F_FULLFSYNC` | Total p50 |
|---|---|---|---|---|---|---|---|---|---|
| 64 B, 20,000 records/s | off | first after a roll | 3 | 2.08 | 68.32–68.39 | 6.53–6.58 ms | 43.2–43.3 | 21.0–22.4 ms | 32.3–34.2 ms |
| 64 B, 20,000 records/s | off | the others | 200 | 2.08 | 2.15 | 41.0–47.3 | 1.75 | 5.46–5.55 ms | 10.6 ms |
| 64 B, 20,000 records/s | on | first after a roll | 3 | 2.08 | 2.24 | 398–401 | 24.4–35.0 | 5.15–6.32 ms | 10.3–11.5 ms |
| 64 B, 20,000 records/s | on | the others | 200 | 2.08 | 4.21 | 498–544 | 1.58–1.71 | 5.00–5.50 ms | 10.6–11.0 ms |
| 64 B, 200,000 records/s | off | first after a roll | 19 | 20.80 | 65.49–66.80 | 4.36–4.58 ms | 34.5–34.6 | 18.2–20.5 ms | 31.7–34.8 ms |
| 64 B, 200,000 records/s | off | the others | 104 | 20.80 | 20.86 | 97.6–104 | 1.75–1.92 | 7.28–7.56 ms | 16.5–16.7 ms |
| 64 B, 200,000 records/s | on | first after a roll | 19 | 20.80 | 28.15 | 1.61–1.62 ms | 27.9–32.7 | 9.13–9.17 ms | 18.2–18.6 ms |
| 64 B, 200,000 records/s | on | the others | 104 | 20.80 | 28.75–28.79 | 1.97–2.11 ms | 1.75–1.79 | 8.50–8.81 ms | 19.2–19.8 ms |
| 1,024 B, 20,000 records/s | off | first after a roll | 19 | 21.28 | 65.21–66.45 | 4.58–5.03 ms | 37.8–38.7 | 19.8–20.6 ms | 33.8–35.8 ms |
| 1,024 B, 20,000 records/s | off | the others | 104 | 21.28 | 21.33 | 99.0–128 | 1.58–1.83 | 7.18–8.00 ms | 16.6–18.5 ms |
| 1,024 B, 20,000 records/s | on | first after a roll | 19 | 21.28 | 28.69–28.87 | 1.59–1.70 ms | 32.3–33.3 | 8.59–9.33 ms | 18.6–18.7 ms |
| 1,024 B, 20,000 records/s | on | the others | 104 | 21.28 | 29.21 | 1.94–1.95 ms | 1.71–1.75 | 8.99–9.28 ms | 19.6 ms |

What the figures show:
- **The floor is the two barriers.** With nothing to write (idle) a checkpoint takes 8.8–9.2 ms, of which the two
  `F_FULLFSYNC` are 4.7–4.9 and 3.9–4.0 ms; `msync`, `fsync` and the directory `fsync` together stay below 0.4 ms.
  Prefaulting changes nothing there (the window was advised once, at the open).
- **With prefault on, each checkpoint writes the zeros the helper advised since the last one**: the bytes written
  exceed the bytes covered by about the distance the writer moved in that second, up to the 8 MiB window: 2.1 MB at
  20,000 records/s of 64 B (2.1 MB/s), 7.9 MB at 200,000 of 64 B and at 20,000 of 1,024 B (21 MB/s). The file-wide
  `fsync`, not the ranged `msync`, writes them: 0.50–0.54 ms instead of 0.04–0.05 ms at 2 MB/s, 1.9–2.1 ms instead
  of 0.10–0.13 ms at 21 MB/s. Those pages are written again, with records, at a later checkpoint.
- **What that adds to a checkpoint**: at 200,000 records/s of 64 B the total went from 16.5–16.7 ms to 19.2–19.8 ms
  (+2.5–3.3 ms: the `fsync` above and a longer first `F_FULLFSYNC`, 7.3–7.6 against 8.5–8.8 ms), in both passes. At
  20,000 records/s of 64 B (10.6 against 10.6–11.0 ms) and of 1,024 B (16.6–18.5 against 19.6 ms) the two passes of
  the same configuration differ by as much as on and off do, so no figure is given for those two.
- **With prefault off the spare segment is off as well** (it applies only with prefault on), so the roll creates and
  preallocates the segment on the writer's thread and the **first checkpoint after each roll** pays the first `fsync` of
  the new file: 65–68 MB written for 2.1 or 21 MB covered, the `fsync` 4.4–6.6 ms and the first `F_FULLFSYNC` 18–22 ms,
  32–36 ms in all against 10.6–18.5 ms for the others in the same runs. With prefault on, the spare thread has paid that
  `fsync` before the roll, and the first checkpoint after a roll costs what the others cost (18–19 ms; 10–12 ms at
  20,000 of 64 B). So on macOS prefaulting moves about 45–65 MB of zeros per segment off the checkpoint into the spare
  thread, and adds up to 8 MiB of zeros to every checkpoint.
- **Appends during the checkpoints**: the writer's longest stretch of work while a checkpoint was in flight (pause)
  stayed at most 1.3–1.8 ms with prefault on in every cell; with it off, up to 4.2 ms at 200,000 records/s of 64 B and
  32.6 ms (one pass) at 20,000 of 1,024 B, where the writer creates segments and faults pages itself. At 20,000
  records/s of 64 B the pause's median was 0.47–0.52 ms with prefault on against 19–25 µs off, in both passes; a store
  into a page that the checkpoint's `fsync` is writing back (one of the advised pages just ahead of the writer) would
  explain it, but this was not traced.

What they do not show: whether the zeros the advice dirties wear the SSD measurably (8 MiB a second at most, against
the records themselves); whether an advice that does not dirty pages on APFS (reading the pages, which wrote nothing in
the C check) prefaults as well for the writer's stores (not measured); and whether the advised pages are
write-protected again after a checkpoint, as on Linux (the writer's page faults were not counted here).

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

> **Superseded** by the [comparison rerun at v1.0.0](#2026-11-25-to-2026-11-26-comparison-rerun-at-v100-google-cloud-vms-linux-x86-64-and-aarch64),
> which corrected the harness: these runs had the old JVM settings and W1, no pinning, and v0.3.0's library. These results stay as they were recorded.

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
