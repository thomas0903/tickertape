#!/usr/bin/env python3
"""Summarise issue #62 runs: python3 summ.py <results-dir> (the dir holding ext4/, xfs/)."""
import json, os, re, sys, statistics as st
from collections import defaultdict

root = sys.argv[1]
CFG = {"A": "prefault on, write-back on (default)", "B": "prefault on, write-back off", "C": "prefault off (write-back off)",
       "D": "prefault off, write-back on (exp)", "E": "prefault on, ranged sync (exp)", "F": "prefault 4 MiB window (exp)",
       "G": "default, no checkpoints"}
RATES = ["idle", "r20k", "r200k", "k1"]

def diskstat(path):
    return [int(x) for x in open(path).read().split()]

def checkpoints_from_trace(path):
    lines = [l.split() for l in open(path) if l.startswith("S ")]
    if not lines:
        return None, {}
    tids = defaultdict(int)
    for l in lines:
        tids[l[2]] += 1
    wtid = max(tids, key=tids.get)
    calls = []
    for l in lines:
        if l[2] != wtid:
            continue
        end, call, fd, ln, dur, bio, devw, devn, fl = int(l[1]), l[3].split("_")[-1], int(l[4]), int(l[5]), int(l[6]), int(l[7]), int(l[8]), int(l[9]), int(l[10])
        calls.append(dict(start=end - dur, end=end, call=call, fd=fd, len=ln, dur=dur, bio=bio, devw=devw, flushes=fl))
    groups, cur = [], []
    for c in calls:
        if cur and c["start"] - cur[-1]["end"] > 100_000_000:
            groups.append(cur); cur = []
        cur.append(c)
    if cur:
        groups.append(cur)
    dirt = {}
    for l in open(path):
        m = re.match(r"@dirtied\[(.*), (.*)\]: (\d+)", l)
        if m:
            dirt[(m.group(1), m.group(2))] = int(m.group(3))
    return groups, dirt

def q(vals, f):
    return f(vals) if vals else None

runs = defaultdict(list)
for fs in sorted(os.listdir(root)):
    p = os.path.join(root, fs)
    if not os.path.isdir(p) or fs not in ("ext4", "xfs", "btrfs"):
        continue
    for name in sorted(os.listdir(p)):
        m = re.match(r"(\w+)-([A-G])-r(\d+)$", name)
        if not m:
            continue
        d = os.path.join(p, name)
        try:
            r = json.load(open(os.path.join(d, "result.json")))
            open(os.path.join(d, "diskstat.after")).close()
        except Exception as e:
            print("FAILED", fs, name, e); continue
        w = r["writer"]; rd = r["readers"][0]["latency"]
        b, a = diskstat(os.path.join(d, "diskstat.before")), diskstat(os.path.join(d, "diskstat.after"))
        rec = dict(fs=fs, rate=m.group(1), cfg=m.group(2), round=int(m.group(3)), w=w, rd=rd,
                   dev_bytes=(a[6] - b[6]) * 512, dev_writes=a[4] - b[4], dev_flushes=(a[15] - b[15]) if len(a) > 15 else None,
                   appended=r["dataset"]["appended_bytes"], ckpts=w.get("checkpoints", 0), traced=os.path.exists(os.path.join(d, "trace.txt")))
        if rec["traced"]:
            rec["groups"], rec["dirtied"] = checkpoints_from_trace(os.path.join(d, "trace.txt"))
        runs[(fs, m.group(1), m.group(2))].append(rec)

def rng(vals, fmt="{:.0f}"):
    vals = [v for v in vals if v is not None]
    if not vals:
        return "–"
    lo, hi = min(vals), max(vals)
    return fmt.format(lo) if fmt.format(lo) == fmt.format(hi) else fmt.format(lo) + "–" + fmt.format(hi)

def us(v):
    return v if v is None else v

print("# per cell: n, ckpt p50/max (us), dev bytes per ckpt (KiB, whole run incl. final), appended per ckpt (KiB), reader p50/p99/p99.9/max (us), append p99.9/max (us), writer-thread minor faults, late %")
for (fs, rate, cfg), rs in sorted(runs.items(), key=lambda k: (k[0][0], RATES.index(k[0][1]) if k[0][1] in RATES else 9, k[0][2])):
    ck = [x["w"]["checkpoint_latency"] for x in rs if x["w"].get("checkpoint_latency")]
    n_ck = lambda x: x["ckpts"] + 1
    print(f"{fs:5} {rate:5} {cfg} {CFG[cfg]:40} n={len(rs)} "
          f"ckpt p50 {rng([c['p50_us'] for c in ck])} p99 {rng([c['p99_us'] for c in ck])} max {rng([c['max_us'] for c in ck])} | "
          f"devKiB/ckpt {rng([x['dev_bytes'] / n_ck(x) / 1024 for x in rs])} appKiB/ckpt {rng([x['appended'] / n_ck(x) / 1024 for x in rs])} "
          f"flush/ckpt {rng([x['dev_flushes'] / n_ck(x) for x in rs if x['dev_flushes'] is not None], '{:.1f}')} | "
          f"rd p50 {rng([x['rd']['p50_us'] for x in rs], '{:.1f}')} p99 {rng([x['rd']['p99_us'] for x in rs], '{:.1f}')} p99.9 {rng([x['rd']['p99_9_us'] for x in rs])} max {rng([x['rd']['max_us'] for x in rs])} | "
          f"app p99.9 {rng([x['w']['append_latency']['p99_9_us'] for x in rs], '{:.1f}')} max {rng([x['w']['append_latency']['max_us'] for x in rs])} | "
          f"wflt {rng([x['w']['hot_thread_cpu']['minor_faults'] for x in rs])} late% {rng([100 * x['w']['late_records'] / x['w']['count'] for x in rs], '{:.2f}')}")

print("\n# traced runs: per checkpoint (writer thread's sync calls), KiB written to the device during the calls; first = first checkpoint after open; roll = checkpoints with two msyncs")
for (fs, rate, cfg), rs in sorted(runs.items(), key=lambda k: (k[0][0], RATES.index(k[0][1]) if k[0][1] in RATES else 9, k[0][2])):
    for x in rs:
        if not x["traced"] or not x.get("groups"):
            continue
        gs = x["groups"]
        # drop the final checkpoint (after the last record) from steady stats but keep first
        def kib(g): return sum(c["devw"] for c in g) / 1024
        def durs(g): return (g[-1]["end"] - g[0]["start"]) / 1000
        first = gs[0]
        rolls = [g for g in gs[1:] if sum(1 for c in g if c["call"] == "msync") > 1]
        steady = [g for g in gs[1:-1] if sum(1 for c in g if c["call"] == "msync") == 1]
        # within steady checkpoints: bytes of each call position
        pos = defaultdict(list)
        for g in steady:
            for i, c in enumerate(g):
                pos[(i, c["call"])].append(c["devw"] / 1024)
        posdesc = " ".join(f"{i}:{call} {st.median(v):.0f}" for (i, call), v in sorted(pos.items()))
        posdur = defaultdict(list)
        for g in steady:
            for i, c in enumerate(g):
                posdur[(i, c["call"])].append(c["dur"] / 1000)
        durdesc = " ".join(f"{i}:{call} {st.median(v):.0f}" for (i, call), v in sorted(posdur.items()))
        dirt = {k: v for k, v in x["dirtied"].items() if not k[1].startswith("btrfs") or True}
        dpre = sum(v for (c, b), v in x["dirtied"].items() if c.startswith("tickertape-pref"))
        dw = sum(v for (c, b), v in x["dirtied"].items() if c.startswith("tt-sustained"))
        print(f"{fs:5} {rate:5} {cfg} n_ckpt={len(gs)} first {kib(first):.0f} KiB {durs(first):.0f} us ({len(first)} calls) | "
              f"roll {rng([kib(g) for g in rolls])} KiB {rng([durs(g) for g in rolls])} us (n={len(rolls)}) | "
              f"steady median {q([kib(g) for g in steady], st.median) or 0:.0f} KiB [{rng([kib(g) for g in steady])}] dur med {q([durs(g) for g in steady], st.median) or 0:.0f} us | "
              f"per call KiB {posdesc} | per call us {durdesc} | pages dirtied: prefault {dpre} writer {dw}")
