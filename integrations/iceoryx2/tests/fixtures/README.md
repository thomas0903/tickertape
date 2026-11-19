# Test fixtures

## `iox2-0.10.0-record-ticks.txt`

A human-readable recording file written by the upstream recorder, not by hand, for the import test
(`tests/journal.rs`, `import_reads_a_file_written_by_upstream`).

- Written on 2026-11-19 on macOS (arm64) by `iox2 service record` from `iceoryx2-cli` 0.10.0 (installed with
  `cargo install --locked iceoryx2-cli@0.10.0`), default options (human-readable representation, RON format).
- The service was published by this crate's `publisher` example (`examples/publisher.rs`, `tick` kind: a `Tick`
  payload of 24 bytes and a `TickHeader` user header of 16 bytes, counters 0 to 4).
- Commands, from `integrations/iceoryx2` after `cargo test --all-features --no-run`:

  ```bash
  ./target/debug/examples/publisher --service tt-fixture/ticks --count 5 --rate 50 --wait-subscribers 1 &
  iox2 service record -o tests/fixtures/iox2-0.10.0-record-ticks.txt --max-messages 5 -t 20 tt-fixture/ticks
  ```

The file is kept exactly as written. The system headers hold iceoryx2's sample origins (unique ids of that
publisher), which the import does not keep.
