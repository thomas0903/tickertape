//! `tt-scale archive`: moves the old segments of a journal to an archive directory, with readers open in them, and
//! records what reading and seeking across the boundary does.

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{Value, json};
use tickertape::retention::{self, Action, Policy};
use tickertape::{ReaderConfig, inspect};

use super::{ReaderKind, timed};
use crate::Result;

/// The error's variant name, then its text: what the tables report.
pub fn error_text(error: &tickertape::Error) -> String {
    let debug = format!("{error:?}");
    let name: String = debug.chars().take_while(|c| c.is_alphanumeric()).collect();
    format!("{name}: {error}")
}

/// `retention::plan` and `retention::apply` (`MoveTo`), as `tickertape archive --to --keep-segments` runs them, in
/// this process, with their cost. A move reads and validates the whole segment before it renames it.
pub fn move_segments(dir: &Path, to: &Path, keep: usize) -> Result<Value> {
    timed(|| {
        let start = Instant::now();
        let plan = retention::plan(dir, &Policy::default().max_segments(keep))?;
        let plan_s = start.elapsed().as_secs_f64();
        let report = retention::apply(dir, &plan, &Action::MoveTo(to.to_path_buf()))?;
        Ok(
            json!({"plan_s": plan_s, "apply_s": start.elapsed().as_secs_f64() - plan_s,
            "moved_segments": report.removed_segments, "moved_bytes": report.removed_bytes}),
        )
    })
}

/// Hard-links every segment of `from` into `to`.
fn link_segments(from: &Path, to: &Path) -> Result<()> {
    for entry in std::fs::read_dir(from)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "tape") {
            if let Some(name) = path.file_name() {
                std::fs::hard_link(&path, to.join(name))?;
            }
        }
    }
    Ok(())
}

/// A reader left open in the oldest segment while that segment is moved away: how many records it still returns, how
/// it learns of the move, and what seeking to its position does then.
fn held_reader(reader: &mut tickertape::Reader, already: u64) -> Value {
    let mut records = already;
    let end = loop {
        match reader.try_next() {
            Ok(Some(_)) => records += 1,
            Ok(None) => break "end of journal".to_owned(),
            Err(e) => break error_text(&e),
        }
    };
    let position = reader.position();
    let seek = match reader.seek(position) {
        Ok(()) => "ok".to_owned(),
        Err(e) => error_text(&e),
    };
    json!({"records": records, "end": end, "position": position, "seek_to_position": seek})
}

/// Reads `[first, end)` from a reader on `dir`, checking that the sequence numbers have no gap.
fn read_range(kind: ReaderKind, dir: &Path, first: u64, end: u64) -> Result<u64> {
    let mut reader = kind.open(dir, ReaderConfig::default())?;
    reader.seek(first)?;
    for seq in first..end {
        let found = reader
            .try_next()?
            .ok_or_else(|| format!("no record {seq} in {}", dir.display()))?
            .seq;
        if found != seq {
            return Err(format!("{} returned {found}, not {seq}", dir.display()).into());
        }
    }
    Ok(end - first)
}

/// Moves all but the newest `keep` segments of the journal in `dir` to `to` (new or empty), running the move with
/// `mover` (a child process, so that its cost is its own), and records what readers see. `joined` is a scratch
/// directory for the rejoined journal; it is removed again.
pub fn run(
    dir: &Path,
    to: &Path,
    joined: &Path,
    mover: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    let first = inspect::list_segments(dir)?[0].base_seq;
    let end = inspect::read_checkpoint(dir)?
        .ok_or("the journal has no checkpoint marker")?
        .next_seq;
    crate::fresh_directory(to)?;
    crate::fresh_directory(joined)?;
    let mut held = Vec::new();
    for kind in [ReaderKind::Safe, ReaderKind::Mapped] {
        let mut reader = kind.open(dir, ReaderConfig::default())?;
        reader.try_next()?.ok_or("the journal holds no records")?;
        held.push((kind, reader));
    }
    let moved = mover()?;
    let boundary = inspect::list_segments(dir)?[0].base_seq;
    if boundary == first {
        return Err(
            "nothing was moved: the journal has too few segments before its checkpoint".into(),
        );
    }
    let mut readers = Vec::new();
    for (kind, mut reader) in held {
        let held = held_reader(&mut reader, 1);
        let mut fresh = kind.open(dir, ReaderConfig::default())?;
        let opens_at = fresh.position();
        let seek_before = match fresh.seek(boundary - 1) {
            Ok(()) => "ok".to_owned(),
            Err(e) => error_text(&e),
        };
        drop(fresh);
        let across = timed(|| {
            let archived = read_range(kind, to, first, boundary)?;
            let rest = read_range(kind, dir, boundary, end)?;
            Ok(json!({"archived_records": archived, "journal_records": rest}))
        })?;
        // Past the archive's last record, the seal names a segment that is not there.
        let mut reader = kind.open(to, ReaderConfig::default())?;
        reader.seek(boundary - 1)?;
        reader.try_next()?;
        let past_end = match reader.try_next() {
            Ok(Some(r)) => format!("record {}", r.seq),
            Ok(None) => "end of journal".to_owned(),
            Err(e) => error_text(&e),
        };
        let seek_past = match reader.seek(boundary).and_then(|()| reader.next_owned()) {
            Ok(Some(r)) => format!("record {}", r.seq),
            Ok(None) => "end of journal".to_owned(),
            Err(e) => error_text(&e),
        };
        readers.push(
            json!({"reader": kind.name(), "open_reader_in_moved_segment": held,
            "journal_opens_at": opens_at, "journal_seek_to_archived_record": seek_before,
            "archive_then_journal": across, "archive_past_last_record": past_end,
            "archive_seek_past_last_record": seek_past}),
        );
    }
    let problems = |dir: &Path| -> Result<Value> {
        let report = inspect::verify(dir)?;
        Ok(
            json!({"records": report.records, "problems": report.problems.iter()
            .map(|p| p.reason.to_string()).collect::<Vec<_>>()}),
        )
    };
    let durable = {
        let mut reader = ReaderKind::Safe.open(to, ReaderConfig::default().durable_only(true))?;
        reader.try_next()?.is_some()
    };
    link_segments(to, joined)?;
    link_segments(dir, joined)?;
    std::fs::copy(dir.join("checkpoint"), joined.join("checkpoint"))?;
    let joined_verify = problems(joined)?;
    let joined_read = read_range(ReaderKind::Safe, joined, first, end)?;
    std::fs::remove_dir_all(joined)?;
    Ok(
        json!({"first_seq": first, "boundary_seq": boundary, "end_seq": end, "move": moved, "readers": readers,
        "archive_verify": problems(to)?, "journal_verify": problems(dir)?, "durable_only_reads_archive": durable,
        "joined_verify": joined_verify, "joined_records_read": joined_read}),
    )
}

/// The scratch directory for the rejoined journal of an archive run (same filesystem as the archive, for hard links).
pub fn joined_dir(to: &Path) -> PathBuf {
    to.with_extension("joined")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scale::build;

    #[test]
    fn archived_history_reads_up_to_the_boundary_and_rejoins() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("journal");
        let to = root.path().join("archive");
        let built = build(&dir, 6, 65_536, 100, 0, 50).unwrap();
        let capacity = built["records_per_segment"].as_u64().unwrap();
        let out = run(&dir, &to, &joined_dir(&to), || move_segments(&dir, &to, 2)).unwrap();
        assert_eq!(out["boundary_seq"], 4 * capacity);
        assert_eq!(out["move"]["moved_segments"], 4);
        for reader in out["readers"].as_array().unwrap() {
            let held = &reader["open_reader_in_moved_segment"];
            assert_eq!(held["records"], capacity);
            assert!(
                held["end"].as_str().unwrap().starts_with("Discarded"),
                "{held}"
            );
            assert!(
                held["seek_to_position"]
                    .as_str()
                    .unwrap()
                    .starts_with("SeqUnavailable")
            );
            assert_eq!(reader["journal_opens_at"], 4 * capacity);
            assert!(
                reader["journal_seek_to_archived_record"]
                    .as_str()
                    .unwrap()
                    .starts_with("SeqUnavailable")
            );
            assert_eq!(
                reader["archive_then_journal"]["archived_records"],
                4 * capacity
            );
            assert!(
                reader["archive_past_last_record"]
                    .as_str()
                    .unwrap()
                    .starts_with("Corrupt")
            );
        }
        assert_eq!(
            out["archive_verify"]["problems"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            out["joined_verify"]["problems"].as_array().unwrap().len(),
            0
        );
        assert_eq!(out["durable_only_reads_archive"], false);
        assert_eq!(out["joined_records_read"], out["end_seq"]);
    }
}
