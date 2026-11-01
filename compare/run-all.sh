#!/usr/bin/env bash
# Build once, then let the parent bound and reap all benchmark child processes.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
exec python3 "$HERE/run.py" "$@"
