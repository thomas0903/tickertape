//! Little-endian market demo payloads. Identifiers and fixed-point values are preserved by the encoders.
//! Decoding rejects invalid lengths, reserved bytes and enum tags; unknown schemas return `None`.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookSide {
    Bid,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Upsert,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trade {
    pub instrument_id: u32,
    pub side: TradeSide,
    /// Signed fixed-point value, scaled by 10^8.
    pub price: i64,
    /// Signed fixed-point value, scaled by 10^8.
    pub qty: i64,
    pub trade_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Book {
    pub instrument_id: u32,
    pub side: BookSide,
    pub action: Action,
    /// Signed fixed-point value, scaled by 10^8.
    pub price: i64,
    /// Signed fixed-point value, scaled by 10^8.
    pub qty: i64,
}

/// Fields shared by decoded JSONL objects and optional Parquet columns.
#[derive(Debug, PartialEq, Serialize)]
pub struct Decoded {
    pub kind: &'static str,
    pub instrument_id: u32,
    pub side: &'static str,
    pub price: f64,
    pub qty: f64,
    pub trade_id: Option<u64>,
    pub action: Option<&'static str>,
}

/// Encode a schema 1 trade with zeroed reserved bytes.
pub fn encode_trade(trade: Trade) -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes[..4].copy_from_slice(&trade.instrument_id.to_le_bytes());
    bytes[4] = match trade.side {
        TradeSide::Buy => 1,
        TradeSide::Sell => 2,
    };
    bytes[8..16].copy_from_slice(&trade.price.to_le_bytes());
    bytes[16..24].copy_from_slice(&trade.qty.to_le_bytes());
    bytes[24..].copy_from_slice(&trade.trade_id.to_le_bytes());
    bytes
}

/// Encode a schema 2 book update with zeroed reserved bytes.
pub fn encode_book(book: Book) -> [u8; 24] {
    let mut bytes = [0; 24];
    bytes[..4].copy_from_slice(&book.instrument_id.to_le_bytes());
    bytes[4] = match book.side {
        BookSide::Bid => 1,
        BookSide::Ask => 2,
    };
    bytes[5] = match book.action {
        Action::Upsert => 1,
        Action::Delete => 2,
    };
    bytes[8..16].copy_from_slice(&book.price.to_le_bytes());
    bytes[16..24].copy_from_slice(&book.qty.to_le_bytes());
    bytes
}

/// Decode known, valid market demo payloads, or return `None` without affecting opaque export.
pub fn decode(schema_id: u32, payload: &[u8]) -> Option<Decoded> {
    let (kind, side, trade_id, action) = match (schema_id, payload.len()) {
        (1, 32) if payload[5..8] == [0; 3] => (
            "trade",
            match payload[4] {
                1 => "buy",
                2 => "sell",
                _ => return None,
            },
            Some(u64::from_le_bytes(payload[24..32].try_into().ok()?)),
            None,
        ),
        (2, 24) if payload[6..8] == [0; 2] => (
            "book",
            match payload[4] {
                1 => "bid",
                2 => "ask",
                _ => return None,
            },
            None,
            Some(match payload[5] {
                1 => "upsert",
                2 => "delete",
                _ => return None,
            }),
        ),
        _ => return None,
    };
    Some(Decoded {
        kind,
        instrument_id: u32::from_le_bytes(payload[..4].try_into().ok()?),
        side,
        price: i64::from_le_bytes(payload[8..16].try_into().ok()?) as f64 / 1e8,
        qty: i64::from_le_bytes(payload[16..24].try_into().ok()?) as f64 / 1e8,
        trade_id,
        action,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trade_layout_and_decode() {
        for (side, tag, name) in [(TradeSide::Buy, 1, "buy"), (TradeSide::Sell, 2, "sell")] {
            let bytes = encode_trade(Trade {
                instrument_id: u32::MAX,
                side,
                price: -12_350_000_000,
                qty: 250_000_000,
                trade_id: u64::MAX,
            });
            assert_eq!(&bytes[..8], &[255, 255, 255, 255, tag, 0, 0, 0]);
            assert_eq!(&bytes[8..16], &(-12_350_000_000_i64).to_le_bytes());
            assert_eq!(&bytes[16..24], &250_000_000_i64.to_le_bytes());
            assert_eq!(&bytes[24..], &u64::MAX.to_le_bytes());
            let d = decode(1, &bytes).unwrap();
            assert_eq!(
                d,
                Decoded {
                    kind: "trade",
                    instrument_id: u32::MAX,
                    side: name,
                    price: -123.5,
                    qty: 2.5,
                    trade_id: Some(u64::MAX),
                    action: None
                }
            );
        }
    }

    #[test]
    fn book_layout_and_decode() {
        for (side, tag, name) in [(BookSide::Bid, 1, "bid"), (BookSide::Ask, 2, "ask")] {
            for (action, action_tag, action_name) in
                [(Action::Upsert, 1, "upsert"), (Action::Delete, 2, "delete")]
            {
                let bytes = encode_book(Book {
                    instrument_id: 7,
                    side,
                    action,
                    price: i64::MIN,
                    qty: i64::MAX,
                });
                assert_eq!(&bytes[..8], &[7, 0, 0, 0, tag, action_tag, 0, 0]);
                assert_eq!(&bytes[8..16], &i64::MIN.to_le_bytes());
                assert_eq!(&bytes[16..24], &i64::MAX.to_le_bytes());
                assert_eq!(
                    decode(2, &bytes).unwrap(),
                    Decoded {
                        kind: "book",
                        instrument_id: 7,
                        side: name,
                        price: i64::MIN as f64 / 1e8,
                        qty: i64::MAX as f64 / 1e8,
                        trade_id: None,
                        action: Some(action_name)
                    }
                );
            }
        }
    }

    #[test]
    fn invalid_payloads_are_not_decodable() {
        for schema in [0, 1, 2, 3, u32::MAX] {
            for len in 0..40 {
                assert!(decode(schema, &vec![0; len]).is_none());
            }
        }
        for (schema, len) in [(1, 32), (2, 24)] {
            let mut valid = vec![0; len];
            valid[4] = 1;
            if schema == 2 {
                valid[5] = 1;
            }
            assert!(decode(schema, &valid).is_some());
            for offset in if schema == 1 { 5..8 } else { 6..8 } {
                let mut bad = valid.clone();
                bad[offset] = 1;
                assert!(decode(schema, &bad).is_none());
            }
            for tag in [0, 3, 255] {
                let mut bad = valid.clone();
                bad[4] = tag;
                assert!(decode(schema, &bad).is_none());
                if schema == 2 {
                    bad = valid.clone();
                    bad[5] = tag;
                    assert!(decode(schema, &bad).is_none());
                }
            }
        }
    }
}
