#!/usr/bin/env python3
"""One-off export: the values the comparison charts plot, from the raw results of the rerun at v1.0.0.

The raw results (one directory per machine, as `benches/rerun.py` left them) are not in the repository. This script
was run once to write `data/rerun-v1.0.0.json`; it is kept so that the export can be checked and repeated by someone
who has the raw results:

    python3 docs/charts/export_rerun.py <raw x86-64 dir> <raw aarch64 dir> > docs/charts/data/rerun-v1.0.0.json

Each raw directory is the one that holds `results/rerun/rerun.json`. Only units that `rerun.json` marks done are read
(the failed sustained units are read for the cells that completed), as for the published tables. Only the cells of the
README's tables are exported (ext4, `pinned`), and for the
ten-minute chart of docs/benchmarks.md also the SV runs on xfs (`sustained-journals`) and the plain file's SV runs on ext4
(`sustained-others`). Values are the raw floats of the result files, not rounded.
"""
import glob
import json
import os
import sys

SECTION = "2026-11-25-to-2026-11-26-comparison-rerun-at-v100-google-cloud-vms-linux-x86-64-and-aarch64"
TOOL = {"tickertape": "tickertape", "chronicle": "Chronicle Queue", "aeron": "Aeron IPC", "yaque": "yaque",
        "plain-file": "plain file", "queue-file": "queue-file"}
CONFIGS = ("microtouch-only", "jvm-old", "unpinned", "pinned", "tuned")


def load(root):
    root = os.path.join(root, "results", "rerun")
    manifest = json.load(open(os.path.join(root, "rerun.json")))
    short, sustained = [], []
    for label, unit in manifest["units"].items():
        if unit["status"] != "done" and not (unit["status"] == "failed" and "/sustained" in label):
            continue
        part, name = label.split("/")
        kind, body = name.split("-", 1)
        rnd = int(body.rsplit("-round", 1)[1])
        body = body.rsplit("-round", 1)[0]
        config = next(c for c in CONFIGS if body.startswith(c + "-"))
        meta = dict(part=part.rsplit("-", 1)[0], fs=part.rsplit("-", 1)[1], config=config,
                    tool=body[len(config) + 1:], round=rnd)
        d = os.path.join(root, label)
        if kind == "short":
            for f in sorted(glob.glob(d + "/*-W*.jsonl")):
                for line in open(f):
                    short.append((meta, json.loads(line)))
        else:
            for f in sorted(glob.glob(d + "/runs/*.json")):
                sustained.append((meta, json.load(open(f))))
    return manifest, short, sustained


def main(x86_dir, arm_dir):
    cells = {}

    def add(key, value, **fields):
        cell = cells.setdefault(key, dict(fields, runs=[]))
        cell["runs"].append(value)

    commits = {}
    for machine, root in (("x86-64", x86_dir), ("aarch64", arm_dir)):
        manifest, short, sustained = load(root)
        commits[machine] = manifest["identity"]["commit"]
        for m, r in short:
            if m["fs"] != "ext4" or m["config"] != "pinned":
                continue
            tool = TOOL[m["tool"]]
            if r["workload"] in ("W1-cold", "W1-steady"):
                if m["tool"] not in ("tickertape", "chronicle"):
                    continue  # the README table has these two; the others are in a sentence under it
                add(("throughput", r["workload"], machine, r["payload"], tool, None),
                    r["append"]["records_per_s"] / 1e6, chart="throughput", workload=r["workload"],
                    machine=machine, payload=r["payload"], tool=tool, reader=None, unit="million records/s")
            elif r["workload"] == "W3":
                if m["tool"] not in ("tickertape", "chronicle", "yaque"):
                    continue
                reader = {"default": None, "safe": "safe", "mapped": "zero-copy"}[r["reader"]] if m["tool"] == "tickertape" else None
                add(("throughput", "W3", machine, r["payload"], tool, reader), r["replay"]["records_per_s"] / 1e6,
                    chart="throughput", workload="W3", machine=machine, payload=r["payload"], tool=tool,
                    reader=reader, unit="million records/s")
            elif r["workload"] == "W2":
                if m["tool"] not in ("tickertape", "chronicle", "aeron", "plain-file"):
                    continue
                reader = {"safe": "safe", "mapped": "zero-copy"}[r["reader"]] if m["tool"] == "tickertape" else None
                add(("w2", machine, r["payload"], r["rate"], tool, reader), r["latency"]["p99_9_us"], chart="w2",
                    workload="W2", metric="reader p99.9", machine=machine, payload=r["payload"], rate=r["rate"],
                    tool=tool, reader=reader, unit="µs")
        for m, r in sustained:
            # ext4 pinned, both rounds (the README's table); on xfs (part sustained-journals, one round) the SV runs of
            # the two journals; the plain file's SV runs on ext4 (part sustained-others, one round, run separately).
            wanted = ((m["fs"], m["part"]) == ("ext4", "sustained")
                      or ((m["fs"], m["part"]) == ("xfs", "sustained-journals") and r["workload"] == "SV")
                      or ((m["fs"], m["part"], m["tool"]) == ("ext4", "sustained-others", "plain-file")
                          and r["workload"] == "SV"))
            if m["config"] != "pinned" or not wanted:
                continue
            tool = TOOL[m["tool"]]
            base = dict(chart="ten-minute", machine=machine, fs=m["fs"], part=m["part"], payload=r["payload"],
                        tool=tool, reader="zero-copy" if m["tool"] == "tickertape" else None)
            key = (m["fs"], m["part"], machine, r["payload"], tool)
            run = {"round": m["round"]}
            w, rr = r["writer"], r["reader_result"]
            if r["workload"] == "probe":
                add(("probe",) + key, dict(run, records_per_s=w["achieved_records_per_s"]),
                    workload="probe", unit="records/s", **base)
            elif r["workload"] == "SV":
                add(("SV",) + key, dict(run, target_records_per_s=r["rate"],
                    reader_p99_9_us=rr["p99_9_us"], reader_max_us=rr["max_us"]), workload="SV", unit="µs", **base)
            elif r["workload"] == "SD":
                s = w["periodic_sync"]["latency"]
                add(("SD",) + key, dict(run, target_records_per_s=r["rate"],
                    sync_p50_us=s["p50_us"], sync_max_us=s["max_us"], reader_p99_9_us=rr["p99_9_us"]),
                    workload="SD", unit="µs", sync_operation={"tickertape": "Writer::checkpoint (blocking)",
                    "Chronicle Queue": "synchronous store flush (ExcerptAppender.sync, SyncMode.SYNC)"}[tool], **base)
    out = []
    for cell in cells.values():
        if cell["chart"] == "ten-minute":
            cell["runs"].sort(key=lambda r: r["round"])
        cell["n"] = len(cell["runs"])
        out.append(cell)
    order = {"throughput": 0, "w2": 1, "ten-minute": 2}
    out.sort(key=lambda c: (order[c["chart"]], c.get("workload", ""), c.get("fs", ""), c.get("part", ""), c["machine"],
                            c["payload"], c.get("rate") or 0, c["tool"], c.get("reader") or ""))
    doc = {
        "about": "Values plotted by the comparison charts: the README tables of the rerun at v1.0.0 (ext4, pinned), plus "
                 "the SV runs on xfs and the plain file's SV runs on ext4 for the paced chart of docs/benchmarks.md.",
        "source": "raw result files of the rerun (not in the repository), exported by docs/charts/export_rerun.py",
        "section": "docs/benchmarks.md#" + SECTION,
        "session_commit": commits,
        "library": "v1.0.0",
        "config": "pinned",
        "machines": {"x86-64": "Google Cloud c4-standard-16-lssd (Xeon 6985P-C)",
                     "aarch64": "Google Cloud c4a-standard-8-lssd (Neoverse-V2)"},
        "cells": out,
    }
    json.dump(doc, sys.stdout, indent=1, ensure_ascii=False)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main(*sys.argv[1:3])
