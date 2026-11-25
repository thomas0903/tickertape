//! Startup, seek, replay, recovery, verification and listing costs on journals of a given shape (roadmap: "Make
//! replay and recovery excellent at scale"). Each operation runs in a child process of its own.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use tickertape::{RecoveryPolicy, WriterConfig};
use tickertape_bench::scale::damage::{self, Base, Shape};
use tickertape_bench::scale::{
    self, Journal, OPS, OPTIONAL_OPS, Op, READER_OPS, ReaderKind, SEEK_OPS, catch_up,
};
use tickertape_bench::sustained::{WaitMode, total_ram_bytes};
use tickertape_bench::{Options, Result};

const HELP: &str = "tt-scale: startup, seek, replay and recovery costs at scale

Usage: tt-scale build    --dir <DIR> --segments <N> [--segment-size <bytes>] [--payload <bytes>]
                         [--checkpoint-every <segments>] [--fill-last <percent>] [--json]
       tt-scale measure  --dir <DIR> [--ops <list>] [--readers <list>] [--repeats <N>] [--tail-records <N>]
                         [--count <N>] [--end <seq>] [--cli <program>] [--max-open-files <N>] [--label <text>]
                         [--cold] [--sample-open-files] [--allow-errors] [--json]
       tt-scale run      <build options> <measure options>
       tt-scale archive  --dir <DIR> --to <DIR> [--keep-segments <N>] [--cold] [--json]
       tt-scale damage   --dir <DIR> [--segments <N>] [--segment-size <bytes>] [--payload <bytes>]
                         [--cases <list>] [--cold] [--json]
       tt-scale catch-up --dir <DIR> --reader safe|mapped --wait spin|block --rate <records/s>
                         --backlog <records> [--warmup <D>] [--baseline <D>] [--live <D>] [--limit <D>]
                         [--checkpoint-every <D>] [--writer-cpu <N> --reader-cpu <N>] [--cold] [--json]
       tt-scale --help

build generates a journal of exactly N segments (--dir must be new or empty; data is retained) with fixed-size
records: every segment but the newest full, the newest --fill-last percent full (default 50, at least one
record). --segment-size defaults to 1048576 (1 MiB; multiples of 65536), --payload to 1024 bytes, and a
checkpoint is taken every --checkpoint-every segments (default 64; 0: only at the end) and at the end.
run is build followed by measure. measure works on a journal build left (it needs the checkpoint marker at the
tail, or --end, the sequence number after the last record, for a directory without one, such as an archive) and
changes it: writer operations append records and checkpoint.

Operations (--ops takes a comma-separated subset; default all but the optional ones, in this order; `seeks`
stands for reader-open and every seek):
  list-directory              reads the journal directory
  list-segments               inspect::list_segments (opens every segment header)
  reader-open                 Reader::open (lists the directory, opens the oldest segment)
  seek-first|middle|last      Reader::seek to the first, middle or last record, then the first try_next
                              (the reader skips the records before the target in its segment)
  seek-segment-end            the same to the last record of a full segment in the middle of the journal: the
                              longest scan a seek costs
  seek-forward                seeks to that segment's first record, then through it in 16 equal forward steps,
                              reading one record after each (steps_s, steps_total_s)
  seek-to-end                 Reader::seek_to_end and one try_next
  replay                      opens a reader and calls try_next to the end (or to --end)
  status                      status::status, one sample
  verify                      inspect::verify (scans every record)
  retention-plan              retention::plan under a max_segments limit no journal reaches: the cost every
                              retention run pays before removing anything (a plan only, nothing is removed)
  writer-open-clean           Writer::open after a clean close (checkpoint at the tail)
  writer-open-after-kill      a child writer appends --tail-records records (default: half a segment) without a
                              checkpoint and is killed with SIGKILL; then Writer::open (recovery) is timed
  writer-open-without-marker  the checkpoint marker is moved aside, Writer::open recovers from the oldest
                              segment; the marker is rewritten by a checkpoint afterwards (checkpoint_after_s)
  checkpoint-loop             --count (default 100) blocking checkpoints, one record before each; the writer's
                              open is not timed (writer_open_s)
  checkpoint-loop-retention   the same with a retention limit no journal reaches, so that every checkpoint also
                              makes a retention plan
Optional operations:
  retention-delete            plans and deletes the --count oldest segments (shrinks the journal)
  cli-verify|export|stats     the tickertape program given by --cli, as a process: verify, export --format jsonl
                              to a pipe nobody reads (bounded by --end), stats

Reader operations run once per reader in --readers (default safe; safe,mapped for both). Every operation runs
--repeats times (default 1), each in a new process, so that its peak RSS is its own (macOS: getrusage ru_maxrss;
Linux: VmHWM, reset through /proc/self/clear_refs just before the operation, because ru_maxrss carries the
parent's mark across exec). Reported per run: wall time, user and system CPU time, minor and major page faults,
bytes read from and written to storage (Linux, /proc/self/io), peak RSS (on Linux it includes the journal pages
the process has mapped), and the limit on open files. Writer operations time Writer::open only, not the
checkpoint and close after it, and report the files the open writer holds. --sample-open-files also counts open
files about every millisecond while an operation runs (not for timings); --max-open-files lowers the limit.
An operation that fails is reported with its error; the exit status is then 1, unless --allow-errors is given.

Page cache: with --cold the cache is dropped before every run (sudo -n; Linux: sync, then
echo 3 > /proc/sys/vm/drop_caches; macOS: purge). Each run states cache_dropped; if dropping fails, the
runs continue warm and cold_unavailable is set. Without --cold the cache holds whatever build and the earlier
runs left. Major faults show how much was read from storage. On Linux, a cgroup memory limit (memory.max) is
reported: page cache counts against it, so a limit below the journal's size gives a journal larger than the
memory the run may use. Results depend on storage and filesystem; CI runs are smoke checks, never results.

archive moves all but the newest --keep-segments (default: half) segments of the journal to a new directory, with
a reader of each kind open in the oldest one, and reports the move's cost and what the readers, seeks across the
boundary, verify on both directories and the rejoined journal do. Then: measure --dir <archive> --end <seq>.

damage builds intact journals in --dir (new or empty) and, for every case (missing and corrupt segments), reports
what both readers, seeks, seek_to_end, verify, status and Writer::open under each recovery policy do, how long
they take, and where every committed record is afterwards: in a segment, in quarantine/, or in neither. --cases
takes a comma-separated subset. Defaults: 64 segments of 1048576 bytes, 1024-byte payloads.

catch-up opens a writer on a journal build left and appends --rate records/s; after --warmup (default 2 s) and
--baseline (default 10 s) a reader starts --backlog records behind the journal's head, reads to the head and
follows for --live (default 10 s). Reported: the time to catch up, the reader's rate and cost, and the writer's
append latency in each phase (the call alone, and from the intended time, which counts a delayed schedule). The
writer requests a background checkpoint every --checkpoint-every (default 1s; 0s: none). The run appends to the
journal and leaves it checkpointed.";

const MEASURE_VALUES: &[&str] = &[
    "--ops",
    "--readers",
    "--repeats",
    "--tail-records",
    "--count",
    "--end",
    "--cli",
    "--max-open-files",
    "--label",
];
const MEASURE_FLAGS: &[&str] = &["--cold", "--sample-open-files", "--allow-errors"];
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

/// The operations `--ops` names.
fn operations(list: Option<&str>) -> Result<Vec<&'static str>> {
    let Some(list) = list else {
        return Ok(OPS.to_vec());
    };
    let mut ops = Vec::new();
    for name in list.split(',') {
        if name == "seeks" {
            ops.extend(SEEK_OPS);
            continue;
        }
        let known = OPS.iter().chain(OPTIONAL_OPS).find(|op| **op == name);
        ops.push(*known.ok_or_else(|| format!("unknown operation {name}"))?);
    }
    Ok(ops)
}

fn measure(dir: &Path, options: &Options) -> Result<Value> {
    let ops = operations(options.optional("--ops"))?;
    let readers = options
        .optional("--readers")
        .unwrap_or("safe")
        .split(',')
        .map(ReaderKind::parse)
        .collect::<Result<Vec<_>>>()?;
    let repeats = options.number("--repeats", Some(1))?;
    if repeats == 0 {
        return Err("repeats must be positive".into());
    }
    let cold = options.flag("--cold");
    let end = options
        .optional("--end")
        .map(str::parse::<u64>)
        .transpose()?;
    let journal = Journal::describe(dir, end)?;
    let payload = scale::payload_size(dir)?;
    let capacity = tickertape_bench::segment_capacity(journal.segment_size, payload)?;
    let tail_records = options.number("--tail-records", Some(capacity / 2))?;
    let mut cold_unavailable = false;
    let mut runs = Vec::new();
    let mut errors = 0;
    for op in &ops {
        let readers = if READER_OPS.contains(op) {
            &readers[..]
        } else {
            &[ReaderKind::Safe]
        };
        for (repeat, reader) in (0..repeats).flat_map(|r| readers.iter().map(move |k| (r, *k))) {
            if *op == "writer-open-after-kill" {
                kill_after_tail(dir, tail_records, payload, journal.segment_size)?;
            }
            let dropped = cold && scale::drop_caches();
            cold_unavailable |= cold && !dropped;
            let mut args: Vec<String> = vec![
                "op".to_owned(),
                "--dir".to_owned(),
                dir.to_string_lossy().into_owned(),
                "--op".to_owned(),
                (*op).to_owned(),
                "--reader".to_owned(),
                reader.name().to_owned(),
                "--segment-size".to_owned(),
                journal.segment_size.to_string(),
                "--count".to_owned(),
                options.number("--count", Some(100))?.to_string(),
            ];
            let mut pass = |key: &str, value: Option<String>| {
                if let Some(value) = value {
                    args.extend([key.to_owned(), value]);
                }
            };
            let target = journal.seek_target(op);
            pass("--seq", target.map(|s| s.to_string()));
            pass(
                "--span-end",
                (*op == "seek-forward").then(|| journal.segment_end(target).to_string()),
            );
            pass("--end", end.map(|e| e.to_string()));
            pass("--cli", options.optional("--cli").map(str::to_owned));
            pass(
                "--max-open-files",
                options.optional("--max-open-files").map(str::to_owned),
            );
            if options.flag("--sample-open-files") {
                args.push("--sample-open-files".to_owned());
            }
            let mut run = child(&args)?;
            run["repeat"] = json!(repeat);
            run["cache_dropped"] = json!(dropped);
            if let Some(seq) = target {
                // Records the reader skips in the target's segment before it returns the target.
                run["skipped_records"] = json!(seq - journal.segment_of(seq));
            }
            if *op == "writer-open-after-kill" {
                run["tail_records"] = json!(tail_records);
            }
            if let Some(error) = run.get("error") {
                errors += 1;
                eprintln!("tt-scale: {op} ({}): {error}", reader.name());
            }
            runs.push(run);
        }
    }
    Ok(json!({
        "journal": journal.json(), "payload": payload, "cold_requested": cold, "cold_unavailable": cold_unavailable,
        "ram_bytes": total_ram_bytes(), "cgroup_memory_max_bytes": scale::cgroup_memory_limit(),
        "os": std::env::consts::OS, "arch": std::env::consts::ARCH, "dir": dir, "label": options.optional("--label"),
        "end": end, "core_1_1": cfg!(feature = "core-1-1"), "errors": errors, "runs": runs,
    }))
}

/// `tt-scale archive`.
fn archive(options: &Options) -> Result<Value> {
    let dir = PathBuf::from(options.required("--dir")?);
    let to = PathBuf::from(options.required("--to")?);
    let segments = Journal::describe(&dir, None)?.bases.len() as u64;
    let keep = options.number("--keep-segments", Some(segments / 2))?;
    let mut dropped = false;
    let mut out = scale::archive::run(&dir, &to, &scale::archive::joined_dir(&to), || {
        dropped = options.flag("--cold") && scale::drop_caches();
        child(&[
            "archive-move".to_owned(),
            "--dir".to_owned(),
            dir.to_string_lossy().into_owned(),
            "--to".to_owned(),
            to.to_string_lossy().into_owned(),
            "--keep-segments".to_owned(),
            keep.to_string(),
        ])
    })?;
    out["cache_dropped"] = json!(dropped);
    out["segments"] = json!(segments);
    out["keep_segments"] = json!(keep);
    out["ram_bytes"] = json!(total_ram_bytes());
    Ok(out)
}

fn shape(options: &Options) -> Result<Shape> {
    Ok(Shape {
        segments: options.number("--segments", Some(64))?,
        segment_size: options.number("--segment-size", Some(1 << 20))?,
        payload: options.number("--payload", Some(1024))?.try_into()?,
    })
}

/// `tt-scale damage`.
fn damage(options: &Options) -> Result<Value> {
    let root = PathBuf::from(options.required("--dir")?);
    let shape = shape(options)?;
    let cold = options.flag("--cold");
    let cases = match options.optional("--cases") {
        None => damage::CASES.to_vec(),
        Some(list) => list.split(',').map(damage::find).collect::<Result<_>>()?,
    };
    tickertape_bench::fresh_directory(&root)?;
    let base_dir = |base: Base| {
        root.join(match base {
            Base::MarkerInNewest => "marker-in-newest",
            Base::MarkerTwoBack => "marker-two-back",
        })
    };
    let mut fingerprints = Vec::new();
    for base in [Base::MarkerInNewest, Base::MarkerTwoBack] {
        if cases.iter().any(|c| c.base == base) {
            damage::build_base(&base_dir(base), shape, base)?;
            fingerprints.push((base, damage::Fingerprint::of(&base_dir(base))?));
        }
    }
    let dir = root.join("case");
    let probe = |args: &[(&str, String)], cold: bool| -> Result<Value> {
        let dropped = cold && scale::drop_caches();
        let mut all = vec!["probe".to_owned()];
        all.extend(args.iter().flat_map(|(k, v)| [(*k).to_owned(), v.clone()]));
        all.extend([
            "--dir".to_owned(),
            dir.to_string_lossy().into_owned(),
            "--segments".to_owned(),
            shape.segments.to_string(),
            "--segment-size".to_owned(),
            shape.segment_size.to_string(),
            "--payload".to_owned(),
            shape.payload.to_string(),
        ]);
        let mut out = child(&all)?;
        out["cache_dropped"] = json!(dropped);
        Ok(out)
    };
    let mut results = Vec::new();
    for case in cases {
        let base = base_dir(case.base);
        damage::prepare(&base, &dir, shape, case)?;
        let before = damage::census(&base, &dir, shape)?;
        let mut probes = Vec::new();
        for name in damage::READ_PROBES {
            for kind in [ReaderKind::Safe, ReaderKind::Mapped] {
                let mut args = vec![
                    ("--probe", (*name).to_owned()),
                    ("--reader", kind.name().to_owned()),
                ];
                if let Some(seq) = damage::seek_target(name, shape, case)? {
                    args.push(("--seq", seq.to_string()));
                }
                probes.push(probe(&args, cold)?);
            }
        }
        for name in ["verify", "status"] {
            probes.push(probe(&[("--probe", name.to_owned())], cold)?);
        }
        let mut writers = Vec::new();
        for policy in [RecoveryPolicy::Quarantine, RecoveryPolicy::Fail] {
            damage::prepare(&base, &dir, shape, case)?;
            let name = damage::policy_name(policy);
            let mut out = probe(&[("--probe", format!("writer-open-{name}"))], cold)?;
            let after = damage::census(&base, &dir, shape)?;
            out["census"] = after.json();
            out["gone"] = json!(
                before
                    .in_segments
                    .saturating_sub(after.in_segments + after.quarantined)
            );
            // What the journal looks like to a reader and to `verify` afterwards (never with a dropped cache).
            out["replay_after"] = probe(
                &[
                    ("--probe", "replay".to_owned()),
                    ("--reader", "safe".to_owned()),
                ],
                false,
            )?;
            out["verify_after"] = probe(&[("--probe", "verify".to_owned())], false)?;
            writers.push(out);
        }
        for (kind, fingerprint) in &fingerprints {
            fingerprint.check(&base_dir(*kind))?;
        }
        results.push(
            json!({"case": case.name, "census_before": before.json(), "probes": probes,
            "writers": writers}),
        );
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(
        json!({"tool": "tt-scale damage", "segments": shape.segments, "segment_size": shape.segment_size,
        "payload": shape.payload, "records": shape.records()?, "cold_requested": cold,
        "ram_bytes": total_ram_bytes(), "core_1_1": cfg!(feature = "core-1-1"), "cases": results}),
    )
}

fn print_damage(result: &Value) {
    for case in result["cases"].as_array().into_iter().flatten() {
        println!("{}: {}", case["case"], case["census_before"]);
        let rows = [&case["probes"], &case["writers"]];
        for row in rows.iter().flat_map(|r| r.as_array().into_iter().flatten()) {
            println!(
                "  {:<24} {:<7} {:>10.3} ms  {}{}",
                row["probe"].as_str().unwrap_or("?"),
                row["reader"].as_str().unwrap_or(""),
                row["wall_s"].as_f64().unwrap_or(0.0) * 1000.0,
                row["outcome"].as_str().unwrap_or("?"),
                row.get("census").map_or_else(String::new, |census| format!(
                    "; gone {}; {census}; actions {}; then replay {} records ({}), verify {} problems",
                    row["gone"],
                    row["actions"],
                    row["replay_after"]["records"],
                    row["replay_after"]["outcome"],
                    row["verify_after"]["problem_count"]
                ))
            );
        }
    }
}

/// `tt-scale catch-up`.
fn catch_up(options: &Options) -> Result<Value> {
    let duration = |key: &str, default: &str| {
        tickertape_bench::sustained::parse_duration(options.optional(key).unwrap_or(default))
    };
    let cpu = |key: &str| options.optional(key).map(str::parse::<u64>).transpose();
    let config = catch_up::Config {
        dir: PathBuf::from(options.required("--dir")?),
        reader: ReaderKind::parse(options.required("--reader")?)?,
        wait: WaitMode::parse(options.required("--wait")?)?,
        rate: options.number("--rate", None)?,
        backlog: options.number("--backlog", None)?,
        warmup: duration("--warmup", "2s")?,
        baseline: duration("--baseline", "10s")?,
        live: duration("--live", "10s")?,
        limit: duration("--limit", "900s")?,
        checkpoint_every: duration("--checkpoint-every", "1s")?,
        cpus: match (cpu("--writer-cpu")?, cpu("--reader-cpu")?) {
            (Some(writer), Some(reader)) => Some((writer, reader)),
            (None, None) => None,
            _ => return Err("give both --writer-cpu and --reader-cpu, or neither".into()),
        },
        cold: options.flag("--cold"),
    };
    catch_up::run(&config, |from| {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("catch-up-reader")
            .arg("--dir")
            .arg(&config.dir)
            .args(["--reader", config.reader.name()])
            .args(["--wait", config.wait.name()])
            .args(["--from", &from.to_string()]);
        if let Some((_, reader)) = config.cpus {
            command.args(["--reader-cpu", &reader.to_string()]);
        }
        Ok(command)
    })
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
        "{:<28} {:<7} {:>10} {:>10} {:>10} {:>9} {:>10}  extra",
        "operation", "reader", "wall ms", "user ms", "sys ms", "majflt", "rss MiB"
    );
    for run in m["runs"].as_array().into_iter().flatten() {
        let ms = |k: &str| run[k].as_f64().unwrap_or(0.0) * 1000.0;
        let extra = match run["op"].as_str() {
            _ if run.get("error").is_some() => format!("ERROR {}", run["error"]),
            Some("seek-forward") => format!(
                "16 forward seeks {:.3} ms",
                run["steps_total_s"].as_f64().unwrap_or(0.0) * 1000.0
            ),
            Some(op) if op.starts_with("checkpoint") => format!("{}", run["checkpoint_latency"]),
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
            "{:<28} {:<7} {:>10.3} {:>10.3} {:>10.3} {:>9} {:>10.1}  {extra}{}",
            run["op"].as_str().unwrap_or("?"),
            run["reader"].as_str().unwrap_or(""),
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
            let options = Options::parse(
                rest,
                &[
                    "--dir",
                    "--op",
                    "--reader",
                    "--seq",
                    "--span-end",
                    "--end",
                    "--segment-size",
                    "--count",
                    "--cli",
                    "--max-open-files",
                ],
                &["--sample-open-files"],
            )?;
            if options.optional("--max-open-files").is_some() {
                scale::limit_open_files(options.number("--max-open-files", None)?)?;
            }
            let number = |key: &str| options.optional(key).map(str::parse::<u64>).transpose();
            let out = scale::run_op(&Op {
                dir: PathBuf::from(options.required("--dir")?),
                name: options.required("--op")?.to_owned(),
                reader: ReaderKind::parse(options.required("--reader")?)?,
                seq: number("--seq")?,
                span_end: number("--span-end")?,
                end: number("--end")?,
                segment_size: options
                    .number("--segment-size", Some(WriterConfig::DEFAULT_SEGMENT_SIZE))?,
                count: options.number("--count", Some(100))?,
                cli: options.path("--cli"),
                sample_files: options.flag("--sample-open-files"),
            })?;
            println!("{out}");
        }
        // Internal: the move of `archive`.
        "archive-move" => {
            let options = Options::parse(rest, &["--dir", "--to", "--keep-segments"], &[])?;
            let out = scale::archive::move_segments(
                &PathBuf::from(options.required("--dir")?),
                &PathBuf::from(options.required("--to")?),
                options.number("--keep-segments", None)?.try_into()?,
            )?;
            println!("{out}");
        }
        // Internal: one probe of a damaged journal.
        "probe" => {
            let options = Options::parse(
                rest,
                &[
                    "--dir",
                    "--probe",
                    "--reader",
                    "--seq",
                    "--segments",
                    "--segment-size",
                    "--payload",
                ],
                &[],
            )?;
            let dir = PathBuf::from(options.required("--dir")?);
            let out = match options.required("--probe")? {
                name @ ("verify" | "status") => damage::check_probe(&dir, name)?,
                "writer-open-quarantine" => {
                    damage::writer_probe(&dir, RecoveryPolicy::Quarantine, shape(&options)?)?
                }
                "writer-open-fail" => {
                    damage::writer_probe(&dir, RecoveryPolicy::Fail, shape(&options)?)?
                }
                name => damage::read_probe(
                    &dir,
                    name,
                    ReaderKind::parse(options.required("--reader")?)?,
                    options.optional("--seq").map(str::parse).transpose()?,
                )?,
            };
            println!("{out}");
        }
        // Internal: the reader of `catch-up`.
        "catch-up-reader" => {
            let options = Options::parse(
                rest,
                &["--dir", "--reader", "--wait", "--from", "--reader-cpu"],
                &[],
            )?;
            catch_up::reader(
                &PathBuf::from(options.required("--dir")?),
                ReaderKind::parse(options.required("--reader")?)?,
                WaitMode::parse(options.required("--wait")?)?,
                options.number("--from", None)?,
                options
                    .optional("--reader-cpu")
                    .map(str::parse)
                    .transpose()?,
            )?;
        }
        "archive" => {
            let options = Options::parse(
                rest,
                &["--dir", "--to", "--keep-segments"],
                &["--cold", "--json"],
            )?;
            println!("{}", archive(&options)?);
        }
        "damage" => {
            let options = Options::parse(
                rest,
                &[
                    "--dir",
                    "--segments",
                    "--segment-size",
                    "--payload",
                    "--cases",
                ],
                &["--cold", "--json"],
            )?;
            let result = damage(&options)?;
            if options.flag("--json") {
                println!("{result}");
            } else {
                print_damage(&result);
            }
        }
        "catch-up" => {
            let options = Options::parse(
                rest,
                &[
                    "--dir",
                    "--reader",
                    "--wait",
                    "--rate",
                    "--backlog",
                    "--warmup",
                    "--baseline",
                    "--live",
                    "--limit",
                    "--checkpoint-every",
                    "--writer-cpu",
                    "--reader-cpu",
                ],
                &["--cold", "--json"],
            )?;
            println!("{}", catch_up(&options)?);
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
            let mut flags = vec!["--json"];
            if command != "build" {
                flags.extend(MEASURE_FLAGS);
            }
            let options = Options::parse(rest, &values, &flags)?;
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
            if result["measure"]["errors"].as_u64().unwrap_or(0) > 0
                && !options.flag("--allow-errors")
            {
                return Err("an operation failed".into());
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
