# Issue #60 experiment scripts (never merged)

Run on a VM with ext4 on `/data` and this repository in `/data/tickertape`: `bash setup.sh` (tracer, all builds), then the
`m*.sh` matrices with all files of this directory in `$HOME`. Results go to `~/r/<run>`; `sum.py ~/r/m1-*` summarises,
`ana.py RUN` groups a traced run's off-CPU waits by stack. Switches: `TT_EXP_SPARE=off|sync|nosync`,
`TT_EXP_WINDOW_KIB=<n>`, `TT_EXP_RANGED=1` (no whole-file `fdatasync` of the active segment at a checkpoint).
