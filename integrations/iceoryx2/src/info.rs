//! Summarize a recording: sessions, services, message counts, time ranges and reported losses. No iceoryx2.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use tickertape::{Reader, ReaderConfig};

use crate::layout::{self, Decoded, Descriptor};
use crate::{Catalog, Error};

/// A summary of a recording, through the end the reader saw when it got there.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Info {
    /// First sequence number read, if any.
    pub first_seq: Option<u64>,
    /// Last sequence number read, if any.
    pub last_seq: Option<u64>,
    /// Records read, of any schema.
    pub records: u64,
    /// Records with schema ids this crate doesn't write.
    pub other_records: u64,
    /// Recording sessions, by session id.
    pub sessions: Vec<Session>,
    /// Recorded services, by name.
    pub services: Vec<Service>,
}

/// One run of the recorder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Session {
    /// The journal's next sequence number when the recorder opened it.
    pub session_id: u64,
    /// When it started, nanoseconds since the Unix epoch.
    pub start_unix_ns: u64,
    /// Who recorded it.
    pub recorder: String,
    /// iceoryx2 version on the recording side.
    pub iceoryx2_version: String,
    /// True if imported from an iceoryx2 recording file (times reconstructed).
    pub imported: bool,
    /// Services that have a descriptor in this session.
    pub services: Vec<String>,
}

/// One recorded service, across sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Service {
    /// Service name.
    pub name: String,
    /// Payload type name, from the latest descriptor.
    pub payload_type: String,
    /// User header type name, from the latest descriptor.
    pub user_header_type: String,
    /// Descriptor records (one per session that recorded the service).
    pub descriptors: u64,
    /// Messages recorded.
    pub messages: u64,
    /// Payload bytes recorded (user headers excluded).
    pub payload_bytes: u64,
    /// Sequence number of the first message.
    pub first_seq: Option<u64>,
    /// Sequence number of the last message.
    pub last_seq: Option<u64>,
    /// `local_ts_ns` of the first message.
    pub first_local_ts_ns: Option<u64>,
    /// `local_ts_ns` of the last message.
    pub last_local_ts_ns: Option<u64>,
    /// Loss records: counter gaps and counter regressions.
    pub loss_records: u64,
    /// Messages missing according to the counter gaps.
    pub missing_messages: u64,
}

/// Reads the journal in `dir` from its first record to the end committed when the reader gets there.
///
/// With `durable_only`, stops at the last checkpoint instead.
///
/// # Errors
///
/// [`Error::Journal`] if the journal can't be read; [`Error::Layout`] or [`Error::MissingDescriptor`] if a record of
/// this crate's schemas is malformed or names a descriptor that isn't there.
pub fn scan(dir: impl AsRef<Path>, durable_only: bool) -> Result<Info, Error> {
    let dir = dir.as_ref();
    let mut reader = Reader::open(dir, ReaderConfig::default().durable_only(durable_only))?;
    let mut catalog = Catalog::new(dir);
    let mut info = Info::default();
    let mut sessions: BTreeMap<u64, Session> = BTreeMap::new();
    let mut services: BTreeMap<String, Service> = BTreeMap::new();
    while let Some(record) = reader.try_next()? {
        let seq = record.seq;
        info.first_seq.get_or_insert(seq);
        info.last_seq = Some(seq);
        info.records += 1;
        let decoded = layout::decode(record.schema_id, record.payload)
            .map_err(|source| Error::Layout { seq, source })?;
        match decoded {
            Decoded::Descriptor(d) => {
                let session = sessions.entry(d.session_id).or_insert_with(|| Session {
                    session_id: d.session_id,
                    start_unix_ns: d.session_start_unix_ns,
                    recorder: d.recorder.clone(),
                    iceoryx2_version: d.iceoryx2_version.to_string(),
                    imported: d.imported,
                    services: Vec::new(),
                });
                if !session.services.contains(&d.service_name) {
                    session.services.push(d.service_name.clone());
                }
                let service = service_entry(&mut services, &d);
                service.descriptors += 1;
                service.payload_type = d.payload.name.clone();
                service.user_header_type = d.user_header.name.clone();
                catalog.insert(seq, *d);
            }
            Decoded::Data(data) => {
                let d = catalog.get(seq, data.descriptor_seq)?;
                let service = service_entry(&mut services, &d);
                service.messages += 1;
                service.payload_bytes += data.payload.len() as u64;
                service.first_seq.get_or_insert(seq);
                service.last_seq = Some(seq);
                service.first_local_ts_ns.get_or_insert(record.local_ts_ns);
                service.last_local_ts_ns = Some(record.local_ts_ns);
            }
            Decoded::Loss(loss) => {
                let d = catalog.get(seq, loss.descriptor_seq)?;
                let service = service_entry(&mut services, &d);
                service.loss_records += 1;
                service.missing_messages += loss.missing();
            }
            Decoded::Other => info.other_records += 1,
        }
    }
    info.sessions = sessions.into_values().collect();
    info.services = services.into_values().collect();
    Ok(info)
}

fn service_entry<'a>(
    services: &'a mut BTreeMap<String, Service>,
    d: &Descriptor,
) -> &'a mut Service {
    services
        .entry(d.service_name.clone())
        .or_insert_with(|| Service {
            name: d.service_name.clone(),
            payload_type: d.payload.name.clone(),
            user_header_type: d.user_header.name.clone(),
            descriptors: 0,
            messages: 0,
            payload_bytes: 0,
            first_seq: None,
            last_seq: None,
            first_local_ts_ns: None,
            last_local_ts_ns: None,
            loss_records: 0,
            missing_messages: 0,
        })
}
