//! The planner against the one it replaced, which read every header before it looked at the policy (ADR 0051).
//!
//! [`reference`] is that planner, unchanged. On any journal the two agree, except that the reference also fails for a
//! header the policy does not need. The property test states exactly which headers those are ([`considered`]), repairs
//! them and compares again.

use std::collections::BTreeSet;
use std::os::unix::fs::symlink;

use proptest::prelude::*;

use super::*;
use crate::format::SegmentHeader;

const SIZE: u64 = format::SEGMENT_SIZE_UNIT;
/// When the plans are made.
const NOW: u64 = 1_000_000;

/// The planner before ADR 0051. `seals` false leaves the seals unchecked (to find what the policy considers).
fn reference(
    dir: &Path,
    policy: &Policy,
    now: u64,
    stop: &dyn Fn(usize) -> bool,
    seals: bool,
) -> Result<Plan> {
    let directory = directory_identity(dir)?;
    let paths = journal::list_segments(dir)?;
    if paths.is_empty() {
        return Err(Error::NotFound { dir: dir.into() });
    }
    let mut plan = Plan {
        segments: Vec::new(),
        directory,
    };
    if policy == &Policy::default() {
        return Ok(plan);
    }
    let Some(checkpoint) = checkpoint(dir)? else {
        return Ok(plan);
    };
    let headers = paths
        .iter()
        .map(|(base, path)| {
            let file = File::open(path).map_err(|e| Error::io("open segment", path, e))?;
            Ok((
                journal::read_header(&file, path, *base)?,
                journal::file_identity(&file, path)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut count = paths.len();
    let mut bytes: u128 = headers
        .iter()
        .map(|(h, _)| u128::from(h.segment_size))
        .sum();
    for (i, (base, path)) in paths.iter().enumerate() {
        if *base >= checkpoint.segment_base_seq || i + 1 == paths.len() {
            break;
        }
        let (header, identity) = headers[i];
        let sealed = headers[i + 1].0.created_unix_ns;
        let reasons = reasons(policy, count, bytes, now, sealed);
        if reasons.is_empty() || (!plan.segments.is_empty() && stop(plan.segments.len())) {
            break;
        }
        if seals {
            let file = File::open(path).map_err(|e| Error::io("open segment", path, e))?;
            check_seal(&file, path, header.segment_size, *base, paths[i + 1].0)?;
        }
        plan.segments.push(PlannedSegment {
            path: path.clone(),
            base_seq: *base,
            size_bytes: header.segment_size,
            sealed_unix_ns: sealed,
            reasons,
            identity,
            successor: paths[i + 1].0,
        });
        count -= 1;
        bytes -= u128::from(header.segment_size);
    }
    Ok(plan)
}

/// How many of the oldest segments the policy considers on a journal whose headers are all valid: every candidate and its
/// successor, and with `max_age` the first segment that stays and its successor.
fn considered(dir: &Path, policy: &Policy, stop: &dyn Fn(usize) -> bool) -> usize {
    let candidates = reference(dir, policy, NOW, stop, false)
        .unwrap()
        .segments
        .len();
    let bases = bases(dir);
    let marker = checkpoint(dir).unwrap().unwrap().segment_base_seq;
    let eligible = candidates + 1 < bases.len() && bases[candidates] < marker;
    // The first segment that stays: its age is looked at unless another rule ended the plan before.
    if policy.max_age.is_some() && eligible {
        candidates + 2
    } else if candidates > 0 {
        candidates + 1
    } else {
        0
    }
}

fn bases(dir: &Path) -> Vec<u64> {
    journal::list_bases(dir)
        .unwrap()
        .iter()
        .map(|s| s.base)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Header {
    Valid,
    Magic,
    Checksum,
    Version,
    /// Valid, for another base.
    Base,
    /// Valid, for twice the file's size.
    Size,
    /// The header of a segment that never reached the disk (zeros).
    Zero,
    /// The file is this long.
    Short(u64),
    /// Not a file that can be opened and read: a directory.
    Directory,
    /// A name for a file that is not there.
    Dangling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seal {
    Valid,
    Missing,
    Damaged,
    /// For another successor.
    Other,
}

#[derive(Debug, Clone)]
struct Segment {
    records: u64,
    units: u64,
    created: u64,
    header: Header,
    seal: Seal,
    /// Not created: a gap.
    gone: bool,
    symlink: bool,
    index: bool,
}

#[derive(Debug, Clone, Copy)]
enum Marker {
    None,
    Damaged,
    /// Names the segment at this place (modulo their number), at an offset that is valid or not.
    At(usize, bool),
    /// Names a segment that is not there.
    Dangling,
}

#[derive(Debug, Clone)]
struct Case {
    first: u64,
    segments: Vec<Segment>,
    marker: Marker,
    strays: BTreeSet<usize>,
    policy: Policy,
    stop: Option<usize>,
}

const STRAYS: [&str; 7] = [
    "next-segment.tmp",
    "00000000000000000001.tape.tmp",
    "0000000000000000001.tape",
    "99999999999999999999.tape",
    "00000000000000000001.tape.bak",
    "00000000000000000001.TAPE",
    "quarantine",
];

fn header() -> impl Strategy<Value = Header> {
    prop_oneof![
        40 => Just(Header::Valid),
        1 => Just(Header::Magic),
        1 => Just(Header::Checksum),
        1 => Just(Header::Version),
        1 => Just(Header::Base),
        1 => Just(Header::Size),
        1 => Just(Header::Zero),
        1 => prop_oneof![Just(0), Just(63), Just(64), Just(SIZE / 2)].prop_map(Header::Short),
        1 => Just(Header::Directory),
        1 => Just(Header::Dangling),
    ]
}

fn segment() -> impl Strategy<Value = Segment> {
    let seal = prop_oneof![
        30 => Just(Seal::Valid),
        1 => Just(Seal::Missing),
        1 => Just(Seal::Damaged),
        1 => Just(Seal::Other),
    ];
    (
        1u64..4,
        1u64..4,
        prop_oneof![0..NOW, Just(NOW), NOW..2 * NOW],
        header(),
        seal,
        prop::bool::weighted(0.03),
        prop::bool::weighted(0.1),
        any::<bool>(),
    )
        .prop_map(
            |(records, units, created, header, seal, gone, symlink, index)| Segment {
                records,
                units,
                created,
                header,
                seal,
                gone,
                symlink,
                index,
            },
        )
}

fn policy() -> impl Strategy<Value = Policy> {
    (
        prop::option::of(0usize..9),
        prop::option::of(0u64..22 * SIZE),
        prop::option::of(0u64..2 * NOW),
    )
        .prop_map(|(max_segments, max_bytes, age)| Policy {
            max_segments,
            max_bytes,
            max_age: age.map(Duration::from_nanos),
        })
}

fn case() -> impl Strategy<Value = Case> {
    let marker = prop_oneof![
        1 => Just(Marker::None),
        1 => Just(Marker::Damaged),
        20 => (0usize..8, prop::bool::weighted(0.95)).prop_map(|(i, ok)| Marker::At(i, ok)),
        // The newest segment: the usual one.
        10 => Just(Marker::At(usize::MAX, true)),
        1 => Just(Marker::Dangling),
    ];
    (
        prop_oneof![Just(0u64), 1u64..1000, Just(u64::MAX - 60)],
        prop::collection::vec(segment(), 0..9),
        marker,
        prop::collection::btree_set(0..STRAYS.len(), 0..4),
        policy(),
        prop::option::weighted(0.3, 1usize..4),
    )
        .prop_map(|(first, segments, marker, strays, policy, stop)| Case {
            first,
            segments,
            marker,
            strays,
            policy,
            stop,
        })
}

/// A sealed segment file of zeros but for its header and its seal (after one record's worth of space, which holds no
/// valid record).
fn write_segment(path: &Path, s: &Segment, base: u64, next: u64) {
    if s.header == Header::Directory {
        fs::create_dir(path).unwrap();
        return;
    }
    if s.header == Header::Dangling {
        symlink("nowhere", path).unwrap();
        return;
    }
    let target = if s.symlink {
        let target = path.with_extension("target");
        symlink(&target, path).unwrap();
        target
    } else {
        path.to_path_buf()
    };
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
        .unwrap();
    let size = s.units * SIZE;
    file.set_len(size).unwrap();
    let encode = |base_seq, segment_size| {
        format::encode_segment_header(&SegmentHeader {
            segment_size,
            base_seq,
            created_unix_ns: s.created,
        })
    };
    let mut bytes = match s.header {
        Header::Base => encode(base ^ 1, size),
        Header::Size => encode(base, size * 2),
        _ => encode(base, size),
    };
    match s.header {
        Header::Magic => bytes[0] ^= 1,
        Header::Checksum => bytes[60] ^= 1,
        Header::Version => bytes[8] = 9,
        Header::Zero => bytes.fill(0),
        _ => {}
    }
    file.write_all_at(&bytes, 0).unwrap();
    let mut seal = [0; 8];
    let named = if s.seal == Seal::Other {
        next ^ 1
    } else {
        next
    };
    seal[..4].copy_from_slice(&format::SEAL_MARKER.to_le_bytes());
    seal[4..].copy_from_slice(&format::seal_crc(named).to_le_bytes());
    match s.seal {
        Seal::Missing => seal.fill(0),
        Seal::Damaged => seal[5] ^= 1,
        _ => {}
    }
    file.write_all_at(&seal, size - 8).unwrap();
    if let Header::Short(len) = s.header {
        file.set_len(len).unwrap();
    }
}

fn write(dir: &Path, case: &Case) {
    let mut base = case.first;
    let mut written = Vec::new();
    for s in &case.segments {
        let next = base + s.records;
        if !s.gone {
            write_segment(&journal::segment_path(dir, base), s, base, next);
            written.push(base);
        }
        if s.index {
            let index = dir.join("seek-index");
            fs::create_dir_all(&index).unwrap();
            fs::write(index.join(format!("{base:020}.idx")), b"x").unwrap();
        }
        base = next;
    }
    for stray in &case.strays {
        let path = dir.join(STRAYS[*stray]);
        if STRAYS[*stray] == "quarantine" {
            fs::create_dir(&path).unwrap();
            fs::write(path.join("00000000000000000000.tape"), b"x").unwrap();
        } else if fs::symlink_metadata(&path).is_err() {
            // (A file system that ignores case may have the name already.)
            fs::write(path, b"x").unwrap();
        }
    }
    let (named, offset) = match case.marker {
        Marker::None => return,
        Marker::Damaged => {
            fs::write(dir.join(journal::CHECKPOINT_FILE), [7; 64]).unwrap();
            return;
        }
        Marker::At(i, valid) => {
            let Some(last) = written.len().checked_sub(1) else {
                return;
            };
            let i = if i == usize::MAX { last } else { i.min(last) };
            (written[i], if valid { 64 } else { 65 })
        }
        Marker::Dangling => (base + 5, 64),
    };
    let slot = CheckpointSlot {
        generation: 2,
        next_seq: named,
        segment_base_seq: named,
        offset,
    };
    fs::write(
        dir.join(journal::CHECKPOINT_FILE),
        format::encode_checkpoint_slot(&slot),
    )
    .unwrap();
}

/// Everything there is to see of a result: the plan with its reasons, sizes and identities, or the error with its path,
/// offset and cause.
fn show(result: &Result<Plan>) -> String {
    format!("{result:?}")
}

/// Compares the planner with the reference on the journal in `dir`.
fn compare(
    dir: &Path,
    policy: &Policy,
    stop: &dyn Fn(usize) -> bool,
    sizes: &mut Sizes,
) -> std::result::Result<(), TestCaseError> {
    let new = plan_at(dir, policy, NOW, stop, sizes);
    let mut repaired = BTreeSet::new();
    loop {
        let old = reference(dir, policy, NOW, stop, true);
        if show(&new) == show(&old) {
            break;
        }
        // The only difference allowed: the reference stopped at the header of a segment the policy does not consider.
        let path = match &old {
            Err(
                Error::Corrupt { path, .. }
                | Error::UnsupportedVersion { path, .. }
                | Error::Io { path, .. },
            ) => path.clone(),
            other => panic!("{other:?} became {new:?}"),
        };
        let place = journal::list_segments(dir)
            .unwrap()
            .iter()
            .position(|(_, p)| *p == path);
        let Some(place) = place.filter(|p| !repaired.contains(p)) else {
            panic!("{old:?} became {new:?}");
        };
        // Make that header valid for the file there is, of any creation time: nothing else of the result may change.
        let size = fs::metadata(&path).map_or(0, |m| if m.is_dir() { 0 } else { m.len() });
        if size == 0 || size % SIZE != 0 {
            // No header makes a segment of this one. With `max_bytes` its length counts either way: make the file the
            // length of no segment's, and take what the planner says of that journal.
            if policy.max_bytes.is_some() {
                return Ok(());
            }
            let _ = fs::remove_dir(&path);
            let _ = fs::remove_file(&path);
            File::create(&path).unwrap().set_len(SIZE).unwrap();
        }
        let size = fs::metadata(&path).unwrap().len();
        let base = bases(dir)[place];
        let header = format::encode_segment_header(&SegmentHeader {
            segment_size: size,
            base_seq: base,
            created_unix_ns: 1,
        });
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all_at(&header, 0).unwrap();
        repaired.insert(place);
    }
    if !repaired.is_empty() {
        // Every header is valid now, at least up to what matters.
        let considered = considered(dir, policy, stop);
        for place in &repaired {
            prop_assert!(
                *place >= considered,
                "the header of segment {place} is one of the {considered} considered, but was not an error"
            );
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// The same plan, or the same error, for any journal and any policy, except for headers no limit needs. The sizes a
    /// writer keeps make no difference, also after other processes have removed, moved and replaced segments.
    #[test]
    #[cfg_attr(miri, ignore = "files")]
    fn any_plan_is_the_one_the_old_planner_made(case in case(), other in policy(), drop in 0usize..4) {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &case);
        let stop = |planned: usize| case.stop.is_some_and(|at| planned >= at);
        // (The sizes as this journal shows them; `compare` may repair headers, in place.)
        let mut sizes = Sizes::default();
        let _ = plan_at(dir.path(), &Policy::default().max_bytes(0), NOW, &stop, &mut sizes);
        for policy in [&case.policy, &other] {
            compare(dir.path(), policy, &stop, &mut Sizes::default())?;
            compare(dir.path(), policy, &stop, &mut sizes)?;
        }
        // Another process archives the oldest segments, and one segment is replaced by a file of another size.
        for base in bases(dir.path()).into_iter().take(drop) {
            let path = journal::segment_path(dir.path(), base);
            let _ = fs::remove_file(&path);
            let _ = fs::remove_dir_all(&path);
        }
        if let Some(base) = bases(dir.path()).into_iter().nth(drop) {
            let path = journal::segment_path(dir.path(), base);
            if fs::symlink_metadata(&path).unwrap().is_file() {
                let s = Segment {
                    records: 1,
                    units: 5,
                    created: 3,
                    header: Header::Valid,
                    seal: Seal::Valid,
                    gone: false,
                    symlink: false,
                    index: false,
                };
                let copy = dir.path().join("replacement");
                write_segment(&copy, &s, base, base + 1);
                fs::rename(&copy, &path).unwrap();
            }
        }
        for policy in [&case.policy, &other] {
            compare(dir.path(), policy, &stop, &mut sizes)?;
        }
    }
}

/// `n` sealed segments of one size, sealed at 0, 10, 20, ..., with the marker in the newest.
fn journal_of(n: u64) -> (tempfile::TempDir, Vec<PathBuf>) {
    let dir = tempfile::tempdir().unwrap();
    let segments = (0..n)
        .map(|i| Segment {
            records: 1,
            units: 1,
            created: i * 10,
            header: Header::Valid,
            seal: Seal::Valid,
            gone: false,
            symlink: false,
            index: false,
        })
        .collect();
    let case = Case {
        first: 0,
        segments,
        marker: Marker::At(usize::MAX, true),
        strays: BTreeSet::new(),
        policy: Policy::default(),
        stop: None,
    };
    write(dir.path(), &case);
    let paths = (0..n)
        .map(|b| journal::segment_path(dir.path(), b))
        .collect();
    (dir, paths)
}

/// An age limit that the first `k` segments of [`journal_of`] exceed.
fn older(k: u64) -> Policy {
    Policy::default().max_age(Duration::from_nanos(NOW - k * 10 - 5))
}

/// Which damaged header stops which policy. Segment `at` of 20 is damaged; the old planner failed for every one.
#[test]
#[cfg_attr(miri, ignore = "files")]
fn a_damaged_header_matters_only_where_the_policy_looks() {
    let planned = |policy: &Policy, at: usize| {
        let (dir, paths) = journal_of(20);
        let file = OpenOptions::new().write(true).open(&paths[at]).unwrap();
        file.write_all_at(&[0xFF], 0).unwrap();
        assert!(reference(dir.path(), policy, NOW, &|_| false, true).is_err());
        match plan_at(dir.path(), policy, NOW, &|_| false, &mut Sizes::default()) {
            Ok(plan) => Some(plan.segments.len()),
            Err(Error::Corrupt {
                path,
                offset: 0,
                reason: CorruptReason::BadMagic,
                ..
            }) if path == paths[at] => None,
            other => panic!("{other:?}"),
        }
    };
    let three = [
        Policy::default().max_segments(17),
        Policy::default().max_bytes(17 * SIZE),
    ];
    for at in 0..20 {
        // Nothing to remove by count or size: only the marker's segment is opened.
        for policy in [
            Policy::default().max_segments(20),
            Policy::default().max_bytes(20 * SIZE),
        ] {
            assert_eq!(planned(&policy, at), (at != 19).then_some(0), "{at}");
        }
        // Segments 0 to 2 go: their headers and the one of segment 3.
        for policy in &three {
            let clear = at > 3 && at != 19;
            assert_eq!(planned(policy, at), clear.then_some(3), "{at}");
        }
        // Nothing is old enough: segment 0 and its successor say so. Three are: segments 0 to 4.
        let clear = at > 1 && at != 19;
        assert_eq!(planned(&older(0), at), clear.then_some(0), "{at}");
        let clear = at > 4 && at != 19;
        assert_eq!(planned(&older(3), at), clear.then_some(3), "{at}");
        // A candidate by count, looked at for its age too.
        let both = older(0).max_segments(18);
        let clear = at > 3 && at != 19;
        assert_eq!(planned(&both, at), clear.then_some(2), "{at}");
    }
}

/// A stop that comes only while the seals are read ends the plan there: the rest is neither read nor planned.
#[test]
#[cfg_attr(miri, ignore = "files")]
fn a_stop_while_the_seals_are_read_ends_the_plan() {
    let (dir, paths) = journal_of(8);
    // The fourth seal is damaged. The first phase asks 6 times (before each candidate but the first), so the stop comes
    // before the third or the fourth seal.
    let file = OpenOptions::new().write(true).open(&paths[3]).unwrap();
    file.write_all_at(&[0; 8], SIZE - 8).unwrap();
    let policy = Policy::default().max_segments(0);
    let at = |calls: usize| {
        let asked = std::cell::Cell::new(0);
        let stop = |_| {
            asked.set(asked.get() + 1);
            asked.get() > calls
        };
        plan_at(dir.path(), &policy, NOW, &stop, &mut Sizes::default())
    };
    assert_eq!(at(7).unwrap().segments.len(), 2);
    assert_eq!(at(8).unwrap().segments.len(), 3);
    assert!(matches!(at(9), Err(Error::Corrupt { .. })));
}
