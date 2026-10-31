# tickertape

A Rust library for a local, append-only event journal on memory-mapped segment files: one writer process appends
records (bytes, a gap-free sequence number, a schema id and two caller-supplied timestamps); readers in other
processes on the same machine follow them live or replay from any sequence number. Linux and macOS.

- Simple, versioned on-disk format: [`docs/format.md`](https://github.com/thomas0903/tickertape/blob/main/docs/format.md)
- Explicit crash contract, with the test for each guarantee:
  [`docs/crash-contract.md`](https://github.com/thomas0903/tickertape/blob/main/docs/crash-contract.md)
- *Visible* (after `append`) is not *durable* (after `checkpoint()`); power-loss safety is not claimed.
- Safe `pread`-based readers by default; zero-copy readers are an `unsafe` opt-in.

The `tickertape` CLI (crate `tickertape-cli`) tails, verifies and exports journals to JSONL or Parquet.
See the [repository](https://github.com/thomas0903/tickertape) for the quickstart, examples and benchmarks.

Licensed under MIT OR Apache-2.0.
