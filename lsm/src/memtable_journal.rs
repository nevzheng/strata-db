//! The memtable journal — the memtable's durable shadow.
//!
//! Every write is logged here (via the reusable [`journal`] crate) before it
//! reaches the memtable, and replayed on open to recover writes that hadn't
//! been flushed. It fills and is truncated in lockstep with the memtable. A
//! logged record is just a [`KeyValue`] — user key, seq, op, and value —
//! encoded with the very same codec as on-disk data blocks, so the journal and
//! the SSTables agree on the byte format.

use journal::{Codec, Journal, JournalError};

use crate::key::KeyValue;
use crate::storage::{Decode, Encode};

/// The memtable journal: a [`Journal`] of [`KeyValue`] records.
pub(crate) type MemtableJournal = Journal<KeyValueCodec>;

/// Encodes a [`KeyValue`] for the journal, reusing the data-block entry codec.
#[derive(Default)]
pub(crate) struct KeyValueCodec;

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
