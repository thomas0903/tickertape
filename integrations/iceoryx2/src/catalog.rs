use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tickertape::{Reader, ReaderConfig};

use crate::Error;
use crate::layout::{Descriptor, SCHEMA_DESCRIPTOR};

/// Finds and caches descriptor records by sequence number, with its own tickertape reader.
///
/// Every data and loss record names its descriptor's sequence number, so a reader that starts in the middle of a
/// journal resolves descriptors with one seek each, not a scan from the start. Descriptors are validated when they
/// are decoded ([`Descriptor::decode`]).
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

    /// The descriptor at `descriptor_seq`, named by the record at `seq`, or `None` if it is no longer in the
    /// journal: retention or `tickertape archive` removed the segment that held it. A reader skips and counts such
    /// records; it doesn't stop.
    ///
    /// # Errors
    ///
    /// [`Error::MissingDescriptor`] if `descriptor_seq` is not before `seq` or the record there is not a
    /// descriptor; [`Error::Layout`] if it is malformed or invalid; [`Error::Journal`] if reading fails.
    pub fn resolve(
        &mut self,
        seq: u64,
        descriptor_seq: u64,
    ) -> Result<Option<Arc<Descriptor>>, Error> {
        if let Some(d) = self.cache.get(&descriptor_seq) {
            return Ok(Some(Arc::clone(d)));
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
            Err(tickertape::Error::SeqUnavailable { .. }) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let Some(record) = reader.try_next()? else {
            return Err(missing("not in the journal".into()));
        };
        if record.seq != descriptor_seq {
            // The journal starts after it (removed while this reader was open).
            return Ok(None);
        }
        if record.schema_id != SCHEMA_DESCRIPTOR {
            return Err(missing(format!("it has schema id {:#x}", record.schema_id)));
        }
        let descriptor = Descriptor::decode(record.payload).map_err(|source| Error::Layout {
            seq: descriptor_seq,
            source,
        })?;
        Ok(Some(self.insert(descriptor_seq, descriptor)))
    }

    /// Like [`Catalog::resolve`], but a descriptor that is no longer in the journal is an error too.
    ///
    /// # Errors
    ///
    /// As for [`Catalog::resolve`], and [`Error::MissingDescriptor`] if the descriptor was removed.
    pub fn get(&mut self, seq: u64, descriptor_seq: u64) -> Result<Arc<Descriptor>, Error> {
        self.resolve(seq, descriptor_seq)?
            .ok_or_else(|| Error::MissingDescriptor {
                seq,
                descriptor_seq,
                reason: "removed from the journal with an older segment".into(),
            })
    }
}
