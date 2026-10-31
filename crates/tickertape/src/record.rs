//! Record types.

/// A record to append. The writer assigns the sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewRecord<'a> {
    /// Caller-defined event type or schema identifier. Opaque to tickertape.
    pub schema_id: u32,
    /// Caller-supplied timestamp, by convention nanoseconds since the Unix epoch (e.g. the venue's event time).
    /// 0 means "not set".
    pub venue_ts_ns: u64,
    /// Caller-supplied timestamp, by convention nanoseconds since the Unix epoch (e.g. local receive time).
    /// 0 means "not set". See [`clock::realtime_ns`](crate::clock::realtime_ns).
    pub local_ts_ns: u64,
    /// The payload bytes.
    pub payload: &'a [u8],
}

/// A committed, validated record, borrowed from a reader's mapping.
///
/// The payload points directly into the memory-mapped segment (zero-copy). It stays valid until the next call on
/// the [`Reader`](crate::Reader) that returned it; the borrow checker enforces this. Use [`Record::to_owned`] to
/// keep it longer. Soundness relies on committed bytes never changing; see `docs/crash-contract.md` → Known risks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record<'a> {
    /// Sequence number: the record's position in the journal, starting at 0, without gaps.
    pub seq: u64,
    /// As given to [`NewRecord::schema_id`].
    pub schema_id: u32,
    /// As given to [`NewRecord::venue_ts_ns`].
    pub venue_ts_ns: u64,
    /// As given to [`NewRecord::local_ts_ns`].
    pub local_ts_ns: u64,
    /// The payload bytes, borrowed from the mapping.
    pub payload: &'a [u8],
}

impl Record<'_> {
    /// Copies the record out of the mapping.
    pub fn to_owned(&self) -> OwnedRecord {
        OwnedRecord {
            seq: self.seq,
            schema_id: self.schema_id,
            venue_ts_ns: self.venue_ts_ns,
            local_ts_ns: self.local_ts_ns,
            payload: self.payload.to_vec(),
        }
    }
}

/// A record copied out of the journal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OwnedRecord {
    /// Sequence number.
    pub seq: u64,
    /// Schema id.
    pub schema_id: u32,
    /// Venue timestamp, 0 if not set.
    pub venue_ts_ns: u64,
    /// Local timestamp, 0 if not set.
    pub local_ts_ns: u64,
    /// The payload bytes.
    pub payload: Vec<u8>,
}

impl OwnedRecord {
    /// Borrows this record as a [`Record`].
    pub fn as_record(&self) -> Record<'_> {
        Record {
            seq: self.seq,
            schema_id: self.schema_id,
            venue_ts_ns: self.venue_ts_ns,
            local_ts_ns: self.local_ts_ns,
            payload: &self.payload,
        }
    }
}
