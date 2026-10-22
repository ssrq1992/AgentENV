//! Errors at the native volume catalog boundary, independent of VM snapshots.
use anyhow::Error as AnyhowError;
use thiserror::Error;

pub type CatalogResult<T> = Result<T, CatalogError>;

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("invalid repository request: {reason}")]
    InvalidRequest { reason: String },
    #[error("volume not found: {lookup}")]
    VolumeNotFound { lookup: String },
    #[error("volume name already exists: {name}")]
    VolumeNameConflict { name: String },
    #[error("unsupported operation: {feature}")]
    Unsupported { feature: String },
    #[error("backend error: {message}")]
    Backend {
        message: String,
        #[source]
        source: Option<AnyhowError>,
    },
}

impl CatalogError {
    pub fn backend(message: impl Into<String>, source: impl Into<AnyhowError>) -> Self {
        Self::Backend {
            message: message.into(),
            source: Some(source.into()),
        }
    }
}
