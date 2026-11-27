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
import math
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

# Colours: the categorical palette of the dataviz method, in an order whose adjacent pairs pass its CVD and
# normal-vision checks. Light: three hues are a step darker than the method's defaults so that every mark has at least
# 3:1 contrast on #ffffff; dark: the method's dark steps on #0d1117 (GitHub's dark background). Every row also
# carries its tool's name as text and its own marker shape, so no reading depends on colour.
THEMES = {
    "light": dict(bg="#ffffff", ink="#1f2328", ink2="#59636e", grid="#e4e7eb", axis="#b8bec6",
                  tools={"tickertape": "#257de0", "tickertape safe": "#00967a", "Chronicle Queue": "#d9591a",
                         "Aeron IPC": "#6a4ce2", "plain file": "#d6457f", "yaque": "#008a17"}),
    "dark": dict(bg="#0d1117", ink="#e6edf3", ink2="#9198a1", grid="#262c36", axis="#4a525c",
                 tools={"tickertape": "#338ced", "tickertape safe": "#00a888", "Chronicle Queue": "#e76624",
                        "Aeron IPC": "#9887ef", "plain file": "#e24d8b", "yaque": "#0aac23"}),
}
MARKERS = {"tickertape": "o", "tickertape safe": "s", "Chronicle Queue": "D", "Aeron IPC": "^", "plain file": "v",
           "yaque": "P"}

W = 6.4             # figure width, inches (614 px at 96 px per inch)
RH = 0.24           # one row, inches
GROUP_GAP = 0.45    # rows of space between groups
COL_GAP = 0.25
MARGIN = 0.1
F_TITLE, F_SUB, F_PANEL, F_LABEL, F_TICK, F_NOTE, F_CAPTION = 12.5, 10, 10.5, 10, 9.5, 9.5, 9
ADV = 0.6           # average advance of DejaVu Sans on this kind of text, in em


def wrap(text, size, width_in=W - 2 * MARGIN):
    return textwrap.wrap(text, int(width_in * 72 / (size * ADV)))


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
    rh = spec.get("row_h", RH)
    subtitle = [(line, bold) for text, bold in spec.get("subtitle", []) for line in wrap(text, F_SUB)]
    caption = [line for text in spec["caption"] for line in wrap(text, F_CAPTION)]
    top = 0.38 + 0.21 * len(subtitle) + (0.3 if spec.get("legend") else 0) + 0.06
    bottom = 0.12 + 0.18 * len(caption)
    panels = spec["panels"]
    layout = []
    for i, p in enumerate(panels):
        ys, heads = row_positions(p["rows"], headers)
        notes = [line for text in p.get("notes", []) for line in wrap(text, F_NOTE)]
        show_head = len(p["columns"]) > 1 and (i == 0 or spec.get("headings_every_panel"))
        show_x = i == len(panels) - 1 or panels[i + 1]["xlabel"] != p["xlabel"]
        head_h = 0.32 + 0.19 * len(notes) + (0.24 if show_head else 0) + (0.17 if p.get("annot_title") else 0)
        ax_h = (max(ys) + 1.2) * rh
        foot = 0.5 if show_x else 0.3
        layout.append((ys, heads, notes, show_head, show_x, head_h, ax_h, head_h + ax_h + foot))
    H = top + sum(x[-1] for x in layout) + 0.12 * (len(layout) - 1) + bottom
    fig = plt.figure(figsize=(W, H), facecolor=t["bg"])

    def text(x_in, y_in, s, **kw):
        kw.setdefault("color", t["ink"])
        kw.setdefault("va", "baseline")
        fig.text(x_in / W, 1 - y_in / H, s, **kw)

    text(MARGIN, 0.27, spec["title"], fontsize=F_TITLE, fontweight="bold")
    y = 0.27
    for line, bold in subtitle:
        y += 0.21
        text(MARGIN, y, line, fontsize=F_SUB, fontweight="bold" if bold else "normal")
    if spec.get("legend"):
        y += 0.3
        draw_legend(fig, t, spec["legend"], y, H)
    cursor = top
    plot_left, plot_right = label_w, W - 0.12 - annot_w
    for p, (ys, heads, notes, show_head, show_x, head_h, ax_h, h) in zip(panels, layout):
        ncols = len(p["columns"])
        col_w = (plot_right - plot_left - COL_GAP * (ncols - 1)) / ncols
        text(MARGIN, cursor + 0.24, p["title"], fontsize=F_PANEL, fontweight="bold")
        yy_note = cursor + 0.24
        for line in notes:
            yy_note += 0.19
            text(MARGIN, yy_note, line, fontsize=F_NOTE, color=t["ink"])
        ax_top = cursor + head_h
        for ci, col in enumerate(p["columns"]):
            left = plot_left + ci * (col_w + COL_GAP)
            ax = fig.add_axes([left / W, 1 - (ax_top + ax_h) / H, col_w / W, ax_h / H])
            style_axes(ax, t, p)
            ax.set_ylim(max(ys) + 0.6, -0.6)
            if show_head and col.get("heading"):
                text(left + col_w / 2, ax_top - 0.08 - (0.17 if p.get("annot_title") else 0), col["heading"],
                     fontsize=F_LABEL, fontweight="bold", ha="center")
            for yy, row, data in zip(ys, p["rows"], col["data"]):
                draw_row(ax, t, yy, row, data, p["xlim"])
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
                    text(left + col_w + 0.1, ax_top - 0.06, p["annot_title"], fontsize=F_NOTE, color=t["ink2"])
                for yy, row in zip(ys, p["rows"]):
                    if row.get("annot"):
                        ax.text(1 + 0.1 / col_w, yy, row["annot"], transform=tr, ha="left", va="center",
                                fontsize=F_NOTE, color=t["ink"])
        if show_x:
            text((plot_left + plot_right) / 2, ax_top + ax_h + 0.43, p["xlabel"], ha="center", fontsize=F_NOTE,
                 color=t["ink2"])
        cursor += h + 0.12
    y = H - bottom + 0.06
    for line in caption:
        y += 0.18
        text(MARGIN, y, line, fontsize=F_CAPTION, color=t["ink2"])
    buf = io.StringIO()
    fig.savefig(buf, format="svg", facecolor=t["bg"], metadata={"Date": None, "Creator": None})
    plt.close(fig)
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(compact(buf.getvalue()))
    return H


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


def offsets(n, span=None):
    if n <= 1:
        return [0.0]
    span = span if span is not None else (0.5 if n > 2 else 0.3)
    return [-span / 2 + span * i / (n - 1) for i in range(n)]


def draw_row(ax, t, y, row, data, xlim):
    """One row: each series is points (runs, spread vertically so that none hides another), a line from the smallest
    to the largest, or a median with its range. A run beyond the axis is drawn as an arrowhead at the edge with its
    value; a text-only row carries a label."""
    if data is None:
        return
    if data.get("text"):
        ax.text(data["text_x"], y, data["text"], ha=data.get("ha", "left"), va="center", fontsize=F_NOTE - 1,
                color=t["ink"], clip_on=False)
        return
    key = row.get("key", "")
    for s in data.get("series", [data]):
        c = t["tools"].get(s.get("key", key), t["ink"])
        mk = s.get("marker", MARKERS.get(key, "o"))
        yy = y + s.get("dy", 0.0)
        hollow = s.get("hollow", False)
        face, edge = (t["bg"], c) if hollow else (c, t["bg"])
        mark = dict(marker=mk, linestyle="none", markersize=s.get("size", 5.8), markerfacecolor=face, markeredgecolor=edge,
                    markeredgewidth=1.3 if hollow else 0.9, zorder=4, clip_on=False)
        if s.get("range"):
            lo, hi = s["range"]
            ax.plot([lo, hi], [yy, yy], color=c, linewidth=1.4, solid_capstyle="butt", zorder=3)
            if s.get("ends"):
                ax.plot([lo, hi], [yy, yy], color=c, **mark)
            if s.get("caps"):
                ax.plot([lo, hi], [yy, yy], color=c, marker="|", markersize=8, markeredgewidth=1.4,
                        linestyle="none", zorder=3)
        runs = [v for v in s.get("runs", []) if v <= xlim[1]]
        over = [v for v in s.get("runs", []) if v > xlim[1]]
        if runs:
            if len(runs) > 1 and not s.get("no_line"):
                ax.plot([min(runs), max(runs)], [yy, yy], color=c, linewidth=1.4, alpha=0.85,
                        solid_capstyle="butt", zorder=3)
            offs = offsets(len(runs), s.get("span")) if s.get("jitter", True) else [0.0] * len(runs)
            ax.plot(runs, [yy + o for o in offs], color=c, **mark)
        if over:
            edge_x = xlim[1]
            ax.plot([edge_x], [yy], color=c, **dict(mark, marker=">"))
            ax.annotate(s["over_label"], (edge_x, yy), xytext=(-7, 0), textcoords="offset points", ha="right",
                        va="center", fontsize=F_NOTE - 1, color=t["ink"])


def legend_mark(t, x, yf, item):
    return Line2D([x / W], [yf], marker=item["marker"], markersize=5.8, linestyle="none", color=t["ink"],
                  markerfacecolor=t["bg"] if item.get("hollow") else t["ink"],
                  markeredgewidth=1.3 if item.get("hollow") else 0.9,
                  markeredgecolor=t["ink"] if item.get("hollow") else t["bg"])


def draw_legend(fig, t, items, y_in, H):
    x = MARGIN
    yf = 1 - (y_in - 0.05) / H
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
        x += len(item["text"]) * F_NOTE * ADV / 72 + 0.28


# ----------------------------------------------------------------------------------------------------------------
# The charts

def load(name):
    with open(os.path.join(HERE, "data", name), encoding="utf-8") as f:
        return json.load(f)


MACHINES = ["x86-64", "aarch64"]
VMS = ("Google Cloud VMs: x86-64 c4-standard-16-lssd (Xeon 6985P-C), aarch64 c4a-standard-8-lssd (Neoverse-V2); "
       "local NVMe SSD; each measured thread pinned.")
V100 = "Library v1.0.0 (session commit fb2d91b0, 2026-11-25 and 26)."
RERUN_DATA = "Data: docs/charts/data/rerun-v1.0.0.json; tables: README.md and docs/benchmarks.md (comparison rerun)."
SCOPE = "ext4, pinned: the README's comparison cells (the tables also have xfs, unpinned, tuned and jvm-old rows)."


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
            ("W3 warm replay, 64 B", ["W3"], w3_tools, 64, 40, 10),
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
                           ticklabels=[str(v) for v in ticks], xlabel="million records/s (each axis from zero)"))
    return dict(
        title="Append (W1) and warm replay (W3) throughput, v1.0.0",
        subtitle=[(SCOPE, False),
                  ("Higher is better. W1 counts appends visible to readers, not durable ones. Each of the n = 4 runs "
                   "of a cell is a point; the line spans min–max.", False)],
        panels=panels, label_w=1.95,
        caption=[
            "n = 4 per cell: two rounds of two runs. " + VMS + " " + V100,
            "x86-64 W1-cold at 64 B: the first repeat of each round ran at 29.12–30.84, the second at 19.11–20.07 "
            "(cause not traced). yaque, queue-file and the plain file appended more slowly than both journals in "
            "every W1 cell (not drawn).",
            RERUN_DATA,
        ])


def w2(rerun):
    cells = rerun["cells"]
    tools = [("tickertape", "zero-copy"), ("tickertape", "safe"), ("Chronicle Queue", None), ("Aeron IPC", None),
             ("plain file", None)]
    xlim = (0.2, 100)
    ticks = [0.5, 1, 2, 5, 10, 20, 50]
    panels = []
    for payload, rate in ((64, 10000), (64, 100000), (1024, 10000), (1024, 100000)):
        rows = [dict(label=t if r is None else f"{t}, {r}", key=tool_key(t, r), sel=dict(tool=t, reader=r))
                for t, r in tools]
        columns = [dict(heading=m, data=[]) for m in MACHINES]
        notes = {}
        for col, m in zip(columns, MACHINES):
            for row in rows:
                c = cell(cells, chart="w2", machine=m, payload=payload, rate=rate, **row["sel"])
                assert c["n"] == len(c["runs"]) == 4
                runs = c["runs"]
                over = [v for v in runs if v > xlim[1]]
                d = dict(runs=runs)
                if over:
                    inside = sorted(v for v in runs if v <= xlim[1])
                    d["over_label"] = f"{len(over)} run{'s' if len(over) > 1 else ''}: " + ", ".join(
                        f"{v / 1000:.2f} ms" for v in over)
                    notes[(m, row["label"])] = (math.sqrt(inside[0] * inside[-1]),
                                                f"↑ {len(inside)} runs: {inside[0]:.2f}–{inside[-1]:.2f} µs")
                col["data"].append(d)
        # a text row under any row with a run beyond the axis, so that both groups of runs are labelled
        for label in [lab for (_, lab) in notes]:
            i = next(k for k, r in enumerate(rows) if r["label"] == label) + 1
            rows.insert(i, dict(label="", key=""))
            for col, m in zip(columns, MACHINES):
                x, txt = notes.get((m, label), (None, None))
                col["data"].insert(i, dict(text=txt, text_x=x, ha="center") if txt else None)
        panels.append(dict(title=f"{payload:,} B at {rate:,} records/s", rows=rows, columns=columns, scale="log",
                           xlim=xlim, ticks=ticks, ticklabels=[f"{v:g}" for v in ticks],
                           xlabel="reader p99.9 in µs, logarithmic scale (each axis 0.2–100 µs)"))
    return dict(
        title="Reader latency p99.9 (W2), v1.0.0",
        subtitle=[(SCOPE, False),
                  ("From each record's scheduled send to the reader's validated read; lower is better. Logarithmic "
                   "axis. Each of the n = 4 runs of a cell is a point; the line spans min–max. Tools in the table's "
                   "column order.", False)],
        panels=panels, label_w=1.95,
        caption=[
            "n = 4 per cell: two rounds of two runs, each 1,000,000 records measured after 100,000 unmeasured. "
            + VMS + " " + V100,
            "One run lies beyond the axis and is drawn as an arrowhead at its edge with its value (Chronicle Queue, "
            "x86-64, 1,024 B, 10,000/s). Aeron IPC writes no durable journal; the plain file has no checksum or "
            "recovery. yaque (5.76 µs to 18.30 ms in these cells) is not drawn, as in the table.",
            RERUN_DATA,
        ])


def probe(rerun):
    cells = rerun["cells"]
    tools = ["tickertape", "Chronicle Queue", "Aeron IPC"]
    panels = []
    for payload, xmax, step, notes in (
            (64, 8, 1, []),
            (1024, 4.5, 0.5, ["Both journals are at about the SSD's limit; Chronicle Queue is above tickertape in "
                              "both rounds on both machines, by 1.1–1.6 % (too small to see here)."])):
        rows = [dict(label=t, key=t) for t in tools]
        columns = []
        for m in MACHINES:
            data = []
            for t in tools:
                c = cell(cells, chart="ten-minute", workload="probe", machine=m, payload=payload, tool=t, fs="ext4",
                         part="sustained")
                assert [r["round"] for r in c["runs"]] == [1, 2]
                data.append(dict(runs=[r["records_per_s"] / 1e6 for r in c["runs"]]))
            columns.append(dict(heading=m, data=data))
        ticks = [round(i * step, 1) for i in range(int(xmax / step) + 1)][::2 if payload == 1024 else 1]
        panels.append(dict(title=f"Unpaced probe, {payload:,} B", notes=notes, rows=rows, columns=columns,
                           xlim=(0, xmax), ticks=ticks, ticklabels=[f"{v:g}" for v in ticks],
                           xlabel="million records/s, unpaced, a reader following (each axis from zero)"))
    return dict(
        title="Ten-minute runs: unpaced probe, v1.0.0",
        subtitle=[(SCOPE, False),
                  ("Records/s appended in 10 minutes with a reader following; higher is better. One point per round "
                   "(n = 1 per round, two rounds). Aeron IPC is not a durable journal: its term buffers were files "
                   "on the same SSD.", False)],
        panels=panels, label_w=1.95,
        caption=[
            "The probe is the only ten-minute row in which every tool answers the same question. One local SSD "
            "writes at most 351 MB/s by itself. tickertape ran its zero-copy reader. " + VMS + " " + V100,
            RERUN_DATA,
        ])


def paced(rerun):
    cells = rerun["cells"]
    xlim, ticks = (2, 2e6), [10, 100, 1000, 10000, 100000, 1000000]

    def targets(c):
        return "; ".join(f"{r['target_records_per_s']:,}" for r in c["runs"])

    def series(c, value, maxkey):
        n = len(c["runs"])
        # the filled p99.9 (or median) mark is drawn larger than the hollow maximum: it is the figure the table leads with
        return dict(series=[dict(runs=[r[value] for r in c["runs"]], no_line=True, dy=-0.2, span=0.14 if n > 1 else 0,
                                 size=7.4),
                            dict(runs=[r[maxkey] for r in c["runs"]], hollow=True, no_line=True, dy=0.2,
                                 span=0.14 if n > 1 else 0)])

    sv_rows, sv_data = [], []
    for m in MACHINES:
        for p in (64, 1024):
            for tool, fs, part, label in (
                    ("tickertape", "ext4", "sustained", "tickertape, ext4"),
                    ("tickertape", "xfs", "sustained-journals", "tickertape, xfs, 1 run"),
                    ("Chronicle Queue", "ext4", "sustained", "Chronicle Queue, ext4"),
                    ("Chronicle Queue", "xfs", "sustained-journals", "Chronicle Queue, xfs, 1 run"),
                    ("Aeron IPC", "ext4", "sustained", "Aeron IPC, ext4"),
                    ("plain file", "ext4", "sustained-others", "plain file, ext4, 1 run *")):
                c = cell(cells, chart="ten-minute", workload="SV", machine=m, payload=p, tool=tool, fs=fs, part=part)
                sv_rows.append(dict(group=f"{m}, {p:,} B", label=label, key=tool, annot=targets(c)))
                sv_data.append(series(c, "reader_p99_9_us", "reader_max_us"))
    sd_rows, sd_data = [], []
    for m in MACHINES:
        for p in (64, 1024):
            for tool, label in (("tickertape", "tickertape checkpoint"),
                                ("Chronicle Queue", "Chronicle store flush")):
                c = cell(cells, chart="ten-minute", workload="SD", machine=m, payload=p, tool=tool, fs="ext4",
                         part="sustained")
                sd_rows.append(dict(group=f"{m}, {p:,} B", label=label, key=tool, annot=targets(c)))
                sd_data.append(series(c, "sync_p50_us", "sync_max_us"))
    common = dict(scale="log", xlim=xlim, ticks=ticks, ticklabels=[us_label(v) if i % 2 == 0 else "" for i, v in
                                                                     enumerate(ticks)],
                  xlabel="logarithmic scale", annot_title="target records/s")
    return dict(
        title="Ten-minute paced runs, pinned, v1.0.0",
        subtitle=[("Each tool at half of its own unpaced rate; not the same load.", True),
                  ("SV on ext4 and xfs, SD on ext4. Each mark's target rate is printed beside it. Rows without a mark: "
                   "one run per round, two rounds. \"1 run\": a single run, on xfs in its own part of the session, "
                   "and for the plain file (*) in a separate part, later. Lower is better.", False)],
        panels=[
            dict(title="SV, no syncs: reader p99.9 (filled, upper) and max (hollow, lower)", rows=sv_rows,
                 columns=[dict(data=sv_data)], **common),
            dict(title="SD on ext4, a sync every second: duration, median (filled), max (hollow)",
                 notes=["Not the same work: tickertape's early write-back has started writing most of the second's "
                        "data before the checkpoint begins. Without early write-back a tickertape checkpoint became "
                        "durable 0.93 s after its request (later tickertape-only runs at about 90 % of the device's "
                        "rate)."], rows=sd_rows, columns=[dict(data=sd_data)], **common),
        ],
        label_w=2.35, annot_w=1.55, group_headers=True, row_h=0.28,
        caption=[
            "The targets are half of each tool's own probe in the same round and configuration. tickertape: "
            "zero-copy reader; in SD the blocking Writer::checkpoint(). Chronicle Queue: ExcerptAppender.sync with "
            "SyncMode.SYNC (msync of its store). Aeron IPC has no sync (no SD rows). The SD reader p99.9 is in the "
            "table below.",
            "Both JVM tools did better in these rows with their old settings (the jvm-old rows of the table): "
            "Chronicle's SD flush median 97.98–119.87 ms, Aeron's 64 B SV reader p99.9 4.66–6.32 µs.",
            VMS + " " + V100 + " Data: docs/charts/data/rerun-v1.0.0.json; tables below.",
        ])


# --- since 1.0.0 ---------------------------------------------------------------------------------------------------

BEFORE = dict(key="before", marker="o", hollow=True, dy=-0.16)
AFTER = dict(key="after", marker="s", hollow=False, dy=0.16)


def adr0058(since):
    cells = [c for c in since["cells"] if c["chart"] == "adr0058"]

    def pct_range(items):
        """How many fewer device bytes the change wrote, in % of the base's (derived from the table's ratios)."""
        vals = []
        for machine, cellname in items:
            b = cell(cells, metric="device bytes per appended byte", machine=machine, cell=cellname, build="base")
            a = cell(cells, metric="device bytes per appended byte", machine=machine, cell=cellname, build="change")
            vals += [(1 - y / x) * 100 for x in b["values"] for y in a["values"]]
        return f"{min(vals):.1f}–{max(vals):.1f} % by the table's ratios"

    def p(title, items, xlabel, scale, xlim, ticks, ticklabels, transform=lambda v: v, annot="table: before → after",
          unit="", notes=()):
        rows, data = [], []
        for metric, machine, cellname, group, label in items:
            b = cell(cells, metric=metric, machine=machine, cell=cellname, build="base")
            a = cell(cells, metric=metric, machine=machine, cell=cellname, build="change")
            assert len(b["values"]) == b["n"] == 2 and len(a["values"]) == a["n"] == 2

            def u(txt):  # the table's text, with "a ms to b ms" written "a–b ms" and the unit on bare numbers
                txt = re.sub(r"^([\d.,]+) (ms|s) to ([\d.,]+) \2$", r"\1–\3 \2", txt)
                return txt if not unit or txt.endswith(("ms", " s")) else f"{txt} {unit}"
            ann = f"{u(b['table_text'])} → {u(a['table_text'])}"
            rows.append(dict(group=group, label=label, annot=ann))
            data.append(dict(series=[dict(BEFORE, runs=[transform(v) for v in b["values"]], jitter=False),
                                     dict(AFTER, runs=[transform(v) for v in a["values"]], jitter=False)]))
        return dict(title=title, notes=list(notes), rows=rows, columns=[dict(data=data)], scale=scale, xlim=xlim,
                    ticks=ticks, ticklabels=ticklabels, xlabel=xlabel, annot_title=annot)

    s90e, s90x = "steady-90, 1,024 B, ext4", "steady-90, 1,024 B, xfs"
    a64, x1k = "steady-50, 64 B, ext4", "steady-50, 1,024 B, ext4"
    k1, k2 = "x86-64, kernel 6.8 (VM 1)", "x86-64, kernel 6.8 (VM 2)"
    stall, dev, dur = ("seconds with an append of 50 ms or more", "device bytes per appended byte",
                       "background checkpoint, request to durable, median")
    roll = "roll append, median"
    k7, x7 = "aarch64, kernel 7.0", "x86-64, kernel 7.0"
    panels = [
        p("Seconds (of 611) with an append of 50 ms or more",
          [(stall, "aarch64", s90e, k7, "steady-90, 1,024 B, ext4"),
           (stall, "aarch64", s90x, k7, "steady-90, 1,024 B, xfs")],
          "seconds of the 611-second run (axis from zero)", "linear", (0, 160), [0, 40, 80, 120, 160],
          ["0", "40", "80", "120", "160"]),
        p("Extra device writes above 1× appended (%)",
          [(dev, "aarch64", s90e, k7, "steady-90, 1,024 B, ext4"),
           (dev, "aarch64", s90x, k7, "steady-90, 1,024 B, xfs"),
           (dev, "aarch64", a64, k7, "steady-50, 64 B, ext4"),
           (dev, "x86-64", x1k, x7, "steady-50, 1,024 B, ext4"),
           (dev, k1, x1k, "x86-64, kernel 6.8 (no large folios)", "VM 1, steady-50, 1,024 B"),
           (dev, k1, a64, "x86-64, kernel 6.8 (no large folios)", "VM 1, steady-50, 64 B"),
           (dev, k2, x1k, "x86-64, kernel 6.8 (no large folios)", "VM 2, steady-50, 1,024 B")],
          "device bytes ÷ appended bytes − 1, in % (axis from zero)", "linear", (0, 10), [0, 2, 4, 6, 8, 10],
          ["0", "2", "4", "6", "8", "10"], transform=lambda v: (v - 1) * 100, annot="table (ratio): before → after",
          notes=["For a user: about 5–8 % fewer bytes written to the device on kernel 7.0 (" + pct_range(
              [("aarch64", s90e), ("aarch64", s90x), ("aarch64", a64), ("x86-64", x1k)]) + "), none on kernel 6.8."]),
        p("Background checkpoint, request to durable, median",
          [(dur, "aarch64", s90e, k7, "steady-90, 1,024 B, ext4"),
           (dur, "aarch64", s90x, k7, "steady-90, 1,024 B, xfs")],
          "logarithmic scale", "log", (300, 1e6), [1000, 10000, 100000, 1000000], ["1 ms", "10 ms", "100 ms", "1 s"],
          unit="µs"),
        p("The roll's append, median",
          [(roll, "aarch64", s90e, k7, "steady-90, 1,024 B, ext4"),
           (roll, "aarch64", s90x, k7, "steady-90, 1,024 B, xfs"),
           (roll, "aarch64", a64, k7, "steady-50, 64 B, ext4"),
           (roll, "x86-64", x1k, x7, "steady-50, 1,024 B, ext4"),
           (roll, k1, x1k, "x86-64, kernel 6.8", "VM 1, steady-50, 1,024 B"),
           (roll, k1, a64, "x86-64, kernel 6.8", "VM 1, steady-50, 64 B"),
           (roll, k2, x1k, "x86-64, kernel 6.8", "VM 2, steady-50, 1,024 B")],
          "µs (axis from zero)", "linear", (0, 400), [0, 100, 200, 300, 400], ["0", "100", "200", "300", "400"],
          unit="µs"),
        p("64 B, aarch64, steady-50, ext4: to durable and reader p99.9",
          [(dur, "aarch64", a64, None, "to durable, median"),
           ("zero-copy reader p99.9", "aarch64", a64, None, "zero-copy p99.9")],
          "µs (axis from zero)", "linear", (0, 1000), [0, 250, 500, 750, 1000], ["0", "250", "500", "750", "1,000"],
          unit="µs"),
        p("1,024 B, x86-64, steady-50, ext4: checkpoint to durable, p99",
          [("background checkpoint, request to durable, p99", "x86-64", x1k, None, "to durable, p99")],
          "ms (axis from zero)", "linear", (0, 7000), [0, 1000, 2000, 3000, 4000, 5000, 6000, 7000],
          ["0", "1", "2", "3", "4", "5", "6", "7"], unit="ms"),
    ]
    return dict(
        title="Early write-back to the folio boundary (ADR 0058)",
        subtitle=[("On main after 1.0.0, on by default there. Base build against the change; it missed ADR 0058's "
                   "first gate for a default and was accepted. 10-minute runs (611 s with the warm-up), n = 2 per "
                   "build, each run a point (two equal runs make one mark). A background checkpoint every second; "
                   "steady-90 and steady-50 are 90 % and 50 % of an unpaced rate measured in the sustained session "
                   "of 2026-11-19 to 22.", False)],
        legend=[dict(marker="o", hollow=True, text="before: base build"),
                dict(marker="s", text="after: the change")],
        panels=panels, label_w=2.05, annot_w=2.2, group_headers=True,
        caption=[
            "Google Cloud VMs with local NVMe SSD: aarch64 c4a-standard-8-lssd and x86-64 c4-standard-16-lssd on "
            "kernel 7.0, whose ext4 and xfs fill 128 KiB folios; kernel 6.8 (ext4 without large folios) on "
            "c4-standard-4-lssd. The unpaced 60-second cells (roll median 849–930 → 347–1,260 µs on aarch64, "
            "286–1,980 → 976–1,980 µs on x86-64) and the W2 cells are in the section. Not compared with the "
            "comparison at v1.0.0.",
            "Section: docs/benchmarks.md, \"2026-11-26, early write-back to the folio boundary\". Data: "
            "docs/charts/data/since-1.0.0.json (from the published tables).",
        ])


def adr0059(since):
    cells = [c for c in since["cells"] if c["chart"] == "adr0059"]
    names = [("1,024 B, none, on", "1,024 B, no checkpoints, spare"),
             ("1,024 B, none, off", "1,024 B, no checkpoints, no spare"),
             ("64 B, none, on", "64 B, no checkpoints, spare"),
             ("64 B, none, off", "64 B, no checkpoints, no spare"),
             ("1,024 B, every 1 s, off (control)", "1,024 B, checkpoint every 1 s")]

    def p(title, metric, xlim, ticks, notes=()):
        rows, data = [], []
        for name, label in names:
            b = cell(cells, metric=metric, cell=name, build="base")
            a = cell(cells, metric=metric, cell=name, build="change")
            rows.append(dict(label=label, annot=f"{b['table_text']} → {a['table_text']}"))
            data.append(dict(series=[dict(BEFORE, range=tuple(b["values"]), ends=True),
                                     dict(AFTER, range=tuple(a["values"]), ends=True)]))
        return dict(title=title, notes=list(notes), rows=rows, columns=[dict(data=data)], xlim=xlim, ticks=ticks,
                    ticklabels=[f"{v:,}" for v in ticks], xlabel="µs (axis from zero)",
                    annot_title="before → after (µs)")

    return dict(
        title="Segments released off the writer's thread (ADR 0059)",
        subtitle=[("On main after 1.0.0 (with ADR 0058), base build against the change; Linux only. x86-64, "
                   "60-second runs at steady-50 (half of an unpaced probe's rate), 6 strictly alternating pairs per "
                   "cell. Each bar runs from the smallest to the largest of a build's 6 runs; the single runs in "
                   "between are not published.", False)],
        legend=[dict(line=True, marker="o", hollow=True, text="before: base build"),
                dict(line=True, marker="s", text="after: the change")],
        panels=[p("The roll's append, median", "roll append, median", (0, 1100), [0, 250, 500, 750, 1000]),
                p("All appends' p99.99", "append p99.99", (0, 14), [0, 2, 4, 6, 8, 10, 12, 14],
                  notes=["At 64 B without the spare segment, higher with the change in all 6 pairs, by about 0.5 µs "
                         "(not explained); in the other cells within the base-to-base spread."])],
        label_w=2.65, annot_w=1.55,
        caption=[
            "Google Cloud c4-standard-8-lssd (Xeon 6985P-C), ext4 on local NVMe SSD, 128 MiB segments; spare: "
            "WriterConfig::spare_segment. Round 1 (both architectures, one run per build) is in the section.",
            "Section: docs/benchmarks.md, \"2026-11-26, finished segments released off the writer's thread\", round 2. "
            "Data: docs/charts/data/since-1.0.0.json (from the published table).",
        ])


def seek(since):
    cells = [c for c in since["cells"] if c["chart"] == "seek"]
    groups = [(p, r) for p in ("64 B", "1,024 B") for r in ("safe", "zero-copy")]
    targets = ["1/64 in", "middle", "last record"]

    def series(c, base):
        med, lo, hi = c["values"]
        return dict(base, runs=[med], range=(lo, hi), caps=True, jitter=False)

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
        panels.append(dict(title=f"Seek and first record, sealed 128 MiB segment, {state} (n = {n})",
                           rows=rows, columns=columns, scale="log", xlim=(30, 4e5), ticks=[100, 1000, 10000, 100000],
                           ticklabels=["100 µs", "1 ms", "10 ms", "100 ms"],
                           xlabel="logarithmic scale; target: 1/64 in, middle, last record"))
    rows = [dict(group=p, label=f"{s} (n = {n})") for p in ("64 B", "1,024 B") for s, n in (("warm", 10), ("cold", 5))]
    columns = []
    for m in MACHINES:
        data = []
        for p in ("64 B", "1,024 B"):
            for s in ("warm", "cold"):
                c = cell(cells, metric="build the indexes of 1 GiB (8 segments)", machine=m, payload=p, state=s)
                data.append(dict(series=[series(c, dict(AFTER, dy=0.0))]))
        columns.append(dict(heading=m, data=data))
    panels.append(dict(title="Building the indexes of 8 sealed segments (1 GiB)", rows=rows, columns=columns,
                       scale="log", xlim=(5000, 4e6), ticks=[10000, 100000, 1000000],
                       ticklabels=["10 ms", "100 ms", "1 s"], xlabel="logarithmic scale"))
    return dict(
        title="Seeks with and without the seek index",
        subtitle=[("Added after 1.0.0, off by default; on main after 1.0.0. Sealed, indexed segments only: the "
                   "newest segment is never indexed. Costs: index files of 49,256 bytes per 128 MiB segment, "
                   "building them (bottom panel), and the records before the place a seek lands on are not read, so "
                   "damage there goes unnoticed. Mark: the median; line with caps: min–max of the n runs (single "
                   "runs not published). Lower is better.", False)],
        legend=[dict(marker="o", hollow=True, text="without the seek index"),
                dict(marker="s", text="with the seek index; building it")],
        panels=panels, label_w=1.9, headings_every_panel=True, group_headers=True,
        caption=[
            "Google Cloud VMs: x86-64 c4-standard-4-lssd, aarch64 c4a-standard-4-lssd; ext4 on local NVMe SSD; each "
            "value the first seek of a new process; cold: page cache dropped before the process. In the newest "
            "segment (not drawn) a seek reads every record before the target with or without the option; cold, at "
            "1/64 in, the safe reader was 0.07–0.08 ms slower with it.",
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
            h = render(spec, theme, path)
            print(f"{os.path.relpath(path)}  {os.path.getsize(path):,} bytes  {W:g} × {h:.2f} in "
                  f"({math.ceil(W * 96)} × {math.ceil(h * 96)} px)")


if __name__ == "__main__":
    main()
