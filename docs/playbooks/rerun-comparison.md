# Plan: rerun the whole comparison at v1.0.0

The comparison against Chronicle Queue, Aeron, yaque, queue-file and plain files (README → Performance,
`docs/benchmarks.md`) is rerun with the corrected harness (issue #57). This file is the plan for that run: what is
measured and why, on which machines, how long it takes and what it costs, and what is published. It replaces nothing
that is already recorded. Nothing here is a result. The driver is
`benches/rerun.py` (what runs, and in which order) and `benches/rerun-vm.sh` (what runs on the VM); the harness is
`compare/` ([README](../../compare/README.md)).

**Rule: whatever the numbers are, they are published.** The same cells, repeats and tables are published whether a
number moved up or down. A cell that fails is published as failed, with the error. Nothing is removed from a table
because it looks bad, and no cell is measured again because its result is unwelcome. Only a run that the harness
itself rejects is repeated, and only as described under [Failures](#repeats-order-and-failures).

## What is run

**Commit.** The tickertape library of v1.0.0 (`crates/tickertape` as in the tag; `main` was identical to the tag on
2026-11-25). The harness is the newest `main` that has #65 and #67 (`1e9846d` or later), in one commit for the whole
session. The driver refuses to start if `crates/tickertape` differs from the tag `v1.0.0`, or if the checkout has
uncommitted changes. If `main` has moved the library by then, check out the harness
commit and restore `crates/tickertape` from the tag in a local commit.

**Tools.** tickertape (its zero-copy reader is the one the headlines use; the safe reader runs as well in the short
mode), Chronicle Queue, Aeron IPC, yaque, queue-file, plain file. **Record sizes:** 64 B and 1,024 B. Versions as
pinned in `compare/`.

**Short mode** (`compare/run.py`): W1-cold and W1-steady (append burst), W2 (writer-to-reader latency at 10,000 and
100,000 records/s, 1,100,000 records each) and W3 (warm replay). **Sustained mode** (`--sustained`, ten minutes per
run): probe (unpaced), SV (paced at a fraction of the tool's own probe, no sync) and SD (SV plus the tool's
checkpoint equivalent every second).

| Row | Where | Question it answers |
|---|---|---|
| pinned, new settings, every tool | `short`, `sustained`, both machines, ext4 | The headline rows: what the corrected harness says, with every tool configured as its documentation recommends |
| pinned, new settings, every tool, xfs | `short-pinned`, `sustained-journals` (tickertape, Chronicle) | Whether the filesystem changes the picture |
| unpinned, new settings, every tool | `short` | Whether pinning alone moves the numbers of any tool (the old runs were unpinned) |
| Chronicle Queue and Aeron, all six old settings, pinned | `short` (both), `sustained-jvm-old` (both) | The before row the owner promised. `--no-long-warmup --no-micro-touch --chronicle-block-size default --aeron-sparse-terms --no-aeron-pre-touch --no-jvm-logs`. Compared with the pinned new row, it shows what the new settings gave |
| Chronicle Queue, `microTouch()` alone | `short`, both architectures | What `microTouch()` does on its own (the reviewers asked for it): Chronicle's own evidence is unclear, and its effect is expected to differ between aarch64 and x86-64 |
| 90 % of the probe with a checkpoint every second, tickertape and Chronicle | `sustained-90`, ext4 and xfs | The README sentence about the 90 % run and the x86-64 xfs backlog |
| tuned: isolated CPUs, one thread per core, helpers on other cores, every tool | `tuned-short`, ext4; `tuned-sustained`: tickertape, Chronicle, Aeron | The upper bound of what the machine allows. A reader with a tuned host wants to know whether the ranking holds there |
| yaque, queue-file, plain file, probe and SV at half of the probe | `sustained-others`, ext4 | Promised in #57 ("every tool"); they have no README sustained sentence, so one round and no SD |

The driver orders all of this as units: one tool, one configuration. Each unit is one invocation of `compare/run.py`
in its own directory. A unit has a fixed two repeats in the short mode and one in the sustained mode, where the rounds
below repeat it.

**Cut, and why.**

- SV at 90 % of the probe. The README does not use it; the saturated runs of the earlier sessions only showed large
  seconds-long backlogs. The SD run at 90 % stays.
- Unpinned rows in the sustained mode. Ten minutes average out what the scheduler does in single runs, and the short
  mode answers the pinning question for every tool.
- Aeron in the sustained xfs rows (it uses no journal file: only its shared buffers live there).
- `microTouch()` alone in the sustained mode, the opt-in JVM experiments (`--java-experiment`), and a bare-metal run
  (the owner skipped it).
- `--threads-per-core=1` as a separate machine shape. `nosmt` on the same VM gives one online thread per core, keeps
  one machine per architecture, and needs no second machine. The difference to
  the Google Cloud option is that the sibling threads exist but are offline.
- Windows, macOS and other shapes.

## Repeats, order and failures

- **Repeats.** Short mode: each unit runs every cell twice. `short` has two rounds: pinned, `jvm-old` and
  `microtouch-only` run in both, so those cells have four samples, taken at least an hour apart, and unpinned runs
  once (two samples).
  Sustained: the headline tools (tickertape, Chronicle, Aeron) run `sustained` twice,
  each round with its own probe, so the probe has two samples too and the paced rates of a round are those of its own
  probe. Other sustained cells run once.
- **Spread.** Report the range of the samples of every cell, separately for the repeats inside a unit (minutes apart)
  and for the rounds (hours apart). The playbook's rule of 10 % stays: state differences above it.
- **Interleaving.** Within a round the units go tool by tool; the tool order is rotated from one round to the next, and
  every second round reverses the order of the configurations within a tool. In a round each tool's configurations run
  one after the other, so they share
  the machine's state. Across the session the order is fixed by `PARTS` in `benches/rerun.py`, short part first, so
  a drift of the machine hits the tools alike. Filesystems and architectures are separate, not interleaved.
- **Failures.** Never change a setting because of a failure. A wall-clock step is discarded and repeated inside the run
  (`retries.jsonl`, at most three; existing harness behaviour).
  A unit that fails is run once more after its part, and fails for good the second time: then its cell is published as
  failed with the error from `children.log`. An unfinished short unit is set aside as `<unit>.incomplete-N`, and the
  driver starts it again; its incomplete data is kept, not used. A sustained unit resumes where it stopped.
- **Resume.** The driver can be killed and started again with the same command: finished units are skipped
  (`rerun.json`). The same OUT must be one commit; other commits are refused.

## Machines

| Role | Machine | Account | Filesystems |
|---|---|---|---|
| x86-64 | `c4-standard-16-lssd` (Xeon 6985P-C, 8 cores with 2 threads, 60 GB, two 375 GB local SSDs) | `benchmaxing2` (`~/tmp/tt-gcp/`) | ext4 on `/data`, xfs on `/data2`, same VM |
| aarch64 | `c4a-standard-8-lssd` (Axion, 8 cores, no SMT, 32 GB, two 375 GB local SSDs) | the same | the same |

Both shapes fit that account (quota 64 vCPUs in total, 24 per family and region); they are the shapes of the earlier
sessions. Both machines run the whole matrix, one VM per machine: ext4 and xfs are on two of its SSDs, so one session
measures both filesystems on the same VM. `provision.sh` formats the first local SSD as ext4 on `/data` and the second as xfs on
`/data2`, without lazy initialisation. The driver writes results to the boot disk (`~/results`), not to the filesystem under test. Use
any zone of `us-central1` or `us-east1` with capacity (C4: `us-central1-b`, `us-east1-b`, `us-east1-d` in earlier
sessions), and mind the C4 quota of 24 vCPUs per region: the x86-64 machine is 16 of them, so another C4 VM of 8 fits
in the same region.

**CPU layout**, checked on the x86-64 machine in the dry run: CPUs 0-7 are the first threads of the eight cores, 8-15
the second (`thread_siblings_list` of CPU 1 is `1,9`; one L3 for all 16). The automatic choice of `compare/affinity.py` there is
writer CPU 1, reader CPU 2, helpers `0,3-8,11-15` (12 CPUs: every allowed CPU except the siblings of the measured
cores). On the aarch64 machine, no SMT, the same rule gives writer 1, reader 2 and the six helpers `0,3-7`.

**Are 8 vCPUs enough for the aarch64 machine?** Every tool ran on 13 CPUs in the earlier checks; on 8 the automatic
choice leaves six helper CPUs, of which Aeron uses three busy-spinning driver threads (conductor, sender, receiver)
and two client conductors, with the JVM's threads on the rest. The harness needs at least four and warns below that
(`affinity.warnings`). Six is enough to place them, not enough to keep them apart from the JVM's GC and compiler
threads: that is the same on every
machine, and Aeron's row says so. It is not a reason for another shape.

**Tuned boot (both machines).** `rerun-vm.sh tune` writes
`isolcpus=managed_irq,domain,1,2 nohz_full=1,2 rcu_nocbs=1,2 irqaffinity=<other CPUs> nosmt` to the kernel command line,
runs `update-grub`, and reboots; the sibling threads and one thread per core are then online (`smt/control` is `off`).
Measured CPUs 1 and 2 (the writer and the reader; their siblings 9 and 10 are offline), helpers are the remaining online
CPUs. The Ubuntu 24.04 `7.0.0-1017-gcp` kernel supports it on both architectures: `CONFIG_NO_HZ_FULL`,
`CONFIG_RCU_NOCB_CPU` and `CONFIG_CPU_ISOLATION` are set (checked on the x86-64 VM and in the aarch64 package),
`CONFIG_HZ=1000`. The local SSD mounts do not survive a reboot, so `rerun-vm.sh` mounts them again and the driver
continues by itself after the reboot, from cron.

## Time and money

Estimated with the list prices of 2026-11-25 (`cloud.google.com/compute/all-pricing`, us-central1):
`c4-standard-16-lssd` US$0.955/h, `c4a-standard-8-lssd` US$0.482/h. At A$1.55 per US$ (the rate the ledger has used)
that is A$1.48/h and A$0.75/h; the ledgers of 2026-11 assumed US$0.95 and US$0.45.

