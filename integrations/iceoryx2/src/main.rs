//! `tickertape-iox2`: record iceoryx2 services into a tickertape journal, replay them, convert to and from
//! iceoryx2 recording files, and summarize a recording.

// The only `unsafe` here is the one call that passes the user's trust in a recording on to
// `Replayer::run_dynamic` (see CONTRIBUTING.md and the `replay` help).
#![deny(unsafe_code)]

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use tickertape_iceoryx2::Error;
use tickertape_iceoryx2::dynamic::DynamicService;
use tickertape_iceoryx2::layout::{CounterField, CounterLocation};
use tickertape_iceoryx2::recorder::Recorder;
use tickertape_iceoryx2::replayer::{Remap, Replayer};
use tickertape_iceoryx2::upstream::{self, ExportOptions, ImportOptions};

const EXIT_CODES: &str = "Exit codes: 0 success (including help); 1 usage, I/O, iceoryx2 or missing journal; 2 corruption, unsupported journal format or a malformed record. The first SIGINT or SIGTERM stops cleanly; one more, taken after it, exits at once with 130 or 143 (two of the same kind sent back to back can be merged into one by the operating system).";

#[derive(Debug, Parser)]
#[command(
    name = "tickertape-iox2",
    version,
    about = "Record iceoryx2 publish/subscribe services into a tickertape journal, replay them, and convert to and from iceoryx2 recording files",
    after_help = EXIT_CODES
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Record one or more services into a journal until SIGINT, SIGTERM or a limit (changes the journal)
    #[command(after_help = RECORD_HELP)]
    Record(RecordArgs),
    /// Publish recorded messages onto iceoryx2 services, renamed with a prefix by default (replay only recordings you trust)
    #[command(after_help = REPLAY_HELP)]
    Replay(ReplayArgs),
    /// Write one service's messages to a new iceoryx2 recording file (for iox2 service replay)
    #[command(after_help = EXPORT_HELP)]
    Export(ExportArgs),
    /// Append a human-readable iceoryx2 recording file (from iox2 service record) to a journal (changes the journal; import only files you trust)
    #[command(after_help = IMPORT_HELP)]
    Import(ImportArgs),
    /// Summarize recording runs, services, message counts, time ranges and reported losses
    #[command(after_help = INFO_HELP)]
    Info(InfoArgs),
}

const RECORD_HELP: &str = "Services are opened by name with the types their publishers created them with (this uses iceoryx2 API that upstream marks internal; see the crate docs). A service that doesn't exist yet, fails to open (types or settings outside the limits in the crate README, other types than expected) or fails while receiving is reported on stderr and retried every 100 ms; the other services keep recording. A full volume, or a message larger than a journal segment, stops the recorder (after a final checkpoint). Every --checkpoint-interval-ms, and once more on SIGINT/SIGTERM, recorded messages are made durable; a checkpoint pauses recording, so messages arriving meanwhile must fit in the subscriber buffer. The subscriber buffer is the smaller of --buffer-size and the service's subscriber_max_buffer_size (iceoryx2's default is 2); messages that overflow it are lost before the recorder sees them. Use --counter to detect that. Every segment holds the descriptors of the records in it: the descriptors of all open services start each new segment, after a filler record that ends the previous one when needed (see the crate README).\n\nWith --json, the final report is one JSON object: {\"run_start_seq\":u64,\"next_seq\":u64,\"messages\":u64,\"checkpoints\":u64,\"fillers\":u64,\"stop_reason\":\"signal\"|\"stop_flag\"|\"max_messages\"|\"max_duration\",\"services\":[{\"name\":string,\"descriptor_seq\":u64|null,\"descriptors\":u64,\"messages\":u64,\"payload_bytes\":u64,\"buffer_size\":u64|null,\"subscriber_max_buffer_size\":u64|null,\"safe_overflow\":bool|null,\"gaps\":u64,\"missing\":u64,\"backwards\":u64,\"errors\":u64,\"last_error\":string|null}]}.\n\nExit codes: 0 success (including help); 1 usage, I/O, iceoryx2 or missing journal; 2 corruption, unsupported journal format or a malformed record. The first SIGINT or SIGTERM stops cleanly; one more, taken after it, exits at once with 130 or 143 (two of the same kind sent back to back can be merged into one by the operating system).";

const REPLAY_HELP: &str = concat!(
    "Each recorded service is replayed onto PREFIX + name (default prefix replay/), so a replay can't feed a recorder of the original service; --original-names replays onto the recorded names. While a writer (a recorder or an import) holds the journal, a target name that any descriptor in the journal names is refused unless --allow-live-targets is given (the replayer reads the whole journal once to learn them; a service the recorder is still waiting for has no descriptor yet and isn't known), and the replay ends at the journal's committed end when it started, so it makes one pass. Target services are created with the recorded types and settings, or opened if they exist; one whose settings in force would make its publisher allocate more than the limits in the crate README is refused, and so is an existing service with other types, and so are two recorded descriptors with different types for one target (a type change between recording runs, or two --map rules onto one name). Paced replay (--speed, from 0.000001 to 1000000000) schedules each message from the first replayed one by the recorded receive times divided by the speed; gaps between recording runs and forward steps of the recording clock are waited out. Records whose descriptor was removed by retention are skipped and counted.\n\n",
    "Replay only recordings you trust, as with upstream's own replay tool: the recorded bytes are published as values of the recorded types without being checked to be valid values (a subscriber of a type such as bool or an enum has undefined behaviour if they are not). Equal type names, sizes and alignments, all that iceoryx2 compares, don't establish that a subscriber's type has the recording publisher's native representation (field order, byte order, a type changed under the same name, another architecture). tickertape's checksums detect accidental damage, not a journal or file written with wrong contents. Sizes, alignments, element counts and service settings are validated.",
    "\n\nWith --json, the report is one JSON object: {\"first_seq\":u64|null,\"last_seq\":u64|null,\"messages\":u64,\"filtered\":u64,\"undescribed\":u64,\"other_records\":u64,\"services\":[{\"recorded\":string,\"target\":string,\"messages\":u64}],\"pacing\":null|{\"speed\":f64,\"late_threshold_ns\":u64,\"sends\":u64,\"late_sends\":u64,\"max_late_ns\":u64,\"max_late_seq\":u64|null,\"max_late_scheduled_ns\":u64,\"max_late_actual_ns\":u64,\"mean_late_ns\":u64,\"last_scheduled_ns\":u64,\"last_actual_ns\":u64},\"end\":\"end_of_journal\"|\"to_reached\"|\"signal\"|\"stop_flag\"}.\n\nExit codes: 0 success (including help); 1 usage, I/O, iceoryx2 or missing journal; 2 corruption, unsupported journal format or a malformed record. The first SIGINT or SIGTERM stops cleanly; one more, taken after it, exits at once with 130 or 143 (two of the same kind sent back to back can be merged into one by the operating system)."
);

const EXPORT_HELP: &str = "An iceoryx2 recording file holds one service, with timestamps in whole milliseconds since its first message. Lost on export: absolute time, sub-millisecond precision, recording-run boundaries, sequence numbers, publisher ids, venue_ts_ns, loss records, service settings; the system header is written as zeros. Backward timestamps are raised to the previous one (counted as clamped). Records whose descriptor was removed by retention are skipped (counted as undescribed). Only the human-readable representation is written (iox2dump export was removed: upstream's writer put uninitialised padding into its header and didn't check for short writes). The output file must not exist; it is written completely and synced under a temporary name of its own, and appears only when complete.\n\nWith --json: {\"service\":string,\"messages\":u64,\"first_seq\":u64|null,\"last_seq\":u64|null,\"runs\":u64,\"clamped\":u64,\"undescribed\":u64}.\n\nExit codes: 0 success (including help); 1 usage, I/O, iceoryx2 or missing journal; 2 corruption, unsupported journal format or a malformed record.";

const IMPORT_HELP: &str = concat!(
    "Reads a human-readable iceoryx2 recording file (iox2-dump files can't be imported). The whole file is read into memory and validated first (file format version 1, publish-subscribe, service and type names valid for iceoryx2, type details within the limits in the crate README, every record's lengths, non-decreasing timestamps whose absolute times fit, every line ending with a newline as upstream writes them, so no truncated record); a file that fails leaves the journal untouched. Its messages become one new recording run (its descriptor, flagged as imported, is repeated in every segment the run fills). Its times are relative, so local_ts_ns is --start-unix-ns (default: now) plus each message's offset. System headers (and with them publisher ids) and service settings are not imported.\n\nImport only files you trust: a replay of the journal publishes the imported bytes. ",
    "Replay only recordings you trust, as with upstream's own replay tool: the recorded bytes are published as values of the recorded types without being checked to be valid values (a subscriber of a type such as bool or an enum has undefined behaviour if they are not). Equal type names, sizes and alignments, all that iceoryx2 compares, don't establish that a subscriber's type has the recording publisher's native representation (field order, byte order, a type changed under the same name, another architecture). tickertape's checksums detect accidental damage, not a journal or file written with wrong contents. Sizes, alignments, element counts and service settings are validated.",
    "\n\nWith --json: {\"service\":string,\"iceoryx2_version\":string,\"descriptor_seq\":u64,\"messages\":u64,\"start_unix_ns\":u64,\"next_seq\":u64}.\n\nExit codes: 0 success (including help); 1 usage, I/O, iceoryx2, missing journal or an invalid file; 2 corruption, unsupported journal format or a malformed record."
);

const INFO_HELP: &str = "Reads from the first record to the committed end observed when the reader gets there (no writer lock). A recording run is one run of the recorder or one import, identified by run_start_seq; every segment holds the descriptors of the records in it. Filler records (zeros that end a segment early) are counted apart from data. Records whose descriptor was removed by retention are counted as undescribed.\n\nWith --json: {\"first_seq\":u64|null,\"last_seq\":u64|null,\"records\":u64,\"other_records\":u64,\"filler_records\":u64,\"filler_bytes\":u64,\"undescribed_records\":u64,\"runs\":[{\"run_start_seq\":u64,\"start_unix_ns\":u64,\"recorder\":string,\"iceoryx2_version\":string,\"imported\":bool,\"services\":[string],\"descriptors\":u64}],\"services\":[{\"name\":string,\"payload_type\":string,\"user_header_type\":string,\"runs\":u64,\"descriptors\":u64,\"messages\":u64,\"payload_bytes\":u64,\"first_seq\":u64|null,\"last_seq\":u64|null,\"first_local_ts_ns\":u64|null,\"last_local_ts_ns\":u64|null,\"loss_records\":u64,\"missing_messages\":u64}]}.\n\nExit codes: 0 success (including help); 1 usage, I/O or missing journal; 2 corruption, unsupported journal format or a malformed record.";

#[derive(Debug, Args)]
struct RecordArgs {
    /// Journal directory (created if missing; continued if it exists)
    journal: PathBuf,
    /// iceoryx2 service to record (repeat for several)
    #[arg(long = "service", value_name = "NAME", required = true)]
    services: Vec<String>,
    /// Checkpoint interval in milliseconds
    #[arg(long, value_name = "MS", default_value_t = 1000)]
    checkpoint_interval_ms: u64,
    /// WaitSet interval in microseconds: the longest a message waits when no event wakes the recorder
    #[arg(long, value_name = "US", default_value_t = 1000)]
    poll_interval_us: u64,
    /// Subscriber buffer to ask for (capped by the service's subscriber_max_buffer_size; at most 65536)
    #[arg(long, value_name = "N", default_value_t = 1024)]
    buffer_size: usize,
    /// Publisher-side counter to check for lost messages, in every service: header|payload:OFFSET:WIDTH (width 4 or 8, little-endian)
    #[arg(long, value_name = "SPEC", value_parser = parse_counter)]
    counter: Option<CounterField>,
    /// Read venue_ts_ns as a little-endian u64 at this byte offset of every service's user header
    #[arg(long, value_name = "OFFSET")]
    venue_ts_offset: Option<u32>,
    /// Also wake up on notifications of this iceoryx2 event service (created if missing)
    #[arg(long, value_name = "NAME")]
    notify_service: Option<String>,
    /// Stop after N messages (all services)
    #[arg(long, value_name = "N")]
    max_messages: Option<u64>,
    /// Stop after S seconds
    #[arg(long, value_name = "S", value_parser = parse_seconds)]
    max_seconds: Option<Duration>,
    /// Size of new journal segment files in bytes (a multiple of 65536); recovery after a crash zeroes the unused
    /// rest of the active segment, so smaller segments restart faster
    #[arg(long, value_name = "BYTES", default_value_t = tickertape::WriterConfig::DEFAULT_SEGMENT_SIZE)]
    segment_size: u64,
    /// iceoryx2 node name
    #[arg(long, value_name = "NAME", default_value = "tickertape-recorder")]
    node_name: String,
    /// Print the final report as one JSON object (schema below)
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ReplayArgs {
    /// Journal directory (opened read-only; no writer lock)
    journal: PathBuf,
    /// Start at this sequence number, inclusive
    #[arg(long, value_name = "SEQ")]
    from: Option<u64>,
    /// Stop after this sequence number, inclusive
    #[arg(long, value_name = "SEQ")]
    to: Option<u64>,
    /// Replay at F times the recorded pace (1 = recorded pace, 2 = twice as fast; 0.000001 to 1000000000); default: as fast as possible
    #[arg(long, value_name = "F", conflicts_with = "as_fast_as_possible")]
    speed: Option<f64>,
    /// Replay as fast as possible (the default)
    #[arg(long)]
    as_fast_as_possible: bool,
    /// Prefix for replayed service names
    #[arg(long, value_name = "PREFIX", default_value = tickertape_iceoryx2::replayer::DEFAULT_PREFIX, conflicts_with = "original_names")]
    prefix: String,
    /// Replay onto the recorded service names (a recorder of those services would record the replay)
    #[arg(long)]
    original_names: bool,
    /// Replay the recorded service FROM onto TO (repeatable; overrides the prefix)
    #[arg(long = "map", value_name = "FROM=TO", value_parser = parse_map)]
    maps: Vec<(String, String)>,
    /// Allow target names the journal is recording right now (the replay would be recorded into the journal)
    #[arg(long)]
    allow_live_targets: bool,
    /// Replay only this recorded service (repeat for several; default: all)
    #[arg(long = "service", value_name = "NAME")]
    services: Vec<String>,
    /// Before the first message to each service, wait for at least N subscribers
    #[arg(long, value_name = "N")]
    wait_subscribers: Option<usize>,
    /// How long --wait-subscribers waits, in seconds
    #[arg(long, value_name = "S", default_value = "10", value_parser = parse_seconds)]
    wait_timeout_secs: Duration,
    /// Keep the publishers open this many milliseconds after the last message, so subscribers can take what is queued
    #[arg(long, value_name = "MS", default_value_t = 500)]
    linger_ms: u64,
    /// Spin instead of sleeping for the last this-many microseconds before each paced send
    #[arg(long, value_name = "US", default_value_t = 1000)]
    spin_us: u64,
    /// Paced sends later than this many microseconds count as late
    #[arg(long, value_name = "US", default_value_t = 1000)]
    late_threshold_us: u64,
    /// iceoryx2 node name
    #[arg(long, value_name = "NAME", default_value = "tickertape-replayer")]
    node_name: String,
    /// Print the report as one JSON object (schema below)
    #[arg(long)]
    json: bool,
}

/// The file representation: only human-readable (iox2dump was removed, ADR 0030, review round 3).
#[derive(Debug, Clone, Copy, ValueEnum)]
enum Repr {
    HumanReadable,
}

#[derive(Debug, Args)]
struct ExportArgs {
    /// Journal directory (opened read-only; no writer lock)
    journal: PathBuf,
    /// Output file (must not exist)
    #[arg(long, value_name = "FILE")]
    output: PathBuf,
    /// Service to export (required if the range holds several)
    #[arg(long, value_name = "NAME")]
    service: Option<String>,
    /// First sequence number, inclusive
    #[arg(long, value_name = "SEQ")]
    from: Option<u64>,
    /// Last sequence number, inclusive
    #[arg(long, value_name = "SEQ")]
    to: Option<u64>,
    /// File representation (as iox2 service record --data-representation): human-readable only
    #[arg(long, value_enum, default_value = "human-readable")]
    representation: Repr,
    /// Print the report as one JSON object (schema below)
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ImportArgs {
    /// Human-readable iceoryx2 recording file
    input: PathBuf,
    /// Journal directory (created if missing)
    journal: PathBuf,
    /// Start time to add the file's relative timestamps to, in nanoseconds since the Unix epoch (default: now)
    #[arg(long, value_name = "NS")]
    start_unix_ns: Option<u64>,
    /// Print the report as one JSON object (schema below)
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct InfoArgs {
    /// Journal directory (opened read-only; no writer lock)
    journal: PathBuf,
    /// Only records covered by a checkpoint
    #[arg(long)]
    durable_only: bool,
    /// Print one JSON object (schema below)
    #[arg(long)]
    json: bool,
}

fn parse_counter(value: &str) -> Result<CounterField, String> {
    let usage = || "expected header|payload:OFFSET:WIDTH with WIDTH 4 or 8".to_string();
    let mut parts = value.split(':');
    let location = match parts.next() {
        Some("header") => CounterLocation::UserHeader,
        Some("payload") => CounterLocation::Payload,
        _ => return Err(usage()),
    };
    let offset = parts
        .next()
        .and_then(|v| v.parse().ok())
        .ok_or_else(usage)?;
    let width = parts
        .next()
        .and_then(|v| v.parse().ok())
        .ok_or_else(usage)?;
    if parts.next().is_some() || !(width == 4 || width == 8) {
        return Err(usage());
    }
    Ok(CounterField {
        location,
        offset,
        width,
    })
}

fn parse_map(value: &str) -> Result<(String, String), String> {
    match value.split_once('=') {
        Some((from, to)) if !from.is_empty() && !to.is_empty() => Ok((from.into(), to.into())),
        _ => Err("expected FROM=TO".into()),
    }
}

fn parse_seconds(value: &str) -> Result<Duration, String> {
    let seconds: f64 = value
        .parse()
        .map_err(|_| "expected nonnegative seconds".to_string())?;
    Duration::try_from_secs_f64(seconds)
        .map_err(|_| "expected finite nonnegative seconds".to_string())
}

fn json(out: &mut dyn Write, value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    writeln!(out)
}

fn ns_to_text(ns: Option<u64>) -> String {
    match ns {
        None | Some(0) => "-".into(),
        Some(ns) => format!("{}.{:09}", ns / 1_000_000_000, ns % 1_000_000_000),
    }
}

fn record(args: RecordArgs, out: &mut dyn Write) -> Result<(), Error> {
    let mut builder = Recorder::builder(&args.journal)
        .writer_config(tickertape::WriterConfig::default().segment_size(args.segment_size))
        .checkpoint_interval(Duration::from_millis(args.checkpoint_interval_ms))
        .poll_interval(Duration::from_micros(args.poll_interval_us))
        .buffer_size(args.buffer_size)
        .node_name(&args.node_name)
        .on_start({
            let journal = args.journal.display().to_string();
            move |seq| eprintln!("tickertape-iox2: recording into {journal} from sequence {seq}")
        })
        .on_open(|name, seq| {
            eprintln!("tickertape-iox2: recording \"{name}\" (descriptor at sequence {seq})")
        })
        .on_error(|name, error| {
            eprintln!("tickertape-iox2: service \"{name}\": {error} (retrying every 100 ms)")
        });
    for name in &args.services {
        let mut service = DynamicService::new(name);
        if let Some(c) = args.counter {
            service = service.counter(c);
        }
        if let Some(o) = args.venue_ts_offset {
            service = service.venue_ts_offset(o);
        }
        if let Some(n) = &args.notify_service {
            service = service.notify_service(n);
        }
        builder = builder.dynamic(service);
    }
    if let Some(n) = args.max_messages {
        builder = builder.max_messages(n);
    }
    if let Some(d) = args.max_seconds {
        builder = builder.max_duration(d);
    }
    let report = builder.build()?.run()?;
    let io = |e| Error::Io {
        op: "write report",
        path: "stdout".into(),
        source: e,
    };
    if args.json {
        return json(out, &report).map_err(io);
    }
    (|| -> io::Result<()> {
        writeln!(
            out,
            "recording run {} stopped ({:?}): {} messages, sequence numbers {}..{}, {} checkpoints",
            report.run_start_seq,
            report.stop_reason,
            report.messages,
            report.run_start_seq,
            report.next_seq,
            report.checkpoints
        )?;
        for s in &report.services {
            write!(
                out,
                "  {}: {} messages, {} payload bytes",
                s.name, s.messages, s.payload_bytes
            )?;
            match (s.buffer_size, s.subscriber_max_buffer_size, s.safe_overflow) {
                (Some(b), Some(m), Some(o)) => write!(
                    out,
                    ", buffer {b} (service max {m}, {})",
                    if o {
                        "safe overflow: oldest dropped when full"
                    } else {
                        "no safe overflow"
                    }
                )?,
                _ => write!(out, ", never opened")?,
            }
            if s.gaps + s.backwards > 0 {
                write!(
                    out,
                    ", {} gaps ({} missing), {} backwards",
                    s.gaps, s.missing, s.backwards
                )?;
            }
            if let Some(e) = &s.last_error {
                write!(out, ", {} errors (last: {e})", s.errors)?;
            }
            writeln!(out)?;
        }
        Ok(())
    })()
    .map_err(io)
}

fn replay(args: ReplayArgs, out: &mut dyn Write) -> Result<(), Error> {
    let mut remap = if args.original_names {
        Remap::default().original_names()
    } else {
        Remap::default().prefix(&args.prefix)
    };
    for (from, to) in &args.maps {
        remap = remap.rule(from, to);
    }
    let mut builder = Replayer::builder(&args.journal)
        .remap(remap)
        .late_threshold(Duration::from_micros(args.late_threshold_us))
        .linger(Duration::from_millis(args.linger_ms))
        .spin(Duration::from_micros(args.spin_us))
        .node_name(&args.node_name)
        .on_start({
            let journal = args.journal.display().to_string();
            move || eprintln!("tickertape-iox2: replaying {journal}")
        });
    if let Some(s) = args.from {
        builder = builder.from(s);
    }
    if let Some(s) = args.to {
        builder = builder.to(s);
    }
    if let Some(s) = args.speed {
        builder = builder.speed(s);
    }
    if !args.services.is_empty() {
        builder = builder.services(args.services.clone());
    }
    if let Some(n) = args.wait_subscribers {
        builder = builder.wait_for_subscribers(n, args.wait_timeout_secs);
    }
    builder = builder.allow_live_targets(args.allow_live_targets);
    let replayer = builder.build()?;
    // SAFETY: Replaying a recording publishes its bytes as values of the recorded types. Running `replay` is the
    // user's statement that they trust the recording; the help, the README and the crate docs say so ("replay only
    // recordings you trust", as with upstream's `iox2 service replay`). Everything else (sizes, alignments, element
    // counts, settings) is validated by the library.
    #[allow(unsafe_code)]
    let report = unsafe { replayer.run_dynamic() }?;
    let io = |e| Error::Io {
        op: "write report",
        path: "stdout".into(),
        source: e,
    };
    if args.json {
        return json(out, &report).map_err(io);
    }
    (|| -> io::Result<()> {
        writeln!(out, "replayed {} messages ({:?})", report.messages, report.end)?;
        if report.undescribed > 0 {
            writeln!(
                out,
                "  skipped {} records whose descriptor is no longer in the journal",
                report.undescribed
            )?;
        }
        for s in &report.services {
            writeln!(out, "  {} -> {}: {} messages", s.recorded, s.target, s.messages)?;
        }
        if let Some(p) = &report.pacing {
            writeln!(
                out,
                "  pacing at {}x: {} of {} sends later than {} us; latest {} us late (seq {}, scheduled {} ms, sent {} ms); mean {} us; last scheduled {} ms, sent {} ms",
                p.speed,
                p.late_sends,
                p.sends,
                p.late_threshold_ns / 1000,
                p.max_late_ns / 1000,
                p.max_late_seq.map_or("-".into(), |s| s.to_string()),
                p.max_late_scheduled_ns / 1_000_000,
                p.max_late_actual_ns / 1_000_000,
                p.mean_late_ns / 1000,
                p.last_scheduled_ns / 1_000_000,
                p.last_actual_ns / 1_000_000,
            )?;
        }
        Ok(())
    })()
    .map_err(io)
}

fn export(args: ExportArgs, out: &mut dyn Write) -> Result<(), Error> {
    let Repr::HumanReadable = args.representation;
    let mut options = ExportOptions::default();
    if let Some(s) = &args.service {
        options = options.service(s);
    }
    if let Some(s) = args.from {
        options = options.from(s);
    }
    if let Some(s) = args.to {
        options = options.to(s);
    }
    let report = upstream::export(&args.journal, &args.output, &options)?;
    let io = |e| Error::Io {
        op: "write report",
        path: "stdout".into(),
        source: e,
    };
    if args.json {
        return json(out, &report).map_err(io);
    }
    writeln!(
        out,
        "exported {} messages of \"{}\" from {} recording run(s) to {} ({} timestamps clamped, {} records skipped without a descriptor)",
        report.messages,
        report.service,
        report.runs,
        args.output.display(),
        report.clamped,
        report.undescribed
    )
    .map_err(io)
}

fn import(args: ImportArgs, out: &mut dyn Write) -> Result<(), Error> {
    let mut options = ImportOptions::default();
    if let Some(ns) = args.start_unix_ns {
        options = options.start_unix_ns(ns);
    }
    let report = upstream::import(&args.input, &args.journal, &options)?;
    let io = |e| Error::Io {
        op: "write report",
        path: "stdout".into(),
        source: e,
    };
    if args.json {
        return json(out, &report).map_err(io);
    }
    writeln!(
        out,
        "imported {} messages of \"{}\" (iceoryx2 {}) at sequence numbers {}..{}",
        report.messages,
        report.service,
        report.iceoryx2_version,
        report.descriptor_seq,
        report.next_seq
    )
    .map_err(io)
}

fn info(args: InfoArgs, out: &mut dyn Write) -> Result<(), Error> {
    let info = tickertape_iceoryx2::info::scan(&args.journal, args.durable_only)?;
    let io = |e| Error::Io {
        op: "write report",
        path: "stdout".into(),
        source: e,
    };
    if args.json {
        return json(out, &info).map_err(io);
    }
    (|| -> io::Result<()> {
        writeln!(
            out,
            "records: {} (sequence numbers {}..={}), {} fillers ({} bytes), {} of other schemas, {} without their descriptor (removed)",
            info.records,
            info.first_seq.map_or("-".into(), |s| s.to_string()),
            info.last_seq.map_or("-".into(), |s| s.to_string()),
            info.filler_records,
            info.filler_bytes,
            info.other_records,
            info.undescribed_records
        )?;
        writeln!(out, "recording runs:")?;
        for s in &info.runs {
            writeln!(
                out,
                "  {}  start {}  {}{}  iceoryx2 {}  services: {}",
                s.run_start_seq,
                ns_to_text(Some(s.start_unix_ns)),
                s.recorder,
                if s.imported { " (imported)" } else { "" },
                s.iceoryx2_version,
                s.services.join(", ")
            )?;
        }
        writeln!(out, "services:")?;
        for s in &info.services {
            writeln!(
                out,
                "  {}  payload {}  header {}  runs {}  messages {}  bytes {}  seq {}..={}  time {}..{}  losses {} ({} missing)",
                s.name,
                s.payload_type,
                s.user_header_type,
                s.runs,
                s.messages,
                s.payload_bytes,
                s.first_seq.map_or("-".into(), |v| v.to_string()),
                s.last_seq.map_or("-".into(), |v| v.to_string()),
                ns_to_text(s.first_local_ts_ns),
                ns_to_text(s.last_local_ts_ns),
                s.loss_records,
                s.missing_messages
            )?;
        }
        Ok(())
    })()
    .map_err(io)
}

fn main() -> ExitCode {
    // iceoryx2 logs a warning when no config file exists, which is the normal case; IOX2_LOG_LEVEL overrides this.
    iceoryx2::prelude::set_log_level_from_env_or(iceoryx2::prelude::LogLevel::Error);
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
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let result = match cli.command {
        Command::Record(args) => record(args, &mut out),
        Command::Replay(args) => replay(args, &mut out),
        Command::Export(args) => export(args, &mut out),
        Command::Import(args) => import(args, &mut out),
        Command::Info(args) => info(args, &mut out),
    };
    let _ = out.flush();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "tickertape-iox2: {error}");
            ExitCode::from(if error.is_data_error() { 2 } else { 1 })
        }
    }
}
