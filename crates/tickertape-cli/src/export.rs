use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::{DirEntryExt, MetadataExt};
use std::path::Path;

use tickertape::{Error, OwnedRecord, Reader, ReaderConfig, Record};

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
    let scan = capture(&mut snapshot, args.to, end_seq);
    // Nothing to write before the failure, or the checkpoint marker changed under the scan (the records counted are a
    // shorter prefix than the one selected): report it without creating or truncating the destination.
    if scan.count == 0 || scan.marker_changed {
        if let Some(failure) = scan.terminal {
            return Err(failure);
        }
    }
    // Open the input and read the first selected record before touching the destination, and reject output paths
    // that alias journal files.
    let mut reader = Reader::open(&args.dir, config(&args))?;
    match reader.seek(start) {
        Ok(()) => {}
        // Nothing to export, so nothing was lost.
        Err(Error::SeqUnavailable { .. }) if scan.count == 0 => {}
        Err(Error::SeqUnavailable { first, .. }) => return Err(removed(start, first)),
        Err(error) => return Err(error.into()),
    }
    let mut pass = Pass::new(&mut reader, &scan)?;
    match args.format {
        ExportFormat::Jsonl => {
            if args.output == Path::new("-") {
                jsonl(&mut pass, stdout, args.decode)?;
            } else {
                let file = destination(&args.dir, &args.output)?;
                let mut out = BufWriter::new(file);
                let result = jsonl(&mut pass, &mut out, args.decode);
                out.flush()
                    .map_err(|e| output::io_error("flush export file", e))?;
                result?;
            }
        }
        ExportFormat::Parquet => {
            let file = destination(&args.dir, &args.output)?;
            parquet_export::write(&mut pass, file, args.decode)?;
        }
    }
    if let Some(failure) = scan.terminal {
        return Err(failure);
    }
    Ok(())
}

fn config(args: &ExportArgs) -> ReaderConfig {
    ReaderConfig::default()
        .durable_only(args.durable_only)
        .seek_index(args.seek_index)
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
/// hadn't read stops, so a partial export is not presented as complete. Records that recovery replaced are caught by
/// the fingerprint ([`Pass::each`]).
fn resume(reader: &mut Reader, seq: u64) -> Result<()> {
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

/// What the startup scan selected: `count` records from its start, their fingerprint, and the failure that ended it.
struct Scan {
    count: u64,
    fingerprint: u64,
    terminal: Option<Failure>,
    /// The scan ended below the frozen checkpoint bound (`terminal` says so): nothing is written, whatever was counted.
    marker_changed: bool,
}

/// A running hash of the selected records (sequence number, schema id, both timestamps, payload length and bytes), in
/// order. The writing pass compares its own with the scan's: the same count of different records (recovery replaced
/// records the scan counted, and the writer wrote others with the same sequence numbers) is a changed journal.
///
/// It detects records that differ by accident, not by design: anyone who can write the journal's files can change what
/// export reads anyway. Four lanes, each updated with the multiply-rotate round of xxHash64, take the records' words
/// in turn, so four words are mixed at a time; they are combined at the end. It is written here because no dependency
/// provides one, and `std`'s SipHash, one word at a time, measurably slowed both passes.
struct Fingerprint([u64; 4]);

impl Default for Fingerprint {
    fn default() -> Self {
        Self([Self::P1, Self::P2, !Self::P1, !Self::P2])
    }
}

impl Fingerprint {
    const P1: u64 = 0x9E37_79B1_85EB_CA87;
    const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;

    fn round(acc: u64, input: u64) -> u64 {
        acc.wrapping_add(input.wrapping_mul(Self::P2))
            .rotate_left(31)
            .wrapping_mul(Self::P1)
    }

    fn add(&mut self, record: &Record<'_>) {
        let word = |bytes: &[u8]| {
            let mut word = [0; 8];
            word[..bytes.len()].copy_from_slice(bytes);
            u64::from_le_bytes(word)
        };
        let [a, b, c, d] = &mut self.0;
        // Each record starts with the same lane. The payload length comes before the payload, so the zero padding of
        // its last word is unambiguous. A payload is shorter than 4 GiB (a frame's length is a u32).
        *a = Self::round(*a, record.seq);
        *b = Self::round(
            *b,
            u64::from(record.schema_id) | (record.payload.len() as u64) << 32,
        );
        *c = Self::round(*c, record.venue_ts_ns);
        *d = Self::round(*d, record.local_ts_ns);
        let mut blocks = record.payload.chunks_exact(32);
        for block in &mut blocks {
            for (lane, bytes) in self.0.iter_mut().zip(block.chunks_exact(8)) {
                *lane = Self::round(*lane, word(bytes));
            }
        }
        for (lane, bytes) in self.0.iter_mut().zip(blocks.remainder().chunks(8)) {
            *lane = Self::round(*lane, word(bytes));
        }
    }

    fn finish(&self) -> u64 {
        self.0.into_iter().fold(0, Self::round)
    }
}

/// Counts the selected records, without reading past the exclusive startup checkpoint boundary or past `--to`.
/// A visible scan stops at the first empty slot it sees.
fn capture(reader: &mut Reader, to: Option<u64>, end_seq: Option<u64>) -> Scan {
    let mut count = 0;
    let mut fingerprint = Fingerprint::default();
    let mut marker_changed = false;
    let terminal = loop {
        // Check the bounds before reading: a read past them could fail (retention) although nothing is selected.
        let position = reader.position();
        if end_seq.is_some_and(|end| position >= end) || to.is_some_and(|to| position > to) {
            break None;
        }
        match reader.try_next() {
            Ok(Some(record)) if to.is_some_and(|to| record.seq > to) => break None,
            Ok(Some(record)) => {
                count += 1;
                fingerprint.add(&record);
                if to == Some(record.seq) {
                    break None;
                }
            }
            // Below the frozen checkpoint bound: the marker no longer covers records it covered at startup.
            Ok(None) if end_seq.is_some() => {
                marker_changed = true;
                break Some(changed(CHECKPOINT_CHANGED));
            }
            Ok(None) => break None,
            Err(Error::Discarded { seq, .. }) => {
                if let Err(failure) = resume(reader, seq) {
                    break Some(failure);
                }
            }
            Err(error) => break Some(error.into()),
        }
    };
    Scan {
        count,
        fingerprint: fingerprint.finish(),
        terminal,
        marker_changed,
    }
}

/// The writing pass: emits the records the startup scan counted, from the record it started at.
pub(crate) struct Pass<'r> {
    reader: &'r mut Reader,
    first: Option<OwnedRecord>,
    count: u64,
    fingerprint: u64,
}

impl<'r> Pass<'r> {
    /// Reads the first selected record, so the export fails before the destination is opened if it is gone.
    fn new(reader: &'r mut Reader, scan: &Scan) -> Result<Self> {
        let mut first = None;
        while scan.count > 0 && first.is_none() {
            match reader.next_owned() {
                Ok(record) => first = Some(record.ok_or_else(|| changed(MISSING))?),
                Err(Error::Discarded { seq, .. }) => resume(reader, seq)?,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(Self {
            reader,
            first,
            count: scan.count,
            fingerprint: scan.fingerprint,
        })
    }

    /// The number of records the pass emits.
    pub(crate) fn count(&self) -> u64 {
        self.count
    }

    /// Passes each selected record to `emit`, in order. Fails after the last one if they are not the records the
    /// scan counted.
    pub(crate) fn each(&mut self, mut emit: impl FnMut(&Record<'_>) -> Result<()>) -> Result<()> {
        let mut fingerprint = Fingerprint::default();
        let mut written = 0;
        if let Some(first) = self.first.take() {
            let record = Record {
                seq: first.seq,
                schema_id: first.schema_id,
                venue_ts_ns: first.venue_ts_ns,
                local_ts_ns: first.local_ts_ns,
                payload: &first.payload,
            };
            fingerprint.add(&record);
            emit(&record)?;
            written = 1;
        }
        while written < self.count {
            match self.reader.try_next() {
                Ok(Some(record)) => {
                    fingerprint.add(&record);
                    emit(&record)?;
                    written += 1;
                }
                Ok(None) => return Err(changed(MISSING)),
                Err(Error::Discarded { seq, .. }) => resume(self.reader, seq)?,
                Err(error) => return Err(error.into()),
            }
        }
        if fingerprint.finish() != self.fingerprint {
            return Err(changed(DIFFERENT));
        }
        Ok(())
    }
}

/// Emit the captured records, with optional payload decoding.
fn jsonl(pass: &mut Pass<'_>, out: &mut dyn Write, decoder: Option<Decoder>) -> Result<()> {
    pass.each(|record| output::export_record(out, record, decoder))
}

/// The writing pass found fewer records than the startup scan counted.
const MISSING: &str = "captured records are no longer available";
/// A durable-only startup scan found the checkpoint marker covering less than it did when export started.
const CHECKPOINT_CHANGED: &str =
    "the checkpoint marker no longer covers records it covered when export started";
/// The writing pass read as many records as the startup scan counted, but different ones.
const DIFFERENT: &str = "the records read differ from those the startup scan counted; the output is not a consistent export";

fn changed(reason: &str) -> Failure {
    Failure::usage(format!("journal changed during export: {reason}"))
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
/// unless the inode number in its directory entry is the output's: a segment that archiving moves out of the directory
/// during the walk may be the output's other name. The device can't be compared for an entry that is gone (its
/// directory's device is not always its own: overlayfs), so an unrelated output on another filesystem that happens to
/// have a vanished entry's inode number is refused too; running the export again succeeds. The seek index directory
/// `seek-index/` is not descended into: it holds no journal data (ADR 0045), so one that is unreadable or changing does
/// not fail an export, as it can in 1.0.0.
fn reject_alias(dir: &Path, dev: u64, ino: u64) -> Result<()> {
    let readers = dir.join(tickertape::status::READERS_DIR);
    let indexes = dir.join(tickertape::seek_index::DIR_NAME);
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
            // Seek indexes can be rebuilt, and come and go while the writer and `tickertape index` run.
            if metadata.is_dir() && entry.path() != indexes {
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
pub(crate) mod tests {
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
        let scan = capture(&mut reader, None, Some(end));
        assert_eq!(scan.count, 1);
        assert!(scan.terminal.is_none());
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
                seek_index: false,
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

    /// A writing pass over `count` records from `reader`'s position in `dir`, expecting the records there now.
    pub(crate) fn writing_pass<'r>(dir: &Path, reader: &'r mut Reader, count: u64) -> Pass<'r> {
        let start = reader.position();
        let mut scan = Reader::open(dir, ReaderConfig::default()).unwrap();
        scan.seek(start).unwrap();
        let scan = Scan {
            count,
            ..capture(&mut scan, Some(start + count - 1), None)
        };
        Pass::new(reader, &scan).unwrap()
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
            seek_index: false,
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
        let mut pass = writing_pass(dir.path(), &mut reader, 6);
        let path = dir.path().to_path_buf();
        // At the seal of segment 0, after the reader checked that its segment is in place.
        at(Point::ReaderSeal, 0, move || retain(&path, 5));
        let mut out = Vec::new();
        let result = jsonl(&mut pass, &mut out, None);
        tickertape::fault::clear();
        assert!(result.is_ok(), "{:?}", result.err().map(|f| f.message));
        assert_eq!(seqs(&out), [0, 1, 2, 3, 4, 5]);

        let (dir, _writer) = retained_journal();
        let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
        let mut pass = writing_pass(dir.path(), &mut reader, 6);
        let path = dir.path().to_path_buf();
        at(Point::ReaderSeal, 0, move || retain(&path, 3));
        let mut out = Vec::new();
        let result = jsonl(&mut pass, &mut out, None);
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
        let scan = capture(&mut reader, None, None);
        tickertape::fault::clear();
        assert_eq!(
            (scan.count, scan.terminal.map(|e| e.to_string())),
            (6, None)
        );

        let (dir, _writer) = retained_journal();
        let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
        let path = dir.path().to_path_buf();
        at(Point::ReaderSeal, 0, move || retain(&path, 4));
        let scan = capture(&mut reader, None, None);
        tickertape::fault::clear();
        assert_eq!(
            (scan.count, scan.terminal.map(|e| e.to_string())),
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

    fn file_args(dir: &Path, format: ExportFormat, output: &Path) -> ExportArgs {
        ExportArgs {
            output: output.into(),
            format,
            durable_only: true,
            seek_index: false,
            ..export_args(dir)
        }
    }

    /// Runs a durable-only export of `dir` into an existing file holding `keep`, in each format, with `inject` armed
    /// before each run, and returns each result with the file's contents afterwards.
    fn export_into_kept_file(
        inject: impl Fn(&Path),
    ) -> Vec<(ExportFormat, std::result::Result<(), Failure>, Vec<u8>)> {
        [ExportFormat::Jsonl, ExportFormat::Parquet]
            .into_iter()
            .map(|format| {
                let (dir, _writer) = retained_journal();
                let out = tempfile::tempdir().unwrap();
                let output = out.path().join("out");
                fs::write(&output, b"keep").unwrap();
                inject(dir.path());
                let result = run(file_args(dir.path(), format, &output), &mut Vec::new());
                tickertape::fault::clear();
                (format, result, fs::read(&output).unwrap())
            })
            .collect()
    }

    /// Review round 1, item 1 (a): retention overtakes the startup scan before it counts a record (a durable-only
    /// reader finds its segment gone after reading the checkpoint marker). The export fails with the removal, before
    /// it creates or truncates the destination.
    #[test]
    fn failed_scan_with_nothing_counted_leaves_the_destination_alone() {
        for (format, result, contents) in export_into_kept_file(|dir| {
            let path = dir.to_path_buf();
            at(Point::ReaderCheckpointIdentity, 0, move || retain(&path, 5));
        }) {
            let failure = result.unwrap_err();
            assert_eq!(
                (failure.code, failure.message.as_str()),
                (
                    1,
                    "export stopped at sequence 0: record 0 was removed from the journal (by retention or \
                     archiving) before export read it"
                ),
                "{format:?}"
            );
            assert_eq!(contents, b"keep", "{format:?}");
        }
    }

    /// Review round 1, item 1 (b): with `--durable-only`, the writing pass's `seek` succeeds and retention then
    /// removes the segment before the pass reads its first record (the reader notices after reading the checkpoint
    /// marker). The first record is read before the destination is opened, so the export fails without touching it.
    #[test]
    fn first_record_removed_after_the_seek_leaves_the_destination_alone() {
        for (format, result, contents) in export_into_kept_file(|dir| {
            let path = dir.to_path_buf();
            // The startup scan passes the hook point once (its first read); the writing pass's first read is next.
            at(Point::ReaderCheckpointIdentity, 1, move || retain(&path, 5));
        }) {
            let failure = result.unwrap_err();
            assert_eq!(
                (failure.code, failure.message.as_str()),
                (
                    1,
                    "export stopped at sequence 0: record 0 was removed from the journal (by retention or \
                     archiving) before export read it"
                ),
                "{format:?}"
            );
            assert_eq!(contents, b"keep", "{format:?}");
        }
    }

    /// Review round 1, item 3 (b): the checkpoint marker becomes invalid after export froze its bound and before the
    /// durable-only startup scan read a record, so the reader's own refresh covers nothing and the scan ends early.
    /// That is a changed journal, not an empty export: exit 1, destination untouched.
    #[test]
    fn durable_scan_ending_below_the_frozen_bound_is_a_change() {
        for (format, result, contents) in export_into_kept_file(|dir| {
            let marker = dir.join("checkpoint");
            at(Point::ReaderCheckpoint, 0, move || {
                let len = fs::metadata(&marker).unwrap().len();
                fs::write(&marker, vec![0; len as usize]).unwrap();
            });
        }) {
            let failure = result.unwrap_err();
            assert_eq!(failure.code, 1, "{format:?}");
            assert!(
                failure
                    .message
                    .starts_with("journal changed during export: "),
                "{format:?}: {}",
                failure.message
            );
            assert_eq!(contents, b"keep", "{format:?}");
        }
    }

    /// The newer checkpoint slot becomes invalid after export froze its bound, so the durable-only scan falls back to the
    /// older slot and counts a shorter prefix before it ends below the bound. The destination is not touched.
    #[test]
    fn durable_scan_cut_short_after_counting_records_leaves_the_destination_alone() {
        for format in [ExportFormat::Jsonl, ExportFormat::Parquet] {
            let dir = tempfile::tempdir().unwrap();
            let mut writer =
                Writer::open(dir.path(), WriterConfig::default().segment_size(65536)).unwrap();
            for _ in 0..2 {
                for _ in 0..3 {
                    append_large(&mut writer);
                }
                writer.checkpoint().unwrap();
            }
            let out = tempfile::tempdir().unwrap();
            let output = out.path().join("out");
            fs::write(&output, b"keep").unwrap();
            // The second checkpoint (generation 2) is in slot 0.
            let marker = dir.path().join("checkpoint");
            at(Point::ReaderCheckpoint, 0, move || {
                use std::os::unix::fs::FileExt;
                let file = OpenOptions::new().write(true).open(&marker).unwrap();
                file.write_all_at(
                    &[0; tickertape::format::CHECKPOINT_SLOT_LEN],
                    tickertape::format::CHECKPOINT_SLOT_OFFSETS[0] as u64,
                )
                .unwrap();
            });
            let result = run(file_args(dir.path(), format, &output), &mut Vec::new());
            tickertape::fault::clear();
            let failure = result.unwrap_err();
            assert_eq!(
                (failure.code, failure.message.as_str()),
                (
                    1,
                    "journal changed during export: the checkpoint marker no longer covers records it covered when \
                     export started"
                ),
                "{format:?}"
            );
            assert_eq!(fs::read(&output).unwrap(), b"keep", "{format:?}");
        }
    }

    /// Review round 1, item 3 (a): `--to` is below the oldest record. Retention removes the segment while the startup
    /// scan would read it; the selection is still empty, so the export is empty with exit 0, not a failure naming
    /// records outside the range.
    #[test]
    fn range_below_the_oldest_record_stays_empty_under_retention() {
        let (dir, _writer) = retained_journal();
        retain(dir.path(), 5); // the oldest record is now 1
        let path = dir.path().to_path_buf();
        at(Point::ReaderCheckpointIdentity, 0, move || retain(&path, 4));
        let mut out = Vec::new();
        let result = run(
            ExportArgs {
                to: Some(0),
                durable_only: true,
                seek_index: false,
                ..export_args(dir.path())
            },
            &mut out,
        );
        tickertape::fault::clear();
        assert!(result.is_ok(), "{:?}", result.err().map(|f| f.message));
        assert!(out.is_empty());
    }

    fn append_small(writer: &mut Writer, payload: u8) {
        writer
            .append(&NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload: &[payload; 32],
            })
            .unwrap();
    }

    /// Records 0..=5 in one segment, none covered by a checkpoint. The returned hook, run between the startup scan
    /// and the writing pass, damages record 3 on storage, lets the next writer's recovery replace the segment (keeping
    /// records 0 to 2, crash contract C8/C11) and appends three different records with sequence numbers 3 to 5.
    pub(crate) fn journal_with_replaced_suffix() -> (tempfile::TempDir, impl FnOnce() + 'static) {
        use std::os::unix::fs::FileExt;
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), WriterConfig::default()).unwrap();
        for _ in 0..6 {
            append_small(&mut writer, 7);
        }
        let path = dir.path().to_path_buf();
        let replace = move || {
            drop(writer);
            let segment = path.join(tickertape::format::segment_file_name(0));
            let frame = tickertape::format::frame_len(32).unwrap() as u64;
            let at = tickertape::format::SEGMENT_HEADER_LEN as u64
                + 3 * frame
                + tickertape::format::RECORD_HEADER_LEN as u64;
            let file = OpenOptions::new().write(true).open(&segment).unwrap();
            file.write_all_at(&[0xFF], at).unwrap();
            let mut writer = Writer::open(&path, WriterConfig::default()).unwrap();
            for _ in 0..3 {
                append_small(&mut writer, 9);
            }
        };
        (dir, replace)
    }

    /// Review round 1, item 2: recovery replaces records the startup scan counted and the writer appends records with
    /// the same sequence numbers before the writing pass. The count matches, the records don't: exit 1 with "journal
    /// changed during export", and the output holds what the writing pass read.
    #[test]
    fn writing_pass_detects_a_different_history() {
        for to_file in [false, true] {
            let (dir, replace) = journal_with_replaced_suffix();
            at(Point::ReaderOpen, 1, replace);
            let out_dir = tempfile::tempdir().unwrap();
            let output = out_dir.path().join("out.jsonl");
            let mut stdout = Vec::new();
            let args = ExportArgs {
                output: if to_file { output.clone() } else { "-".into() },
                ..export_args(dir.path())
            };
            let result = run(args, &mut stdout);
            tickertape::fault::clear();
            let jsonl = if to_file {
                fs::read(&output).unwrap()
            } else {
                stdout
            };
            assert_eq!(seqs(&jsonl), [0, 1, 2, 3, 4, 5]);
            let failure = result.unwrap_err();
            assert_eq!(failure.code, 1);
            assert!(
                failure
                    .message
                    .starts_with("journal changed during export: ")
                    && failure.message.contains("not a consistent export"),
                "{}",
                failure.message
            );
        }
    }

    /// Review round 1, item 4: an entry that vanishes during the walk is refused only if its inode number is the
    /// output's on the output's device. The same inode number on another device (another filesystem) is not the
    /// output and is skipped.
    #[test]
    fn alias_walk_refuses_a_vanished_entry_with_the_outputs_inode_number() {
        let (dir, _writer) = retained_journal();
        let out = tempfile::tempdir().unwrap();
        let segment = dir.path().join(tickertape::format::segment_file_name(0));
        let linked = out.path().join("linked.jsonl");
        fs::hard_link(&segment, &linked).unwrap();
        let meta = fs::metadata(&linked).unwrap();
        let archived = out.path().join("archived.tape");
        for (dev, refused) in [(meta.dev() ^ 1, true), (meta.dev(), true)] {
            if !segment.exists() {
                fs::rename(&archived, &segment).unwrap();
            }
            // Archiving moves the segment out of the journal between the listing and the `stat` of its entry.
            let (watched, away) = (segment.clone(), archived.clone());
            BEFORE_STAT.with(|h| {
                *h.borrow_mut() = Some(Box::new(move |path: &Path| {
                    if path == watched {
                        fs::rename(path, &away).unwrap();
                    }
                }))
            });
            let result = reject_alias(dir.path(), dev, meta.ino());
            BEFORE_STAT.with(|h| h.borrow_mut().take());
            assert!(!segment.exists());
            assert_eq!(
                result.err().map(|f| f.message),
                refused.then(|| "export output aliases a journal file".to_owned()),
                "device {dev}"
            );
        }
    }
}
