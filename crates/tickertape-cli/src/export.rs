use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use tickertape::{Reader, ReaderConfig};

use crate::{Decoder, ExportArgs, ExportFormat, Failure, Result, output, parquet_export};

pub(crate) fn help() -> String {
    format!(
        r#"{}

Parquet (Snappy, at most 100,000 rows per row group; requires --output FILE):
{}

Timestamps are raw unsigned nanoseconds since the Unix epoch; 0 = not set.

--decode market-demo adds these optional Parquet columns:
{}
JSONL adds a decoded object with the same fields (null for unknown/invalid payloads).
kind: trade|book; side: buy|sell (trade), bid|ask (book); action: upsert|delete (book).
trade_id is null for books; action is null for trades. price and qty = signed fixed-point raw / 1e8.
Schema 1: 32 bytes, little-endian: instrument_id u32 @0; side u8 @4 (1 buy, 2 sell); 3 zero bytes @5; price i64 @8; qty i64 @16; trade_id u64 @24.
Schema 2: 24 bytes, little-endian: instrument_id u32 @0; side u8 @4 (1 bid, 2 ask); action u8 @5 (1 upsert, 2 delete); 2 zero bytes @6; price i64 @8; qty i64 @16.
Unknown schema ids, wrong lengths, invalid tags or nonzero reserved bytes decode to null.
Corruption finalizes the valid prefix before exit 2. Inclusive ranges and startup snapshot apply to both formats."#,
        crate::RECORD_SCHEMA,
        parquet_export::SCHEMA,
        parquet_export::DECODED_SCHEMA,
    )
}

pub(crate) fn run(args: ExportArgs, stdout: &mut dyn Write) -> Result<()> {
    if matches!(args.format, ExportFormat::Parquet) && args.output == Path::new("-") {
        return Err(Failure::usage(
            "Parquet requires --output <FILE>; stdout (-) is not supported",
        ));
    }
    if args.from.zip(args.to).is_some_and(|(from, to)| to < from) {
        return Err(Failure::usage(
            "--to must be greater than or equal to --from",
        ));
    }
    // Capture the selected committed prefix using only the safe reader. Keep an error so the second pass can
    // emit all records preceding corruption before returning it. Neither pass waits for later appends.
    // Freeze checkpoint coverage before the scan: later checkpoints must not enlarge this export.
    let end_seq = if args.durable_only {
        Some(tickertape::inspect::read_checkpoint(&args.dir)?.map_or(0, |c| c.next_seq))
    } else {
        None
    };
    let mut snapshot = open(&args)?;
    let (count, terminal) = capture(&mut snapshot, args.to, end_seq);
    // Open the input before touching the destination, and reject output paths that alias journal files.
    let mut reader = open(&args)?;
    match args.format {
        ExportFormat::Jsonl => {
            if args.output == Path::new("-") {
                jsonl(&mut reader, count, stdout, args.decode)?;
            } else {
                let file = destination(&args.dir, &args.output)?;
                let mut out = BufWriter::new(file);
                let result = jsonl(&mut reader, count, &mut out, args.decode);
                out.flush()
                    .map_err(|e| output::io_error("flush export file", e))?;
                result?;
            }
        }
        ExportFormat::Parquet => {
            let file = destination(&args.dir, &args.output)?;
            parquet_export::write(&mut reader, count, file, args.decode)?;
        }
    }
    if let Some(error) = terminal {
        return Err(error.into());
    }
    Ok(())
}

fn open(args: &ExportArgs) -> Result<Reader> {
    let mut reader = Reader::open(
        &args.dir,
        ReaderConfig::default()
            .durable_only(args.durable_only)
            .notify(false),
    )?;
    if let Some(from) = args.from {
        reader.seek(from)?;
    }
    Ok(reader)
}

/// Capture a prefix without reading past the exclusive startup checkpoint boundary.
fn capture(
    reader: &mut Reader,
    to: Option<u64>,
    end_seq: Option<u64>,
) -> (u64, Option<tickertape::Error>) {
    let mut count = 0;
    let terminal = loop {
        if end_seq.is_some_and(|end| reader.position() >= end) {
            break None;
        }
        match reader.try_next() {
            Ok(Some(record)) if to.is_some_and(|to| record.seq > to) => break None,
            Ok(Some(record)) => {
                count += 1;
                if to == Some(record.seq) {
                    break None;
                }
            }
            Ok(None) => break None,
            Err(error) => break Some(error),
        }
    };
    (count, terminal)
}

/// Emit the captured record count, with optional payload decoding.
fn jsonl(
    reader: &mut Reader,
    count: u64,
    out: &mut dyn Write,
    decoder: Option<Decoder>,
) -> Result<()> {
    for _ in 0..count {
        match reader.try_next()? {
            Some(record) => output::export_record(out, &record, decoder)?,
            None => {
                return Err(Failure::usage(
                    "journal changed during export: captured records are no longer available",
                ));
            }
        }
    }
    Ok(())
}

fn destination(dir: &Path, path: &Path) -> Result<File> {
    let root =
        fs::canonicalize(dir).map_err(|e| output::io_error("resolve journal directory", e))?;
    let resolved = match fs::canonicalize(path) {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(Failure::usage("export output is an unresolved symlink"));
            }
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let parent = fs::canonicalize(parent)
                .map_err(|e| output::io_error("resolve output directory", e))?;
            let name = path
                .file_name()
                .ok_or_else(|| Failure::usage("output must name a file"))?;
            parent.join(name)
        }
        Err(e) => return Err(output::io_error("resolve export output", e)),
    };
    if resolved.starts_with(&root) {
        return Err(Failure::usage(
            "export output must be outside the journal directory",
        ));
    }
    // Do not truncate until we have compared the opened file's identity to journal entries, including hard links.
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&resolved)
        .map_err(|e| output::io_error(&format!("open export output {}", path.display()), e))?;
    let metadata = file
        .metadata()
        .map_err(|e| output::io_error("stat export output", e))?;
    reject_alias(&root, metadata.dev(), metadata.ino())?;
    file.set_len(0)
        .map_err(|e| output::io_error("truncate export output", e))?;
    Ok(file)
}

/// Fails if a file in the journal directory tree is the output file (a hard link), or if that can't be established.
/// The one exception is the reader beacon directory `readers/`, which holds no journal data: beacons that disappear
/// during the walk (dropped) and a `readers/` that can't be listed (another user's) are skipped, where v0.4.0
/// failed. Everywhere else (segments, the checkpoint marker, `quarantine/`) the check fails closed, as in v0.4.0: an
/// entry that vanishes or a directory that can't be listed is an error, so evidence in an unreadable `quarantine/`
/// is never truncated.
fn reject_alias(dir: &Path, dev: u64, ino: u64) -> Result<()> {
    let readers = dir.join(tickertape::status::READERS_DIR);
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let tolerant = dir.starts_with(&readers);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if tolerant && is_skippable(&e) => continue,
            Err(e) => return Err(output::io_error("list journal entries", e)),
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) if tolerant && is_skippable(&e) => continue,
                Err(e) => return Err(output::io_error("read journal entry", e)),
            };
            let tolerant = entry.path().starts_with(&readers);
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(e) if tolerant && e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(output::io_error("stat journal entry", e)),
            };
            if metadata.dev() == dev && metadata.ino() == ino {
                return Err(Failure::usage("export output aliases a journal file"));
            }
            if metadata.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}

/// A part of `readers/` that vanished or can't be read.
fn is_skippable(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::NotADirectory
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unreadable `readers/` doesn't fail the alias check (ADR 0032: beacon directories of other users), but any
    /// other unreadable subdirectory does (round 2, A3): an output hard-linked to evidence in an unreadable
    /// `quarantine/` is refused, never truncated.
    #[test]
    fn alias_walk_fails_closed_except_in_readers() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let readers = dir.path().join("readers");
        fs::create_dir(&readers).unwrap();
        fs::write(dir.path().join("file"), b"x").unwrap();
        let quarantine = dir.path().join("quarantine");
        fs::create_dir(&quarantine).unwrap();
        let evidence = quarantine.join("00000000000000000000.tape.0");
        fs::write(&evidence, b"evidence").unwrap();
        let out = tempfile::tempdir().unwrap();
        let output = out.path().join("out.jsonl");
        fs::hard_link(&evidence, &output).unwrap();
        let evidence_meta = fs::metadata(&evidence).unwrap();

        fs::set_permissions(&readers, fs::Permissions::from_mode(0o000)).unwrap();
        fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o000)).unwrap();
        let root = fs::read_dir(&readers).is_ok(); // permissions don't apply to root
        let unrelated = reject_alias(dir.path(), 0, 0);
        let alias = reject_alias(dir.path(), evidence_meta.dev(), evidence_meta.ino());
        fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o755)).unwrap();
        // An unreadable readers/ alone is skipped.
        let readers_only = reject_alias(dir.path(), 0, 0);
        fs::set_permissions(&readers, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            alias.is_err(),
            "an alias inside an unreadable quarantine/ was accepted"
        );
        if !root {
            assert!(unrelated.is_err(), "an unreadable quarantine/ was skipped");
        }
        assert!(
            readers_only.is_ok(),
            "{:?}",
            readers_only.err().map(|f| f.message)
        );
        let meta = fs::metadata(dir.path().join("file")).unwrap();
        assert!(reject_alias(dir.path(), meta.dev(), meta.ino()).is_err());
    }
    use std::io;
    use tickertape::{NewRecord, Writer, WriterConfig};

    struct AppendOnWrite {
        writer: Writer,
        bytes: Vec<u8>,
        appended: bool,
    }

    impl Write for AppendOnWrite {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if !self.appended {
                self.writer
                    .append(&NewRecord {
                        schema_id: 2,
                        venue_ts_ns: 0,
                        local_ts_ns: 0,
                        payload: b"later",
                    })
                    .unwrap();
                self.appended = true;
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn durable_snapshot_keeps_startup_checkpoint_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            Writer::open(dir.path(), WriterConfig::default().segment_size(65536)).unwrap();
        let record = NewRecord {
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: b"record",
        };
        writer.append(&record).unwrap();
        writer.checkpoint().unwrap();
        let end = tickertape::inspect::read_checkpoint(dir.path())
            .unwrap()
            .unwrap()
            .next_seq;
        let mut reader =
            Reader::open(dir.path(), ReaderConfig::default().durable_only(true)).unwrap();
        writer.append(&record).unwrap();
        writer.checkpoint().unwrap();
        let (count, terminal) = capture(&mut reader, None, Some(end));
        assert_eq!(count, 1);
        assert!(terminal.is_none());
        // The reader can see the later checkpoint; only the fixed startup cap excludes it.
        assert_eq!(reader.next_owned().unwrap().unwrap().seq, 1);
    }

    #[test]
    fn export_excludes_appends_after_startup_scan() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            Writer::open(dir.path(), WriterConfig::default().segment_size(65536)).unwrap();
        writer
            .append(&NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload: b"initial",
            })
            .unwrap();
        let mut sink = AppendOnWrite {
            writer,
            bytes: Vec::new(),
            appended: false,
        };
        run(
            ExportArgs {
                dir: dir.path().into(),
                format: ExportFormat::Jsonl,
                decode: None,
                output: "-".into(),
                from: None,
                to: None,
                durable_only: false,
            },
            &mut sink,
        )
        .unwrap();
        assert!(sink.appended);
        let text = String::from_utf8(sink.bytes).unwrap();
        assert_eq!(text.lines().count(), 1);
        let json: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(json["seq"], 0);
        let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
        assert_eq!(reader.next_owned().unwrap().unwrap().seq, 0);
        assert_eq!(reader.next_owned().unwrap().unwrap().seq, 1);
    }
}
