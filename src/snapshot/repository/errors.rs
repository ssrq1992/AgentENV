use anyhow::Error as AnyhowError;
use thiserror::Error;

use crate::snapshot::types::SnapshotId;

pub type RepositoryResult<T> = Result<T, RepositoryError>;

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error("invalid repository request: {reason}")]
    InvalidRequest { reason: String },

    #[error("snapshot not found: {lookup}")]
    SnapshotNotFound { lookup: String },

    #[error("volume not found: {lookup}")]
    VolumeNotFound { lookup: String },

    #[error("volume name already exists: {name}")]
    VolumeNameConflict { name: String },

    #[error("snapshot alias not found: {alias}")]
    AliasNotFound { alias: String },

    #[error("alias '{alias}' already points to '{existing}', cannot rebind to '{new_id}'")]
    AliasConflict {
        alias: String,
        existing: SnapshotId,
        new_id: SnapshotId,
    },

    #[error("artifact not found: {artifact}")]
    ArtifactNotFound { artifact: String },

    #[error("managed layer not found: {digest}")]
    ManagedLayerNotFound { digest: String },

    #[error("integrity mismatch for {artifact}: expected {expected}, got {actual}")]
    IntegrityMismatch {
        artifact: String,
        expected: String,
        actual: String,
    },

    #[error("unsupported operation: {feature}")]
    Unsupported { feature: String },

    #[error("backend error: {message}")]
    Backend {
        message: String,
        #[source]
        source: Option<AnyhowError>,
    },
}

impl RepositoryError {
    pub fn backend(message: impl Into<String>, source: impl Into<AnyhowError>) -> Self {
        Self::Backend {
            message: message.into(),
            source: Some(source.into()),
        }
    }
}

impl From<aenv_volume_core::catalog::CatalogError> for RepositoryError {
    fn from(error: aenv_volume_core::catalog::CatalogError) -> Self {
        use aenv_volume_core::catalog::CatalogError as E;
        match error {
            E::InvalidRequest { reason } => Self::InvalidRequest { reason },
            E::VolumeNotFound { lookup } => Self::VolumeNotFound { lookup },
            E::VolumeNameConflict { name } => Self::VolumeNameConflict { name },
            E::Unsupported { feature } => Self::Unsupported { feature },
            E::Backend { message, source } => Self::Backend { message, source },
        }
    }
}
