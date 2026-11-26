# Issue #62 experiment (never merged)

Switches (environment, read once): `TT_EXP_NO_POPULATE` (the prefault helper runs but advises nothing, so early
write-back stays on), `TT_EXP_RANGED` (the active segment is synced by its ranged `msync` alone, without the two
whole-file `fdatasync` calls), `TT_EXP_WINDOW_KIB` (the prefault window). `build.sh` builds `tt-sustained` from `main`
and from this branch; `remote.sh` runs the matrix; `ckpt.bt` traces the checkpoint calls and device writes.

What ran (2026-11-26): `remote.sh` on the x86-64 VM (150 runs; its traced round on ext4 used a first `ckpt.bt` whose
thread-name filter matched nothing), then `rerun.sh` → `remote2.sh` (the ext4 traced round again with the fixed filter,
which is the `ckpt.bt` here); `remote3.sh` on the second aarch64 VM (configurations A, C, D, E and G; results packed into
one archive). `tables.py` makes the tables of `docs/benchmarks.md`; `summ.py` is a rougher summary.
