use crate::db::SharedEngine;
use crate::ids::{DatasetId, ProjectId, TableId};
use crate::schema::Schema;

#[derive(Debug, Clone, Copy)]
pub enum ResourceKind {
    Project,
    Dataset,
    Table,
}

#[derive(Debug)]
pub enum CatalogError {
    NotFound { kind: ResourceKind, name: String },
    AlreadyExists { kind: ResourceKind, name: String },
    InternalError(String),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ProjectMeta {
    pub id: ProjectId,
    pub name: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct DatasetMeta {
    pub id: DatasetId,
    pub name: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct TableMeta {
    pub id: TableId,
    pub name: String,
    pub schema: Schema,
}

pub(crate) struct Catalog {
    // Read once the catalog bodies are filled in; held now so handles can
    // construct a Catalog on demand.
    #[allow(dead_code)]
    engine: SharedEngine,
}

impl Catalog {
    pub(crate) fn new(engine: SharedEngine) -> Self {
        Self { engine }
    }

    // --- Projects ---

    pub(crate) fn create_project(&self, _name: &str) -> Result<ProjectMeta, CatalogError> {
        todo!()
    }

    pub(crate) fn open_project(&self, _name: &str) -> Result<Option<ProjectMeta>, CatalogError> {
        todo!()
    }

    pub(crate) fn drop_project(&self, _name: &str) -> Result<(), CatalogError> {
        todo!()
    }

    pub(crate) fn list_projects(&self) -> Result<Vec<ProjectMeta>, CatalogError> {
        todo!()
    }

    // --- Datasets (scoped to a project) ---

    pub(crate) fn create_dataset(
        &self,
        _project_id: ProjectId,
        _name: &str,
    ) -> Result<DatasetMeta, CatalogError> {
        todo!()
    }

    pub(crate) fn open_dataset(
        &self,
        _project_id: ProjectId,
        _name: &str,
    ) -> Result<Option<DatasetMeta>, CatalogError> {
        todo!()
    }

    pub(crate) fn drop_dataset(
        &self,
        _project_id: ProjectId,
        _name: &str,
    ) -> Result<(), CatalogError> {
        todo!()
    }

    pub(crate) fn list_datasets(
        &self,
        _project_id: ProjectId,
    ) -> Result<Vec<DatasetMeta>, CatalogError> {
        todo!()
    }

    // --- Tables (scoped to a project + dataset) ---

    pub(crate) fn create_table(
        &self,
        _project_id: ProjectId,
        _dataset_id: DatasetId,
        _name: &str,
        _schema: Schema,
    ) -> Result<TableMeta, CatalogError> {
        todo!()
    }

    pub(crate) fn open_table(
        &self,
        _project_id: ProjectId,
        _dataset_id: DatasetId,
        _name: &str,
    ) -> Result<Option<TableMeta>, CatalogError> {
        todo!()
    }

    pub(crate) fn drop_table(
        &self,
        _project_id: ProjectId,
        _dataset_id: DatasetId,
        _name: &str,
    ) -> Result<(), CatalogError> {
        todo!()
    }

    pub(crate) fn list_tables(
        &self,
        _project_id: ProjectId,
        _dataset_id: DatasetId,
    ) -> Result<Vec<TableMeta>, CatalogError> {
        todo!()
    }
}
