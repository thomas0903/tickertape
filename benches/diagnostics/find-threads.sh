#!/bin/sh
# Lists the threads of tt-sustained's writer and reader processes, or of the processes given, with the role of each
# thread and its CPU time so far. Part of docs/playbooks/diagnose-latency-with-ebpf.md. Linux only; no root needed.
#
#   benches/diagnostics/find-threads.sh            # a running tt-sustained
#   benches/diagnostics/find-threads.sh PID...     # any process that uses tickertape
#
# Columns: pid, tid, comm (cut to 15 characters by the kernel), CPU seconds (user + system), role.
set -eu

if [ "$#" -eq 0 ]; then
  set -- $(pgrep -f '^[^ ]*tt-sustained (writer|reader) ' || true)
  if [ "$#" -eq 0 ]; then
    echo "no tt-sustained writer or reader is running; pass process ids instead" >&2
    exit 1
  fi
fi
tick=$(getconf CLK_TCK)
printf '%-8s %-8s %-16s %8s  %s\n' pid tid comm cpu_s role
for pid in "$@"; do
  [ -d "/proc/$pid" ] || { echo "no process $pid" >&2; continue; }
  cmdline=$(tr '\0' ' ' < "/proc/$pid/cmdline")
  for task in /proc/"$pid"/task/*; do
    tid=${task##*/}
    comm=$(cat "$task/comm" 2>/dev/null) || continue
    # Fields 14 and 15 of stat (utime, stime) counted after the ")" that ends the command name.
    cpu=$(sed 's/.*) //' "$task/stat" 2>/dev/null | awk -v t="$tick" '{ printf "%.2f", ($12 + $13) / t }') || continue
    case "$comm" in
      tickertape-chec*) role="checkpoint thread (background checkpoints)" ;;
      tickertape-writ*) role="early write-back thread" ;;
      tickertape-pref*) role="prefault thread" ;;
      tickertape-spar*) role="spare segment thread" ;;
      tt-sampler) role="tt-sustained sampler" ;;
      *)
        role=""
        if [ "$tid" = "$pid" ]; then
          case "$cmdline" in
            *"tt-sustained writer "*) role="writer: appends (measuring thread)" ;;
            *"tt-sustained reader "*)
              index=$(printf '%s\n' "$cmdline" | sed -n 's/.*--index \([0-9]*\).*/\1/p')
              role="reader $index: reads (measuring thread)" ;;
          esac
        fi
        ;;
    esac
    printf '%-8s %-8s %-16s %8s  %s\n' "$pid" "$tid" "$comm" "$cpu" "$role"
  done
done
