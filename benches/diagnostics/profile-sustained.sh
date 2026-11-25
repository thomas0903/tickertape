#!/usr/bin/env bash
# Runs tt-sustained and, in the middle of the run, samples or counts what its writer's and readers' measuring threads
# (the threads that append and read: each process's main thread) do. Linux; perf and bpftrace run under sudo.
#
#   benches/diagnostics/profile-sustained.sh OUT_DIR MODE SECONDS -- target/release/tt-sustained run --dir ... --out ...
#
# MODE:
#   perf    CPU profile: `perf record` with frame-pointer call graphs on the writer's and each reader's main thread,
#           SECONDS long, then folded stacks (fold-stacks.py). Uses the cycles counter if the machine has it and
#           cpu-clock sampling otherwise; event.txt says which. Build tt-sustained with
#           RUSTFLAGS="-C force-frame-pointers=yes" CARGO_PROFILE_RELEASE_DEBUG=1.
#   counts  system calls and bytes read (counts.bt), and heap allocations with their stacks (allocs.bt), of those
#           threads for SECONDS each, one after the other, and `perf trace -s` for SECONDS after them. Records per
#           second come from series.jsonl, so counts can be given per record.
#   none    nothing: the same run unobserved.
# Environment: SETTLE (seconds to wait after the writer starts, default 12), HZ (default 1999).
# Start the run with a --duration of at least SETTLE plus SECONDS (three times SECONDS for counts) plus a few seconds.
set -euo pipefail

if [ "$#" -lt 5 ] || [ "$4" != "--" ]; then
  sed -n '2,19p' "$0" >&2
  exit 2
fi
OUT=$1 MODE=$2 SECONDS_=$3
shift 4
HERE=$(cd "$(dirname "$0")" && pwd)
SETTLE=${SETTLE:-12}
HZ=${HZ:-1999}
PERF=${PERF:-perf}
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
printf '%q ' "$0" "$OUT" "$MODE" "$SECONDS_" -- "$@" > "$OUT/command.txt"

"$@" > "$OUT/tt-sustained.stdout" 2> "$OUT/tt-sustained.stderr" &
RUN=$!
sleep "$SETTLE"
writer=$(pgrep -f '^[^ ]*tt-sustained writer ' | head -1)
mapfile -t readers < <(pgrep -f '^[^ ]*tt-sustained reader ' || true)
"$HERE/find-threads.sh" > "$OUT/threads.txt" 2>&1 || true
names=(writer) tids=("$writer")
for i in "${!readers[@]}"; do names+=("reader-$i"); tids+=("${readers[$i]}"); done

mark() { date +%s.%N >> "$OUT/$1.window"; }

case "$MODE" in
  perf)
    event=cycles
    if ! sudo "$PERF" stat -e cycles true 2>&1 | grep -Eq '[0-9] +cycles'; then event=cpu-clock; fi
    echo "$event at $HZ Hz" > "$OUT/event.txt"
    mark perf
    pids=()
    for i in "${!tids[@]}"; do
      sudo "$PERF" record -q -e "$event" -F "$HZ" --call-graph fp -t "${tids[$i]}" -o "$OUT/${names[$i]}.data" \
        -- sleep "$SECONDS_" > "$OUT/${names[$i]}.record.txt" 2>&1 &
      pids+=($!)
    done
    for p in "${pids[@]}"; do wait "$p" || true; done
    mark perf
    ;;
  counts)
    for tool in counts allocs; do
      mark "$tool"
      pids=()
      for i in "${!tids[@]}"; do
        sudo bpftrace "$HERE/$tool.bt" "${tids[$i]}" "$SECONDS_" > "$OUT/${names[$i]}.$tool.txt" 2>&1 &
        pids+=($!)
      done
      for p in "${pids[@]}"; do wait "$p" || true; done
      mark "$tool"
    done
    mark trace
    pids=()
    for i in "${!tids[@]}"; do
      sudo "$PERF" trace -s -t "${tids[$i]}" -o "$OUT/${names[$i]}.trace.txt" -- sleep "$SECONDS_" \
        > "$OUT/${names[$i]}.trace.log" 2>&1 &
      pids+=($!)
    done
    for p in "${pids[@]}"; do wait "$p" || true; done
    mark trace
    ;;
  none) ;;
  *) echo "unknown mode $MODE" >&2; kill "$RUN"; exit 2 ;;
esac

status=0
wait "$RUN" || status=$?
if [ "$MODE" = perf ]; then
  # After the run, so that writing out the stacks does not compete with it.
  for name in "${names[@]}"; do
    sudo "$PERF" script -i "$OUT/$name.data" -F comm,ip,sym,dso,dsoff,event --no-inline --max-stack 64 \
      > "$OUT/$name.script" 2> "$OUT/$name.script.err" || true
    sudo chown "$(id -u)" "$OUT/$name.data"
    python3 "$HERE/fold-stacks.py" "$OUT/$name.script" > "$OUT/$name.folded"
    gzip -f "$OUT/$name.script"
  done
fi
exit "$status"
