//! Pure Binance message parsing, tested without opening network connections.

use crate::Event;

/// An exchange trade ready for the existing market-demo encoder.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trade {
    /// Schema 1 payload; `buy` denotes the taker side (`!m` in Binance messages).
    pub event: Event,
    /// Exchange trade time in nanoseconds since the Unix epoch.
    pub venue_ts_ns: u64,
}

/// Parses a signed decimal exactly at scale 10^8, without floating-point conversion.
///
/// Returns `None` for invalid syntax, i64 overflow, or nonzero digits beyond eight fractional places.
/// Additional trailing zeros are exact and accepted. Exponents, whitespace and a leading `+` are rejected.
pub fn decimal_to_fixed(input: &str) -> Option<i64> {
    let (negative, unsigned) = match input.strip_prefix('-') {
        Some(value) => (true, value),
        None => (false, input),
    };
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || (unsigned.contains('.') && fraction.is_empty())
        || fraction.bytes().skip(8).any(|b| b != b'0')
    {
        return None;
    }
    // Accumulate negatively to represent i64::MIN as well as i64::MAX exactly.
    let mut value = 0i64;
    for digit in whole
        .bytes()
        .chain(fraction.bytes().take(8))
        .chain(std::iter::repeat_n(
            b'0',
            8usize.saturating_sub(fraction.len()),
        ))
    {
        value = value
            .checked_mul(10)?
            .checked_sub(i64::from(digit - b'0'))?;
    }
    if negative {
        Some(value)
    } else {
        value.checked_neg()
    }
}

/// Parses raw or combined Binance trade JSON and maps a symbol to its position in `symbols`.
///
/// Symbol matching is ASCII case-insensitive. Invalid messages, other event types, unselected symbols,
/// nonpositive prices/quantities and overflowing timestamps return `None`; malformed input never panics.
pub fn parse_trade(json: &str, symbols: &[String]) -> Option<Trade> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let data = value.get("data").unwrap_or(&value);
    if data.get("e")?.as_str()? != "trade" {
        return None;
    }
    let symbol = data.get("s")?.as_str()?;
    let instrument_id = u32::try_from(
        symbols
            .iter()
            .position(|s| s.eq_ignore_ascii_case(symbol))?,
    )
    .ok()?;
    let price = decimal_to_fixed(data.get("p")?.as_str()?)?;
    let qty = decimal_to_fixed(data.get("q")?.as_str()?)?;
    if price <= 0 || qty <= 0 {
        return None;
    }
    Some(Trade {
        event: Event::Trade {
            instrument_id,
            buy: !data.get("m")?.as_bool()?,
            price,
            qty,
            trade_id: data.get("t")?.as_u64()?,
        },
        venue_ts_ns: data.get("T")?.as_u64()?.checked_mul(1_000_000)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hand-written fixtures following Binance's public trade stream format, not captured market data.
    const SAMPLES: [&str; 4] = [
        r#"{"stream":"btcusdt@trade","data":{"e":"trade","E":1793400000124,"s":"BTCUSDT","t":745392001,"p":"68420.12000000","q":"0.00123000","T":1793400000123,"m":false,"M":true}}"#,
        r#"{"e":"trade","E":1793400000224,"s":"ETHUSDT","t":123456789,"p":"2500.123456780000","q":"1.000000000000","T":1793400000223,"m":true,"M":true}"#,
        r#"{"e":"trade","s":"BTCUSDT","t":745392002,"p":"68420.123456789","q":"0.00100000","T":1793400000323,"m":false}"#,
        r#"{"e":"trade","s":"BTCUSDT","t":"broken","p":"oops","q":"1","T":-1,"m":null}"#,
    ];

    fn symbols() -> Vec<String> {
        vec!["btcusdt".into(), "ethusdt".into()]
    }

    #[test]
    fn raw_and_combined_trades_match_market_demo_schema() {
        let expected = [
            Trade {
                event: Event::Trade {
                    instrument_id: 0,
                    buy: true,
                    price: 6_842_012_000_000,
                    qty: 123_000,
                    trade_id: 745_392_001,
                },
                venue_ts_ns: 1_793_400_000_123_000_000,
            },
            Trade {
                event: Event::Trade {
                    instrument_id: 1,
                    buy: false,
                    price: 250_012_345_678,
                    qty: 100_000_000,
                    trade_id: 123_456_789,
                },
                venue_ts_ns: 1_793_400_000_223_000_000,
            },
        ];
        for (sample, expected) in SAMPLES.iter().zip(expected) {
            let trade = parse_trade(sample, &symbols()).unwrap();
            assert_eq!(trade, expected);
            assert_eq!(trade.event.schema_id(), 1);
            assert_eq!(
                Event::decode(1, &trade.event.encode()),
                Some(expected.event)
            );
        }
    }

    #[test]
    fn decimal_precision_and_signed_boundaries_are_exact() {
        for (input, expected) in [
            ("0", 0),
            ("-0.00000001", -1),
            ("0001.2300000000000", 123_000_000),
            ("92233720368.54775807", i64::MAX),
            ("-92233720368.54775808", i64::MIN),
        ] {
            assert_eq!(decimal_to_fixed(input), Some(expected), "{input}");
        }
        for input in [
            "",
            "-",
            ".1",
            "1.",
            "+1",
            " 1",
            "1e2",
            "NaN",
            "1.2.3",
            "１",
            "0.000000001",
            "92233720368.54775808",
            "-92233720368.54775809",
            "999999999999999999999999999999999999",
        ] {
            assert_eq!(decimal_to_fixed(input), None, "{input}");
        }
    }

    #[test]
    fn malformed_and_unrepresentable_messages_are_skipped() {
        for sample in &SAMPLES[2..] {
            assert_eq!(parse_trade(sample, &symbols()), None);
        }
        for sample in ["", "{", "null", "[]", r#"{"data":{}}"#] {
            assert_eq!(parse_trade(sample, &symbols()), None);
        }
        for (field, value) in [
            ("e", serde_json::json!("aggTrade")),
            ("s", serde_json::json!("SOLUSDT")),
            ("t", serde_json::json!(-1)),
            ("T", serde_json::json!(u64::MAX)),
            ("p", serde_json::json!("92233720368.54775808")),
            ("p", serde_json::json!("-1")),
            ("q", serde_json::json!("0")),
            ("m", serde_json::json!("false")),
        ] {
            let mut msg: serde_json::Value = serde_json::from_str(SAMPLES[1]).unwrap();
            msg[field] = value;
            assert_eq!(parse_trade(&msg.to_string(), &symbols()), None, "{field}");
        }
        let mut msg: serde_json::Value = serde_json::from_str(SAMPLES[1]).unwrap();
        for field in ["e", "s", "t", "T", "p", "q", "m"] {
            let removed = msg.as_object_mut().unwrap().remove(field).unwrap();
            assert_eq!(parse_trade(&msg.to_string(), &symbols()), None, "{field}");
            msg[field] = removed;
        }
        assert_eq!(parse_trade(SAMPLES[0], &[]), None);
    }
}
