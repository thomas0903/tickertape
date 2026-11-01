//! Single-pass statistics; retain only aggregates and a fixed payload histogram in RAM.

use std::collections::BTreeMap;
use std::io::Write;

use serde::Serialize;
use tickertape::{Reader, ReaderConfig, Record};

use crate::{Failure, Result, StatsArgs, output, report::rfc3339, stats_seconds::Seconds};

const SECOND: u64 = 1_000_000_000;
const EXACT: usize = 4096;

pub(crate) const HELP: &str = r#"One nonblocking startup scan, never following or rereading records. The end is observed during
that scan, not an atomic snapshot of a live writer. Ranges are inclusive; --to must not precede
--from. --durable-only freezes checkpoint coverage before scanning (no valid marker = no records).
Corruption or input I/O failure prints no stats, only an error on stderr.

Payload percentiles use nearest-rank (ceil(p * records / 100)): exact lengths through 4096 bytes;
above 4096 they are power-of-two bucket upper bounds. Min/max and schema means use actual lengths.
Zero timestamps are unset. Decreases compare adjacent selected records only when both fields are
nonzero. Both busiest_second values count fixed UTC seconds (ts / 1e9), including unordered input;
ties choose the earliest second. This is not a sliding window. Rate = set records / span_s;
a zero span has null rate. Largest local gap uses adjacent nonzero, nondecreasing timestamps;
ties choose the first pair. UTC strings use RFC 3339 with nine fractional digits and Z.
Exact second counts use bounded RAM and anonymous temporary sort files outside the journal when
needed (temporary disk space grows with timestamp count). Per-schema aggregates stay in memory.

JSON schema (one object; u64 integers unless schema_id:u32; number = floating point):
{"totals":{"records":u64,"first_seq":u64|null,"last_seq":u64|null,"payload_bytes":u64},
 "schemas":[{"schema_id":u32,"records":u64,"share_percent":number,"payload_bytes":u64,
 "min_payload_bytes":u64,"mean_payload_bytes":number,"max_payload_bytes":u64}],
 "payload_length":{"min_bytes":u64|null,"p50_bytes":u64|null,"p90_bytes":u64|null,
 "p99_bytes":u64|null,"max_bytes":u64|null},
 "venue_ts":TIME,"local_ts":TIME,
 "largest_local_gap":{"gap_s":number,"from_seq":u64,"to_seq":u64}|null}
TIME = {"records":u64,"min_ns":u64|null,"max_ns":u64|null,"min_utc":string|null,
 "max_utc":string|null,"span_s":number|null,"mean_rate_per_s":number|null,
 "busiest_second":{"records":u64,"start_ns":u64,"start_utc":string}|null,"decreases":u64}.
Schemas are sorted by id; share_percent = 100 * schema records / total records. Empty summaries
use null extrema/percentiles/times/gap, zero counts/bytes, and an empty schemas array.

Exit codes: 0 success (or help); 1 usage, I/O or missing journal; 2 corruption or unsupported journal format."#;

#[derive(Default, Serialize)]
struct Totals {
    records: u64,
    first_seq: Option<u64>,
    last_seq: Option<u64>,
    payload_bytes: u64,
}

#[derive(Serialize)]
struct Schema {
    schema_id: u32,
    records: u64,
    share_percent: f64,
    payload_bytes: u64,
    min_payload_bytes: u64,
    mean_payload_bytes: f64,
    max_payload_bytes: u64,
}

#[derive(Serialize)]
struct PayloadLength {
    min_bytes: Option<u64>,
    p50_bytes: Option<u64>,
    p90_bytes: Option<u64>,
    p99_bytes: Option<u64>,
    max_bytes: Option<u64>,
}

#[derive(Serialize)]
struct BusiestSecond {
    records: u64,
    start_ns: u64,
    start_utc: String,
}

#[derive(Serialize)]
struct Time {
    records: u64,
    min_ns: Option<u64>,
    max_ns: Option<u64>,
    min_utc: Option<String>,
    max_utc: Option<String>,
    span_s: Option<f64>,
    mean_rate_per_s: Option<f64>,
    busiest_second: Option<BusiestSecond>,
    decreases: u64,
}

#[derive(Serialize)]
struct Gap {
    gap_s: f64,
    from_seq: u64,
    to_seq: u64,
}

#[derive(Serialize)]
struct Stats {
    totals: Totals,
    schemas: Vec<Schema>,
    payload_length: PayloadLength,
    venue_ts: Time,
    local_ts: Time,
    largest_local_gap: Option<Gap>,
}

#[derive(Default)]
struct Lengths {
    records: u64,
    bytes: u64,
    min: Option<u64>,
    max: Option<u64>,
}

impl Lengths {
    fn push(&mut self, length: u64) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(length)
            .ok_or_else(|| Failure::usage("total payload bytes exceeds u64"))?;
        self.records += 1;
        self.min = Some(self.min.map_or(length, |min| min.min(length)));
        self.max = Some(self.max.map_or(length, |max| max.max(length)));
        Ok(())
    }
}

#[derive(Default)]
struct Clock {
    records: u64,
    min: Option<u64>,
    max: Option<u64>,
    previous: u64,
    decreases: u64,
    seconds: Seconds,
}

impl Clock {
    fn push(&mut self, ns: u64) -> Result<()> {
        if ns != 0 {
            self.records += 1;
            self.min = Some(self.min.map_or(ns, |min| min.min(ns)));
            self.max = Some(self.max.map_or(ns, |max| max.max(ns)));
            self.decreases += u64::from(self.previous != 0 && ns < self.previous);
            self.seconds.push(ns / SECOND).map_err(temp_error)?;
        }
        self.previous = ns;
        Ok(())
    }

    fn finish(self) -> Result<Time> {
        let span_s = self
            .min
            .zip(self.max)
            .map(|(min, max)| (max - min) as f64 / SECOND as f64);
        Ok(Time {
            records: self.records,
            min_ns: self.min,
            max_ns: self.max,
            min_utc: self.min.map(rfc3339),
            max_utc: self.max.map(rfc3339),
            span_s,
            mean_rate_per_s: span_s
                .filter(|&span| span > 0.0)
                .map(|span| self.records as f64 / span),
            busiest_second: self
                .seconds
                .busiest()
                .map_err(temp_error)?
                .map(|(second, records)| {
                    let start_ns = second * SECOND;
                    BusiestSecond {
                        records,
                        start_ns,
                        start_utc: rfc3339(start_ns),
                    }
                }),
            decreases: self.decreases,
        })
    }
}

fn temp_error(error: std::io::Error) -> Failure {
    output::io_error("aggregate timestamp seconds in temporary storage", error)
}

struct Accumulator {
    totals: Totals,
    lengths: Lengths,
    schemas: BTreeMap<u32, Lengths>,
    // Every length through EXACT, plus buckets (2^12, 2^13] ... (2^31, 2^32].
    histogram: [u64; EXACT + 1 + 20],
    venue: Clock,
    local: Clock,
    previous_local: Option<(u64, u64)>,
    largest_gap: Option<(u64, u64, u64)>,
}

impl Default for Accumulator {
    fn default() -> Self {
        Self {
            totals: Totals::default(),
            lengths: Lengths::default(),
            schemas: BTreeMap::new(),
            histogram: [0; EXACT + 1 + 20],
            venue: Clock::default(),
            local: Clock::default(),
            previous_local: None,
            largest_gap: None,
        }
    }
}

impl Accumulator {
    fn push(&mut self, record: &Record<'_>) -> Result<()> {
        let length = record.payload.len() as u64;
        self.lengths.push(length)?;
        self.schemas
            .entry(record.schema_id)
            .or_default()
            .push(length)?;
        let bucket = if length <= EXACT as u64 {
            length as usize
        } else {
            EXACT + (64 - (length - 1).leading_zeros()) as usize - 12
        };
        self.histogram[bucket] += 1;
        self.totals.records += 1;
        self.totals.first_seq.get_or_insert(record.seq);
        self.totals.last_seq = Some(record.seq);
        self.totals.payload_bytes = self.lengths.bytes;
        self.venue.push(record.venue_ts_ns)?;
        self.local.push(record.local_ts_ns)?;
        if let Some((seq, previous)) = self.previous_local {
            if previous != 0 && record.local_ts_ns != 0 {
                if let Some(gap) = record.local_ts_ns.checked_sub(previous) {
                    if self.largest_gap.is_none_or(|(largest, _, _)| gap > largest) {
                        self.largest_gap = Some((gap, seq, record.seq));
                    }
                }
            }
        }
        self.previous_local = Some((record.seq, record.local_ts_ns));
        Ok(())
    }

    fn percentile(&self, percent: u64) -> Option<u64> {
        if self.totals.records == 0 {
            return None;
        }
        let rank = (u128::from(self.totals.records) * u128::from(percent)).div_ceil(100);
        let mut count = 0_u128;
        for (bucket, &records) in self.histogram.iter().enumerate() {
            count += u128::from(records);
            if count >= rank {
                return Some(if bucket <= EXACT {
                    bucket as u64
                } else {
                    1_u64 << (12 + bucket - EXACT)
                });
            }
        }
        unreachable!("histogram contains every record")
    }

    fn finish(self) -> Result<Stats> {
        let payload_length = PayloadLength {
            min_bytes: self.lengths.min,
            p50_bytes: self.percentile(50),
            p90_bytes: self.percentile(90),
            p99_bytes: self.percentile(99),
            max_bytes: self.lengths.max,
        };
        Ok(Stats {
            schemas: self
                .schemas
                .into_iter()
                .map(|(schema_id, lengths)| Schema {
                    schema_id,
                    records: lengths.records,
                    share_percent: 100.0 * lengths.records as f64 / self.totals.records as f64,
                    payload_bytes: lengths.bytes,
                    min_payload_bytes: lengths.min.expect("nonempty schema"),
                    mean_payload_bytes: lengths.bytes as f64 / lengths.records as f64,
                    max_payload_bytes: lengths.max.expect("nonempty schema"),
                })
                .collect(),
            totals: self.totals,
            payload_length,
            venue_ts: self.venue.finish()?,
            local_ts: self.local.finish()?,
            largest_local_gap: self.largest_gap.map(|(gap, from_seq, to_seq)| Gap {
                gap_s: gap as f64 / SECOND as f64,
                from_seq,
                to_seq,
            }),
        })
    }
}

/// Aggregate once at startup, stopping at the first uncommitted slot. No output until all input validates.
pub(crate) fn run(args: StatsArgs, out: &mut dyn Write) -> Result<()> {
    if args.from.zip(args.to).is_some_and(|(from, to)| to < from) {
        return Err(Failure::usage(
            "--to must be greater than or equal to --from",
        ));
    }
    let end = if args.durable_only {
        Some(tickertape::inspect::read_checkpoint(&args.dir)?.map_or(0, |c| c.next_seq))
    } else {
        None
    };
    let mut reader = Reader::open(
        &args.dir,
        ReaderConfig::default()
            .durable_only(args.durable_only)
            .notify(false),
    )?;
    if let Some(from) = args.from {
        reader.seek(from)?;
    }
    let mut stats = Accumulator::default();
    stats.venue.seconds = Seconds::outside_journal(&args.dir);
    stats.local.seconds = Seconds::outside_journal(&args.dir);
    loop {
        // Check before reading so corruption beyond the selected range/coverage cannot affect this report.
        if end.is_some_and(|end| reader.position() >= end)
            || args.to.is_some_and(|to| reader.position() > to)
        {
            break;
        }
        let Some(record) = reader.try_next()? else {
            break;
        };
        stats.push(&record)?;
    }
    let stats = stats.finish()?;
    if args.json {
        output::json(out, &stats)
    } else {
        stats
            .text(out)
            .map_err(|e| output::io_error("write stats output", e))
    }
}

fn optional(value: Option<impl std::fmt::Display>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "none".into())
}

impl Stats {
    fn text(&self, out: &mut dyn Write) -> std::io::Result<()> {
        writeln!(
            out,
            "records={} first_seq={} last_seq={} payload_bytes={}",
            self.totals.records,
            optional(self.totals.first_seq),
            optional(self.totals.last_seq),
            self.totals.payload_bytes
        )?;
        writeln!(
            out,
            "{:>10} {:>10} {:>8} {:>14} {:>10} {:>12} {:>10}",
            "schema", "records", "share %", "payload bytes", "min bytes", "mean bytes", "max bytes"
        )?;
        for s in &self.schemas {
            writeln!(
                out,
                "{:>10} {:>10} {:>8.2} {:>14} {:>10} {:>12.2} {:>10}",
                s.schema_id,
                s.records,
                s.share_percent,
                s.payload_bytes,
                s.min_payload_bytes,
                s.mean_payload_bytes,
                s.max_payload_bytes
            )?;
        }
        let p = &self.payload_length;
        writeln!(
            out,
            "payload bytes: min={} p50={} p90={} p99={} max={} (percentiles above 4096: bucket upper bounds)",
            optional(p.min_bytes),
            optional(p.p50_bytes),
            optional(p.p90_bytes),
            optional(p.p99_bytes),
            optional(p.max_bytes)
        )?;
        for (name, time) in [("venue", &self.venue_ts), ("local", &self.local_ts)] {
            writeln!(
                out,
                "{name:>5} time: records={} decreases={} span_s={} mean_rate_per_s={}",
                time.records,
                time.decreases,
                optional(time.span_s),
                optional(time.mean_rate_per_s)
            )?;
            writeln!(
                out,
                "            min_ns={} ({}) max_ns={} ({})",
                optional(time.min_ns),
                time.min_utc.as_deref().unwrap_or("none"),
                optional(time.max_ns),
                time.max_utc.as_deref().unwrap_or("none")
            )?;
            match &time.busiest_second {
                Some(b) => writeln!(
                    out,
                    "            busiest UTC second: records={} start_ns={} ({})",
                    b.records, b.start_ns, b.start_utc
                )?,
                None => writeln!(out, "            busiest UTC second: none")?,
            }
        }
        match &self.largest_local_gap {
            Some(g) => writeln!(
                out,
                "largest local gap: gap_s={} from_seq={} to_seq={}",
                g.gap_s, g.from_seq, g.to_seq
            ),
            None => writeln!(out, "largest local gap: none"),
        }
    }
}
