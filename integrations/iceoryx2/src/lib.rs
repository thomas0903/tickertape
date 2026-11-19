//! Record [iceoryx2](https://github.com/eclipse-iceoryx/iceoryx2) publish/subscribe services into a
//! [tickertape](https://github.com/thomas0903/tickertape) journal, follow the recording live from other processes,
//! replay it back onto iceoryx2, and convert it to and from iceoryx2's own recording files.
//!
//! - [`layout`]: the byte layout of the records this crate writes (version 1). It has no iceoryx2 dependency, and
//!   is all a reader in another language needs: any tickertape reader plus that layout.
//! - [`Catalog`] and [`info`]: look up descriptors and summarize a recording, also without iceoryx2.
//! - `recorder` (feature `iceoryx2`, on by default): `Recorder` records one or several services into one journal.
//! - `replayer` (feature `iceoryx2`): `Replayer` publishes recorded messages again, as fast as possible or at the
//!   recorded pace. Typed replay is safe; replaying services by their recorded type details is an `unsafe fn`
//!   (`Replayer::run_dynamic`, feature `dynamic`), because the recorded bytes are published as values of the
//!   recorded types: replay only recordings you trust.
//! - `dynamic` and `upstream` (feature `dynamic`): services whose Rust types are only known at run time, and
//!   iceoryx2's recording file format. These use iceoryx2 API that upstream marks internal; see the `dynamic`
//!   module.
//!
//! A typed example is in the `recorder` module.
//!
//! # What a recording proves, and what it doesn't
//!
//! tickertape's sequence numbers are gap-free, so they prove that nothing the recorder *wrote* was lost. They
//! cannot show messages the recorder never received. iceoryx2 delivers into a bounded subscriber buffer; when it
//! is full, a service with safe overflow (iceoryx2's default) silently replaces the oldest sample, and the recorder
//! never sees it. Give each publisher a counter in its user header or payload and tell the recorder where it is
//! ([`layout::CounterField`]); the recorder then writes a [`layout::LossRecord`] for every gap. And as in
//! tickertape's crash contract, a recorded message is *visible* to readers as soon as it is written, but *durable*
//! only after the next checkpoint.

#![warn(missing_docs)]
// `unsafe` is allowed in exactly one module, `dynamic` (see CONTRIBUTING.md).
#![deny(unsafe_code)]

mod catalog;
mod error;
pub mod info;
pub mod layout;
#[cfg(feature = "iceoryx2")]
mod tail;

#[cfg(feature = "dynamic")]
#[allow(unsafe_code)]
pub mod dynamic;
#[cfg(feature = "iceoryx2")]
mod pacing;
#[cfg(feature = "iceoryx2")]
pub mod recorder;
#[cfg(feature = "iceoryx2")]
pub mod replayer;
#[cfg(feature = "iceoryx2")]
mod typed;
#[cfg(feature = "dynamic")]
pub mod upstream;

pub use catalog::Catalog;
pub use error::Error;

#[cfg(feature = "iceoryx2")]
pub use typed::{Recordable, RecordableHeader, Replayable, ReplayableHeader};

/// The version of this crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The `recorder` string this crate writes into descriptors.
pub fn recorder_name() -> String {
    format!("tickertape-iceoryx2 {VERSION}")
}
