//! Message types shared by the examples and the tests. Each sets an explicit iceoryx2 type name, so that separately
//! compiled programs agree on it (the default is the Rust path, which includes the crate name).

#![allow(dead_code)]

use iceoryx2::prelude::ZeroCopySend;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// User header: a per-publisher message counter and an event time.
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    ZeroCopySend,
    IntoBytes,
    FromBytes,
    Immutable,
    KnownLayout,
)]
#[type_name("tickertape_iceoryx2::example::TickHeader")]
#[repr(C)]
pub struct TickHeader {
    /// Incremented by one per message, starting at 0.
    pub counter: u64,
    /// Nanoseconds since the Unix epoch when the publisher sent the message.
    pub venue_ts_ns: u64,
}

/// Payload of the `tick` services: 24 bytes, no padding.
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    ZeroCopySend,
    IntoBytes,
    FromBytes,
    Immutable,
    KnownLayout,
)]
#[type_name("tickertape_iceoryx2::example::Tick")]
#[repr(C)]
pub struct Tick {
    pub counter: u64,
    pub price: i64,
    pub qty: u32,
    pub side: u32,
}

impl Tick {
    /// The deterministic payload for message `counter`, so a receiver can check every byte.
    pub fn for_counter(counter: u64) -> Tick {
        Tick {
            counter,
            price: 10_000_000 + (counter as i64 % 1000) * 7 - 3500,
            qty: (counter % 97) as u32 + 1,
            side: (counter % 2) as u32,
        }
    }
}

/// The deterministic slice payload (`[u32]`, length 1 to 16) for message `counter` of the `slice` services.
pub fn slice_for_counter(counter: u64) -> Vec<u32> {
    let len = (counter % 16) as usize + 1;
    (0..len)
        .map(|i| (counter as u32).wrapping_mul(31).wrapping_add(i as u32))
        .collect()
}

/// Byte offset of `TickHeader::counter` (for `--counter header:0:8`).
pub const COUNTER_OFFSET: u32 = 0;
/// Byte offset of `TickHeader::venue_ts_ns`.
pub const VENUE_TS_OFFSET: u32 = 8;
