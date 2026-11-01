# tickertape-cli

Tools for [tickertape](https://github.com/thomas0903/tickertape) event journals.

```sh
cargo run -p tickertape-cli -- --help
# Or install the binary from this checkout:
cargo install --path crates/tickertape-cli

tickertape tail ./journal --from 42 --format json --count 100
tickertape tail ./journal --from-end --timeout-secs 5
tickertape inspect ./journal --json
tickertape verify ./journal
tickertape stats ./journal --json
tickertape archive ./journal --to ./archive --keep-segments 10 --dry-run
tickertape export ./journal --format jsonl --from 10 --to 99 --output events.jsonl
tickertape export ./journal --format parquet --output events.parquet   # then: SELECT * FROM 'events.parquet' in DuckDB
```

`tail` starts at the oldest record and follows forever by default. Use `--no-follow` for replay only. Text output
previews payloads as hex; JSONL preserves payloads as standard base64 with padding. Export stops at the committed
end observed in its startup scan. Commands never take the writer lock. Only `archive` changes journal files, removing a sealed prefix strictly
before the checkpoint segment; its destination directory must exist and existing files are never replaced.

Exit codes: **0** success, **1** usage/I/O/missing journal, **2** corruption (including unsupported format).
Errors and warnings go to stderr. Each command's `--help` documents options, output and exit codes. The exact
machine-readable output schemas (JSONL, Parquet, `inspect`/`verify`/`stats`/`archive --json`) are in
[`docs/cli.md`](https://github.com/thomas0903/tickertape/blob/main/docs/cli.md).
