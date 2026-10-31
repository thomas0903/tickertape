//! The example market-data schema used by the demo producer and consumer.
//!
//! The byte layout is documented in `docs/cli.md` → Market demo decoder, and `tickertape export --decode
//! market-demo` decodes it. All integers are little-endian; prices and quantities are fixed point (value × 10^8).
//!
//! | schema_id | Event | Payload |
//! |---|---|---|
//! | 1 | trade | 32 bytes: `instrument_id u32 @0`, `side u8 @4` (1 buy, 2 sell), 3 zero bytes, `price i64 @8`, `qty i64 @16`, `trade_id u64 @24` |
//! | 2 | book update | 24 bytes: `instrument_id u32 @0`, `side u8 @4` (1 bid, 2 ask), `action u8 @5` (1 upsert, 2 delete), 2 zero bytes, `price i64 @8`, `qty i64 @16` |

/// Schema id of trades.
pub const TRADE: u32 = 1;
/// Schema id of order-book updates.
pub const BOOK: u32 = 2;
/// Fixed-point scale of prices and quantities.
pub const SCALE: f64 = 1e8;

/// A decoded event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    /// A trade.
    Trade {
        /// Instrument.
        instrument_id: u32,
        /// True for a buyer-initiated trade.
        buy: bool,
        /// Price (fixed point, × 10^8).
        price: i64,
        /// Quantity (fixed point, × 10^8).
        qty: i64,
        /// Exchange trade id.
        trade_id: u64,
    },
    /// An order-book level update.
    Book {
        /// Instrument.
        instrument_id: u32,
        /// True for the bid side.
        bid: bool,
        /// True if the level was removed.
        delete: bool,
        /// Price (fixed point, × 10^8).
        price: i64,
        /// New quantity at the level (fixed point, × 10^8).
        qty: i64,
    },
}

impl Event {
    /// The schema id this event is stored under.
    pub fn schema_id(&self) -> u32 {
        match self {
            Event::Trade { .. } => TRADE,
            Event::Book { .. } => BOOK,
        }
    }

    /// Encodes the payload.
    pub fn encode(&self) -> Vec<u8> {
        match *self {
            Event::Trade {
                instrument_id,
                buy,
                price,
                qty,
                trade_id,
            } => {
                let mut b = vec![0u8; 32];
                b[0..4].copy_from_slice(&instrument_id.to_le_bytes());
                b[4] = if buy { 1 } else { 2 };
                b[8..16].copy_from_slice(&price.to_le_bytes());
                b[16..24].copy_from_slice(&qty.to_le_bytes());
                b[24..32].copy_from_slice(&trade_id.to_le_bytes());
                b
            }
            Event::Book {
                instrument_id,
                bid,
                delete,
                price,
                qty,
            } => {
                let mut b = vec![0u8; 24];
                b[0..4].copy_from_slice(&instrument_id.to_le_bytes());
                b[4] = if bid { 1 } else { 2 };
                b[5] = if delete { 2 } else { 1 };
                b[8..16].copy_from_slice(&price.to_le_bytes());
                b[16..24].copy_from_slice(&qty.to_le_bytes());
                b
            }
        }
    }

    /// Decodes a payload stored under `schema_id`. `None` for other schema ids or malformed payloads.
    pub fn decode(schema_id: u32, p: &[u8]) -> Option<Event> {
        let u32_at = |i: usize| Some(u32::from_le_bytes(p.get(i..i + 4)?.try_into().ok()?));
        let i64_at = |i: usize| Some(i64::from_le_bytes(p.get(i..i + 8)?.try_into().ok()?));
        match (schema_id, p.len()) {
            (TRADE, 32) => Some(Event::Trade {
                instrument_id: u32_at(0)?,
                buy: match p[4] {
                    1 => true,
                    2 => false,
                    _ => return None,
                },
                price: i64_at(8)?,
                qty: i64_at(16)?,
                trade_id: u64::from_le_bytes(p[24..32].try_into().ok()?),
            }),
            (BOOK, 24) => Some(Event::Book {
                instrument_id: u32_at(0)?,
                bid: match p[4] {
                    1 => true,
                    2 => false,
                    _ => return None,
                },
                delete: match p[5] {
                    1 => false,
                    2 => true,
                    _ => return None,
                },
                price: i64_at(8)?,
                qty: i64_at(16)?,
            }),
            _ => None,
        }
    }
}

/// A tiny deterministic random number generator (xorshift64*), so demo runs are reproducible from a seed.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator with the given seed.
    pub fn new(seed: u64) -> Rng {
        Rng(seed.max(1))
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A number in `0..n` (n > 0).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let events = [
            Event::Trade {
                instrument_id: 7,
                buy: true,
                price: 12_350_000_000,
                qty: 200_000_000,
                trade_id: 42,
            },
            Event::Trade {
                instrument_id: u32::MAX,
                buy: false,
                price: -1,
                qty: 0,
                trade_id: u64::MAX,
            },
            Event::Book {
                instrument_id: 3,
                bid: true,
                delete: false,
                price: 100,
                qty: 5,
            },
            Event::Book {
                instrument_id: 3,
                bid: false,
                delete: true,
                price: 101,
                qty: 0,
            },
        ];
        for e in events {
            assert_eq!(Event::decode(e.schema_id(), &e.encode()), Some(e));
        }
        assert_eq!(Event::decode(TRADE, &[0; 24]), None);
        assert_eq!(Event::decode(9, &[0; 32]), None);
        let mut bad = events[2].encode();
        bad[5] = 9;
        assert_eq!(Event::decode(BOOK, &bad), None);
    }

    #[test]
    fn matches_the_documented_layout() {
        // Byte-for-byte example from docs/cli.md: trade, instrument 7, buy, price 123.5, qty 2.0, trade id 42.
        let e = Event::Trade {
            instrument_id: 7,
            buy: true,
            price: 12_350_000_000,
            qty: 200_000_000,
            trade_id: 42,
        };
        let b = e.encode();
        assert_eq!(&b[0..8], &[7, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(
            i64::from_le_bytes(b[8..16].try_into().unwrap()) as f64 / SCALE,
            123.5
        );
        assert_eq!(
            i64::from_le_bytes(b[16..24].try_into().unwrap()) as f64 / SCALE,
            2.0
        );
        assert_eq!(u64::from_le_bytes(b[24..32].try_into().unwrap()), 42);
    }
}
