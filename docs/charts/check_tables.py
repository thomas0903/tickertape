#!/usr/bin/env python3
"""Checks the chart data against the published tables, cell by cell.

    python3 docs/charts/check_tables.py

For every cell of the comparison charts (`data/rerun-v1.0.0.json`) it rounds the smallest and the largest run as the
tables do (half up; three significant digits, two decimals for million records/s and for milliseconds, whole records/s)
and compares the result with the cell of the README's table and with the matching row of `docs/benchmarks.md`
(ext4, pinned). For the since-1.0.0 charts (`data/since-1.0.0.json`), whose values were taken from the published tables,
it checks that each cell's `table_text` occurs in the section of `docs/benchmarks.md` named by the cell. It prints
every mismatch and exits 1 if there is one.
"""
import json
import os
import re
import sys
from decimal import ROUND_HALF_UP, Decimal

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))


def half_up(v, decimals):
    q = Decimal(1).scaleb(-decimals)
    return Decimal(repr(float(v))).quantize(q, rounding=ROUND_HALF_UP)


def sig3(v):
    """Three significant digits, half up, as the tables print them (0.39, 2.17, 11.8, 642)."""
    r = half_up(v, 2)
    if r < 10:
        return f"{r:.2f}"
    r = half_up(v, 1)
    if r < 100:
        return f"{r:.1f}"
    return f"{half_up(v, 0):,.0f}"


def us_text(v):
    """µs below 1 ms (three significant digits), else ms with two decimals; returns (number, unit)."""
    s = sig3(v)
    if Decimal(s.replace(",", "")) < 1000:
        return s, "µs"
    return f"{half_up(v / 1000, 2):,.2f}", "ms"


def mrs(v):
    return f"{half_up(v, 2):.2f}"


def rng(values, fmt):
    a, b = fmt(min(values)), fmt(max(values))
    return a if a == b else f"{a}–{b}"


def us_range(values):
    (a, ua), (b, ub) = us_text(min(values)), us_text(max(values))
    if (a, ua) == (b, ub):
        return a if ua == "µs" else f"{a} ms"
    if ua == ub == "µs":
        return f"{a}–{b}"
    return f"{a} {ua} to {b} {ub}"


def us_unit(v):
    n, u = us_text(v)
    return f"{n} {u}"


def table_rows(text):
    rows = []
    for line in text.splitlines():
        if line.startswith("|") and not line.startswith("|---"):
            rows.append([c.strip() for c in line.strip().strip("|").split("|")])
    return rows


def section(text, start, stop="\n## "):
    i = text.index(start)
    j = text.find(stop, i + len(start))
    return text[i:j if j >= 0 else len(text)]


UNIT = {"µs": 1.0, "ms": 1000.0, "s": 1e6}


def parse_cell(text, unit):
    """Numbers of a cell ("141 ms to 149 ms", "659 µs (633 µs–854 µs)", "188–189"), in µs for times."""
    out = []
    for num, u in re.findall(r"(\d[\d,]*(?:\.\d+)?)\s*(ms|µs|s)?(?![\w.])", text):
        x = float(num.replace(",", ""))
        if unit == "µs":
            x *= UNIT[u or "µs"]
        out.append(x)
    return out


def find_cell(text, row_prefix, column):
    header = None
    for line in text.splitlines():
        if not line.startswith("|") or line.startswith("|---"):
            header = None if not line.startswith("|") else header
            continue
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if header is None:
            header = cells
            continue
        if line.startswith(row_prefix) and column in header:
            return cells[header.index(column)]
    return None


def main():
    readme = open(os.path.join(ROOT, "README.md"), encoding="utf-8").read()
    bench = open(os.path.join(ROOT, "docs", "benchmarks.md"), encoding="utf-8").read()
    perf = section(readme, "## Performance")
    data = json.load(open(os.path.join(HERE, "data", "rerun-v1.0.0.json"), encoding="utf-8"))
    cells = data["cells"]
    errors, checked = [], 0

    def compare(where, expected, got):
        nonlocal checked
        checked += 1
        if expected != got:
            errors.append(f"{where}: table has {expected!r}, data rounds to {got!r}")

    cols = [("x86-64", 64), ("x86-64", 1024), ("aarch64", 64), ("aarch64", 1024)]
    # README: throughput table
    names = {"W1 cold": "W1-cold", "W1 steady": "W1-steady", "W3": "W3"}
    tools = {"tickertape": ("tickertape", None), "Chronicle Queue": ("Chronicle Queue", None),
             "tickertape, zero-copy": ("tickertape", "zero-copy"), "tickertape, safe": ("tickertape", "safe"),
             "yaque (a scan of its queue)": ("yaque", None)}
    rows = [r for r in table_rows(perf) if re.match(r"(W1 cold|W1 steady|W3): ", r[0])]
    for row in rows:
        wl, tool = row[0].split(": ", 1)
        tool, reader = tools[tool]
        for (machine, payload), text in zip(cols, row[1:]):
            c = [c for c in cells if c["chart"] == "throughput" and c["workload"] == names[wl] and c["machine"] == machine
                 and c["payload"] == payload and c["tool"] == tool and c["reader"] == reader]
            assert len(c) == 1, (row, machine, payload)
            compare(f"README throughput {row[0]} {machine} {payload} B", text, rng(c[0]["runs"], mrs))
    # README: W2 table
    w2_tools = [("tickertape", "zero-copy"), ("tickertape", "safe"), ("Chronicle Queue", None), ("Aeron IPC", None),
                ("plain file", None)]
    for row in table_rows(perf):
        m = re.match(r"(x86-64|aarch64), (64|1,024) B, (10,000|100,000)/s$", row[0])
        if not m:
            continue
        machine, payload, rate = m.group(1), int(m.group(2).replace(",", "")), int(m.group(3).replace(",", ""))
        for (tool, reader), text in zip(w2_tools, row[1:]):
            c = [c for c in cells if c["chart"] == "w2" and c["machine"] == machine and c["payload"] == payload
                 and c["rate"] == rate and c["tool"] == tool and c["reader"] == reader]
            assert len(c) == 1
            compare(f"README W2 {row[0]} {tool} {reader or ''}", text, us_range(c[0]["runs"]))
    # README: ten-minute table
    def tm(workload, tool, machine, payload):
        c = [c for c in cells if c["chart"] == "ten-minute" and c["workload"] == workload and c["tool"] == tool
             and c["machine"] == machine and c["payload"] == payload]
        assert len(c) == 1 and [r["round"] for r in c[0]["runs"]] == [1, 2]
        return c[0]["runs"]
    whole = lambda v: f"{half_up(v, 0):,.0f}"
    specs = {
        "Probe, records/s: tickertape": ("probe", "tickertape", lambda r: whole(r["records_per_s"])),
        "Probe: Chronicle Queue": ("probe", "Chronicle Queue", lambda r: whole(r["records_per_s"])),
        "Probe: Aeron IPC": ("probe", "Aeron IPC", lambda r: whole(r["records_per_s"])),
        "SV and SD target, records/s: tickertape": ("SV", "tickertape", lambda r: whole(r["target_records_per_s"])),
        "Target: Chronicle Queue": ("SV", "Chronicle Queue", lambda r: whole(r["target_records_per_s"])),
        "SV target: Aeron IPC": ("SV", "Aeron IPC", lambda r: whole(r["target_records_per_s"])),
        "SV reader p99.9 (max): tickertape": ("SV", "tickertape",
                                              lambda r: f"{us_unit(r['reader_p99_9_us'])} ({us_unit(r['reader_max_us'])})"),
        "SV: Chronicle Queue": ("SV", "Chronicle Queue",
                                lambda r: f"{us_unit(r['reader_p99_9_us'])} ({us_unit(r['reader_max_us'])})"),
        "SV: Aeron IPC": ("SV", "Aeron IPC", lambda r: f"{us_unit(r['reader_p99_9_us'])} ({us_unit(r['reader_max_us'])})"),
        "SD, tickertape checkpoint, median (max)": ("SD", "tickertape",
                                                    lambda r: f"{us_unit(r['sync_p50_us'])} ({us_unit(r['sync_max_us'])})"),
        "SD, Chronicle Queue synchronous store flush, median (max)": (
            "SD", "Chronicle Queue", lambda r: f"{us_unit(r['sync_p50_us'])} ({us_unit(r['sync_max_us'])})"),
        "SD reader p99.9: tickertape": ("SD", "tickertape", lambda r: us_unit(r["reader_p99_9_us"])),
        "SD reader p99.9: Chronicle Queue": ("SD", "Chronicle Queue", lambda r: us_unit(r["reader_p99_9_us"])),
    }
    seen = set()
    for row in table_rows(perf):
        if row[0] not in specs:
            continue
        seen.add(row[0])
        workload, tool, f = specs[row[0]]
        for (machine, payload), text in zip(cols, row[1:]):
            runs = tm(workload, tool, machine, payload)
            compare(f"README ten-minute {row[0]} {machine} {payload} B", text, "; ".join(f(r) for r in runs))
    # SD targets equal SV targets (the README prints one row for both)
    for tool in ("tickertape", "Chronicle Queue"):
        for machine, payload in cols:
            compare(f"SD target = SV target {tool} {machine} {payload} B",
                    [r["target_records_per_s"] for r in tm("SV", tool, machine, payload)],
                    [r["target_records_per_s"] for r in tm("SD", tool, machine, payload)])
    missing = set(specs) - seen
    if missing:
        errors.append(f"README ten-minute rows not found: {sorted(missing)}")
    # docs/benchmarks.md: W1, W3 and W2 p99.9 rows (ext4, pinned)
    rerun = section(bench, "## 2026-11-25 to 2026-11-26, comparison rerun at v1.0.0")
    for title, wl in (("**W1-cold**", "W1-cold"), ("**W1-steady**", "W1-steady"), ("**W3, warm replay**", "W3")):
        part = section(rerun, title, "\n\n**")
        for row in table_rows(part):
            if row[:2] != ["ext4", "pinned"]:
                continue
            tool = row[2]
            reader = (row[3] or None) if wl == "W3" else None
            values = row[5:] if wl == "W3" else row[4:]
            for (machine, payload), text in zip(cols, values):
                c = [c for c in cells if c["chart"] == "throughput" and c["workload"] == wl and c["machine"] == machine
                     and c["payload"] == payload and c["tool"] == tool and c["reader"] == reader]
                if c:
                    compare(f"benchmarks.md {wl} {tool} {reader or ''} {machine} {payload} B", text, rng(c[0]["runs"], mrs))
    for payload, rate in ((64, 10000), (64, 100000), (1024, 10000), (1024, 100000)):
        title = f"**{payload:,} B at {rate:,} records/s**"
        part = section(rerun, title, "\n\n**")
        for row in table_rows(part):
            if row[:2] != ["ext4", "pinned"]:
                continue
            tool, reader = row[2], row[3] or None
            for machine, text in (("x86-64", row[7]), ("aarch64", row[12])):
                c = [c for c in cells if c["chart"] == "w2" and c["machine"] == machine and c["payload"] == payload
                     and c["rate"] == rate and c["tool"] == tool and c["reader"] == reader]
                if c:
                    compare(f"benchmarks.md W2 {payload} B {rate}/s {tool} {reader or ''} {machine}", text,
                            us_range(c[0]["runs"]))
    # since-1.0.0: each cell's values against the table row and column it names
    since_path = os.path.join(HERE, "data", "since-1.0.0.json")
    since = json.load(open(since_path, encoding="utf-8"))
    for c in since["cells"]:
        checked += 1
        sub = section(section(bench, c["section_heading"]), c["subsection"], "\n### ")
        text = find_cell(sub, c["row_prefix"], c["column"])
        if text is None:
            errors.append(f"since-1.0.0 {c['id']}: row {c['row_prefix']!r} column {c['column']!r} not found")
            continue
        if "side" in c:
            text = text.split(" → ")[c["side"]]
        if c.get("table_text") != text:
            errors.append(f"since-1.0.0 {c['id']}: table_text {c.get('table_text')!r}, table has {text!r}")
        got = parse_cell(text, c["unit"])
        v = c["values"]
        if c["stat"] == "median_range":
            want = [v[0], v[1], v[2]]
            if len(got) == 1:  # "1.38 ms" with no range: median, min and max print the same
                got = got * 3
        else:
            want = [min(v), max(v)] if min(v) != max(v) else [v[0]]
            if c["stat"] == "runs" and len(v) != c["n"]:
                errors.append(f"since-1.0.0 {c['id']}: {len(v)} runs for n = {c['n']}")
        if len(got) != len(want) or any(abs(a - b) > 1e-9 * max(1.0, abs(b)) for a, b in zip(got, want)):
            errors.append(f"since-1.0.0 {c['id']}: table has {text!r}, data has {v}")
    for e in errors:
        print("MISMATCH", e)
    print(f"{checked} comparisons, {len(errors)} mismatches")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
