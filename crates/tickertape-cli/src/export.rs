use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use tickertape::{Reader, ReaderConfig};

use crate::{ExportArgs, ExportFormat, Failure, Result, output};

pub(crate) fn run(args: ExportArgs, stdout: &mut dyn Write) -> Result<()> {
    if args.from.zip(args.to).is_some_and(|(from, to)| to < from) {
        return Err(Failure::usage(
            "--to must be greater than or equal to --from",
        ));
    }
    // Capture the selected committed prefix using only the safe reader. Keep an error so the second pass can
    // emit all records preceding corruption before returning it. Neither pass waits for later appends.
    let mut snapshot = open(&args)?;
    let mut count = 0_u64;
    let terminal = loop {
        match snapshot.try_next() {
            Ok(Some(record)) if args.to.is_some_and(|to| record.seq > to) => break None,
            Ok(Some(record)) => {
                count += 1;
                if args.to == Some(record.seq) {
                    break None;
                }
            }
            Ok(None) => break None,
            Err(error) => break Some(error),
        }
    };
    // Open the input before touching the destination, and reject output paths that alias journal files.
    let mut reader = open(&args)?;
    match args.format {
        ExportFormat::Jsonl => {
            if args.output == Path::new("-") {
                jsonl(&mut reader, count, stdout)?;
            } else {
                let file = destination(&args.dir, &args.output)?;
                let mut out = BufWriter::new(file);
                let result = jsonl(&mut reader, count, &mut out);
                out.flush()
                    .map_err(|e| output::io_error("flush export file", e))?;
                result?;
            }
        }
    }
    if let Some(error) = terminal {
        return Err(error.into());
    }
    Ok(())
}

fn open(args: &ExportArgs) -> Result<Reader> {
    let mut reader = Reader::open(&args.dir, ReaderConfig::default())?;
    if let Some(from) = args.from {
        reader.seek(from)?;
    }
    Ok(reader)
}

/// The JSONL backend; future formats get a separate backend and a new match arm in `run`.
fn jsonl(reader: &mut Reader, count: u64, out: &mut dyn Write) -> Result<()> {
    for _ in 0..count {
        match reader.try_next()? {
            Some(record) => output::record(out, &record)?,
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

fn reject_alias(dir: &Path, dev: u64, ino: u64) -> Result<()> {
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).map_err(|e| output::io_error("list journal entries", e))? {
            let entry = entry.map_err(|e| output::io_error("read journal entry", e))?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|e| output::io_error("stat journal entry", e))?;
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

#[cfg(test)]
mod tests {
    use super::*;
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
                output: "-".into(),
                from: None,
                to: None,
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
