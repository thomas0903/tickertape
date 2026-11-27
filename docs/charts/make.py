# /// script
# requires-python = "==3.12.*"
# dependencies = ["matplotlib==3.10.3", "numpy==2.2.6"]
# ///
"""Draws the performance charts of README.md and docs/benchmarks.md from the data files next to this script.

    ~/.local/bin/uv run docs/charts/make.py        # or: uv run docs/charts/make.py

Reads only `data/rerun-v1.0.0.json` and `data/since-1.0.0.json`; writes `<name>-light.svg` and `<name>-dark.svg` for
every chart into this directory. The output is deterministic: the SVG ids are salted with a fixed string, no date or
creator is written, and coordinates are rounded to 0.01 pt, so a rerun with the pinned versions gives byte-identical
files. Text stays text (font-family DejaVu Sans, then the viewer's sans-serif), which keeps the files small; the layout
leaves room for DejaVu Sans, the widest of the usual fallbacks. See README.md in this directory for how to add a chart.
"""
import io
import json
import os
import re
import textwrap

import matplotlib

matplotlib.use("svg")
import matplotlib.pyplot as plt  # noqa: E402
from matplotlib.lines import Line2D  # noqa: E402
from matplotlib.transforms import blended_transform_factory  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))

plt.rcParams.update({
    "svg.fonttype": "none",
    "svg.hashsalt": "tickertape-charts",
    "font.family": "sans-serif",
    "font.sans-serif": ["DejaVu Sans"],
    "font.size": 9,
    "axes.unicode_minus": False,
})

# Colours: the validated categorical palette of the dataviz method, in an order whose adjacent pairs pass the CVD and
# normal-vision checks (light on #ffffff, dark on #0d1117, GitHub's dark background). Every row also carries its
# tool's name as text and its own marker shape, so no reading depends on colour.
THEMES = {
    "light": dict(bg="#ffffff", ink="#1f2328", ink2="#59636e", grid="#e4e7eb", axis="#b8bec6",
                  tools={"tickertape": "#257de0", "tickertape safe": "#00b894", "Chronicle Queue": "#f47230",
                         "Aeron IPC": "#6a4ce2", "plain file": "#ee7fac", "yaque": "#008a17"}),
    "dark": dict(bg="#0d1117", ink="#e6edf3", ink2="#9198a1", grid="#262c36", axis="#4a525c",
                 tools={"tickertape": "#338ced", "tickertape safe": "#00a888", "Chronicle Queue": "#e76624",
                        "Aeron IPC": "#9887ef", "plain file": "#e24d8b", "yaque": "#0aac23"}),
}
MARKERS = {"tickertape": "o", "tickertape safe": "s", "Chronicle Queue": "D", "Aeron IPC": "^", "plain file": "v",
           "yaque": "P"}

W = 7.2             # figure width, inches (691 px at 96 px per inch)
RH = 0.23           # one row, inches
GROUP_GAP = 0.45    # rows of space between groups
COL_GAP = 0.3
MARGIN = 0.12
F_TITLE, F_SUB, F_PANEL, F_LABEL, F_TICK, F_NOTE, F_CAPTION = 12, 9.5, 10, 9.5, 8.5, 8.5, 8


def wrap(text, size, width_in=W - 2 * MARGIN):
    """Lines that fit the width in DejaVu Sans (average advance about 0.6 em on this kind of text)."""
    return textwrap.wrap(text, int(width_in * 72 / (size * 0.6)))


def us_label(v):
    if v >= 1e6:
        return f"{v / 1e6:g} s"
    if v >= 1e3:
        return f"{v / 1e3:g} ms"
    return f"{v:g} µs"


def row_positions(rows, headers):
    """y of each row (in rows), and y of each group header when the groups get a line of their own."""
    ys, heads, y, last = [], [], -1.0, object()
    for row in rows:
        g = row.get("group")
        if g != last:
            if ys:
                y += GROUP_GAP
            if headers and g:
                y += 1.0
                heads.append((y, g))
        y += 1.0
        ys.append(y)
        last = g
    return ys, heads


def render(spec, theme_name, path):
    t = THEMES[theme_name]
    label_w, annot_w = spec.get("label_w", 2.0), spec.get("annot_w", 0.0)
    headers = spec.get("group_headers", False)
    subtitle = [(line, bold) for text, bold in spec.get("subtitle", []) for line in wrap(text, F_SUB)]
    caption = [line for text in spec["caption"] for line in wrap(text, F_CAPTION)]
    top = 0.36 + 0.2 * len(subtitle) + (0.3 if spec.get("legend") else 0) + 0.08
    bottom = 0.14 + 0.17 * len(caption)
    layout = []
    for p in spec["panels"]:
        ys, heads = row_positions(p["rows"], headers)
        head_h = 0.34 + (0.24 if len(p["columns"]) > 1 else 0) + (0.16 if p.get("annot_title") else 0)
        ax_h = (max(ys) + 1.2) * RH
        layout.append((ys, heads, head_h, ax_h, head_h + ax_h + 0.5))
    H = top + sum(x[-1] for x in layout) + 0.16 * (len(layout) - 1) + bottom
    fig = plt.figure(figsize=(W, H), facecolor=t["bg"])

    def text(x_in, y_in, s, **kw):
        kw.setdefault("color", t["ink"])
        kw.setdefault("va", "baseline")
        fig.text(x_in / W, 1 - y_in / H, s, **kw)

    text(MARGIN, 0.26, spec["title"], fontsize=F_TITLE, fontweight="bold")
    y = 0.26
    for line, bold in subtitle:
        y += 0.2
        text(MARGIN, y, line, fontsize=F_SUB, fontweight="bold" if bold else "normal")
    if spec.get("legend"):
        y += 0.3
        draw_legend(fig, t, spec["legend"], y, H)
    cursor = top
    plot_left, plot_right = label_w, W - 0.15 - annot_w
    for p, (ys, heads, head_h, ax_h, h) in zip(spec["panels"], layout):
        ncols = len(p["columns"])
        col_w = (plot_right - plot_left - COL_GAP * (ncols - 1)) / ncols
        text(MARGIN, cursor + 0.24, p["title"], fontsize=F_PANEL, fontweight="bold")
        ax_top = cursor + head_h
        ymax = max(ys) + 0.6
        for ci, col in enumerate(p["columns"]):
            left = plot_left + ci * (col_w + COL_GAP)
            ax = fig.add_axes([left / W, 1 - (ax_top + ax_h) / H, col_w / W, ax_h / H])
            style_axes(ax, t, p)
            ax.set_ylim(ymax, -0.6)
            if col.get("heading"):
                text(left + col_w / 2, cursor + 0.5, col["heading"], fontsize=F_LABEL, fontweight="bold",
                     ha="center")
            for yy, row, data in zip(ys, p["rows"], col["data"]):
                draw_row(ax, t, yy, row, data)
            tr = blended_transform_factory(ax.transAxes, ax.transData)
            if ci == 0:
                last = object()
                for yy, row in zip(ys, p["rows"]):
                    g = row.get("group")
                    if g and g != last and not headers:
                        ax.text(-(left - MARGIN) / col_w, yy, g, transform=tr, ha="left", va="center",
                                fontsize=F_NOTE, color=t["ink2"])
                    last = g
                    ax.text(-0.1 / col_w, yy, row["label"], transform=tr, ha="right", va="center",
                            fontsize=F_LABEL, color=t["ink"])
                for yy, g in heads:
                    ax.text(-(left - MARGIN) / col_w, yy, g, transform=tr, ha="left", va="center",
                            fontsize=F_LABEL, color=t["ink2"], fontweight="bold")
            if annot_w and ci == ncols - 1:
                if p.get("annot_title"):
                    text(left + col_w + 0.12, ax_top - 0.06, p["annot_title"], fontsize=F_NOTE, color=t["ink2"])
                for yy, row in zip(ys, p["rows"]):
                    if row.get("annot"):
                        ax.text(1 + 0.12 / col_w, yy, row["annot"], transform=tr, ha="left", va="center",
                                fontsize=F_NOTE, color=t["ink"])
        text((plot_left + plot_right) / 2, ax_top + ax_h + 0.42, p["xlabel"], ha="center", fontsize=F_NOTE,
             color=t["ink2"])
        cursor += h + 0.16
    y = H - bottom + 0.06
    for line in caption:
        y += 0.17
        text(MARGIN, y, line, fontsize=F_CAPTION, color=t["ink2"])
    buf = io.StringIO()
    fig.savefig(buf, format="svg", facecolor=t["bg"], metadata={"Date": None, "Creator": None})
    plt.close(fig)
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(compact(buf.getvalue()))


def compact(svg):
    """Coordinates in attributes to 0.01 pt, no indentation, no no-op rotations: smaller, the same picture."""
    svg = re.sub(r'="([^"]*)"', lambda m: '="' + re.sub(r"(\d+\.\d\d)\d+", r"\1", m.group(1)) + '"', svg)
    svg = re.sub(r' transform="rotate\(-0 [\d.]+ [\d.]+\)"', "", svg)
    return "\n".join(line.strip() for line in svg.splitlines()) + "\n"


def style_axes(ax, t, p):
    ax.set_facecolor(t["bg"])
    for s in ("top", "right", "left"):
        ax.spines[s].set_visible(False)
    ax.spines["bottom"].set_color(t["axis"])
    ax.spines["bottom"].set_linewidth(0.8)
    ax.tick_params(axis="y", left=False, labelleft=False)
    ax.tick_params(axis="x", colors=t["axis"], labelcolor=t["ink2"], labelsize=F_TICK, length=0, pad=4)
    if p.get("scale") == "log":
        ax.set_xscale("log")
        ax.minorticks_off()
    ax.set_xlim(*p["xlim"])
    ax.set_xticks(p["ticks"])
    ax.set_xticklabels(p["ticklabels"])
    ax.grid(axis="x", color=t["grid"], linewidth=0.7, zorder=0)
    ax.set_axisbelow(True)


def offsets(n):
    if n <= 1:
        return [0.0]
    span = 0.5 if n > 2 else 0.3
    return [-span / 2 + span * i / (n - 1) for i in range(n)]


def colour(t, key):
    return t["tools"].get(key, t["ink"])


def draw_row(ax, t, y, row, data):
    """One row: each series is points (runs, spread vertically so that none hides another), a line from the smallest
    to the largest, or a median with its range."""
    key = row.get("key", "")
    for s in data.get("series", [data]):
        c = colour(t, s.get("key", key))
        mk = s.get("marker", MARKERS.get(key, "o"))
        yy = y + s.get("dy", 0.0)
        hollow = s.get("hollow", False)
        face, edge = (t["bg"], c) if hollow else (c, t["bg"])
        mark = dict(marker=mk, linestyle="none", markersize=5.6, markerfacecolor=face, markeredgecolor=edge,
                    markeredgewidth=1.3 if hollow else 0.9, zorder=4)
        if s.get("range"):
            lo, hi = s["range"]
            ax.plot([lo, hi], [yy, yy], color=c, linewidth=1.4, solid_capstyle="butt", zorder=3)
            if s.get("ends"):
                ax.plot([lo, hi], [yy, yy], color=c, **mark)
        runs = s.get("runs", [])
        if runs:
            if len(runs) > 1 and not s.get("no_line"):
                ax.plot([min(runs), max(runs)], [yy, yy], color=c, linewidth=1.4, alpha=0.85,
                        solid_capstyle="butt", zorder=3)
            offs = offsets(len(runs)) if s.get("jitter", True) else [0.0] * len(runs)
            ax.plot(runs, [yy + o for o in offs], color=c, **mark)


def legend_mark(t, x, yf, item):
    return Line2D([x / W], [yf], marker=item["marker"], markersize=5.6, linestyle="none", color=t["ink"],
                  markerfacecolor=t["bg"] if item.get("hollow") else t["ink"],
                  markeredgewidth=1.3 if item.get("hollow") else 0.9,
                  markeredgecolor=t["ink"] if item.get("hollow") else t["bg"])


def draw_legend(fig, t, items, y_in, H):
    x = MARGIN
    yf = 1 - (y_in - 0.045) / H
    for item in items:
        if item.get("line"):
            fig.add_artist(Line2D([(x + 0.04) / W, (x + 0.34) / W], [yf, yf], color=t["ink"], linewidth=1.4))
            fig.add_artist(legend_mark(t, x + 0.04, yf, item))
            fig.add_artist(legend_mark(t, x + 0.34, yf, item))
            x += 0.48
        else:
            fig.add_artist(legend_mark(t, x + 0.06, yf, item))
            x += 0.2
        fig.text(x / W, 1 - y_in / H, item["text"], fontsize=F_NOTE, color=t["ink2"], va="baseline")
        x += len(item["text"]) * F_NOTE * 0.6 / 72 + 0.3


# ----------------------------------------------------------------------------------------------------------------
# The charts

def load(name):
    with open(os.path.join(HERE, "data", name), encoding="utf-8") as f:
        return json.load(f)


MACHINES = ["x86-64", "aarch64"]
VMS = ("Google Cloud VMs: x86-64 c4-standard-16-lssd (Xeon 6985P-C), aarch64 c4a-standard-8-lssd (Neoverse-V2); "
       "ext4 on local NVMe SSD; each measured thread pinned.")
V100 = "Library v1.0.0 (session commit fb2d91b0, 2026-11-25 and 26)."
RERUN_DATA = "Data: docs/charts/data/rerun-v1.0.0.json; tables: README.md and docs/benchmarks.md (comparison rerun)."


def cell(cells, **kw):
    found = [c for c in cells if all(c.get(k) == v for k, v in kw.items())]
    assert len(found) == 1, (kw, len(found))
    return found[0]


def tool_key(tool, reader):
    return "tickertape safe" if (tool, reader) == ("tickertape", "safe") else tool


def throughput(rerun):
    cells = rerun["cells"]
    w1_tools = [("tickertape", None), ("Chronicle Queue", None)]
    w3_tools = [("tickertape", "zero-copy"), ("tickertape", "safe"), ("Chronicle Queue", None), ("yaque", None)]
    panels = []
    for title, wls, tools, payload, xmax, step in (
            ("W1 append, 64 B", ["W1-cold", "W1-steady"], w1_tools, 64, 35, 5),
            ("W1 append, 1,024 B", ["W1-cold", "W1-steady"], w1_tools, 1024, 5, 1),
            ("W3 warm replay, 64 B", ["W3"], w3_tools, 64, 40, 5),
            ("W3 warm replay, 1,024 B", ["W3"], w3_tools, 1024, 20, 5)):
        rows = []
        for wl in wls:
            for tool, reader in tools:
                label = "yaque (queue scan)" if tool == "yaque" else tool if reader is None else f"{tool}, {reader}"
                rows.append(dict(group={"W1-cold": "cold", "W1-steady": "steady"}.get(wl), label=label,
                                 key=tool_key(tool, reader), sel=dict(workload=wl, tool=tool, reader=reader)))
        columns = []
        for m in MACHINES:
            data = []
            for row in rows:
                c = cell(cells, chart="throughput", machine=m, payload=payload, **row["sel"])
                assert c["n"] == len(c["runs"]) == 4
                data.append(dict(runs=c["runs"]))
            columns.append(dict(heading=m, data=data))
        ticks = list(range(0, xmax + 1, step))
        panels.append(dict(title=title, rows=rows, columns=columns, xlim=(0, xmax), ticks=ticks,
                           ticklabels=[str(v) for v in ticks], xlabel="million records/s (axis from zero)"))
    return dict(
        title="Append (W1) and warm replay (W3) throughput, v1.0.0",
        subtitle=[("Million records/s, higher is better. Each of the n = 4 runs of a cell is a point; the line spans "
                   "min–max.", False)],
        panels=panels, label_w=2.1,
        caption=[
            "n = 4 per cell: two rounds of two runs. " + VMS + " " + V100,
            "W1 is visible, not durable, append throughput (both timers stop before the final flush). x86-64 W1-cold "
            "at 64 B: the first repeat of every round ran at 29.12–30.84, the second at 17.53–20.07 (cause not "
            "traced). yaque, queue-file and the plain file appended more slowly than both journals in every W1 cell "
            "(not drawn).",
            RERUN_DATA,
        ])


def w2(rerun):
    cells = rerun["cells"]
    tools = [("tickertape", "zero-copy"), ("tickertape", "safe"), ("Chronicle Queue", None), ("Aeron IPC", None),
             ("plain file", None)]
    rows = [dict(label=t if r is None else f"{t}, {r}", key=tool_key(t, r), sel=dict(tool=t, reader=r))
            for t, r in tools]
    ticks = [1, 10, 100, 1000, 10000]
    panels = []
    for payload, rate in ((64, 10000), (64, 100000), (1024, 10000), (1024, 100000)):
        columns = []
        for m in MACHINES:
            data = []
            for row in rows:
                c = cell(cells, chart="w2", machine=m, payload=payload, rate=rate, **row["sel"])
                assert c["n"] == len(c["runs"]) == 4
                data.append(dict(runs=c["runs"]))
            columns.append(dict(heading=m, data=data))
        panels.append(dict(title=f"{payload:,} B at {rate:,} records/s", rows=rows, columns=columns, scale="log",
                           xlim=(0.2, 30000), ticks=ticks, ticklabels=[us_label(v) for v in ticks],
                           xlabel="reader p99.9, logarithmic scale: each gridline is ten times the one to its left"))
    return dict(
        title="Reader latency p99.9 (W2), v1.0.0",
        subtitle=[("From each record's scheduled send to the reader's validated read; lower is better. "
                   "Logarithmic axis.", False),
                  ("Each of the n = 4 runs of a cell is a point; the line spans min–max. Tools in the table's column "
                   "order.", False)],
        panels=panels, label_w=2.1,
        caption=[
            "n = 4 per cell: two rounds of two runs, each 1,000,000 records measured after 100,000 unmeasured. "
            + VMS + " " + V100,
            "Chronicle Queue at x86-64, 1,024 B, 10,000/s: three runs at 2.36–2.82 µs and one at 15.75 ms. Aeron IPC "
            "writes no durable journal; the plain file has no checksum or recovery. yaque (5.76 µs to 18.30 ms in "
            "these cells) is not drawn, as in the table.",
            RERUN_DATA,
        ])


def probe(rerun):
    cells = rerun["cells"]
    tools = ["tickertape", "Chronicle Queue", "Aeron IPC"]
    panels = []
    for payload, xmax, step in ((64, 8, 1), (1024, 4.5, 0.5)):
        rows = [dict(label=t, key=t) for t in tools]
        columns = []
        for m in MACHINES:
            data = []
            for t in tools:
                c = cell(cells, chart="ten-minute", workload="probe", machine=m, payload=payload, tool=t)
                assert [r["round"] for r in c["runs"]] == [1, 2]
                data.append(dict(runs=[r["records_per_s"] / 1e6 for r in c["runs"]]))
            columns.append(dict(heading=m, data=data))
        ticks = [round(i * step, 1) for i in range(int(xmax / step) + 1)]
        panels.append(dict(title=f"Unpaced probe, {payload:,} B", rows=rows, columns=columns, xlim=(0, xmax),
                           ticks=ticks, ticklabels=[f"{v:g}" for v in ticks],
                           xlabel="million records/s, unpaced, a reader following (axis from zero)"))
    return dict(
        title="Ten-minute runs: unpaced probe, v1.0.0",
        subtitle=[("Records/s appended in 10 minutes with a reader following; higher is better. One point per round "
                   "(n = 1 per round, two rounds).", False)],
        panels=panels, label_w=2.1,
        caption=[
            "The probe is the only ten-minute row in which every tool answers the same question. At 1,024 B the two "
            "journals reach about the limit of the disk (one local SSD writes at most 351 MB/s by itself). Aeron IPC "
            "writes no durable journal (its term buffers are memory-mapped files on the same disk). tickertape ran "
            "its zero-copy reader.",
            VMS + " " + V100,
            RERUN_DATA,
        ])


def paced(rerun):
    cells = rerun["cells"]
    groups = [(m, p) for m in MACHINES for p in (64, 1024)]
    ticks = [100, 1000, 10000, 100000, 1000000]

    def panel(title, workload, tools, labels, value, maxkey, xlabel):
        rows, data = [], []
        for m, p in groups:
            for tool, label in zip(tools, labels):
                c = cell(cells, chart="ten-minute", workload=workload, machine=m, payload=p, tool=tool)
                assert [r["round"] for r in c["runs"]] == [1, 2]
                rows.append(dict(group=f"{m}, {p:,} B", label=label, key=tool,
                                 annot="; ".join(f"{r['target_records_per_s']:,}" for r in c["runs"])))
                series = [dict(runs=[r[value] for r in c["runs"]], no_line=True)]
                if maxkey:
                    series.append(dict(runs=[r[maxkey] for r in c["runs"]], hollow=True, no_line=True))
                data.append(dict(series=series))
        return dict(title=title, rows=rows, columns=[dict(data=data)], scale="log", xlim=(150, 2e6), ticks=ticks,
                    ticklabels=[us_label(v) for v in ticks], xlabel=xlabel,
                    annot_title="target records/s, rounds 1; 2")

    return dict(
        title="Ten-minute runs: paced (SV, SD), v1.0.0",
        subtitle=[("Each tool at half of its own unpaced rate; not the same load.", True),
                  ("Each tool's target rate is printed beside its marks. One point per round (n = 1 per round, two "
                   "rounds); lower is better. Logarithmic axes.", False)],
        panels=[
            panel("SV, no syncs: reader p99.9 (filled) and maximum (hollow)", "SV",
                  ["tickertape", "Chronicle Queue", "Aeron IPC"], ["tickertape", "Chronicle Queue", "Aeron IPC"],
                  "reader_p99_9_us", "reader_max_us", "logarithmic scale"),
            panel("SD, a sync every second: its duration, median (filled) and maximum (hollow)", "SD",
                  ["tickertape", "Chronicle Queue"], ["tickertape checkpoint", "Chronicle store flush"],
                  "sync_p50_us", "sync_max_us", "logarithmic scale; the two syncs are not the same operation"),
            panel("SD, a sync every second: reader p99.9", "SD", ["tickertape", "Chronicle Queue"],
                  ["tickertape", "Chronicle Queue"], "reader_p99_9_us", None, "logarithmic scale"),
        ],
        label_w=2.1, annot_w=1.75, group_headers=True,
        caption=[
            "The targets are half of each tool's own probe in the same round. tickertape: zero-copy reader; in SD the "
            "blocking Writer::checkpoint() (msync, fdatasync of the segments, the marker written and synced), with "
            "early write-back starting each MiB's write-back as the writer passes it. Chronicle Queue: "
            "ExcerptAppender.sync with SyncMode.SYNC (msync of its store). Aeron IPC has no sync (no SD rows).",
            VMS + " " + V100,
            RERUN_DATA,
        ])


# --- since 1.0.0 ---------------------------------------------------------------------------------------------------

BEFORE = dict(key="before", marker="o", hollow=True, dy=-0.16)
AFTER = dict(key="after", marker="s", hollow=False, dy=0.16)


def adr0058(since):
    cells = [c for c in since["cells"] if c["chart"] == "adr0058"]

    def p(title, items, xlabel, scale, xlim, ticks, ticklabels, transform=lambda v: v, annot="table: before → after"):
        rows, data = [], []
        for metric, machine, cellname, group, label in items:
            b = cell(cells, metric=metric, machine=machine, cell=cellname, build="base")
            a = cell(cells, metric=metric, machine=machine, cell=cellname, build="change")
            assert len(b["values"]) == b["n"] == 2 and len(a["values"]) == a["n"] == 2
            rows.append(dict(group=group, label=label, annot=f"{b['table_text']} → {a['table_text']}"))
            data.append(dict(series=[dict(BEFORE, runs=[transform(v) for v in b["values"]], jitter=False),
                                     dict(AFTER, runs=[transform(v) for v in a["values"]], jitter=False)]))
        return dict(title=title, rows=rows, columns=[dict(data=data)], scale=scale, xlim=xlim, ticks=ticks,
                    ticklabels=ticklabels, xlabel=xlabel, annot_title=annot)

    s90e, s90x = "steady-90, 1,024 B, ext4", "steady-90, 1,024 B, xfs"
    a64, x1k = "steady-50, 64 B, ext4", "steady-50, 1,024 B, ext4"
    stall, dev, dur = ("seconds with an append of 50 ms or more", "device bytes per appended byte",
                       "background checkpoint, request to durable, median")
    roll = "roll append, median"
    panels = [
        p("Gain: seconds (of 611) with an append of 50 ms or more",
          [(stall, "aarch64", s90e, "aarch64", "steady-90, 1,024 B, ext4"),
           (stall, "aarch64", s90x, "aarch64", "steady-90, 1,024 B, xfs")],
          "seconds of the 611-second run (axis from zero)", "linear", (0, 160), [0, 40, 80, 120, 160],
          ["0", "40", "80", "120", "160"]),
        p("Gain: bytes written to the device beyond the appended bytes",
          [(dev, "aarch64", s90e, "aarch64", "steady-90, 1,024 B, ext4"),
           (dev, "aarch64", s90x, "aarch64", "steady-90, 1,024 B, xfs"),
           (dev, "aarch64", a64, "aarch64", "steady-50, 64 B, ext4"),
           (dev, "x86-64", x1k, "x86-64", "steady-50, 1,024 B, ext4")],
          "device bytes ÷ appended bytes − 1, in % (axis from zero); the table gives the ratio", "linear", (0, 10),
          [0, 2, 4, 6, 8, 10], ["0 %", "2 %", "4 %", "6 %", "8 %", "10 %"], transform=lambda v: (v - 1) * 100,
          annot="table (ratio): before → after"),
        p("Gain: background checkpoint, request to durable, median",
          [(dur, "aarch64", s90e, "aarch64", "steady-90, 1,024 B, ext4"),
           (dur, "aarch64", s90x, "aarch64", "steady-90, 1,024 B, xfs")],
          "logarithmic scale", "log", (300, 1e6), [1000, 10000, 100000, 1000000], ["1 ms", "10 ms", "100 ms", "1 s"]),
        p("Cost: the roll's append, median",
          [(roll, "aarch64", s90e, "aarch64", "steady-90, 1,024 B, ext4"),
           (roll, "aarch64", s90x, "aarch64", "steady-90, 1,024 B, xfs"),
           (roll, "aarch64", a64, "aarch64", "steady-50, 64 B, ext4"),
           (roll, "x86-64, kernel 6.8 (VM 1)", x1k, "x86-64, kernel 6.8", "VM 1, steady-50, 1,024 B, ext4"),
           (roll, "x86-64, kernel 6.8 (VM 2)", x1k, "x86-64, kernel 6.8", "VM 2, steady-50, 1,024 B, ext4")],
          "µs (axis from zero)", "linear", (0, 400), [0, 100, 200, 300, 400], ["0", "100", "200", "300", "400"]),
        p("Cost: at 64 B, checkpoint to durable and the zero-copy reader",
          [(dur, "aarch64", a64, "aarch64, steady-50, 64 B, ext4", "checkpoint to durable, median"),
           ("zero-copy reader p99.9", "aarch64", a64, "aarch64, steady-50, 64 B, ext4", "zero-copy reader p99.9")],
          "µs (axis from zero)", "linear", (0, 1000), [0, 250, 500, 750, 1000], ["0", "250", "500", "750", "1,000"]),
        p("Cost: at x86-64 1,024 B, checkpoint to durable, p99",
          [("background checkpoint, request to durable, p99", "x86-64", x1k, "x86-64", "steady-50, 1,024 B, ext4")],
          "ms (axis from zero)", "linear", (0, 7000), [0, 1000, 2000, 3000, 4000, 5000, 6000, 7000],
          ["0", "1", "2", "3", "4", "5", "6", "7"]),
    ]
    return dict(
        title="Early write-back to the folio boundary (ADR 0058): gains and costs",
        subtitle=[("main after 1.0.0: the base build against the change, which is the default from 1.1. 10-minute "
                   "runs (611 s with the warm-up), n = 2 per build: each run is a point (two runs with the same value "
                   "make one mark). A background checkpoint every second; steady-90 and steady-50 are 90 % and "
                   "50 % of an unpaced probe's rate in the sustained session of 2026-11-19 to 22.", False)],
        legend=[dict(marker="o", hollow=True, text="before: base build"),
                dict(marker="s", text="after: the change (the default from 1.1)")],
        panels=panels, label_w=2.3, annot_w=2.55, group_headers=True,
        caption=[
            "Google Cloud VMs with local NVMe SSD: aarch64 c4a-standard-8-lssd and x86-64 c4-standard-16-lssd on "
            "kernel 7.0; kernel 6.8 on c4-standard-4-lssd. Not compared with the comparison at v1.0.0. The x86-64 "
            "unpaced and W2 cells and the gate figures within the base's range are in the section.",
            "Section: docs/benchmarks.md, \"2026-11-26, early write-back to the folio boundary\". Data: "
            "docs/charts/data/since-1.0.0.json (from the published tables).",
        ])


def adr0059(since):
    cells = [c for c in since["cells"] if c["chart"] == "adr0059"]
    names = [("1,024 B, none, on", "1,024 B, no checkpoints, spare segment"),
             ("1,024 B, none, off", "1,024 B, no checkpoints, no spare"),
             ("64 B, none, on", "64 B, no checkpoints, spare segment"),
             ("64 B, none, off", "64 B, no checkpoints, no spare"),
             ("1,024 B, every 1 s, off (control)", "1,024 B, checkpoint every 1 s (control)")]

    def p(title, metric, xlim, ticks):
        rows, data = [], []
        for name, label in names:
            b = cell(cells, metric=metric, cell=name, build="base")
            a = cell(cells, metric=metric, cell=name, build="change")
            rows.append(dict(label=label, annot=f"{b['table_text']} → {a['table_text']}"))
            data.append(dict(series=[dict(BEFORE, range=tuple(b["values"]), ends=True),
                                     dict(AFTER, range=tuple(a["values"]), ends=True)]))
        return dict(title=title, rows=rows, columns=[dict(data=data)], xlim=xlim, ticks=ticks,
                    ticklabels=[f"{v:,}" for v in ticks], xlabel="µs (axis from zero)",
                    annot_title="table (µs): before → after")

    return dict(
        title="Segments released off the writer's thread (ADR 0059): gain and cost",
        subtitle=[("main after 1.0.0 (with ADR 0058) against the change. Linux x86-64, 60-second runs at steady-50 (half "
                   "of an unpaced probe's rate), 6 strictly alternating pairs per cell. Each bar runs from the smallest to the "
                   "largest of a build's 6 runs; the single runs in between are not published.", False)],
        legend=[dict(line=True, marker="o", hollow=True, text="before: base build"),
                dict(line=True, marker="s", text="after: the change (Linux only)")],
        panels=[p("Gain: the roll's append, median", "roll append, median", (0, 1100), [0, 250, 500, 750, 1000]),
                p("Cost: all appends' p99.99", "append p99.99", (0, 14), [0, 2, 4, 6, 8, 10, 12, 14])],
        label_w=2.95, annot_w=1.6,
        caption=[
            "Google Cloud c4-standard-8-lssd (Xeon 6985P-C), ext4 on local NVMe SSD, 128 MiB segments; spare segment: "
            "WriterConfig::spare_segment. At 64 B without the spare segment p99.99 was higher in all 6 pairs (about "
            "0.5 µs, not explained); in the other cells within the base-to-base spread. Round 1 (both architectures, "
            "one run per build) is in the section.",
            "Section: docs/benchmarks.md, \"2026-11-26, finished segments released off the writer's thread\", round 2. "
            "Data: docs/charts/data/since-1.0.0.json (from the published table).",
        ])


def seek(since):
    cells = [c for c in since["cells"] if c["chart"] == "seek"]
    groups = [(p, r) for p in ("64 B", "1,024 B") for r in ("safe", "zero-copy")]
    targets = ["1/64 in", "middle", "last record"]

    def series(c, base):
        med, lo, hi = c["values"]
        return dict(base, runs=[med], range=(lo, hi), jitter=False)

    panels = []
    for state, n in (("warm", 50), ("cold", 16)):
        rows = [dict(group=f"{p}, {r} reader", label=t) for p, r in groups for t in targets]
        columns = []
        for m in MACHINES:
            data = []
            for p, r in groups:
                for t in targets:
                    sel = dict(machine=m, payload=p, reader=r, target=t, state=state)
                    data.append(dict(series=[series(cell(cells, index="without index", **sel), BEFORE),
                                             series(cell(cells, index="with index", **sel), AFTER)]))
            columns.append(dict(heading=m, data=data))
        panels.append(dict(title=f"Seek and first record in a sealed 128 MiB segment, {state} (n = {n} per cell)",
                           rows=rows, columns=columns, scale="log", xlim=(30, 4e5), ticks=[100, 1000, 10000, 100000],
                           ticklabels=["100 µs", "1 ms", "10 ms", "100 ms"],
                           xlabel="logarithmic scale; targets: 1/64 of the way in, the middle, the last record"))
    rows = [dict(group=p, label=f"{s} (n = {n})") for p in ("64 B", "1,024 B") for s, n in (("warm", 10), ("cold", 5))]
    columns = []
    for m in MACHINES:
        data = []
        for p in ("64 B", "1,024 B"):
            for s in ("warm", "cold"):
                c = cell(cells, metric="build the indexes of 1 GiB (8 segments)", machine=m, payload=p, state=s)
                data.append(dict(series=[series(c, dict(AFTER, dy=0.0))]))
        columns.append(dict(heading=m, data=data))
    panels.append(dict(title="Cost: building the indexes of 8 sealed segments (1 GiB)", rows=rows, columns=columns,
                       scale="log", xlim=(5000, 4e6), ticks=[10000, 100000, 1000000],
                       ticklabels=["10 ms", "100 ms", "1 s"],
                       xlabel="logarithmic scale; the index files take 49,256 bytes per 128 MiB segment"))
    return dict(
        title="Seeks with and without the seek index (new in 1.1, off by default)",
        subtitle=[("main after 1.0.0; lower is better. Mark: the median; line: min–max of the n runs (the single runs "
                   "are not published). With the index, the records before the place a seek lands on are not read, "
                   "so damage there goes unnoticed.", False)],
        legend=[dict(marker="o", hollow=True, text="without the seek index"),
                dict(marker="s", text="with the seek index, and building it")],
        panels=panels, label_w=2.35,
        caption=[
            "Google Cloud VMs: x86-64 c4-standard-4-lssd, aarch64 c4a-standard-4-lssd; ext4 on local NVMe SSD; each "
            "value is the first seek of a new process; cold: page cache dropped before the process. The newest "
            "segment is never indexed: seeks there are the same with and without the option (in the section, not "
            "drawn).",
            "Section: docs/benchmarks.md, \"2026-11-26, seeks with and without the seek index\". Data: "
            "docs/charts/data/since-1.0.0.json (from the published tables).",
        ])


CHARTS = {
    "rerun-throughput": lambda r, s: throughput(r),
    "rerun-w2-p999": lambda r, s: w2(r),
    "rerun-probe": lambda r, s: probe(r),
    "rerun-paced": lambda r, s: paced(r),
    "since-adr0058": lambda r, s: adr0058(s),
    "since-adr0059": lambda r, s: adr0059(s),
    "since-seek-index": lambda r, s: seek(s),
}


def main():
    rerun, since = load("rerun-v1.0.0.json"), load("since-1.0.0.json")
    for name, build in CHARTS.items():
        spec = build(rerun, since)
        for theme in THEMES:
            path = os.path.join(HERE, f"{name}-{theme}.svg")
            render(spec, theme, path)
            print(f"{os.path.relpath(path)}  {os.path.getsize(path):,} bytes")


if __name__ == "__main__":
    main()
