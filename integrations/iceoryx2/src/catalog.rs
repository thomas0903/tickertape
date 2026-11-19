use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tickertape::{Reader, ReaderConfig};

use crate::Error;
use crate::layout::{Descriptor, SCHEMA_DESCRIPTOR};

/// Finds and caches descriptor records by sequence number, with its own tickertape reader.
///
/// Every data record names its descriptor's sequence number, so a reader that starts in the middle of a journal
/// resolves descriptors with one seek each, not a scan from the start.
#[derive(Debug)]
pub struct Catalog {
    dir: PathBuf,
    reader: Option<Reader>,
    cache: HashMap<u64, Arc<Descriptor>>,
}

impl Catalog {
    /// A catalog of the journal in `dir`. Opens its reader on first use.
    pub fn new(dir: impl AsRef<Path>) -> Catalog {
        Catalog {
            dir: dir.as_ref().to_path_buf(),
            reader: None,
            cache: HashMap::new(),
        }
    }

    /// Adds a descriptor read elsewhere (for example by a reader passing over it).
    pub fn insert(&mut self, seq: u64, descriptor: Descriptor) -> Arc<Descriptor> {
        let descriptor = Arc::new(descriptor);
        self.cache.insert(seq, Arc::clone(&descriptor));
        descriptor
    }

    /// The descriptor at `descriptor_seq`, named by the record at `seq`.
    ///
    /// # Errors
    ///
    /// [`Error::MissingDescriptor`] if that record is not a descriptor or is no longer in the journal;
    /// [`Error::Layout`] if it is malformed; [`Error::Journal`] if reading fails.
    pub fn get(&mut self, seq: u64, descriptor_seq: u64) -> Result<Arc<Descriptor>, Error> {
        if let Some(d) = self.cache.get(&descriptor_seq) {
            return Ok(Arc::clone(d));
        }
        let missing = |reason: String| Error::MissingDescriptor {
            seq,
            descriptor_seq,
            reason,
        };
        if descriptor_seq >= seq {
            return Err(missing(
                "a descriptor must come before the records that name it".into(),
            ));
        }
        let reader = match &mut self.reader {
            Some(r) => r,
            None => self
                .reader
                .insert(Reader::open(&self.dir, ReaderConfig::default())?),
        };
        match reader.seek(descriptor_seq) {
            Ok(()) => {}
            Err(tickertape::Error::SeqUnavailable { first, .. }) => {
                return Err(missing(format!(
                    "removed from the journal, which now starts at {first}"
                )));
            }
            Err(e) => return Err(e.into()),
        }
        let record = reader
            .try_next()?
            .ok_or_else(|| missing("not in the journal".into()))?;
        if record.schema_id != SCHEMA_DESCRIPTOR {
            return Err(missing(format!("it has schema id {:#x}", record.schema_id)));
        }
        let descriptor = Descriptor::decode(record.payload).map_err(|source| Error::Layout {
            seq: descriptor_seq,
            source,
        })?;
        Ok(self.insert(descriptor_seq, descriptor))
    }
}
