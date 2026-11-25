#!/usr/bin/env python3
"""Attributes the late sends and slow receives of a JVM tool's run to GC, other safepoints or deoptimisations.

usage: attribute.py OUTDIR/jvm-logs/LABEL.events.json ...   (the .log beside each is read too)

For each child process: the safepoints and level 4 (C2) deoptimisations in its measured window, and for its
episodes (late sends of the writer, slow receives of the reader) how many an event can explain: safepoints that
overlap the episode, or end less than 50 us before it, and together take at least half of its longest delay; or a
deoptimisation in the same place, if the delay is at most 5 ms. That is not proof of a cause, and "neither" includes
everything outside the JVM: page faults, storage, the scheduler.
A reader's slow receive can also be the writer's late send; compare both files.
"""
import json
from pathlib import Path
import re
import sys

LINE = re.compile(r"\[[^\]]*\]\[(\d+)ns\]\[([a-z,]+)\s*\] ?(.*)")
SAFEPOINT = re.compile(r'Safepoint "([^"]+)".*?Reaching safepoint: (\d+) ns.*?Total: (\d+) ns')
DEOPT = re.compile(r"level=4 (\S+) trap_bci=\d+ (?:osr_bci=\d+ )?(\S+)")
# An event in one record's send delays the next record, which is due a little later.
SLACK_NS = 50_000
DEOPT_MAX_NS = 5_000_000
# VM operations whose safepoint is a collection.
GC = re.compile(r"GC|Collect|^G1")


def parse_log(text):
    """(safepoints, deopts, times): [(start, end, name, is_gc)], [(time, method, reason)] and the times of all lines,
    System.nanoTime() values."""
    safepoints, deopts, times = [], [], []
    for line in text.splitlines():
        match = LINE.match(line)
        if not match:
            continue
        time, tag, message = int(match[1]), match[2], match[3]
        times.append(time)
        if tag == "safepoint" and (found := SAFEPOINT.search(message)):
            safepoints.append((time - int(found[3]), time, found[1], bool(GC.search(found[1]))))
        elif tag == "deoptimization" and (found := DEOPT.search(message)):
            deopts.append((time, found[1], found[2]))
    return safepoints, deopts, times


def dropped_lines(text):
    """Messages -Xlog:async had to drop, or 0."""
    return sum(int(n) for n in re.findall(r"(\d+) messages dropped due to async logging", text))


def episode_intervals(events):
    """The time each episode covers: from the moment its first record was due until its last record."""
    shift = events["anchor_nano"] - events["anchor_wall_ns"]
    for first_seq, start, records, max_ns, end in events["episodes"]:
        yield start - max_ns + shift, end + shift, first_seq, records, max_ns


def summarize(events, text, slack=SLACK_NS):
    safepoints, deopts, times = parse_log(text)
    begin, end = events["measured_from_nano"], events["end_nano"]
    inside = [s for s in safepoints if s[1] >= begin and s[0] <= end]
    traps = [d for d in deopts if begin <= d[0] <= end]
    counts = {"gc": 0, "safepoint": 0, "deoptimisation": 0, "neither": 0}
    worst = {}
    total = 0
    for start, stop, _, _, max_ns in episode_intervals(events):
        if stop < begin or start > end:
            continue
        total += 1
        overlapping = [s for s in inside if s[1] >= start - slack and s[0] <= stop]
        stopped = sum(s[1] - s[0] for s in overlapping)
        if overlapping and 2 * stopped >= max_ns:
            kind = "gc" if any(s[3] for s in overlapping) else "safepoint"
        elif max_ns <= DEOPT_MAX_NS and any(start - slack <= d[0] <= stop for d in traps):
            kind = "deoptimisation"
        else:
            kind = "neither"
        counts[kind] += 1
        worst[kind] = max(worst.get(kind, 0), max_ns)
    return {"role": events["role"], "window_s": (end - begin) / 1e9, "safepoints": inside, "deopts": traps,
            "episodes": total, "episodes_dropped": events["episodes_dropped"], "counts": counts, "worst_ns": worst,
            "log_lines": sum(begin <= t <= end for t in times), "log_lines_dropped": dropped_lines(text), "begin": begin}


def report(path):
    log = path.with_name(path.name.replace(".events.json", ".log"))
    summary = summarize(json.loads(path.read_text()), log.read_text())
    print(f"{path.name}: {summary['role']}, window {summary['window_s']:.3f} s")
    gcs = [s for s in summary["safepoints"] if s[3]]
    longest = max((s[1] - s[0] for s in summary["safepoints"]), default=0)
    print(f"  safepoints {len(summary['safepoints'])} (of them GC {len(gcs)}), longest {longest / 1e3:.1f} us; "
          f"log lines in the window {summary['log_lines']}")
    print(f"  C2 deoptimisations {len(summary['deopts'])}")
    for time, method, reason in summary["deopts"]:
        print(f"    {(time - summary['begin']) / 1e9:9.3f} s  {reason}  {method}")
    print(f"  episodes {summary['episodes']} (not logged: {summary['episodes_dropped']}):")
    for kind, count in summary["counts"].items():
        print(f"    {kind}: {count}" + (f", worst {summary['worst_ns'][kind] / 1e3:.1f} us" if count else ""))
    if summary["log_lines_dropped"]:
        print(f"  the JVM dropped {summary['log_lines_dropped']} log messages: incomplete")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    for argument in sys.argv[1:]:
        report(Path(argument))
