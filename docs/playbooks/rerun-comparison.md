# Plan: rerun the whole comparison at v1.0.0

The comparison against Chronicle Queue, Aeron, yaque, queue-file and plain files (README → Performance,
`docs/benchmarks.md`) is rerun with the corrected harness (issue #57). This file is the plan for that run: what is
measured and why, on which machines, how long it takes and what it costs, and what is published. It replaces nothing
that is already recorded. Nothing here is a result. The driver is `benches/rerun.py` (what runs, and in which order)
and `benches/rerun-vm.sh` (what runs on the VM); the harness is `compare/` ([README](../../compare/README.md)).

**Rule: whatever the numbers are, they are published.** The same cells, repeats and tables are published whether a
number moved up or down. A cell that fails is published as failed, with the error. Nothing is removed from a table
because it looks bad, and no cell is measured again because its result is unwelcome. Only a run that the harness
itself rejects is repeated, and only as described under [Failures](#repeats-order-and-failures).

## What is run

**Commit.** The tickertape library of v1.0.0 and the harness of `1e9846d` (the newest `main` with #65 and #67 in which
`crates/tickertape` is identical to the tag). Later `main` commits change the library (#70 on 2026-11-26), so the
session uses a branch: `1e9846d` plus a cherry-pick of this change (the driver files), one commit for the whole
session. The driver refuses to start if `crates/tickertape` differs from the tag `v1.0.0`, or if the checkout has
uncommitted changes. If a later harness fix is needed, cherry-pick it onto the branch, not the library.

**Tools.** tickertape (its zero-copy reader is the one the headlines use; the safe reader runs as well in the short
mode), Chronicle Queue, Aeron IPC, yaque, queue-file, plain file. **Record sizes:** 64 B and 1,024 B. Versions as
pinned in `compare/`.

**Short mode** (`compare/run.py`): W1-cold and W1-steady (append burst), W2 (writer-to-reader latency at 10,000 and
100,000 records/s, 1,100,000 records each) and W3 (warm replay). **Sustained mode** (`--sustained`, ten minutes per
run): probe (unpaced), SV (paced at a fraction of the tool's own probe, no sync) and SD (SV plus the tool's checkpoint
equivalent every second).

| Row | Where | Question it answers |
|---|---|---|
| pinned, new settings, every tool | `short`, `sustained`, both machines, ext4 | The headline rows: what the corrected harness says, with every tool configured as its documentation recommends |
| pinned, new settings, every tool, xfs | `short-pinned`, `sustained-journals` (tickertape, Chronicle) | Whether the filesystem changes the picture |
| unpinned, new settings, every tool | `short` | Whether pinning alone moves the numbers of any tool (the old runs were unpinned) |
| "Old settings, new harness" (`jvm-old`): Chronicle Queue and Aeron, all six old settings, pinned | `short` (both), `sustained-jvm-old` (both) | The before row the owner promised. `--no-long-warmup --no-micro-touch --chronicle-block-size default --aeron-sparse-terms --no-aeron-pre-touch --no-jvm-logs`. In the short mode the difference to the pinned new row is what the settings gave, with the limits under [Reading the rows](#reading-the-rows); in the sustained mode only the probe is comparable |
| "Old settings plus `microTouch()`" (`microtouch-only`), Chronicle Queue | `short`, both architectures | What `microTouch()` does on its own: long warm-up off, 64 MiB `blockSize`, JVM logs off, so it is not `microTouch()` on top of the new settings. Chronicle's own evidence is unclear, and its effect is expected to differ between aarch64 and x86-64 |
| 90 % of the probe with a checkpoint every second, tickertape and Chronicle | `sustained-90`, ext4 and xfs | The README sentence about the 90 % run and the x86-64 xfs backlog |
| tuned: isolated CPUs, one thread per core, helpers on other cores, every tool | `tuned-short`, ext4; `tuned-sustained`: tickertape, Chronicle, Aeron | The upper bound of what the machine allows. A reader with a tuned host wants to know whether the ranking holds there |
| yaque, queue-file, plain file, probe and SV at half of the probe | `sustained-others`, ext4 | Promised in #57 ("every tool"); they have no README sustained sentence, so one round and no SD |

The driver orders all of this as units: one tool, one configuration. Each unit is one invocation of `compare/run.py`
in its own directory. A unit has a fixed two repeats in the short mode and one in the sustained mode, where the rounds
below repeat it.

**Cut, and why.**

- SV at 90 % of the probe, for cost (12 runs per round and filesystem). The README quotes no figure from it. It was
  measured in the earlier sessions and some tools look bad in it, tickertape on x86-64 xfs among them (p99 of 21 to
  22 ms at 64 B), so the old SV 90 % rows stay in `docs/benchmarks.md`, dated. The SD run at 90 % stays.
- Unpinned rows in the sustained mode, for cost. The short mode has them for every tool; what pinning does over ten
  minutes is not answered.
- Aeron in the sustained xfs rows (it uses no journal file: only its shared buffers live there).
- `microTouch()` alone in the sustained mode, the opt-in JVM experiments (`--java-experiment`), and a bare-metal run
  (the owner skipped it).
- `--threads-per-core=1`, as a separate machine shape. The tuned boot uses `nosmt` on the same VM instead: one online
  thread per core, no second machine. The Google Cloud option was not tried.
- Windows, macOS and other shapes.

## Repeats, order and failures

- **Repeats.** Short mode: each unit runs every cell twice. `short` has two rounds: pinned, `jvm-old` and
  `microtouch-only` run in both, so those cells have four samples, and unpinned runs once (two samples). The gap between
  the rounds differs per cell (some are under an hour): take it from the timestamps and state it. Sustained: the headline tools (tickertape, Chronicle, Aeron) run `sustained` twice, each round
  with its own probe, so the probe has two samples. SV and SD do not: the paced rates of a round are those of its own
  probe, so the two rounds are different loads and are published separately. All other sustained cells run once
  (n = 1: the 90 % rows, the xfs rows, the other tools, the old settings, the tuned rows).
- **Spread.** Report the range of the samples of every cell, separately for the repeats inside a unit (minutes apart)
  and for the rounds (hours apart). The playbook's rule of 10 % stays: state differences above it.
- **Order.** Within a round the units go tool by tool. The tool order is rotated between the rounds of `short` and
  `sustained`, and the second round of `short` reverses the order of the configurations within a tool. Every other part
  has one round and a fixed order with tickertape first: the xfs parts, `sustained-90`, `sustained-jvm-old`,
  `sustained-others` and both tuned parts (in `sustained-others` yaque is first). In the xfs parts tickertape's first
  large writes go to a freshly made filesystem on an SSD that nothing trims afterwards, and Chronicle follows on a
  disk already written. Compare per tool the `sustained-journals` xfs probe with the later `sustained-90` xfs probe,
  and on ext4 round 1, round 2 and `sustained-90`, and publish those comparisons.
- **Failures.** Never change a setting because of a failure. A wall-clock step is discarded and repeated inside the
  run (`retries.jsonl`, at most three; existing harness behaviour). A unit that fails is run once more after its part,
  and fails for good the second time: then its cell is published as failed with the error from `children.log`. An
  unfinished short unit is set aside as `<unit>.incomplete-N` (by `compare/run.py` itself since the run, or by the
  driver when it starts the unit again), and the driver starts it again; its incomplete data is
  kept, not used. A sustained unit resumes where it stopped. A cell that failed and passed on the second attempt is
  published as "failed once, then ..." with the error from `failures.jsonl`. A retried short unit measures all its
  cells again: say which attempt is used.
- **Resume.** The driver can be killed and started again with the same command: finished units are skipped
  (`rerun.json`). The same OUT must be one commit; other commits are refused.

## Reading the rows

- **Different loads.** Every sustained unit is paced at 50 % (or 90 %) of the probe of its own configuration and round.
  The SV and SD figures of two configurations therefore mix the setting with another absolute rate. Print the absolute
  rate beside every SV and SD figure, call it half of each tool's own rate in that configuration, and attribute only
  probe differences to a setting. `sustained-jvm-old` also runs hours after `sustained`.
- **Old settings are not the old harness.** What else differs is in `compare/README.md` ("The old settings are not the
  old code"); repeat it with the row.
- **Unpinned rows** can run on any CPU, CPU 0 included, which also runs the result pulls (nice 19, about every ten
  minutes).
- **Tuned rows.** CPU isolation is recorded. `irqaffinity` only sets the default, so the report says where interrupts
  landed from `/proc/interrupts` and `/proc/irq/*/effective_affinity_list`, captured several times during the tuned
  parts, or does not call the CPUs interrupt-free.
- **Aggregation.** Use only units marked `done` in `rerun.json`, never a glob over the directories: a unit that failed
  twice leaves partial results in its directory. Print n for every cell.
- **Commit.** It is in `rerun.json` and in the environment files, not in each result line.

## Machines

| Role | Machine | Account | Filesystems |
|---|---|---|---|
| x86-64 | `c4-standard-16-lssd` (Xeon 6985P-C, 8 cores with 2 threads, 60 GB, two 375 GB local SSDs) | `benchmaxing2` (`~/tmp/tt-gcp/`) | ext4 on `/data`, xfs on `/data2`, same VM |
| aarch64 | `c4a-standard-8-lssd` (Axion, 8 cores, no SMT, 32 GB, two 375 GB local SSDs) | the same | the same |

Both shapes fit that account (quota 64 vCPUs in total, 24 per family and region); they are the shapes of the earlier
sessions. Both machines run the whole matrix, one VM per machine: ext4 and xfs are on two of its SSDs, so one session
measures both filesystems on the same VM. `provision.sh` formats the first local SSD as ext4 on `/data` and the second
as xfs on `/data2`, without lazy initialisation. The driver writes results to the boot disk (`~/results`), not to the
filesystem under test. Use any zone of `us-central1` or `us-east1` with capacity (C4: `us-central1-b`, `us-east1-b`,
`us-east1-d` in earlier sessions), and mind the C4 quota of 24 vCPUs per region: the x86-64 machine is 16 of them, so
another C4 VM of 8 fits in the same region.

**CPU layout**, checked on the x86-64 machine in the dry run: CPUs 0-7 are the first threads of the eight cores, 8-15
the second (`thread_siblings_list` of CPU 1 is `1,9`; one L3 for all 16). The automatic choice of
`compare/affinity.py` there is writer CPU 1, reader CPU 2, helpers `0,3-8,11-15` (12 CPUs: every allowed CPU except
the siblings of the measured cores). On the aarch64 machine, no SMT, the same rule gives writer 1, reader 2 and the
six helpers `0,3-7`.

**Are 8 vCPUs enough for the aarch64 machine?** With 8 cores the automatic choice leaves six helper CPUs, of which
Aeron uses three busy-spinning driver threads (conductor, sender, receiver) and two client conductors, with the JVM's
threads on the rest. The harness needs at least four and warns below that (`affinity.warnings`). Six is enough to
place them, not enough to keep them apart from the JVM's GC and compiler threads: that is the same on every machine,
and Aeron's row says so. It is not a reason for another shape.

**Tuned boot (both machines).** `rerun-vm.sh tune` writes `isolcpus=managed_irq,domain,1,2 nohz_full=1,2 rcu_nocbs=1,2
irqaffinity=<other CPUs> nosmt` to the kernel command line, runs `update-grub`, and reboots; the sibling threads and
one thread per core are then online (`smt/control` is `off`). Measured CPUs 1 and 2 (the writer and the reader; their
siblings 9 and 10 are offline), helpers are the remaining online CPUs. The Ubuntu 24.04 `7.0.0-1017-gcp` kernel
supports it on both architectures: `CONFIG_NO_HZ_FULL`, `CONFIG_RCU_NOCB_CPU` and `CONFIG_CPU_ISOLATION` are set
(checked on the x86-64 VM and in the aarch64 package), `CONFIG_HZ=1000`. The local SSD mounts do not survive a reboot,
so `rerun-vm.sh` mounts them again and the driver continues by itself after the reboot, from cron.

## Time and money

Estimated with the list prices of 2026-11-25 (Google Cloud's general-purpose price table, us-central1; the ledgers of
2026-11 used US$0.95/h and US$0.45/h for the same shapes): `c4-standard-16-lssd` US$0.955/h, `c4a-standard-8-lssd`
US$0.482/h. At A$1.55 per US$ (the rate the ledger has used) that is A$1.48/h and A$0.75/h.

Durations come from measurements, not from list prices: the W2 schedule is fixed (110 s per run at 10,000/s, 11 s at
100,000/s); the sustained runs take 600 s plus the 10 s warm-up plus 3 to 15 s in the dry run (613 s for tickertape,
617 s for Chronicle Queue with the old JVM settings, 625 s with the new ones, each one run); a short-mode unit of
Chronicle Queue took 1,117 s and of Aeron 1,070 s on the x86-64 machine, because the long JVM warm-up runs in every
process; queue-file's short unit took 17 s (it has no W2). `rerun.py --list` prints the estimate of every unit; unlike
the dry run's figures, the sustained units have a margin (15 s, 30 s for the JVM tools) and the aarch64 machine was
assumed to take as long as the x86-64 one. Measured for planning; none of them is a published result.

| Part | Hours | What it is |
|---|---|---|
| `short` (ext4) | 4.7 | 24 units, all tools; two rounds of the before/after rows |
| `sustained` (ext4) | 5.6 | headline tools, two rounds; one round is 6 runs of ten minutes for tickertape and Chronicle, 4 for Aeron |
| `sustained-90` (ext4) | 1.4 | |
| `sustained-jvm-old` (ext4) | 1.8 | |
| `short-pinned` (xfs) | 1.2 | |
| `sustained-journals` (xfs) | 2.1 | tickertape and Chronicle, one round |
| `sustained-90` (xfs) | 1.4 | |
| `sustained-others` (ext4) | 2.1 | |
| `tuned-short` (ext4) | 1.2 | after one reboot |
| `tuned-sustained` (ext4) | 2.8 | tickertape, Chronicle, Aeron, one round |
| set-up, builds, reboot | 0.5 | |
| **Total per machine** | **24.9** | |

| Machine | Hours | A$ |
|---|---|---|
| x86-64 | 24.9 | 36.9 |
| aarch64 | 24.9 | 18.7 |
| **Total** | | **55.6** |

Cap: A$60. The estimate leaves A$4.4 for retries, so the caps below matter. The VMs get `--max-run-duration` of 26 h
each, a deadline 20 minutes earlier in the driver, and at most A$38.5 (x86-64) + A$19.5 (aarch64) = A$58 if both run
to the limit. The driver starts no unit that would end after the deadline and lists it as not started, so an overrun
costs cells and not money. If the owner prefers a smaller bill, the fallback is:

**Cheaper fallback (about A$21.5, 9.6 h per machine):** the same parts and machines with `EXTRA="--units round1"`:
`short` (one round: pinned, unpinned, jvm-old, microtouch-only), `sustained` (round 1: tickertape, Chronicle, Aeron),
`sustained-journals` (xfs), and the `tuned-short` part. It leaves out the second round (so no repeats hours apart),
the 90 % runs, the sustained jvm-old rows, the xfs short mode, the other three tools' sustained rows, and the tuned
sustained rows. The README's 90 % sentence then stays as
it is, with its date.

**Data volume and disk.** A 10-minute run of 64 B records writes about 110 GB for tickertape at 1.77 million records/s
and 75 GB for Chronicle Queue at 1.5 million (measured in the dry run). Unpaced 1,024 B runs reach about 350 MB/s of
payload, so about 220 GB per ten minutes. The 375 GB SSDs suffice, because each run's data is deleted before the next.
The harness checks the predicted size before a run and skips it with a reason otherwise. Chronicle's 1 GiB `blockSize`
makes no difference to that: the files are sparse in their last block. The JVM logs were 5.5 MB for a ten-minute
Chronicle run and 19 MB for a short-mode unit; at the estimated number of units the results are well below 1 GB in
total, on the boot disk (`~/results`).

## Self-deletion, results and what happens to a lost machine

- Every VM has `--max-run-duration` with termination action DELETE (1560 minutes at most), a `--deadline` 20 minutes
  before it in the driver, and a watcher. `rerun-vm.sh` always writes `RESULTS/DONE` (with `ok` or the reason it is
  not), so the watcher copies everything and deletes the VM. If no watcher deleted it, the VM stops itself 60 minutes
  after DONE (`POWEROFF_AFTER_MIN`): the vCPUs stop costing money while the boot disk keeps the results.
- `~/tmp/tt-gcp/rerun/start.sh watch <name> <zone> <minutes>` restarts the watcher after the Mac restarted. It copies
  what changed as one tar stream every 10 minutes (on CPU 0, at nice 19), and deletes the VM only when `DONE` is there
  and every file has the same checksum here as on the VM: `scp --recurse` (`pull.sh`, `watch.sh`) takes minutes for the
  thousands of small result files of this run. In the dry run an `scp` of the eight quick-matrix directories had not
  finished after more than two minutes, and one `tar` over ssh took 8 seconds.
- A lost VM: the results up to the last pull are on the Mac under `results/rerun-<name>/`. Start a new VM with
  `--units`/`--part` for what is missing, and run the parts in order; the driver keeps finished units of one OUT only.

## Run it

The scripts are on the VM: `benches/rerun.py`, `benches/rerun-vm.sh`; the launch scripts on this Mac in
`~/tmp/tt-gcp/` (`launch.sh`, `provision.sh`, `ssh.sh`, `rerun/start.sh`).

```sh
cat > full.conf <<'C'     # x86-64; the same for aarch64 (tuned options are derived from ISOLATED on the VM)
PARTS="--part short:/data/tt --part sustained:/data/tt --part sustained-90:/data/tt --part sustained-jvm-old:/data/tt --part short-pinned:/data2/tt --part sustained-journals:/data2/tt --part sustained-90:/data2/tt --part sustained-others:/data/tt"
TUNED_PARTS="--part tuned-short:/data/tt --part tuned-sustained:/data/tt"
ISOLATED="1,2"            # one CPU each for the writer and the reader (on x86-64 their siblings are offline after nosmt)
C
./start.sh c4-standard-16-lssd tt-rerun-x86 <commit> full.conf 1560 us-central1-b us-east1-b us-east1-d
./start.sh c4a-standard-8-lssd tt-rerun-arm <commit> full.conf 1560 us-central1-b us-east1-b us-east1-d
```

Then nothing needs this Mac except the watcher, which only copies. `rerun-vm.sh status` on the VM prints the progress.
The commit must be on `origin` (the VM clones the public repository); `start.sh` fetches the tags, which the driver's
library check needs. Create and push the session branch of [Commit](#what-is-run) first.

**After the run.** Check that the driver ended with `ok` in `DONE`, that `rerun.json` has no unit that is not `done`,
and read `retries.jsonl` and `failures.jsonl` of every unit. Publishing is a separate change to `docs/benchmarks.md`
and `README.md`:

- **`docs/benchmarks.md`:** one new dated section, "Comparison rerun at v1.0.0", with the environment per machine
  (`environment-*.txt`: machine type, kernel, mount options, CPU layout with the sibling list, `/proc/cmdline` of both
  boots, commit and the driver's exact commands), the tables of the matrix, the sample spread, the failed and skipped
  cells, what is not comparable with older sections, and what the run does not answer. The older comparison sections
  stay, with a sentence that they are superseded and why (the harness changed: `compare/README.md`).
- **README → Performance:** the headline tables and sentences are replaced by the new ones, whatever they say: the
  short-run section (W1, W3 and W2 at v1.0.0 on the cloud machines; the bare-metal v0.4.0 figures stay only in
  `docs/benchmarks.md`, dated), and the ten-minute section. Sentences that the rerun does not support are removed: the
  sustained 90 % sentence and the "weaker comparison" caveat of the earlier sessions are replaced by what
  `sustained-90` and the single session give. The statements about different guarantees stay. The README names which
  commit and which harness (pinned, JVM warm-up) the numbers come from.
- **Reddit follow-up:** the owner posts it; the draft is written from the new tables.
- The dry-run numbers are not published, in `docs/benchmarks.md` or anywhere else.

## What the dry run showed

Machine `c4-standard-16-lssd` (us-central1-b), 2026-11-25, Ubuntu 24.04, kernel 7.0.0-1017-gcp. The tested code was PR
#65 and #67's head merged with the rerun driver, then `origin/main` 1e9846d (both PRs merged) with the same driver;
`compare/` and `benches/` were the same between the last two. Nothing measured there is a result.

- **Quick pinned matrix.** All tools, both drivers (short mode and `--sustained`), ext4 and xfs, with the automatic
  CPU choice, with named CPUs (writer 3, reader 5, helpers `0-2,8-10`), and with the two threads of one core named
  (writer 1, reader 9), plus `--no-pin`: every run finished. Automatic: writer 1, reader 2, helpers `0,3-8,11-15`, in
  the sibling layout of the machine (`1,9` is one core).
- **Placement.** An independent sampler that reads `/proc` every 5 ms and lists running threads, and knows nothing of
  the
  harness, saw the measured threads only on their CPU and every other thread on the helper CPUs, in the quick matrices
  and in five ten-minute runs (tickertape SD on #65 and on `1e9846d`, Chronicle SD on #65 alone, Chronicle SV on #67
  and on `1e9846d`). Exceptions: yaque's file watchers start on the reader's CPU and are moved (up to three per run,
  recorded in `moved_threads`), tickertape's helpers in `W1-cold`, both documented; and short-lived JVM threads
  (`Thread-0`, `Logging-Cleaner`) on a measured CPU, which are probably shutdown hooks: the harness does not judge
  threads after the read-back.
- **Isolated boot.** The reboot with `isolcpus`, `nohz_full`, `rcu_nocbs`, `irqaffinity` and `nosmt` for CPUs 1 and 2
  worked: the kernel reported CPUs 1-2 as isolated and SMT off, and all 258 results of the tuned runs record
  `isolated_cpus: [1, 2]`, writer 1, reader 2 and helpers `0,3-7` read back from `/proc`; the sampler saw the same.
  The automatic choice on that boot avoids the isolated CPUs (it picks 3 and 4, helpers `0,5-7`), as documented, so a
  tuned run must name them.
- **Ten-minute cells.** tickertape SD at 1.77 million records/s of 64 B took 613 s in all; Chronicle SV at 1.5 million
  624 s; their disk use, JVM logs (about 5.5 MB) and timeouts are in the disk paragraph above. Nothing timed out.
- **Full-size timing.** A short-mode unit of Chronicle took 1,117 s and of Aeron 1,070 s (the JVM warm-up runs in
  every process). The quick short matrix takes about 190 s with the new harness (about 40 s before #67).
- **The driver.** SIGTERM in a unit marked it interrupted, the next start kept the partial directory as
  `.incomplete-1`, ran the unit again and skipped the finished ones. The untuned parts ended, the VM rebooted itself,
  mounted its disks, continued on the isolated boot and wrote `DONE`. This was with `--quick` units; after SIGTERM of
  one tickertape unit no harness process was left (the check did not cover `tt-latency`).
- **aarch64 check** (`c4a-standard-8-lssd`, 19 minutes, at the session commit): quick units of `short` round 1,
  `sustained`, `sustained-others` and `tuned-short`. All 27 units finished, all 398 results read back writer 1, reader
  2 and helpers `0,3-7` (automatic choice), the sampler agreed, the tuned boot worked (`smt/control` is `notsupported`
  there, so `nosmt` does nothing), and `start.sh` pulled, compared the checksums and deleted the VM after `DONE`.
- **Not checked before the run.** Full-size short runs of tickertape, yaque and the plain file; a full-length tuned
  run; runs near saturation.
- **Sampler.** The sampler used for the placement checks is not part of the repository.

Defects found, with reproduction, in the PR description: one in the short mode's signal handling, one missing option.
Both are fixed since the run, without a change to what is measured: the short mode ends its children on every way out
and marks an unfinished run `.incomplete-N`, a guard process covers SIGKILL of the runner and of the driver, and
`run.py` has `--tools` in the short mode, which the driver now uses for its short units (the old
`rerun.py --short-tools TOOL -- ...` form of the recorded commands still runs the same thing). See "Signals, failures
and leftover processes" in `compare/README.md`.
