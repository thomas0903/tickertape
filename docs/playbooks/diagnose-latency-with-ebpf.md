# Playbook: diagnose latency with eBPF

When a sustained run (`tt-sustained`, `benches/README.md`) or an application shows stalls, this playbook finds out
where the writer and reader threads wait: in the kernel's scheduler, page faults, filesystem and write-back, or block
I/O. It uses existing tools (bpftrace, bcc and its libbpf builds) and a few small scripts in `benches/diagnostics/`.
Diagnostics are optional: tickertape and its CLI need none of this, no elevated permissions and no collector.

Tracing results are for diagnosis. They never go in the documentation as figures; recorded results come from untraced
runs (`run-benchmark.md`), and conclusions are confirmed with tracing off (step 6).

## 1. Requirements

- **Linux** with eBPF and BTF: `ls /sys/kernel/btf/vmlinux` must exist, and the kernel configuration
  (`/boot/config-$(uname -r)`, or `/proc/config.gz`) must have `CONFIG_BPF_SYSCALL`, `CONFIG_BPF_EVENTS`,
  `CONFIG_KPROBES` and `CONFIG_DEBUG_INFO_BTF`. macOS has no equivalent here.
- **Packages** (Ubuntu and Debian names; other distributions name them differently):

  | Package | Provides | Needs |
  |---|---|---|
  | `bpftrace` | `bpftrace`, the scripts in `benches/diagnostics/`, tools such as `/usr/sbin/biolatency.bt` | BTF |
  | `libbpf-tools` | compiled bcc tools, named `<tool>-libbpf` (`offcputime-libbpf`, `runqlat-libbpf`, ...) | BTF |
  | `bpfcc-tools` | the Python bcc tools, named `<tool>-bpfcc` | headers for the running kernel (`linux-headers-$(uname -r)`), and CPU and memory to compile each tool with clang when it starts |
  | `bpftool` | `bpftool prog show`, the in-kernel cost of the probes (step 6) | |

  Prefer the `-libbpf` builds and bpftrace: they start at once and need no kernel headers. The `-bpfcc` tools print
  compiler warnings from the kernel headers when they start (`warning: multiple identical address spaces specified
  for type`); these are harmless, and the tool's output follows them.
- **Privileges:** every tracer runs as root (`sudo`). So does `stack-sampler.py`: reading another thread's
  `/proc/<pid>/task/<tid>/stack` needs root. `find-threads.sh` and `align.py` need none.
- **Containers and PID namespaces.** Inside a container (or an OrbStack machine) the ids that `ps` shows are not
  the kernel's. `offcpu.bt` and `events.bt` take the ids you see. The bcc and libbpf tools (`-p`, `-t`) and the
  scheduler tracepoints' `prev_pid`/`next_pid` use kernel-wide ids; translate them with (replace `PID`):

  ```bash
  sudo bpftrace -q -e 'profile:hz:99, tracepoint:sched:sched_switch { if (pid == PID) { @kernel_tid[tid, comm] = curtask->pid; } } interval:s:2 { exit(); }'
  ```

  It prints each thread that ran in those two seconds as `@kernel_tid[<tid you see>, <comm>]: <kernel-wide tid>`;
  the main thread's kernel-wide tid is the process's kernel-wide pid. On a virtual machine without containers the
  ids are the same.
- **Python** 3.9 or later for the scripts (standard library only).

## 2. Which tool answers which question

| Question | bpftrace or script | bcc (`-libbpf`, `-bpfcc`) |
|---|---|---|
| Where does a writer or reader thread wait, for how long, by kernel stack? Preempted (waiting for a CPU) or blocked? | `offcpu.bt` | `offcputime-libbpf -t TID -m 1000 30`, `offcputime-bpfcc -K -t TID -m 1000 30` |
| When does each checkpoint sync, write-back request and segment roll happen, on which thread, for how long? | `events.bt` | `fsslower-libbpf -t ext4 -p PID -m 1` (`-t xfs`, `btrfs`, `nfs`, `f2fs`; any file operation slower than 1 ms) |
| Is the device slow? | block I/O one-liner below, `/usr/sbin/biolatency.bt` | `biolatency-libbpf -D -m 10 1`, `biosnoop-libbpf -t -m 5` (each I/O slower than 5 ms, with its process) |
| Does the writer take page faults, and how long are they? Do readers take major faults (data not in RAM)? | page-fault one-liner below | `funclatency-libbpf -u -p PID handle_mm_fault` |
| Does a page fault or a file system call (create, `fallocate`, link) of a thread wait, and for which journal commit (ext4)? | `stalls.bt` | |
| Does write-back throttle the process (`balance_dirty_pages`)? | throttling one-liner below | |
| Does a thread wait in the run queue for a CPU? | run-queue one-liner below, `/usr/sbin/runqlat.bt` (whole system) | `runqlat-libbpf -L -p PID 5 1` (per thread), `runqslower-libbpf -p PID 1000` (each wait over 1 ms) |
| No eBPF at all | `stack-sampler.py` | |

The one-liners (replace `PID` and `READER_PID` with the process ids you see; each stops after the `interval`, or on
Ctrl-C):

```bash
# Block I/O latency by operation (R read, W write, S sync, F flush, M metadata), whole system.
sudo bpftrace -e 'tracepoint:block:block_rq_issue { @s[args.dev, args.sector] = nsecs; }
  tracepoint:block:block_rq_complete /@s[args.dev, args.sector]/ {
    @bio_us[args.rwbs] = hist((nsecs - @s[args.dev, args.sector]) / 1000); $d = delete(@s, (args.dev, args.sector)); }
  interval:s:30 { exit(); } END { clear(@s); }'

# Page-fault time per thread of one process, and major faults of a writer and a reader process.
sudo bpftrace -e 'kprobe:handle_mm_fault /pid == PID/ { @s[tid] = nsecs; }
  kretprobe:handle_mm_fault /@s[tid]/ { @fault_us[comm, tid] = hist((nsecs - @s[tid]) / 1000); $d = delete(@s, tid); }
  software:major-faults:1 /pid == PID || pid == READER_PID/ { @major[comm, tid] = count(); }
  interval:s:30 { exit(); } END { clear(@s); }'

# Write-back throttling: each pause of a thread of the process, in jiffies (CONFIG_HZ per second).
sudo bpftrace -e 'tracepoint:writeback:balance_dirty_pages /pid == PID/ { @pause_jiffies[comm, tid] = hist(args.pause); }
  interval:s:30 { exit(); }'

# Run-queue latency of every thread named tt-sustained (kernel-wide tids): time from wake-up or preemption to running.
sudo bpftrace -e 'tracepoint:sched:sched_wakeup, tracepoint:sched:sched_wakeup_new { @q[args.pid] = nsecs; }
  tracepoint:sched:sched_switch { if (args.prev_state == 0) { @q[args.prev_pid] = nsecs; }
    $t = @q[args.next_pid];
    if ($t != 0) { if (args.next_comm == "tt-sustained") { @runq_us[args.next_comm, args.next_pid] = hist((nsecs - $t) / 1000); }
      $d = delete(@q, args.next_pid); } }
  interval:s:30 { exit(); } END { clear(@q); }'
```

What to expect from tickertape's threads:

- The **writer's measuring thread** in `tt-sustained` is almost always on a CPU: paced, it busy-waits for each
  intended send time; unpaced, it appends without pause. Off-CPU time it has during the run is a stall: a sync, a
  fault it had to wait for, write-back throttling, or preemption (state `R` in `offcpu.bt`).
- A **paced writer pinned with `--cpus`** should show next to no run-queue latency; if it does, something else runs on
  its CPU (a kernel thread, another process, or a helper thread placed there with `--helper-cpus inherited`).
- A stall in `series.jsonl` with **no off-CPU time** in its second means the thread was on a CPU: a page fault handled
  without sleeping (the page-fault one-liner shows it), or, in a virtual machine, the host not running the virtual
  CPU, which no tracer inside the guest sees (watch steal time, `st` in `vmstat 1`).
- **Helper threads** (`tickertape-prefault`, `tickertape-writeback`, `tickertape-checkpoint`) sleep on futexes most of
  the time; that is their normal off-CPU time, not a stall. A background checkpoint's syncs appear on
  `tickertape-chec` (thread names are cut to 15 characters).
- **Readers** that have caught up sleep by design unless they spin: `wait=block` blocks at once, `wait=default`
  after a few spins and yields, `wait=sleep` sleeps in short steps. A reader's stall shows as off-CPU time in state
  `D`, or as major faults when it reads data that is no longer in the page cache.

## 3. Find the writer and reader threads

**`tt-sustained`.** One writer process and one process per reader; the thread that appends (or reads) is each
process's main thread, so its tid is the process id. While a run is going:

```bash
benches/diagnostics/find-threads.sh
```

prints every thread of those processes with its role (`writer: appends`, `reader N: reads`, the helper threads, the
`tt-sampler` thread) and its CPU time so far.

**An application.** `benches/diagnostics/find-threads.sh PID` lists its threads with their CPU time. The tickertape
helper threads are named; the thread that calls `append` is the application's own, so give it a name
(`std::thread::Builder::name`, `pthread_setname_np`, `Thread.setName` in Java) to find it by `comm`. A busy-polling
writer or reader is the thread with the most CPU time (also `top -H -p PID`). Run the script twice a few seconds apart
to see which threads are accumulating CPU time.

## 4. Run a traced session

`trace-sustained.sh` starts `tt-sustained`, attaches tracers to the writer process as soon as it starts, saves
`find-threads.sh`'s output, and stops the tracers when the run ends:

```bash
cargo build -p tickertape-bench --release --bin tt-sustained
J=/mnt/test/tt        # a directory on the filesystem under test
benches/diagnostics/trace-sustained.sh ~/trace/visibility events,offcpu -- \
  target/release/tt-sustained run --dir "$J/visibility" --out ~/trace/visibility/out --rate 50000 --payload 1024 \
  --duration 300s --warmup 10s --readers mapped,safe:wait=default
```

Tools: `events`, `offcpu` (each wait of at least `MIN_MS`, default 20 ms, is listed with its stack), `sampler` (no
eBPF), or `none` (the same run untraced, step 6). The trace directory gets `events.txt`, `offcpu.txt`, `sampler.txt`,
`threads.txt`, `tracer-cpu.txt` and the run's own output. Take the `tt-sustained` arguments from the scenario you are
investigating (`python3 benches/session.py --help` lists the session's scenarios; `result.json` has each run's
arguments).

The tools in step 2 attach to a process that is already running, `tt-sustained` or an application:

```bash
sudo bpftrace benches/diagnostics/offcpu.bt PID 20 60 > offcpu.txt    # 60 s; waits of 20 ms or more listed
sudo bpftrace benches/diagnostics/events.bt PID 60 > events.txt
sudo python3 benches/diagnostics/stack-sampler.py --pid PID --tid TID --seconds 60 --out stacks.txt
sudo TZ=UTC runqslower-libbpf -p KERNEL_PID 1000                       # times in UTC, as series.jsonl's t_utc
```

## 5. Line traces up with the series and with checkpoints and rolls

`OUT/series.jsonl` has one line per wall-clock second (`t_unix_s`, `t_utc`), with the writer's `append_max_us`,
`rolls`, `roll_max_us`, `checkpoints`, `checkpoint_max_us` (the writer thread's time in the checkpoint call: the whole
checkpoint when blocking, the request in background mode), `dirty_bytes` and `writeback_bytes` (the whole system's,
from `/proc/meminfo`), and each reader's `latency_max_us`. Operations are attributed to the second in which they
started, so a writer stall shows in the second it began and readers' latency where they saw it. The timestamps are
`CLOCK_REALTIME`; `offcpu.bt`, `events.bt` and `stack-sampler.py` print the start of each event in Unix seconds from
the same clock (bpftrace's `strftime("%s.%f", nsecs)`).

```bash
python3 benches/diagnostics/align.py ~/trace/visibility/out/series.jsonl \
  ~/trace/visibility/offcpu.txt ~/trace/visibility/events.txt --top 10 --match 'comm=tt-sustained '
```

lists the ten seconds with the largest append maximum (`--by reader` ranks by the readers' latency instead), each with
its rolls, checkpoints, page-cache figures and reader latency, followed by every trace event that started in that
second or the one before (`--stacks` adds the stacks). `--match` keeps the lines you want: the writer's thread
(`comm=tt-sustained ` or `tid=N `), blocked waits only (`state=D`), or one call (`fdatasync`).

To place checkpoints and rolls exactly, use `events.bt`:

- **Rolls:** the writer publishes each new segment by linking its file under its name, so every `linkat` from the
  writer's thread is a roll.
- **Checkpoints:** `msync` and `fdatasync` calls (and `fsync` on directories) on the writer's thread for
  `Writer::checkpoint`, on `tickertape-chec` for background checkpoints (`Writer::request_checkpoint`, ADR 0038). The
  writer's `series.jsonl` gives the second, `checkpoint_max_us` the time on the writer's thread, and `events.bt` each
  call with its duration.
- **Early write-back:** `sync_file_range` on `tickertape-writ`.

`fsslower` shows file names: a segment created at a roll can appear under the temporary name it was opened with
(`<base>.tape.tmp`).

## 6. Measure the tracing overhead, and confirm with tracing off

Tracing changes what it measures. `offcpu.bt` runs at every context switch on every CPU, `events.bt` at every call of
its system calls in the whole system, the page-fault one-liner at every page fault in the whole system (each filters
by process inside the probe), and
`stack-sampler.py` wakes up every 2 ms. Measure what that costs on the machine at hand:

1. **Alternate traced and untraced runs** of the same command on the same machine, at least two of each, with new
   `--dir` and `--out` directories every time: `trace-sustained.sh DIR none -- ...` and
   `trace-sustained.sh DIR events,offcpu -- ...`. For the cost on throughput, use an `--unpaced` run.
2. **Compare the results** (`result.json`): the writer's `achieved_records_per_s` (unpaced), `late_records`,
   `max_schedule_delay_us`, `append_latency` and `roll_append_latency` percentiles, `hot_thread_cpu`, and each reader's
   `latency`. Differences between the untraced runs show the noise; only differences beyond it are tracing.
3. **The tracers' own CPU:** `tracer-cpu.txt` has each tracer process's user and system time. The probes themselves
   run inside the kernel, charged to whatever task triggered them; measure them with
   `sudo sysctl -w kernel.bpf_stats_enabled=1`, then `sudo bpftool prog show` while the tracer runs (`run_time_ns` and
   `run_cnt` per program), then `sudo sysctl -w kernel.bpf_stats_enabled=0`.

**Measured once** (2026-11-25, `docs/benchmarks.md` → "2026-11-25, roll and journal stalls", tracing overhead): with
`offcpu.bt` (`MIN_MS` 3), `events.bt`, `handle_mm_fault` and file system calls (`stalls.bt` of that session) and every
block request attached at the same time (bpftrace 0.20.2, kernel 7.0.0-1017-gcp), the writer's achieved rate, p50 and
stall counts were unchanged, and its p99.9 rose by about 0.5 µs (at 90 % of the probe) to 1.1 µs (unpaced). That is one tool set on two
machines; measure yours.

**Confirm every conclusion with tracing off.** A trace explains *why*; the evidence that the explanation is right must
come from untraced runs. State what the explanation predicts in `series.jsonl` or `result.json` and check it there:
for example, "the stalls are the roll's sync" predicts that the worst seconds of an untraced run are seconds with a
roll, with `roll_max_us` close to `append_max_us`, and that `roll_append_latency` carries the tail. Judge a fix the
same way: untraced runs before and after, on the same machine (`run-benchmark.md`).

## 7. Worked example: the roll's `fdatasync`

This investigation led to the change in pull request #28 (ADR 0038, "Rolls without a sync").

**Symptom.** In sustained runs on a cloud Linux virtual machine (x86-64, local NVMe, ext4 and xfs, 1,024 B records at
half the rate the machine could sustain), the writer's worst appends were roll appends, also in runs with no
checkpoints at all.

**Tracing.** During that run without checkpoints, `stack-sampler.py`'s predecessor sampled the writer's measuring
thread every 2 ms, and `offcputime-bpfcc -K -m 1000 -p PID` summed the process's off-CPU time by kernel stack.

**What the stacks showed.** On ext4 the longest uninterrupted waits of the writer's thread were all inside
`fdatasync`, waiting for the ext4 journal to commit:

```
jbd2_log_wait_commit;jbd2_complete_transaction;ext4_fc_commit;ext4_sync_file;do_fsync;__x64_sys_fdatasync
```

The next longest were the same `fdatasync` writing back the file's dirty pages and waiting in the block layer's
write-back throttling (`wbt_wait`):

```
rq_qos_wait;wbt_wait;__rq_qos_throttle;blk_mq_submit_bio;...;ext4_writepages;do_writepages;filemap_writeback;file_write_and_wait_range
```

On xfs the waits were the same `fdatasync` forcing the xfs log (`xlog_wait_on_iclog`), waiting for page write-back
(`folio_wait_writeback`), and the same `wbt_wait` under `xfs_vm_writepages`. `offcputime` agreed: apart from the
sleeps of the helper threads and of `tt-sampler`, the writer process's largest off-CPU totals were these `fdatasync`
stacks on the main thread.

**Whose sync.** With no checkpoints, the only `fdatasync` on the writer's thread was the one at a roll: the new
segment was synced after its header was written and before it was linked under its name, so that a name on disk
implied a durable header. At every roll that sync waited for the filesystem's log (the ext4 journal commit, the xfs
log force) and for write-back of the file's dirty pages, while the sustained write load kept the device busy.

**The fix.** Pull request #28 removed that sync from the writer's thread: each checkpoint now syncs every segment
linked before it, before it writes its marker, and recovery restores the header of a segment whose name reached the
disk without it. The exact rules are in the crash contract (C7, C8, C11) and in ADR 0038; the cost a roll still has
shows in `tt-sustained`'s `roll_append_latency`.

**Reproducing it.** Build `tt-sustained` at the commit before #28 (`git checkout 279f59a~1`), run a scenario without
checkpoints with `events,offcpu`, and look at the writer's thread with `align.py ... --match 'comm=tt-sustained '`:
every roll is an `fdatasync` immediately followed by the `linkat` that publishes the segment, with the thread's
off-CPU waits inside the `fdatasync`. On the current code the same run shows the `linkat` alone. How long the sync
waits depends on the filesystem and the device; the ext4 journal-commit waits need ext4 under a sustained write load.

## Notes

- `offcputime-bpfcc` needs headers matching the running kernel, and attaches a kprobe to `finish_task_switch`; on a
  kernel where that function is not traceable it stops with `Failed to attach BPF program b'oncpu' to kprobe`. Use
  `offcputime-libbpf` or `offcpu.bt` there. `offcputime-libbpf` prints `failed to get syms` for user-space stacks it
  cannot symbolize; the kernel stacks are still printed.
- The scripts do not parse on bpftrace 0.20.2 (Ubuntu 24.04's package): its `delete` takes one argument,
  `delete(@map[key])`, and is a statement. Rewriting those calls (`$d = delete(@t, tid)` to `delete(@t[tid])`) and the
  `args.x` accesses to `args->x` was enough for `offcpu.bt`, `events.bt` and `stalls.bt`; a newer release build of
  bpftrace needs no change.
- bpftrace 0.25 warns `Return value discarded` when `delete()`'s result is unused; the scripts assign it to a variable.
- Inside a container, `offcpu.bt` can print a few `get_ns_current_pid_tgid` warnings when it starts (tasks of other
  namespaces, before it has seen the target process); `events.bt` can print them when such tasks make the traced
  calls. Neither affects the results for the target process.
