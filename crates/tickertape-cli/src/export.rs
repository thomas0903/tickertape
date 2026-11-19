use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::{DirEntryExt, MetadataExt};
use std::path::Path;

use tickertape::{Error, Reader, ReaderConfig};

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
    // The writing pass starts at the same record, not at the oldest segment when it opens: retention may remove the
    // oldest segment between the passes.
    let start = snapshot.position();
    let (count, terminal) = capture(&mut snapshot, args.to, end_seq);
    // Open the input before touching the destination, and reject output paths that alias journal files.
    let mut reader = Reader::open(&args.dir, config(&args))?;
    match reader.seek(start) {
        Ok(()) => {}
        // Nothing to export, so nothing was lost.
        Err(Error::SeqUnavailable { .. }) if count == 0 => {}
        Err(Error::SeqUnavailable { first, .. }) => return Err(removed(start, first)),
        Err(error) => return Err(error.into()),
    }
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
    if let Some(failure) = terminal {
        return Err(failure);
    }
    Ok(())
}

fn config(args: &ExportArgs) -> ReaderConfig {
    ReaderConfig::default()
        .durable_only(args.durable_only)
        .notify(false)
}

fn open(args: &ExportArgs) -> Result<Reader> {
    let mut reader = Reader::open(&args.dir, config(args))?;
    if let Some(from) = args.from {
        reader.seek(from)?;
    }
    Ok(reader)
}

/// Continues a pass after [`Error::Discarded`] at `seq` (recovery or retention replaced or removed the segment the
/// reader holds): at `seq` on the current chain, or, if retention or archiving removed it, an error naming the records
/// removed. A pass that has read a segment retention then removed loses nothing; one that meets removed records it
/// hadn't read stops, so a partial export is never presented as complete.
pub(crate) fn resume(reader: &mut Reader, seq: u64) -> Result<()> {
    match reader.seek(seq) {
        Ok(()) => Ok(()),
        Err(Error::SeqUnavailable { first, .. }) => Err(removed(seq, first)),
        Err(error) => Err(error.into()),
    }
}

/// Records `seq` to `first - 1` (`first`, the oldest sequence number left, is greater than `seq`) are gone.
fn removed(seq: u64, first: u64) -> Failure {
    let records = if first - 1 == seq {
        format!(
            "record {seq} was removed from the journal (by retention or archiving) before export read it"
        )
    } else {
        format!(
            "records {seq} to {} were removed from the journal (by retention or archiving) before export read them",
            first - 1
        )
    };
    Failure::usage(format!("export stopped at sequence {seq}: {records}"))
}

/// Capture a prefix without reading past the exclusive startup checkpoint boundary.
fn capture(reader: &mut Reader, to: Option<u64>, end_seq: Option<u64>) -> (u64, Option<Failure>) {
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
            Err(Error::Discarded { seq }) => {
                if let Err(failure) = resume(reader, seq) {
                    break Some(failure);
                }
            }
            Err(error) => break Some(error.into()),
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
    let mut written = 0;
    while written < count {
        match reader.try_next() {
            Ok(Some(record)) => {
                output::export_record(out, &record, decoder)?;
                written += 1;
            }
            Ok(None) => return Err(changed()),
            Err(Error::Discarded { seq }) => resume(reader, seq)?,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// The writing pass found fewer records than the startup scan counted.
pub(crate) fn changed() -> Failure {
    Failure::usage("journal changed during export: captured records are no longer available")
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
/// failed. Everywhere else (segments, the checkpoint marker, `quarantine/`) the check fails closed, as in v0.4.0: a
/// directory that can't be listed is an error, so evidence in an unreadable `quarantine/` is never truncated. An
/// entry that vanishes between the listing and its `stat` (retention removing or archiving a segment) is skipped,
/// unless the inode number in its directory entry is the output's: a segment that archiving moves out of the
/// directory during the walk may be the output's other name.
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
            #[cfg(test)]
            tests::before_stat(&entry.path());
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(e) if tolerant && e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && entry.ino() != ino => {
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(Failure::usage("export output aliases a journal file"));
                }
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

    use std::cell::RefCell;
    use std::rc::Rc;
    use tickertape::fault::Point;
    use tickertape::retention::{self, Action, Policy};

    type StatHook = Box<dyn FnMut(&Path)>;

    thread_local! {
        static BEFORE_STAT: RefCell<Option<StatHook>> = const { RefCell::new(None) };
    }

    /// Test seam: runs in `reject_alias` after it reads a directory entry and before it stats it.
    pub(super) fn before_stat(path: &Path) {
        let hook = BEFORE_STAT.with(|h| h.borrow_mut().take());
        if let Some(mut hook) = hook {
            hook(path);
            BEFORE_STAT.with(|h| {
                h.borrow_mut().get_or_insert(hook);
            });
        }
    }

    fn append_large(writer: &mut Writer) {
        writer
            .append(&NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload: &[7; 40_000],
            })
            .unwrap();
    }

    /// Records 0..=5, one per segment, and a checkpoint in the newest segment: retention may remove segments 0 to 4.
    fn retained_journal() -> (tempfile::TempDir, Writer) {
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            Writer::open(dir.path(), WriterConfig::default().segment_size(65536)).unwrap();
        for _ in 0..6 {
            append_large(&mut writer);
        }
        writer.checkpoint().unwrap();
        (dir, writer)
    }

    /// Deletes the oldest segments, keeping `keep`.
    fn retain(dir: &Path, keep: usize) {
        let plan = retention::plan(dir, &Policy::default().max_segments(keep)).unwrap();
        retention::apply(dir, &plan, &Action::Delete).unwrap();
    }

    fn at(point: Point, skip: usize, action: impl FnOnce() + 'static) {
        tickertape::fault::clear();
        tickertape::fault::run_at(point, skip, action);
    }

    fn seqs(jsonl: &[u8]) -> Vec<u64> {
        String::from_utf8_lossy(jsonl)
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["seq"]
                    .as_u64()
                    .unwrap()
            })
            .collect()
    }

    fn export_args(dir: &Path) -> ExportArgs {
        ExportArgs {
            dir: dir.into(),
            format: ExportFormat::Jsonl,
            decode: None,
            output: "-".into(),
            from: None,
            to: None,
            durable_only: false,
        }
    }

    /// A segment that retention removes between the listing and the `stat` of its entry is not an alias and not an
    /// error. But an entry that is the output (a hard link) and moves out of the journal during the walk, as archiving
    /// moves segments, is still refused: the inode number in its directory entry matches.
    #[test]
    fn alias_walk_skips_an_entry_removed_during_the_walk() {
        let (dir, _writer) = retained_journal();
        let out = tempfile::tempdir().unwrap();
        let output = out.path().join("out.jsonl");
        fs::write(&output, b"").unwrap();
        let meta = fs::metadata(&output).unwrap();
        let mut removed = false;
        BEFORE_STAT.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |path: &Path| {
                if !removed && path.extension().is_some_and(|e| e == "tape") {
                    fs::remove_file(path).unwrap();
                    removed = true;
                }
            }))
        });
        let result = reject_alias(dir.path(), meta.dev(), meta.ino());
        BEFORE_STAT.with(|h| h.borrow_mut().take());
        assert!(result.is_ok(), "{:?}", result.err().map(|f| f.message));

        let segment = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "tape"))
            .unwrap();
        let linked = out.path().join("linked.jsonl");
        fs::hard_link(&segment, &linked).unwrap();
        let meta = fs::metadata(&linked).unwrap();
        let archived = out.path().join("archived.tape");
        BEFORE_STAT.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |path: &Path| {
                if path == segment {
                    fs::rename(path, &archived).unwrap();
                }
            }))
        });
        let result = reject_alias(dir.path(), meta.dev(), meta.ino());
        BEFORE_STAT.with(|h| h.borrow_mut().take());
        assert_eq!(
            result.err().map(|f| f.message).as_deref(),
            Some("export output aliases a journal file")
        );
    }

    /// Retention removes the oldest segment between the startup scan and the writing pass, and the writer appends: the
    /// writing pass must start where the scan started (record 0), not at the new oldest segment, which would shift
    /// the window by a record. Record 0 is gone, so the export fails naming it, before writing anything.
    #[test]
    fn writing_pass_starts_where_the_startup_scan_started() {
        let (dir, writer) = retained_journal();
        let writer = Rc::new(RefCell::new(writer));
        let (path, w) = (dir.path().to_path_buf(), writer.clone());
        // The startup scan's `Reader::open` is the first pass through the hook point, the writing pass's the second.
        at(Point::ReaderOpen, 1, move || {
            retain(&path, 5);
            append_large(&mut w.borrow_mut());
        });
        let mut out = Vec::new();
        let result = run(export_args(dir.path()), &mut out);
        tickertape::fault::clear();
        assert_eq!(seqs(&out), Vec::<u64>::new());
        let failure = result.unwrap_err();
        assert_eq!(failure.code, 1);
        assert_eq!(
            failure.message,
            "export stopped at sequence 0: record 0 was removed from the journal (by retention or archiving) \
             before export read it"
        );
    }

    /// Retention removes the segment the writing pass has just read: nothing it planned to export is lost, so it
    /// continues in the next segment. If retention also removed the next segment, the records in it are gone: the
    /// export fails naming the first of them, after the records before it.
    #[test]
    fn writing_pass_continues_past_a_removed_segment_it_has_read() {
        let (dir, _writer) = retained_journal();
        let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
        let path = dir.path().to_path_buf();
        // At the seal of segment 0, after the reader checked that its segment is in place.
        at(Point::ReaderSeal, 0, move || retain(&path, 5));
        let mut out = Vec::new();
        let result = jsonl(&mut reader, 6, &mut out, None);
        tickertape::fault::clear();
        assert!(result.is_ok(), "{:?}", result.err().map(|f| f.message));
        assert_eq!(seqs(&out), [0, 1, 2, 3, 4, 5]);

        let (dir, _writer) = retained_journal();
        let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
        let path = dir.path().to_path_buf();
        at(Point::ReaderSeal, 0, move || retain(&path, 3));
        let mut out = Vec::new();
        let result = jsonl(&mut reader, 6, &mut out, None);
        tickertape::fault::clear();
        assert_eq!(seqs(&out), [0]);
        let failure = result.unwrap_err();
        assert_eq!(failure.code, 1);
        assert_eq!(
            failure.message,
            "export stopped at sequence 1: records 1 to 2 were removed from the journal (by retention or archiving) \
             before export read them"
        );
    }

    /// The same for the startup scan: a removed segment it has read doesn't end the count; removed records it hadn't
    /// read end it with the error naming the first of them.
    #[test]
    fn startup_scan_continues_past_a_removed_segment_it_has_read() {
        let (dir, _writer) = retained_journal();
        let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
        let path = dir.path().to_path_buf();
        at(Point::ReaderSeal, 0, move || retain(&path, 5));
        let (count, terminal) = capture(&mut reader, None, None);
        tickertape::fault::clear();
        assert_eq!((count, terminal.map(|e| e.to_string())), (6, None));

        let (dir, _writer) = retained_journal();
        let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
        let path = dir.path().to_path_buf();
        at(Point::ReaderSeal, 0, move || retain(&path, 4));
        let (count, terminal) = capture(&mut reader, None, None);
        tickertape::fault::clear();
        assert_eq!(
            (count, terminal.map(|e| e.to_string())),
            (
                1,
                Some(
                    "export stopped at sequence 1: record 1 was removed from the journal (by retention or \
                     archiving) before export read it"
                        .to_owned()
                )
            )
        );
    }
}
