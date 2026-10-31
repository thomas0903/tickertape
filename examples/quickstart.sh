#!/usr/bin/env bash
# The README quickstart as a script, so CI can check that every command in it works.
# Usage: examples/quickstart.sh [python-with-duckdb]
set -euo pipefail
PYTHON="${1:-python3}"
J="$(mktemp -d)/journal"
OUT="$(mktemp -d)/demo.parquet"

cargo build --release -p tickertape-examples -p tickertape-cli
./target/release/demo-producer --dir "$J" --count 200000 --rate 50000 &
./target/release/demo-consumer --dir "$J" --count 200000
wait
./target/release/tickertape verify "$J"
./target/release/tickertape export "$J" --format parquet --decode market-demo --output "$OUT"
"$PYTHON" -c "import duckdb, sys; print(duckdb.sql(f\"SELECT kind, count(*) AS events, min(seq), max(seq), round(avg(price), 2) AS avg_price FROM read_parquet('{sys.argv[1]}') GROUP BY kind ORDER BY kind\"))" "$OUT"
