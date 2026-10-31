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
