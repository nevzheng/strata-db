//! The memstore's durability journal.
//!
//! [`Journaled`] wraps any [`MemStore`], logging every write to a [`journal`]
//! before applying it and replaying the journal on open, so the store recovers
//! writes that hadn't been flushed. It truncates the journal when the store is
//! cleared (on flush). The rest of the LSM sees only a `MemStore` — the journal
//! is entirely internal to the memstore.

use std::ops::RangeBounds;
use std::path::Path;

use journal::{Codec, Journal, JournalError};

use crate::error::{ReadError, WriteError};
use crate::key::{InternalKey, KeyValue};
use crate::storage::{Decode, Encode};
use crate::store::{MemStore, ReadStore, WriteStore};

/// A [`MemStore`] made crash-safe by an append-only journal.
pub(crate) struct Journaled<M> {
    inner: M,
    journal: Journal<KeyValueCodec>,
}

impl<M: MemStore> Journaled<M> {
    /// Open the journal at `path` and replay it into `inner`, recovering any
    /// writes that hadn't been flushed.
    pub(crate) fn open(path: impl AsRef<Path>, mut inner: M) -> Result<Self, WriteError> {
        let journal = Journal::open(path).map_err(into_write)?;
        for record in journal.replay().map_err(into_write)? {
            let KeyValue { key, value } = record.map_err(into_write)?;
            inner.put(key, &value)?;
        }
        Ok(Self { inner, journal })
    }
}

fn into_write(e: JournalError) -> WriteError {
    WriteError::Internal(e.to_string())
}

impl<M: ReadStore> ReadStore for Journaled<M> {
    fn get_at(&self, key: &[u8], max_seq: u64) -> Result<Option<Vec<u8>>, ReadError> {
        self.inner.get_at(key, max_seq)
    }

    fn scan_at(
        &self,
        range: impl RangeBounds<Vec<u8>>,
        max_seq: u64,
    ) -> impl Iterator<Item = Result<(InternalKey, Vec<u8>), ReadError>> + '_ {
        self.inner.scan_at(range, max_seq)
    }
}

impl<M: WriteStore> WriteStore for Journaled<M> {
    fn put(&mut self, key: InternalKey, value: &[u8]) -> Result<(), WriteError> {
        // Log durably before applying, so a crash can't lose an acked write.
        self.journal
            .append(&KeyValue {
                key: key.clone(),
                value: value.to_vec(),
            })
            .map_err(into_write)?;
        self.inner.put(key, value)
    }
}

impl<M: MemStore> MemStore for Journaled<M> {
    fn size(&self) -> usize {
        self.inner.size()
    }

    fn clear(&mut self) -> Result<(), WriteError> {
        // The journal is intentionally NOT truncated here yet: without a
        // manifest, a flushed SSTable isn't rediscovered on open, so the
        // journal must retain every write for lossless recovery. Once the
        // manifest records flushes, truncation moves here (hence the fallible
        // signature).
        self.inner.clear()
    }
}

/// Encodes a [`KeyValue`] for the journal, reusing the data-block entry codec so
/// the journal and the SSTables agree on the byte format.
#[derive(Default)]
struct KeyValueCodec;

impl Codec for KeyValueCodec {
    type Record = KeyValue;

    fn encode(&self, record: &KeyValue, buf: &mut Vec<u8>) {
        record.encode(buf);
    }

    fn decode(&self, bytes: &[u8]) -> Result<KeyValue, JournalError> {
        let mut cursor = bytes;
        KeyValue::decode(&mut cursor).map_err(|e| JournalError::Decode(e.to_string()))
    }
}
