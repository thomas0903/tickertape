//! Conversion to and from iceoryx2's own recording files (feature `dynamic`), the files `iox2 service record`
//! writes and `iox2 service replay` plays. Uses the `iceoryx2-userland-record-and-replay` crate.
//!
//! An iceoryx2 recording file holds one service: a header (iceoryx2 version, service name, payload, user header and
//! system header type details) and, per message, a timestamp in whole milliseconds since the recording started,
//! the system header, the user header and the payload. It comes in two representations: human-readable (a TOML
//! header and hex lines, the default of both tools) and `iox2dump` (raw bytes).
//!
//! # What each direction loses
//!
//! **Journal to iceoryx2 file ([`export`]):**
//! - Only one service per file: choose it when the journal has several.
//! - Absolute time: only the offset from the first exported message remains, truncated to whole milliseconds.
//!   Timestamps that go backwards (the wall clock stepped back while recording) are raised to the previous one,
//!   because the format requires them to be non-decreasing; [`ExportReport::clamped`] counts them.
//! - Session boundaries, tickertape sequence numbers, `venue_ts_ns`, loss records and the service settings in the
//!   descriptors (history, buffer sizes, overflow).
//! - The iceoryx2 version in the file header is the version this crate is built with, not the recording side's.
//! - The system header (iceoryx2's per-sample header, with publisher and node ids) is not recorded by tickertape;
//!   the file gets zeros of the right size. `iox2 service replay` ignores it.
//!
//! **iceoryx2 file to journal ([`import`]):**
//! - Absolute time was never in the file: `local_ts_ns` is a chosen start time (the import time by default) plus
//!   each record's offset in milliseconds. The descriptor's `imported` flag says so.
//! - The system header bytes are dropped.
//! - Service settings are unknown: the descriptor's settings are 0 (safe overflow is set, iceoryx2's default), and
//!   a replay creates the service with iceoryx2's defaults.
//!
//! Message bytes (user header and payload) and order survive both directions unchanged.
//!
//! The `iox2dump` representation stores the file header as a raw memory image of a Rust struct, so it is only
//! readable by the same iceoryx2 version on the same architecture, and reading a damaged or hostile file of that
//! kind is not memory-safe (the upstream reader trusts it). Prefer the human-readable representation, and import
//! `iox2dump` files only from sources you trust.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use iceoryx2::prelude::*;
use iceoryx2_userland_record_and_replay::prelude::*;
use serde::Serialize;
use tickertape::{NewRecord, Reader, ReaderConfig, Writer, WriterConfig};

use crate::dynamic::{system_header_detail, type_detail, type_info_of};
use crate::layout::{
    self, DataRecord, Decoded, Descriptor, SCHEMA_DATA, SCHEMA_DESCRIPTOR, ServiceSettings, Version,
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
    /// Raw bytes. See the [module docs](self) for why it is less portable and less safe to read.
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
    /// Recording sessions the messages came from (their boundaries are lost).
    pub sessions: u64,
    /// Messages whose timestamp went backwards and was raised to the previous one.
    pub clamped: u64,
}

/// Writes the messages of one service in the journal at `journal` to a new iceoryx2 recording file at `output`.
///
/// Reads through the end of the journal as it is when the export gets there.
///
/// # Errors
///
/// [`Error::Usage`] if the range holds several services and none was chosen, the chosen service isn't there, or
/// its types changed between sessions; [`Error::Upstream`] if the file exists or can't be written; journal and
/// layout errors as for [`info::scan`](crate::info::scan).
pub fn export(
    journal: impl AsRef<Path>,
    output: impl AsRef<Path>,
    options: &ExportOptions,
) -> Result<ExportReport, Error> {
    let journal = journal.as_ref();
    let output = output.as_ref();
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
    let mut reader = open_at(journal, options.from)?;
    let mut catalog = Catalog::new(journal);
    let mut recorder: Option<(
        iceoryx2_userland_record_and_replay::recorder::Recorder,
        Descriptor,
    )> = None;
    let mut report = ExportReport {
        service: service.clone(),
        messages: 0,
        first_seq: None,
        last_seq: None,
        sessions: 0,
        clamped: 0,
    };
    let mut sessions = BTreeSet::new();
    let mut first_ts = 0u64;
    let mut last_offset = Duration::ZERO;
    while let Some(record) = reader.try_next()? {
        let seq = record.seq;
        if options.to.is_some_and(|to| seq > to) {
            break;
        }
        let data = match layout::decode(record.schema_id, record.payload)
            .map_err(|source| Error::Layout { seq, source })?
        {
            Decoded::Descriptor(d) => {
                catalog.insert(seq, *d);
                continue;
            }
            Decoded::Data(d) => d,
            _ => continue,
        };
        let d = catalog.get(seq, data.descriptor_seq)?;
        if d.service_name != service {
            continue;
        }
        sessions.insert(d.session_id);
        let (rec, first) = match &mut recorder {
            Some((r, first)) => (r, &*first),
            None => {
                let types = ServiceTypes {
                    payload: type_detail(&d.payload)?,
                    user_header: type_detail(&d.user_header)?,
                    system_header: system_header_detail(),
                };
                let path = file_path(output)?;
                let r = RecorderBuilder::new(&types)
                    .data_representation(options.representation.into())
                    .messaging_pattern(MessagingPattern::PublishSubscribe)
                    .create(&path, &service_name(&service)?)
                    .map_err(|e| Error::Upstream(format!("create {}: {e:?}", output.display())))?;
                first_ts = record.local_ts_ns;
                let (r, first) = recorder.insert((r, (*d).clone()));
                (r, &*first)
            }
        };
        if first.payload != d.payload || first.user_header != d.user_header {
            return Err(Error::Usage(format!(
                "service \"{service}\" changed types between sessions (record {seq}); export the sessions separately with --from/--to"
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
    report.sessions = sessions.len() as u64;
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
                names.insert(catalog.get(seq, data.descriptor_seq)?.service_name.clone());
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
    /// File representation.
    pub representation: Representation,
    /// The start time the file's relative timestamps are added to, nanoseconds since the Unix epoch. Default: now.
    pub start_unix_ns: Option<u64>,
    /// Settings for the journal writer.
    pub writer_config: WriterConfig,
}

impl ImportOptions {
    /// Reads this representation.
    pub fn representation(mut self, r: Representation) -> Self {
        self.representation = r;
        self
    }

    /// Adds the file's relative timestamps to this start time.
    pub fn start_unix_ns(mut self, ns: u64) -> Self {
        self.start_unix_ns = Some(ns);
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

/// Appends the messages of an iceoryx2 recording file to the journal at `journal` (created if needed), as one new
/// session with one descriptor, then checkpoints.
///
/// # Errors
///
/// [`Error::Upstream`] if the file can't be read; [`Error::Journal`] if the journal can't be written (for example,
/// a recorder is writing to it).
pub fn import(
    input: impl AsRef<Path>,
    journal: impl AsRef<Path>,
    options: &ImportOptions,
) -> Result<ImportReport, Error> {
    let input = input.as_ref();
    let mut replayer = ReplayerOpener::new(&file_path(input)?)
        .data_representation(options.representation.into())
        .open()
        .map_err(|e| Error::Upstream(format!("open {}: {e:?}", input.display())))?;
    let header = replayer.header().clone();
    let mut writer = Writer::open(journal.as_ref(), options.writer_config.clone())?;
    let start_unix_ns = options
        .start_unix_ns
        .unwrap_or_else(tickertape::clock::realtime_ns);
    let service = header.service_name.as_str().to_string();
    let types = &header.details.types;
    let descriptor = Descriptor {
        session_id: writer.next_seq(),
        session_start_unix_ns: start_unix_ns,
        imported: true,
        iceoryx2_version: Version {
            major: header.iceoryx2_version.major,
            minor: header.iceoryx2_version.minor,
            patch: header.iceoryx2_version.patch,
        },
        settings: ServiceSettings {
            history_size: 0,
            subscriber_max_buffer_size: 0,
            subscriber_max_borrowed_samples: 0,
            max_publishers: 0,
            max_subscribers: 0,
            max_nodes: 0,
            safe_overflow: true,
        },
        recorder_buffer_size: 0,
        counter: None,
        payload: type_info_of(&types.payload),
        user_header: type_info_of(&types.user_header),
        system_header: type_info_of(&types.system_header),
        service_name: service.clone(),
        recorder: format!(
            "iox2 recording file, imported by {}",
            crate::recorder_name()
        ),
    };
    let bytes = descriptor.encode().map_err(|source| Error::Layout {
        seq: writer.next_seq(),
        source,
    })?;
    let descriptor_seq = writer.append(&NewRecord {
        schema_id: SCHEMA_DESCRIPTOR,
        venue_ts_ns: 0,
        local_ts_ns: start_unix_ns,
        payload: &bytes,
    })?;
    let mut messages = 0;
    while let Some(record) = replayer
        .next_record()
        .map_err(|e| Error::Upstream(format!("read {}: {e:?}", input.display())))?
    {
        let data = DataRecord {
            descriptor_seq,
            user_header: &record.user_header,
            payload: &record.payload,
        };
        let payload = data.encode().map_err(|source| Error::Layout {
            seq: writer.next_seq(),
            source,
        })?;
        let offset = u64::try_from(record.timestamp.as_nanos()).unwrap_or(u64::MAX);
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
        service,
        iceoryx2_version: descriptor.iceoryx2_version.to_string(),
        descriptor_seq,
        messages,
        start_unix_ns,
        next_seq: writer.next_seq(),
    })
}
