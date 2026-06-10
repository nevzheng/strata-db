use uuid::Uuid;

macro_rules! id_newtype {
    ($name:ident) => {
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            serde::Serialize,
            serde::Deserialize,
        )]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            pub fn nil() -> Self {
                Self(Uuid::nil())
            }

            pub fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<Uuid> for $name {
            fn from(u: Uuid) -> Self {
                Self(u)
            }
        }
    };
}

id_newtype!(ProjectId);
id_newtype!(DatasetId);
id_newtype!(TableId);
id_newtype!(QueryId);

/// Monotonic per-table incarnation counter.
///
/// Bumped once per `CREATE OR REPLACE TABLE`; the largest value among a
/// table name's catalog rows identifies its live incarnation. Encoded
/// big-endian as a fixed-width segment of the row key, so incarnations
/// sort in creation order and a prefix scan isolates a single one — the
/// rest are retained-but-unreachable history (see the GC note in
/// [`crate::catalog`]).
///
/// A `u64` is bumped only by an explicit DDL statement, so even a
/// pathological `CREATE OR REPLACE` loop can't exhaust it in any
/// realistic timescale; [`next`](Self::next) still guards the boundary
/// rather than wrapping, because wrapping would alias new writes onto
/// old, un-GC'd incarnation data.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct TruncationId(pub u64);

impl TruncationId {
    /// The incarnation a freshly created table starts at.
    pub const INITIAL: Self = Self(0);

    /// Fixed-width big-endian encoding for use as a key segment.
    pub fn to_be_bytes(self) -> [u8; 8] {
        self.0.to_be_bytes()
    }

    /// Decode from a big-endian key segment.
    pub fn from_be_bytes(bytes: [u8; 8]) -> Self {
        Self(u64::from_be_bytes(bytes))
    }

    /// The next incarnation, or `None` if the counter is exhausted
    /// (`u64::MAX`). Never wraps — see the type-level note.
    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

impl Default for TruncationId {
    fn default() -> Self {
        Self::INITIAL
    }
}
