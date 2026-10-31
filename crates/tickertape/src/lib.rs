//! tickertape: a local, append-only event journal backed by memory-mapped segment files.
//!
//! One [`Writer`] process appends records; any number of [`Reader`]s, in the same or other processes on the same
//! host, follow the journal live or replay it from any sequence number. Records are opaque payload bytes with a
//! sequence number, a caller-defined schema id and two caller-supplied timestamps.
//!
//! - The byte layout is specified in `docs/format.md` in the repository, and mirrored by the [`mod@format`] module.
//! - What survives a process crash or power loss is specified in `docs/crash-contract.md`. In short: a record is
//!   *visible* to readers, and survives the writer process dying, as soon as [`Writer::append`] returns; it is
//!   *durable* only after a later [`Writer::checkpoint`] returns.
//!
//! ```no_run
//! use tickertape::{NewRecord, Reader, ReaderConfig, Writer, WriterConfig};
//!
//! # fn main() -> tickertape::Result<()> {
//! let mut writer = Writer::open("/tmp/journal", WriterConfig::default())?;
//! let seq = writer.append(&NewRecord {
//!     schema_id: 1,
//!     venue_ts_ns: 0,
//!     local_ts_ns: tickertape::clock::realtime_ns(),
//!     payload: b"hello",
//! })?;
//! writer.checkpoint()?; // make it durable, not only visible
//!
//! let mut reader = Reader::open("/tmp/journal", ReaderConfig::default())?;
//! reader.seek(seq)?;
//! if let Some(record) = reader.try_next()? {
//!     assert_eq!(record.payload, b"hello");
//! }
//! # Ok(())
//! # }
//! ```

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

#[cfg(not(target_endian = "little"))]
compile_error!(
    "tickertape's on-disk format is little-endian and only little-endian targets are supported"
);

#[cfg(not(unix))]
compile_error!("tickertape supports Unix (Linux and macOS) only");

pub mod clock;
mod config;
mod error;
#[cfg(feature = "fault-injection")]
#[doc(hidden)]
pub mod fault;
#[cfg(not(feature = "fault-injection"))]
mod fault;
pub mod format;
pub mod inspect;
mod journal;
mod mmap;
mod reader;
mod record;
mod writer;

pub use config::{Backoff, ReaderConfig, RecoveryPolicy, WriterConfig};
pub use error::{CorruptReason, Error, Result};
pub use reader::Reader;
pub use record::{NewRecord, OwnedRecord, Record};
pub use writer::{RecoveryAction, RecoveryReport, Writer};

/// The version of this crate, as set in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_set() {
        assert!(!super::VERSION.is_empty());
    }
}
