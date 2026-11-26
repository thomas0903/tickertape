# Playbook: run a benchmark and record the result

`docs/benchmarks.md` contains **measured numbers only**. A number that isn't there, with its provenance, must not
appear anywhere in the README or docs. Never compare with other tools.

1. **Clean state.** Commit your code first, so the run maps to a clean commit (`git status` shows nothing). Close
   heavy applications. On a laptop, plug in the power and note it.
2. **Build in release mode** (the bench profile does this): `cargo bench -p tickertape-bench --no-run`.
3. **Run** the benchmark, writing raw output to a file outside the repo, e.g.
   `cargo bench -p tickertape-bench -- <filter> 2>&1 | tee /tmp/tickertape-bench-$(date +%Y%m%d-%H%M).txt`.
   For cross-process latency tools, use the exact command printed in `benches/README.md`.
4. **Collect the environment:**
   - macOS: `sw_vers`, `uname -m`, `sysctl -n machdep.cpu.brand_string hw.memsize`, and `diskutil info /` (filesystem).
   - Linux: `uname -r`, `lscpu | head -20`, `free -g`, `df -T <journal dir>`; for OrbStack, say "OrbStack VM" and the
     host Mac model.
   - `rustc --version`, the commit SHA, and the journal directory used.
5. **Record** a new dated section in `docs/benchmarks.md`: environment, exact command, commit, the results table
   (p50/p99/p99.9 and throughput as printed, with units), and how coordinated omission was handled. Keep the old
   sections; results are dated history.
6. **Sanity check.** Run each benchmark at least twice. If runs differ by more than about 10 %, say so and report both.
7. CI runners are smoke runs only (they prove the benchmark runs). Never record their numbers as results.
8. Commit: `bench: record <benchmark> on <machine>`.

## Sustained session (ADR 0026)

Sustained runs last 10 minutes each and a full session about a day, so they run on a cloud Linux machine with
`benches/session.py`, which runs the tickertape scenarios (`tt-sustained`) and the sustained comparison
(`compare/README.md`, "Sustained mode") and keeps everything in one output directory.

1. **Machine.** Cloud measurements run on Google Cloud VMs with local SSD (roadmap, owner decisions of 2026-11-19);
   label them as cloud VMs, never bare metal. Note the machine type, zone and local SSD count, since the environment
   capture only sees the DMI product name. Use the local SSD or the volume you want to measure, formatted without
   lazy initialisation (ext4: `mkfs.ext4 -E lazy_itable_init=0,lazy_journal_init=0`;
   ADR 0025 found a fresh lazily initialised ext4 volume confounding). Mount it, create a directory on it for
   `--journal-dir`, and keep `--out` on another filesystem if possible (e.g. the root volume).
2. **Software.** Rust (the repository's `rust-toolchain.toml`), Python 3.9+, Java 21 and Maven (set `JAVA_HOME`), at a
   clean commit. Optional: `numactl`, and free CPUs for `--cpus` pinning.
3. **Validate the machine** with the quick matrix (a few minutes; functional only, never results):
   `python3 benches/session.py --journal-dir /mnt/test/tt --out ~/session-quick --quick --yes`.
   Check `~/session-quick/manifest.json` (every run `done`, none `failed`) and `logs/`.
4. **Estimate.** `python3 benches/session.py --journal-dir /mnt/test/tt --out ~/session-full --dry-run` prints the
   plan and `estimated_seconds`. Reduce with `--only`, `--scenarios`, `--compare-tools`, `--duration` or `--repeats`
   if the budget requires, and say so when recording.
5. **Run** inside `tmux` or `nohup` so a dropped SSH connection doesn't end it:
   `python3 benches/session.py --journal-dir /mnt/test/tt --out ~/session-full --yes`.
   If it is interrupted (or a run fails), run the same command again: finished runs are skipped, partial output and
   scratch data are removed, and the manifest records every session.
6. **Keep the raw output.** Copy the whole `--out` directory off the machine before deleting it (results, series,
   environment, commands, logs); local SSD contents do not survive the VM. Delete the cloud resources afterwards.
7. **Record** in `docs/benchmarks.md` as for other runs: environment (from `environment.txt`), the session command,
   commit, the tables with units and percentiles as printed, achieved rates and late sends, reader lag, CPU, the
   durability and wait settings of each scenario, and how coordinated omission was handled. Report visibility-only
   and periodic-durability comparisons separately, with the unsupported cases and unequal guarantees from
   `compare/README.md`. Point stalls out with their wall-clock times from `series.jsonl`.
8. **Repeats.** The full matrix runs every paced configuration twice; flag differences above about 10 % as in the
   general procedure's sanity check. A single quick or CI run is never a result.

## Comparison rerun (issue #57)

The planned comparison rerun at v1.0.0 has its own plan: [`rerun-comparison.md`](rerun-comparison.md) (matrix, driver,
machines, cost, what gets published). Its results are in [`benchmarks.md`](../benchmarks.md#2026-11-25-to-2026-11-26-comparison-rerun-at-v100-google-cloud-vms-linux-x86-64-and-aarch64)
(2026-11-25 to 2026-11-26).
