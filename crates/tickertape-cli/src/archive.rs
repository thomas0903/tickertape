use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgGroup, Args};
use serde::Serialize;
use tickertape::retention::{self, Action, Policy, Reason};

use crate::{Result, output};

pub(crate) const HELP: &str = "Remove a sealed prefix strictly before the checkpoint segment. Without a valid checkpoint nothing is eligible. Limits combine with OR; checkpoint and newer segments are always kept. No writer lock is taken. The destination directory must exist; existing files are never replaced.\n\nJSON schema: {\"dry_run\":bool,\"segments\":[{\"file\":string,\"base_seq\":u64,\"size_bytes\":u64,\"sealed_unix_ns\":u64,\"reasons\":[\"max_segments\"|\"max_bytes\"|\"max_age\"]}],\"report\":{\"removed_segments\":u64,\"removed_bytes\":u64,\"already_removed_segments\":u64}|null}. report is null for --dry-run.\n\nExit codes: 0 success (including nothing eligible or help); 1 usage or I/O error; 2 corruption or unsupported journal format. On failure earlier removals remain in effect; diagnostics go to stderr.";

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("action").required(true).args(["to", "delete"])))]
#[command(group(ArgGroup::new("limits").required(true).multiple(true).args(["keep_segments", "max_bytes", "older_than"])))]
pub(crate) struct ArchiveArgs {
    /// Journal directory (only this command changes a journal; no writer lock)
    dir: PathBuf,
    /// Move segments to this existing directory, never replacing files
    #[arg(long, value_name = "DEST")]
    to: Option<PathBuf>,
    /// Permanently delete eligible segments
    #[arg(long)]
    delete: bool,
    /// Keep at most N total segments, subject to checkpoint eligibility
    #[arg(long, value_name = "N")]
    keep_segments: Option<usize>,
    /// Keep at most B segment bytes; optional K/M/G/T suffix multiplies by powers of 1024
    #[arg(long, value_name = "B", value_parser = parse_bytes)]
    max_bytes: Option<u64>,
    /// Remove segments sealed longer ago than DUR (integer followed by d, h, m or s)
    #[arg(long, value_name = "DUR", value_parser = parse_duration)]
    older_than: Option<Duration>,
    /// Print the plan without changing any files
    #[arg(long)]
    dry_run: bool,
    /// Emit the JSON object documented below
    #[arg(long)]
    json: bool,
}

fn scaled(value: &str, suffixes: &[(char, u64)]) -> std::result::Result<u64, String> {
    let (digits, multiplier) = match value.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => {
            let multiplier = suffixes
                .iter()
                .find(|(s, _)| c == *s)
                .map(|(_, m)| *m)
                .ok_or_else(|| "unsupported suffix".to_string())?;
            (&value[..value.len() - 1], multiplier)
        }
        _ => (value, 1),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err("expected a nonnegative integer with an optional supported suffix".into());
    }
    digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .ok_or_else(|| "value exceeds u64 range".into())
}

fn parse_bytes(value: &str) -> std::result::Result<u64, String> {
    scaled(
        value,
        &[
            ('K', 1024),
            ('M', 1024_u64.pow(2)),
            ('G', 1024_u64.pow(3)),
            ('T', 1024_u64.pow(4)),
        ],
    )
}

fn parse_duration(value: &str) -> std::result::Result<Duration, String> {
    if !value.ends_with(['d', 'h', 'm', 's']) {
        return Err("expected an integer followed by d, h, m or s".into());
    }
    scaled(value, &[('d', 86400), ('h', 3600), ('m', 60), ('s', 1)]).map(Duration::from_secs)
}

#[derive(Serialize)]
struct Segment {
    file: String,
    base_seq: u64,
    size_bytes: u64,
    sealed_unix_ns: u64,
    reasons: Vec<&'static str>,
}

#[derive(Serialize)]
struct Report {
    removed_segments: u64,
    removed_bytes: u64,
    already_removed_segments: u64,
}

#[derive(Serialize)]
struct Archive {
    dry_run: bool,
    segments: Vec<Segment>,
    report: Option<Report>,
}

pub(crate) fn run(args: ArchiveArgs, out: &mut impl Write) -> Result<()> {
    let mut policy = Policy::default();
    policy.max_segments = args.keep_segments;
    policy.max_bytes = args.max_bytes;
    policy.max_age = args.older_than;
    let plan = retention::plan(&args.dir, &policy)?;
    let segments: Vec<_> = plan
        .segments()
        .iter()
        .map(|s| Segment {
            file: s
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            base_seq: s.base_seq,
            size_bytes: s.size_bytes,
            sealed_unix_ns: s.sealed_unix_ns,
            reasons: s
                .reasons
                .iter()
                .map(|reason| match reason {
                    Reason::MaxSegments => "max_segments",
                    Reason::MaxBytes => "max_bytes",
                    Reason::MaxAge => "max_age",
                    _ => "unknown",
                })
                .collect(),
        })
        .collect();
    let report = if args.dry_run {
        None
    } else {
        let action = args.to.map_or(Action::Delete, Action::MoveTo);
        let report = retention::apply(&args.dir, &plan, &action)?;
        Some(Report {
            removed_segments: report.removed_segments,
            removed_bytes: report.removed_bytes,
            already_removed_segments: report.already_removed_segments,
        })
    };
    let result = Archive {
        dry_run: args.dry_run,
        segments,
        report,
    };
    if args.json {
        output::json(out, &result)?;
    } else {
        for segment in &result.segments {
            writeln!(
                out,
                "{} {} bytes ({})",
                segment.file,
                segment.size_bytes,
                segment.reasons.join(", ")
            )
            .map_err(|e| output::io_error("write archive output", e))?;
        }
        if let Some(report) = result.report {
            writeln!(
                out,
                "Removed {} segments ({} bytes); {} already removed",
                report.removed_segments, report.removed_bytes, report.already_removed_segments
            )
            .map_err(|e| output::io_error("write archive output", e))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_durations_and_overflow() {
        assert_eq!(parse_bytes("64K").unwrap(), 65536);
        assert_eq!(parse_bytes("2M").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_bytes("1G").unwrap(), 1024_u64.pow(3));
        assert_eq!(parse_bytes("1T").unwrap(), 1024_u64.pow(4));
        assert_eq!(parse_bytes("0").unwrap(), 0);
        for bad in ["", "K", "1KB", "-1", "+1", "1.5M", "18446744073709551615K"] {
            assert!(parse_bytes(bad).is_err(), "{bad}");
        }
        for (value, seconds) in [
            ("30d", 2592000),
            ("12h", 43200),
            ("90m", 5400),
            ("3600s", 3600),
            ("0s", 0),
        ] {
            assert_eq!(parse_duration(value).unwrap(), Duration::from_secs(seconds));
        }
        for bad in ["1", "1w", "-1s", "0.1s", "18446744073709551615d"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }
}
