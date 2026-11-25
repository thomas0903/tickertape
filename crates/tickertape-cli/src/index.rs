use std::io::Write;
use std::path::PathBuf;

use clap::{ArgGroup, Args};
use serde::Serialize;
use tickertape::seek_index::{self, IndexState, SegmentIndex};

use crate::{Failure, Result, output};

pub(crate) const HELP: &str = "Write seek indexes (files in seek-index/ of the journal that let a reader with the seek_index option start a seek inside a sealed segment; ADR 0045). Only sealed segments before the segment of the checkpoint marker are indexed, or, without a marker and without a running writer (an archive), every sealed segment. Safe beside a live writer and readers; run it from a timer or after archive, as the journal's owner. An index that is right is kept, unless --rebuild. Deleting the files is always safe (--remove).\n\nJSON schema (schema_version 1; fields may be added, see docs/cli.md): {\"schema_version\":1,\"action\":\"build\"|\"check\"|\"remove\",\"segments\":[{\"file\":string,\"base_seq\":u64,\"state\":\"built\"|\"valid\"|\"missing\"|\"ignored\"|\"wrong\"|\"not_needed\"|\"not_sealed\"|\"not_covered\"|\"not_permitted\"|\"vanished\",\"ignored_by_readers\":bool,\"reason\":string|null}],\"removed\":u64|null}. segments is empty for remove; removed is null otherwise. file is the segment's basename; reason is a human-readable diagnostic. The set of states may grow.\n\nExit codes: 0 success (also for segments that have or get no index); 1 usage or I/O error, missing journal, a writer is open and there is no checkpoint marker, the directory may not be used, or a segment is not_permitted; 2 corruption (a checkpoint marker that names a missing position) or, with --check, a wrong index. Segments built before a failure keep their indexes; diagnostics go to stderr.";

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("action").args(["rebuild", "check", "remove"])))]
pub(crate) struct IndexArgs {
    /// Journal directory (only its seek-index/ subdirectory is changed)
    dir: PathBuf,
    /// Write every index anew, also one that is right or of a newer version
    #[arg(long)]
    rebuild: bool,
    /// Change nothing: read every segment and compare every entry of every index with it
    #[arg(long)]
    check: bool,
    /// Delete every index and leave the (then empty) directory
    #[arg(long)]
    remove: bool,
    /// Emit the JSON object documented below
    #[arg(long)]
    json: bool,
}

#[derive(Serialize)]
struct Segment {
    file: String,
    base_seq: u64,
    state: &'static str,
    ignored_by_readers: bool,
    reason: Option<String>,
}

#[derive(Serialize)]
struct Index {
    action: &'static str,
    segments: Vec<Segment>,
    removed: Option<u64>,
}

fn state(state: IndexState) -> &'static str {
    match state {
        IndexState::Built => "built",
        IndexState::Valid => "valid",
        IndexState::Missing => "missing",
        IndexState::Ignored => "ignored",
        IndexState::Wrong => "wrong",
        IndexState::NotNeeded => "not_needed",
        IndexState::NotSealed => "not_sealed",
        IndexState::NotCovered => "not_covered",
        IndexState::NotPermitted => "not_permitted",
        IndexState::Vanished => "vanished",
        _ => "unknown",
    }
}

pub(crate) fn run(args: IndexArgs, out: &mut impl Write) -> Result<()> {
    let (action, segments, removed) = if args.remove {
        ("remove", Vec::new(), Some(seek_index::remove(&args.dir)?))
    } else if args.check {
        ("check", seek_index::check(&args.dir)?.segments, None)
    } else {
        let report = seek_index::build(&args.dir, args.rebuild)?;
        ("build", report.segments, None)
    };
    let result = Index {
        action,
        segments: segments
            .iter()
            .map(|s| Segment {
                file: tickertape::format::segment_file_name(s.base_seq),
                base_seq: s.base_seq,
                state: state(s.state),
                ignored_by_readers: s.ignored_by_readers,
                reason: s.reason.clone(),
            })
            .collect(),
        removed,
    };
    if args.json {
        output::json_document(out, &result)?;
    } else {
        write_text(out, &result).map_err(|e| output::io_error("write index output", e))?;
    }
    finish(&segments)
}

fn write_text(out: &mut impl Write, result: &Index) -> std::io::Result<()> {
    for segment in &result.segments {
        write!(out, "{} {}", segment.file, segment.state)?;
        if segment.ignored_by_readers {
            write!(out, " (ignored by readers)")?;
        }
        match &segment.reason {
            Some(reason) => writeln!(out, ": {reason}")?,
            None => writeln!(out)?,
        }
    }
    if let Some(removed) = result.removed {
        writeln!(out, "Removed {removed} files")?;
    }
    Ok(())
}

fn finish(segments: &[SegmentIndex]) -> Result<()> {
    let count = |state| segments.iter().filter(|s| s.state == state).count();
    let failure = |code, message| Err(Failure { code, message });
    match (count(IndexState::Wrong), count(IndexState::NotPermitted)) {
        (0, 0) => Ok(()),
        (0, n) => failure(1, format!("{n} segment(s) may not be indexed by this user")),
        (n, _) => failure(
            2,
            format!("{n} seek index(es) are wrong; run with --rebuild"),
        ),
    }
}
