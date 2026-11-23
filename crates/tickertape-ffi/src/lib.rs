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

// The private compiler cfg `tickertape_test_panic_abort` lifts this check for one test only (tests/c_api.rs, which
// passes it in the rustflags of its own build), to show that an aborted fill needs no unwinding. It is not a Cargo
// feature, so `--all-features` cannot lift the check, and it is refused without `panic = "abort"`, so it cannot be
// left on by accident.
#[cfg(all(panic = "abort", not(tickertape_test_panic_abort)))]
compile_error!(
    "tickertape-ffi needs panic = \"unwind\": it reports caught panics as TICKERTAPE_E_PANIC (ADR 0028)"
);
#[cfg(all(tickertape_test_panic_abort, not(panic = "abort")))]
compile_error!(
    "the cfg tickertape_test_panic_abort is for one test of tickertape-ffi built with panic = \"abort\"; remove it"
);

mod args;
mod inspect;
mod metrics;
mod options;
mod reader;
mod status;
mod writer;

#[cfg(test)]
mod tests;

pub use inspect::*;
pub use metrics::*;
pub use options::*;
pub use reader::*;
pub use status::*;
pub use writer::*;

use std::ffi::c_char;

/// The version of the C ABI this header describes. Compare it with `tickertape_abi_version()` at start-up.
///
/// Before tickertape 1.0 the ABI version is 0, which means **unstable**: any release may change the ABI, so a program
/// (or binding) must load exactly the library version its header came from. From 1.0 the ABI version is 1 (and the
/// soname `libtickertape.so.1`); a later major version that breaks the ABI gets a new ABI version. Within one ABI
/// version, functions, constants, status codes and fields of extensible structs are only ever added, so a program
/// works with any library at least as new as its header. The start-up check that covers both cases:
///
/// ```c
/// uint32_t lib = tickertape_version_number();
/// int compatible = tickertape_abi_version() == TICKERTAPE_ABI_VERSION &&
///                  (TICKERTAPE_ABI_VERSION == 0 ? lib == TICKERTAPE_VERSION_NUMBER
///                                               : lib >= TICKERTAPE_VERSION_NUMBER);
/// ```
pub const TICKERTAPE_ABI_VERSION: u32 = 0;

/// The major version of the library this header belongs to.
pub const TICKERTAPE_VERSION_MAJOR: u32 = 0;

/// The minor version of the library this header belongs to.
pub const TICKERTAPE_VERSION_MINOR: u32 = 5;

/// The patch version of the library this header belongs to.
pub const TICKERTAPE_VERSION_PATCH: u32 = 1;

/// The version of the library this header belongs to as one comparable number: `MAJOR * 1000000 + MINOR * 1000 +
/// PATCH` (0.4.0 is 4000, 1.2.3 is 1002003). Compare it with `tickertape_version_number()`; see
/// `TICKERTAPE_ABI_VERSION` for the rule.
pub const TICKERTAPE_VERSION_NUMBER: u32 =
    TICKERTAPE_VERSION_MAJOR * 1000000 + TICKERTAPE_VERSION_MINOR * 1000 + TICKERTAPE_VERSION_PATCH;

/// Parses a decimal version component at compile time.
const fn parse_version(text: &str) -> u32 {
    let bytes = text.as_bytes();
    let mut value = 0;
    let mut i = 0;
    while i < bytes.len() {
        assert!(bytes[i].is_ascii_digit(), "version components are decimal");
        value = value * 10 + (bytes[i] - b'0') as u32;
        i += 1;
    }
    value
}

// The header's version macros are literals (cbindgen cannot evaluate `env!`); a version bump that forgets them fails
// to compile here. Update the constants above and regenerate the header.
#[allow(dead_code)]
fn version_macros_match_the_crate() {
    // Inside a function so that cbindgen does not parse it; the constant is evaluated all the same.
    const _: () = {
        assert!(TICKERTAPE_VERSION_MAJOR == parse_version(env!("CARGO_PKG_VERSION_MAJOR")));
        assert!(TICKERTAPE_VERSION_MINOR == parse_version(env!("CARGO_PKG_VERSION_MINOR")));
        assert!(TICKERTAPE_VERSION_PATCH == parse_version(env!("CARGO_PKG_VERSION_PATCH")));
        assert!(TICKERTAPE_VERSION_MINOR < 1000 && TICKERTAPE_VERSION_PATCH < 1000);
    };
}

/// Returns the C ABI version of the loaded library. See `TICKERTAPE_ABI_VERSION` for the start-up check.
#[unsafe(no_mangle)]
pub extern "C" fn tickertape_abi_version() -> u32 {
    TICKERTAPE_ABI_VERSION
}

/// Returns the version of the loaded library as one comparable number (`TICKERTAPE_VERSION_NUMBER` of the header it
/// was built with). See `TICKERTAPE_ABI_VERSION` for the start-up check.
#[unsafe(no_mangle)]
pub extern "C" fn tickertape_version_number() -> u32 {
    TICKERTAPE_VERSION_NUMBER
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
