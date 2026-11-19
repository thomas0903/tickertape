//! Summarize a recording: recording runs, services, message counts, time ranges and reported losses. No iceoryx2.

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
    /// Filler records: zeros that end a segment early so that the descriptors start the next one. Not data.
    pub filler_records: u64,
    /// Payload bytes of the filler records.
    pub filler_bytes: u64,
    /// Data and loss records skipped because the descriptor they name is no longer in the journal (retention or
    /// archiving removed its segment).
    pub undescribed_records: u64,
    /// Recording runs, by `run_start_seq`, as far as their descriptors are still in the journal.
    pub runs: Vec<Run>,
    /// Recorded services, by name.
    pub services: Vec<Service>,
}

/// One recording run: one run of the recorder, or one import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Run {
    /// The journal's next sequence number when the run began.
    pub run_start_seq: u64,
    /// When it began, nanoseconds since the Unix epoch.
    pub start_unix_ns: u64,
    /// Who recorded it.
    pub recorder: String,
    /// iceoryx2 version on the recording side.
    pub iceoryx2_version: String,
    /// True if imported from an iceoryx2 recording file (times reconstructed).
    pub imported: bool,
    /// Services that have a descriptor in this run.
    pub services: Vec<String>,
    /// Descriptor records of this run still in the journal (repeated in every segment that holds the run's records).
    pub descriptors: u64,
}

/// One recorded service, across runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Service {
    /// Service name.
    pub name: String,
    /// Payload type name, from the latest descriptor.
    pub payload_type: String,
    /// User header type name, from the latest descriptor.
    pub user_header_type: String,
    /// Recording runs with a descriptor of this service still in the journal.
    pub runs: u64,
    /// Descriptor records (repeated in every segment that holds the service's records, so several per run).
    pub descriptors: u64,
    /// Messages recorded (whose descriptor is still in the journal).
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
    /// Messages missing according to the counter gaps (saturating).
    pub missing_messages: u64,
    #[serde(skip)]
    last_run: Option<u64>,
}

/// Reads the journal in `dir` from its first record to the end committed when the reader gets there.
///
/// With `durable_only`, stops at the last checkpoint instead. Records whose descriptor is no longer in the journal
/// are counted in [`Info::undescribed_records`] and otherwise skipped.
///
/// # Errors
///
/// [`Error::Journal`] if the journal can't be read; [`Error::Layout`] or [`Error::MissingDescriptor`] if a record of
/// this crate's schemas is malformed, doesn't match its descriptor, or names a record that isn't a descriptor.
pub fn scan(dir: impl AsRef<Path>, durable_only: bool) -> Result<Info, Error> {
    let dir = dir.as_ref();
    let mut reader = Reader::open(dir, ReaderConfig::default().durable_only(durable_only))?;
    let mut catalog = Catalog::new(dir);
    let mut info = Info::default();
    let mut runs: BTreeMap<u64, Run> = BTreeMap::new();
    let mut services: BTreeMap<String, Service> = BTreeMap::new();
    while let Some(record) = reader.try_next()? {
        let seq = record.seq;
        info.first_seq.get_or_insert(seq);
        info.last_seq = Some(seq);
        info.records += 1;
        let layout_error = |source| Error::Layout { seq, source };
        match layout::decode(record.schema_id, record.payload).map_err(layout_error)? {
            Decoded::Descriptor(d) => {
                let run = runs.entry(d.run_start_seq).or_insert_with(|| Run {
                    run_start_seq: d.run_start_seq,
                    start_unix_ns: d.run_start_unix_ns,
                    recorder: d.recorder.clone(),
                    iceoryx2_version: d.iceoryx2_version.to_string(),
                    imported: d.imported,
                    services: Vec::new(),
                    descriptors: 0,
                });
                run.descriptors += 1;
                if !run.services.contains(&d.service_name) {
                    run.services.push(d.service_name.clone());
                }
                let service = service_entry(&mut services, &d);
                service.descriptors += 1;
                if service.last_run != Some(d.run_start_seq) {
                    service.runs += 1;
                    service.last_run = Some(d.run_start_seq);
                }
                service.payload_type = d.payload.name.clone();
                service.user_header_type = d.user_header.name.clone();
                catalog.insert(seq, *d);
            }
            Decoded::Data(data) => {
                let Some(d) = catalog.resolve(seq, data.descriptor_seq)? else {
                    info.undescribed_records += 1;
                    continue;
                };
                data.check(&d).map_err(layout_error)?;
                let service = service_entry(&mut services, &d);
                service.messages += 1;
                service.payload_bytes = service
                    .payload_bytes
                    .saturating_add(data.payload.len() as u64);
                service.first_seq.get_or_insert(seq);
                service.last_seq = Some(seq);
                service.first_local_ts_ns.get_or_insert(record.local_ts_ns);
                service.last_local_ts_ns = Some(record.local_ts_ns);
            }
            Decoded::Loss(loss) => {
                let Some(d) = catalog.resolve(seq, loss.descriptor_seq)? else {
                    info.undescribed_records += 1;
                    continue;
                };
                loss.check(&d).map_err(layout_error)?;
                let service = service_entry(&mut services, &d);
                service.loss_records += 1;
                service.missing_messages = service.missing_messages.saturating_add(loss.missing());
            }
            Decoded::Filler(len) => {
                info.filler_records += 1;
                info.filler_bytes = info.filler_bytes.saturating_add(len as u64);
            }
            Decoded::Other => info.other_records += 1,
        }
    }
    info.runs = runs.into_values().collect();
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
            runs: 0,
            descriptors: 0,
            messages: 0,
            payload_bytes: 0,
            first_seq: None,
            last_seq: None,
            first_local_ts_ns: None,
            last_local_ts_ns: None,
            loss_records: 0,
            missing_messages: 0,
            last_run: None,
        })
}
