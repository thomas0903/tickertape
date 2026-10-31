//! Read-only command-line interface to tickertape journals.

mod export;
mod output;
mod parquet_export;
mod report;
mod tail;

use std::fmt;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};

const EXIT_CODES: &str = "Exit codes: 0 success (including help, end of input or idle timeout); 1 usage, I/O or missing journal; 2 corruption or unsupported journal format.";
const RECORD_SCHEMA: &str = "Record JSONL schema (keys in order): {\"seq\":u64,\"schema_id\":u32,\"venue_ts_ns\":u64,\"local_ts_ns\":u64,\"payload_b64\":string}. payload_b64 is standard base64 with padding. One object per line.\n\nExit codes: 0 success (including help, end of input or idle timeout); 1 usage, I/O or missing journal; 2 corruption or unsupported journal format.";

#[derive(Debug, Parser)]
#[command(name = "tickertape", version, about = "Read-only tools for tickertape event journals", after_help = EXIT_CODES)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Follow committed records, starting at the oldest record by default
    #[command(after_help = RECORD_SCHEMA)]
    Tail(TailArgs),
    /// Inspect segments, totals, checkpoint and every verification problem
    #[command(after_help = report::INSPECT_HELP)]
    Inspect(ReportArgs),
    /// Check every segment header, record checksum, seal and chain link
    #[command(after_help = report::VERIFY_HELP)]
    Verify(ReportArgs),
    /// Export records through the committed end observed in a startup scan
    #[command(after_help = export::help())]
    Export(ExportArgs),
}

#[derive(Debug, Args)]
struct TailArgs {
    /// Journal directory (opened read-only; no writer lock)
    dir: PathBuf,
    /// Start at this sequence number, inclusive
    #[arg(long, value_name = "SEQ", conflicts_with = "from_end")]
    from: Option<u64>,
    /// Start after the last record observed in a startup scan
    #[arg(long)]
    from_end: bool,
    /// Stop after N records (zero writes nothing)
    #[arg(long, value_name = "N")]
    count: Option<u64>,
    /// Stop at the current committed end instead of waiting for new records
    #[arg(long)]
    no_follow: bool,
    /// Return only checkpointed records; wait for coverage (no valid checkpoint means no records)
    #[arg(long)]
    durable_only: bool,
    /// Text: metadata and first 32 payload bytes as hex; json: record JSONL
    #[arg(long, value_enum, default_value = "text")]
    format: TailFormat,
    /// Decode a known payload schema: adds a summary to text output and a "decoded" object to JSON (as in export)
    #[arg(long, value_enum)]
    decode: Option<Decoder>,
    /// Stop after S idle seconds (nonnegative decimal; reset after each record)
    #[arg(long, value_name = "S", value_parser = parse_timeout)]
    timeout_secs: Option<Duration>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TailFormat {
    Text,
    Json,
}

#[derive(Debug, Args)]
struct ReportArgs {
    /// Journal directory (opened read-only; no writer lock)
    dir: PathBuf,
    /// Emit one JSON report instead of human-readable text (schema below)
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ExportArgs {
    /// Journal directory (opened read-only; no writer lock)
    dir: PathBuf,
    /// Output format: JSONL with base64 payloads, or Snappy Parquet
    #[arg(long, value_enum)]
    format: ExportFormat,
    /// Output file outside journal (created/truncated); JSONL defaults to -; Parquet requires a file
    #[arg(long, value_name = "FILE", default_value = "-")]
    output: PathBuf,
    /// Decode a known payload schema into additional fields (both formats)
    #[arg(long, value_enum)]
    decode: Option<Decoder>,
    /// First sequence number to export, inclusive
    #[arg(long, value_name = "SEQ")]
    from: Option<u64>,
    /// Last sequence number to export, inclusive
    #[arg(long, value_name = "SEQ")]
    to: Option<u64>,
    /// Export only records below the checkpoint's next_seq captured at startup (both formats; no valid checkpoint means no records)
    #[arg(long)]
    durable_only: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ExportFormat {
    Jsonl,
    Parquet,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Decoder {
    MarketDemo,
}

impl Decoder {
    fn decode(
        self,
        record: &tickertape::Record<'_>,
    ) -> Option<tickertape_cli::market_demo::Decoded> {
        match self {
            Self::MarketDemo => {
                tickertape_cli::market_demo::decode(record.schema_id, record.payload)
            }
        }
    }
}

#[derive(Debug)]
struct Failure {
    code: u8,
    message: String,
}

type Result<T> = std::result::Result<T, Failure>;

impl Failure {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
        }
    }
}

impl From<tickertape::Error> for Failure {
    fn from(error: tickertape::Error) -> Self {
        let code = if matches!(
            error,
            tickertape::Error::Corrupt { .. } | tickertape::Error::UnsupportedVersion { .. }
        ) {
            2
        } else {
            1
        };
        Self {
            code,
            message: error.to_string(),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

fn parse_timeout(value: &str) -> std::result::Result<Duration, String> {
    let seconds: f64 = value
        .parse()
        .map_err(|_| "expected nonnegative seconds".to_string())?;
    let duration = Duration::try_from_secs_f64(seconds)
        .map_err(|_| "expected finite nonnegative seconds".to_string())?;
    // Reader::next_timeout adds the duration to Instant; reject user input that could overflow it.
    if std::time::Instant::now().checked_add(duration).is_none() {
        return Err("timeout exceeds the platform's clock range".into());
    }
    Ok(duration)
}

fn run(cli: Cli) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match cli.command {
        Command::Tail(args) => tail::run(args, &mut out)?,
        Command::Inspect(args) => report::inspect(args, &mut out)?,
        Command::Verify(args) => report::verify(args, &mut out)?,
        Command::Export(args) => export::run(args, &mut out)?,
    }
    out.flush().map_err(|e| output::io_error("flush stdout", e))
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = u8::from(error.use_stderr());
            if error.print().is_err() {
                return ExitCode::from(1);
            }
            return ExitCode::from(code);
        }
    };
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "tickertape: {error}");
            ExitCode::from(error.code)
        }
    }
}
