#!/usr/bin/env bash
# The recorded tt-scale session (benches/README.md -> "Replay and recovery at scale"): every stage, or the named ones.
#
#   benches/scale-session.sh <journal-root> <out-dir> [counts|many|sizes|seeks|damage|big|retention ...]
#
# <journal-root> is an empty directory on the filesystem under test (about 250 GB free for `big`); <out-dir> receives
# one JSON file per command, progress.txt and environment.txt. Environment:
#   TT_SCALE       the tool under test (default: target/release/tt-scale)
#   TT_SCALE_BASE  the same tool built against another core, measured on the same journals (optional)
#   TT_CLI         the tickertape program (default: next to TT_SCALE)
# Needs password-less sudo for the cold runs. Nothing is recorded without a clean commit (docs/playbooks/run-benchmark.md).
set -uo pipefail
ROOT="${1:?journal root}"; OUT="${2:?output directory}"; shift 2
STAGES=("$@"); [ ${#STAGES[@]} -eq 0 ] && STAGES=(counts many sizes seeks damage big)
T="${TT_SCALE:-${CARGO_TARGET_DIR:-target}/release/tt-scale}"
BASE="${TT_SCALE_BASE:-}"
CLI="${TT_CLI:-$(dirname "$T")/tickertape}"
MIB=1048576
mkdir -p "$OUT" "$ROOT"

{
  date -u +%FT%TZ; uname -srmv; cat /sys/class/dmi/id/product_name; lscpu; free -b
  df -T "$ROOT"; findmnt -T "$ROOT" -o TARGET,SOURCE,FSTYPE,OPTIONS; lsblk -o NAME,SIZE,MODEL,ROTA,SCHED
  ulimit -n; ulimit -Hn; sysctl fs.nr_open fs.file-max vm.max_map_count vm.dirty_ratio vm.dirty_background_ratio
  rustc -V; git log --oneline -1; git status --short; echo "tool $T base $BASE cli $CLI"
} > "$OUT/environment.txt" 2>&1

log() { echo "$(date -u +%FT%TZ) $*" >> "$OUT/progress.txt"; }
# step <name> <command...>: standard output to <name>.json, errors to <name>.err.
step() {
  local name="$1"; shift
  log "start $name: $*"
  "$@" > "$OUT/$name.json" 2> "$OUT/$name.err"
  log "end $name exit=$?"
}
# Both readers, warm and cold, and the same with the other core. The warm runs come first: every cold run ends with an
# empty cache.
both() {
  local name="$1" dir="$2"; shift 2
  [[ " $* " == *" --ops "* ]] || set -- "$@" --ops unindexed
  step "$name-warm" "$T" measure --dir "$dir" --readers safe,mapped --label main --allow-errors --json "$@"
  [ -n "$BASE" ] && step "$name-warm-base" "$BASE" measure --dir "$dir" --readers safe,mapped --label base --allow-errors --json "$@"
  step "$name-cold" "$T" measure --dir "$dir" --readers safe,mapped --label main --allow-errors --cold --json "$@"
  [ -n "$BASE" ] && step "$name-cold-base" "$BASE" measure --dir "$dir" --readers safe,mapped --label base --allow-errors --cold --json "$@"
}

# Before and after on the same warm journal, which these operations do not change: the tool, the other core, the tool.
compare() {
  local shape name dir
  for shape in "1000 $MIB" "10000 $MIB" "10 16777216" "10 134217728" "10 1073741824"; do
    set -- $shape
    name="compare-$1x$2"; dir="$ROOT/$name"
    step "$name-build" "$T" build --dir "$dir" --segments "$1" --segment-size "$2" --json
    step "$name-warmup" "$T" measure --dir "$dir" --ops verify --json
    step "$name-main-1" "$T" measure --dir "$dir" --readers safe,mapped --repeats 5 --allow-errors --json --ops seeks,replay,status,verify,list-segments,retention-plan
    step "$name-base" "$BASE" measure --dir "$dir" --readers safe,mapped --repeats 5 --allow-errors --json --ops seeks,replay,status,verify,list-segments,retention-plan
    step "$name-main-2" "$T" measure --dir "$dir" --readers safe,mapped --repeats 5 --allow-errors --json --ops seeks,replay,status,verify,list-segments,retention-plan
    rm -rf "${dir:?}"
  done
}

# Segment count at a fixed segment size (1 MiB): 10 to 20,000 segments.
counts() {
  local n dir
  for n in 10 1000 10000 20000; do
    dir="$ROOT/seg-$n"
    step "seg-$n-build" "$T" build --dir "$dir" --segments "$n" --segment-size $MIB --json
    both "seg-$n" "$dir" --repeats 2
    step "seg-$n-files" "$T" measure --dir "$dir" --readers safe,mapped --sample-open-files --allow-errors --json --ops unindexed
    step "seg-$n-files64" "$T" measure --dir "$dir" --readers safe,mapped --max-open-files 64 --allow-errors --json \
      --ops seeks,status,verify,retention-plan,writer-open-clean,writer-open-without-marker,checkpoint-loop-retention
    if [ "$n" = 10000 ]; then
      step "seg-$n-archive" "$T" archive --dir "$dir" --to "$ROOT/seg-$n-archive" --keep-segments 5000 --cold --json
      archived "seg-$n-archive" "$ROOT/seg-$n-archive"
      rm -rf "${ROOT:?}/seg-$n-archive"
    fi
    step "seg-$n-retention" "$T" measure --dir "$dir" --ops retention-delete --count $((n / 10)) --repeats 2 --cold --json
    rm -rf "${dir:?}"
  done
  # A writer that rolls 20,000 times without a checkpoint, then takes one.
  step "seg-20000-unchecked-build" "$T" build --dir "$ROOT/unchecked" --segments 20000 --segment-size $MIB --checkpoint-every 0 --json
  rm -rf "${ROOT:?}/unchecked"
}

# Reads, seeks and the command line on an archive directory, up to the last record it holds.
archived() {
  local name="$1" dir="$2" end
  end=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["boundary_seq"])' "$OUT/$name.json") || return
  for cache in "" --cold; do
    step "$name-read${cache#-}" "$T" measure --dir "$dir" --end "$end" --readers safe,mapped --repeats 2 --allow-errors --json $cache \
      --ops list-directory,list-segments,seeks,replay,verify
    step "$name-cli${cache#-}" "$T" measure --dir "$dir" --end "$end" --cli "$CLI" --allow-errors --json $cache \
      --ops cli-verify,cli-stats,cli-export
  done
}

# 100,000 segments of 64 KiB: where directory reads and file opens dominate.
many() {
  local dir="$ROOT/seg-100000"
  step "seg-100000-build" "$T" build --dir "$dir" --segments 100000 --segment-size 65536 --json
  step "seg-100000-warm" "$T" measure --dir "$dir" --readers safe,mapped --repeats 2 --allow-errors --json --ops unindexed
  step "seg-100000-cold" "$T" measure --dir "$dir" --readers safe,mapped --repeats 2 --allow-errors --cold --json --ops unindexed
  rm -rf "${dir:?}"
}

# Segment size at a fixed count: 1 MiB to 1 GiB segments.
sizes() {
  local size dir
  for size in $MIB 16777216 134217728 1073741824; do
    dir="$ROOT/size-$size"
    step "size-$size-build" "$T" build --dir "$dir" --segments 10 --segment-size "$size" --json
    both "size-$size" "$dir" --repeats 2
    rm -rf "${dir:?}"
  done
}

# Seek indexes (ADR 0045): the seeks without and with them, warm and cold, on the same journals; what building, refreshing
# and checking costs; and what index files add to writer open, checkpoints with retention and deleting segments.
seeks() {
  local shape name dir cache
  for shape in "10000 $MIB" "20000 $MIB" "160 134217728" "10 1073741824"; do
    set -- $shape
    name="index-$1x$2"; dir="$ROOT/$name"
    step "$name-build" "$T" build --dir "$dir" --segments "$1" --segment-size "$2" --json
    for cache in "" --cold; do
      step "$name-off${cache#-}" "$T" measure --dir "$dir" --readers safe,mapped --ops seeks --repeats 5 --allow-errors --json $cache
    done
    step "$name-writer-off" "$T" measure --dir "$dir" --ops writer-open-clean,checkpoint-loop-retention --repeats 5 --allow-errors --json
    step "$name-make-cold" "$T" measure --dir "$dir" --ops index-build --cold --repeats 2 --allow-errors --json
    step "$name-make-warm" "$T" measure --dir "$dir" --ops index-build,index-refresh,index-check --repeats 2 --allow-errors --json
    step "$name-refresh-cold" "$T" measure --dir "$dir" --ops index-refresh --cold --repeats 2 --allow-errors --json
    for cache in "" --cold; do
      step "$name-on${cache#-}" "$T" measure --dir "$dir" --readers safe,mapped --ops seeks --seek-index --repeats 5 --allow-errors --json $cache
    done
    step "$name-writer-on" "$T" measure --dir "$dir" --ops writer-open-clean,checkpoint-loop-retention --repeats 5 --allow-errors --json
    [ "$1" -ge 1000 ] && step "$name-retention" "$T" measure --dir "$dir" --ops retention-delete --count $(($1 / 10)) --repeats 2 --cold --allow-errors --json
    rm -rf "${dir:?}"
  done
}

# Checkpoints with retention, a writer's open with it, and removing segments (ADR 0051): both cores in turn on one journal
# per shape, five rounds, the first of the two alternating. Not a default stage; needs TT_SCALE_BASE.
retention() {
  local shape name dir round side tool sides
  for shape in "10000 $MIB" "20000 $MIB" "100000 65536"; do
    set -- $shape
    name="retention-$1x$2"; dir="$ROOT/$name"
    step "$name-build" "$T" build --dir "$dir" --segments "$1" --segment-size "$2" --json
    step "$name-warmup" "$BASE" measure --dir "$dir" --ops list-segments --json
    for round in 1 2 3 4 5; do
      sides="base new"; [ $((round % 2)) = 0 ] && sides="new base"
      for side in $sides; do
        tool="$T"; [ "$side" = base ] && tool="$BASE"
        step "$name-idle-$side-$round" "$tool" measure --dir "$dir" --label "$side" --count 20 --allow-errors --json \
          --ops retention-plan,checkpoint-loop,checkpoint-loop-retention,checkpoint-loop-age,checkpoint-loop-bytes
        step "$name-remove-$side-$round" "$tool" measure --dir "$dir" --label "$side" --count 20 --allow-errors --json \
          --ops checkpoint-loop-remove-count,checkpoint-loop-remove-age
        step "$name-delete-$side-$round" "$tool" measure --dir "$dir" --label "$side" --count 100 --allow-errors --json \
          --ops retention-delete
      done
    done
    for round in 1 2 3 4 5; do
      sides="base new"; [ $((round % 2)) = 0 ] && sides="new base"
      for side in $sides; do
        tool="$T"; [ "$side" = base ] && tool="$BASE"
        step "$name-cold-$side-$round" "$tool" measure --dir "$dir" --label "$side" --count 1 --cold --allow-errors --json \
          --ops checkpoint-loop,checkpoint-loop-retention,checkpoint-loop-age,checkpoint-loop-bytes
      done
    done
    rm -rf "${dir:?}"
  done
}

# Missing and corrupt segments, with many small segments and with the default segment size.
damage() {
  step "damage-10000-warm" "$T" damage --dir "$ROOT/damage" --segments 10000 --segment-size $MIB --json
  rm -rf "${ROOT:?}/damage"
  step "damage-10000-cold" "$T" damage --dir "$ROOT/damage" --segments 10000 --segment-size $MIB --cold --json
  rm -rf "${ROOT:?}/damage"
  step "damage-128m-cold" "$T" damage --dir "$ROOT/damage" --segments 64 --segment-size 134217728 --cold --json
  rm -rf "${ROOT:?}/damage"
}

# Larger than RAM: 64 GiB at the default segment size (512 x 128 MiB): operations, catch-up, archive.
big() {
  local dir="$ROOT/big" reader wait run cpus w r size
  step big-build "$T" build --dir "$dir" --segments 512 --segment-size 134217728 --json
  step big-cold "$T" measure --dir "$dir" --readers safe,mapped --repeats 2 --allow-errors --cold --json --ops unindexed
  step big-warm "$T" measure --dir "$dir" --readers safe,mapped --repeats 2 --allow-errors --json --ops unindexed
  if [ -n "$BASE" ]; then
    step big-cold-base "$BASE" measure --dir "$dir" --readers safe,mapped --allow-errors --cold --json --ops seeks,replay
  fi
  # One logical CPU per physical core: the writer on the last core, the reader on the one before it.
  cpus=($(lscpu -p=CPU,CORE | grep -v '^#' | awk -F, '!seen[$2]++ {print $1}'))
  w=${cpus[-1]}; r=${cpus[-2]}
  for run in 1 2; do
    for reader in safe mapped; do
      for wait in spin block; do
        # 32 GiB behind with a cold cache; 2 GiB behind, in the page cache (the run before has just written it).
        step "catchup-32g-$reader-$wait-$run" "$T" catch-up --dir "$dir" --reader $reader --wait $wait --rate 100000 \
          --backlog 32000000 --warmup 5s --baseline 20s --live 20s --writer-cpu "$w" --reader-cpu "$r" --cold --json
        step "catchup-2g-$reader-$wait-$run" "$T" catch-up --dir "$dir" --reader $reader --wait $wait --rate 100000 \
          --backlog 2000000 --warmup 5s --baseline 20s --live 20s --writer-cpu "$w" --reader-cpu "$r" --json
      done
    done
  done
  # The oldest 256 segments (32 GiB) to an archive.
  size=$(find "$dir" -name '*.tape' | wc -l)
  step big-archive "$T" archive --dir "$dir" --to "$ROOT/big-archive" --keep-segments $((size - 256)) --cold --json
  archived big-archive "$ROOT/big-archive"
  rm -rf "${dir:?}" "${ROOT:?}/big-archive"
}

[ -n "$BASE" ] && [ $# -eq 0 ] && STAGES+=(compare)
for stage in "${STAGES[@]}"; do
  log "stage $stage"
  "$stage"
done
log "done"
