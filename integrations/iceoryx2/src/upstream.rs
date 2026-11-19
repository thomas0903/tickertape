//! Conversion to and from iceoryx2's own recording files (feature `dynamic`), the files `iox2 service record`
//! writes and `iox2 service replay` plays.
//!
//! An iceoryx2 recording file holds one service: a header (iceoryx2 version, service name, payload, user header and
//! system header type details) and, per message, a timestamp in whole milliseconds since the recording started,
//! the system header, the user header and the payload. It comes in two representations: human-readable (a TOML
//! header and hex lines, the default of both tools) and `iox2dump` (raw bytes).
//!
//! Only the human-readable representation is supported, in both directions, with a writer and a parser of this
//! crate's own. [`export`] writes it with checked, complete writes (upstream's writer issues one `write` call per
//! piece and treats a short write as complete, so a full volume could leave a truncated file reported as written).
//! [`import`] parses it for untrusted input: upstream's readers are not meant for that (the text reader panics when
//! a multi-byte character crosses a fixed byte index; the `iox2dump` reader treats file bytes as an initialised Rust
//! struct and allocates from unvalidated lengths). `iox2dump` files are neither written nor read: upstream's writer
//! serialises the memory image of its header struct, including padding and unused string capacity it leaves
//! uninitialised, and has the same short-write behaviour (ADR 0030, amendment of review round 3).
//!
//! # What each direction loses
//!
//! **Journal to iceoryx2 file ([`export`]):**
//! - Only one service per file: choose it when the journal has several.
//! - Absolute time: only the offset from the first exported message remains, truncated to whole milliseconds.
//!   Timestamps that go backwards (the wall clock stepped back while recording) are raised to the previous one,
//!   because the format requires them to be non-decreasing; [`ExportReport::clamped`] counts them.
//! - Recording-run boundaries, tickertape sequence numbers, publisher ids, `venue_ts_ns`, loss records and the
//!   service settings in the descriptors (history, buffer sizes, overflow).
//! - The iceoryx2 version in the file header is the version this crate is built with, not the recording side's.
//! - The system header (iceoryx2's per-sample header, with publisher and node ids) is not recorded by tickertape;
//!   the file gets zeros of the right size. `iox2 service replay` ignores it.
//!
//! **iceoryx2 file to journal ([`import`]):**
//! - Absolute time was never in the file: `local_ts_ns` is a chosen start time (the import time by default) plus
//!   each record's offset in milliseconds. The descriptor's `imported` flag says so.
//! - The system header bytes are dropped, and with them the publisher ids (stored as 0, unknown).
//! - Service settings are unknown: the descriptor's settings are 0 (safe overflow is set, iceoryx2's default), and
//!   a replay creates the service with iceoryx2's defaults.
//!
//! Message bytes (user header and payload) and order survive both directions unchanged.
//!
//! # Import is all or nothing
//!
//! The whole file is read into memory and validated, with all its decoded messages held in memory too, before
//! anything is appended: the header's version, messaging pattern, service name and type details
//! ([`layout::validate_types`], and iceoryx2's own name checks), every record's lengths against them, non-decreasing
//! timestamps that stay representable once added to the start time, and that every line of every record, and the
//! blank line after it, ends with a newline, as upstream writes them (a file cut anywhere is refused). A file that
//! fails any check leaves the journal untouched. Only a journal error while appending (a full volume) can leave part
//! of an import in the journal, as for any writer.
//!
//! Imported bytes are not checked to be valid values of their types: replaying an import publishes them under the
//! recorded type names (see [`Replayer::run_dynamic`](crate::replayer::Replayer::run_dynamic)). Import only files you
//! trust.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use tickertape::{NewRecord, Reader, ReaderConfig, WriterConfig};

use crate::dynamic::{system_header_detail, type_detail};
use crate::layout::{
    self, DataRecord, Decoded, Descriptor, MAX_SERVICE_NAME_LEN, SCHEMA_DATA, ServiceSettings,
    TypeInfo, TypeVariant, Version,
};
use crate::placement::Journal;
use crate::recorder::service_name;
use crate::{Catalog, Error};

/// The iceoryx2 version this crate is built with (the exact pin in `Cargo.toml`), written into exported files as
/// upstream's recorder writes its own.
const ICEORYX2_VERSION: Version = Version {
    major: 0,
    minor: 10,
    patch: 0,
};

/// What to export.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ExportOptions {
    /// The service to export; required when the range holds several.
    pub service: Option<String>,
    /// First sequence number, inclusive.
    pub from: Option<u64>,
    /// Last sequence number, inclusive.
    pub to: Option<u64>,
}

impl ExportOptions {
    /// Exports `service`.
    pub fn service(mut self, name: impl Into<String>) -> Self {
        self.service = Some(name.into());
        self
    }

    /// Starts at `seq`.
    pub fn from(mut self, seq: u64) -> Self {
        self.from = Some(seq);
        self
    }

    /// Stops after `seq`.
    pub fn to(mut self, seq: u64) -> Self {
        self.to = Some(seq);
        self
    }
}

/// The results of [`export`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportReport {
    /// The exported service.
    pub service: String,
    /// Messages written.
    pub messages: u64,
    /// First exported sequence number.
    pub first_seq: Option<u64>,
    /// Last exported sequence number.
    pub last_seq: Option<u64>,
    /// Recording runs the messages came from (their boundaries are lost).
    pub runs: u64,
    /// Messages whose timestamp went backwards and was raised to the previous one.
    pub clamped: u64,
    /// Data and loss records skipped because the descriptor they name is no longer in the journal.
    pub undescribed: u64,
}

/// Writes the messages of one service in the journal at `journal` to a new human-readable iceoryx2 recording file at
/// `output`.
///
/// Reads through the end of the journal as it is when the export gets there. The file is written, completely and
/// synced, under a temporary name of its own next to `output` (created exclusively; concurrent exports never share
/// one) and linked to `output` only when complete, so a failed export leaves no partial file and an existing file
/// is never replaced; the directory is then synced, so the name survives a power loss once `export` returns. The
/// directory is opened for that sync before anything is written, so a directory that can't be opened (write and
/// search permission without read permission) fails the export before the file exists, and a retry can succeed.
///
/// # Errors
///
/// [`Error::Usage`] if the range holds several services and none was chosen, the chosen service isn't there, or
/// its types changed between recording runs; [`Error::Upstream`] if the file exists; [`Error::Io`] if it can't be
/// written; journal and layout errors as for [`info::scan`](crate::info::scan).
pub fn export(
    journal: impl AsRef<Path>,
    output: impl AsRef<Path>,
    options: &ExportOptions,
) -> Result<ExportReport, Error> {
    let journal = journal.as_ref();
    let output = output.as_ref();
    if output.symlink_metadata().is_ok() {
        return Err(Error::Upstream(format!(
            "{} already exists",
            output.display()
        )));
    }
    let service = match &options.service {
        Some(s) => s.clone(),
        None => {
            let names = services_in(journal, options)?;
            match names.len() {
                1 => names.into_iter().next().unwrap_or_default(),
                0 => return Err(Error::Usage("no recorded messages in the range".into())),
                _ => {
                    return Err(Error::Usage(format!(
                        "the range holds several services ({}); choose one, since an iceoryx2 recording file holds only one",
                        names.into_iter().collect::<Vec<_>>().join(", ")
                    )));
                }
            }
        }
    };
    // The directory is opened for syncing before anything is created in it, so that an export that can't make its
    // file durable fails before the file is published, not after (a directory with write and search permission but
    // no read permission takes the link, but can't be opened).
    let dir = match output.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let dir_handle = std::fs::File::open(dir).map_err(|source| Error::Io {
        op: "open directory (to sync it)",
        path: dir.to_path_buf(),
        source,
    })?;
    let (partial, file) = create_partial(output)?;
    let io = |op: &'static str| {
        let path = partial.clone();
        move |source| Error::Io { op, path, source }
    };
    let result = (|| {
        let mut w = std::io::BufWriter::new(file);
        let report = export_to(journal, &mut w, &service, options)?;
        let file = w.into_inner().map_err(|e| io("write")(e.into_error()))?;
        file.sync_all().map_err(io("sync"))?;
        drop(file);
        // A hard link fails if `output` exists, so an existing file is never replaced.
        std::fs::hard_link(&partial, output).map_err(|source| Error::Io {
            op: "create",
            path: output.to_path_buf(),
            source,
        })?;
        // Then the temporary name goes, and the directory is synced so that the new name (and the removal) survive
        // a power loss; before that, a power loss may lose the name, never leave a partial file under it.
        let _ = std::fs::remove_file(&partial);
        // Only an I/O error of the file system can fail this now; the file is then complete under its name, but
        // the name may not survive a power loss, which the error says.
        dir_handle.sync_all().map_err(|source| Error::Io {
            op: "sync directory (the exported file is complete, but its name may not survive a power loss)",
            path: dir.to_path_buf(),
            source,
        })?;
        Ok(report)
    })();
    // Only this export's own temporary file is removed: its name is unique and it was created exclusively.
    let _ = std::fs::remove_file(&partial);
    result
}

/// The name of an export's temporary file: a short fixed prefix, the process id, a per-process counter and the
/// time, never the destination's name, so that it fits a file name (at most 255 bytes) whatever the destination's
/// length: at most 66 bytes.
fn partial_name(counter: u64) -> String {
    format!(
        ".tt-iox2-export-{:x}-{counter:x}-{:x}.partial",
        std::process::id(),
        tickertape::clock::realtime_ns()
    )
}

/// Creates a new, uniquely named temporary file in `output`'s directory ([`partial_name`]), created exclusively
/// (retrying with another name if one exists), so two exports to one destination, in one process or several, never
/// write or remove each other's file.
fn create_partial(output: &Path) -> Result<(PathBuf, std::fs::File), Error> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    if output.file_name().is_none() {
        return Err(Error::Usage(format!(
            "{} is not a file name",
            output.display()
        )));
    }
    let mut last = None;
    for _ in 0..100 {
        let path = output.with_file_name(partial_name(COUNTER.fetch_add(1, Ordering::Relaxed)));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some((path, e)),
            Err(source) => {
                return Err(Error::Io {
                    op: "create",
                    path,
                    source,
                });
            }
        }
    }
    let (path, source) = last.expect("at least one attempt");
    Err(Error::Io {
        op: "create",
        path,
        source,
    })
}

/// Writes the export to `w`: the header when the service's first message is found, then each message. Every write
/// is complete or an error (`write_all`).
fn export_to(
    journal: &Path,
    w: &mut dyn Write,
    service: &str,
    options: &ExportOptions,
) -> Result<ExportReport, Error> {
    let mut reader = open_at(journal, options.from)?;
    let mut catalog = Catalog::new(journal);
    let mut first: Option<Descriptor> = None;
    let system_header = vec![0u8; system_header_detail().size()];
    let mut report = ExportReport {
        service: service.to_string(),
        messages: 0,
        first_seq: None,
        last_seq: None,
        runs: 0,
        clamped: 0,
        undescribed: 0,
    };
    let write_error = |source| Error::Io {
        op: "write",
        path: PathBuf::from("the export file"),
        source,
    };
    let mut runs = BTreeSet::new();
    let mut first_ts = 0u64;
    let mut last_offset = Duration::ZERO;
    while let Some(record) = reader.try_next()? {
        let seq = record.seq;
        if options.to.is_some_and(|to| seq > to) {
            break;
        }
        let layout_error = |source| Error::Layout { seq, source };
        let data = match layout::decode(record.schema_id, record.payload).map_err(layout_error)? {
            Decoded::Descriptor(d) => {
                catalog.insert(seq, *d);
                continue;
            }
            Decoded::Data(d) => d,
            Decoded::Loss(loss) => {
                // Not exported, but checked against its descriptor like everywhere else, and counted if that is gone.
                match catalog.resolve(seq, loss.descriptor_seq)? {
                    Some(d) => loss.check(&d).map_err(layout_error)?,
                    None => report.undescribed += 1,
                }
                continue;
            }
            Decoded::Filler(_) | Decoded::Other => continue,
        };
        let Some(d) = catalog.resolve(seq, data.descriptor_seq)? else {
            report.undescribed += 1;
            continue;
        };
        data.check(&d).map_err(layout_error)?;
        if d.service_name != service {
            continue;
        }
        runs.insert(d.run_start_seq);
        let first = match &first {
            Some(first) => first,
            None => {
                // The names must be valid for iceoryx2, as for replay.
                service_name(service)?;
                type_detail(&d.payload)?;
                type_detail(&d.user_header)?;
                text::write_header(w, service, &d.payload, &d.user_header).map_err(write_error)?;
                first_ts = record.local_ts_ns;
                first.insert((*d).clone())
            }
        };
        if first.payload != d.payload || first.user_header != d.user_header {
            return Err(Error::Usage(format!(
                "service \"{service}\" changed types between recording runs (record {seq}); export the runs separately with --from/--to"
            )));
        }
        let mut offset = Duration::from_nanos(record.local_ts_ns.saturating_sub(first_ts));
        if offset < last_offset || record.local_ts_ns < first_ts {
            offset = last_offset;
            report.clamped += 1;
        }
        last_offset = offset;
        text::write_record(
            w,
            offset.as_millis() as u64,
            &system_header,
            data.user_header,
            data.payload,
        )
        .map_err(write_error)?;
        report.messages += 1;
        report.first_seq.get_or_insert(seq);
        report.last_seq = Some(seq);
    }
    if first.is_none() {
        return Err(Error::Usage(format!(
            "no messages of service \"{service}\" in the range"
        )));
    }
    report.runs = runs.len() as u64;
    Ok(report)
}

fn services_in(journal: &Path, options: &ExportOptions) -> Result<BTreeSet<String>, Error> {
    let mut reader = open_at(journal, options.from)?;
    let mut catalog = Catalog::new(journal);
    let mut names = BTreeSet::new();
    while let Some(record) = reader.try_next()? {
        let seq = record.seq;
        if options.to.is_some_and(|to| seq > to) {
            break;
        }
        let layout_error = |source| Error::Layout { seq, source };
        match layout::decode(record.schema_id, record.payload).map_err(layout_error)? {
            Decoded::Descriptor(d) => {
                catalog.insert(seq, *d);
            }
            Decoded::Data(data) => {
                if let Some(d) = catalog.resolve(seq, data.descriptor_seq)? {
                    data.check(&d).map_err(layout_error)?;
                    names.insert(d.service_name.clone());
                }
            }
            Decoded::Loss(loss) => {
                if let Some(d) = catalog.resolve(seq, loss.descriptor_seq)? {
                    loss.check(&d).map_err(layout_error)?;
                }
            }
            Decoded::Filler(_) | Decoded::Other => {}
        }
    }
    Ok(names)
}

fn open_at(journal: &Path, from: Option<u64>) -> Result<Reader, Error> {
    let mut reader = Reader::open(journal, ReaderConfig::default())?;
    if let Some(from) = from {
        reader.seek(from)?;
    }
    Ok(reader)
}

/// How to import.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ImportOptions {
    /// The start time the file's relative timestamps are added to, nanoseconds since the Unix epoch. Default: now.
    pub start_unix_ns: Option<u64>,
    /// Settings for the journal writer.
    pub writer_config: WriterConfig,
}

impl ImportOptions {
    /// Adds the file's relative timestamps to this start time.
    pub fn start_unix_ns(mut self, ns: u64) -> Self {
        self.start_unix_ns = Some(ns);
        self
    }

    /// Settings for the journal writer.
    pub fn writer_config(mut self, config: WriterConfig) -> Self {
        self.writer_config = config;
        self
    }
}

/// The results of [`import`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportReport {
    /// The service in the file.
    pub service: String,
    /// iceoryx2 version that wrote the file.
    pub iceoryx2_version: String,
    /// Sequence number of the descriptor written.
    pub descriptor_seq: u64,
    /// Messages imported.
    pub messages: u64,
    /// The start time used, nanoseconds since the Unix epoch.
    pub start_unix_ns: u64,
    /// The journal's next sequence number after the import.
    pub next_seq: u64,
}

/// Appends the messages of a human-readable iceoryx2 recording file to the journal at `journal` (created if
/// needed), as one new recording run, then checkpoints. Its descriptor is written first and again in every segment
/// the run fills, as the recorder does ([where descriptors lie](crate::layout#where-descriptors-lie)). The file is read and validated
/// completely first (see the [module docs](self#import-is-all-or-nothing)).
///
/// # Errors
///
/// [`Error::Upstream`] if the file can't be read or breaks a rule (nothing is appended then); [`Error::Journal`] if
/// the journal can't be written (for example, a recorder is writing to it).
pub fn import(
    input: impl AsRef<Path>,
    journal: impl AsRef<Path>,
    options: &ImportOptions,
) -> Result<ImportReport, Error> {
    let input = input.as_ref();
    let bytes = std::fs::read(input).map_err(|source| Error::Io {
        op: "read",
        path: input.to_path_buf(),
        source,
    })?;
    let invalid_file = |e: String| Error::Upstream(format!("{}: {e}", input.display()));
    let file = text::parse(&bytes, options.writer_config.segment_size).map_err(invalid_file)?;
    drop(bytes);
    // The names must be valid for iceoryx2, with the same checks replay and export apply, before the journal is
    // touched.
    service_name(&file.service_name).map_err(|e| invalid_file(e.to_string()))?;
    for t in [&file.payload, &file.user_header, &file.system_header] {
        type_detail(t).map_err(|e| invalid_file(e.to_string()))?;
    }
    let start_unix_ns = options
        .start_unix_ns
        .unwrap_or_else(tickertape::clock::realtime_ns);
    // Every record's absolute time must be representable: refused, not saturated.
    let mut local_ts = Vec::with_capacity(file.records.len());
    for r in &file.records {
        let ts = r
            .timestamp_ms
            .checked_mul(1_000_000)
            .and_then(|offset| start_unix_ns.checked_add(offset))
            .ok_or_else(|| {
                invalid_file(format!(
                    "record at line {}: its timestamp of {} ms after the start time {start_unix_ns} ns overflows \
                     nanoseconds since the Unix epoch",
                    r.line, r.timestamp_ms
                ))
            })?;
        local_ts.push(ts);
    }
    let mut descriptor = Descriptor {
        run_start_seq: 0,
        run_start_unix_ns: start_unix_ns,
        imported: true,
        iceoryx2_version: file.iceoryx2_version,
        settings: ServiceSettings::UNKNOWN,
        recorder_buffer_size: 0,
        counter: None,
        payload: file.payload.clone(),
        user_header: file.user_header.clone(),
        system_header: file.system_header.clone(),
        service_name: file.service_name.clone(),
        recorder: format!(
            "iox2 recording file, imported by {}",
            crate::recorder_name()
        ),
    };
    let invalid = |e: layout::LayoutError| Error::Upstream(format!("{}: {e}", input.display()));
    descriptor.validate().map_err(invalid)?;
    // Everything is checked; now append, with the descriptor repeated wherever the recorder would repeat it (see
    // where descriptors lie, in the layout docs).
    let mut journal = Journal::open(journal.as_ref(), options.writer_config.clone(), 1)?;
    descriptor.run_start_seq = journal.writer.next_seq();
    let bytes = descriptor.encode().map_err(invalid)?;
    let descriptor_seq = journal.open_service(0, bytes, start_unix_ns)?;
    let mut messages = 0;
    for (record, &local_ts_ns) in file.records.iter().zip(&local_ts) {
        let data = DataRecord {
            descriptor_seq,
            publisher_id: 0,
            user_header: &record.user_header,
            payload: &record.payload,
        };
        let len = data.encoded_len().map_err(invalid)?;
        journal.append_named(
            0,
            &NewRecord {
                schema_id: SCHEMA_DATA,
                venue_ts_ns: 0,
                local_ts_ns,
                payload: &[],
            },
            len,
            // The length was checked above, so this can't fail; if it did, the panic makes the writer discard the
            // record instead of committing it.
            |buf, descriptor_seq| {
                DataRecord {
                    descriptor_seq,
                    ..data
                }
                .encode_into(buf)
                .expect("data record length checked")
            },
        )?;
        messages += 1;
    }
    journal.writer.checkpoint()?;
    Ok(ImportReport {
        service: file.service_name,
        iceoryx2_version: file.iceoryx2_version.to_string(),
        descriptor_seq,
        messages,
        start_unix_ns,
        next_seq: journal.writer.next_seq(),
    })
}

/// A parser for iceoryx2's human-readable recording files (file format version 1), written for untrusted input:
/// bounded, checked and panic-free.
mod text {
    use super::*;

    /// The line that ends the header.
    const MARKER: &str = "### Recorded Data Start ###";
    /// The longest header accepted.
    const MAX_HEADER_LEN: usize = 1 << 20;
    const PREFIXES: [&str; 4] = ["time:     ", "sys head: ", "usr head: ", "payload:  "];

    /// A parsed and validated file.
    #[derive(Debug)]
    pub(super) struct File {
        pub iceoryx2_version: Version,
        pub service_name: String,
        pub payload: TypeInfo,
        pub user_header: TypeInfo,
        pub system_header: TypeInfo,
        pub records: Vec<Record>,
    }

    #[derive(Debug)]
    pub(super) struct Record {
        /// The line its timestamp is on.
        pub line: usize,
        pub timestamp_ms: u64,
        pub user_header: Vec<u8>,
        pub payload: Vec<u8>,
    }

    /// Parses and validates a whole file. Records must fit in a journal record with segments of `segment_size`.
    pub(super) fn parse(bytes: &[u8], segment_size: u64) -> Result<File, String> {
        let text = std::str::from_utf8(bytes)
            .map_err(|e| {
                format!(
                    "not UTF-8 text (byte {}): not a human-readable iceoryx2 recording file (iox2dump files can't be \
                     imported)",
                    e.valid_up_to()
                )
            })?;
        // Upstream ends every line with a newline, the last record with an empty line ("\n\n"): text after the
        // last newline is a line cut short.
        let complete = text.rfind('\n').map_or(0, |i| i + 1);
        if complete < text.len() {
            return Err(format!(
                "line {} doesn't end with a newline: the file is truncated",
                text[..complete].matches('\n').count() + 1
            ));
        }
        let mut lines = text
            .split_terminator('\n')
            .enumerate()
            .map(|(i, l)| (i + 1, l));
        let mut header_len = 0usize;
        let mut marker_line = None;
        for (n, line) in lines.by_ref() {
            if line == MARKER {
                marker_line = Some(n);
                break;
            }
            header_len += line.len() + 1;
            if header_len > MAX_HEADER_LEN {
                return Err(format!("the header is longer than {MAX_HEADER_LEN} bytes"));
            }
        }
        if marker_line.is_none() {
            return Err(format!(
                "no \"{MARKER}\" line: not a human-readable iceoryx2 recording file (iox2dump files can't be imported)"
            ));
        }
        let header: toml::Table = text[..header_len]
            .parse()
            .map_err(|e| format!("the header is not valid TOML: {e}"))?;
        let mut file = header_fields(&header)?;
        layout::validate_types(&file.payload, &file.user_header, &file.system_header)
            .map_err(|e| format!("header: {e}"))?;
        let max_record = tickertape::format::max_payload_len(segment_size);
        let mut last_ts = 0u64;
        let mut fields: Vec<(usize, &str)> = Vec::with_capacity(4);
        // The empty line upstream writes after each record, still to come after the record starting on this line.
        let mut blank_after: Option<usize> = None;
        for (n, line) in lines {
            if let Some(start) = blank_after.take() {
                if !line.is_empty() {
                    return Err(format!(
                        "line {n}: expected the empty line that ends the record starting on line {start}"
                    ));
                }
                continue;
            }
            if fields.is_empty() && line.is_empty() {
                continue;
            }
            if !line.is_ascii() {
                return Err(format!("line {n}: not ASCII"));
            }
            let prefix = PREFIXES[fields.len()];
            let Some(value) = line.strip_prefix(prefix) else {
                return Err(format!(
                    "line {n}: expected a line starting with \"{prefix}\""
                ));
            };
            fields.push((n, value));
            if fields.len() < 4 {
                continue;
            }
            let (ts_line, ts) = fields[0];
            if ts.is_empty() || !ts.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("line {ts_line}: invalid timestamp"));
            }
            let timestamp_ms: u64 = ts
                .parse()
                .map_err(|_| format!("line {ts_line}: timestamp out of range"))?;
            if timestamp_ms < last_ts {
                return Err(format!(
                    "line {ts_line}: timestamp {timestamp_ms} is before the previous one ({last_ts})"
                ));
            }
            last_ts = timestamp_ms;
            let system_header = hex(fields[1])?;
            let user_header = hex(fields[2])?;
            let payload = hex(fields[3])?;
            if system_header.len() as u64 != file.system_header.size {
                return Err(format!(
                    "line {}: the system header has {} bytes, the header says {}",
                    fields[1].0,
                    system_header.len(),
                    file.system_header.size
                ));
            }
            let lengths_ok = user_header.len() as u64 == file.user_header.size
                && match file.payload.variant {
                    TypeVariant::FixedSize => payload.len() as u64 == file.payload.size,
                    TypeVariant::Dynamic => {
                        (payload.len() as u64).is_multiple_of(file.payload.size)
                    }
                };
            if !lengths_ok {
                return Err(format!(
                    "record at line {ts_line}: a {}-byte user header and a {}-byte payload don't fit the header's types",
                    user_header.len(),
                    payload.len()
                ));
            }
            let len = DataRecord {
                descriptor_seq: 0,
                publisher_id: 0,
                user_header: &user_header,
                payload: &payload,
            }
            .encoded_len()
            .map_err(|e| format!("record at line {ts_line}: {e}"))?;
            if len as u64 > max_record {
                return Err(format!(
                    "record at line {ts_line} needs {len} bytes, more than a journal record can hold ({max_record})"
                ));
            }
            file.records.push(Record {
                line: ts_line,
                timestamp_ms,
                user_header,
                payload,
            });
            blank_after = Some(ts_line);
            fields.clear();
        }
        if let Some(n) = fields.first().map(|f| f.0).or(blank_after) {
            return Err(format!(
                "the file ends inside the record that starts on line {n} (truncated?)"
            ));
        }
        Ok(file)
    }

    /// Writes a file's header as upstream's recorder does (its TOML serialisation, an empty line, the marker
    /// line), for a file of `service` with these payload and user header types and this build's system header.
    pub(super) fn write_header(
        w: &mut dyn Write,
        service: &str,
        payload: &TypeInfo,
        user_header: &TypeInfo,
    ) -> std::io::Result<()> {
        use std::fmt::Write as _;
        let system_header = crate::recorder::type_info(&system_header_detail());
        let mut h = String::new();
        let v = ICEORYX2_VERSION;
        let _ = write!(
            h,
            "service_name = {}\n\n[iceoryx2_version]\nmajor = {}\nminor = {}\npatch = {}\n\n[details]\n\
             file_format_version = 1\nmessaging_pattern = \"PublishSubscribe\"\n",
            toml_string(service),
            v.major,
            v.minor,
            v.patch
        );
        for (key, t) in [
            ("payload", payload),
            ("user_header", user_header),
            ("system_header", &system_header),
        ] {
            let variant = match t.variant {
                TypeVariant::FixedSize => "FixedSize",
                TypeVariant::Dynamic => "Dynamic",
            };
            let _ = write!(
                h,
                "\n[details.types.{key}]\nvariant = \"{variant}\"\ntype_name = {}\nsize = {}\nalignment = {}\n",
                toml_string(&t.name),
                t.size,
                t.alignment
            );
        }
        h.push_str("\n\n");
        h.push_str(MARKER);
        h.push('\n');
        w.write_all(h.as_bytes())
    }

    /// Writes one record as upstream's recorder does: four lines and an empty one, bytes as two-digit lower-case hex
    /// numbers each followed by a space.
    pub(super) fn write_record(
        w: &mut dyn Write,
        timestamp_ms: u64,
        system_header: &[u8],
        user_header: &[u8],
        payload: &[u8],
    ) -> std::io::Result<()> {
        use std::fmt::Write as _;
        let mut r = String::with_capacity(
            64 + 3 * (system_header.len() + user_header.len() + payload.len()),
        );
        let _ = writeln!(r, "time:     {timestamp_ms}");
        for (prefix, bytes) in [
            ("sys head: ", system_header),
            ("usr head: ", user_header),
            ("payload:  ", payload),
        ] {
            r.push_str(prefix);
            for b in bytes {
                let _ = write!(r, "{b:02x} ");
            }
            r.push('\n');
        }
        r.push('\n');
        w.write_all(r.as_bytes())
    }

    /// A TOML basic string.
    fn toml_string(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    /// Bytes written as two-digit hex numbers separated by spaces (with an optional trailing space).
    fn hex((n, value): (usize, &str)) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(value.len() / 3 + 1);
        for token in value.split(' ').filter(|t| !t.is_empty()) {
            let bytes = token.as_bytes();
            let digit = |b: u8| (b as char).to_digit(16);
            match (
                bytes.len(),
                bytes.first().copied().and_then(digit),
                bytes.get(1).copied().and_then(digit),
            ) {
                (2, Some(hi), Some(lo)) => out.push((hi * 16 + lo) as u8),
                _ => return Err(format!("line {n}: \"{token}\" is not a two-digit hex byte")),
            }
        }
        Ok(out)
    }

    fn header_fields(header: &toml::Table) -> Result<File, String> {
        only(
            header,
            "the header",
            &["service_name", "iceoryx2_version", "details"],
        )?;
        let service_name = string(header, "service_name")?;
        if service_name.is_empty() || service_name.len() > MAX_SERVICE_NAME_LEN {
            return Err(format!(
                "service_name must have 1 to {MAX_SERVICE_NAME_LEN} bytes"
            ));
        }
        let version = table(header, "iceoryx2_version")?;
        only(version, "iceoryx2_version", &["major", "minor", "patch"])?;
        let part = |key| -> Result<u16, String> {
            u16::try_from(integer(version, key)?)
                .map_err(|_| format!("iceoryx2_version.{key} is out of range"))
        };
        let iceoryx2_version = Version {
            major: part("major")?,
            minor: part("minor")?,
            patch: part("patch")?,
        };
        let details = table(header, "details")?;
        only(
            details,
            "details",
            &["file_format_version", "messaging_pattern", "types"],
        )?;
        let format_version = integer(details, "file_format_version")?;
        if format_version != 1 {
            return Err(format!(
                "file format version {format_version} is not supported (this build reads version 1)"
            ));
        }
        let pattern = string(details, "messaging_pattern")?;
        if pattern != "PublishSubscribe" {
            return Err(format!(
                "messaging pattern \"{pattern}\" is not supported (only PublishSubscribe)"
            ));
        }
        let types = table(details, "types")?;
        only(
            types,
            "details.types",
            &["payload", "user_header", "system_header"],
        )?;
        Ok(File {
            iceoryx2_version,
            service_name: service_name.to_string(),
            payload: type_info(types, "payload")?,
            user_header: type_info(types, "user_header")?,
            system_header: type_info(types, "system_header")?,
            records: Vec::new(),
        })
    }

    fn type_info(types: &toml::Table, key: &str) -> Result<TypeInfo, String> {
        let t = table(types, key)?;
        only(t, key, &["variant", "type_name", "size", "alignment"])?;
        let variant = match string(t, "variant")? {
            "FixedSize" => TypeVariant::FixedSize,
            "Dynamic" => TypeVariant::Dynamic,
            other => {
                return Err(format!(
                    "{key}.variant \"{other}\" is not FixedSize or Dynamic"
                ));
            }
        };
        let unsigned = |field| -> Result<u64, String> {
            u64::try_from(integer(t, field)?).map_err(|_| format!("{key}.{field} is negative"))
        };
        Ok(TypeInfo {
            variant,
            name: string(t, "type_name")?.to_string(),
            size: unsigned("size")?,
            alignment: unsigned("alignment")?,
        })
    }

    fn only(t: &toml::Table, what: &str, keys: &[&str]) -> Result<(), String> {
        match t.keys().find(|k| !keys.contains(&k.as_str())) {
            Some(k) => Err(format!("unknown key \"{k}\" in {what}")),
            None => Ok(()),
        }
    }

    fn table<'a>(t: &'a toml::Table, key: &str) -> Result<&'a toml::Table, String> {
        t.get(key)
            .and_then(toml::Value::as_table)
            .ok_or_else(|| format!("missing table \"{key}\""))
    }

    fn string<'a>(t: &'a toml::Table, key: &str) -> Result<&'a str, String> {
        t.get(key)
            .and_then(toml::Value::as_str)
            .ok_or_else(|| format!("missing string \"{key}\""))
    }

    fn integer(t: &toml::Table, key: &str) -> Result<i64, String> {
        t.get(key)
            .and_then(toml::Value::as_integer)
            .ok_or_else(|| format!("missing integer \"{key}\""))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const SEGMENT: u64 = 1 << 20;

        fn file(payload: (&str, u64, u64), records: &str) -> String {
            format!(
                "service_name = \"demo/x\"\n\n[iceoryx2_version]\nmajor = 0\nminor = 10\npatch = 0\n\n[details]\n\
                 file_format_version = 1\nmessaging_pattern = \"PublishSubscribe\"\n\n[details.types.payload]\n\
                 variant = \"{}\"\ntype_name = \"u32\"\nsize = {}\nalignment = {}\n\n[details.types.user_header]\n\
                 variant = \"FixedSize\"\ntype_name = \"H\"\nsize = 2\nalignment = 1\n\n\
                 [details.types.system_header]\nvariant = \"FixedSize\"\ntype_name = \"S\"\nsize = 4\nalignment = 4\n\n\n\
                 {MARKER}\n{records}",
                payload.0, payload.1, payload.2
            )
        }

        fn record(ts: u64, payload: &str) -> String {
            format!(
                "time:     {ts}\nsys head: 00 00 00 00 \nusr head: 0a 0b \npayload:  {payload} \n\n"
            )
        }

        #[test]
        fn parses_what_upstream_writes() {
            let text = file(
                ("Dynamic", 4, 4),
                &(record(0, "01 00 00 00")
                    + &record(11, "")
                    + &record(11, "02 00 00 00 03 00 00 00")),
            );
            let f = parse(text.as_bytes(), SEGMENT).unwrap();
            assert_eq!(f.service_name, "demo/x");
            assert_eq!(f.records.len(), 3);
            assert_eq!(f.records[2].payload, [2, 0, 0, 0, 3, 0, 0, 0]);
            assert_eq!(f.records[1].timestamp_ms, 11);
            assert_eq!(f.records[0].user_header, [0x0a, 0x0b]);
        }

        #[test]
        fn rejects_damaged_files() {
            let good = file(
                ("FixedSize", 4, 4),
                &(record(0, "01 00 00 00") + &record(5, "02 00 00 00")),
            );
            assert!(parse(good.as_bytes(), SEGMENT).is_ok());
            let cases: Vec<(String, &str)> = vec![
                // Review findings: an alignment of 3, a missing last payload line, a mid-file error, another
                // messaging pattern or file format version, and a multi-byte character across a field boundary.
                (
                    good.replace(
                        "alignment = 4\n\n[details.types.user_header]",
                        "alignment = 3\n\n[details.types.user_header]",
                    ),
                    "power of two",
                ),
                (
                    good.trim_end().rsplit_once('\n').unwrap().0.to_string(),
                    "truncated",
                ),
                (
                    good.trim_end().rsplit_once('\n').unwrap().0.to_string() + "\n",
                    "ends inside the record",
                ),
                (good.replace("02 00 00 00", "02 00 0g 00"), "two-digit hex"),
                (
                    good.replace("PublishSubscribe", "Event"),
                    "messaging pattern",
                ),
                (
                    good.replace("file_format_version = 1", "file_format_version = 2"),
                    "file format version 2",
                ),
                (good.replace("time:     5", "time:    é5"), "not ASCII"),
                (good.replace("time:     5", "tim€:     5"), "not ASCII"),
                (good.replace("01 00 00 00", "01 00 00"), "don't fit"),
                (
                    good.replace("time:     5", "time:     -5"),
                    "invalid timestamp",
                ),
                (
                    good.replace("time:     5", "time:     99999999999999999999999"),
                    "out of range",
                ),
                (
                    good.replace(
                        "usr head: 0a 0b \npayload:  02",
                        "usr head: 0a \npayload:  02",
                    ),
                    "don't fit",
                ),
                (
                    good.replace(
                        "sys head: 00 00 00 00 \nusr head: 0a 0b \npayload:  02",
                        "sys head: 00 \nusr head: 0a 0b \npayload:  02",
                    ),
                    "system header",
                ),
                (
                    good.replacen("time:     0", "time:     9", 1),
                    "before the previous",
                ),
                (good.replace(MARKER, "### nope ###"), "iox2dump"),
                (good.replace("size = 2", "size = -2"), "negative"),
                (
                    good.replace("[details]", "bogus = 1\n[details]"),
                    "unknown key",
                ),
                (
                    good.replace(
                        "variant = \"FixedSize\"\ntype_name = \"H\"",
                        "variant = \"Dynamic\"\ntype_name = \"H\"",
                    ),
                    "user header type",
                ),
                (
                    good.replace(
                        "size = 4\nalignment = 4\n\n[details.types.user_header]",
                        "size = 18446744073709551615\nalignment = 1\n\n[details.types.user_header]",
                    ),
                    "not valid TOML",
                ),
                (good.replace("major = 0", "major = 70000"), "out of range"),
                (good.replace("\"demo/x\"", "\"\""), "service_name"),
                // Review round 3: a payload line cut after two complete hex tokens of four, with or without its
                // newline, and a last record without the empty line upstream writes after it.
                (
                    good.trim_end().strip_suffix(" 00 00").unwrap().to_string(),
                    "doesn't end with a newline",
                ),
                (
                    good.trim_end().strip_suffix(" 00 00").unwrap().to_string() + "\n",
                    "don't fit",
                ),
                (
                    good.strip_suffix('\n').unwrap().to_string(),
                    "ends inside the record that starts on line",
                ),
                (
                    good.replacen(" \n\ntime:     5", " \ntime:     5", 1),
                    "expected the empty line",
                ),
            ];
            for (text, expected) in cases {
                let err = parse(text.as_bytes(), SEGMENT).expect_err(expected);
                assert!(
                    err.contains(expected),
                    "{err} (expected {expected})\n{text}"
                );
            }
            assert!(parse(&[0xff, 0xfe], SEGMENT).unwrap_err().contains("UTF-8"));
            // A record larger than a journal segment can hold.
            let big = file(("Dynamic", 4, 4), &record(0, &"00 ".repeat(70_000)));
            assert!(
                parse(big.as_bytes(), 65_536)
                    .unwrap_err()
                    .contains("more than a journal record")
            );
        }

        /// A four-byte `[u8]` message whose payload line ends after two complete hex tokens was imported as a
        /// two-byte message: every record must end with its newlines, as upstream writes them.
        #[test]
        fn a_truncated_last_record_is_refused() {
            let text = file(("Dynamic", 1, 1), &record(0, "01 02 03 04"));
            assert_eq!(
                parse(text.as_bytes(), SEGMENT).unwrap().records[0].payload,
                [1, 2, 3, 4]
            );
            let cut = text.find("01 02 03 04").unwrap() + "01 02 ".len();
            for end in cut..text.len() {
                let err = parse(&text.as_bytes()[..end], SEGMENT).expect_err("truncated");
                assert!(
                    err.contains("truncated") || err.contains("newline"),
                    "{end}: {err}"
                );
            }
        }

        /// The writer produces exactly what upstream's recorder wrote for the fixture (header and record layout),
        /// and what it writes parses back, including names that need TOML escapes.
        #[test]
        fn the_writer_matches_upstream_and_parses_back() {
            let fixture = include_str!("../tests/fixtures/iox2-0.10.0-record-ticks.txt");
            let header_end = fixture.find(MARKER).unwrap() + MARKER.len() + 1;
            let fixed = |name: &str, size, alignment| TypeInfo {
                variant: TypeVariant::FixedSize,
                name: name.into(),
                size,
                alignment,
            };
            let mut out = Vec::new();
            write_header(
                &mut out,
                "tt-fixture/ticks",
                &fixed("tickertape_iceoryx2::example::Tick", 24, 8),
                &fixed("tickertape_iceoryx2::example::TickHeader", 16, 8),
            )
            .unwrap();
            assert_eq!(std::str::from_utf8(&out).unwrap(), &fixture[..header_end]);
            let first = parse(fixture.as_bytes(), SEGMENT).unwrap();
            let sys: Vec<u8> = (0..48).collect();
            let r = &first.records[0];
            write_record(&mut out, 0, &sys, &r.user_header, &r.payload).unwrap();
            let record_end = fixture[header_end..].find("\n\n").unwrap() + 2;
            let mut expected = fixture[header_end..header_end + record_end].to_string();
            let sys_line = expected.lines().nth(1).unwrap().to_string();
            let ours: String = sys.iter().map(|b| format!("{b:02x} ")).collect();
            expected = expected.replace(&sys_line, &format!("sys head: {ours}"));
            assert_eq!(std::str::from_utf8(&out[header_end..]).unwrap(), expected);

            let odd = "a\"b\\c\u{7f}d/é";
            let mut out = Vec::new();
            write_header(&mut out, odd, &fixed(odd, 4, 4), &fixed("()", 0, 1)).unwrap();
            write_record(&mut out, 7, &[0; 48], &[], &[1, 2, 3, 4]).unwrap();
            let parsed = parse(&out, SEGMENT).unwrap();
            assert_eq!(
                (parsed.service_name.as_str(), parsed.payload.name.as_str()),
                (odd, odd)
            );
            assert_eq!(parsed.records[0].payload, [1, 2, 3, 4]);
        }

        #[test]
        fn arbitrary_bytes_never_panic() {
            let good = file(
                ("Dynamic", 4, 4),
                &(record(0, "01 00 00 00") + &record(5, "02 00 00 00")),
            );
            let bytes = good.as_bytes();
            for i in 0..bytes.len() {
                for b in [0u8, b'\n', b' ', 0xC3, 0xFF, b'9'] {
                    let mut damaged = bytes.to_vec();
                    damaged[i] = b;
                    let _ = parse(&damaged, SEGMENT);
                }
                let _ = parse(&bytes[..i], SEGMENT);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{SCHEMA_DESCRIPTOR, Version};

    /// A journal with one service of `u32` payloads and `n` messages.
    fn journal(n: u32) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j");
        let mut w = tickertape::Writer::open(
            &path,
            WriterConfig::default().segment_size(65_536).prefault(false),
        )
        .unwrap();
        let fixed = |name: &str, size, alignment| TypeInfo {
            variant: TypeVariant::FixedSize,
            name: name.into(),
            size,
            alignment,
        };
        let d = Descriptor {
            run_start_seq: 0,
            run_start_unix_ns: 1,
            imported: false,
            iceoryx2_version: Version {
                major: 0,
                minor: 10,
                patch: 0,
            },
            settings: ServiceSettings::UNKNOWN,
            recorder_buffer_size: 0,
            counter: None,
            payload: fixed("u32", 4, 4),
            user_header: fixed("()", 0, 1),
            system_header: type_info_of_system_header(),
            service_name: "export/x".into(),
            recorder: "test".into(),
        };
        let bytes = d.encode().unwrap();
        w.append(&NewRecord {
            schema_id: SCHEMA_DESCRIPTOR,
            venue_ts_ns: 0,
            local_ts_ns: 1_000_000,
            payload: &bytes,
        })
        .unwrap();
        for i in 0..n {
            let data = DataRecord {
                descriptor_seq: 0,
                publisher_id: 0,
                user_header: &[],
                payload: &i.to_le_bytes(),
            }
            .encode()
            .unwrap();
            w.append(&NewRecord {
                schema_id: SCHEMA_DATA,
                venue_ts_ns: 0,
                local_ts_ns: 1_000_000 * u64::from(i + 1),
                payload: &data,
            })
            .unwrap();
        }
        w.checkpoint().unwrap();
        (dir, path)
    }

    fn type_info_of_system_header() -> TypeInfo {
        crate::recorder::type_info(&system_header_detail())
    }

    /// Accepts at most `chunk` bytes per call, and fails once `limit` bytes were written (a full volume).
    struct Limited {
        out: Vec<u8>,
        chunk: usize,
        limit: usize,
    }

    impl Write for Limited {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let room = self.limit - self.out.len();
            if room == 0 {
                return Err(std::io::Error::from_raw_os_error(28)); // ENOSPC
            }
            let n = buf.len().min(self.chunk).min(room);
            self.out.extend_from_slice(&buf[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Short writes are continued until everything is written, and a write that fails fails the export: upstream's
    /// writer took any `Ok(n)` as complete (review round 3).
    #[test]
    fn exports_write_everything_or_fail() {
        let (_dir, journal) = journal(50);
        let options = ExportOptions::default();
        let mut whole = Vec::new();
        let report = export_to(&journal, &mut whole, "export/x", &options).unwrap();
        assert_eq!(report.messages, 50);
        let parsed = text::parse(&whole, 65_536).unwrap();
        assert_eq!(parsed.records.len(), 50);
        assert_eq!(parsed.records[49].payload, 49u32.to_le_bytes());

        let mut short = Limited {
            out: Vec::new(),
            chunk: 7,
            limit: usize::MAX,
        };
        export_to(&journal, &mut short, "export/x", &options).unwrap();
        assert_eq!(short.out, whole, "short writes are continued");

        for limit in [0, 10, whole.len() / 2, whole.len() - 1] {
            let mut full = Limited {
                out: Vec::new(),
                chunk: 7,
                limit,
            };
            let err = export_to(&journal, &mut full, "export/x", &options).unwrap_err();
            assert!(
                matches!(err, Error::Io { op: "write", .. }),
                "{limit}: {err}"
            );
        }
    }

    /// Two exports to one destination at the same time, in one process: each writes its own temporary file, one
    /// links it into place, the other fails without touching the winner's file, and no temporary file remains.
    #[test]
    fn concurrent_exports_to_one_destination_never_collide() {
        let (dir, journal) = journal(2_000);
        let output = dir.path().join("out.iox2");
        for _ in 0..5 {
            let _ = std::fs::remove_file(&output);
            let barrier = std::sync::Barrier::new(2);
            let results: Vec<_> = std::thread::scope(|s| {
                let run = || {
                    barrier.wait();
                    export(&journal, &output, &ExportOptions::default())
                };
                let a = s.spawn(run);
                let b = s.spawn(run);
                vec![a.join().unwrap(), b.join().unwrap()]
            });
            let ok = results.iter().filter(|r| r.is_ok()).count();
            assert_eq!(ok, 1, "{results:?}");
            let parsed = text::parse(&std::fs::read(&output).unwrap(), 65_536).unwrap();
            assert_eq!(parsed.records.len(), 2_000, "the winner's file is complete");
            let left: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.contains("partial"))
                .collect();
            assert!(left.is_empty(), "{left:?}");
        }
    }

    /// Review round 5: a directory with write and search permission but no read permission takes the hard link
    /// but can't be opened for the sync after it. The export fails before anything is created, so no complete file
    /// is left behind under the destination's name with an error, and a retry once the directory is readable
    /// succeeds. (Skipped when the permission doesn't stop this process, as for root.)
    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_fails_before_the_file_is_published() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, journal) = journal(10);
        let out_dir = dir.path().join("write-and-search-only");
        std::fs::create_dir(&out_dir).unwrap();
        std::fs::set_permissions(&out_dir, std::fs::Permissions::from_mode(0o300)).unwrap();
        if std::fs::File::open(&out_dir).is_ok() {
            std::fs::set_permissions(&out_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            eprintln!("skipped: the directory permission doesn't apply to this process");
            return;
        }
        let output = out_dir.join("out.iox2");
        let err = export(&journal, &output, &ExportOptions::default()).unwrap_err();
        std::fs::set_permissions(&out_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            matches!(err, Error::Io { op, .. } if op.starts_with("open directory")),
            "{err}"
        );
        let left: Vec<_> = std::fs::read_dir(&out_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            left.is_empty(),
            "nothing is published or left behind: {left:?}"
        );
        let report = export(&journal, &output, &ExportOptions::default()).unwrap();
        assert_eq!(report.messages, 10, "a retry succeeds");
    }

    /// Review round 4: the temporary name doesn't grow with the destination's, so a destination whose name has the
    /// longest length a file system takes (255 bytes) exports, next to it, without a "file name too long" error.
    #[test]
    fn a_destination_name_at_the_limit_exports() {
        let (dir, journal) = journal(10);
        let output = dir.path().join(format!("{}.iox2", "n".repeat(250)));
        assert_eq!(output.file_name().unwrap().len(), 255);
        export(&journal, &output, &ExportOptions::default()).unwrap();
        let parsed = text::parse(&std::fs::read(&output).unwrap(), 65_536).unwrap();
        assert_eq!(parsed.records.len(), 10);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.len() > 200 || n.to_string_lossy().contains("partial"))
            .collect();
        assert_eq!(
            names,
            [output.file_name().unwrap()],
            "only the destination remains"
        );
        // The longest temporary name: every number at its largest.
        let longest = format!(
            ".tt-iox2-export-{:x}-{:x}-{:x}.partial",
            u32::MAX,
            u64::MAX,
            u64::MAX
        );
        assert_eq!(longest.len(), 66);
        assert!(partial_name(u64::MAX).len() <= 66);
    }
}
