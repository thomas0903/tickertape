# tickertape

**A Rust library and CLI for a local, append-only event journal on memory-mapped files.**

> Status: early development (pre-0.1). Nothing here is ready for use yet. See [`docs/status.md`](docs/status.md) and
> [`docs/roadmap.md`](docs/roadmap.md).

One writer process appends events to fixed-size segment files. Several reader processes follow the journal live and
can replay it from any sequence number. A CLI tails, inspects, verifies and exports journals, including to Parquet for
DuckDB and Python. The goal is a small, permissively licensed journal with a simple, versioned byte format and an
explicit, tested crash contract.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you,
as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
