#!/usr/bin/env python3
"""Samples one thread's kernel stack through /proc, without eBPF.

The fallback of docs/playbooks/diagnose-latency-with-ebpf.md when eBPF tools are not available. Every --interval-ms it
reads the thread's state and /proc/<pid>/task/<tid>/stack; when the thread is not runnable (state other than R) it
counts the stack, and it joins consecutive samples with the same stack into one wait. It prints the longest waits
with their wall-clock start (Unix seconds, to line up with tt-sustained's series.jsonl), then the sample counts by
stack. It stops after --seconds, when the thread ends, or on SIGINT or SIGTERM. A wait shorter than the interval can
be missed, and a wait's length is known to about one interval.

Reading another thread's kernel stack needs root (CAP_SYS_ADMIN) and a kernel built with CONFIG_STACKTRACE.
Python 3.9 or later, standard library only.

    sudo python3 benches/diagnostics/stack-sampler.py --pid PID [--tid TID] --seconds 60 --out stacks.txt
"""

import argparse
import collections
import signal
import sys
import time


def read_sample(pid, tid):
    base = f"/proc/{pid}/task/{tid}"
    with open(f"{base}/stat") as f:
        state = f.read().rsplit(")", 1)[1].split()[0]
    with open(f"{base}/stack") as f:
        text = f.read()
    # Lines look like "[<0>] jbd2_log_wait_commit+0xb4/0x120".
    frames = [line.split()[1].split("+")[0] for line in text.splitlines() if len(line.split()) > 1]
    return state, frames


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--pid", type=int, required=True, help="process id")
    parser.add_argument("--tid", type=int, help="thread id (default: the process's main thread, tid = pid)")
    parser.add_argument("--seconds", type=float, required=True, help="how long to sample")
    parser.add_argument("--interval-ms", type=float, default=2.0, help="time between samples (default 2)")
    parser.add_argument("--depth", type=int, default=14, help="frames kept per stack (default 14)")
    parser.add_argument("--top", type=int, default=25, help="waits and stacks listed (default 25)")
    parser.add_argument("--out", help="output file (default: standard output)")
    args = parser.parse_args()
    tid = args.tid if args.tid is not None else args.pid
    interval = args.interval_ms / 1000.0

    try:
        with open(f"/proc/{args.pid}/task/{tid}/comm") as f:
            comm = f.read().strip()
        read_sample(args.pid, tid)
    except PermissionError:
        sys.exit("reading /proc/<pid>/task/<tid>/stack needs root")
    except OSError as e:
        sys.exit(f"thread {tid} of process {args.pid}: {e}")

    counts = collections.Counter()
    waits = []  # [key, first sample (Unix s), last sample (Unix s)]
    current = None
    samples = 0
    end = time.time() + args.seconds
    stop = []
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, lambda *_: stop.append(True))  # stop early and still write the result
    while time.time() < end and not stop:
        now = time.time()
        try:
            state, frames = read_sample(args.pid, tid)
        except OSError:
            break  # the thread or process ended
        samples += 1
        if state != "R" and frames:
            key = (state, ";".join(frames[: args.depth]))
            counts[key] += 1
            if current and current[0] == key and now - current[2] < 10 * interval:
                current[2] = now
            else:
                if current:
                    waits.append(current)
                current = [key, now, now]
        elif current:
            waits.append(current)
            current = None
        time.sleep(interval)
    if current:
        waits.append(current)

    out = open(args.out, "w") if args.out else sys.stdout
    with out:
        out.write(
            f"pid {args.pid} tid {tid} ({comm}); {samples} samples every ~{args.interval_ms:g} ms; "
            f"stacks of samples when not runnable\n\n== longest waits (start in Unix seconds) ==\n"
        )
        for key, first, last in sorted(waits, key=lambda w: w[1] - w[2])[: args.top]:
            out.write(f"{first:.3f} {(last - first) * 1000 + args.interval_ms:9.1f} ms  state {key[0]}  {key[1]}\n")
        out.write("\n== sample counts by stack ==\n")
        for key, n in counts.most_common(args.top):
            out.write(f"{n:8d}  state {key[0]}  {key[1]}\n")


if __name__ == "__main__":
    main()
