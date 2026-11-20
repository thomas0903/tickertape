#!/bin/sh
# The install test of ADR 0031 for the C library archive (`release.py c-package`): unpacks the archive, then builds
# the C examples (examples/c/writer.c, reader.c) and the C++ example (example.cpp) with nothing but the archive,
# through pkg-config, once against the shared library (`tickertape`) and once against the static one
# (`tickertape-static`). It checks the shared library's soname or install name, that the shared programs load the
# archive's library and the static ones no tickertape library at all, then records, follows live and replays a
# journal with each, and fails on any output to standard error.
#
#   install-test.sh <archive.tar.gz> <work dir>
set -eu

if [ $# -ne 2 ]; then
    echo "usage: $0 <archive.tar.gz> <work dir>" >&2
    exit 2
fi
archive=$1
work=$2
root=$(cd "$(dirname "$0")/../../.." && pwd)
examples="$root/examples/c"
rm -rf "$work"
mkdir -p "$work"
tar -xzf "$archive" -C "$work"
prefix=$(cd "$work"/tickertape-c-* && pwd)
version=$(basename "$prefix" | sed -E 's/^tickertape-c-v([0-9]+\.[0-9]+\.[0-9]+)-.*$/\1/')

# Only the archive's .pc files: no tickertape installed elsewhere can stand in for it.
PKG_CONFIG_PATH="$prefix/lib/pkgconfig"
PKG_CONFIG_LIBDIR="$prefix/lib/pkgconfig"
export PKG_CONFIG_PATH PKG_CONFIG_LIBDIR
for module in tickertape tickertape-static; do
    test "$(pkg-config --modversion "$module")" = "$version"
done
libdir=$(cd "$(pkg-config --variable=libdir tickertape)" && pwd)
test "$libdir" = "$prefix/lib"

major=${version%%.*}
minor=${version#*.}
minor=${minor%%.*}
if [ "$major" = 0 ]; then compatible="0.$minor"; else compatible=$major; fi
case $(uname) in
    Darwin)
        install_name=$(otool -D "$libdir/libtickertape.dylib" | sed -n 2p)
        test "$install_name" = "@rpath/libtickertape.$compatible.dylib"
        ;;
    *)
        readelf -dW "$libdir/libtickertape.so" | grep -q "(SONAME).*\[libtickertape\.so\.$compatible\]"
        ;;
esac

cc=${CC:-cc}
cxx=${CXX:-c++}
cflags="-std=c99 -O2 -Wall -Wextra -Werror -pedantic"
cxxflags="-std=c++17 -O2 -Wall -Wextra -Werror -pedantic"
# pkg-config output is a list of words, so it is deliberately unquoted below.
shared=$(pkg-config --cflags --libs tickertape)
static=$(pkg-config --cflags --libs tickertape-static)
rpath="-Wl,-rpath,$libdir"
for kind in shared static; do
    if [ "$kind" = shared ]; then flags="$shared $rpath"; else flags=$static; fi
    # shellcheck disable=SC2086
    "$cc" $cflags "$examples/writer.c" $flags -o "$work/writer-$kind"
    # shellcheck disable=SC2086
    "$cc" $cflags "$examples/reader.c" $flags -o "$work/reader-$kind"
    # shellcheck disable=SC2086
    "$cxx" $cxxflags "$examples/example.cpp" $flags -o "$work/example-$kind"
done

# What each program loads: the archive's library for the shared builds, no tickertape library for the static ones.
for program in writer reader example; do
    case $(uname) in
        Darwin)
            otool -L "$work/$program-shared" | grep -q "@rpath/libtickertape.$compatible.dylib"
            if otool -L "$work/$program-static" | grep -q libtickertape; then
                echo "$program-static links a tickertape shared library" >&2
                exit 1
            fi
            ;;
        *)
            ldd "$work/$program-shared" | grep -q "libtickertape.so.$compatible => $libdir/libtickertape.so.$compatible"
            if readelf -dW "$work/$program-static" | grep -q 'NEEDED.*libtickertape'; then
                echo "$program-static links a tickertape shared library" >&2
                exit 1
            fi
            ;;
    esac
done

lines() {
    wc -l < "$1" | tr -d ' '
}

status=0
expect_lines() {
    got=$(lines "$work/$1")
    if [ "$got" != "$2" ]; then
        echo "$1: $got lines, expected $2" >&2
        status=1
    fi
}

# C: the static reader follows live while the shared writer records, then both readers replay.
"$work/reader-static" "$work/c" follow 20 > "$work/c-follow.out" 2> "$work/c-follow.err" &
follower=$!
"$work/writer-shared" "$work/c" 20 20 > "$work/c-record.out" 2> "$work/c-record.err"
wait "$follower"
"$work/reader-shared" "$work/c" replay > "$work/c-replay.out" 2> "$work/c-replay.err"
"$work/reader-static" "$work/c" replay 10 > "$work/c-replay10.out" 2> "$work/c-replay10.err"
# A journal written by the static writer reads back with the shared reader.
"$work/writer-static" "$work/c-static" 5 > "$work/c-static-record.out" 2> "$work/c-static-record.err"
"$work/reader-shared" "$work/c-static" replay > "$work/c-static-replay.out" 2> "$work/c-static-replay.err"
expect_lines c-follow.out 20
expect_lines c-record.out 20
expect_lines c-replay.out 20
expect_lines c-replay10.out 10
expect_lines c-static-replay.out 5
if ! cmp -s "$work/c-follow.out" "$work/c-replay.out"; then
    echo "the C replay differs from what the C follower saw" >&2
    status=1
fi

# C++: the same with the shared and static builds of the binding's example.
"$work/example-shared" "$work/cpp" follow 20 > "$work/cpp-follow.out" 2> "$work/cpp-follow.err" &
follower=$!
"$work/example-static" "$work/cpp" record 20 20 > "$work/cpp-record.out" 2> "$work/cpp-record.err"
wait "$follower"
"$work/example-static" "$work/cpp" replay > "$work/cpp-replay.out" 2> "$work/cpp-replay.err"
"$work/example-shared" "$work/cpp" replay 10 > "$work/cpp-replay10.out" 2> "$work/cpp-replay10.err"
expect_lines cpp-follow.out 20
expect_lines cpp-replay.out 20
expect_lines cpp-replay10.out 10
if ! cmp -s "$work/cpp-follow.out" "$work/cpp-replay.out"; then
    echo "the C++ replay differs from what the C++ follower saw" >&2
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
    echo "install test passed with $(basename "$archive"): C and C++ examples, shared and static, through pkg-config"
fi
exit "$status"
