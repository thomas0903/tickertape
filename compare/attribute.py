#!/usr/bin/env python3
"""Attributes the late sends and slow receives of a JVM tool's run to GC, other safepoints or deoptimisations.

usage: attribute.py OUTDIR/jvm-logs/LABEL.events.json ...   (the .log beside each is read too)

For each child process: the safepoints, handshakes and level 4 (C2) deoptimisations in its measured window, and for
its episodes (late sends of the writer, slow receives of the reader) how many an event can explain. Only the time in
which an episode's worst delay built up counts: from 50 us before its record was due until it was sent or received.
Safepoints (or handshakes) explain it if what they take of that time is at least half of the delay; a deoptimisation if
it falls in that time and the delay is at most 5 ms. That is not proof of a cause, and "neither" includes everything
outside the JVM: page faults, storage, the scheduler.
A reader's slow receive can also be the writer's late send; compare both files.
"""
import json
from pathlib import Path
import re
import sys

LINE = re.compile(r"\[[^\]]*\]\[(\d+)ns\]\[([a-z,]+)\s*\] ?(.*)")
HANDSHAKE = re.compile(r'Handshake "([^"]+)".*?Total completion time: (\d+) ns')
SAFEPOINT = re.compile(r'Safepoint "([^"]+)".*?Reaching safepoint: (\d+) ns.*?Total: (\d+) ns')
DEOPT = re.compile(r"level=4 (\S+) trap_bci=\d+ (?:osr_bci=\d+ )?(\S+)")
# An event in one record's send delays the next record, which is due a little later.
SLACK_NS = 50_000
DEOPT_MAX_NS = 5_000_000
# VM operations whose safepoint is a collection.
GC = re.compile(r"GC|Collect|^G1")


def parse_log(text):
    """Pauses [(start, end, name, kind)] with kind gc, safepoint or handshake, deoptimisations [(time, method, reason)]
    and the times of all lines, as System.nanoTime() values."""
    pauses, deopts, times = [], [], []
    for line in text.splitlines():
        match = LINE.match(line)
        if not match:
            continue
        time, tag, message = int(match[1]), match[2], match[3]
        times.append(time)
        if tag == "safepoint" and (found := SAFEPOINT.search(message)):
            pauses.append((time - int(found[3]), time, found[1], "gc" if GC.search(found[1]) else "safepoint"))
        elif tag.startswith("handshake") and (found := HANDSHAKE.search(message)):
            pauses.append((time - int(found[2]), time, found[1], "handshake"))
        elif tag == "deoptimization" and (found := DEOPT.search(message)):
            deopts.append((time, found[1], found[2]))
    return pauses, deopts, times


def dropped_lines(text):
    """Messages -Xlog:async had to drop, or 0."""
    return sum(int(n) for n in re.findall(r"(\d+) messages dropped due to async logging", text))


def wall_to_nano(anchors):
    """Converts wall clock to System.nanoTime() between the two anchors (NTP may slew the wall clock), and how far
    their offset moved."""
    (wall0, nano0), (wall1, nano1) = anchors
    moved = (nano1 - wall1) - (nano0 - wall0)

    def convert(wall):
        share = (wall - wall0) / (wall1 - wall0) if wall1 > wall0 else 0
        return wall + (nano0 - wall0) + round(moved * share)
    return convert, moved


def summarize(events, text, slack=SLACK_NS):
    pauses, deopts, times = parse_log(text)
    begin, end = events["measured_from_nano"], events["end_nano"]
    inside = [p for p in pauses if p[1] >= begin and p[0] <= end]
    traps = [d for d in deopts if begin <= d[0] <= end]
    nano, moved = wall_to_nano(events["anchors"])
    counts = {"gc": 0, "safepoint": 0, "handshake": 0, "deoptimisation": 0, "neither": 0}
    worst = {}
    total = 0
    for _, _, _, max_ns, _, max_wall in events["episodes"]:
        stop = nano(max_wall)
        start = stop - max_ns - slack
        if stop < begin or start > end:
            continue
        total += 1
        stopped = {}
        for first, last, _, kind in inside:
            if (part := min(last, stop) - max(first, start)) > 0:
                stopped[kind] = stopped.get(kind, 0) + part
        if 2 * sum(stopped.values()) >= max_ns and stopped:
            kind = max(stopped, key=stopped.get)
        elif max_ns <= DEOPT_MAX_NS and any(start <= d[0] <= stop for d in traps):
            kind = "deoptimisation"
        else:
            kind = "neither"
        counts[kind] += 1
        worst[kind] = max(worst.get(kind, 0), max_ns)
    return {"role": events["role"], "window_s": (end - begin) / 1e9, "pauses": inside, "deopts": traps,
            "episodes": total, "episodes_dropped": events["episodes_dropped"], "counts": counts, "worst_ns": worst,
            "log_lines": sum(begin <= t <= end for t in times), "log_lines_dropped": dropped_lines(text),
            "clock_moved_ns": moved, "clock_warning": abs(moved) > slack, "begin": begin}


def report(path):
    log = path.with_name(path.name.replace(".events.json", ".log"))
    summary = summarize(json.loads(path.read_text()), log.read_text())
    print(f"{path.name}: {summary['role']}, window {summary['window_s']:.3f} s, "
          f"{summary['log_lines']} log lines in it")
    for kind in ("gc", "safepoint", "handshake"):
        lengths = [p[1] - p[0] for p in summary["pauses"] if p[3] == kind]
        print(f"  {kind} pauses {len(lengths)}" + (f", longest {max(lengths) / 1e3:.1f} us" if lengths else ""))
    print(f"  C2 deoptimisations {len(summary['deopts'])}")
    for time, method, reason in summary["deopts"]:
        print(f"    {(time - summary['begin']) / 1e9:9.3f} s  {reason}  {method}")
    print(f"  episodes {summary['episodes']} (not logged: {summary['episodes_dropped']}):")
    for kind, count in summary["counts"].items():
        print(f"    {kind}: {count}" + (f", worst {summary['worst_ns'][kind] / 1e3:.1f} us" if count else ""))
    if summary["clock_warning"]:
        print(f"  warning: the wall clock moved by {summary['clock_moved_ns'] / 1e3:.1f} us against the monotonic clock "
              f"during the run; the attribution assumes that it moved evenly")
    if summary["log_lines_dropped"]:
        print(f"  warning: the JVM dropped {summary['log_lines_dropped']} log messages: incomplete")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    for argument in sys.argv[1:]:
        report(Path(argument))
