use std::path::PathBuf;

use crate::layout::LayoutError;

/// Everything that can go wrong in this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The journal failed.
    #[error(transparent)]
    Journal(#[from] tickertape::Error),
    /// A record of this crate's schemas is malformed.
    #[error("record {seq}: {source}")]
    Layout {
        /// The record's sequence number.
        seq: u64,
        /// What is wrong with it.
        #[source]
        source: LayoutError,
    },
    /// A data or loss record names a record that isn't a descriptor (or, from [`Catalog::get`](crate::Catalog::get),
    /// a descriptor that retention removed).
    #[error("record {seq} names descriptor record {descriptor_seq}: {reason}")]
    MissingDescriptor {
        /// The record that names it.
        seq: u64,
        /// The sequence number it names.
        descriptor_seq: u64,
        /// What was found instead.
        reason: String,
    },
    /// An iceoryx2 call failed.
    #[error("iceoryx2: {0}")]
    Iceoryx2(String),
    /// An existing service doesn't match the recorded types or settings, or a typed service doesn't match.
    #[error("service \"{service}\" is incompatible with the recording: {reason}")]
    Incompatible {
        /// The service on the bus.
        service: String,
        /// What differs.
        reason: String,
    },
    /// A service's types, settings or messages are outside what the [layout](crate::layout#validity-rules) accepts,
    /// so it can't be recorded or replayed.
    #[error("service \"{service}\" can't be recorded or replayed: {reason}")]
    Unsupported {
        /// The service.
        service: String,
        /// Which rule it breaks.
        reason: String,
    },
    /// The request can't be carried out as given.
    #[error("{0}")]
    Usage(String),
    /// A file operation failed.
    #[error("{op} {}: {source}", path.display())]
    Io {
        /// What was being done.
        op: &'static str,
        /// The file.
        path: PathBuf,
        /// The cause.
        #[source]
        source: std::io::Error,
    },
    /// An iceoryx2 recording file can't be read or written.
    #[error("iceoryx2 recording file: {0}")]
    Upstream(String),
}

impl Error {
    /// True if the cause is damaged or unreadable data (journal corruption, an unsupported format version, or a
    /// malformed record), as opposed to usage, I/O or iceoryx2 errors. The command-line tool exits with 2 for these.
    pub fn is_data_error(&self) -> bool {
        matches!(
            self,
            Error::Journal(
                tickertape::Error::Corrupt { .. } | tickertape::Error::UnsupportedVersion { .. }
            ) | Error::Layout { .. }
                | Error::MissingDescriptor { .. }
        )
    }

    /// True if the journal itself failed (the recorder stops for these; other errors affect one service).
    pub fn is_journal_error(&self) -> bool {
        matches!(self, Error::Journal(_))
    }

    #[cfg(feature = "iceoryx2")]
    pub(crate) fn iox(context: &str, e: impl core::fmt::Debug) -> Error {
        Error::Iceoryx2(format!("{context}: {e:?}"))
    }
}
