//! Startup, seek, replay, recovery, verification and listing costs on journals of a given shape (roadmap: "Make
//! replay and recovery excellent at scale"). Each operation runs in a child process of its own.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use tickertape::WriterConfig;
use tickertape_bench::scale::{self, Journal, OPS};
use tickertape_bench::sustained::total_ram_bytes;
use tickertape_bench::{Options, Result};

const HELP: &str = "tt-scale: startup, seek, replay and recovery costs at scale

Usage: tt-scale build   --dir <DIR> --segments <N> [--segment-size <bytes>] [--payload <bytes>]
                        [--checkpoint-every <segments>] [--fill-last <percent>] [--json]
       tt-scale measure --dir <DIR> [--ops <list>] [--repeats <N>] [--tail-records <N>] [--cold] [--json]
       tt-scale run     <build options> <measure options>
       tt-scale --help

build generates a journal of exactly N segments (--dir must be new or empty; data is retained) with fixed-size
records: every segment but the newest full, the newest --fill-last percent full (default 50, at least one
record). --segment-size defaults to 1048576 (1 MiB; multiples of 65536), --payload to 1024 bytes, and a
checkpoint is taken every --checkpoint-every segments (default 64; 0: only at the end) and at the end.
run is build followed by measure. measure works on a journal build left (it needs the checkpoint marker at the
tail) and changes it: writer operations append records and checkpoint.

Operations (--ops takes a comma-separated subset; default all, in this order):
  list-segments               inspect::list_segments (opens every segment header)
  reader-open                 Reader::open (lists the directory, opens the oldest segment)
  seek-first|middle|last      Reader::seek to the first, middle or last record, then the first try_next
                              (the reader skips the records before the target in its segment)
  replay                      Reader::open and try_next to the end (safe reader, pread)
  verify                      inspect::verify (scans every record)
  retention-plan              retention::plan under a max_segments limit no journal reaches: the cost every
                              retention run pays before removing anything (a plan only, nothing is removed)
  writer-open-clean           Writer::open after a clean close (checkpoint at the tail)
  writer-open-after-kill      a child writer appends --tail-records records (default: half a segment) without a
                              checkpoint and is killed with SIGKILL; then Writer::open (recovery) is timed
  writer-open-without-marker  the checkpoint marker is moved aside, Writer::open recovers from the oldest
                              segment; the marker is rewritten by a checkpoint afterwards

Every operation runs --repeats times (default 1), each in a new process, so that its peak RSS is its own
(macOS: getrusage ru_maxrss; Linux: VmHWM, reset through /proc/self/clear_refs just before the operation,
because ru_maxrss carries the parent's mark across exec). Reported per run: wall time, user and system CPU
time, minor and major page faults, peak RSS (on Linux it includes the journal pages the process has mapped).
Writer operations time Writer::open only, not the checkpoint and close after it.

Page cache: with --cold the cache is dropped before every run (sudo -n; Linux: sync, then
echo 3 > /proc/sys/vm/drop_caches; macOS: purge). Each run states cache_dropped; if dropping fails, the
runs continue warm and cold_unavailable is set. Without --cold the cache holds whatever build and the earlier
runs left. Major faults show how much was read from storage. On Linux, a cgroup memory limit (memory.max) is
reported: page cache counts against it, so a limit below the journal's size gives a journal larger than the
memory the run may use. Results depend on storage and filesystem; CI runs are smoke checks, never results.";

const MEASURE_VALUES: &[&str] = &["--ops", "--repeats", "--tail-records"];
const BUILD_VALUES: &[&str] = &[
    "--segments",
    "--segment-size",
    "--payload",
    "--checkpoint-every",
    "--fill-last",
];

fn build(dir: &Path, options: &Options) -> Result<Value> {
    scale::build(
        dir,
        options.number("--segments", None)?,
        options.number("--segment-size", Some(1 << 20))?,
        options.number("--payload", Some(1024))?.try_into()?,
        options.number("--checkpoint-every", Some(64))?,
        options.number("--fill-last", Some(50))?,
    )
}

/// Runs `tt-scale <args>` and parses the JSON line it prints.
fn child(args: &[String]) -> Result<Value> {
    let output = Command::new(std::env::current_exe()?)
        .args(args)
        .stderr(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        return Err(format!("tt-scale {} failed: {}", args.join(" "), output.status).into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

/// Starts a writer child that appends `records` records without a checkpoint, waits until it has, and kills it.
fn kill_after_tail(dir: &Path, records: u64, payload: usize, segment_size: u64) -> Result<()> {
    let mut writer = Command::new(std::env::current_exe()?)
        .arg("tail")
        .arg("--dir")
        .arg(dir)
        .args(["--records", &records.to_string()])
        .args(["--payload", &payload.to_string()])
        .args(["--segment-size", &segment_size.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut line = String::new();
    let stdout = writer
        .stdout
        .take()
        .ok_or("no stdout from the tail writer")?;
    BufReader::new(stdout).read_line(&mut line)?;
    if line.trim() != "ready" {
        let _ = writer.kill();
        let status = writer.wait()?;
        return Err(format!("tail writer failed: {status}").into());
    }
    writer.kill()?; // SIGKILL
    writer.wait()?;
    Ok(())
}

fn measure(dir: &Path, options: &Options) -> Result<Value> {
    let ops: Vec<String> = match options.optional("--ops") {
        Some(list) => list.split(',').map(str::to_owned).collect(),
        None => OPS.iter().map(|s| (*s).to_owned()).collect(),
    };
    if let Some(op) = ops.iter().find(|op| !OPS.contains(&op.as_str())) {
        return Err(format!("unknown operation {op}").into());
    }
    let repeats = options.number("--repeats", Some(1))?;
    if repeats == 0 {
        return Err("repeats must be positive".into());
    }
    let cold = options.flag("--cold");
    let journal = Journal::describe(dir)?;
    let payload = scale::payload_size(dir)?;
    let capacity = tickertape_bench::segment_capacity(journal.segment_size, payload)?;
    let tail_records = options.number("--tail-records", Some(capacity / 2))?;
    let mut cold_unavailable = false;
    let mut runs = Vec::new();
    for op in &ops {
        for repeat in 0..repeats {
            if op == "writer-open-after-kill" {
                kill_after_tail(dir, tail_records, payload, journal.segment_size)?;
            }
            let dropped = cold && scale::drop_caches();
            cold_unavailable |= cold && !dropped;
            let mut args = vec![
                "op".to_owned(),
                "--dir".to_owned(),
                dir.to_string_lossy().into_owned(),
                "--op".to_owned(),
                op.clone(),
                "--segment-size".to_owned(),
                journal.segment_size.to_string(),
            ];
            let target = journal.seek_target(op);
            if let Some(seq) = target {
                args.extend(["--seq".to_owned(), seq.to_string()]);
            }
            let mut run = child(&args)?;
            run["repeat"] = json!(repeat);
            run["cache_dropped"] = json!(dropped);
            if let Some(seq) = target {
                // Records the reader skips in the target's segment before it returns the target.
                run["skipped_records"] = json!(seq - journal.segment_of(seq));
            }
            if op == "writer-open-after-kill" {
                run["tail_records"] = json!(tail_records);
            }
            runs.push(run);
        }
    }
    Ok(json!({
        "journal": journal.json(), "payload": payload, "cold_requested": cold, "cold_unavailable": cold_unavailable,
        "ram_bytes": total_ram_bytes(), "cgroup_memory_max_bytes": scale::cgroup_memory_limit(),
        "os": std::env::consts::OS, "arch": std::env::consts::ARCH, "dir": dir, "runs": runs,
    }))
}

fn print_text(result: &Value) {
    if let Some(build) = result.get("build").filter(|b| !b.is_null()) {
        println!(
            "build: {} segments x {} B, payload {} B, {} records, {:.2} s",
            build["segments"],
            build["segment_size"],
            build["payload"],
            build["records"],
            build["seconds"].as_f64().unwrap_or(0.0)
        );
    }
    let m = &result["measure"];
    println!(
        "journal: {}; ram {} B; cgroup memory.max {}; cold requested {}, unavailable {}",
        m["journal"],
        m["ram_bytes"],
        m["cgroup_memory_max_bytes"],
        m["cold_requested"],
        m["cold_unavailable"]
    );
    println!(
        "{:<28} {:>10} {:>10} {:>10} {:>9} {:>10}  extra",
        "operation", "wall ms", "user ms", "sys ms", "majflt", "rss MiB"
    );
    for run in m["runs"].as_array().into_iter().flatten() {
        let ms = |k: &str| run[k].as_f64().unwrap_or(0.0) * 1000.0;
        let extra = match run["op"].as_str() {
            Some("replay") => format!(
                "{:.0} records/s",
                run["records_per_s"].as_f64().unwrap_or(0.0)
            ),
            Some(op) if op.starts_with("seek") => format!("skipped {}", run["skipped_records"]),
            Some(op) if op.starts_with("writer") => format!(
                "scanned from {} to {}, {} actions",
                run["scanned_from_seq"], run["next_seq"], run["recovery_actions"]
            ),
            _ => String::new(),
        };
        println!(
            "{:<28} {:>10.3} {:>10.3} {:>10.3} {:>9} {:>10.1}  {extra}{}",
            run["op"].as_str().unwrap_or("?"),
            ms("wall_s"),
            ms("user_cpu_s"),
            ms("system_cpu_s"),
            run["major_faults"],
            run["peak_rss_bytes"].as_f64().unwrap_or(0.0) / (1 << 20) as f64,
            if run["cache_dropped"] == true {
                " (cold)"
            } else {
                ""
            }
        );
    }
}

fn run(args: &[String]) -> Result<()> {
    let (command, rest) = args.split_first().ok_or("missing command")?;
    match command.as_str() {
        // Internal: one timed operation in this process.
        "op" => {
            let options = Options::parse(rest, &["--dir", "--op", "--seq", "--segment-size"], &[])?;
            let seq = options.optional("--seq").map(str::parse).transpose()?;
            let segment_size =
                options.number("--segment-size", Some(WriterConfig::DEFAULT_SEGMENT_SIZE))?;
            let out = scale::run_op(
                &PathBuf::from(options.required("--dir")?),
                options.required("--op")?,
                seq,
                segment_size,
            )?;
            println!("{out}");
        }
        // Internal: append a tail without a checkpoint, say so, and wait to be killed.
        "tail" => {
            let options = Options::parse(
                rest,
                &["--dir", "--records", "--payload", "--segment-size"],
                &[],
            )?;
            let _writer = scale::write_tail(
                &PathBuf::from(options.required("--dir")?),
                options.number("--records", None)?,
                options.number("--payload", None)?.try_into()?,
                options.number("--segment-size", None)?,
            )?;
            println!("ready");
            loop {
                std::thread::park();
            }
        }
        "build" | "measure" | "run" => {
            let values: Vec<&str> = match command.as_str() {
                "build" => BUILD_VALUES.to_vec(),
                "measure" => MEASURE_VALUES.to_vec(),
                _ => [BUILD_VALUES, MEASURE_VALUES].concat(),
            };
            let values: Vec<&str> = values.into_iter().chain(["--dir"]).collect();
            let flags: &[&str] = if command == "build" {
                &["--json"]
            } else {
                &["--json", "--cold"]
            };
            let options = Options::parse(rest, &values, flags)?;
            let dir = PathBuf::from(options.required("--dir")?);
            let built = if command == "measure" {
                Value::Null
            } else {
                build(&dir, &options)?
            };
            let measured = if command == "build" {
                Value::Null
            } else {
                measure(&dir, &options)?
            };
            let result = json!({"tool": "tt-scale", "build": built, "measure": measured});
            if options.flag("--json") {
                println!("{result}");
            } else if command == "build" {
                println!("{}", result["build"]);
            } else {
                print_text(&result);
            }
        }
        other => return Err(format!("unknown command {other}").into()),
    }
    Ok(())
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP}");
        return;
    }
    if let Err(error) = run(&args) {
        eprintln!("tt-scale: {error}");
        std::process::exit(1);
    }
}
