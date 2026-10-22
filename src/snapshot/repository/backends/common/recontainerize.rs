//! Native configuration adapter for the shared layer publication algorithm.
use crate::sandbox::OverlaybdCompactOutput;
use crate::snapshot::repository::RepositoryResult;
pub(crate) use aenv_volume_core::recontainerize::PreparedLayerUpload;
use std::path::Path;
pub(crate) async fn prepare_layer_upload(
    source: &Path,
    mode: OverlaybdCompactOutput,
    trusted: Option<(&str, u64)>,
) -> RepositoryResult<PreparedLayerUpload> {
    aenv_volume_core::recontainerize::prepare_layer_upload(source, mode.into(), trusted)
        .await
        .map_err(Into::into)
}
