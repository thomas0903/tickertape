#!/usr/bin/env bash
# Runs the model of ADR 0048 without the bound under Miri, which must report undefined behaviour in it (a data race or a
# mixed-size one): otherwise the model with the bound, which must pass, would show nothing.
set -uo pipefail
out=$(cargo +nightly miri test -p tickertape --lib -- --ignored map_ro_unbounded_loads_are_reported 2>&1)
status=$?
if [ $status -ne 0 ] && grep -qE 'Undefined Behavior: (Data race|Race condition) detected' <<<"$out"; then
    grep -m1 'Undefined Behavior' <<<"$out"
    exit 0
fi
printf '%s\n' "$out"
echo "Miri did not report the race it must report (exit status $status)"
exit 1
