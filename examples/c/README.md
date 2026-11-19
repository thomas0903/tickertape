# C and C++ examples

Programs that use the tickertape C ABI (`crates/tickertape-ffi`, documented in [`docs/c-api.md`](../../docs/c-api.md)):

- `writer.c`: records events, checkpointing every 100 records and at the end.
- `reader.c`: replays a journal (`replay`), or follows it live from another process (`follow`).
- `example.cpp`: the same with the C++ binding (`crates/tickertape-ffi/include/tickertape.hpp`, C++17,
  [`docs/cpp-api.md`](../../docs/cpp-api.md)): `record`, `follow` live from another process, and `replay` with batch
  reads, handling errors as values (it also builds with `-fno-exceptions`).

Build the library, then the examples, from the repository root:

```bash
cargo build --release -p tickertape-ffi
make -C examples/c            # or: cmake -S examples/c -B build/examples && cmake --build build/examples
```

Record in one terminal while another follows live, then replay:

```bash
examples/c/reader /tmp/tt-c follow 20 &      # waits for the writer to create the journal
examples/c/writer /tmp/tt-c 20 50            # 20 records, one every 50 ms
wait
examples/c/reader /tmp/tt-c replay 10        # records 10..19
TICKERTAPE_DURABLE_ONLY=1 examples/c/reader /tmp/tt-c replay

examples/c/example /tmp/tt-cpp follow 20 &   # the same with the C++ binding
examples/c/example /tmp/tt-cpp record 20 50
wait
examples/c/example /tmp/tt-cpp replay 10
```

`tests/c_api.rs` in `crates/tickertape-ffi` builds and runs these examples in CI.
