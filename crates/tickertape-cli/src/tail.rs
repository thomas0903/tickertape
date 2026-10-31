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
                    TailFormat::Text => output::text(out, &record)
                        .map_err(|e| output::io_error("write tail output", e))?,
                    TailFormat::Json => output::record(out, &record)?,
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

fn resume(reader: &mut Reader, seq: u64) -> Result<()> {
    writeln!(
        io::stderr().lock(),
        "tickertape: warning: history discarded at sequence {seq}; seeking to the current chain"
    )
    .map_err(|e| output::io_error("write recovery warning", e))?;
    reader.seek(seq)?;
    Ok(())
}
