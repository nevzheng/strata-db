//! Per-query execution context.
//!
//! Borrows the storage engine for the lifetime of a query and exposes the
//! layered handles every query operates through:
//!
//! - [`QueryContext::table`] / [`QueryContext::table_mut`] — typed row
//!   CRUD against a `Table`.
//! - [`QueryContext::catalog`] — read-side catalog operations.

use std::cell::RefMut;

use strata_store::StorageEngine;
use strata_store::memstore::BTreeMapStore;

use crate::catalog::CatalogReader;
use crate::catalog::tables::Table;
use crate::query::JoinConfig;
use crate::storage::table_api::{TableReader, TableWriter};

pub struct QueryContext<'db> {
    pub(crate) engine: RefMut<'db, StorageEngine<BTreeMapStore>>,
    /// Executor tunables (join scratch sizing), copied from the owning `Db` at
    /// context open. See [`JoinConfig`].
    pub(crate) join_config: JoinConfig,
}

impl QueryContext<'_> {
    /// Read handle over `table`. The returned reader owns a clone of
    /// the table's schema + ids, so its lifetime is tied to the engine
    /// only — `&table` can drop as soon as this call returns.
    pub fn table<'a>(&'a self, table: &Table) -> TableReader<'a> {
        TableReader::new(&self.engine, table)
    }

    /// Write handle over `table`. Same lifetime story as
    /// [`table`](Self::table) — the writer doesn't borrow `&table`.
    pub fn table_mut<'a>(&'a mut self, table: &Table) -> TableWriter<'a> {
        TableWriter::new(&mut self.engine, table)
    }

    /// Read-side catalog handle scoped to this context's engine lock.
    pub(crate) fn catalog(&self) -> CatalogReader<'_> {
        CatalogReader::new(&self.engine)
    }

    /// Executor tunables for this query (join scratch sizing).
    pub(crate) fn join_config(&self) -> JoinConfig {
        self.join_config
    }
}
