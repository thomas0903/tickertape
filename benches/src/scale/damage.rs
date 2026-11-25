//! `tt-scale damage`: missing and corrupt segments at scale. For every case: what readers, `verify`, `status` and the
//! writer's recovery (both policies) do, how long it takes, and where every committed record is afterwards.
//!
//! Each case is a copy of an intact journal (hard links to the segments no case changes, so that a case on a large
//! journal costs little), damaged, and then probed by one process per operation. A writer probe changes the journal,
//! so each gets a fresh copy.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tickertape::{
    ReaderConfig, RecoveryAction, RecoveryPolicy, Writer, WriterConfig, format, inspect,
};

use super::archive::error_text;
use super::{ReaderKind, build, record, timed};
use crate::sustained::record_bytes;
use crate::{Result, segment_capacity};

/// What an intact journal a case starts from looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base {
    /// The checkpoint marker is in the newest segment: a quarter of it is covered, the next quarter is not.
    MarkerInNewest,
    /// The marker is a quarter into the third-newest segment; everything after it is uncovered.
    MarkerTwoBack,
}

/// How a case damages the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Damage {
    None,
    Missing(Segment),
    /// A payload byte of one record is flipped.
    Record(Segment, At),
    /// The commit word of one record is zeroed.
    EmptyWord(Segment, At),
    /// A byte of the segment header is flipped.
    Header(Segment),
}

/// Which segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Segment {
    Oldest,
    Middle,
    /// The one before the newest.
    SecondNewest,
    Newest,
}

/// Which record of the segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum At {
    /// The middle record of a full segment.
    Middle,
    /// An eighth into the segment (before the marker of `Base::MarkerInNewest`).
    Eighth,
    /// Three eighths into the segment (after that marker, before the last record).
    ThreeEighths,
}

/// One row of the table.
#[derive(Debug, Clone, Copy)]
pub struct Case {
    /// Its name.
    pub name: &'static str,
    /// The journal it starts from.
    pub base: Base,
    damage: Damage,
    /// The checkpoint marker file is removed too.
    without_marker: bool,
}

const fn case(name: &'static str, base: Base, damage: Damage) -> Case {
    Case {
        name,
        base,
        damage,
        without_marker: false,
    }
}

use Base::{MarkerInNewest as In, MarkerTwoBack as Back};

/// Every case, in the order `damage` runs them.
pub const CASES: &[Case] = &[
    case("intact", In, Damage::None),
    case("missing-oldest", In, Damage::Missing(Segment::Oldest)),
    case("missing-middle", In, Damage::Missing(Segment::Middle)),
    case("missing-newest", In, Damage::Missing(Segment::Newest)),
    case(
        "record-middle-segment",
        In,
        Damage::Record(Segment::Middle, At::Middle),
    ),
    case(
        "record-newest-before-marker",
        In,
        Damage::Record(Segment::Newest, At::Eighth),
    ),
    case(
        "record-newest-after-marker",
        In,
        Damage::Record(Segment::Newest, At::ThreeEighths),
    ),
    case(
        "empty-word-middle-segment",
        In,
        Damage::EmptyWord(Segment::Middle, At::Middle),
    ),
    case(
        "empty-word-newest-after-marker",
        In,
        Damage::EmptyWord(Segment::Newest, At::ThreeEighths),
    ),
    case("header-oldest", In, Damage::Header(Segment::Oldest)),
    case("header-middle", In, Damage::Header(Segment::Middle)),
    case("header-newest", In, Damage::Header(Segment::Newest)),
    Case {
        name: "no-marker-intact",
        base: In,
        damage: Damage::None,
        without_marker: true,
    },
    Case {
        name: "no-marker-record-middle-segment",
        base: In,
        damage: Damage::Record(Segment::Middle, At::Middle),
        without_marker: true,
    },
    Case {
        name: "no-marker-missing-middle",
        base: In,
        damage: Damage::Missing(Segment::Middle),
        without_marker: true,
    },
    case("uncovered-intact", Back, Damage::None),
    case(
        "uncovered-missing-newest",
        Back,
        Damage::Missing(Segment::Newest),
    ),
    case(
        "uncovered-missing-second-newest",
        Back,
        Damage::Missing(Segment::SecondNewest),
    ),
    case(
        "uncovered-record-second-newest",
        Back,
        Damage::Record(Segment::SecondNewest, At::Middle),
    ),
    case(
        "uncovered-header-second-newest",
        Back,
        Damage::Header(Segment::SecondNewest),
    ),
    case(
        "uncovered-header-newest",
        Back,
        Damage::Header(Segment::Newest),
    ),
];

/// The case called `name`.
pub fn find(name: &str) -> Result<Case> {
    CASES
        .iter()
        .find(|c| c.name == name)
        .copied()
        .ok_or_else(|| format!("unknown case {name}").into())
}

/// The read operations probed for every case, per reader (`verify` and `status` use no reader).
pub const READ_PROBES: &[&str] = &[
    "open",
    "replay",
    "seek-first",
    "seek-middle",
    "seek-after-damage",
    "seek-last",
    "seek-to-end",
];

/// The shape of a base journal.
#[derive(Debug, Clone, Copy)]
pub struct Shape {
    /// Segments in the journal.
    pub segments: u64,
    /// Size of each.
    pub segment_size: u64,
    /// Payload bytes of each record.
    pub payload: usize,
}

impl Shape {
    fn capacity(&self) -> Result<u64> {
        segment_capacity(self.segment_size, self.payload)
    }

    /// Records in the journal: every segment full, the newest half full.
    pub fn records(&self) -> Result<u64> {
        let capacity = self.capacity()?;
        Ok((self.segments - 1) * capacity + capacity / 2)
    }

    fn index(&self, segment: Segment) -> u64 {
        match segment {
            Segment::Oldest => 0,
            Segment::Middle => self.segments / 2,
            Segment::SecondNewest => self.segments - 2,
            Segment::Newest => self.segments - 1,
        }
    }

    fn file_of(&self, index: u64) -> Result<String> {
        Ok(format::segment_file_name(index * self.capacity()?))
    }

    /// File offset of the `i`th record of a segment.
    fn offset_of(&self, i: u64) -> Result<u64> {
        Ok(format::SEGMENT_HEADER_LEN as u64 + i * record_bytes(self.payload)?)
    }
}

/// Builds the intact journal of `shape` for `base` in `dir`.
pub fn build_base(dir: &Path, shape: Shape, base: Base) -> Result<()> {
    if shape.segments < 8 {
        return Err("damage needs at least 8 segments".into());
    }
    let capacity = shape.capacity()?;
    if capacity < 16 {
        return Err("damage needs at least 16 records per segment".into());
    }
    let covered_segments = match base {
        Base::MarkerInNewest => shape.segments,
        Base::MarkerTwoBack => shape.segments - 2,
    };
    let built = build(
        dir,
        covered_segments,
        shape.segment_size,
        shape.payload,
        64,
        25,
    )?;
    let covered = built["records"]
        .as_u64()
        .ok_or("build reported no records")?;
    // Not checkpointed; dropping the writer does not checkpoint either.
    drop(super::write_tail(
        dir,
        shape.records()? - covered,
        shape.payload,
        shape.segment_size,
    )?);
    Ok(())
}

/// File identity and state of every segment of a base journal, to tell a hard link nobody changed.
#[derive(Debug)]
pub struct Fingerprint(BTreeMap<String, (u64, i64, i64, u64)>);

fn stamp(meta: &fs::Metadata) -> (u64, i64, i64, u64) {
    (meta.ino(), meta.mtime(), meta.mtime_nsec(), meta.len())
}

impl Fingerprint {
    /// Of the segments in `dir`.
    pub fn of(dir: &Path) -> Result<Fingerprint> {
        let mut out = BTreeMap::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".tape") {
                out.insert(name, stamp(&entry.metadata()?));
            }
        }
        Ok(Fingerprint(out))
    }

    /// Fails if a segment of the base journal changed: a case must not have changed a file it shares with it.
    pub fn check(&self, dir: &Path) -> Result<()> {
        if Fingerprint::of(dir)?.0 != self.0 {
            return Err("a segment of the base journal changed while a case ran".into());
        }
        Ok(())
    }
}

/// Makes the journal of `case` in `dir` from the intact one in `base`: hard links, except for the segments the case
/// damages, their neighbours, and the newest four (which recovery may write to), which are copied.
pub fn prepare(base: &Path, dir: &Path, shape: Shape, case: Case) -> Result<()> {
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    fs::create_dir(dir)?;
    let target = match case.damage {
        Damage::None => None,
        Damage::Missing(s) | Damage::Record(s, _) | Damage::EmptyWord(s, _) | Damage::Header(s) => {
            Some(shape.index(s))
        }
    };
    for index in 0..shape.segments {
        let name = shape.file_of(index)?;
        let near = target.is_some_and(|t| index.abs_diff(t) <= 1);
        if near || index + 4 >= shape.segments {
            fs::copy(base.join(&name), dir.join(&name))?;
        } else {
            fs::hard_link(base.join(&name), dir.join(&name))?;
        }
    }
    if !case.without_marker {
        fs::copy(base.join("checkpoint"), dir.join("checkpoint"))?;
    }
    let capacity = shape.capacity()?;
    let at = |at| match at {
        At::Middle => capacity / 2,
        At::Eighth => capacity / 8,
        At::ThreeEighths => capacity * 3 / 8,
    };
    match case.damage {
        Damage::None => {}
        Damage::Missing(s) => fs::remove_file(dir.join(shape.file_of(shape.index(s))?))?,
        Damage::Record(s, i) => flip(
            &dir.join(shape.file_of(shape.index(s))?),
            shape.offset_of(at(i))? + record_bytes(shape.payload)? / 2,
        )?,
        Damage::EmptyWord(s, i) => {
            let file = OpenOptions::new()
                .write(true)
                .open(dir.join(shape.file_of(shape.index(s))?))?;
            file.write_all_at(&[0; 4], shape.offset_of(at(i))?)?;
            file.sync_all()?;
        }
        // In the base sequence number of the header.
        Damage::Header(s) => flip(&dir.join(shape.file_of(shape.index(s))?), 16)?,
    }
    Ok(())
}

fn flip(path: &Path, offset: u64) -> Result<()> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    let mut byte = [0];
    file.read_exact_at(&mut byte, offset)?;
    byte[0] ^= 0xff;
    file.write_all_at(&byte, offset)?;
    file.sync_all()?;
    Ok(())
}

/// The sequence number `probe` seeks to in the journal of `case`.
pub fn seek_target(probe: &str, shape: Shape, case: Case) -> Result<Option<u64>> {
    let capacity = shape.capacity()?;
    let records = shape.records()?;
    let damaged = match case.damage {
        Damage::None => shape.segments / 2,
        Damage::Missing(s) | Damage::Record(s, _) | Damage::EmptyWord(s, _) | Damage::Header(s) => {
            shape.index(s)
        }
    };
    Ok(match probe {
        "seek-first" => Some(0),
        "seek-middle" => Some(records / 2),
        // The first record of the segment after the damaged one; with none after it, the last record.
        "seek-after-damage" => Some(((damaged + 1) * capacity).min(records - 1)),
        "seek-last" => Some(records - 1),
        _ => None,
    })
}

/// Runs one read probe in this process: its cost, and what it returned (`outcome`: `ok`, `end` or the error).
pub fn read_probe(dir: &Path, probe: &str, kind: ReaderKind, seq: Option<u64>) -> Result<Value> {
    let mut out = timed(|| {
        Ok(match probe {
            "open" => match kind.open(dir, ReaderConfig::default()) {
                Ok(r) => json!({"outcome": "ok", "position": r.position()}),
                Err(e) => json!({"outcome": error_text(&e)}),
            },
            "replay" => replay(dir, kind),
            "seek-to-end" => seek_to_end(dir, kind),
            _ => {
                let seq = seq.ok_or("a seek probe needs --seq")?;
                match kind.open(dir, ReaderConfig::default()) {
                    Err(e) => json!({"outcome": error_text(&e), "at": "open"}),
                    Ok(mut reader) => match reader.seek(seq) {
                        Err(e) => json!({"outcome": error_text(&e), "at": "seek"}),
                        Ok(()) => match reader.try_next() {
                            Ok(Some(r)) if r.seq == seq => json!({"outcome": "ok"}),
                            Ok(Some(r)) => json!({"outcome": format!("record {}", r.seq)}),
                            Ok(None) => json!({"outcome": "end", "at": "next"}),
                            Err(e) => json!({"outcome": error_text(&e), "at": "next"}),
                        },
                    },
                }
            }
        })
    })?;
    out["probe"] = json!(probe);
    out["reader"] = json!(kind.name());
    out["seq"] = json!(seq);
    Ok(out)
}

/// Reads from the oldest record on and reports how far it got.
fn replay(dir: &Path, kind: ReaderKind) -> Value {
    let mut reader = match kind.open(dir, ReaderConfig::default()) {
        Ok(reader) => reader,
        Err(e) => return json!({"outcome": error_text(&e), "records": 0}),
    };
    let (mut records, mut first, mut last) = (0u64, None, None);
    let outcome = loop {
        match reader.try_next() {
            Ok(Some(r)) => {
                records += 1;
                first = first.or(Some(r.seq));
                last = Some(r.seq);
            }
            Ok(None) => break "end".to_owned(),
            Err(e) => break error_text(&e),
        }
    };
    json!({"outcome": outcome, "records": records, "first_seq": first, "last_seq": last,
        "position": reader.position()})
}

#[cfg(feature = "core-1-1")]
fn seek_to_end(dir: &Path, kind: ReaderKind) -> Value {
    match kind.open(dir, ReaderConfig::default()) {
        Err(e) => json!({"outcome": error_text(&e), "at": "open"}),
        Ok(mut reader) => match reader.seek_to_end() {
            Ok(()) => json!({"outcome": "ok", "position": reader.position()}),
            Err(e) => {
                json!({"outcome": error_text(&e), "at": "seek", "position": reader.position()})
            }
        },
    }
}

#[cfg(not(feature = "core-1-1"))]
fn seek_to_end(_: &Path, _: ReaderKind) -> Value {
    json!({"outcome": "not measured: this core has no Reader::seek_to_end"})
}

/// Runs `verify` or `status` in this process.
pub fn check_probe(dir: &Path, probe: &str) -> Result<Value> {
    let mut out = timed(|| {
        Ok(if probe == "verify" {
            match inspect::verify(dir) {
                Err(e) => json!({"outcome": error_text(&e)}),
                Ok(report) => {
                    json!({"outcome": "ok", "records": report.records, "last_seq": report.last_seq,
                    "segments": report.segments.len(), "problem_count": report.problems.len(),
                    "problems": report.problems.iter().take(3).map(|p| format!("{} at {}: {}",
                        p.path.file_name().unwrap_or_default().to_string_lossy(), p.offset, p.reason))
                        .collect::<Vec<_>>()})
                }
            }
        } else {
            match tickertape::status::status(dir) {
                Err(e) => json!({"outcome": error_text(&e)}),
                Ok(status) => {
                    let breaches = status.breaches(&tickertape::status::Thresholds::default());
                    json!({"outcome": "ok", "next_seq": status.head.position.next_seq,
                        "verified": status.head.verified,
                        "corrupt": breaches.iter().any(tickertape::status::Breach::is_corruption),
                        "breaches": breaches.iter().map(tickertape::status::Breach::code).collect::<Vec<_>>()})
                }
            }
        })
    })?;
    out["probe"] = json!(probe);
    Ok(out)
}

/// Opens a writer under `policy` in this process; if it opens, appends one record and checkpoints.
pub fn writer_probe(dir: &Path, policy: RecoveryPolicy, shape: Shape) -> Result<Value> {
    let mut opened = None;
    let mut out = timed(|| {
        let config = WriterConfig::default()
            .segment_size(shape.segment_size)
            .recovery(policy);
        Ok(match Writer::open(dir, config) {
            Err(e) => json!({"outcome": error_text(&e)}),
            Ok(writer) => {
                let report = writer.recovery_report();
                let mut actions: BTreeMap<String, u64> = BTreeMap::new();
                for action in &report.actions {
                    *actions.entry(action_name(action)).or_default() += 1;
                }
                let out = json!({"outcome": "ok", "next_seq": report.next_seq, "actions": actions,
                    "scanned_from_seq": report.scanned_from_seq, "had_checkpoint": report.had_checkpoint});
                opened = Some(writer);
                out
            }
        })
    })?;
    out["open_files_held"] = json!(super::open_files());
    if let Some(mut writer) = opened {
        let payload = vec![0x5a; shape.payload];
        // Not a record of the base journal, so that the census can't take it for the one it may replace.
        let mut probe = record(&payload, writer.next_seq());
        probe.schema_id = 2;
        let after = writer
            .append(&probe)
            .and_then(|seq| writer.checkpoint().map(|()| seq))
            .and_then(|seq| writer.close().map(|()| seq));
        out["append_after"] = match after {
            Ok(seq) => json!(seq),
            Err(e) => json!(error_text(&e)),
        };
    }
    out["probe"] = json!(format!("writer-open-{}", policy_name(policy)));
    Ok(out)
}

/// The name of a policy on the command line and in results.
pub fn policy_name(policy: RecoveryPolicy) -> &'static str {
    if policy == RecoveryPolicy::Fail {
        "fail"
    } else {
        "quarantine"
    }
}

fn action_name(action: &RecoveryAction) -> String {
    match action {
        RecoveryAction::RepairedTail { reason, .. } => format!("RepairedTail ({reason})"),
        RecoveryAction::QuarantinedSegment { .. } => "QuarantinedSegment".to_owned(),
        other => {
            let debug = format!("{other:?}");
            debug.chars().take_while(|c| c.is_alphanumeric()).collect()
        }
    }
}

/// Where the records committed in the base journal are: byte for byte in their place in a segment of `dir`, byte for
/// byte in a file of its `quarantine/`, or neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Census {
    /// In a segment of the journal.
    pub in_segments: u64,
    /// Not in a segment, but in a quarantined file.
    pub quarantined: u64,
    /// In neither.
    pub absent: u64,
    /// Files in `quarantine/`.
    pub quarantine_files: u64,
}

impl Census {
    /// As JSON.
    pub fn json(&self) -> Value {
        json!({"in_segments": self.in_segments, "quarantined": self.quarantined, "absent": self.absent,
            "quarantine_files": self.quarantine_files})
    }
}

/// A quarantined file: the segment it came from and the offset its bytes start at.
fn quarantined_from(name: &str) -> Option<(u64, u64)> {
    let stem = name.strip_suffix(".bin")?;
    if let Some((segment, _)) = stem.split_once(".tape-") {
        return Some((segment.parse().ok()?, 0));
    }
    let mut parts = stem.split('-');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// Counts where every record of the base journal is in `dir`.
pub fn census(base: &Path, dir: &Path, shape: Shape) -> Result<Census> {
    let capacity = shape.capacity()?;
    let frame = record_bytes(shape.payload)?;
    let mut remaining = shape.records()?;
    let mut quarantine: BTreeMap<u64, Vec<(u64, PathBuf)>> = BTreeMap::new();
    let mut out = Census::default();
    let qdir = dir.join("quarantine");
    if qdir.exists() {
        for entry in fs::read_dir(&qdir)? {
            let entry = entry?;
            out.quarantine_files += 1;
            if let Some((segment, offset)) = quarantined_from(&entry.file_name().to_string_lossy())
            {
                quarantine
                    .entry(segment)
                    .or_default()
                    .push((offset, entry.path()));
            }
        }
    }
    for index in 0..shape.segments {
        let count = remaining.min(capacity);
        remaining -= count;
        let name = shape.file_of(index)?;
        let original = base.join(&name);
        let now = dir.join(&name);
        let held = quarantine.remove(&(index * capacity)).unwrap_or_default();
        if now.exists() && fs::metadata(&now)?.ino() == fs::metadata(&original)?.ino() {
            // A hard link to a file `Fingerprint::check` finds unchanged.
            out.in_segments += count;
            continue;
        }
        let end = shape.offset_of(count)?;
        let want = read_prefix(&original, end)?;
        let segment = if now.exists() {
            read_prefix(&now, end)?
        } else {
            Vec::new()
        };
        let mut files = Vec::new();
        for (offset, path) in held {
            let bytes = fs::read(&path)?;
            files.push((offset, bytes));
        }
        for i in 0..count {
            let start = shape.offset_of(i)?;
            let frame_at = |bytes: &[u8], from: u64| -> bool {
                let Some(at) = start.checked_sub(from) else {
                    return false;
                };
                let (at, len) = (at as usize, frame as usize);
                // A preserved tail ends at its last non-zero byte, which can cut the padding of its last record.
                let found = bytes.get(at..bytes.len().min(at + len)).unwrap_or(&[]);
                let original = &want[start as usize..start as usize + len];
                !found.is_empty()
                    && original.starts_with(found)
                    && original[found.len()..].iter().all(|b| *b == 0)
            };
            if frame_at(&segment, 0) {
                out.in_segments += 1;
            } else if files.iter().any(|(from, bytes)| frame_at(bytes, *from)) {
                out.quarantined += 1;
            } else {
                out.absent += 1;
            }
        }
    }
    Ok(out)
}

fn read_prefix(path: &Path, len: u64) -> Result<Vec<u8>> {
    let file = File::open(path)?;
    let len = len.min(file.metadata()?.len());
    let mut bytes = vec![0; usize::try_from(len)?];
    file.read_exact_at(&mut bytes, 0)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHAPE: Shape = Shape {
        segments: 8,
        segment_size: 65_536,
        payload: 100,
    };

    struct Bases(tempfile::TempDir);

    impl Bases {
        fn new() -> Bases {
            let root = tempfile::tempdir().unwrap();
            build_base(&root.path().join("in"), SHAPE, Base::MarkerInNewest).unwrap();
            build_base(&root.path().join("back"), SHAPE, Base::MarkerTwoBack).unwrap();
            Bases(root)
        }

        fn base(&self, case: Case) -> PathBuf {
            self.0.path().join(match case.base {
                Base::MarkerInNewest => "in",
                Base::MarkerTwoBack => "back",
            })
        }

        /// A fresh damaged copy.
        fn journal(&self, case: Case) -> PathBuf {
            let dir = self.0.path().join("case");
            prepare(&self.base(case), &dir, SHAPE, case).unwrap();
            dir
        }
    }

    /// The roadmap's rule, for every case and both policies: no record that was in a segment before the writer opened
    /// is in neither a segment nor `quarantine/` afterwards, unless the open reported it.
    #[test]
    fn recovery_keeps_or_quarantines_every_committed_record() {
        let bases = Bases::new();
        for case in CASES {
            for policy in [RecoveryPolicy::Quarantine, RecoveryPolicy::Fail] {
                let dir = bases.journal(*case);
                let base = bases.base(*case);
                let fingerprint = Fingerprint::of(&base).unwrap();
                let before = census(&base, &dir, SHAPE).unwrap();
                let out = writer_probe(&dir, policy, SHAPE).unwrap();
                let after = census(&base, &dir, SHAPE).unwrap();
                fingerprint.check(&base).unwrap();
                assert_eq!(
                    after.in_segments + after.quarantined,
                    before.in_segments,
                    "{} {policy:?}: {out} {before:?} {after:?}",
                    case.name
                );
                if out["outcome"] != "ok" {
                    assert_eq!(after, before, "{} {policy:?}: {out}", case.name);
                }
            }
        }
    }

    #[test]
    fn census_counts_what_the_damage_removed() {
        let bases = Bases::new();
        let records = SHAPE.records().unwrap();
        let capacity = SHAPE.capacity().unwrap();
        for (name, absent) in [
            ("intact", 0),
            ("missing-oldest", capacity),
            ("missing-newest", capacity / 2),
            ("record-middle-segment", 1),
            ("header-middle", 0),
            ("empty-word-newest-after-marker", 1),
        ] {
            let case = find(name).unwrap();
            let dir = bases.journal(case);
            let census = census(&bases.base(case), &dir, SHAPE).unwrap();
            assert_eq!(census.absent, absent, "{name}");
            assert_eq!(census.in_segments, records - absent, "{name}");
        }
        assert!(find("wat").is_err());
    }

    #[test]
    fn probes_report_outcomes_not_failures() {
        let bases = Bases::new();
        for name in ["missing-middle", "record-middle-segment", "header-oldest"] {
            let case = find(name).unwrap();
            let dir = bases.journal(case);
            for probe in READ_PROBES {
                let seq = seek_target(probe, SHAPE, case).unwrap();
                for kind in [ReaderKind::Safe, ReaderKind::Mapped] {
                    let out = read_probe(&dir, probe, kind, seq).unwrap();
                    assert!(out["outcome"].is_string(), "{name} {probe} {out}");
                }
            }
            let verify = check_probe(&dir, "verify").unwrap();
            assert!(
                verify["problem_count"].as_u64().unwrap() > 0,
                "{name} {verify}"
            );
            assert!(check_probe(&dir, "status").unwrap()["outcome"].is_string());
        }
        assert!(
            read_probe(
                &bases.journal(find("intact").unwrap()),
                "seek-last",
                ReaderKind::Safe,
                None
            )
            .unwrap()["error"]
                .is_string()
        );
    }
}
