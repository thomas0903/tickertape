#!/usr/bin/env bash
# Runs tt-sustained and attaches tracers to its writer process as soon as it starts; stops them when the run ends.
# Part of docs/playbooks/diagnose-latency-with-ebpf.md. Linux; the tracers run under sudo.
#
#   benches/diagnostics/trace-sustained.sh TRACE_DIR TOOLS -- target/release/tt-sustained run --dir ... --out ...
#
# TOOLS is a comma-separated list of:
#   events   events.bt: sync calls and segment links with wall-clock times (bpftrace)
#   offcpu   offcpu.bt: off-CPU time by thread and kernel stack, and each wait of at least MIN_MS (bpftrace)
#   sampler  stack-sampler.py on the writer's measuring thread (no eBPF; needs root for /proc/<pid>/task/<tid>/stack)
#   none     nothing: the same run untraced, for comparison
# Environment: MIN_MS (default 20) for offcpu; BPFTRACE (default bpftrace).
# TRACE_DIR gets one output file per tool, threads.txt (find-threads.sh, once the writer runs), tracer-cpu.txt (each
# tracer process's user and system CPU seconds, which do not include the time its probes run inside the kernel) and
# tt-sustained's standard output and error.
set -euo pipefail

if [ "$#" -lt 4 ] || [ "$3" != "--" ]; then
  sed -n '2,15p' "$0" >&2
  exit 2
fi
TRACE_DIR=$1 TOOLS=$2
shift 3
HERE=$(cd "$(dirname "$0")" && pwd)
MIN_MS=${MIN_MS:-20}
BPFTRACE=${BPFTRACE:-bpftrace}
mkdir -p "$TRACE_DIR"
TRACE_DIR=$(cd "$TRACE_DIR" && pwd)

"$@" > "$TRACE_DIR/tt-sustained.stdout" 2> "$TRACE_DIR/tt-sustained.stderr" &
RUN=$!

writer=""
for _ in $(seq 1 600); do
  writer=$(pgrep -f '^[^ ]*tt-sustained writer ' | head -1 || true)
  [ -n "$writer" ] && break
  kill -0 "$RUN" 2>/dev/null || break
  sleep 0.05
done
if [ -z "$writer" ]; then
  echo "tt-sustained writer did not start; see $TRACE_DIR/tt-sustained.stderr" >&2
  wait "$RUN" || true
  exit 1
fi

pids=()
names=()
start_tracer() { # <name> <command...>: runs it under sudo in the background and remembers its process id
  local name=$1
  shift
  sudo sh -c 'echo $$ > "$1"; shift; exec "$@"' sh "$TRACE_DIR/$name.pid" "$@" > "$TRACE_DIR/$name.txt" 2>&1 &
  for _ in $(seq 1 100); do [ -s "$TRACE_DIR/$name.pid" ] && break; sleep 0.05; done
  pids+=("$(cat "$TRACE_DIR/$name.pid")")
  names+=("$name")
}
IFS=, read -r -a tools <<< "$TOOLS"
for tool in "${tools[@]}"; do
  case "$tool" in
    events) start_tracer events "$BPFTRACE" "$HERE/events.bt" "$writer" 86400 ;;
    offcpu) start_tracer offcpu "$BPFTRACE" "$HERE/offcpu.bt" "$writer" "$MIN_MS" 86400 ;;
    sampler) start_tracer sampler python3 "$HERE/stack-sampler.py" --pid "$writer" --seconds 86400 ;;
    none) ;;
    *) echo "unknown tool $tool" >&2; kill "$RUN"; exit 2 ;;
  esac
done
sleep 1
"$HERE/find-threads.sh" > "$TRACE_DIR/threads.txt" 2>&1 || true

status=0
wait "$RUN" || status=$?
: > "$TRACE_DIR/tracer-cpu.txt"
tick=$(getconf CLK_TCK)
for i in "${!pids[@]}"; do
  pid=${pids[$i]}
  if [ -r "/proc/$pid/stat" ]; then
    sed 's/.*) //' "/proc/$pid/stat" | awk -v n="${names[$i]}" -v t="$tick" \
      '{ printf "%s user_cpu_s %.2f system_cpu_s %.2f\n", n, $12 / t, $13 / t }' >> "$TRACE_DIR/tracer-cpu.txt"
  fi
  sudo kill -INT "$pid" 2>/dev/null || true
done
wait || true
sudo rm -f "$TRACE_DIR"/*.pid
exit "$status"
