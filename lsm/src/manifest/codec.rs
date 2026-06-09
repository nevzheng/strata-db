//! On-disk codec for [`ManifestEdit`], carried by the manifest journal.
//!
//! Staged: exercised by tests and the manifest manager; wired into the tree's
//! open/flush path in a later stage.
#![allow(dead_code)]

use journal::{Codec, JournalError};

use super::{ManifestEdit, ManifestOp, RunDescriptor, RunId};
use crate::SsTableId;

const OP_ADD_RUN: u8 = 1;
const OP_REMOVE_RUN: u8 = 2;
const OP_SET_NEXT_SST_ID: u8 = 3;

/// Encodes a [`ManifestEdit`] for the manifest journal: a count of ops, then a
/// tagged record per op.
#[derive(Default)]
pub(crate) struct ManifestEditCodec;

impl Codec for ManifestEditCodec {
    type Record = ManifestEdit;

    fn encode(&self, edit: &ManifestEdit, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&(edit.ops.len() as u32).to_be_bytes());
        for op in &edit.ops {
            match op {
                ManifestOp::AddRun(run) => {
                    buf.push(OP_ADD_RUN);
                    buf.extend_from_slice(&run.level.to_be_bytes());
                    buf.extend_from_slice(&run.run.0.to_be_bytes());
                    buf.extend_from_slice(&(run.files.len() as u32).to_be_bytes());
                    for file in &run.files {
                        buf.extend_from_slice(&file.0.to_be_bytes());
                    }
                }
                ManifestOp::RemoveRun(id) => {
                    buf.push(OP_REMOVE_RUN);
                    buf.extend_from_slice(&id.0.to_be_bytes());
                }
                ManifestOp::SetNextSstId(next) => {
                    buf.push(OP_SET_NEXT_SST_ID);
                    buf.extend_from_slice(&next.to_be_bytes());
                }
            }
        }
    }

    fn decode(&self, bytes: &[u8]) -> Result<ManifestEdit, JournalError> {
        let mut cursor = bytes;
        let count = get_u32(&mut cursor)?;
        let mut ops = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let op = match get_u8(&mut cursor)? {
                OP_ADD_RUN => {
                    let level = get_u32(&mut cursor)?;
                    let run = RunId(get_u64(&mut cursor)?);
                    let file_count = get_u32(&mut cursor)?;
                    let mut files = Vec::with_capacity(file_count as usize);
                    for _ in 0..file_count {
                        files.push(SsTableId(get_u64(&mut cursor)?));
                    }
                    ManifestOp::AddRun(RunDescriptor { level, run, files })
                }
                OP_REMOVE_RUN => ManifestOp::RemoveRun(RunId(get_u64(&mut cursor)?)),
                OP_SET_NEXT_SST_ID => ManifestOp::SetNextSstId(get_u64(&mut cursor)?),
                other => return Err(JournalError::Decode(format!("unknown manifest op {other}"))),
            };
            ops.push(op);
        }
        Ok(ManifestEdit { ops })
    }
}

fn take<'a>(bytes: &mut &'a [u8], n: usize) -> Result<&'a [u8], JournalError> {
    if bytes.len() < n {
        return Err(JournalError::Decode("truncated manifest edit".into()));
    }
    let (head, tail) = bytes.split_at(n);
    *bytes = tail;
    Ok(head)
}

fn get_u8(bytes: &mut &[u8]) -> Result<u8, JournalError> {
    Ok(take(bytes, 1)?[0])
}

fn get_u32(bytes: &mut &[u8]) -> Result<u32, JournalError> {
    Ok(u32::from_be_bytes(take(bytes, 4)?.try_into().unwrap()))
}

fn get_u64(bytes: &mut &[u8]) -> Result<u64, JournalError> {
    Ok(u64::from_be_bytes(take(bytes, 8)?.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Version;

    #[test]
    fn edit_round_trips_through_the_codec() {
        let edit = ManifestEdit::new()
            .add_run(RunDescriptor {
                level: 1,
                run: RunId(7),
                files: vec![SsTableId(10), SsTableId(11)],
            })
            .remove_run(RunId(3))
            .set_next_sst_id(42);

        let codec = ManifestEditCodec;
        let mut buf = Vec::new();
        codec.encode(&edit, &mut buf);
        let decoded = codec.decode(&buf).unwrap();
        assert_eq!(decoded, edit);

        // And it folds into the same version either way.
        let mut a = Version::default();
        a.apply(&edit);
        let mut b = Version::default();
        b.apply(&decoded);
        assert_eq!(a, b);
    }
}
