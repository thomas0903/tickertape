# Performance charts

The charts of the README's Performance section and of [`docs/benchmarks.md`](../benchmarks.md), each in a light and a
dark variant (`<name>-light.svg`, `<name>-dark.svg`). They are generated; do not edit the SVGs by hand.

| File | What it shows | Shown in |
|---|---|---|
| `make.py` | Draws every chart from the data files, and nothing else | |
| `check_tables.py` | Checks the data files against the published tables, cell by cell | |
| `export_rerun.py` | One-off: exported `data/rerun-v1.0.0.json` from the raw results of the rerun, which are not in the repository | |
| `data/rerun-v1.0.0.json` | Every run of every cell of the README's comparison tables (ext4, pinned), unrounded, with machine, payload, tool, reader, n and unit | |
| `data/since-1.0.0.json` | The before and after values of the since-1.0.0 charts, typed from the tables of `docs/benchmarks.md`; each cell names its table row and column | |
| `rerun-throughput-*.svg` | W1 append and W3 warm replay, million records/s | README (throughput table), benchmarks.md (rerun, W1 and W3) |
| `rerun-w2-p999-*.svg` | W2 reader p99.9, logarithmic axis | README (latency table), benchmarks.md (rerun, W2) |
| `rerun-probe-*.svg` | Ten-minute unpaced probe | README (ten-minute table), benchmarks.md (rerun, sustained) |
| `rerun-paced-*.svg` | Ten-minute SV and SD, each tool at its own target rate | README (ten-minute table), benchmarks.md (rerun, sustained) |
| `since-adr0058-*.svg` | Early write-back to the folio boundary: gains and costs | benchmarks.md (ADR 0058 section) |
| `since-adr0059-*.svg` | Segments released off the writer's thread: gain and cost | benchmarks.md (ADR 0059 section, round 2) |
| `since-seek-index-*.svg` | Seeks with and without the seek index, and building it | benchmarks.md (seek index section) |

## Regenerate

```sh
uv run docs/charts/make.py          # Python 3.12, matplotlib 3.10.3, numpy 2.2.6 (pinned in the script's header)
python3 docs/charts/check_tables.py # standard library only; exits 1 on any mismatch
```

`make.py` reads only the two data files. With the pinned versions a rerun writes byte-identical files: the SVG ids
are salted with a fixed string, no date or creator is written, and coordinates are rounded to 0.01 pt. Text is kept as
SVG text in DejaVu Sans with the viewer's sans-serif as fallback (it keeps the files at 25–62 KB); the layout leaves
room for DejaVu Sans, the widest of the usual fallbacks.

`check_tables.py` rounds the smallest and largest run of every comparison cell as the tables do (half up; three
significant digits, two decimals for million records/s and for milliseconds, whole records/s) and compares the result
with the README's tables and the matching rows of `docs/benchmarks.md`; for the since-1.0.0 data it finds the named row
and column and compares the numbers and the text.

## The rules the charts follow

- A chart shows what the table under it shows: every cell, no more favourable and no less complete. Where a table gives
  a range of runs, each run is a point and a line spans min–max; where only a median and range are published, the
  chart says so.
- Linear axes start at zero; a logarithmic axis says so in the image. Titles name what is measured, not a conclusion.
- One colour and one marker per tool in every chart (the categorical palette of the dataviz method, validated for
  colour-blind readers in this order on both backgrounds), and every row carries the tool's name; no colour marks a
  winner. Before and after charts use one ink with a hollow circle (before) and a filled square (after).
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
