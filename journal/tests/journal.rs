//! End-to-end tests for the journal: append/replay, durability across reopen,
//! checkpointing via truncate, crash-safe torn-tail recovery, and custom codecs.

use journal::{BytesCodec, Codec, Journal, JournalError};

#[test]
fn append_then_replay_in_order() {
    let tmp = tempfile::tempdir().unwrap();
    let mut j = Journal::<BytesCodec>::open(tmp.path().join("j")).unwrap();
    j.append(&b"first".to_vec()).unwrap();
    j.append(&b"second".to_vec()).unwrap();

    let got: Vec<Vec<u8>> = j.replay().unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(got, vec![b"first".to_vec(), b"second".to_vec()]);
}

#[test]
fn replay_of_empty_journal_is_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let j = Journal::<BytesCodec>::open(tmp.path().join("j")).unwrap();
    assert_eq!(j.replay().unwrap().count(), 0);
}

#[test]
fn records_survive_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("j");
    {
        let mut j = Journal::<BytesCodec>::open(&path).unwrap();
        j.append(&b"a".to_vec()).unwrap();
        j.append(&b"b".to_vec()).unwrap();
    }
    let j = Journal::<BytesCodec>::open(&path).unwrap();
    let got: Vec<Vec<u8>> = j.replay().unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(got, vec![b"a".to_vec(), b"b".to_vec()]);
}

#[test]
fn truncate_discards_records_and_journal_stays_usable() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("j");
    let mut j = Journal::<BytesCodec>::open(&path).unwrap();
    j.append(&b"x".to_vec()).unwrap();
    j.truncate().unwrap();
    assert!(j.replay().unwrap().next().is_none());

    j.append(&b"y".to_vec()).unwrap();
    let got: Vec<Vec<u8>> = j.replay().unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(got, vec![b"y".to_vec()]);
}

#[test]
fn torn_tail_is_ignored() {
    use std::io::Write;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("j");
    {
        let mut j = Journal::<BytesCodec>::open(&path).unwrap();
        j.append(&b"one".to_vec()).unwrap();
        j.append(&b"two".to_vec()).unwrap();
    }
    // Simulate a crash mid-append: a frame whose CRC won't match its payload.
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(&3u32.to_be_bytes()).unwrap(); // len = 3
        f.write_all(&0xDEAD_BEEFu32.to_be_bytes()).unwrap(); // bogus crc
        f.write_all(b"xyz").unwrap();
    }
    let j = Journal::<BytesCodec>::open(&path).unwrap();
    let got: Vec<Vec<u8>> = j.replay().unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(
        got,
        vec![b"one".to_vec(), b"two".to_vec()],
        "the torn trailing frame must be discarded"
    );
}

#[test]
fn bad_header_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("j");
    std::fs::write(&path, b"not a journal at all").unwrap();
    assert!(matches!(
        Journal::<BytesCodec>::open(&path),
        Err(JournalError::BadHeader)
    ));
}

/// A typed codec: records are `u64`s stored big-endian.
struct U64Codec;
impl Codec for U64Codec {
    type Record = u64;
    fn encode(&self, record: &u64, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&record.to_be_bytes());
    }
    fn decode(&self, bytes: &[u8]) -> Result<u64, JournalError> {
        bytes
            .try_into()
            .map(u64::from_be_bytes)
            .map_err(|_| JournalError::Decode("expected 8 bytes".into()))
    }
}

#[test]
fn custom_codec_round_trips_typed_records() {
    let tmp = tempfile::tempdir().unwrap();
    let mut j = Journal::with_codec(tmp.path().join("j"), U64Codec).unwrap();
    for n in [1u64, 42, 9999] {
        j.append(&n).unwrap();
    }
    let got: Vec<u64> = j.replay().unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(got, vec![1, 42, 9999]);
}
