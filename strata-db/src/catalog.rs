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

// --- Catalog (top-level) ---

pub(crate) struct Catalog {
    // Read once the catalog bodies are filled in.
    #[allow(dead_code)]
    engine: SharedEngine,
}

impl Catalog {
    pub(crate) fn new(engine: SharedEngine) -> Self {
        Self { engine }
    }

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

    /// Narrow the catalog to a single project's scope for dataset operations.
    pub(crate) fn project(&self, project_id: ProjectId) -> CatalogProject {
        CatalogProject {
            engine: self.engine.clone(),
            project_id,
        }
    }
}

// --- CatalogProject (scoped to one project) ---

pub(crate) struct CatalogProject {
    #[allow(dead_code)]
    engine: SharedEngine,
    #[allow(dead_code)]
    project_id: ProjectId,
}

impl CatalogProject {
    pub(crate) fn create_dataset(&self, _name: &str) -> Result<DatasetMeta, CatalogError> {
        todo!()
    }

    pub(crate) fn open_dataset(&self, _name: &str) -> Result<Option<DatasetMeta>, CatalogError> {
        todo!()
    }

    pub(crate) fn drop_dataset(&self, _name: &str) -> Result<(), CatalogError> {
        todo!()
    }

    pub(crate) fn list_datasets(&self) -> Result<Vec<DatasetMeta>, CatalogError> {
        todo!()
    }

    /// Narrow further to a single dataset's scope for table operations.
    pub(crate) fn dataset(&self, dataset_id: DatasetId) -> CatalogDataset {
        CatalogDataset {
            engine: self.engine.clone(),
            project_id: self.project_id,
            dataset_id,
        }
    }
}

// --- CatalogDataset (scoped to one project + dataset) ---

pub(crate) struct CatalogDataset {
    #[allow(dead_code)]
    engine: SharedEngine,
    #[allow(dead_code)]
    project_id: ProjectId,
    #[allow(dead_code)]
    dataset_id: DatasetId,
}

impl CatalogDataset {
    pub(crate) fn create_table(
        &self,
        _name: &str,
        _schema: Schema,
    ) -> Result<TableMeta, CatalogError> {
        todo!()
    }

    pub(crate) fn open_table(&self, _name: &str) -> Result<Option<TableMeta>, CatalogError> {
        todo!()
    }

    pub(crate) fn drop_table(&self, _name: &str) -> Result<(), CatalogError> {
        todo!()
    }

    pub(crate) fn list_tables(&self) -> Result<Vec<TableMeta>, CatalogError> {
        todo!()
    }
}
