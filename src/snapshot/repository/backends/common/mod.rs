pub(crate) mod acr;
pub(crate) mod recontainerize;

use std::path::{Path, PathBuf};

use anyhow::Result;
use overlaybd::dense_export;

use crate::snapshot::{ManagedLayer, OverlaybdLayerRef, RepositoryResult};

pub(crate) async fn materialize_volume_image_config(
    layers: &[OverlaybdLayerRef],
    destination: &Path,
    managed_layer: impl Fn(&ManagedLayer) -> overlaybd::config::LayerConfig,
) -> RepositoryResult<PathBuf> {
    aenv_volume_core::materialize::materialize_volume_image_config(
        layers,
        destination,
        managed_layer,
    )
    .await
    .map_err(Into::into)
}

pub(crate) async fn write_dense_overlaybd_layer_to_file(
    source: &Path,
    destination: &Path,
) -> Result<dense_export::DenseLayerDescriptor> {
    aenv_volume_core::materialize::write_dense_overlaybd_layer_to_file(source, destination).await
}
