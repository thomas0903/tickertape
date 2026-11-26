#!/usr/bin/env python3
"""Markdown tables for docs/benchmarks.md from issue #62 runs: python3 tables.py <results-dir> <arch-label>."""
import json, os, re, sys, statistics as st
from collections import defaultdict
root, arch = sys.argv[1], sys.argv[2]
CFGS = ["A", "D", "C", "B", "E", "F"]
LABEL = {"A": "default: prefault on, write-back on", "D": "prefault off, write-back on (exp)",
         "C": "`prefault(false)` (write-back off too)", "B": "prefault on, write-back off",
         "E": "prefault on, ranged sync (exp)", "F": "4 MiB window (exp)", "G": "default, no checkpoints"}
RATES = [("idle", "idle (1/s), 64 B"), ("r20k", "20,000/s, 64 B"), ("r200k", "200,000/s, 64 B"), ("k1", "20,000/s, 1,024 B")]
FS = ["ext4", "xfs"]

def load(d):
    r = json.load(open(os.path.join(d, "result.json")))
    b = [int(x) for x in open(os.path.join(d, "diskstat.before")).read().split()]
    a = [int(x) for x in open(os.path.join(d, "diskstat.after")).read().split()]
    return r, (a[6] - b[6]) * 512

def trace(path):
    rows = [l.split() for l in open(path) if l.startswith("S ")]
    if not rows: return None
    tids = defaultdict(int)
    for l in rows: tids[l[2]] += 1
    w = max(tids, key=tids.get)
    calls = [dict(end=int(l[1]), call=l[3].split("_")[-1], fd=int(l[4]), dur=int(l[6]), bio=int(l[7]), devw=int(l[8])) for l in rows if l[2] == w]
    for c in calls: c["start"] = c["end"] - c["dur"]
    groups, cur = [], []
    for c in calls:
        if cur and c["start"] - cur[-1]["end"] > 100_000_000: groups.append(cur); cur = []
        cur.append(c)
    if cur: groups.append(cur)
    out = []
    for g in groups:
        if not any(c["call"] == "msync" for c in g): continue  # the writer's open
        roles = defaultdict(lambda: [0, 0])
        for i, c in enumerate(g):
            if i == len(g) - 1: role = "marker"
            elif c["call"] == "msync": role = "msync"
            elif c["call"] == "fsync": role = "dir"
            elif i > 0 and g[i - 1]["call"] == "msync": role = "segsync"
            else: role = "barrier"
            roles[role][0] += c["devw"]; roles[role][1] += c["dur"]
        out.append(dict(roll=sum(c["call"] == "msync" for c in g) > 1, dur=g[-1]["end"] - g[0]["start"],
                        devw=sum(c["devw"] for c in g), roles=dict(roles)))
    return out

data = defaultdict(list); traced = {}
for fs in FS:
    p = os.path.join(root, fs)
    if not os.path.isdir(p): continue
    for name in os.listdir(p):
        m = re.match(r"(\w+)-([A-G])-r(\d+)$", name)
        if not m: continue
        d = os.path.join(p, name)
        try: r, dev = load(d)
        except Exception: continue
        key = (fs, m.group(1), m.group(2))
        if os.path.exists(os.path.join(d, "trace.txt")):
            traced[key] = (r, dev, trace(os.path.join(d, "trace.txt")))
        else:
            data[key].append((r, dev))

def rng(vals, fmt):
    vals = [v for v in vals if v is not None]
    if not vals: return "–"
    a, b = fmt(min(vals)), fmt(max(vals))
    return a if a == b else f"{a}–{b}"
def t_us(v):  # µs, ms above 1,000
    if v >= 1000: return f"{v/1000:.2f}".rstrip("0").rstrip(".") + " ms" if v < 10000 else f"{v/1000:.1f} ms"
    return f"{v:.0f}" if v >= 10 else f"{v:.1f}"
def kib(v): return f"{v:,.0f}"
def cell(fn):
    return " / ".join(fn(fs) for fs in FS)

print(f"**{arch}: checkpoint duration and bytes** (two untraced runs per cell; ranges over runs; ext4 / xfs)\n")
print("| Rate | Configuration | Checkpoint p50 | Checkpoint max | Device KiB per checkpoint | Appended KiB per checkpoint |")
print("|---|---|---|---|---|---|")
for rk, rl in RATES:
    for c in CFGS:
        rs = {fs: data.get((fs, rk, c), []) for fs in FS}
        if not any(rs.values()): continue
        n = lambda r: r["writer"]["checkpoints"] + 1
        print(f"| {rl} | {LABEL[c]} | " +
              cell(lambda fs: rng([r["writer"]["checkpoint_latency"]["p50_us"] for r, _ in rs[fs]], t_us)) + " | " +
              cell(lambda fs: rng([r["writer"]["checkpoint_latency"]["max_us"] for r, _ in rs[fs]], t_us)) + " | " +
              cell(lambda fs: rng([dev / n(r) / 1024 for r, dev in rs[fs]], kib)) + " | " +
              cell(lambda fs: rng([r["dataset"]["appended_bytes"] / n(r) / 1024 for r, _ in rs[fs]], kib)) + " |")
print()
print(f"**{arch}: latency during the same runs** (reader: the zero-copy reader, from the scheduled send; append: one `append` call; ext4 / xfs)\n")
print("| Rate | Configuration | Reader p50 | p99 | p99.9 | max | Append p99.9 | Append max | Writer-thread faults | Late sends (%) |")
print("|---|---|---|---|---|---|---|---|---|---|")
for rk, rl in RATES:
    for c in CFGS + ["G"]:
        rs = {fs: data.get((fs, rk, c), []) for fs in FS}
        if not any(rs.values()): continue
        rd = lambda r: r["readers"][0]["latency"]
        print(f"| {rl} | {LABEL[c]} | " + " | ".join(
            cell(lambda fs, k=k: rng([rd(r)[k] for r, _ in rs[fs]], t_us)) for k in ["p50_us", "p99_us", "p99_9_us", "max_us"]) + " | " +
            cell(lambda fs: rng([r["writer"]["append_latency"]["p99_9_us"] for r, _ in rs[fs]], t_us)) + " | " +
            cell(lambda fs: rng([r["writer"]["append_latency"]["max_us"] for r, _ in rs[fs]], t_us)) + " | " +
            cell(lambda fs: rng([r["writer"]["hot_thread_cpu"]["minor_faults"] for r, _ in rs[fs]], lambda v: f"{v:,}")) + " | " +
            cell(lambda fs: rng([100 * r["writer"]["late_records"] / r["writer"]["count"] for r, _ in rs[fs]], lambda v: f"{v:.2f}")) + " |")
print()
print(f"**{arch}: where a checkpoint's writes go** (one traced run per cell; medians over the checkpoints that cover one segment, KiB written to the device during each call / its time in µs; ext4 / xfs)\n")
print("| Rate | Configuration | Ranged `msync` | Segment `fdatasync` | Marker `fdatasync` | Whole checkpoint | Checkpoints with a roll: KiB |")
print("|---|---|---|---|---|---|---|")
for rk, rl in RATES[1:]:
    for c in CFGS:
        ts = {fs: traced.get((fs, rk, c)) for fs in FS}
        if not any(ts.values()): continue
        def role(fs, ro):
            t = ts[fs]
            if not t or not t[2]: return "–"
            steady = [g for g in t[2][:-1] if not g["roll"]][1:] or [g for g in t[2] if not g["roll"]]
            if not any(ro in g["roles"] for g in steady): return "–"
            k = st.median([g["roles"].get(ro, [0, 0])[0] / 1024 for g in steady])
            u = st.median([g["roles"].get(ro, [0, 0])[1] / 1000 for g in steady])
            return f"{k:,.0f} / {t_us(u)}"
        def whole(fs):
            t = ts[fs]
            if not t or not t[2]: return "–"
            steady = [g for g in t[2][:-1] if not g["roll"]][1:]
            return f"{st.median([g['devw'] / 1024 for g in steady]):,.0f} / {t_us(st.median([g['dur'] / 1000 for g in steady]))}"
        def rolls(fs):
            t = ts[fs]
            if not t or not t[2]: return "–"
            rr = [g["devw"] / 1024 for g in t[2][:-1] if g["roll"]]
            return rng(rr, kib) if rr else "–"
        print(f"| {rl} | {LABEL[c]} | " + " | ".join(" ; ".join(role(fs, ro) for fs in FS) for ro in ["msync", "segsync", "marker"]) +
              " | " + " ; ".join(whole(fs) for fs in FS) + " | " + " / ".join(rolls(fs) for fs in FS) + " |")
print()
print(f"**{arch}: idle writer, checkpoint by checkpoint** (traced; KiB written during each checkpoint, in order; ext4 / xfs)\n")
for c in CFGS:
    for fs in FS:
        t = traced.get((fs, "idle", c))
        if t and t[2]:
            print(f"- {fs} {LABEL[c]}: " + ", ".join(f"{g['devw']/1024:.0f}" for g in t[2][:6]) + " …")
print()
print(f"**{arch}: tracing check** (checkpoint p50 in µs, traced run against the untraced ones)\n")
for rk, rl in RATES:
    for c in ["A", "D", "E"]:
        print(f"- {rl}, {c}: " + " ; ".join(f"{fs} traced {t_us(traced[(fs, rk, c)][0]['writer']['checkpoint_latency']['p50_us'])} vs " +
              rng([r["writer"]["checkpoint_latency"]["p50_us"] for r, _ in data.get((fs, rk, c), [])], t_us)
              for fs in FS if (fs, rk, c) in traced))
