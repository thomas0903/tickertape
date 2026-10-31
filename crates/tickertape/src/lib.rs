//! tickertape: a local, append-only event journal backed by memory-mapped segment files.
//!
//! The public API is specified in milestone M1; see `docs/format.md` and `docs/crash-contract.md` in the repository.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

/// The version of this crate, as set in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_set() {
        assert!(!super::VERSION.is_empty());
    }
}
