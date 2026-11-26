#!/bin/bash
# Checks that the released v1.0.0 tools are unaffected by the temporary name an archiver gives a segment it removes
# (`{base:020}.tape.{pid}.{unix_ns}.removing`, ADR 0055), as a crash leaves it: builds the v1.0.0 tag and this
# checkout's `tickertape`, writes a journal with v1.0.0's writer, copies it, and in one copy renames the oldest segment to
# such a name (a crash after taking it out) where the other loses it (a removal that finished). Runs the same v1.0.0
# commands on both (verify, inspect, tail, stats, export, status, archive, a writer that continues, one that recovers a
# corrupt tail, one without a marker). Fails unless exit statuses, outputs and the segments left are the same and the
# file under the temporary name is unchanged. Then this checkout's `archive` leaves a new (and unsealed) name alone and
# unlinks the old one: an hour old, its process gone, and the segment its seal names long removed.
#
# Usage: tests/compat/v1.0.0-ignores-removing.sh [work_dir]   (from the repository root; needs git and cargo)
# Run weekly (.github/workflows/soak.yml). Builds into <work_dir>/target-v1.0.0 and <work_dir>/target; delete
# <work_dir> afterwards.
set -euo pipefail
ROOT=$(pwd)
WORK=${1:-$(mktemp -d)}
mkdir -p "$WORK/v1.0.0"
git -C "$ROOT" archive v1.0.0 | tar -x -C "$WORK/v1.0.0"
(cd "$WORK/v1.0.0" && CARGO_TARGET_DIR="$WORK/target-v1.0.0" cargo build -q -p tickertape-cli -p tickertape-tests --bins)
CARGO_TARGET_DIR="$WORK/target" cargo build -q -p tickertape-cli --bins
OLD=$WORK/target-v1.0.0/debug
NEW=$WORK/target/debug/tickertape
W=$WORK/removing
rm -rf "$W"
mkdir -p "$W/with/dest" "$W/without/dest"

"$OLD/tt-writer" "$W/without/journal" 9000 131072 64 > /dev/null
cp -Rp "$W/without/journal" "$W/with/journal"
J=$W/with/journal
OLDEST=$(cd "$J" && ls -- *.tape | sort | head -1)
# What a crash after taking the oldest segment out leaves: its name gone, the file under a temporary name of a process
# that has exited, two hours ago.
true &
DEAD=$!
wait $DEAD
NOW_NS=$(($(date +%s) * 1000000000))
LEFTOVER="$OLDEST.$DEAD.$((NOW_NS - 7200000000000)).removing"
mv "$J/$OLDEST" "$J/$LEFTOVER"
rm "$W/without/journal/$OLDEST"
SUM=$(shasum < "$J/$LEFTOVER")

# Runs a v1.0.0 command in both, with the journal as ./journal, and compares status and output.
both() {
    echo "== $*"
    local side status
    for side in with without; do
        status=0
        (cd "$W/$side" && "$OLD/$1" "${@:2}" > "$W/$side.out" 2> "$W/$side.err") || status=$?
        echo "status $status" >> "$W/$side.out"
        [ -z "${EXPECT:-}" ] || [ "$status" = "$EXPECT" ]
    done
    # Masked: what depends on the clock (timestamps) and on the run (pids).
    local mask='s/[0-9]{10,}/T/g; s/"created_utc":"[^"]*"/"created_utc":T/g; s/"pid":[0-9]+/"pid":P/g'
    diff <(sed -E "$mask" "$W/with.out") <(sed -E "$mask" "$W/without.out")
    diff "$W/with.err" "$W/without.err"
}
readers() {
    both tickertape verify journal --json
    both tickertape inspect journal --json
    both tickertape tail journal --no-follow --format json --from 4000
    both tickertape tail journal --no-follow --durable-only
    both tickertape stats journal --json --from 3000 --to 8999
    both tickertape export journal --format jsonl --output export.jsonl
    diff <(sed -E 's/[0-9]{10,}/T/g' "$W/with/export.jsonl") <(sed -E 's/[0-9]{10,}/T/g' "$W/without/export.jsonl")
}
EXPECT=0 readers
# Status depends on the moment it is taken: its exit status only.
for side in with without; do
    "$OLD/tickertape" status "$W/$side/journal" --json > /dev/null
done
EXPECT=0 both tickertape archive journal --to dest --keep-segments 5 --json
diff -r "$W/with/dest" "$W/without/dest"
EXPECT=0 both tt-writer journal 10000 131072 64
EXPECT=0 readers
# Damage after the checkpoint, for the next open's recovery.
for side in with without; do
    NEWEST=$(ls "$W/$side/journal"/*.tape | sort | tail -1)
    printf '\x01\x02\x03\x04\x05\x06\x07\x08' | dd of="$NEWEST" bs=1 seek=130000 conv=notrunc 2> /dev/null
done
EXPECT=0 both tt-writer journal 10100 131072 64
# No marker: recovery from the oldest segment.
rm "$W/with/journal/checkpoint" "$W/without/journal/checkpoint"
EXPECT=0 both tt-writer journal 10200 131072 64
EXPECT=0 readers
[ "$(shasum < "$J/$LEFTOVER")" = "$SUM" ]
diff <(cd "$J" && ls | grep -v '\.removing$') <(cd "$W/without/journal" && ls)

# This checkout's archive: a new name (of this shell, alive) stays, the old one of a process that has exited goes.
YOUNG="$OLDEST.$$.$NOW_NS.removing"
echo young > "$J/$YOUNG"
# The first run removes the segment the old name's seal names (it is the oldest of the run's own listing, so the name
# stays until the next run); the second unlinks the old name.
"$NEW" archive "$J" --delete --keep-segments 3 > /dev/null
[ -e "$J/$LEFTOVER" ]
"$NEW" archive "$J" --delete --keep-segments 3 > /dev/null
[ -e "$J/$YOUNG" ]
[ ! -e "$J/$LEFTOVER" ]
"$NEW" verify "$J" > /dev/null
echo "ok: v1.0.0 is unaffected by a leftover temporary name of a removal, and this checkout removes it once retention has removed the segment after it"
