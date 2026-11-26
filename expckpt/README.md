# Issue #62 experiment (never merged)

Switches (environment, read once): `TT_EXP_NO_POPULATE` (the prefault helper runs but advises nothing, so early
write-back stays on), `TT_EXP_RANGED` (the active segment is synced by its ranged `msync` alone, without the two
whole-file `fdatasync` calls), `TT_EXP_WINDOW_KIB` (the prefault window). `build.sh` builds `tt-sustained` from `main`
and from this branch; `remote.sh` runs the matrix; `ckpt.bt` traces the checkpoint calls and device writes.
