#!/bin/sh
# Builds what the Java binding and its tests need, from the repository root's Cargo workspace:
#   - libtickertape (release) and the tickertape CLI, in $CARGO_TARGET_DIR/release (default: target/release);
#   - the C ABI test program and the C example reader, linked against that library, in .../release/java-tools.
# Prints the release directory, which is what -Dtickertape.native.dir expects.
set -eu

root=$(cd "$(dirname "$0")/../../.." && pwd)
target=${CARGO_TARGET_DIR:-$root/target}
release="$target/release"

cargo build --release --locked --manifest-path "$root/Cargo.toml" -p tickertape-ffi -p tickertape-cli >&2

tools="$release/java-tools"
mkdir -p "$tools"
cc=${CC:-cc}
for program in "crates/tickertape-ffi/tests/c/test_api.c:test_api" "examples/c/reader.c:reader"; do
    source=${program%%:*}
    name=${program##*:}
    "$cc" -std=c99 -Wall -Wextra -Werror -pedantic -O1 -I "$root/crates/tickertape-ffi/include" \
        "$root/$source" -L "$release" -ltickertape -Wl,-rpath,"$release" -o "$tools/$name"
done
echo "$release"
