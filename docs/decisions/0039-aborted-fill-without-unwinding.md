# 0039: An aborted fill does not unwind

- **Status:** accepted (amends how [0028](0028-c-abi.md) carries out an aborted fill; the C ABI is unchanged)
- **Date:** 2026-11-20

## Context

`tickertape_writer_append_with` calls the caller's `fill` callback, and a non-zero return means "abandon this
record" (`TICKERTAPE_E_ABORTED`, ADR 0028). The core's only zero-copy append, `Writer::append_with`, takes a closure
that returns nothing, so the C ABI aborted the append by raising a Rust unwind (`resume_unwind`) inside that closure.
The core caught it, zeroed and synced the abandoned bytes, and resumed the unwind; the shim caught it again and
returned `TICKERTAPE_E_ABORTED`.

So an ordinary, documented result of the C API depended on unwinding working inside `libtickertape`. A review of the
C++ binding (ADR 0036) found that it does not always: the GCC driver on macOS (Homebrew GCC) passes
`-no_compact_unwind` to the linker, and a program that links `libtickertape.a` that way ended inside the library at
the first aborted fill. A library built with `panic = "abort"` would end the same way. Before 1.0, when the C ABI and
the Rust API become stable, an ordinary result must not need unwinding.

## Options considered

1. **Keep the unwind and document the linker caveat** (the state before this record). Costs nothing in the core, but
   leaves a documented status code that ends the process under some link setups, and keeps `panic = "unwind"` a
   requirement for ordinary operation rather than only for reporting bugs.
2. **A core append whose closure can decline, returning a nested `Result`:**
   `Writer::try_append_with(&mut self, record, payload_len, fill: F) -> Result<Result<u64, E>>` with
   `F: FnOnce(&mut [u8]) -> Result<(), E>`. The outer `Result` is the journal's (`tickertape::Error`, as for every
   other append), the inner one the closure's: `?` passes journal errors on and leaves the caller with its own
   outcome, and no new public type has to be stabilised.
3. **The same function with its own error enum** (`TryAppendError<E> { Declined(E), Journal(Error) }`). Reads well in
   a `match`, but adds a generic public type, with its `Display`, `Error` and conversion impls, to the stable surface,
   and `?` no longer works directly in functions that return `tickertape::Result`.
4. **Reserve/commit** (two calls, the record committed by the second). Would also remove the unwind, but needs a
   borrow of the segment across calls and a new state in the writer; ADR 0028 already left it for later.

## Decision

Option 2. Declining does what a panic in the closure does, with a return instead of the panic continuing: nothing is
committed, the sequence number is not used, the writer zeroes the record it had begun and makes the zeroing durable
before returning, and if that fails it stops (`Writer::abandoned_write_error`, refused appends and checkpoints) and
keeps the cause.

A decline whose clean-up fails is a journal error: the call returns the outer `Err`, the same `Error::Io` that every
later append returns, and the caller's `e` is dropped. Reasons:

- The C ABI already gives the storage failure precedence: such a call returns `TICKERTAPE_E_IO`, not
  `TICKERTAPE_E_ABORTED`.
- Returning `Ok(Err(e))` would make `writer.try_append_with(..)?` treat a storage incident as an ordinary decline,
  and a contract that asks for a manual `abandoned_write_error` check after every decline is easy to get wrong.
- The caller that declined knows why it declined, so `e` carries nothing it cannot rebuild.

The cost: an outer `Err` no longer always means that `fill` did not run. It means the journal failed; `fill` did not
run, except when it declined and the clean-up failed, which is the one case where `abandoned_write_error` is set by
the call. A panic in the closure is unchanged: the panic continues whether or not the clean-up succeeded, and a failed
clean-up is recorded the same way.

The core has one clean-up function for both ways of abandoning a record (`Writer::abandon`, private and marked cold).
`append_with` is `try_append_with` with a closure that cannot fail (`Result<(), Infallible>`), so the two paths share
all of their code, and in `append` and `append_with` the decline branch matches on an uninhabited type, so the
compiler can remove it. A panic in either closure keeps the behaviour it had.

`tickertape_writer_append_with` uses `try_append_with`: a non-zero return from `fill` declines the record, and the
shim returns `TICKERTAPE_E_ABORTED`, or `TICKERTAPE_E_IO` if the clean-up failed, as before. Nothing in the C header's
signatures, status codes or documented behaviour changes. Unwinding is still how the library catches a Rust panic
(`TICKERTAPE_E_PANIC`), so the crate still refuses to build with `panic = "abort"`; a foreign exception escaping the
callback is still unsupported.

## Consequences

- An aborted fill no longer needs the library's unwind information, so it works with `libtickertape.a` linked by the
  GCC driver on macOS (run by hand with Homebrew GCC 15; CI does not use that compiler). The tests no longer skip
  their statically linked programs in that configuration.
- `tests/c_api.rs::aborted_fill_needs_no_unwinding` builds the static library with `panic = "abort"` (with the private
  compiler cfg `tickertape_test_panic_abort` in that build's own rustflags, which lifts the crate's compile-time check;
  it is not a Cargo feature, so `--all-features` cannot lift it, and the cfg without `panic = "abort"` does not compile)
  and runs the basic C test, including its aborted fill, against it. With the earlier mechanism that program ends with
  `SIGABRT` at the aborted fill.
- The Rust API gains `Writer::try_append_with`, which becomes stable at 1.0 with the rest of the writer.
- With the static library linked by the GCC driver on macOS, a Rust panic inside the library (a bug) ends the process
  instead of returning `TICKERTAPE_E_PANIC`; `docs/c-api.md` and `docs/cpp-api.md` say so.
