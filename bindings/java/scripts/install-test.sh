#!/bin/sh
# The install test of ADR 0031 for the Java binding: runs the example (follow live, record, replay) with nothing but
# the two built JARs, the binding and the natives JAR for this platform, first on the class path and then on the
# module path, and fails on any output to standard error (such as a restricted-method warning).
#
#   install-test.sh <work dir>                          after: mvn -f bindings/java/pom.xml -Pnatives package
#   install-test.sh <work dir> <jar> <natives jar>      any built or downloaded pair, such as the release's
set -eu

if [ $# -ne 1 ] && [ $# -ne 3 ]; then
    echo "usage: $0 <work dir> [<jar> <natives jar>]" >&2
    exit 2
fi
work=$1
here=$(cd "$(dirname "$0")/.." && pwd)
if [ $# -eq 3 ]; then
    main=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
    natives=$(cd "$(dirname "$3")" && pwd)/$(basename "$3")
else
    natives=$(ls "$here"/target/tickertape-*-natives-*.jar)
    if [ "$(echo "$natives" | wc -l | tr -d ' ')" != 1 ]; then
        echo "more than one natives JAR in $here/target; name the pair to test" >&2
        exit 2
    fi
    main=${natives%-natives-*}.jar
fi
example="$here/examples/Example.java"
dir="$work/journal"
rm -rf "$work"
mkdir -p "$work"

on_class_path() {
    java --enable-native-access=ALL-UNNAMED -cp "$main:$natives" "$example" "$@"
}

on_module_path() {
    java -p "$main:$natives" --add-modules io.github.thomas0903.tickertape,io.github.thomas0903.tickertape.natives \
        --enable-native-access=io.github.thomas0903.tickertape "$example" "$@"
}

lines() {
    wc -l < "$1" | tr -d ' '
}

on_class_path follow "$dir" 20 > "$work/follow.out" 2> "$work/follow.err" &
follower=$!
sleep 1
on_class_path record "$dir" 20 10 > "$work/record.out" 2> "$work/record.err"
wait "$follower"
on_class_path replay "$dir" 10 > "$work/replay.out" 2> "$work/replay.err"
on_module_path replay "$dir" > "$work/module.out" 2> "$work/module.err"

status=0
for check in "follow.out 20" "record.out 20" "replay.out 10" "module.out 20"; do
    file=${check% *}
    want=${check#* }
    got=$(lines "$work/$file")
    if [ "$got" != "$want" ]; then
        echo "$file: $got lines, expected $want" >&2
        status=1
    fi
done
if ! cmp -s "$work/follow.out" "$work/module.out"; then
    echo "the module-path replay differs from what the follower saw" >&2
    status=1
fi
for err in "$work"/*.err; do
    if [ -s "$err" ]; then
        echo "unexpected output on standard error ($err):" >&2
        cat "$err" >&2
        status=1
    fi
done
if [ "$status" -eq 0 ]; then
    echo "install test passed with $(basename "$main") and $(basename "$natives")"
fi
exit "$status"
