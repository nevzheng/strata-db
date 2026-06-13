//! Shared fixtures for strata-db integration tests.

use strata_db::Db;
use tempfile::TempDir;

/// Open a `Db` backed by a fresh tempdir.
///
/// Returns both so the caller keeps the `TempDir` alive for the test's
/// duration — if it drops, the dir gets cleaned up under the engine.
pub fn temp_db() -> (TempDir, Db) {
    let tmp = TempDir::new().expect("tempdir");
    let db = Db::open(tmp.path()).expect("open db");
    (tmp, db)
}
