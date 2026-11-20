#!/usr/bin/env python3
"""Lines up trace events with tt-sustained's per-second series.

Part of docs/playbooks/diagnose-latency-with-ebpf.md. Reads OUT/series.jsonl and lists the seconds with the largest
writer append maximum (or the largest reader latency maximum with --by reader), each with its rolls, checkpoints,
dirty and write-back bytes and the readers' worst latency, followed by the trace events that started in that second
or the one before. A trace file is any text file whose event lines start with a Unix time in seconds, as the
scripts in benches/diagnostics print them (offcpu.bt, events.bt, stack-sampler.py); other lines are ignored, and
indented lines (stack frames) after an event line are kept with it when --stacks is given.

Python 3.9 or later, standard library only.

    python3 benches/diagnostics/align.py OUT/series.jsonl [TRACE...] [--top 10] [--by writer|reader] [--stacks] \
        [--match REGEX]
"""

import argparse
import json
import re
import sys

EVENT = re.compile(r"^(\d{9,11}\.\d+)\s+(.*)$")


def load_series(path):
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


def load_events(paths, stacks):
    events = []  # (t_unix_s, source, text)
    for path in paths:
        with open(path) as f:
            last = None
            for line in f:
                m = EVENT.match(line.strip())
                if m:
                    last = [float(m.group(1)), path, m.group(2)]
                    events.append(last)
                elif stacks and last is not None and line.startswith((" ", "\t")) and line.strip():
                    last[2] += "\n        " + line.strip()
    return sorted(events, key=lambda e: e[0])


def fmt(value, unit=""):
    if value is None:
        return "-"
    if isinstance(value, float) and not value.is_integer():
        return f"{value:.1f}{unit}"
    return f"{int(value)}{unit}"


def reader_max(line):
    values = [r.get("latency_max_us") for r in line.get("readers", []) if r.get("latency_max_us") is not None]
    return max(values) if values else None


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("series", help="tt-sustained's OUT/series.jsonl")
    parser.add_argument("traces", nargs="*", help="trace files with event lines starting with Unix seconds")
    parser.add_argument("--top", type=int, default=10, help="seconds listed (default 10)")
    parser.add_argument("--by", choices=["writer", "reader"], default="writer",
                        help="rank seconds by the writer's append maximum or the readers' latency maximum")
    parser.add_argument("--stacks", action="store_true", help="print the stack lines that follow an event")
    parser.add_argument("--match", help="keep only events whose line matches this regular expression, "
                        "e.g. 'tid=1234 ' or 'state=D'")
    args = parser.parse_args()

    series = load_series(args.series)
    if not series:
        sys.exit(f"{args.series}: no lines")
    events = load_events(args.traces, args.stacks)
    if args.match:
        pattern = re.compile(args.match)
        events = [e for e in events if pattern.search(e[2].split("\n")[0])]
    if args.traces:
        print(f"{len(events)} trace events from {len(args.traces)} file(s)")

    def rank(line):
        value = line["writer"].get("append_max_us") if args.by == "writer" else reader_max(line)
        return -1 if value is None else value

    worst = sorted(series, key=rank, reverse=True)[: args.top]
    for line in sorted(worst, key=lambda l: l["t_unix_s"]):
        w = line["writer"]
        second = line["t_unix_s"]
        print(
            f"{second} {line['t_utc']}  append_max {fmt(w.get('append_max_us'), ' us')}"
            f"  rolls {w.get('rolls', 0)} (max {fmt(w.get('roll_max_us'), ' us')})"
            f"  checkpoints {w.get('checkpoints', 0)} (max {fmt(w.get('checkpoint_max_us'), ' us')})"
            f"  readers' latency_max {fmt(reader_max(line), ' us')}"
            f"  dirty {fmt(w.get('dirty_bytes'), ' B')} writeback {fmt(w.get('writeback_bytes'), ' B')}"
        )
        for t, source, text in events:
            if second - 1 <= t < second + 1:
                print(f"    {t:.6f} {source}: {text}")


if __name__ == "__main__":
    main()
