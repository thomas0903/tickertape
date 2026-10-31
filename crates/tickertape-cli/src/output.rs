use std::io::{self, Write};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Serialize;
use tickertape::Record;

use crate::{Decoder, Failure, Result};

#[derive(Serialize)]
struct JsonRecord {
    seq: u64,
    schema_id: u32,
    venue_ts_ns: u64,
    local_ts_ns: u64,
    payload_b64: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    decoded: Option<Option<tickertape_cli::market_demo::Decoded>>,
}

pub(crate) fn io_error(context: &str, error: impl std::fmt::Display) -> Failure {
    Failure::usage(format!("{context}: {error}"))
}

pub(crate) fn json(out: &mut dyn Write, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *out, value).map_err(|e| io_error("write JSON output", e))?;
    writeln!(out).map_err(|e| io_error("write output newline", e))
}

pub(crate) fn record(out: &mut dyn Write, record: &Record<'_>) -> Result<()> {
    export_record(out, record, None)
}

pub(crate) fn export_record(
    out: &mut dyn Write,
    record: &Record<'_>,
    decoder: Option<Decoder>,
) -> Result<()> {
    json(
        out,
        &JsonRecord {
            seq: record.seq,
            schema_id: record.schema_id,
            venue_ts_ns: record.venue_ts_ns,
            local_ts_ns: record.local_ts_ns,
            payload_b64: STANDARD.encode(record.payload),
            decoded: decoder.map(|d| d.decode(record)),
        },
    )
}

pub(crate) fn text(out: &mut dyn Write, record: &Record<'_>) -> io::Result<()> {
    write!(
        out,
        "seq={} schema={} venue_ts={} local_ts={} len={} payload=",
        record.seq,
        record.schema_id,
        record.venue_ts_ns,
        record.local_ts_ns,
        record.payload.len()
    )?;
    for byte in record.payload.iter().take(32) {
        write!(out, "{byte:02x}")?;
    }
    if record.payload.len() > 32 {
        write!(out, "…")?;
    }
    writeln!(out)
}
