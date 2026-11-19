//! The tickertape C ABI (ADR 0028, `docs/c-api.md`).
//!
//! This crate builds `libtickertape.{so,dylib,a}` and is the source of the generated header
//! `include/tickertape.h`: every doc comment on an exported item is copied into it. It uses only the public API of
//! the `tickertape` crate.
//!
//! `unsafe` code is allowed in this crate as a deliberate, scoped extension of the repository rule (ADR 0028): it
//! dereferences raw pointers from C and exports unmangled symbols. Every `unsafe` block is small and has a
//! `// SAFETY:` comment, and every `unsafe extern "C" fn` documents its contract in a `# Safety` section.

#![warn(missing_docs)]

#[cfg(panic = "abort")]
compile_error!(
    "tickertape-ffi needs panic = \"unwind\": it reports caught panics as TICKERTAPE_E_PANIC and aborts fill \
     callbacks by unwinding (ADR 0028)"
);

mod args;
mod inspect;
mod options;
mod reader;
mod status;
mod writer;

#[cfg(test)]
mod tests;

pub use inspect::*;
pub use options::*;
pub use reader::*;
pub use status::*;
pub use writer::*;

use std::ffi::c_char;

/// The version of the C ABI this header describes. Compare it with `tickertape_abi_version()` at start-up.
///
/// Before tickertape 1.0 the ABI version is 0 and the ABI may change in any minor release (the changelog says when).
/// From 1.0 it is 1, and functions, constants and struct layouts are only ever added, never removed or changed.
pub const TICKERTAPE_ABI_VERSION: u32 = 0;

/// Returns the C ABI version of the loaded library; it must equal `TICKERTAPE_ABI_VERSION` from the header the
/// program was compiled with.
#[unsafe(no_mangle)]
pub extern "C" fn tickertape_abi_version() -> u32 {
    TICKERTAPE_ABI_VERSION
}

/// Returns the library version, such as `"0.4.0"`. The string is static; never NULL.
#[unsafe(no_mangle)]
pub extern "C" fn tickertape_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// Returns the system wall-clock time in nanoseconds since the Unix epoch (`CLOCK_REALTIME`: it can jump, and the
/// units do not imply nanosecond accuracy), or 0 if the clock is before the epoch. A convenience for record
/// timestamps, which the library never interprets.
#[unsafe(no_mangle)]
pub extern "C" fn tickertape_realtime_ns() -> u64 {
    tickertape::clock::realtime_ns()
}

/// Handles may be moved between threads (but are never used by two threads at once; ADR 0028 → Threading): this
/// fails to compile if the core types stop being `Send`.
#[allow(dead_code)]
fn handles_can_move_between_threads() {
    fn assert_send<T: Send>() {}
    assert_send::<tickertape::Writer>();
    assert_send::<tickertape::Reader>();
}
