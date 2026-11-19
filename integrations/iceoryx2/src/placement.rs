//! Appending records so that every data and loss record has the descriptor it names in its own segment. No
//! iceoryx2.
//!
//! The rules are in the layout docs ([where descriptors lie](crate::layout#where-descriptors-lie)); this module
//! implements them for the recorder and the importer. The core [`Writer`] has no call to start a new segment, so the
//! journal wrapper here predicts where each record goes with a [`SegmentMirror`] and, when the descriptors must
//! start a new segment, fills the rest of the active one with a filler record ([`SCHEMA_FILLER`]).

use std::path::Path;

use tickertape::{NewRecord, Writer, WriterConfig};

use crate::Error;
use crate::layout::{SCHEMA_DESCRIPTOR, SCHEMA_FILLER};
use crate::tail::SegmentMirror;

/// An open service's descriptor, as the journal tracks it.
#[derive(Debug)]
struct Slot {
    /// The encoded descriptor.
    bytes: Vec<u8>,
    /// Sequence number of its most recent copy.
    seq: u64,
    /// True if that copy is in the active segment.
    here: bool,
}

/// The journal writer, a mirror of its position, and the descriptor of each open service (by index).
#[derive(Debug)]
pub(crate) struct Journal {
    pub(crate) writer: Writer,
    mirror: SegmentMirror,
    slots: Vec<Option<Slot>>,
    descriptors: Vec<u64>,
    fillers: u64,
}

impl Journal {
    /// Opens the journal in `dir` for writing, with room for `services` services.
    ///
    /// # Errors
    ///
    /// [`Error::Journal`] if it can't be opened or read.
    pub(crate) fn open(
        dir: &Path,
        config: WriterConfig,
        services: usize,
    ) -> Result<Journal, Error> {
        let segment_size = config.segment_size;
        let writer = Writer::open(dir, config)?;
        let mirror = SegmentMirror::new(dir, segment_size)?;
        Ok(Journal::with(writer, mirror, services))
    }

    pub(crate) fn with(writer: Writer, mirror: SegmentMirror, services: usize) -> Journal {
        Journal {
            writer,
            mirror,
            slots: (0..services).map(|_| None).collect(),
            descriptors: vec![0; services],
            fillers: 0,
        }
    }

    /// Descriptor records written for service `i` so far: one per opening and one per new segment.
    pub(crate) fn descriptors(&self, i: usize) -> u64 {
        self.descriptors[i]
    }

    /// Filler records written so far.
    pub(crate) fn fillers(&self) -> u64 {
        self.fillers
    }

    /// Starts tracking service `i`, whose encoded descriptor is `descriptor`, and appends the descriptor. Returns its
    /// sequence number.
    ///
    /// # Errors
    ///
    /// [`Error::Journal`] if appending fails; service `i` is then not open.
    pub(crate) fn open_service(
        &mut self,
        i: usize,
        descriptor: Vec<u8>,
        local_ts_ns: u64,
    ) -> Result<u64, Error> {
        self.slots[i] = Some(Slot {
            bytes: descriptor,
            seq: 0,
            here: false,
        });
        match self.place(i, None, local_ts_ns) {
            Ok(()) => Ok(self.slot(i).seq),
            Err(e) => {
                self.slots[i] = None;
                Err(e)
            }
        }
    }

    /// Stops tracking service `i`.
    pub(crate) fn close_service(&mut self, i: usize) {
        self.slots[i] = None;
    }

    /// Appends a record of `len` payload bytes that names the descriptor of the open service `i`, after the
    /// descriptors its placement needs. `fill` writes the payload into a zeroed buffer, given the descriptor's
    /// sequence number; if it panics, nothing is committed (see [`Writer::append_with`]).
    ///
    /// # Errors
    ///
    /// [`Error::Journal`] if appending fails.
    pub(crate) fn append_named(
        &mut self,
        i: usize,
        record: &NewRecord<'_>,
        len: usize,
        fill: impl FnOnce(&mut [u8], u64),
    ) -> Result<u64, Error> {
        self.place(i, Some(len), record.local_ts_ns)?;
        let descriptor_seq = self.slot(i).seq;
        let seq = self
            .writer
            .append_with(record, len, |buf| fill(buf, descriptor_seq))?;
        self.appended(len);
        Ok(seq)
    }

    fn slot(&self, i: usize) -> &Slot {
        self.slots[i].as_ref().expect("the service is open")
    }

    /// Follows an append of `len` payload bytes; after a roll, no descriptor is in the active segment.
    fn appended(&mut self, len: usize) {
        if self.mirror.appended(len) {
            for slot in self.slots.iter_mut().flatten() {
                slot.here = false;
            }
        }
    }

    fn append_descriptor(&mut self, i: usize, local_ts_ns: u64) -> Result<(), Error> {
        let slot = self.slots[i].as_ref().expect("the service is open");
        let len = slot.bytes.len();
        let seq = self.writer.append(&NewRecord {
            schema_id: SCHEMA_DESCRIPTOR,
            venue_ts_ns: 0,
            local_ts_ns,
            payload: &slot.bytes,
        })?;
        self.appended(len);
        let slot = self.slots[i].as_mut().expect("the service is open");
        slot.seq = seq;
        slot.here = true;
        self.descriptors[i] += 1;
        Ok(())
    }

    /// Appends the descriptors a record of service `i` with `record` payload bytes needs before it (for `None`, the
    /// record is service `i`'s own descriptor, which this appends):
    ///
    /// 1. If the service's descriptor is already in the active segment (and a record follows), and the record fits
    ///    there, nothing. If the descriptor isn't there but it and the record both fit, the descriptor.
    /// 2. Otherwise the record goes into a new segment, which starts with the descriptors of all open services, in
    ///    service order, if they and the record fit in a new segment together; otherwise with the descriptor of
    ///    service `i` alone, if it and the record fit; otherwise the record is too large to share a segment with its
    ///    descriptor and is appended as it is (nothing is appended here).
    /// 3. To make those descriptors start the new segment: if the first of them would still fit in the active
    ///    segment, a filler record first takes exactly the space left there.
    fn place(&mut self, i: usize, record: Option<usize>, local_ts_ns: u64) -> Result<(), Error> {
        let len_of = |slots: &[Option<Slot>], j: usize| slots[j].as_ref().map(|s| s.bytes.len());
        let own = self.slot(i).bytes.len();
        let need_own = record.is_none() || !self.slot(i).here;
        let together: Vec<usize> = need_own.then_some(own).into_iter().chain(record).collect();
        if self.mirror.fits(&together) {
            if need_own {
                self.append_descriptor(i, local_ts_ns)?;
            }
            return Ok(());
        }
        let all: Vec<usize> = (0..self.slots.len())
            .filter(|&j| self.slots[j].is_some())
            .collect();
        let lens = |group: &[usize]| -> Vec<usize> {
            group
                .iter()
                .filter_map(|&j| len_of(&self.slots, j))
                .chain(record)
                .collect()
        };
        let group = if self.mirror.fits_fresh(&lens(&all)) {
            all
        } else if self.mirror.fits_fresh(&lens(&[i])) {
            vec![i]
        } else {
            // Too large to share a segment with its descriptor (the documented exception). An opening descriptor
            // that doesn't fit in a segment by itself is appended anyway, and the writer refuses it.
            if record.is_none() {
                self.append_descriptor(i, local_ts_ns)?;
            }
            return Ok(());
        };
        let first = len_of(&self.slots, group[0]).expect("open");
        for len in self.mirror.filler_lens(first) {
            self.writer.append_with(
                &NewRecord {
                    schema_id: SCHEMA_FILLER,
                    venue_ts_ns: 0,
                    local_ts_ns,
                    payload: &[],
                },
                len,
                // The buffer is already zeroed: a filler holds zeros.
                |_| {},
            )?;
            self.appended(len);
            self.fillers += 1;
        }
        for j in group {
            self.append_descriptor(j, local_ts_ns)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use tickertape::format::{SEGMENT_HEADER_LEN, SEGMENT_TRAILER_LEN, align8, frame_len};
    use tickertape::{Reader, ReaderConfig};

    use super::*;
    use crate::layout::{
        DataRecord, Decoded, Descriptor, SCHEMA_DATA, ServiceSettings, TypeInfo, TypeVariant,
        Version,
    };

    const SEGMENT: u64 = 65_536;
    const AREA: u64 = SEGMENT - SEGMENT_TRAILER_LEN as u64 - SEGMENT_HEADER_LEN as u64;

    /// Bytes a record with this payload takes in a segment.
    fn frame(len: usize) -> u64 {
        align8(u64::from(frame_len(len).unwrap()))
    }

    /// An encoded descriptor of a `[u8]` service without user header, named with `name_len` bytes.
    fn descriptor(name_len: usize) -> Vec<u8> {
        let t = |variant, name: &str, size| TypeInfo {
            variant,
            name: name.into(),
            size,
            alignment: 1,
        };
        Descriptor {
            run_start_seq: 0,
            run_start_unix_ns: 0,
            imported: false,
            iceoryx2_version: Version {
                major: 0,
                minor: 10,
                patch: 0,
            },
            settings: ServiceSettings::UNKNOWN,
            recorder_buffer_size: 0,
            counter: None,
            payload: t(TypeVariant::Dynamic, "u8", 1),
            user_header: t(TypeVariant::FixedSize, "()", 0),
            system_header: t(TypeVariant::FixedSize, "S", 48),
            service_name: "s".repeat(name_len),
            recorder: "test".into(),
        }
        .encode()
        .unwrap()
    }

    /// Payload length of a data record holding a message of `m` bytes.
    fn data_len(m: usize) -> usize {
        crate::layout::DATA_HEADER_LEN + m
    }

    fn journal(dir: &Path, names: &[usize]) -> Journal {
        let config = WriterConfig::default()
            .segment_size(SEGMENT)
            .prefault(false);
        let mut j = Journal::open(dir, config, names.len()).unwrap();
        for (i, &n) in names.iter().enumerate() {
            j.open_service(i, descriptor(n), 1).unwrap();
        }
        j
    }

    fn data(j: &mut Journal, i: usize, m: usize) {
        let payload = vec![0xA5; m];
        j.append_named(
            i,
            &NewRecord {
                schema_id: SCHEMA_DATA,
                venue_ts_ns: 0,
                local_ts_ns: 1,
                payload: &[],
            },
            data_len(m),
            |buf, descriptor_seq| {
                DataRecord {
                    descriptor_seq,
                    publisher_id: 0,
                    user_header: &[],
                    payload: &payload,
                }
                .encode_into(buf)
                .unwrap()
            },
        )
        .unwrap();
    }

    /// Appends a record of another schema, as another writer could.
    fn pad(j: &mut Journal, len: usize) {
        j.writer
            .append(&NewRecord {
                schema_id: 7,
                venue_ts_ns: 0,
                local_ts_ns: 1,
                payload: &vec![0; len],
            })
            .unwrap();
        j.appended(len);
    }

    /// Pads the active segment so that exactly `left` bytes remain for records.
    fn leave(j: &mut Journal, left: u64) {
        let have = j.mirror.left();
        assert!(have >= left + 40, "{have} {left}");
        pad(j, (have - left - 40) as usize);
        assert_eq!(j.mirror.left(), left);
    }

    #[derive(Debug, Default, PartialEq, Eq)]
    struct Check {
        segments: usize,
        data: u64,
        fillers: u64,
        exceptions: u64,
        not_leading: u64,
    }

    /// Reads the journal back and checks the placement rules: every data record has the descriptor it names in its
    /// own segment, before it, unless both can't share a segment; a filler holds zeros and ends exactly where its
    /// segment's trailer starts. Counts segments whose data names a descriptor that isn't among those the segment
    /// starts with.
    fn check(dir: &Path) -> Check {
        let segments = tickertape::inspect::list_segments(dir).unwrap();
        let mut out = Check {
            segments: segments.len(),
            ..Check::default()
        };
        let mut lens = HashMap::new();
        let mut reader = Reader::open(dir, ReaderConfig::default()).unwrap();
        let mut k = 0;
        let mut used = SEGMENT_HEADER_LEN as u64;
        let mut here = HashSet::new();
        let mut leading = HashSet::new();
        let mut in_lead = true;
        let mut not_leading = false;
        let mut filled = false;
        while let Some(r) = reader.try_next().unwrap() {
            if segments.get(k + 1).is_some_and(|s| r.seq >= s.base_seq) {
                k += 1;
                used = SEGMENT_HEADER_LEN as u64;
                here.clear();
                leading.clear();
                in_lead = true;
                out.not_leading += u64::from(std::mem::take(&mut not_leading));
                filled = false;
            }
            assert!(!filled, "record {} follows a filler in its segment", r.seq);
            used += frame(r.payload.len());
            if r.schema_id != SCHEMA_DESCRIPTOR {
                in_lead = false;
            }
            match crate::layout::decode(r.schema_id, r.payload).unwrap() {
                Decoded::Descriptor(_) => {
                    lens.insert(r.seq, r.payload.len());
                    here.insert(r.seq);
                    if in_lead {
                        leading.insert(r.seq);
                    }
                }
                Decoded::Data(d) => {
                    out.data += 1;
                    if here.contains(&d.descriptor_seq) {
                        // Only the descriptor's sequence number tells services apart here; every service's
                        // descriptor is written in the leading run of a segment it starts.
                        not_leading |= !leading.contains(&d.descriptor_seq);
                        continue;
                    }
                    let together = SEGMENT_HEADER_LEN as u64
                        + frame(lens[&d.descriptor_seq])
                        + frame(r.payload.len());
                    assert!(
                        together > SEGMENT - SEGMENT_TRAILER_LEN as u64,
                        "data record {} ({} bytes) is not in the segment of its descriptor {}",
                        r.seq,
                        r.payload.len(),
                        d.descriptor_seq
                    );
                    out.exceptions += 1;
                }
                Decoded::Filler(_) => {
                    assert!(r.payload.iter().all(|b| *b == 0));
                    assert_eq!(
                        used,
                        SEGMENT - SEGMENT_TRAILER_LEN as u64,
                        "filler {}",
                        r.seq
                    );
                    out.fillers += 1;
                    filled = true;
                }
                _ => {}
            }
        }
        out.not_leading += u64::from(not_leading);
        out
    }

    /// The largest message that shares a new segment with these descriptors.
    fn largest_shared(names: &[usize]) -> usize {
        let d: u64 = names.iter().map(|&n| frame(descriptor(n).len())).sum();
        (AREA - d - frame(data_len(0))) as usize
    }

    /// The largest message a 64 KiB segment holds.
    fn largest() -> usize {
        tickertape::format::max_payload_len(SEGMENT) as usize - data_len(0)
    }

    /// One service, messages from 1 byte to the largest a segment holds: every data record has its descriptor in its
    /// own segment (exactly the messages too large to share a segment with it don't), and every segment starts with
    /// the descriptor.
    #[test]
    fn every_message_size_from_one_byte_to_the_largest() {
        let names = [40];
        let shared = largest_shared(&names);
        let mut sizes: Vec<usize> = (1..=48).collect();
        sizes.extend((49..3_000).step_by(37));
        sizes.extend([1000, 1008, 1024, 4096, 20_000]);
        for edge in [shared, largest()] {
            sizes.extend(edge - 9..=edge);
        }
        sizes.push(shared + 1);
        sizes.push(shared + 8);
        let mut fillers = 0;
        for m in sizes {
            let dir = tempfile::tempdir().unwrap();
            let mut j = journal(dir.path(), &names);
            let n = (4 * SEGMENT / frame(data_len(m))).max(6);
            for _ in 0..n {
                data(&mut j, 0, m);
            }
            drop(j);
            let c = check(dir.path());
            assert!(c.segments >= 4, "{m}: {c:?}");
            assert_eq!(c.data, n);
            assert_eq!(c.not_leading, 0, "{m}: {c:?}");
            let expected = if m > shared { n } else { 0 };
            assert_eq!(c.exceptions, expected, "{m}: {c:?}");
            fillers += c.fillers;
        }
        assert!(fillers > 100, "{fillers}");
    }

    /// Every amount of space left at the end of a segment, from none to more than any descriptor needs, before a
    /// record that doesn't fit there: with one service and with two whose descriptors differ in size. A filler is
    /// written exactly when the first descriptor would still fit (so it is never smaller than a record), then the
    /// descriptors start the new segment, followed by the record; when the record is too large to share a new
    /// segment with all descriptors, the segment starts with its own descriptor only; when it is too large to share
    /// one with its own descriptor, it is appended as it is, without a filler.
    #[test]
    fn every_amount_of_space_left() {
        for names in [vec![40], vec![10, 200]] {
            let first = frame(descriptor(names[0]).len());
            let shared_all = largest_shared(&names);
            let shared_own = largest_shared(&names[..1]);
            for left in (0..=first + 64).step_by(8) {
                // A record just too large for what is left, a record of about 1 KiB, one that shares a segment with
                // its own descriptor only (two services), and one too large for even that.
                let just_over = (left as usize + 8).saturating_sub(frame(data_len(0)) as usize);
                let mut cases = vec![just_over, 1000.max(left as usize), shared_all];
                if names.len() > 1 {
                    cases.push(shared_all + 8);
                }
                cases.push(shared_own + 1);
                for m in cases {
                    if frame(data_len(m)) <= left {
                        continue;
                    }
                    let dir = tempfile::tempdir().unwrap();
                    let mut j = journal(dir.path(), &names);
                    leave(&mut j, left);
                    let before = j.fillers();
                    data(&mut j, 0, m);
                    let filled = j.fillers() - before;
                    let exception = m > shared_own;
                    let expect_filler = !exception && first <= left;
                    assert_eq!(filled, u64::from(expect_filler), "left {left}, message {m}");
                    if !exception {
                        // The record opened a new segment, after its own descriptor, and after every service's
                        // descriptor if they all fit with it.
                        assert!(j.slot(0).here, "left {left}, message {m}");
                        let all_fit = m <= shared_all;
                        for i in 1..names.len() {
                            assert_eq!(j.slot(i).here, all_fit, "left {left}, message {m}");
                        }
                    }
                    for i in 0..names.len() {
                        data(&mut j, i, 8);
                    }
                    drop(j);
                    let c = check(dir.path());
                    assert_eq!(
                        c.exceptions,
                        u64::from(exception),
                        "left {left}, message {m}: {c:?}"
                    );
                }
            }
        }
    }

    /// A service opened in the middle of a run: its descriptor goes where there is room for it; if there isn't, the
    /// new segment starts with every open service's descriptor, the new one last.
    #[test]
    fn opening_a_service_at_every_amount_of_space_left() {
        let names = [30, 150];
        let second = frame(descriptor(names[1]).len());
        for left in (0..=second + 64).step_by(8) {
            let dir = tempfile::tempdir().unwrap();
            let mut j = journal(dir.path(), &names[..1]);
            j.slots.push(None);
            j.descriptors.push(0);
            data(&mut j, 0, 100);
            leave(&mut j, left);
            let segments = tickertape::inspect::list_segments(dir.path())
                .unwrap()
                .len();
            let seq = j.open_service(1, descriptor(names[1]), 1).unwrap();
            let rolled = tickertape::inspect::list_segments(dir.path())
                .unwrap()
                .len()
                > segments;
            assert_eq!(rolled, second > left, "left {left}");
            assert!(j.slot(0).here && j.slot(1).here);
            assert_eq!(j.slot(1).seq, seq);
            if rolled {
                assert!(
                    j.slot(0).seq < seq,
                    "the new segment starts with the open service's descriptor"
                );
            }
            for _ in 0..200 {
                data(&mut j, 0, 700);
                data(&mut j, 1, 900);
            }
            drop(j);
            let c = check(dir.path());
            assert_eq!(c.exceptions, 0, "left {left}: {c:?}");
            assert!(c.not_leading <= 1, "left {left}: {c:?}");
        }
    }

    /// A service closed and opened again writes a new descriptor before its next record, even within a segment.
    #[test]
    fn a_reopened_service_writes_its_descriptor_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut j = journal(dir.path(), &[20]);
        data(&mut j, 0, 10);
        let first = j.slot(0).seq;
        j.close_service(0);
        let again = j.open_service(0, descriptor(20), 1).unwrap();
        assert!(again > first);
        data(&mut j, 0, 10);
        assert_eq!(j.descriptors(0), 2);
        drop(j);
        let c = check(dir.path());
        assert_eq!((c.data, c.exceptions), (2, 0));
    }
}
