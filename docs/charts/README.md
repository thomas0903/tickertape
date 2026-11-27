# Performance charts

The charts of the README's Performance section and of [`docs/benchmarks.md`](../benchmarks.md), each in a light and a
dark variant (`<name>-light.svg`, `<name>-dark.svg`). They are generated; do not edit the SVGs by hand.

| File | What it holds or shows | Shown in |
|---|---|---|
| `make.py` | Draws every chart from the data files, and nothing else | |
| `check_tables.py` | Checks the data files against the published tables, cell by cell | |
| `export_rerun.py` | One-off: exported `data/rerun-v1.0.0.json` from the raw results of the rerun, which are not in the repository | |
| `data/rerun-v1.0.0.json` | Every run of the plotted comparison cells, unrounded, with machine, file system, part, payload, tool, reader, n and unit | |
| `data/since-1.0.0.json` | The before and after values of the since-1.0.0 charts, typed from the tables of `docs/benchmarks.md`; each cell names its table row and column | |
| `rerun-throughput-*.svg` | W1-cold, W1-steady (tickertape, Chronicle Queue) and W3 (tickertape zero-copy and safe, Chronicle Queue, yaque); ext4, pinned only: the README table's cells, not the unpinned, tuned, jvm-old, microtouch-only or xfs rows, nor yaque, queue-file and the plain file in W1 | README (throughput table), benchmarks.md (rerun, W1 and W3) |
| `rerun-w2-p999-*.svg` | W2 reader p99.9 of the five tools of the README table (not yaque), ext4, pinned; not p50, p99, max or late sends | README (latency table), benchmarks.md (rerun, W2) |
| `rerun-probe-*.svg` | Ten-minute unpaced probe, records/s, tickertape, Chronicle Queue and Aeron IPC, ext4, pinned, both rounds; not the probe's latencies | README (ten-minute table), benchmarks.md (rerun, sustained) |
| `rerun-paced-*.svg` | SV reader p99.9 and maximum (ext4 both rounds, xfs one round, the plain file's separate ext4 run) and the SD sync median and maximum (ext4), each with its target rate; not SD reader p99.9, the 90 % runs, jvm-old or the tuned boot | benchmarks.md (rerun, above the SV tables) |
| `since-adr0058-*.svg` | Early write-back to the folio boundary: the 10-minute paced cells of the "Writer and device" and "Readers, checkpoints and rolls" tables, for the metrics named in its panels; not the unpaced cells, the W2 cells or the gate figures that stayed within the base's range | benchmarks.md (ADR 0058 section) |
| `since-adr0059-*.svg` | Segments released off the writer's thread: round 2 (x86-64, 6 pairs per cell), roll append median and append p99.99; not round 1, the faults or macOS | benchmarks.md (ADR 0059 section, round 2) |
| `since-seek-index-*.svg` | Seeks into a sealed segment, warm and cold, and building the indexes; not the newest segment (in the caption), forward seeks or the writer's open | benchmarks.md (seek index section) |

## Regenerate

```sh
uv run docs/charts/make.py          # Python 3.12, matplotlib 3.10.3, numpy 2.2.6 (pinned in the script's header)
python3 docs/charts/check_tables.py # standard library only; exits 1 on any mismatch
```

`make.py` reads only the two data files. With the pinned versions a rerun writes byte-identical files: the SVG ids
are salted with a fixed string, no date or creator is written, and coordinates are rounded to 0.01 pt. Text is kept as
SVG text in DejaVu Sans with the viewer's sans-serif as fallback (it keeps the files at 25–90 KB); the layout leaves
room for DejaVu Sans, the widest of the usual fallbacks.

`check_tables.py` rounds the smallest and largest run of every comparison cell as the tables do (half up; three
significant digits, two decimals for million records/s and for milliseconds, whole records/s) and compares the result
with the README's tables and the matching rows of `docs/benchmarks.md`, every ten-minute run with its row of the
sustained tables; for the since-1.0.0 data it finds the named row and column and compares the numbers and the text. It
checks values, not pictures: it does not replace looking at both variants of every chart at README and phone width
(clipping, overplotting, labels that run into each other).

## The rules the charts follow

- A chart shows what the table under it shows: every cell, no more favourable and no less complete. Where a table gives
  a range of runs, each run is a point and a line spans min–max; where only a median and range are published, the
  chart says so.
- Linear axes start at zero; a logarithmic axis says so in the image. A run beyond the axis is an arrowhead at the edge
  with its value, never left out. Titles name what is measured, not a conclusion. The scope (file system,
  configuration, code) is near the top of the image.
- One colour and one marker per tool in every chart (the categorical palette of the dataviz method, in an order whose
  neighbours pass its colour-blind and normal-vision checks on both backgrounds; in the light theme every mark has at
  least 3:1 contrast on white), and every row carries the tool's name; no colour marks a winner. Before and after charts use one ink with a hollow circle (before) and a filled square (after).
- n, the machine and the code (v1.0.0 or main after 1.0.0) are in the image; the ten-minute paced chart prints each
  tool's target rate beside its marks and says that the loads differ.
- In Markdown, embed both variants with `<picture>` and `prefers-color-scheme`, as GitHub documents, with alt text that
  says what the chart shows and who leads where.

## Add a chart

1. Put its values in a data file under `data/` (or a new one), each cell with its unit, n, machine, the section of
   `docs/benchmarks.md` it belongs to, and either the runs or the published text it was typed from.
2. Extend `check_tables.py` so that every value is compared with its table, and run it.
3. Write a function in `make.py` that returns a chart spec (title, subtitle, panels of rows, caption) and add it to
   `CHARTS`; run `make.py`, look at both variants at README width and on a phone, and check that a second run changes
   no file.
4. Embed it above its table:

```html
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="charts/NAME-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="charts/NAME-light.svg">
  <img alt="What it shows and who leads where." src="charts/NAME-light.svg">
</picture>
```
