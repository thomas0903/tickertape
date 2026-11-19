//! The payload layout of recorded iceoryx2 messages, version 1. No iceoryx2 dependency.
//!
//! A recording is an ordinary tickertape journal. This crate writes four kinds of records into it, told apart by
//! the record's `schema_id`. Schema ids `0x4932_0000..=0x4932_FFFF` (`"I2"` followed by a 16-bit number) are
//! reserved for this crate; other schema ids may be mixed into the same journal by other writers and are ignored.
//!
//! | schema id     | constant              | payload                                      |
//! |---------------|-----------------------|----------------------------------------------|
//! | `0x4932_0001` | [`SCHEMA_DESCRIPTOR`] | [`Descriptor`]: one recorded service         |
//! | `0x4932_0002` | [`SCHEMA_DATA`]       | [`DataRecord`]: one message                  |
//! | `0x4932_0003` | [`SCHEMA_LOSS`]       | [`LossRecord`]: a gap in a publisher counter |
//! | `0x4932_0004` | [`SCHEMA_FILLER`]     | filler: zeros that end a segment early       |
//!
//! All integers are little-endian. Strings are UTF-8 with a `u16` byte length in front and no terminator. User
//! header and payload bytes are copied exactly as the publisher wrote them: they keep the publisher's native
//! representation (byte order, padding, layout of its Rust or C types), which this layout does not describe.
//!
//! The record's own fields carry the timestamps: `local_ts_ns` is when the recorder received the message
//! (nanoseconds since the Unix epoch, from the system clock), and `venue_ts_ns` is what an optional extractor read
//! from the user header, or 0.
//!
//! # Nothing read is trusted
//!
//! Every value a decoder reads is checked before it is used: [`Descriptor::decode`] applies [`Descriptor::validate`]
//! (type sizes, alignments, the iceoryx2 chunk layout they imply, service settings, the counter field), and a data
//! or loss record is checked against the descriptor it names ([`DataRecord::check`], [`LossRecord::check`]).
//! [`Descriptor::encode`] refuses everything `decode` refuses. The rules are listed under [Validity
//! rules](#validity-rules).
//!
//! # Growth rule
//!
//! Version 1 can grow without breaking version-1 readers:
//!
//! - **Layout version.** Changes only for incompatible changes. A reader rejects a version it doesn't know.
//! - **Flags** (`u16` in every record). Bits 0 to 7 are *required*: a reader rejects a record with a required bit
//!   it doesn't know. Bits 8 to 15 are *optional*: a reader ignores optional bits it doesn't know. Writers set only
//!   bits defined for the version they write. Version 1 defines one bit: descriptor bit 0, `imported`.
//! - **Reserved fields** are written as 0 and ignored by readers. A later revision may use one only for a meaning
//!   that a reader ignoring it still decodes correctly, or behind a required flag bit.
//! - **Extension entries.** Bytes after a descriptor's recorder name, after a loss record's 48 bytes, and between a
//!   data record's 40-byte header and its user header (the header length field says where the user header starts)
//!   are extension entries: `u16` tag, `u16` reserved, `u32` length, then that many bytes, repeated until the end of
//!   the area. Tag bit 15 means *required*: a reader rejects a record with a required tag it doesn't know and skips
//!   optional tags it doesn't know. An entry that runs past the end of its area is malformed. Version 1 defines no
//!   tags; its writers write no entries.
//!
//! # Recording runs and descriptors
//!
//! A *recording run* is one run of a recorder (or one import). It is identified by `run_start_seq`, the journal's
//! next sequence number when the run began; all descriptors of a run carry it. A service's descriptor may appear
//! several times in a run, with the same content (see below). A data or loss record names the most recent descriptor
//! of its service written before it.
//!
//! # Where descriptors lie
//!
//! tickertape segments can be removed from the front of a journal (retention, `tickertape archive`), so a record is
//! only decodable on its own if its descriptor is in the same segment. The recorder and the importer keep this rule:
//!
//! **Every data and loss record has the descriptor it names in its own segment, before it**, except a record too
//! large to share a segment with that descriptor: one with `64 + align8(40 + d) + align8(40 + r) > segment size − 8`,
//! where `d` is the descriptor's length and `r` the record's (tickertape's segment header is 64 bytes, its record
//! header 40, records are 8-byte aligned and the last 8 bytes of a segment hold no record; `docs/format.md`). For
//! 64 KiB segments and a 400-byte descriptor that is a record payload larger than 64,984 bytes; for the default
//! 128 MiB segments, about 128 MiB.
//!
//! So every segment that remains after older ones were removed decodes on its own, and every segment the recorder
//! starts begins with the descriptors of the services open at that point. How it is done, before each record that
//! names a descriptor (and before a service's first descriptor):
//!
//! 1. If the record fits in what is left of the active segment, together with its service's descriptor when that
//!    isn't in the segment yet, they are appended there.
//! 2. Otherwise the record must start a new segment. The new segment begins with the descriptors of all open
//!    services, in the order the services were configured, then the record, if they all fit in one segment together;
//!    otherwise with the record's own descriptor only, then the record (the other services' descriptors are then
//!    written before their next record, by rule 1); and if not even that fits, the record is too large to share a
//!    segment with its descriptor and is appended as it is, wherever the writer puts it (the exception above).
//! 3. tickertape's writer starts a new segment only when a record doesn't fit. So that the first of those
//!    descriptors starts the new segment, the recorder first appends a **filler record** taking exactly the space
//!    left, if that descriptor would still fit there. If it wouldn't (less space left than that descriptor takes,
//!    including less than the 40 bytes any record needs), the descriptor starts the new segment by itself and no
//!    filler is written. A filler is therefore never smaller than a descriptor, and so never smaller than the smallest
//!    record.
//!
//! The space left is the segment size minus 8, minus the bytes in use: 64 for the header plus `align8(40 + payload
//! length)` for each record. The recorder reads the newest segment when it opens the journal, then adds each record
//! it appends; when a record didn't fit, the writer started a new segment of its configured size and the count
//! starts again at 64. The rules come from tickertape's public `format` module (`frame_len`, `align8`, `record_fits`,
//! `SEGMENT_HEADER_LEN`, `SEGMENT_TRAILER_LEN`), and tests check them against a real writer.
//!
//! A reader that finds a record whose descriptor is no longer in the journal (written otherwise, or the exception)
//! skips it, counts it and reports it; it must not stop. `info`, `replay`, `export` and the `follow` example do that.
//!
//! # Filler (`SCHEMA_FILLER`)
//!
//! A record whose payload is all zero bytes, of any length (0 up to tickertape's largest payload). It carries no
//! data and names no descriptor. Readers skip it without looking at its payload; it is the last record of its
//! segment. Version-1 readers written before fillers existed decode its schema id as an unknown reserved id and
//! ignore it too, so fillers need no new layout version. `info` counts fillers separately from data
//! (`filler_records`, `filler_bytes`).
//!
//! # Descriptor (`SCHEMA_DESCRIPTOR`)
//!
//! | offset | size | field                                                                                   |
//! |-------:|-----:|-----------------------------------------------------------------------------------------|
//! |      0 |    4 | magic, the bytes `TTI2`                                                                 |
//! |      4 |    2 | layout version, 1                                                                       |
//! |      6 |    2 | flags: bit 0 `imported` (required class), see below; no other bit is defined             |
//! |      8 |    8 | `run_start_seq`: the journal's next sequence number when the recording run began         |
//! |     16 |    8 | run start, nanoseconds since the Unix epoch                                             |
//! |     24 |    6 | iceoryx2 version of the recording side: major, minor, patch, `u16` each                  |
//! |     30 |    1 | messaging pattern: 1 = publish-subscribe, the only value in version 1                    |
//! |     31 |    1 | safe overflow: 1 = a full subscriber buffer drops its oldest sample; 0 = it doesn't, and the publisher's backpressure strategy applies |
//! |     32 |    8 | service `history_size`                                                                   |
//! |     40 |    8 | service `subscriber_max_buffer_size`                                                     |
//! |     48 |    8 | service `subscriber_max_borrowed_samples`                                                |
//! |     56 |    8 | service `max_publishers`                                                                 |
//! |     64 |    8 | service `max_subscribers`                                                                |
//! |     72 |    8 | service `max_nodes`                                                                      |
//! |     80 |    8 | the recorder subscriber's buffer size                                                    |
//! |     88 |    1 | publisher counter location: 0 none, 1 user header, 2 payload                             |
//! |     89 |    1 | publisher counter width in bytes: 0 (no counter), 4 or 8; an unsigned little-endian integer |
//! |     90 |    2 | reserved                                                                                 |
//! |     92 |    4 | publisher counter offset in bytes, from the start of the user header or payload; 0 without a counter |
//! |     96 |  var | three type details, in this order: payload, user header, iceoryx2's system header       |
//! |        |  var | service name: `u16` length, then UTF-8 bytes                                             |
//! |        |  var | recorder: `u16` length, then UTF-8 bytes (for example `tickertape-iceoryx2 0.1.0`)       |
//! |        |  var | extension entries, to the end of the record (none in version 1)                          |
//!
//! `imported` means the run was imported from a file that only had times relative to its start: the records'
//! `local_ts_ns` are the import's chosen start time plus the relative time, not when the messages arrived. A service
//! setting of 0 means unknown (an import); the recorder buffer size is then 0 too.
//!
//! Each type detail is iceoryx2's `TypeDetail`:
//!
//! | offset | size | field                                                     |
//! |-------:|-----:|-----------------------------------------------------------|
//! |      0 |    1 | variant: 0 fixed size, 1 dynamic (a slice of elements)     |
//! |      1 |    1 | reserved                                                  |
//! |      2 |    2 | type name length in bytes                                 |
//! |      4 |    4 | reserved                                                  |
//! |      8 |    8 | size in bytes (of one element, for a dynamic payload)      |
//! |     16 |    8 | alignment in bytes                                        |
//! |     24 |  var | type name, UTF-8                                          |
//!
//! # Data record (`SCHEMA_DATA`)
//!
//! | offset   | size | field                                                                       |
//! |---------:|-----:|-----------------------------------------------------------------------------|
//! |        0 |    2 | layout version, 1                                                           |
//! |        2 |    2 | flags; none defined                                                         |
//! |        4 |    2 | header length `H`: where the user header starts; 40 in version 1, at least 40 |
//! |        6 |    2 | reserved                                                                    |
//! |        8 |    4 | user header length `h`                                                      |
//! |       12 |    4 | reserved                                                                    |
//! |       16 |    8 | sequence number of the descriptor this record names                         |
//! |       24 |   16 | iceoryx2 publisher id (`u128`, iceoryx2's sample origin); 0 if unknown (imported) |
//! |       40 |  var | extension entries, up to `H` (none in version 1)                            |
//! |      `H` |  `h` | user header bytes                                                           |
//! |  `H + h` | rest | payload bytes                                                               |
//!
//! The payload length is the record's payload length minus `H + h`. A dynamic payload holds `payload length /
//! element size` elements. Bytes are copied exactly as they are in shared memory, including any padding inside the
//! user's types. Neither part is aligned inside the record: copy before reinterpreting.
//!
//! # Loss record (`SCHEMA_LOSS`)
//!
//! Written only when the recorder was told where a publisher-side message counter is (see the descriptor's counter
//! fields), before the message that revealed the gap.
//!
//! | offset | size | field                                                                         |
//! |-------:|-----:|-------------------------------------------------------------------------------|
//! |      0 |    2 | layout version, 1                                                             |
//! |      2 |    2 | flags; none defined                                                           |
//! |      4 |    1 | kind: 1 gap (counter jumped forward), 2 backwards (counter repeated or reset)  |
//! |      5 |    1 | counter width in bytes, 4 or 8 (the descriptor's)                              |
//! |      6 |    2 | reserved                                                                      |
//! |      8 |    8 | sequence number of the descriptor this record names                           |
//! |     16 |   16 | iceoryx2 publisher id (`u128`)                                                |
//! |     32 |    8 | expected counter value                                                        |
//! |     40 |    8 | received counter value                                                        |
//! |     48 |  var | extension entries, to the end of the record (none in version 1)               |
//!
//! Counters are compared modulo 2^(8 × width): the distance `d = (received - expected) mod 2^(8 × width)` is a gap
//! of `d` missing messages if `0 < d < 2^(8 × width - 1)`, and backwards otherwise. So a four-byte counter that
//! wraps from `0xFFFF_FFFF` to 0 is neither.
//!
//! # Validity rules
//!
//! A decoder must apply all of these. This module does.
//!
//! - Layout version 1; descriptor magic `TTI2`; no unknown required flag bit or extension tag; every extension entry
//!   inside its area; descriptor strings valid UTF-8; nothing missing.
//! - Enumerations: type variant 0 or 1; messaging pattern 1; safe overflow 0 or 1; counter location 0, 1 or 2 with
//!   width 0 exactly when the location is 0, otherwise 4 or 8, and offset 0 without a counter; loss kind 1 or 2.
//! - Each type: alignment a power of two from 1 to [`MAX_ALIGNMENT`]; size a multiple of its alignment and at most
//!   [`MAX_RECORD_PAYLOAD`]; name at most [`MAX_TYPE_NAME_LEN`] bytes. The user header and system header are
//!   fixed-size. A dynamic payload's element size is not 0 (every element count of zero-sized elements has the same
//!   bytes, so the count would be lost). A data record of the user header and one payload element fits in a
//!   tickertape record.
//! - The iceoryx2 chunk the types imply, for the largest element count a data record can hold (system header, user
//!   header aligned to its alignment, payload aligned to its alignment, element size rounded up to the payload's
//!   alignment times the count, all rounded up to the largest alignment), computed without overflow, is a valid Rust
//!   `Layout`.
//! - Service settings at most: `history_size` [`MAX_HISTORY_SIZE`], `subscriber_max_buffer_size` and the recorder's
//!   buffer [`MAX_BUFFER_SIZE`], `subscriber_max_borrowed_samples` [`MAX_BORROWED_SAMPLES`], `max_publishers`,
//!   `max_subscribers` and `max_nodes` [`MAX_PORTS`]; `max_subscribers × (buffer + borrowed) + history` at most
//!   [`MAX_SAMPLES_PER_PUBLISHER`]; `max_publishers × max_subscribers × (buffer + borrowed)` at most
//!   [`MAX_CONNECTION_SLOTS`].
//! - Service name 1 to [`MAX_SERVICE_NAME_LEN`] bytes.
//! - Counter: `offset + width` at most the size of the part it is in (the user header, or one payload element).
//! - Data record: `H` at least 40; user header inside the record; user header length equal to the descriptor's user
//!   header size; payload length equal to the payload size, or a multiple of the element size for a slice.
//! - Loss record: counter width equal to the descriptor's; the descriptor has a counter; expected and received fit
//!   in the width and differ; the kind matches the distance rule above.
//!
//! This crate's readers (`info`, `replay`, `export` and the `follow` example) apply every rule to every record they
//! read, including loss records they don't otherwise use.

use serde::Serialize;

/// Schema id of a [`Descriptor`] record.
pub const SCHEMA_DESCRIPTOR: u32 = 0x4932_0001;
/// Schema id of a [`DataRecord`].
pub const SCHEMA_DATA: u32 = 0x4932_0002;
/// Schema id of a [`LossRecord`].
pub const SCHEMA_LOSS: u32 = 0x4932_0003;
/// Schema id of a filler record: zero bytes that end a segment early, so that the descriptors start the next one
/// (see [where descriptors lie](self#where-descriptors-lie)). Readers skip it without looking at its payload.
pub const SCHEMA_FILLER: u32 = 0x4932_0004;
/// Schema ids reserved for this crate.
pub const SCHEMA_RESERVED: core::ops::RangeInclusive<u32> = 0x4932_0000..=0x4932_FFFF;

/// The layout version this module reads and writes.
pub const LAYOUT_VERSION: u16 = 1;
/// The first four bytes of a descriptor.
pub const DESCRIPTOR_MAGIC: [u8; 4] = *b"TTI2";
/// Length of a data record's header, before any extension entries and the user header.
pub const DATA_HEADER_LEN: usize = 40;
/// Length of a loss record (without extension entries).
pub const LOSS_RECORD_LEN: usize = 48;
/// Flag bits a reader must understand (bits 0 to 7). Unknown bits here make a record invalid.
pub const REQUIRED_FLAGS: u16 = 0x00FF;
/// Extension tag bit meaning a reader must understand the entry.
pub const REQUIRED_EXTENSION: u16 = 0x8000;
/// Length of an extension entry's header: tag, reserved, length.
pub const EXTENSION_HEADER_LEN: usize = 8;
const DESCRIPTOR_FIXED_LEN: usize = 96;
const FLAG_IMPORTED: u16 = 1;

/// The largest payload of any tickertape record: tickertape's largest frame minus its record header. A data record
/// (header, user header and payload) must fit in this, and in the journal's segment size too.
pub const MAX_RECORD_PAYLOAD: u64 =
    tickertape::format::MAX_FRAME_LEN as u64 - tickertape::format::RECORD_HEADER_LEN as u64;
/// The largest type alignment accepted (one 4 KiB page).
pub const MAX_ALIGNMENT: u64 = 4096;
/// The longest type name accepted, in bytes (iceoryx2's `MAX_TYPE_NAME_LENGTH`).
pub const MAX_TYPE_NAME_LEN: usize = 256;
/// The longest service name accepted, in bytes (iceoryx2's `MAX_SERVICE_NAME_LENGTH`).
pub const MAX_SERVICE_NAME_LEN: usize = 255;
/// The largest `history_size` accepted.
pub const MAX_HISTORY_SIZE: u64 = 65_536;
/// The largest `subscriber_max_buffer_size`, and recorder buffer size, accepted.
pub const MAX_BUFFER_SIZE: u64 = 65_536;
/// The largest `subscriber_max_borrowed_samples` accepted.
pub const MAX_BORROWED_SAMPLES: u64 = 65_536;
/// The largest `max_publishers`, `max_subscribers` and `max_nodes` accepted.
pub const MAX_PORTS: u64 = 4_096;
/// The most samples one publisher of the service must preallocate, `max_subscribers × (subscriber_max_buffer_size +
/// subscriber_max_borrowed_samples) + history_size`, accepted.
pub const MAX_SAMPLES_PER_PUBLISHER: u64 = 1 << 24;
/// The most queue slots between all publishers and subscribers, `max_publishers × max_subscribers ×
/// (subscriber_max_buffer_size + subscriber_max_borrowed_samples)`, accepted.
pub const MAX_CONNECTION_SLOTS: u64 = 1 << 32;

/// Why a payload doesn't decode, or a value can't be encoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LayoutError {
    /// The payload ends before `field`.
    #[error("record truncated before {0}")]
    Truncated(&'static str),
    /// The descriptor doesn't start with `TTI2`.
    #[error("bad descriptor magic")]
    BadMagic,
    /// The layout version is not [`LAYOUT_VERSION`].
    #[error("unsupported layout version {0} (this build reads version {LAYOUT_VERSION})")]
    UnsupportedVersion(u16),
    /// A required flag bit this version doesn't know is set.
    #[error("unknown required flags {flags:#06x} in a {record}")]
    UnknownRequiredFlags {
        /// The kind of record.
        record: &'static str,
        /// The unknown required bits.
        flags: u16,
    },
    /// An extension entry with a required tag this version doesn't know.
    #[error("unknown required extension tag {0:#06x}")]
    UnknownRequiredExtension(u16),
    /// A field has a value outside its range.
    #[error("invalid {0}")]
    Invalid(&'static str),
    /// A number is larger than this layout accepts.
    #[error("{field} is {value}, more than the limit of {limit}")]
    OutOfRange {
        /// The field or quantity.
        field: &'static str,
        /// Its value.
        value: u64,
        /// The largest value accepted.
        limit: u64,
    },
    /// A string or the user header is too long to encode.
    #[error("{0} is too long to encode")]
    TooLong(&'static str),
    /// A data or loss record doesn't fit the descriptor it names.
    #[error("record doesn't match its descriptor: {0}")]
    Mismatch(String),
}

/// iceoryx2's `TypeVariant`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeVariant {
    /// One value of a fixed size.
    FixedSize,
    /// A slice of elements; `size` is the size of one element.
    Dynamic,
}

/// iceoryx2's `TypeDetail`: what a service says about one part of its messages.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct TypeInfo {
    /// Fixed size or slice.
    pub variant: TypeVariant,
    /// The type name iceoryx2 compares when ports connect (for Rust types, usually `core::any::type_name`).
    pub name: String,
    /// Size in bytes (of one element, for a slice).
    pub size: u64,
    /// Alignment in bytes.
    pub alignment: u64,
}

/// The settings of a publish-subscribe service when it was recorded. 0 means unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct ServiceSettings {
    /// Samples a publisher keeps for late subscribers.
    pub history_size: u64,
    /// The largest buffer a subscriber may ask for.
    pub subscriber_max_buffer_size: u64,
    /// Samples a subscriber may hold at once.
    pub subscriber_max_borrowed_samples: u64,
    /// Maximum publishers.
    pub max_publishers: u64,
    /// Maximum subscribers.
    pub max_subscribers: u64,
    /// Maximum nodes.
    pub max_nodes: u64,
    /// Whether a full subscriber buffer drops its oldest sample (true) or the publisher applies its backpressure
    /// strategy (false).
    pub safe_overflow: bool,
}

impl ServiceSettings {
    /// Settings that are all unknown (0), with iceoryx2's default overflow (safe overflow).
    pub const UNKNOWN: ServiceSettings = ServiceSettings {
        history_size: 0,
        subscriber_max_buffer_size: 0,
        subscriber_max_borrowed_samples: 0,
        max_publishers: 0,
        max_subscribers: 0,
        max_nodes: 0,
        safe_overflow: true,
    };

    /// Checks every setting against its limit (see [Validity rules](self#validity-rules)).
    ///
    /// # Errors
    ///
    /// [`LayoutError::OutOfRange`] naming the first setting or combination over its limit.
    pub fn validate(&self) -> Result<(), LayoutError> {
        let cap = |field, value: u64, limit| {
            if value > limit {
                Err(LayoutError::OutOfRange {
                    field,
                    value,
                    limit,
                })
            } else {
                Ok(value)
            }
        };
        let history = cap("history_size", self.history_size, MAX_HISTORY_SIZE)?;
        let buffer = cap(
            "subscriber_max_buffer_size",
            self.subscriber_max_buffer_size,
            MAX_BUFFER_SIZE,
        )?;
        let borrowed = cap(
            "subscriber_max_borrowed_samples",
            self.subscriber_max_borrowed_samples,
            MAX_BORROWED_SAMPLES,
        )?;
        let publishers = cap("max_publishers", self.max_publishers, MAX_PORTS)?;
        let subscribers = cap("max_subscribers", self.max_subscribers, MAX_PORTS)?;
        cap("max_nodes", self.max_nodes, MAX_PORTS)?;
        // The individual caps keep these products far below u64::MAX; checked anyway.
        let per_subscriber = buffer + borrowed;
        let samples = subscribers
            .checked_mul(per_subscriber)
            .and_then(|v| v.checked_add(history))
            .unwrap_or(u64::MAX);
        cap(
            "max_subscribers × (buffer + borrowed) + history_size",
            samples,
            MAX_SAMPLES_PER_PUBLISHER,
        )?;
        let slots = publishers
            .checked_mul(subscribers)
            .and_then(|v| v.checked_mul(per_subscriber))
            .unwrap_or(u64::MAX);
        cap(
            "max_publishers × max_subscribers × (buffer + borrowed)",
            slots,
            MAX_CONNECTION_SLOTS,
        )?;
        Ok(())
    }
}

/// Where a publisher-side message counter is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CounterLocation {
    /// In the user header.
    UserHeader,
    /// In the payload (inside its first element, for a slice).
    Payload,
}

/// A publisher-side message counter: an unsigned little-endian integer that each publisher increments by one per
/// message (wrapping at its width). With one, the recorder can detect messages lost before they reached it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct CounterField {
    /// User header or payload.
    pub location: CounterLocation,
    /// Byte offset of the counter inside the user header or payload.
    pub offset: u32,
    /// Width in bytes: 4 or 8.
    pub width: u8,
}

impl CounterField {
    /// Reads the counter from a message, or `None` if the message is too short (a slice with no elements).
    pub fn read(&self, user_header: &[u8], payload: &[u8]) -> Option<u64> {
        let bytes = match self.location {
            CounterLocation::UserHeader => user_header,
            CounterLocation::Payload => payload,
        };
        let start = self.offset as usize;
        let field = bytes.get(start..start.checked_add(usize::from(self.width))?)?;
        match self.width {
            4 => Some(u64::from(u32::from_le_bytes(field.try_into().ok()?))),
            8 => Some(u64::from_le_bytes(field.try_into().ok()?)),
            _ => None,
        }
    }

    /// The largest counter value: `2^(8 × width) - 1`.
    pub fn max_value(&self) -> u64 {
        width_mask(self.width)
    }

    /// Checks that the counter has a valid width and lies inside the part of the message it is in.
    ///
    /// # Errors
    ///
    /// [`LayoutError::Invalid`] for a width other than 4 or 8, or a counter that doesn't fit.
    pub fn validate(&self, user_header: &TypeInfo, payload: &TypeInfo) -> Result<(), LayoutError> {
        if self.width != 4 && self.width != 8 {
            return Err(LayoutError::Invalid("counter width"));
        }
        let room = match self.location {
            CounterLocation::UserHeader => user_header.size,
            CounterLocation::Payload => payload.size,
        };
        if u64::from(self.offset) + u64::from(self.width) > room {
            return Err(LayoutError::Invalid(
                "counter field (it must lie inside the user header, or one payload element)",
            ));
        }
        Ok(())
    }
}

fn width_mask(width: u8) -> u64 {
    if width >= 8 {
        u64::MAX
    } else {
        (1u64 << (8 * u32::from(width))) - 1
    }
}

/// A semantic version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct Version {
    /// Major.
    pub major: u16,
    /// Minor.
    pub minor: u16,
    /// Patch.
    pub patch: u16,
}

impl core::fmt::Display for Version {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The iceoryx2 chunk the type details of a service imply, as computed by [`validate_types`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkLayout {
    /// Bytes before the payload: system header, user header, padding.
    pub headers_len: u64,
    /// The largest alignment of the three parts, which the chunk has.
    pub alignment: u64,
    /// The most payload elements a data record can hold (1 for a fixed-size payload).
    pub max_elements: u64,
    /// The chunk size for `max_elements`.
    pub max_chunk_size: u64,
}

/// Checks the three type details of a service and computes the iceoryx2 chunk they imply (see [Validity
/// rules](self#validity-rules)). This is the one place that decides whether type details from a journal, a file or
/// a running service may reach iceoryx2 or a size computation.
///
/// # Errors
///
/// A [`LayoutError`] naming the first rule that fails.
pub fn validate_types(
    payload: &TypeInfo,
    user_header: &TypeInfo,
    system_header: &TypeInfo,
) -> Result<ChunkLayout, LayoutError> {
    for (info, what) in [
        (payload, "payload"),
        (user_header, "user header"),
        (system_header, "system header"),
    ] {
        validate_type(info, what)?;
    }
    if user_header.variant != TypeVariant::FixedSize {
        return Err(LayoutError::Invalid(
            "user header type (it must be fixed-size, not a slice)",
        ));
    }
    if system_header.variant != TypeVariant::FixedSize {
        return Err(LayoutError::Invalid(
            "system header type (it must be fixed-size)",
        ));
    }
    if payload.variant == TypeVariant::Dynamic && payload.size == 0 {
        return Err(LayoutError::Invalid(
            "payload type: a slice of zero-sized elements can't be recorded, because every element count has the \
             same bytes",
        ));
    }
    // A data record with the user header and one element must fit in a tickertape record.
    let fixed = DATA_HEADER_LEN as u64 + user_header.size;
    let one = fixed.saturating_add(payload.size);
    if one > MAX_RECORD_PAYLOAD {
        return Err(LayoutError::OutOfRange {
            field: "data record of the user header and one payload element",
            value: one,
            limit: MAX_RECORD_PAYLOAD,
        });
    }
    let max_elements = match payload.variant {
        TypeVariant::FixedSize => 1,
        TypeVariant::Dynamic => (MAX_RECORD_PAYLOAD - fixed) / payload.size,
    };
    // iceoryx2's MessageTypeDetails::chunk_layout, with checked arithmetic.
    let overflow = LayoutError::Invalid("chunk layout (it overflows)");
    let align = |v: u64, a: u64| -> Result<u64, LayoutError> {
        Ok(v.checked_add(a - 1).ok_or(overflow.clone())? & !(a - 1))
    };
    let after_system = align(system_header.size, user_header.alignment)?;
    let headers_len = align(
        after_system
            .checked_add(user_header.size)
            .ok_or(overflow.clone())?,
        payload.alignment,
    )?;
    let stride = align(payload.size, payload.alignment)?;
    let alignment = system_header
        .alignment
        .max(user_header.alignment)
        .max(payload.alignment);
    let max_chunk_size = align(
        stride
            .checked_mul(max_elements)
            .and_then(|p| p.checked_add(headers_len))
            .ok_or(overflow.clone())?,
        alignment,
    )?;
    let size = usize::try_from(max_chunk_size).map_err(|_| overflow.clone())?;
    let align_usize = usize::try_from(alignment).map_err(|_| overflow.clone())?;
    core::alloc::Layout::from_size_align(size, align_usize).map_err(|_| overflow)?;
    Ok(ChunkLayout {
        headers_len,
        alignment,
        max_elements,
        max_chunk_size,
    })
}

fn validate_type(info: &TypeInfo, what: &'static str) -> Result<(), LayoutError> {
    let invalid = |rule: &'static str| LayoutError::Invalid(rule);
    if info.alignment == 0 || !info.alignment.is_power_of_two() {
        return Err(invalid("type alignment (it must be a power of two)"));
    }
    if info.alignment > MAX_ALIGNMENT {
        return Err(LayoutError::OutOfRange {
            field: match what {
                "payload" => "payload alignment",
                "user header" => "user header alignment",
                _ => "system header alignment",
            },
            value: info.alignment,
            limit: MAX_ALIGNMENT,
        });
    }
    if info.size > MAX_RECORD_PAYLOAD {
        return Err(LayoutError::OutOfRange {
            field: match what {
                "payload" => "payload size",
                "user header" => "user header size",
                _ => "system header size",
            },
            value: info.size,
            limit: MAX_RECORD_PAYLOAD,
        });
    }
    if !info.size.is_multiple_of(info.alignment) {
        return Err(invalid(
            "type size (it must be a multiple of the alignment)",
        ));
    }
    if info.name.len() > MAX_TYPE_NAME_LEN {
        return Err(LayoutError::TooLong("type name"));
    }
    Ok(())
}

/// One recorded service, written before its first record in a recording run and again in every journal segment that
/// holds its records (see [where descriptors lie](self#where-descriptors-lie)). See the [module docs](self) for
/// bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Descriptor {
    /// The journal's next sequence number when the recording run began. Shared by all descriptors of a run.
    pub run_start_seq: u64,
    /// When the run began, nanoseconds since the Unix epoch.
    pub run_start_unix_ns: u64,
    /// True if imported from a file that only had times relative to the start of the recording: the records'
    /// `local_ts_ns` are then the import's chosen start time plus the relative time, not when the messages arrived.
    pub imported: bool,
    /// iceoryx2 version on the recording side.
    pub iceoryx2_version: Version,
    /// Service settings.
    pub settings: ServiceSettings,
    /// The recorder subscriber's buffer size, 0 if unknown.
    pub recorder_buffer_size: u64,
    /// Publisher-side counter, if the recorder was told about one.
    pub counter: Option<CounterField>,
    /// Payload type.
    pub payload: TypeInfo,
    /// User header type (iceoryx2 uses `()`, size 0, when there is none).
    pub user_header: TypeInfo,
    /// iceoryx2's own sample header type (not recorded; describes the recording side's iceoryx2).
    pub system_header: TypeInfo,
    /// Service name.
    pub service_name: String,
    /// Who wrote the recording, for example `tickertape-iceoryx2 0.1.0`.
    pub recorder: String,
}

impl Descriptor {
    /// Checks every value against the [validity rules](self#validity-rules) and returns the chunk layout the types
    /// imply.
    ///
    /// # Errors
    ///
    /// A [`LayoutError`] naming the first rule that fails.
    pub fn validate(&self) -> Result<ChunkLayout, LayoutError> {
        let chunk = validate_types(&self.payload, &self.user_header, &self.system_header)?;
        self.settings.validate()?;
        if self.recorder_buffer_size > MAX_BUFFER_SIZE {
            return Err(LayoutError::OutOfRange {
                field: "recorder buffer size",
                value: self.recorder_buffer_size,
                limit: MAX_BUFFER_SIZE,
            });
        }
        if let Some(c) = &self.counter {
            c.validate(&self.user_header, &self.payload)?;
        }
        if self.service_name.is_empty() {
            return Err(LayoutError::Invalid("service name (it is empty)"));
        }
        if self.service_name.len() > MAX_SERVICE_NAME_LEN {
            return Err(LayoutError::TooLong("service name"));
        }
        if self.recorder.len() > usize::from(u16::MAX) {
            return Err(LayoutError::TooLong("recorder"));
        }
        Ok(chunk)
    }

    /// Encodes the descriptor.
    ///
    /// # Errors
    ///
    /// Any [`LayoutError`] [`Descriptor::validate`] returns: `encode` refuses everything `decode` refuses.
    pub fn encode(&self) -> Result<Vec<u8>, LayoutError> {
        self.validate()?;
        let mut out = Vec::with_capacity(256);
        out.extend_from_slice(&DESCRIPTOR_MAGIC);
        out.extend_from_slice(&LAYOUT_VERSION.to_le_bytes());
        let flags = if self.imported { FLAG_IMPORTED } else { 0 };
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&self.run_start_seq.to_le_bytes());
        out.extend_from_slice(&self.run_start_unix_ns.to_le_bytes());
        for part in [
            self.iceoryx2_version.major,
            self.iceoryx2_version.minor,
            self.iceoryx2_version.patch,
        ] {
            out.extend_from_slice(&part.to_le_bytes());
        }
        out.push(1); // publish-subscribe
        out.push(u8::from(self.settings.safe_overflow));
        let s = &self.settings;
        for value in [
            s.history_size,
            s.subscriber_max_buffer_size,
            s.subscriber_max_borrowed_samples,
            s.max_publishers,
            s.max_subscribers,
            s.max_nodes,
            self.recorder_buffer_size,
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        let (location, width, offset) = match self.counter {
            None => (0u8, 0u8, 0u32),
            Some(c) => {
                let location = match c.location {
                    CounterLocation::UserHeader => 1,
                    CounterLocation::Payload => 2,
                };
                (location, c.width, c.offset)
            }
        };
        out.push(location);
        out.push(width);
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&offset.to_le_bytes());
        debug_assert_eq!(out.len(), DESCRIPTOR_FIXED_LEN);
        for info in [&self.payload, &self.user_header, &self.system_header] {
            out.push(match info.variant {
                TypeVariant::FixedSize => 0,
                TypeVariant::Dynamic => 1,
            });
            out.push(0);
            let len =
                u16::try_from(info.name.len()).map_err(|_| LayoutError::TooLong("type name"))?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&info.size.to_le_bytes());
            out.extend_from_slice(&info.alignment.to_le_bytes());
            out.extend_from_slice(info.name.as_bytes());
        }
        put_str(&mut out, &self.service_name, "service name")?;
        put_str(&mut out, &self.recorder, "recorder")?;
        Ok(out)
    }

    /// Decodes and validates a descriptor.
    ///
    /// # Errors
    ///
    /// A [`LayoutError`] if the bytes are not a valid version-1 descriptor, or a value breaks a [validity
    /// rule](self#validity-rules).
    pub fn decode(bytes: &[u8]) -> Result<Descriptor, LayoutError> {
        let mut c = Cursor { bytes, pos: 0 };
        if c.take(4, "magic")? != DESCRIPTOR_MAGIC {
            return Err(LayoutError::BadMagic);
        }
        let version = c.u16("layout version")?;
        if version != LAYOUT_VERSION {
            return Err(LayoutError::UnsupportedVersion(version));
        }
        let flags = c.u16("flags")?;
        check_flags(flags, FLAG_IMPORTED, "descriptor")?;
        let run_start_seq = c.u64("run start sequence number")?;
        let run_start_unix_ns = c.u64("run start")?;
        let iceoryx2_version = Version {
            major: c.u16("iceoryx2 version")?,
            minor: c.u16("iceoryx2 version")?,
            patch: c.u16("iceoryx2 version")?,
        };
        if c.u8("messaging pattern")? != 1 {
            return Err(LayoutError::Invalid("messaging pattern"));
        }
        let safe_overflow = match c.u8("safe overflow")? {
            0 => false,
            1 => true,
            _ => return Err(LayoutError::Invalid("safe overflow")),
        };
        let settings = ServiceSettings {
            history_size: c.u64("history size")?,
            subscriber_max_buffer_size: c.u64("subscriber max buffer size")?,
            subscriber_max_borrowed_samples: c.u64("subscriber max borrowed samples")?,
            max_publishers: c.u64("max publishers")?,
            max_subscribers: c.u64("max subscribers")?,
            max_nodes: c.u64("max nodes")?,
            safe_overflow,
        };
        let recorder_buffer_size = c.u64("recorder buffer size")?;
        let location = c.u8("counter location")?;
        let width = c.u8("counter width")?;
        c.take(2, "reserved")?;
        let offset = c.u32("counter offset")?;
        let counter = match (location, width) {
            (0, 0) if offset == 0 => None,
            (1 | 2, 4 | 8) => Some(CounterField {
                location: if location == 1 {
                    CounterLocation::UserHeader
                } else {
                    CounterLocation::Payload
                },
                offset,
                width,
            }),
            _ => return Err(LayoutError::Invalid("counter field")),
        };
        let payload = c.type_info()?;
        let user_header = c.type_info()?;
        let system_header = c.type_info()?;
        let service_name = c.string("service name")?;
        let recorder = c.string("recorder")?;
        check_extensions(&bytes[c.pos..])?;
        let descriptor = Descriptor {
            run_start_seq,
            run_start_unix_ns,
            imported: flags & FLAG_IMPORTED != 0,
            iceoryx2_version,
            settings,
            recorder_buffer_size,
            counter,
            payload,
            user_header,
            system_header,
            service_name,
            recorder,
        };
        descriptor.validate()?;
        Ok(descriptor)
    }
}

/// One recorded message, borrowed from a record's payload. See the [module docs](self) for bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataRecord<'a> {
    /// Sequence number of the service's [`Descriptor`] record.
    pub descriptor_seq: u64,
    /// The iceoryx2 publisher id (the sample's origin), 0 if unknown.
    pub publisher_id: u128,
    /// User header bytes.
    pub user_header: &'a [u8],
    /// Payload bytes.
    pub payload: &'a [u8],
}

impl<'a> DataRecord<'a> {
    /// The encoded length of this record.
    ///
    /// # Errors
    ///
    /// [`LayoutError::TooLong`] if the user header is longer than `u32::MAX` bytes, or the record would be longer
    /// than [`MAX_RECORD_PAYLOAD`].
    pub fn encoded_len(&self) -> Result<usize, LayoutError> {
        u32::try_from(self.user_header.len()).map_err(|_| LayoutError::TooLong("user header"))?;
        let len = DATA_HEADER_LEN
            .checked_add(self.user_header.len())
            .and_then(|n| n.checked_add(self.payload.len()))
            .ok_or(LayoutError::TooLong("message"))?;
        if len as u64 > MAX_RECORD_PAYLOAD {
            return Err(LayoutError::TooLong("message"));
        }
        Ok(len)
    }

    /// Writes the record into `out`, which must be exactly [`DataRecord::encoded_len`] bytes long.
    ///
    /// # Errors
    ///
    /// As for [`DataRecord::encoded_len`], and [`LayoutError::Invalid`] if `out` has the wrong length. Nothing is
    /// written then.
    pub fn encode_into(&self, out: &mut [u8]) -> Result<(), LayoutError> {
        if out.len() != self.encoded_len()? {
            return Err(LayoutError::Invalid("output buffer length"));
        }
        let h = self.user_header.len() as u32; // checked by encoded_len
        out[0..2].copy_from_slice(&LAYOUT_VERSION.to_le_bytes());
        out[2..4].copy_from_slice(&0u16.to_le_bytes());
        out[4..6].copy_from_slice(&(DATA_HEADER_LEN as u16).to_le_bytes());
        out[6..8].fill(0);
        out[8..12].copy_from_slice(&h.to_le_bytes());
        out[12..16].fill(0);
        out[16..24].copy_from_slice(&self.descriptor_seq.to_le_bytes());
        out[24..40].copy_from_slice(&self.publisher_id.to_le_bytes());
        let (header, payload) = out[DATA_HEADER_LEN..].split_at_mut(self.user_header.len());
        header.copy_from_slice(self.user_header);
        payload.copy_from_slice(self.payload);
        Ok(())
    }

    /// Encodes the record into a new vector.
    ///
    /// # Errors
    ///
    /// As for [`DataRecord::encoded_len`].
    pub fn encode(&self) -> Result<Vec<u8>, LayoutError> {
        let mut out = vec![0; self.encoded_len()?];
        self.encode_into(&mut out)?;
        Ok(out)
    }

    /// Decodes a data record, borrowing from `bytes`. Check it against its descriptor with [`DataRecord::check`]
    /// before using its bytes.
    ///
    /// # Errors
    ///
    /// A [`LayoutError`] if the bytes are not a valid version-1 data record.
    pub fn decode(bytes: &'a [u8]) -> Result<DataRecord<'a>, LayoutError> {
        let mut c = Cursor { bytes, pos: 0 };
        let version = c.u16("layout version")?;
        if version != LAYOUT_VERSION {
            return Err(LayoutError::UnsupportedVersion(version));
        }
        check_flags(c.u16("flags")?, 0, "data record")?;
        let header_len = usize::from(c.u16("header length")?);
        c.take(2, "reserved")?;
        let uh = c.u32("user header length")? as usize;
        c.take(4, "reserved")?;
        let descriptor_seq = c.u64("descriptor sequence number")?;
        let publisher_id = c.u128("publisher id")?;
        if header_len < DATA_HEADER_LEN {
            return Err(LayoutError::Invalid("data record header length"));
        }
        let extensions = c.take(header_len - DATA_HEADER_LEN, "extension entries")?;
        check_extensions(extensions)?;
        let user_header = c.take(uh, "user header")?;
        Ok(DataRecord {
            descriptor_seq,
            publisher_id,
            user_header,
            payload: &bytes[c.pos..],
        })
    }

    /// Checks the record against the descriptor it names: the user header has the descriptor's user header size,
    /// and the payload has the payload size (or, for a slice, a whole number of elements). Returns the number of
    /// payload elements.
    ///
    /// # Errors
    ///
    /// [`LayoutError::Mismatch`] if it doesn't fit.
    pub fn check(&self, descriptor: &Descriptor) -> Result<u64, LayoutError> {
        check_message(self.user_header, self.payload, descriptor)
    }
}

/// Checks message bytes against a descriptor, as [`DataRecord::check`] does; returns the number of payload elements.
///
/// # Errors
///
/// [`LayoutError::Mismatch`] if they don't fit.
pub fn check_message(
    user_header: &[u8],
    payload: &[u8],
    descriptor: &Descriptor,
) -> Result<u64, LayoutError> {
    if user_header.len() as u64 != descriptor.user_header.size {
        return Err(LayoutError::Mismatch(format!(
            "the user header has {} bytes, the descriptor's user header type has {}",
            user_header.len(),
            descriptor.user_header.size
        )));
    }
    let len = payload.len() as u64;
    let size = descriptor.payload.size;
    match descriptor.payload.variant {
        TypeVariant::FixedSize if len == size => Ok(1),
        // validate_types guarantees size > 0 for a slice.
        TypeVariant::Dynamic if size > 0 && len.is_multiple_of(size) => Ok(len / size),
        variant => Err(LayoutError::Mismatch(format!(
            "the payload has {len} bytes, which doesn't fit the descriptor's {size}-byte {variant:?} payload type"
        ))),
    }
}

/// What a [`LossRecord`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LossKind {
    /// The counter jumped forward: `(received - expected) mod 2^(8 × width)` messages never reached the recorder.
    Gap,
    /// The counter went backwards or repeated: the publisher restarted, or a message was delivered twice (for
    /// example from the publisher's history after the recorder reconnected).
    Backwards,
}

impl LossKind {
    /// Classifies a counter value by its distance from the expected one, modulo `2^(8 × width)`: `None` if they are
    /// equal (nothing lost), a gap if the distance is less than half the range, backwards otherwise.
    pub fn classify(width: u8, expected: u64, received: u64) -> Option<LossKind> {
        let mask = width_mask(width);
        let d = received.wrapping_sub(expected) & mask;
        if d == 0 {
            None
        } else if d <= mask / 2 {
            Some(LossKind::Gap)
        } else {
            Some(LossKind::Backwards)
        }
    }
}

/// A gap in a publisher-side counter. See the [module docs](self) for bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct LossRecord {
    /// Gap or backwards.
    pub kind: LossKind,
    /// The counter's width in bytes, 4 or 8.
    pub width: u8,
    /// Sequence number of the service's [`Descriptor`] record.
    pub descriptor_seq: u64,
    /// The iceoryx2 publisher id.
    pub publisher_id: u128,
    /// The counter value the recorder expected.
    pub expected: u64,
    /// The counter value it received.
    pub received: u64,
}

impl LossRecord {
    /// Number of messages missing, for a gap (the distance modulo `2^(8 × width)`); 0 otherwise.
    pub fn missing(&self) -> u64 {
        match self.kind {
            LossKind::Gap => self.received.wrapping_sub(self.expected) & width_mask(self.width),
            LossKind::Backwards => 0,
        }
    }

    fn validate(&self) -> Result<(), LayoutError> {
        if self.width != 4 && self.width != 8 {
            return Err(LayoutError::Invalid("loss record counter width"));
        }
        let mask = width_mask(self.width);
        if self.expected > mask || self.received > mask {
            return Err(LayoutError::Invalid(
                "loss record counter value (wider than the counter)",
            ));
        }
        if LossKind::classify(self.width, self.expected, self.received) != Some(self.kind) {
            return Err(LayoutError::Invalid(
                "loss record kind (it doesn't match the distance between expected and received)",
            ));
        }
        Ok(())
    }

    /// Encodes the record.
    ///
    /// # Errors
    ///
    /// [`LayoutError::Invalid`] if the decoder would refuse the record: a width other than 4 or 8, counter values
    /// wider than it, or a kind that doesn't match them.
    pub fn encode(&self) -> Result<[u8; LOSS_RECORD_LEN], LayoutError> {
        self.validate()?;
        let mut out = [0u8; LOSS_RECORD_LEN];
        out[0..2].copy_from_slice(&LAYOUT_VERSION.to_le_bytes());
        out[4] = match self.kind {
            LossKind::Gap => 1,
            LossKind::Backwards => 2,
        };
        out[5] = self.width;
        out[8..16].copy_from_slice(&self.descriptor_seq.to_le_bytes());
        out[16..32].copy_from_slice(&self.publisher_id.to_le_bytes());
        out[32..40].copy_from_slice(&self.expected.to_le_bytes());
        out[40..48].copy_from_slice(&self.received.to_le_bytes());
        Ok(out)
    }

    /// Decodes a loss record.
    ///
    /// # Errors
    ///
    /// A [`LayoutError`] if the bytes are not a valid version-1 loss record.
    pub fn decode(bytes: &[u8]) -> Result<LossRecord, LayoutError> {
        let mut c = Cursor { bytes, pos: 0 };
        let version = c.u16("layout version")?;
        if version != LAYOUT_VERSION {
            return Err(LayoutError::UnsupportedVersion(version));
        }
        check_flags(c.u16("flags")?, 0, "loss record")?;
        let kind = match c.u8("loss kind")? {
            1 => LossKind::Gap,
            2 => LossKind::Backwards,
            _ => return Err(LayoutError::Invalid("loss kind")),
        };
        let width = c.u8("counter width")?;
        c.take(2, "reserved")?;
        let record = LossRecord {
            kind,
            width,
            descriptor_seq: c.u64("descriptor sequence number")?,
            publisher_id: c.u128("publisher id")?,
            expected: c.u64("expected counter")?,
            received: c.u64("received counter")?,
        };
        check_extensions(&bytes[c.pos..])?;
        record.validate()?;
        Ok(record)
    }

    /// Checks the record against the descriptor it names: the descriptor has a counter of the same width.
    ///
    /// # Errors
    ///
    /// [`LayoutError::Mismatch`] if it doesn't.
    pub fn check(&self, descriptor: &Descriptor) -> Result<(), LayoutError> {
        match descriptor.counter {
            Some(c) if c.width == self.width => Ok(()),
            Some(c) => Err(LayoutError::Mismatch(format!(
                "the loss record has a {}-byte counter, the descriptor a {}-byte one",
                self.width, c.width
            ))),
            None => Err(LayoutError::Mismatch(
                "a loss record names a descriptor without a counter".into(),
            )),
        }
    }
}

/// A decoded record of any kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded<'a> {
    /// A service descriptor.
    Descriptor(Box<Descriptor>),
    /// A message.
    Data(DataRecord<'a>),
    /// A counter gap.
    Loss(LossRecord),
    /// A filler record, with its payload length; skipped.
    Filler(usize),
    /// A schema id this crate doesn't write; left alone.
    Other,
}

/// Decodes a record by its schema id.
///
/// # Errors
///
/// A [`LayoutError`] if the schema id is one of this crate's and the payload is malformed. Reserved schema ids this
/// version doesn't know decode as [`Decoded::Other`].
pub fn decode(schema_id: u32, payload: &[u8]) -> Result<Decoded<'_>, LayoutError> {
    Ok(match schema_id {
        SCHEMA_DESCRIPTOR => Decoded::Descriptor(Box::new(Descriptor::decode(payload)?)),
        SCHEMA_DATA => Decoded::Data(DataRecord::decode(payload)?),
        SCHEMA_LOSS => Decoded::Loss(LossRecord::decode(payload)?),
        SCHEMA_FILLER => Decoded::Filler(payload.len()),
        _ => Decoded::Other,
    })
}

fn check_flags(flags: u16, known: u16, record: &'static str) -> Result<(), LayoutError> {
    let unknown = flags & REQUIRED_FLAGS & !known;
    if unknown != 0 {
        return Err(LayoutError::UnknownRequiredFlags {
            record,
            flags: unknown,
        });
    }
    Ok(())
}

/// Walks an extension area: every entry must lie inside it, and no tag may be an unknown required one (version 1
/// knows no tags).
fn check_extensions(mut area: &[u8]) -> Result<(), LayoutError> {
    while !area.is_empty() {
        let mut c = Cursor {
            bytes: area,
            pos: 0,
        };
        let tag = c.u16("extension tag")?;
        c.take(2, "extension reserved field")?;
        let len = c.u32("extension length")? as usize;
        c.take(len, "extension data")?;
        if tag & REQUIRED_EXTENSION != 0 {
            return Err(LayoutError::UnknownRequiredExtension(tag));
        }
        area = &area[c.pos..];
    }
    Ok(())
}

fn put_str(out: &mut Vec<u8>, s: &str, what: &'static str) -> Result<(), LayoutError> {
    let len = u16::try_from(s.len()).map_err(|_| LayoutError::TooLong(what))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], LayoutError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(LayoutError::Truncated(what))?;
        let out = self
            .bytes
            .get(self.pos..end)
            .ok_or(LayoutError::Truncated(what))?;
        self.pos = end;
        Ok(out)
    }

    fn array<const N: usize>(&mut self, what: &'static str) -> Result<[u8; N], LayoutError> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N, what)?);
        Ok(out)
    }

    fn u8(&mut self, what: &'static str) -> Result<u8, LayoutError> {
        Ok(self.array::<1>(what)?[0])
    }

    fn u16(&mut self, what: &'static str) -> Result<u16, LayoutError> {
        Ok(u16::from_le_bytes(self.array(what)?))
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, LayoutError> {
        Ok(u32::from_le_bytes(self.array(what)?))
    }

    fn u64(&mut self, what: &'static str) -> Result<u64, LayoutError> {
        Ok(u64::from_le_bytes(self.array(what)?))
    }

    fn u128(&mut self, what: &'static str) -> Result<u128, LayoutError> {
        Ok(u128::from_le_bytes(self.array(what)?))
    }

    fn string(&mut self, what: &'static str) -> Result<String, LayoutError> {
        let len = self.u16(what)?;
        self.utf8(usize::from(len), what)
    }

    fn utf8(&mut self, len: usize, what: &'static str) -> Result<String, LayoutError> {
        let bytes = self.take(len, what)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| LayoutError::Invalid(what))
    }

    fn type_info(&mut self) -> Result<TypeInfo, LayoutError> {
        let variant = match self.u8("type variant")? {
            0 => TypeVariant::FixedSize,
            1 => TypeVariant::Dynamic,
            _ => return Err(LayoutError::Invalid("type variant")),
        };
        self.take(1, "reserved")?;
        let name_len = self.u16("type name length")?;
        self.take(4, "reserved")?;
        let size = self.u64("type size")?;
        let alignment = self.u64("type alignment")?;
        let name = self.utf8(usize::from(name_len), "type name")?;
        Ok(TypeInfo {
            variant,
            name,
            size,
            alignment,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn fixed(name: &str, size: u64, alignment: u64) -> TypeInfo {
        TypeInfo {
            variant: TypeVariant::FixedSize,
            name: name.into(),
            size,
            alignment,
        }
    }

    fn type_info(allow_dynamic: bool) -> impl Strategy<Value = TypeInfo> {
        (
            any::<bool>(),
            "[a-zA-Z0-9_:<>()]{0,40}",
            0u64..4096,
            0u32..7,
        )
            .prop_map(move |(dynamic, name, units, a)| {
                let alignment = 1u64 << a;
                let dynamic = dynamic && allow_dynamic;
                TypeInfo {
                    variant: if dynamic {
                        TypeVariant::Dynamic
                    } else {
                        TypeVariant::FixedSize
                    },
                    name,
                    size: (units + u64::from(dynamic)) * alignment,
                    alignment,
                }
            })
    }

    fn descriptor() -> impl Strategy<Value = Descriptor> {
        (
            (
                any::<u64>(),
                any::<u64>(),
                any::<bool>(),
                any::<(u16, u16, u16)>(),
            ),
            (
                (
                    0..=MAX_HISTORY_SIZE,
                    0..=64u64,
                    0..=64u64,
                    0..=MAX_PORTS,
                    0..=64u64,
                    0..=MAX_PORTS,
                ),
                any::<bool>(),
                0..=MAX_BUFFER_SIZE,
            ),
            proptest::option::of((any::<bool>(), 0u32..8, prop_oneof![Just(4u8), Just(8u8)])),
            (type_info(true), type_info(false), type_info(false)),
            ("[a-z/_0-9]{1,60}", ".{0,30}"),
        )
            .prop_map(|(a, b, counter, types, names)| {
                let (payload, mut user_header, system_header) = types;
                let counter = counter.map(|(header, offset, width)| CounterField {
                    location: if header {
                        CounterLocation::UserHeader
                    } else {
                        CounterLocation::Payload
                    },
                    offset,
                    width,
                });
                // Make room for the counter where it is.
                if let Some(c) = counter {
                    let need = u64::from(c.offset) + u64::from(c.width);
                    user_header.size = user_header
                        .size
                        .max(need)
                        .next_multiple_of(user_header.alignment);
                }
                let counter = counter.map(|mut c| {
                    if payload.size < u64::from(c.offset) + u64::from(c.width) {
                        c.location = CounterLocation::UserHeader;
                    }
                    c
                });
                Descriptor {
                    run_start_seq: a.0,
                    run_start_unix_ns: a.1,
                    imported: a.2,
                    iceoryx2_version: Version {
                        major: a.3.0,
                        minor: a.3.1,
                        patch: a.3.2,
                    },
                    settings: ServiceSettings {
                        history_size: b.0.0,
                        subscriber_max_buffer_size: b.0.1,
                        subscriber_max_borrowed_samples: b.0.2,
                        max_publishers: b.0.3,
                        max_subscribers: b.0.4,
                        max_nodes: b.0.5,
                        safe_overflow: b.1,
                    },
                    recorder_buffer_size: b.2,
                    counter,
                    payload,
                    user_header,
                    system_header,
                    service_name: names.0,
                    recorder: names.1,
                }
            })
    }

    proptest! {
        #[test]
        fn descriptor_round_trips(d in descriptor()) {
            let bytes = d.encode().unwrap();
            prop_assert_eq!(Descriptor::decode(&bytes).unwrap(), d);
        }

        #[test]
        fn truncated_descriptor_is_rejected(d in descriptor(), cut in any::<prop::sample::Index>()) {
            let bytes = d.encode().unwrap();
            let cut = cut.index(bytes.len());
            prop_assert!(Descriptor::decode(&bytes[..cut]).is_err());
        }

        #[test]
        fn descriptor_extension_entries_follow_the_growth_rule(d in descriptor(), tag in 0u16..0x8000, data in prop::collection::vec(any::<u8>(), 0..16)) {
            let mut bytes = d.encode().unwrap();
            let entry = |tag: u16| {
                let mut e = Vec::new();
                e.extend_from_slice(&tag.to_le_bytes());
                e.extend_from_slice(&[0, 0]);
                e.extend_from_slice(&(data.len() as u32).to_le_bytes());
                e.extend_from_slice(&data);
                e
            };
            // An optional entry is skipped.
            let optional = entry(tag);
            bytes.extend_from_slice(&optional);
            prop_assert_eq!(Descriptor::decode(&bytes).unwrap(), d.clone());
            // A required one is refused, as is an entry that runs past the end.
            let mut required = bytes.clone();
            required.extend_from_slice(&entry(tag | REQUIRED_EXTENSION));
            prop_assert_eq!(Descriptor::decode(&required), Err(LayoutError::UnknownRequiredExtension(tag | REQUIRED_EXTENSION)));
            prop_assert!(Descriptor::decode(&bytes[..bytes.len() - 1]).is_err());
        }

        #[test]
        fn data_round_trips(seq in any::<u64>(), id in any::<u128>(), uh in prop::collection::vec(any::<u8>(), 0..64), pl in prop::collection::vec(any::<u8>(), 0..512)) {
            let record = DataRecord { descriptor_seq: seq, publisher_id: id, user_header: &uh, payload: &pl };
            let bytes = record.encode().unwrap();
            prop_assert_eq!(bytes.len(), DATA_HEADER_LEN + uh.len() + pl.len());
            prop_assert_eq!(DataRecord::decode(&bytes).unwrap(), record);
        }

        #[test]
        fn data_with_header_longer_than_record_is_rejected(seq in any::<u64>(), uh in prop::collection::vec(any::<u8>(), 1..64), extra in 1u32..1000) {
            let record = DataRecord { descriptor_seq: seq, publisher_id: 0, user_header: &uh, payload: &[] };
            let mut bytes = record.encode().unwrap();
            let claimed = uh.len() as u32 + extra;
            bytes[8..12].copy_from_slice(&claimed.to_le_bytes());
            prop_assert_eq!(DataRecord::decode(&bytes), Err(LayoutError::Truncated("user header")));
        }

        #[test]
        fn loss_round_trips(width in prop_oneof![Just(4u8), Just(8u8)], seq in any::<u64>(), id in any::<u128>(), e in any::<u64>(), r in any::<u64>()) {
            let mask = width_mask(width);
            let (e, r) = (e & mask, r & mask);
            let Some(kind) = LossKind::classify(width, e, r) else { return Ok(()) };
            let record = LossRecord { kind, width, descriptor_seq: seq, publisher_id: id, expected: e, received: r };
            prop_assert_eq!(LossRecord::decode(&record.encode().unwrap()).unwrap(), record);
        }

        #[test]
        fn arbitrary_bytes_never_panic(schema in prop_oneof![Just(SCHEMA_DESCRIPTOR), Just(SCHEMA_DATA), Just(SCHEMA_LOSS), any::<u32>()], bytes in prop::collection::vec(any::<u8>(), 0..400)) {
            let _ = decode(schema, &bytes);
        }

        #[test]
        fn descriptor_bit_flips_never_panic_and_decode_only_valid_values(d in descriptor(), at in any::<prop::sample::Index>(), bit in 0u8..8) {
            let mut bytes = d.encode().unwrap();
            let i = at.index(bytes.len());
            bytes[i] ^= 1 << bit;
            if let Ok(decoded) = Descriptor::decode(&bytes) {
                // Whatever decodes is valid and encodes again.
                prop_assert!(decoded.validate().is_ok());
                prop_assert!(decoded.encode().is_ok());
            }
        }

        #[test]
        fn arbitrary_type_details_never_overflow(size in any::<u64>(), align in any::<u64>(), uh_size in any::<u64>(), uh_align in any::<u64>(), dynamic in any::<bool>()) {
            let payload = TypeInfo { variant: if dynamic { TypeVariant::Dynamic } else { TypeVariant::FixedSize }, name: "p".into(), size, alignment: align };
            let user_header = fixed("h", uh_size, uh_align);
            if let Ok(chunk) = validate_types(&payload, &user_header, &fixed("s", 48, 8)) {
                prop_assert!(chunk.max_chunk_size >= chunk.headers_len);
                prop_assert!(core::alloc::Layout::from_size_align(chunk.max_chunk_size as usize, chunk.alignment as usize).is_ok());
            }
        }
    }

    fn sample_descriptor() -> Descriptor {
        Descriptor {
            run_start_seq: 7,
            run_start_unix_ns: 1_700_000_000_000_000_000,
            imported: false,
            iceoryx2_version: Version {
                major: 0,
                minor: 10,
                patch: 0,
            },
            settings: ServiceSettings {
                history_size: 0,
                subscriber_max_buffer_size: 2,
                subscriber_max_borrowed_samples: 2,
                max_publishers: 2,
                max_subscribers: 8,
                max_nodes: 20,
                safe_overflow: true,
            },
            recorder_buffer_size: 2,
            counter: None,
            payload: fixed("u64", 8, 8),
            user_header: fixed("()", 0, 1),
            system_header: fixed("Header", 48, 8),
            service_name: "my/service".into(),
            recorder: "test".into(),
        }
    }

    #[test]
    fn descriptor_fixed_fields_are_where_the_docs_say() {
        let mut d = sample_descriptor();
        d.user_header = fixed("H", 16, 8);
        d.counter = Some(CounterField {
            location: CounterLocation::UserHeader,
            offset: 4,
            width: 8,
        });
        d.imported = true;
        let b = d.encode().unwrap();
        let u64_at = |at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
        assert_eq!(&b[0..4], b"TTI2");
        assert_eq!(u16::from_le_bytes([b[4], b[5]]), 1);
        assert_eq!(u16::from_le_bytes([b[6], b[7]]), 1, "imported flag");
        assert_eq!(u64_at(8), 7, "run_start_seq");
        assert_eq!(u64_at(16), 1_700_000_000_000_000_000);
        assert_eq!(u16::from_le_bytes([b[26], b[27]]), 10);
        assert_eq!(b[30], 1);
        assert_eq!(b[31], 1);
        assert_eq!(u64_at(40), 2);
        assert_eq!(u64_at(64), 8);
        assert_eq!(u64_at(72), 20);
        assert_eq!(u64_at(80), 2);
        assert_eq!((b[88], b[89]), (1, 8));
        assert_eq!(u32::from_le_bytes(b[92..96].try_into().unwrap()), 4);
        // Payload type detail starts at 96: variant, reserved, name length, reserved, size, alignment, name.
        assert_eq!(b[96], 0);
        assert_eq!(u16::from_le_bytes([b[98], b[99]]), 3);
        assert_eq!(u64_at(104), 8);
        assert_eq!(u64_at(112), 8);
        assert_eq!(&b[120..123], b"u64");
        // User header type detail follows the name.
        assert_eq!(u16::from_le_bytes([b[125], b[126]]), 1);
        assert_eq!(u64_at(131), 16);
    }

    #[test]
    fn data_fields_are_where_the_docs_say() {
        let b = DataRecord {
            descriptor_seq: 9,
            publisher_id: 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10,
            user_header: &[0xAA, 0xBB],
            payload: &[0xCC],
        }
        .encode()
        .unwrap();
        assert_eq!(b.len(), 43);
        assert_eq!(u16::from_le_bytes([b[0], b[1]]), 1);
        assert_eq!(u16::from_le_bytes([b[2], b[3]]), 0);
        assert_eq!(u16::from_le_bytes([b[4], b[5]]), 40);
        assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), 2);
        assert_eq!(u64::from_le_bytes(b[16..24].try_into().unwrap()), 9);
        assert_eq!(
            u128::from_le_bytes(b[24..40].try_into().unwrap()),
            0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10
        );
        assert_eq!(&b[40..], &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn loss_fields_are_where_the_docs_say() {
        let b = LossRecord {
            kind: LossKind::Backwards,
            width: 4,
            descriptor_seq: 3,
            publisher_id: 5,
            expected: 10,
            received: 2,
        }
        .encode()
        .unwrap();
        assert_eq!(u16::from_le_bytes([b[0], b[1]]), 1);
        assert_eq!((b[4], b[5]), (2, 4));
        assert_eq!(u64::from_le_bytes(b[8..16].try_into().unwrap()), 3);
        assert_eq!(u128::from_le_bytes(b[16..32].try_into().unwrap()), 5);
        assert_eq!(u64::from_le_bytes(b[32..40].try_into().unwrap()), 10);
        assert_eq!(u64::from_le_bytes(b[40..48].try_into().unwrap()), 2);
    }

    #[test]
    fn malformed_descriptors_are_rejected() {
        let good = sample_descriptor().encode().unwrap();
        let cases: [(usize, u8, LayoutError); 6] = [
            (0, b'X', LayoutError::BadMagic),
            (4, 2, LayoutError::UnsupportedVersion(2)),
            (
                6,
                2,
                LayoutError::UnknownRequiredFlags {
                    record: "descriptor",
                    flags: 2,
                },
            ),
            (30, 2, LayoutError::Invalid("messaging pattern")),
            (31, 2, LayoutError::Invalid("safe overflow")),
            (96, 2, LayoutError::Invalid("type variant")),
        ];
        for (at, value, expected) in cases {
            let mut bad = good.clone();
            bad[at] = value;
            assert_eq!(Descriptor::decode(&bad), Err(expected), "byte {at}");
        }
        let mut bad = good.clone();
        bad[88] = 1; // counter location without a width
        assert_eq!(
            Descriptor::decode(&bad),
            Err(LayoutError::Invalid("counter field"))
        );
        // Optional flag bits and reserved fields are ignored (growth rule).
        let mut optional = good.clone();
        optional[7] = 0x80;
        optional[90] = 0xFF;
        optional[97] = 0xFF;
        assert_eq!(Descriptor::decode(&optional), Ok(sample_descriptor()));
    }

    /// Every damaged descriptor reproduced in review is refused by decode, and encode refuses the same values.
    #[test]
    fn unsafe_type_details_and_settings_are_rejected() {
        let check = |d: Descriptor, expected: &str| {
            let err = d.encode().expect_err(expected);
            assert!(err.to_string().contains(expected), "{err} vs {expected}");
            // Bypass encode's validation to produce the bytes a damaged journal or file would hold.
            let mut ok = d.clone();
            ok.payload = fixed("u64", 8, 8);
            ok.user_header = fixed("()", 0, 1);
            ok.system_header = fixed("Header", 48, 8);
            ok.settings = sample_descriptor().settings;
            ok.recorder_buffer_size = 0;
            ok.counter = None;
            let mut bytes = ok.encode().unwrap();
            patch(&mut bytes, &d);
            let err = Descriptor::decode(&bytes).expect_err(expected);
            assert!(err.to_string().contains(expected), "{err} vs {expected}");
        };
        let with = |f: &dyn Fn(&mut Descriptor)| {
            let mut d = sample_descriptor();
            f(&mut d);
            d
        };
        check(
            with(&|d| d.settings.history_size = 1 << 40),
            "history_size is 1099511627776",
        );
        check(
            with(&|d| d.settings.history_size = i64::MAX as u64),
            "history_size",
        );
        check(
            with(&|d| d.settings.max_subscribers = u64::MAX),
            "max_subscribers",
        );
        check(
            with(&|d| d.settings.subscriber_max_buffer_size = u64::MAX),
            "subscriber_max_buffer_size",
        );
        check(
            with(&|d| {
                d.settings.max_subscribers = MAX_PORTS;
                d.settings.subscriber_max_buffer_size = MAX_BUFFER_SIZE;
            }),
            "max_subscribers × (buffer + borrowed) + history_size",
        );
        check(
            with(&|d| {
                d.payload = TypeInfo {
                    variant: TypeVariant::Dynamic,
                    name: "u8".into(),
                    size: u64::MAX,
                    alignment: 1,
                }
            }),
            "payload size",
        );
        check(
            with(&|d| d.payload.alignment = 1 << 63),
            "payload alignment",
        );
        check(with(&|d| d.payload.alignment = 3), "power of two");
        check(with(&|d| d.payload.size = u64::MAX - 7), "payload size");
        check(with(&|d| d.payload.size = 12), "multiple of the alignment");
        check(
            with(&|d| d.user_header.variant = TypeVariant::Dynamic),
            "user header type",
        );
        check(
            with(&|d| {
                d.payload = TypeInfo {
                    variant: TypeVariant::Dynamic,
                    name: "()".into(),
                    size: 0,
                    alignment: 1,
                }
            }),
            "zero-sized elements",
        );
        check(
            with(&|d| {
                d.counter = Some(CounterField {
                    location: CounterLocation::Payload,
                    offset: 4,
                    width: 8,
                })
            }),
            "counter field",
        );
        check(
            with(&|d| d.recorder_buffer_size = u64::MAX),
            "recorder buffer size",
        );
    }

    /// Writes a descriptor's settings, type sizes, alignments, variants and counter over valid bytes, without
    /// validation.
    fn patch(bytes: &mut [u8], d: &Descriptor) {
        let s = &d.settings;
        for (i, v) in [
            s.history_size,
            s.subscriber_max_buffer_size,
            s.subscriber_max_borrowed_samples,
            s.max_publishers,
            s.max_subscribers,
            s.max_nodes,
            d.recorder_buffer_size,
        ]
        .into_iter()
        .enumerate()
        {
            bytes[32 + 8 * i..40 + 8 * i].copy_from_slice(&v.to_le_bytes());
        }
        if let Some(c) = d.counter {
            bytes[88] = if c.location == CounterLocation::UserHeader {
                1
            } else {
                2
            };
            bytes[89] = c.width;
            bytes[92..96].copy_from_slice(&c.offset.to_le_bytes());
        }
        let mut at = 96;
        for info in [&d.payload, &d.user_header, &d.system_header] {
            bytes[at] = u8::from(info.variant == TypeVariant::Dynamic);
            bytes[at + 8..at + 16].copy_from_slice(&info.size.to_le_bytes());
            bytes[at + 16..at + 24].copy_from_slice(&info.alignment.to_le_bytes());
            let name_len = u16::from_le_bytes([bytes[at + 2], bytes[at + 3]]);
            at += 24 + usize::from(name_len);
        }
    }

    #[test]
    fn chunk_layout_matches_iceoryx2s_formula() {
        // u64 payload after a 48-byte system header and a 16-byte user header: headers 64, chunk 72, alignment 8.
        let chunk =
            validate_types(&fixed("u64", 8, 8), &fixed("H", 16, 8), &fixed("S", 48, 8)).unwrap();
        assert_eq!(
            (
                chunk.headers_len,
                chunk.max_chunk_size,
                chunk.alignment,
                chunk.max_elements
            ),
            (64, 72, 8, 1)
        );
        // A u32 slice: the most elements a record can hold.
        let slice = TypeInfo {
            variant: TypeVariant::Dynamic,
            name: "u32".into(),
            size: 4,
            alignment: 4,
        };
        let chunk = validate_types(&slice, &fixed("()", 0, 1), &fixed("S", 48, 8)).unwrap();
        assert_eq!(chunk.max_elements, (MAX_RECORD_PAYLOAD - 40) / 4);
    }

    #[test]
    fn malformed_data_records_are_rejected() {
        let good = DataRecord {
            descriptor_seq: 1,
            publisher_id: 0,
            user_header: &[1, 2],
            payload: &[3],
        }
        .encode()
        .unwrap();
        assert_eq!(
            DataRecord::decode(&good[..39]),
            Err(LayoutError::Truncated("publisher id"))
        );
        let mut bad = good.clone();
        bad[0] = 9;
        assert_eq!(
            DataRecord::decode(&bad),
            Err(LayoutError::UnsupportedVersion(9))
        );
        let mut bad = good.clone();
        bad[2] = 1;
        assert_eq!(
            DataRecord::decode(&bad),
            Err(LayoutError::UnknownRequiredFlags {
                record: "data record",
                flags: 1
            })
        );
        let mut bad = good.clone();
        bad[4] = 39;
        assert_eq!(
            DataRecord::decode(&bad),
            Err(LayoutError::Invalid("data record header length"))
        );
        // An optional flag is ignored.
        let mut optional = good.clone();
        optional[3] = 1;
        assert!(DataRecord::decode(&optional).is_ok());
    }

    #[test]
    fn data_records_with_extension_entries_decode() {
        let record = DataRecord {
            descriptor_seq: 4,
            publisher_id: 7,
            user_header: &[1, 2],
            payload: &[3, 4, 5],
        };
        let plain = record.encode().unwrap();
        let mut extended = plain[..DATA_HEADER_LEN].to_vec();
        extended[4..6].copy_from_slice(&(DATA_HEADER_LEN as u16 + 12).to_le_bytes());
        extended.extend_from_slice(&[0x01, 0x00, 0, 0, 4, 0, 0, 0, 9, 9, 9, 9]);
        extended.extend_from_slice(&plain[DATA_HEADER_LEN..]);
        assert_eq!(DataRecord::decode(&extended), Ok(record));
        extended[41] = 0x80; // the same entry, required
        assert_eq!(
            DataRecord::decode(&extended),
            Err(LayoutError::UnknownRequiredExtension(0x8001))
        );
    }

    #[test]
    fn data_records_are_checked_against_their_descriptor() {
        let mut d = sample_descriptor();
        let ok = |uh: &[u8], pl: &[u8], d: &Descriptor| {
            DataRecord {
                descriptor_seq: 0,
                publisher_id: 0,
                user_header: uh,
                payload: pl,
            }
            .check(d)
        };
        assert_eq!(ok(&[], &[0; 8], &d), Ok(1));
        assert!(matches!(
            ok(&[], &[0; 7], &d),
            Err(LayoutError::Mismatch(_))
        ));
        assert!(matches!(
            ok(&[1], &[0; 8], &d),
            Err(LayoutError::Mismatch(_))
        ));
        d.payload = TypeInfo {
            variant: TypeVariant::Dynamic,
            name: "u32".into(),
            size: 4,
            alignment: 4,
        };
        assert_eq!(ok(&[], &[], &d), Ok(0));
        assert_eq!(ok(&[], &[0; 12], &d), Ok(3));
        assert!(matches!(
            ok(&[], &[0; 6], &d),
            Err(LayoutError::Mismatch(_))
        ));
    }

    #[test]
    fn loss_records_follow_the_modular_distance_rule() {
        let rec = |kind, width, expected, received| LossRecord {
            kind,
            width,
            descriptor_seq: 0,
            publisher_id: 0,
            expected,
            received,
        };
        // A gap with received < expected is refused for an eight-byte counter...
        assert!(rec(LossKind::Gap, 8, 10, 5).encode().is_err());
        let mut bytes = rec(LossKind::Backwards, 8, 10, 5).encode().unwrap();
        bytes[4] = 1;
        assert!(LossRecord::decode(&bytes).is_err());
        // ...and so is a "loss" with received == expected, or a value wider than the counter.
        assert!(rec(LossKind::Gap, 8, 5, 5).encode().is_err());
        assert!(rec(LossKind::Gap, 4, 5, 1 << 32).encode().is_err());
        // A four-byte counter that wraps past 0 is a gap of the modular distance.
        let wrap = rec(LossKind::Gap, 4, 0xFFFF_FFFE, 1);
        assert_eq!(wrap.missing(), 3);
        assert_eq!(LossRecord::decode(&wrap.encode().unwrap()).unwrap(), wrap);
        // After 0xFFFF_FFFF the recorder expects 0: a wrap is no loss.
        assert_eq!(
            LossKind::classify(4, (0xFFFF_FFFFu64 + 1) & 0xFFFF_FFFF, 0),
            None
        );
        assert_eq!(LossKind::classify(4, 0xFFFF_FFFF, 0), Some(LossKind::Gap));
        assert_eq!(LossKind::classify(4, 5, 4), Some(LossKind::Backwards));
        assert_eq!(LossKind::classify(8, 5, 4), Some(LossKind::Backwards));
        assert_eq!(LossKind::classify(8, 5, 0), Some(LossKind::Backwards));
        assert_eq!(LossKind::classify(8, 5, 9), Some(LossKind::Gap));
        // Checked against the descriptor's counter.
        let mut d = sample_descriptor();
        assert!(wrap.check(&d).is_err());
        d.payload = fixed("P", 8, 4);
        d.counter = Some(CounterField {
            location: CounterLocation::Payload,
            offset: 0,
            width: 4,
        });
        assert!(wrap.check(&d).is_ok());
    }

    #[test]
    fn counter_reads_little_endian_and_rejects_short_messages() {
        let c = CounterField {
            location: CounterLocation::Payload,
            offset: 2,
            width: 4,
        };
        assert_eq!(c.read(&[], &[0, 0, 1, 0, 0, 0]), Some(1));
        assert_eq!(c.read(&[], &[0, 0, 1, 0, 0]), None);
        let c = CounterField {
            location: CounterLocation::UserHeader,
            offset: u32::MAX,
            width: 8,
        };
        assert_eq!(c.read(&[0; 16], &[]), None);
    }

    #[test]
    fn unknown_reserved_schema_ids_decode_as_other() {
        assert_eq!(decode(0x4932_00FF, b"anything"), Ok(Decoded::Other));
        // A filler is skipped whatever it holds.
        assert_eq!(decode(SCHEMA_FILLER, &[0; 24]), Ok(Decoded::Filler(24)));
        assert_eq!(decode(SCHEMA_FILLER, b"anything"), Ok(Decoded::Filler(8)));
        assert_eq!(decode(1, b""), Ok(Decoded::Other));
    }
}
