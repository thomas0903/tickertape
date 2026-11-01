#!/usr/bin/env bash
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
source "$HERE/../java-flags.sh"
exec "$JAVA_HOME/bin/java" "${JAVA_FLAGS[@]}" -cp "$HERE/target/classes:$(cat "$HERE/target/classpath.txt")" compare.Main "$@"
