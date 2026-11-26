#!/bin/bash
# Checks that processes of v1.0.0 and of this checkout share a journal's wake-up region without a lost wake-up (ADR 0053):
# builds `tt-wake` (tests/src/bin/tt-wake.rs, 1.0.0 API only) from the v1.0.0 tag and from this checkout, then runs
# every pairing of writer and reader versions, durable-only and visible readers, with one record per checkpoint and
# 23-63 ms pauses; then readers of both versions stay attached while the writer changes version, both ways.
# Fails if a reader misses or repeats a record, or if more than two deliveries of a run take 50 ms or more after the
# checkpoint (durable-only) or the append (visible): a lost wake-up costs the 100 ms cap on each wait, so it shows.
# Then, with 200 records per checkpoint 100 us apart, a new durable-only reader beside a new writer must not wake per
# record (Linux, its voluntary context switches), where a 1.0.0 one does (checked too, so that the check can fail). Last,
# durable-only readers of both versions killed while they wait must not stop others from getting every record.
#
# Usage: tests/compat/v1.0.0-wake-ups.sh [work_dir]   (from the repository root; needs git and cargo)
# Run weekly (.github/workflows/soak.yml). Builds into <work_dir>; delete it afterwards. RECORDS sets the records per
# run (default 40).
set -euo pipefail
ROOT=$(pwd)
WORK=${1:-$(mktemp -d)}
N=${RECORDS:-40}
mkdir -p "$WORK/v1.0.0"
git -C "$ROOT" archive v1.0.0 | tar -x -C "$WORK/v1.0.0"
cp "$ROOT/tests/src/bin/tt-wake.rs" "$WORK/v1.0.0/tests/src/bin/"
(cd "$WORK/v1.0.0" && CARGO_TARGET_DIR="$WORK/target-v1.0.0" cargo build -q --release -p tickertape-tests --bin tt-wake)
CARGO_TARGET_DIR="$WORK/target" cargo build -q --release -p tickertape-tests --bin tt-wake
BIN_old=$WORK/target-v1.0.0/release/tt-wake
BIN_new=$WORK/target/release/tt-wake
J=$WORK/journal
bin() { local v="BIN_$1"; echo "${!v}"; }

check() { # <label> <expected records> <reader output>
    echo "$1: $3"
    [[ "$3" =~ got=$2\  ]] || { echo "FAIL $1: expected $2 records"; exit 1; }
    local slow=${3#*slow=}
    slow=${slow%% *}
    [ "$slow" -le 2 ] || { echo "FAIL $1: $slow slow deliveries"; exit 1; }
}

for w in old new; do
    for r in old new; do
        for kind in durable visible; do
            rm -rf "$J" "$WORK"/w*.log
            "$(bin $w)" writer "$J" "$N" 1500 "$WORK/w.log" &
            sleep 0.8
            out=$("$(bin $r)" reader "$J" "$kind" "$N" "$WORK/w.log")
            wait
            check "writer $w, $kind reader $r" "$N" "$out"
        done
    done
done

for order in "old new" "new old"; do
    set -- $order
    rm -rf "$J" "$WORK"/w*.log
    "$(bin "$1")" writer "$J" "$N" 1500 "$WORK/w1.log" &
    W=$!
    sleep 0.8
    pids=()
    for r in old new; do
        for kind in durable visible; do
            "$(bin $r)" reader "$J" "$kind" $((2 * N)) "$WORK/w1.log" "$WORK/w2.log" > "$WORK/$r-$kind.out" &
            pids+=($!)
        done
    done
    wait "$W"
    "$(bin "$2")" writer "$J" "$N" 10 "$WORK/w2.log"
    wait "${pids[@]}"
    for r in old new; do
        for kind in durable visible; do
            check "writer $1 then $2, $kind reader $r" $((2 * N)) "$(cat "$WORK/$r-$kind.out")"
        done
    done
done
if [ -r /proc/self/status ]; then
    PER=200
    for v in new old; do
        rm -rf "$J" "$WORK"/w*.log
        "$(bin $v)" writer "$J" "$N" 1500 "$WORK/w.log" "$PER" &
        sleep 0.8
        out=$("$(bin $v)" reader "$J" durable $((N * PER)) "$WORK/w.log")
        wait
        check "writer $v, durable reader $v, $PER records per checkpoint" $((N * PER)) "$out"
        switches=${out#*switches=}
        if [ $v = new ]; then
            # About one wake-up per checkpoint, plus timeouts.
            [ "$switches" -lt $((N * 10)) ] || { echo "FAIL: $switches context switches"; exit 1; }
        else
            # 1.0.0's reader wakes for the appends: the limit above would catch it.
            [ "$switches" -ge $((N * 10)) ] || { echo "FAIL: the check cannot tell ($switches)"; exit 1; }
        fi
    done
fi

for w in old new; do
    rm -rf "$J" "$WORK"/w*.log
    "$(bin $w)" writer "$J" "$N" 1500 "$WORK/w.log" &
    W=$!
    sleep 0.8
    killed=()
    for r in old new; do
        "$(bin $r)" reader "$J" durable "$N" "$WORK/w.log" > /dev/null &
        killed+=($!)
    done
    pids=()
    for r in old new; do
        for kind in durable visible; do
            "$(bin $r)" reader "$J" "$kind" "$N" "$WORK/w.log" > "$WORK/$r-$kind.out" &
            pids+=($!)
        done
    done
    sleep 1
    kill -9 "${killed[@]}"
    wait "${killed[@]}" 2> /dev/null || true
    wait "$W" "${pids[@]}"
    for r in old new; do
        for kind in durable visible; do
            check "writer $w, $kind reader $r beside killed durable-only readers" "$N" "$(cat "$WORK/$r-$kind.out")"
        done
    done
done
rm -rf "$J"
echo "v1.0.0-wake-ups: all pairings passed"
