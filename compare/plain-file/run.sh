#!/usr/bin/env bash
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
exec "$HERE/../target/release/compare-plain-file" "$@"
