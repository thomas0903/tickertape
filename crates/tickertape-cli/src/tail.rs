use std::io::{self, Write};

use tickertape::{Error, Reader, ReaderConfig};

use crate::{Result, TailArgs, TailFormat, output};

pub(crate) fn run(args: TailArgs, out: &mut dyn Write) -> Result<()> {
    let mut reader = Reader::open(
        &args.dir,
        ReaderConfig::default().durable_only(args.durable_only),
    )?;
    if let Some(seq) = args.from {
        reader.seek(seq)?;
    }
    if args.from_end {
        // Leave this cursor at the first uncommitted slot, without a race from reopening it.
        loop {
            match reader.try_next() {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(Error::Discarded { seq, .. }) => resume(&mut reader, seq)?,
                Err(error) => return Err(error.into()),
            }
        }
    }
    let mut count = 0;
    while args.count.is_none_or(|limit| count < limit) {
        let next = if args.no_follow {
            reader.try_next()
        } else {
            reader.next_timeout(args.timeout_secs)
        };
        match next {
            Ok(Some(record)) => {
                match args.format {
                    TailFormat::Text => output::text(out, &record, args.decode)
                        .map_err(|e| output::io_error("write tail output", e))?,
                    TailFormat::Json => output::export_record(out, &record, args.decode)?,
                }
                out.flush()
                    .map_err(|e| output::io_error("flush tail output", e))?;
                count += 1;
            }
            Ok(None) => break,
            Err(Error::Discarded { seq, .. }) => resume(&mut reader, seq)?,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Continues after `Error::Discarded` at `seq`: seeks to `seq` on the current chain, or, if retention or archiving
/// removed it (a durable-only reader learns of a removal when the checkpoint moves past its position, ADR 0034), to
/// the oldest record still in the journal. Warnings go to standard error, so standard output keeps its format.
fn resume(reader: &mut Reader, seq: u64) -> Result<()> {
    warn(format_args!(
        "history discarded at sequence {seq}; seeking to the current chain"
    ))?;
    let mut target = seq;
    loop {
        match reader.seek(target) {
            Ok(()) => return Ok(()),
            Err(Error::SeqUnavailable { first, .. }) if first > target => {
                if first - 1 == seq {
                    warn(format_args!(
                        "record {seq} was removed before tail read it; continuing at sequence {first}"
                    ))?;
                } else {
                    warn(format_args!(
                        "records {seq} to {} were removed before tail read them; continuing at sequence {first}",
                        first - 1
                    ))?;
                }
                target = first;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn warn(message: std::fmt::Arguments<'_>) -> Result<()> {
    writeln!(io::stderr().lock(), "tickertape: warning: {message}")
        .map_err(|e| output::io_error("write recovery warning", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use tickertape::fault::Point;
    use tickertape::retention::{self, Action, Policy};
    use tickertape::{NewRecord, Writer, WriterConfig};

    /// Retention overtakes a durable-only tail four times in a row: each time the tail has sought to the oldest
    /// segment, retention removes it before the tail confirms that the segment is in place (after reading the
    /// checkpoint marker, ADR 0034), so the tail gets `Error::Discarded` and resumes again without returning a record
    /// (warning on standard error each time, with the records skipped). Here each resume follows a removal (a
    /// recovery that replaces the segment causes one too), so once retention stops, the tail returns the records still
    /// there and ends.
    #[test]
    fn durable_only_tail_resumes_once_per_removal() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            Writer::open(dir.path(), WriterConfig::default().segment_size(65536)).unwrap();
        for _ in 0..8 {
            writer
                .append(&NewRecord {
                    schema_id: 1,
                    venue_ts_ns: 0,
                    local_ts_ns: 0,
                    payload: &[7; 40_000],
                })
                .unwrap();
        }
        writer.checkpoint().unwrap();
        // Records 0..=7, one per segment; the checkpoint is in segment 7, so retention may remove segments 0 to 6.
        fn overtake(dir: PathBuf, removed: Rc<Cell<usize>>) {
            tickertape::fault::run_at(Point::ReaderCheckpointIdentity, 0, move || {
                removed.set(removed.get() + 1);
                let policy = Policy::default().max_segments(8 - removed.get());
                let plan = retention::plan(&dir, &policy).unwrap();
                retention::apply(&dir, &plan, &Action::Delete).unwrap();
                if removed.get() < 4 {
                    overtake(dir, removed);
                }
            });
        }
        let removed = Rc::new(Cell::new(0));
        tickertape::fault::clear();
        overtake(dir.path().to_path_buf(), removed.clone());
        let mut out = Vec::new();
        let result = run(
            TailArgs {
                dir: dir.path().into(),
                from: None,
                from_end: false,
                count: None,
                no_follow: true,
                durable_only: true,
                format: TailFormat::Json,
                decode: None,
                timeout_secs: None,
            },
            &mut out,
        );
        tickertape::fault::clear();
        assert!(result.is_ok(), "{:?}", result.err().map(|f| f.message));
        assert_eq!(removed.get(), 4);
        let seqs: Vec<u64> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["seq"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        assert_eq!(seqs, [4, 5, 6, 7]);
    }
}
