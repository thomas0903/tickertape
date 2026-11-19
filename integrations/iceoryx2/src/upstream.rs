//! Conversion to and from iceoryx2's own recording files (feature `dynamic`), the files `iox2 service record`
//! writes and `iox2 service replay` plays.
//!
//! An iceoryx2 recording file holds one service: a header (iceoryx2 version, service name, payload, user header and
//! system header type details) and, per message, a timestamp in whole milliseconds since the recording started,
//! the system header, the user header and the payload. It comes in two representations: human-readable (a TOML
//! header and hex lines, the default of both tools) and `iox2dump` (raw bytes).
//!
//! [`export`] writes both representations with the `iceoryx2-userland-record-and-replay` crate. [`import`] reads
//! only the human-readable representation, with a parser of this crate's own: upstream's readers are not meant for
//! untrusted files (the text reader panics when a multi-byte character crosses a fixed byte index; the `iox2dump`
//! reader treats file bytes as an initialised Rust struct and allocates from unvalidated lengths). `iox2dump` import
//! is not offered until it can be done with a parser of this crate's own.
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
//! The whole file is read and validated before anything is appended: the header's version, messaging pattern and
//! type details ([`layout::validate_types`]), every record's lengths against them, non-decreasing timestamps, and
//! that the file doesn't end inside a record. A file that fails any check leaves the journal untouched. Only a
//! journal error while appending (a full volume) can leave part of an import in the journal, as for any writer.
//!
//! Imported bytes are not checked to be valid values of their types: replaying an import publishes them under the
//! recorded type names (see [`Replayer::run_dynamic`](crate::replayer::Replayer::run_dynamic)). Import only files you
//! trust.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use iceoryx2::prelude::*;
use iceoryx2_userland_record_and_replay::prelude::*;
use serde::Serialize;
use tickertape::{NewRecord, Reader, ReaderConfig, Writer, WriterConfig};

use crate::dynamic::{system_header_detail, type_detail};
use crate::layout::{
    self, DataRecord, Decoded, Descriptor, MAX_SERVICE_NAME_LEN, SCHEMA_DATA, SCHEMA_DESCRIPTOR,
    ServiceSettings, TypeInfo, TypeVariant, Version,
};
use crate::recorder::service_name;
use crate::{Catalog, Error};

/// The representation of an iceoryx2 recording file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Representation {
    /// TOML header and hex lines (the default of `iox2 service record` and `replay`).
    #[default]
    HumanReadable,
    /// Raw bytes: the header is a memory image of a Rust struct, readable only by the same iceoryx2 version on the
    /// same architecture. Export only.
    Iox2Dump,
}

impl From<Representation> for DataRepresentation {
    fn from(r: Representation) -> Self {
        match r {
            Representation::HumanReadable => DataRepresentation::HumanReadable,
            Representation::Iox2Dump => DataRepresentation::Iox2Dump,
        }
    }
}

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
    /// File representation.
    pub representation: Representation,
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

    /// Writes this representation.
    pub fn representation(mut self, r: Representation) -> Self {
        self.representation = r;
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
    /// Data records skipped because the descriptor they name is no longer in the journal.
    pub undescribed: u64,
}

/// Writes the messages of one service in the journal at `journal` to a new iceoryx2 recording file at `output`.
///
/// Reads through the end of the journal as it is when the export gets there. The file is written under a temporary
/// name next to `output` and linked to `output` only when complete, so a failed export leaves no partial file.
///
/// # Errors
///
/// [`Error::Usage`] if the range holds several services and none was chosen, the chosen service isn't there, or
/// its types changed between recording runs; [`Error::Upstream`] if the file exists or can't be written; journal and
/// layout errors as for [`info::scan`](crate::info::scan).
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
    let partial = partial_path(output)?;
    let result = export_to(journal, &partial, &service, options);
    let result = result.and_then(|report| {
        // A hard link fails if `output` exists, so an existing file is never replaced.
        std::fs::hard_link(&partial, output).map_err(|source| Error::Io {
            op: "create",
            path: output.to_path_buf(),
            source,
        })?;
        Ok(report)
    });
    let _ = std::fs::remove_file(&partial);
    result
}

fn partial_path(output: &Path) -> Result<PathBuf, Error> {
    let name = output
        .file_name()
        .ok_or_else(|| Error::Usage(format!("{} is not a file name", output.display())))?;
    let mut partial = std::ffi::OsString::from(".");
    partial.push(name);
    partial.push(format!(".partial-{}", std::process::id()));
    Ok(output.with_file_name(partial))
}

fn export_to(
    journal: &Path,
    path: &Path,
    service: &str,
    options: &ExportOptions,
) -> Result<ExportReport, Error> {
    let mut reader = open_at(journal, options.from)?;
    let mut catalog = Catalog::new(journal);
    let mut recorder: Option<(
        iceoryx2_userland_record_and_replay::recorder::Recorder,
        Descriptor,
    )> = None;
    let mut report = ExportReport {
        service: service.to_string(),
        messages: 0,
        first_seq: None,
        last_seq: None,
        runs: 0,
        clamped: 0,
        undescribed: 0,
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
            _ => continue,
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
        let (rec, first) = match &mut recorder {
            Some((r, first)) => (r, &*first),
            None => {
                let types = ServiceTypes {
                    payload: type_detail(&d.payload)?,
                    user_header: type_detail(&d.user_header)?,
                    system_header: system_header_detail(),
                };
                let r = RecorderBuilder::new(&types)
                    .data_representation(options.representation.into())
                    .messaging_pattern(MessagingPattern::PublishSubscribe)
                    .create(&file_path(path)?, &service_name(service)?)
                    .map_err(|e| Error::Upstream(format!("create {}: {e:?}", path.display())))?;
                first_ts = record.local_ts_ns;
                let (r, first) = recorder.insert((r, (*d).clone()));
                (r, &*first)
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
        let system_header = vec![0u8; rec.header().details.types.system_header.size()];
        rec.write(RawRecord {
            timestamp: offset,
            system_header: &system_header,
            user_header: data.user_header,
            payload: data.payload,
        })
        .map_err(|e| Error::Upstream(format!("write record {seq}: {e:?}")))?;
        report.messages += 1;
        report.first_seq.get_or_insert(seq);
        report.last_seq = Some(seq);
    }
    if recorder.is_none() {
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
        match layout::decode(record.schema_id, record.payload)
            .map_err(|source| Error::Layout { seq, source })?
        {
            Decoded::Descriptor(d) => {
                catalog.insert(seq, *d);
            }
            Decoded::Data(data) => {
                if let Some(d) = catalog.resolve(seq, data.descriptor_seq)? {
                    names.insert(d.service_name.clone());
                }
            }
            _ => {}
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

fn file_path(path: &Path) -> Result<FilePath, Error> {
    let bytes = path.as_os_str().as_encoded_bytes();
    FilePath::new(bytes).map_err(|e| {
        Error::Upstream(format!(
            "path {} is not usable by iceoryx2: {e:?}",
            path.display()
        ))
    })
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
/// needed), as one new recording run with one descriptor, then checkpoints. The file is read and validated
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
    let file = text::parse(&bytes, options.writer_config.segment_size)
        .map_err(|e| Error::Upstream(format!("{}: {e}", input.display())))?;
    let start_unix_ns = options
        .start_unix_ns
        .unwrap_or_else(tickertape::clock::realtime_ns);
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
    // Everything is checked; now append.
    let mut writer = Writer::open(journal.as_ref(), options.writer_config.clone())?;
    descriptor.run_start_seq = writer.next_seq();
    let bytes = descriptor.encode().map_err(invalid)?;
    let descriptor_seq = writer.append(&NewRecord {
        schema_id: SCHEMA_DESCRIPTOR,
        venue_ts_ns: 0,
        local_ts_ns: start_unix_ns,
        payload: &bytes,
    })?;
    let mut messages = 0;
    for record in &file.records {
        let data = DataRecord {
            descriptor_seq,
            publisher_id: 0,
            user_header: &record.user_header,
            payload: &record.payload,
        };
        let payload = data.encode().map_err(invalid)?;
        let offset = record.timestamp_ms.saturating_mul(1_000_000);
        writer.append(&NewRecord {
            schema_id: SCHEMA_DATA,
            venue_ts_ns: 0,
            local_ts_ns: start_unix_ns.saturating_add(offset),
            payload: &payload,
        })?;
        messages += 1;
    }
    writer.checkpoint()?;
    Ok(ImportReport {
        service: file.service_name,
        iceoryx2_version: file.iceoryx2_version.to_string(),
        descriptor_seq,
        messages,
        start_unix_ns,
        next_seq: writer.next_seq(),
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
        let mut lines = text.split('\n').enumerate().map(|(i, l)| (i + 1, l));
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
        for (n, line) in lines {
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
                timestamp_ms,
                user_header,
                payload,
            });
            fields.clear();
        }
        if let Some((n, _)) = fields.first() {
            return Err(format!(
                "the file ends inside the record that starts on line {n} (truncated?)"
            ));
        }
        Ok(file)
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
