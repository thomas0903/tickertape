use std::io::Write;
use std::path::Path;

use serde::Serialize;
use tickertape::inspect::{self, SegmentEnd, VerifyReport};

use crate::{Failure, ReportArgs, Result, output};

pub(crate) const VERIFY_HELP: &str = "JSON schema: {\"segments\":u64,\"records\":u64,\"first_seq\":u64|null,\"last_seq\":u64|null,\"problems\":[{\"file\":string,\"offset\":u64,\"reason\":string}]}. file is a basename; reason is a human-readable diagnostic.\n\nExit codes: 0 intact (or help); 1 usage, I/O or missing journal; 2 corruption found.";
pub(crate) const INSPECT_HELP: &str = "JSON schema: {\"segments\":[{\"file\":string,\"base_seq\":u64|null,\"size_bytes\":u64|null,\"created_unix_ns\":u64|null,\"created_utc\":string|null,\"records\":u64,\"end_seq\":u64,\"used_bytes\":u64,\"used_percent\":number|null,\"end_state\":\"sealed\"|\"open\"|\"damaged\"|\"unknown\"}],\"totals\":{\"segments\":u64,\"records\":u64,\"first_seq\":u64|null,\"last_seq\":u64|null},\"checkpoint\":{\"generation\":u64,\"next_seq\":u64,\"segment_base_seq\":u64,\"offset\":u64}|null,\"problems\":[{\"file\":string,\"offset\":u64,\"reason\":string}]}. Metadata is null for invalid headers. created_utc is RFC 3339 UTC with nine fractional digits. end_seq is exclusive. used_bytes includes header and seal; used_percent = 100 * used_bytes / size_bytes. file is a basename; reason is a human-readable diagnostic.\n\nExit codes: 0 intact (or help); 1 usage, I/O or missing journal; 2 corruption found.";

#[derive(Serialize)]
struct Totals {
    segments: usize,
    records: u64,
    first_seq: Option<u64>,
    last_seq: Option<u64>,
}

impl From<&VerifyReport> for Totals {
    fn from(report: &VerifyReport) -> Self {
        Self {
            segments: report.segments.len(),
            records: report.records,
            first_seq: report.first_seq,
            last_seq: report.last_seq,
        }
    }
}

#[derive(Serialize)]
struct Problem {
    file: String,
    offset: u64,
    reason: String,
}

#[derive(Serialize)]
struct Verification {
    #[serde(flatten)]
    totals: Totals,
    problems: Vec<Problem>,
}

#[derive(Serialize)]
struct Segment {
    file: String,
    base_seq: Option<u64>,
    size_bytes: Option<u64>,
    created_unix_ns: Option<u64>,
    created_utc: Option<String>,
    records: u64,
    end_seq: u64,
    used_bytes: u64,
    used_percent: Option<f64>,
    end_state: &'static str,
}

#[derive(Serialize)]
struct Checkpoint {
    generation: u64,
    next_seq: u64,
    segment_base_seq: u64,
    offset: u64,
}

#[derive(Serialize)]
struct Inspection {
    segments: Vec<Segment>,
    totals: Totals,
    checkpoint: Option<Checkpoint>,
    problems: Vec<Problem>,
}

fn filename(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn problems(report: &VerifyReport) -> Vec<Problem> {
    report
        .problems
        .iter()
        .map(|p| Problem {
            file: filename(&p.path),
            offset: p.offset,
            reason: p.reason.to_string(),
        })
        .collect()
}

pub(crate) fn verify(args: ReportArgs, out: &mut dyn Write) -> Result<()> {
    let report = inspect::verify(&args.dir)?;
    let result = Verification {
        totals: Totals::from(&report),
        problems: problems(&report),
    };
    if args.json {
        output::json(out, &result)?;
    } else {
        write_totals(out, &result.totals)?;
        write_problems(out, &result.problems)?;
    }
    finish(&report)
}

pub(crate) fn inspect(args: ReportArgs, out: &mut dyn Write) -> Result<()> {
    let report = inspect::verify(&args.dir)?;
    let checkpoint = inspect::read_checkpoint(&args.dir)?.map(|slot| Checkpoint {
        generation: slot.generation,
        next_seq: slot.next_seq,
        segment_base_seq: slot.segment_base_seq,
        offset: slot.offset,
    });
    let result = Inspection {
        segments: report
            .segments
            .iter()
            .map(|s| Segment {
                file: filename(&s.path),
                base_seq: s.info.as_ref().map(|i| i.base_seq),
                size_bytes: s.info.as_ref().map(|i| i.segment_size),
                created_unix_ns: s.info.as_ref().map(|i| i.created_unix_ns),
                created_utc: s.info.as_ref().map(|i| rfc3339(i.created_unix_ns)),
                records: s.records,
                end_seq: s.end_seq,
                used_bytes: s.used_bytes,
                used_percent: s
                    .info
                    .as_ref()
                    .map(|i| 100.0 * s.used_bytes as f64 / i.segment_size as f64),
                end_state: match s.end {
                    SegmentEnd::Sealed => "sealed",
                    SegmentEnd::Open => "open",
                    SegmentEnd::Damaged => "damaged",
                    _ => "unknown",
                },
            })
            .collect(),
        totals: Totals::from(&report),
        checkpoint,
        problems: problems(&report),
    };
    if args.json {
        output::json(out, &result)?;
    } else {
        for segment in &result.segments {
            writeln!(out, "{} base_seq={} size={} created={} records={} end_seq={} used_bytes={} used_percent={} end_state={}",
                segment.file, optional(segment.base_seq), optional(segment.size_bytes), segment.created_utc.as_deref().unwrap_or("unknown"), segment.records,
                segment.end_seq, segment.used_bytes, segment.used_percent.map(|p| format!("{p:.2}%")).unwrap_or_else(|| "unknown".into()), segment.end_state)
                .map_err(|e| output::io_error("write segment report", e))?;
        }
        write_totals(out, &result.totals)?;
        match &result.checkpoint {
            Some(c) => writeln!(
                out,
                "checkpoint: generation={} next_seq={} segment={} offset={}",
                c.generation, c.next_seq, c.segment_base_seq, c.offset
            ),
            None => writeln!(out, "checkpoint: none"),
        }
        .map_err(|e| output::io_error("write checkpoint report", e))?;
        write_problems(out, &result.problems)?;
    }
    finish(&report)
}

fn optional(value: Option<u64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "none".into())
}

fn write_totals(out: &mut dyn Write, totals: &Totals) -> Result<()> {
    writeln!(
        out,
        "segments={} records={} first_seq={} last_seq={}",
        totals.segments,
        totals.records,
        optional(totals.first_seq),
        optional(totals.last_seq)
    )
    .map_err(|e| output::io_error("write summary", e))
}

fn write_problems(out: &mut dyn Write, problems: &[Problem]) -> Result<()> {
    for p in problems {
        writeln!(
            out,
            "problem: file={} offset={} reason={}",
            p.file, p.offset, p.reason
        )
        .map_err(|e| output::io_error("write problem", e))?;
    }
    Ok(())
}

fn finish(report: &VerifyReport) -> Result<()> {
    if report.problems.is_empty() {
        Ok(())
    } else {
        Err(Failure {
            code: 2,
            message: format!("verification found {} problem(s)", report.problems.len()),
        })
    }
}

/// Gregorian calendar conversion from days since the Unix epoch, with no time-zone or locale dependence.
fn rfc3339(ns: u64) -> String {
    let seconds = ns / 1_000_000_000;
    let mut days = seconds / 86_400;
    let mut year = 1970_u64;
    let leap = |year| year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    loop {
        let length = if leap(year) { 366 } else { 365 };
        if days < length {
            break;
        }
        days -= length;
        year += 1;
    }
    let lengths = [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1;
    for length in lengths {
        if days < length {
            break;
        }
        days -= length;
        month += 1;
    }
    format!(
        "{year:04}-{month:02}-{:02}T{:02}:{:02}:{:02}.{:09}Z",
        days + 1,
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60,
        ns % 1_000_000_000
    )
}

#[cfg(test)]
mod tests {
    use super::rfc3339;

    #[test]
    fn utc_calendar_boundaries() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000000000Z");
        assert_eq!(
            rfc3339(951_782_400_123_456_789),
            "2000-02-29T00:00:00.123456789Z"
        );
        assert_eq!(
            rfc3339(4_107_542_400_000_000_000),
            "2100-03-01T00:00:00.000000000Z"
        );
        assert_eq!(rfc3339(u64::MAX), "2554-07-21T23:34:33.709551615Z");
    }
}
