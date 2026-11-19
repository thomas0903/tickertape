//! The payload layout of recorded iceoryx2 messages, version 1. No iceoryx2 dependency.
//!
//! A recording is an ordinary tickertape journal. This crate writes three kinds of records into it, told apart by
//! the record's `schema_id`. Schema ids `0x4932_0000..=0x4932_FFFF` (`"I2"` followed by a 16-bit number) are
//! reserved for this crate; other schema ids may be mixed into the same journal by other writers and are ignored.
//!
//! | schema id     | constant              | payload                                      |
//! |---------------|-----------------------|----------------------------------------------|
//! | `0x4932_0001` | [`SCHEMA_DESCRIPTOR`] | [`Descriptor`]: one recorded service         |
//! | `0x4932_0002` | [`SCHEMA_DATA`]       | [`DataRecord`]: one message                  |
//! | `0x4932_0003` | [`SCHEMA_LOSS`]       | [`LossRecord`]: a gap in a publisher counter |
//!
//! All integers are little-endian. Strings are UTF-8 with a `u16` byte length in front and no terminator. A
//! decoder must reject a record whose layout version it doesn't know, whose reserved fields are not zero, or
//! that has bytes left over after the last field; this module does all three.
//!
//! The record's own fields carry the timestamps: `local_ts_ns` is when the recorder received the message
//! (nanoseconds since the Unix epoch, from the system clock), and `venue_ts_ns` is what an optional extractor read
//! from the user header, or 0.
//!
//! # Descriptor (`SCHEMA_DESCRIPTOR`)
//!
//! Written once per service per recording session, before the first message of that service. A session is one
//! run of the recorder; a recorder restarted on the same journal writes new descriptors.
//!
//! | offset | size | field                                                                                   |
//! |-------:|-----:|-----------------------------------------------------------------------------------------|
//! |      0 |    4 | magic, the bytes `TTI2`                                                                 |
//! |      4 |    2 | layout version, 1                                                                       |
//! |      6 |    2 | flags: bit 0 set means imported from a file whose times are relative (see `imported`); other bits 0 |
//! |      8 |    8 | session id: the journal's next sequence number when the recorder opened it               |
//! |     16 |    8 | session start, nanoseconds since the Unix epoch                                          |
//! |     24 |    6 | iceoryx2 version of the recording side: major, minor, patch, `u16` each                  |
//! |     30 |    1 | messaging pattern: 1 = publish-subscribe (the only value in version 1)                   |
//! |     31 |    1 | safe overflow: 1 if a full subscriber buffer drops its oldest sample, 0 if not           |
//! |     32 |    8 | service `history_size`                                                                   |
//! |     40 |    8 | service `subscriber_max_buffer_size`                                                     |
//! |     48 |    8 | service `subscriber_max_borrowed_samples`                                                |
//! |     56 |    8 | service `max_publishers`                                                                 |
//! |     64 |    8 | service `max_subscribers`                                                                |
//! |     72 |    8 | service `max_nodes`                                                                      |
//! |     80 |    8 | the recorder subscriber's buffer size (0 if unknown, for example after an import)       |
//! |     88 |    1 | publisher counter location: 0 none, 1 user header, 2 payload                             |
//! |     89 |    1 | publisher counter width in bytes: 0 (none), 4 or 8; unsigned little-endian               |
//! |     90 |    2 | reserved, 0                                                                              |
//! |     92 |    4 | publisher counter offset in bytes, from the start of the user header or payload          |
//! |     96 |      | three type details, in this order: payload, user header, iceoryx2's system header       |
//! |        |      | service name: `u16` length, then UTF-8 bytes                                             |
//! |        |      | recorder: `u16` length, then UTF-8 bytes (for example `tickertape-iceoryx2 0.1.0`)       |
//!
//! Each type detail is iceoryx2's `TypeDetail`:
//!
//! | offset | size | field                                                    |
//! |-------:|-----:|----------------------------------------------------------|
//! |      0 |    1 | variant: 0 fixed size, 1 dynamic (a slice of elements)    |
//! |      1 |    1 | reserved, 0                                              |
//! |      2 |    2 | type name length                                         |
//! |      4 |    4 | reserved, 0                                              |
//! |      8 |    8 | size in bytes (of one element, for a dynamic payload)     |
//! |     16 |    8 | alignment in bytes                                       |
//! |     24 |      | type name, UTF-8                                         |
//!
//! # Data record (`SCHEMA_DATA`)
//!
//! | offset            | size | field                                                       |
//! |------------------:|-----:|-------------------------------------------------------------|
//! |                 0 |    2 | layout version, 1                                           |
//! |                 2 |    2 | flags, 0                                                    |
//! |                 4 |    4 | user header length in bytes (`h`)                           |
//! |                 8 |    8 | sequence number of this service's descriptor record         |
//! |                16 |  `h` | user header bytes, as iceoryx2 stores them                  |
//! |          `16 + h` | rest | payload bytes, as iceoryx2 stores them                      |
//!
//! The payload length is the record's payload length minus `16 + h`. A dynamic payload holds
//! `payload length / element size` elements. Bytes are copied exactly as they are in shared memory, including any
//! padding inside the user's types. Neither part is aligned inside the record: copy before reinterpreting.
//!
//! Every data record names its descriptor by sequence number, so a reader that starts anywhere can seek to the
//! descriptor (with any tickertape reader) and decode the message without scanning from the start.
//!
//! # Loss record (`SCHEMA_LOSS`)
//!
//! Written only when the recorder was told where a publisher-side message counter is (see the descriptor's counter
//! fields), before the message that revealed the gap.
//!
//! | offset | size | field                                                                         |
//! |-------:|-----:|-------------------------------------------------------------------------------|
//! |      0 |    2 | layout version, 1                                                             |
//! |      2 |    2 | kind: 1 gap (counter jumped forward), 2 backwards (counter repeated or reset) |
//! |      4 |    4 | reserved, 0                                                                   |
//! |      8 |    8 | sequence number of the service's descriptor record                            |
//! |     16 |   16 | iceoryx2 publisher id (`u128`)                                                |
//! |     32 |    8 | expected counter value                                                        |
//! |     40 |    8 | received counter value                                                        |

use serde::Serialize;

/// Schema id of a [`Descriptor`] record.
pub const SCHEMA_DESCRIPTOR: u32 = 0x4932_0001;
/// Schema id of a [`DataRecord`].
pub const SCHEMA_DATA: u32 = 0x4932_0002;
/// Schema id of a [`LossRecord`].
pub const SCHEMA_LOSS: u32 = 0x4932_0003;
/// Schema ids reserved for this crate.
pub const SCHEMA_RESERVED: core::ops::RangeInclusive<u32> = 0x4932_0000..=0x4932_FFFF;

/// The layout version this module reads and writes.
pub const LAYOUT_VERSION: u16 = 1;
/// The first four bytes of a descriptor.
pub const DESCRIPTOR_MAGIC: [u8; 4] = *b"TTI2";
/// Length of a data record's header, before the user header.
pub const DATA_HEADER_LEN: usize = 16;
/// Length of a loss record.
pub const LOSS_RECORD_LEN: usize = 48;
const DESCRIPTOR_FIXED_LEN: usize = 96;
const TYPE_DETAIL_FIXED_LEN: usize = 24;
const FLAG_IMPORTED: u16 = 1;

/// Why a payload doesn't decode.
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
    /// A field has a value outside its range, or a reserved field is not zero.
    #[error("invalid {0}")]
    Invalid(&'static str),
    /// Bytes are left after the last field.
    #[error("{0} unexpected bytes after the last field")]
    TrailingBytes(usize),
    /// A string or the user header is too long to encode.
    #[error("{0} is too long to encode")]
    TooLong(&'static str),
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

/// The settings of a publish-subscribe service when it was recorded.
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

/// Where a publisher-side message counter is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CounterLocation {
    /// In the user header.
    UserHeader,
    /// In the payload.
    Payload,
}

/// A publisher-side message counter: an unsigned little-endian integer that each publisher increments by one per
/// message. With one, the recorder can detect messages lost before they reached it.
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
    /// Reads the counter from a message, or `None` if the message is too short.
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

/// One recorded service, written at the start of each recording session. See the [module docs](self) for bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Descriptor {
    /// The journal's next sequence number when the recorder opened it. Shared by all descriptors of a session.
    pub session_id: u64,
    /// When the session started, nanoseconds since the Unix epoch.
    pub session_start_unix_ns: u64,
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
    /// Encodes the descriptor.
    ///
    /// # Errors
    ///
    /// [`LayoutError::TooLong`] if a name is longer than 65,535 bytes.
    pub fn encode(&self) -> Result<Vec<u8>, LayoutError> {
        let mut out = Vec::with_capacity(256);
        out.extend_from_slice(&DESCRIPTOR_MAGIC);
        out.extend_from_slice(&LAYOUT_VERSION.to_le_bytes());
        let flags = if self.imported { FLAG_IMPORTED } else { 0 };
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&self.session_id.to_le_bytes());
        out.extend_from_slice(&self.session_start_unix_ns.to_le_bytes());
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
                if c.width != 4 && c.width != 8 {
                    return Err(LayoutError::Invalid("counter width"));
                }
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

    /// Decodes a descriptor.
    ///
    /// # Errors
    ///
    /// A [`LayoutError`] if the bytes are not a valid version-1 descriptor.
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
        if flags & !FLAG_IMPORTED != 0 {
            return Err(LayoutError::Invalid("descriptor flags"));
        }
        let session_id = c.u64("session id")?;
        let session_start_unix_ns = c.u64("session start")?;
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
        if c.u16("reserved")? != 0 {
            return Err(LayoutError::Invalid("reserved descriptor field"));
        }
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
        c.finish()?;
        Ok(Descriptor {
            session_id,
            session_start_unix_ns,
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
        })
    }
}

/// One recorded message, borrowed from a record's payload. See the [module docs](self) for bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataRecord<'a> {
    /// Sequence number of the service's [`Descriptor`] record.
    pub descriptor_seq: u64,
    /// User header bytes.
    pub user_header: &'a [u8],
    /// Payload bytes.
    pub payload: &'a [u8],
}

impl<'a> DataRecord<'a> {
    /// The encoded length of a message with these parts, or `None` on overflow.
    pub fn encoded_len(user_header_len: usize, payload_len: usize) -> Option<usize> {
        DATA_HEADER_LEN
            .checked_add(user_header_len)?
            .checked_add(payload_len)
    }

    /// Writes the record into `out`, which must be exactly [`DataRecord::encoded_len`] bytes long.
    ///
    /// # Errors
    ///
    /// [`LayoutError::TooLong`] if the user header is longer than `u32::MAX` bytes;
    /// [`LayoutError::Invalid`] if `out` has the wrong length.
    pub fn encode_into(&self, out: &mut [u8]) -> Result<(), LayoutError> {
        let uh = u32::try_from(self.user_header.len())
            .map_err(|_| LayoutError::TooLong("user header"))?;
        if Some(out.len()) != Self::encoded_len(self.user_header.len(), self.payload.len()) {
            return Err(LayoutError::Invalid("output buffer length"));
        }
        out[0..2].copy_from_slice(&LAYOUT_VERSION.to_le_bytes());
        out[2..4].copy_from_slice(&0u16.to_le_bytes());
        out[4..8].copy_from_slice(&uh.to_le_bytes());
        out[8..16].copy_from_slice(&self.descriptor_seq.to_le_bytes());
        let (header, payload) = out[DATA_HEADER_LEN..].split_at_mut(self.user_header.len());
        header.copy_from_slice(self.user_header);
        payload.copy_from_slice(self.payload);
        Ok(())
    }

    /// Encodes the record into a new vector.
    ///
    /// # Errors
    ///
    /// As for [`DataRecord::encode_into`].
    pub fn encode(&self) -> Result<Vec<u8>, LayoutError> {
        let len = Self::encoded_len(self.user_header.len(), self.payload.len())
            .ok_or(LayoutError::TooLong("message"))?;
        let mut out = vec![0; len];
        self.encode_into(&mut out)?;
        Ok(out)
    }

    /// Decodes a data record, borrowing from `bytes`.
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
        if c.u16("flags")? != 0 {
            return Err(LayoutError::Invalid("data record flags"));
        }
        let uh = c.u32("user header length")? as usize;
        let descriptor_seq = c.u64("descriptor sequence number")?;
        let user_header = c.take(uh, "user header")?;
        Ok(DataRecord {
            descriptor_seq,
            user_header,
            payload: &bytes[c.pos..],
        })
    }
}

/// What a [`LossRecord`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LossKind {
    /// The counter jumped forward: `received - expected` messages never reached the recorder.
    Gap,
    /// The counter went backwards or repeated: the publisher restarted, or a message was delivered twice (for
    /// example from the publisher's history after the recorder reconnected).
    Backwards,
}

/// A gap in a publisher-side counter. See the [module docs](self) for bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct LossRecord {
    /// Gap or backwards.
    pub kind: LossKind,
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
    /// Number of messages missing, for a gap; 0 otherwise.
    pub fn missing(&self) -> u64 {
        match self.kind {
            LossKind::Gap => self.received.saturating_sub(self.expected),
            LossKind::Backwards => 0,
        }
    }

    /// Encodes the record.
    pub fn encode(&self) -> [u8; LOSS_RECORD_LEN] {
        let mut out = [0u8; LOSS_RECORD_LEN];
        out[0..2].copy_from_slice(&LAYOUT_VERSION.to_le_bytes());
        let kind: u16 = match self.kind {
            LossKind::Gap => 1,
            LossKind::Backwards => 2,
        };
        out[2..4].copy_from_slice(&kind.to_le_bytes());
        out[8..16].copy_from_slice(&self.descriptor_seq.to_le_bytes());
        out[16..32].copy_from_slice(&self.publisher_id.to_le_bytes());
        out[32..40].copy_from_slice(&self.expected.to_le_bytes());
        out[40..48].copy_from_slice(&self.received.to_le_bytes());
        out
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
        let kind = match c.u16("loss kind")? {
            1 => LossKind::Gap,
            2 => LossKind::Backwards,
            _ => return Err(LayoutError::Invalid("loss kind")),
        };
        if c.u32("reserved")? != 0 {
            return Err(LayoutError::Invalid("reserved loss record field"));
        }
        let descriptor_seq = c.u64("descriptor sequence number")?;
        let publisher_id =
            u128::from_le_bytes(c.take(16, "publisher id")?.try_into().expect("16 bytes"));
        let expected = c.u64("expected counter")?;
        let received = c.u64("received counter")?;
        c.finish()?;
        Ok(LossRecord {
            kind,
            descriptor_seq,
            publisher_id,
            expected,
            received,
        })
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
        _ => Decoded::Other,
    })
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

    fn u8(&mut self, what: &'static str) -> Result<u8, LayoutError> {
        Ok(self.take(1, what)?[0])
    }

    fn u16(&mut self, what: &'static str) -> Result<u16, LayoutError> {
        Ok(u16::from_le_bytes(
            self.take(2, what)?.try_into().expect("2 bytes"),
        ))
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, LayoutError> {
        Ok(u32::from_le_bytes(
            self.take(4, what)?.try_into().expect("4 bytes"),
        ))
    }

    fn u64(&mut self, what: &'static str) -> Result<u64, LayoutError> {
        Ok(u64::from_le_bytes(
            self.take(8, what)?.try_into().expect("8 bytes"),
        ))
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
        let fixed = self.take(TYPE_DETAIL_FIXED_LEN, "type detail")?;
        let variant = match fixed[0] {
            0 => TypeVariant::FixedSize,
            1 => TypeVariant::Dynamic,
            _ => return Err(LayoutError::Invalid("type variant")),
        };
        if fixed[1] != 0 || fixed[4..8] != [0; 4] {
            return Err(LayoutError::Invalid("reserved type detail field"));
        }
        let name_len = u16::from_le_bytes([fixed[2], fixed[3]]);
        let size = u64::from_le_bytes(fixed[8..16].try_into().expect("8 bytes"));
        let alignment = u64::from_le_bytes(fixed[16..24].try_into().expect("8 bytes"));
        if alignment == 0 || !alignment.is_power_of_two() {
            return Err(LayoutError::Invalid("type alignment"));
        }
        let name = self.utf8(usize::from(name_len), "type name")?;
        Ok(TypeInfo {
            variant,
            name,
            size,
            alignment,
        })
    }

    fn finish(&self) -> Result<(), LayoutError> {
        match self.bytes.len() - self.pos {
            0 => Ok(()),
            n => Err(LayoutError::TrailingBytes(n)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn type_info() -> impl Strategy<Value = TypeInfo> {
        (
            any::<bool>(),
            "[a-zA-Z0-9_:<>()]{0,40}",
            any::<u64>(),
            0u32..16,
        )
            .prop_map(|(dynamic, name, size, a)| TypeInfo {
                variant: if dynamic {
                    TypeVariant::Dynamic
                } else {
                    TypeVariant::FixedSize
                },
                name,
                size,
                alignment: 1 << a,
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
            (any::<[u64; 6]>(), any::<bool>(), any::<u64>()),
            proptest::option::of((
                any::<bool>(),
                any::<u32>(),
                prop_oneof![Just(4u8), Just(8u8)],
            )),
            (type_info(), type_info(), type_info()),
            ("[a-z/_0-9]{1,60}", ".{0,30}"),
        )
            .prop_map(|(a, b, counter, types, names)| Descriptor {
                session_id: a.0,
                session_start_unix_ns: a.1,
                imported: a.2,
                iceoryx2_version: Version {
                    major: a.3.0,
                    minor: a.3.1,
                    patch: a.3.2,
                },
                settings: ServiceSettings {
                    history_size: b.0[0],
                    subscriber_max_buffer_size: b.0[1],
                    subscriber_max_borrowed_samples: b.0[2],
                    max_publishers: b.0[3],
                    max_subscribers: b.0[4],
                    max_nodes: b.0[5],
                    safe_overflow: b.1,
                },
                recorder_buffer_size: b.2,
                counter: counter.map(|(header, offset, width)| CounterField {
                    location: if header {
                        CounterLocation::UserHeader
                    } else {
                        CounterLocation::Payload
                    },
                    offset,
                    width,
                }),
                payload: types.0,
                user_header: types.1,
                system_header: types.2,
                service_name: names.0,
                recorder: names.1,
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
        fn descriptor_with_trailing_bytes_is_rejected(d in descriptor(), extra in prop::collection::vec(any::<u8>(), 1..8)) {
            let mut bytes = d.encode().unwrap();
            bytes.extend_from_slice(&extra);
            prop_assert_eq!(Descriptor::decode(&bytes), Err(LayoutError::TrailingBytes(extra.len())));
        }

        #[test]
        fn data_round_trips(seq in any::<u64>(), uh in prop::collection::vec(any::<u8>(), 0..64), pl in prop::collection::vec(any::<u8>(), 0..512)) {
            let record = DataRecord { descriptor_seq: seq, user_header: &uh, payload: &pl };
            let bytes = record.encode().unwrap();
            prop_assert_eq!(bytes.len(), 16 + uh.len() + pl.len());
            prop_assert_eq!(DataRecord::decode(&bytes).unwrap(), record);
        }

        #[test]
        fn data_with_header_longer_than_record_is_rejected(seq in any::<u64>(), uh in prop::collection::vec(any::<u8>(), 1..64), extra in 1u32..1000) {
            let record = DataRecord { descriptor_seq: seq, user_header: &uh, payload: &[] };
            let mut bytes = record.encode().unwrap();
            let claimed = uh.len() as u32 + extra;
            bytes[4..8].copy_from_slice(&claimed.to_le_bytes());
            prop_assert_eq!(DataRecord::decode(&bytes), Err(LayoutError::Truncated("user header")));
        }

        #[test]
        fn loss_round_trips(gap in any::<bool>(), seq in any::<u64>(), id in any::<u128>(), e in any::<u64>(), r in any::<u64>()) {
            let record = LossRecord { kind: if gap { LossKind::Gap } else { LossKind::Backwards }, descriptor_seq: seq, publisher_id: id, expected: e, received: r };
            prop_assert_eq!(LossRecord::decode(&record.encode()).unwrap(), record);
        }

        #[test]
        fn arbitrary_bytes_never_panic(schema in prop_oneof![Just(SCHEMA_DESCRIPTOR), Just(SCHEMA_DATA), Just(SCHEMA_LOSS), any::<u32>()], bytes in prop::collection::vec(any::<u8>(), 0..400)) {
            let _ = decode(schema, &bytes);
        }

        #[test]
        fn descriptor_bit_flips_never_panic(d in descriptor(), at in any::<prop::sample::Index>(), bit in 0u8..8) {
            let mut bytes = d.encode().unwrap();
            let i = at.index(bytes.len());
            bytes[i] ^= 1 << bit;
            let _ = Descriptor::decode(&bytes);
        }
    }

    fn sample_descriptor() -> Descriptor {
        Descriptor {
            session_id: 7,
            session_start_unix_ns: 1_700_000_000_000_000_000,
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
            payload: TypeInfo {
                variant: TypeVariant::FixedSize,
                name: "u64".into(),
                size: 8,
                alignment: 8,
            },
            user_header: TypeInfo {
                variant: TypeVariant::FixedSize,
                name: "()".into(),
                size: 0,
                alignment: 1,
            },
            system_header: TypeInfo {
                variant: TypeVariant::FixedSize,
                name: "Header".into(),
                size: 48,
                alignment: 8,
            },
            service_name: "my/service".into(),
            recorder: "test".into(),
        }
    }

    #[test]
    fn descriptor_fixed_fields_are_where_the_docs_say() {
        let d = sample_descriptor();
        let b = d.encode().unwrap();
        assert_eq!(&b[0..4], b"TTI2");
        assert_eq!(u16::from_le_bytes([b[4], b[5]]), 1);
        assert_eq!(u64::from_le_bytes(b[8..16].try_into().unwrap()), 7);
        assert_eq!(u16::from_le_bytes([b[26], b[27]]), 10);
        assert_eq!(b[30], 1);
        assert_eq!(b[31], 1);
        assert_eq!(u64::from_le_bytes(b[40..48].try_into().unwrap()), 2);
        assert_eq!(u64::from_le_bytes(b[64..72].try_into().unwrap()), 8);
        // Payload type detail starts at 96: variant, reserved, name length, reserved, size, alignment, name.
        assert_eq!(b[96], 0);
        assert_eq!(u16::from_le_bytes([b[98], b[99]]), 3);
        assert_eq!(u64::from_le_bytes(b[104..112].try_into().unwrap()), 8);
        assert_eq!(&b[120..123], b"u64");
    }

    #[test]
    fn malformed_descriptors_are_rejected() {
        let good = sample_descriptor().encode().unwrap();
        let cases: [(usize, u8, LayoutError); 6] = [
            (0, b'X', LayoutError::BadMagic),
            (4, 2, LayoutError::UnsupportedVersion(2)),
            (6, 2, LayoutError::Invalid("descriptor flags")),
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
        let mut bad = good;
        bad[112] = 3; // alignment 3
        assert_eq!(
            Descriptor::decode(&bad),
            Err(LayoutError::Invalid("type alignment"))
        );
    }

    #[test]
    fn malformed_data_records_are_rejected() {
        let good = DataRecord {
            descriptor_seq: 1,
            user_header: &[1, 2],
            payload: &[3],
        }
        .encode()
        .unwrap();
        assert_eq!(
            DataRecord::decode(&good[..15]),
            Err(LayoutError::Truncated("descriptor sequence number"))
        );
        let mut bad = good.clone();
        bad[0] = 9;
        assert_eq!(
            DataRecord::decode(&bad),
            Err(LayoutError::UnsupportedVersion(9))
        );
        let mut bad = good;
        bad[2] = 1;
        assert_eq!(
            DataRecord::decode(&bad),
            Err(LayoutError::Invalid("data record flags"))
        );
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
        assert_eq!(decode(1, b""), Ok(Decoded::Other));
    }
}
