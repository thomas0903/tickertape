#!/bin/sh
# Stages libtickertape for the natives JAR of one platform: copies the library to
#   bindings/java/target/natives/<platform>/META-INF/tickertape/native/<platform>/
# with a .sha256 file next to it, which the binding checks after extracting the library at run time. Staging one
# platform leaves the others staged, so the release workflow stages all four before one Maven build.
#
#   stage-natives.sh <native dir> <platform>     e.g. stage-natives.sh target/release linux-x86_64
#
# <native dir> holds libtickertape.so or libtickertape.dylib (a symbolic link is followed). Then:
# mvn -f bindings/java/pom.xml -Pnatives package, which builds one natives JAR per staged platform. Nothing is
# published.
set -eu

if [ $# -ne 2 ]; then
    echo "usage: $0 <native dir> <platform: linux-x86_64|linux-aarch64|macos-aarch64|macos-x86_64>" >&2
    exit 2
fi
native_dir=$1
platform=$2
case $platform in
    linux-*) lib=libtickertape.so ;;
    macos-*) lib=libtickertape.dylib ;;
    *) echo "unknown platform: $platform" >&2; exit 2 ;;
esac

here=$(cd "$(dirname "$0")/.." && pwd)
staging="$here/target/natives/$platform/META-INF/tickertape/native/$platform"
rm -rf "$here/target/natives/$platform"
mkdir -p "$staging"
cp "$native_dir/$lib" "$staging/$lib"
if command -v sha256sum >/dev/null 2>&1; then
    hash=$(sha256sum "$staging/$lib" | cut -d ' ' -f 1)
else
    hash=$(shasum -a 256 "$staging/$lib" | cut -d ' ' -f 1)
fi
printf '%s  %s\n' "$hash" "$lib" > "$staging/$lib.sha256"
echo "$staging/$lib ($hash)"
