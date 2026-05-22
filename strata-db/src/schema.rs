use crate::codec::DecodeError;
use crate::types::{Field, Tuple, Value};

/// Table schema — an ordered list of fields.
///
/// # Design note
///
/// We deliberately did **not** add an explicit `primary_key:
/// Vec<FieldName>` here, even though the eventual PK design will live at
/// the schema level. For now:
///
/// - A `Schema` is just an ordered `Vec<Field>`.
/// - Callers are expected to put fields in the lexicographic order they
///   want rows sorted by in storage — the leftmost field becomes the
///   most significant bytes of the row key when the encoder lands.
/// - We do not reorder fields or optimize for any particular access
///   pattern. Ordering is the user's responsibility.
///
/// Explicit primary-key declarations, secondary indexes, and storage
/// reordering are future work; none of that exists today.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Schema {
    pub fields: Vec<Field>,
}

impl Schema {
    /// Total bytes `encode` will produce for `tuple` under this schema:
    /// the null-bitmap plus the sum of per-value encoded sizes.
    pub fn encoded_size(&self, tuple: &Tuple) -> usize {
        let bitmap = self.fields.len().div_ceil(8);
        let payload: usize = tuple.values.iter().map(Value::encoded_size).sum();
        bitmap + payload
    }

    /// Encode a tuple to bytes using this schema's field layout.
    ///
    /// Layout: `[null-bitmap | value bytes...]`. The bitmap has one bit
    /// per field (`ceil(fields/8)` bytes); bit `i` set means field `i`
    /// is null and contributes no value bytes.
    pub fn encode(&self, tuple: &Tuple) -> Vec<u8> {
        assert_eq!(
            self.fields.len(),
            tuple.values.len(),
            "tuple length must match schema field count (schema={}, tuple={})",
            self.fields.len(),
            tuple.values.len()
        );

        let mut buf = Vec::with_capacity(self.encoded_size(tuple));
        let bitmap_bytes = self.fields.len().div_ceil(8);
        buf.resize(bitmap_bytes, 0);

        for (i, value) in tuple.values.iter().enumerate() {
            if matches!(value, Value::Null) {
                buf[i / 8] |= 1 << (i % 8);
            } else {
                value.encode(&mut buf);
            }
        }
        buf
    }

    /// Decode bytes back to a tuple, interpreting them per this schema.
    ///
    /// Reads the bitmap, then walks the fields in order: null bits
    /// produce `Value::Null` with no byte advance; cleared bits invoke
    /// the per-type decoder. Errors on a short buffer, malformed values,
    /// or unconsumed trailing bytes.
    pub fn decode(&self, bytes: &[u8]) -> Result<Tuple, DecodeError> {
        let bitmap_bytes = self.fields.len().div_ceil(8);
        if bytes.len() < bitmap_bytes {
            return Err(DecodeError::UnexpectedEof);
        }
        let (bitmap, mut cursor) = bytes.split_at(bitmap_bytes);

        let mut values = Vec::with_capacity(self.fields.len());
        for (i, field) in self.fields.iter().enumerate() {
            let is_null = (bitmap[i / 8] >> (i % 8)) & 1 == 1;
            if is_null {
                values.push(Value::Null);
            } else {
                values.push(Value::decode(field.ty, &mut cursor)?);
            }
        }

        if !cursor.is_empty() {
            return Err(DecodeError::TrailingBytes);
        }

        Ok(Tuple { values })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Field, LogicalType, Value};

    fn schema(fields: Vec<Field>) -> Schema {
        Schema { fields }
    }

    fn tuple(values: Vec<Value>) -> Tuple {
        Tuple { values }
    }

    #[test]
    fn empty_schema_roundtrip() {
        let s = schema(vec![]);
        let t = tuple(vec![]);
        let bytes = s.encode(&t);
        assert_eq!(bytes.len(), s.encoded_size(&t));
        assert_eq!(s.decode(&bytes).unwrap(), t);
    }

    #[test]
    fn mixed_types_roundtrip() {
        let s = schema(vec![
            Field::new("flag", LogicalType::Bool),
            Field::new("count", LogicalType::Int64),
            Field::new("name", LogicalType::Text),
        ]);
        let t = tuple(vec![
            Value::Bool(true),
            Value::Int64(-7),
            Value::Text("hello".into()),
        ]);
        let bytes = s.encode(&t);
        assert_eq!(bytes.len(), s.encoded_size(&t));
        assert_eq!(s.decode(&bytes).unwrap(), t);
    }

    #[test]
    fn nulls_in_various_positions_roundtrip() {
        let s = schema(vec![
            Field::new("a", LogicalType::Int32),
            Field::new("b", LogicalType::Text),
            Field::new("c", LogicalType::Bool),
            Field::new("d", LogicalType::Int16),
        ]);
        let t = tuple(vec![
            Value::Null,
            Value::Text("middle".into()),
            Value::Null,
            Value::Int16(99),
        ]);
        let bytes = s.encode(&t);
        let decoded = s.decode(&bytes).unwrap();
        assert_eq!(decoded, t);
    }

    #[test]
    fn many_fields_bitmap_spans_multiple_bytes() {
        let fields: Vec<Field> = (0..10)
            .map(|i| Field::new(format!("f{i}"), LogicalType::Bool))
            .collect();
        let s = schema(fields);
        let values = (0..10)
            .map(|i| {
                if i % 2 == 0 {
                    Value::Bool(true)
                } else {
                    Value::Null
                }
            })
            .collect();
        let t = tuple(values);
        let bytes = s.encode(&t);
        assert_eq!(s.decode(&bytes).unwrap(), t);
    }

    #[test]
    fn truncated_payload_errors() {
        let s = schema(vec![Field::new("n", LogicalType::Int64)]);
        let mut bytes = s.encode(&tuple(vec![Value::Int64(1)]));
        bytes.truncate(bytes.len() - 1);
        assert!(matches!(s.decode(&bytes), Err(DecodeError::UnexpectedEof)));
    }

    #[test]
    fn trailing_bytes_error() {
        let s = schema(vec![Field::new("n", LogicalType::Int32)]);
        let mut bytes = s.encode(&tuple(vec![Value::Int32(1)]));
        bytes.push(0xAA);
        assert!(matches!(s.decode(&bytes), Err(DecodeError::TrailingBytes)));
    }

    #[test]
    #[should_panic(expected = "tuple length must match schema field count")]
    fn encode_length_mismatch_panics() {
        let s = schema(vec![Field::new("a", LogicalType::Bool)]);
        let _ = s.encode(&tuple(vec![]));
    }
}
