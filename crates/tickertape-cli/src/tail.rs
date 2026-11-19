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
                Err(Error::Discarded { seq }) => resume(&mut reader, seq)?,
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
            Err(Error::Discarded { seq }) => resume(&mut reader, seq)?,
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
                warn(format_args!(
                    "records {seq} to {} were removed before tail read them; continuing at sequence {first}",
                    first - 1
                ))?;
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
