//! Original VolumeManager backing algorithms, shared by native and CSI callers.
use crate::{VolumeError, BYTES_PER_MB};
use overlaybd::config::{ImageConfig, LayerConfig};
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub async fn create_empty_backing(directory: &Path, size_mb: u64) -> Result<PathBuf, VolumeError> {
    create_empty_backing_inner(directory, size_mb, None).await
}

/// CSI callers allocate and persist the filesystem UUID before formatting.
/// A pre-existing working directory is never adopted or reformatted by this
/// entrypoint; unknown operations must inspect their original journal first.
pub async fn create_identified_empty_backing(
    directory: &Path,
    size_mb: u64,
    filesystem_uuid: &str,
) -> Result<PathBuf, VolumeError> {
    uuid::Uuid::parse_str(filesystem_uuid)
        .map_err(|error| VolumeError::Storage(format!("invalid filesystem UUID: {error}")))?;
    if size_mb == 0 || size_mb.checked_mul(BYTES_PER_MB).is_none() {
        return Err(VolumeError::InvalidSize);
    }
    tokio::fs::create_dir(directory)
        .await
        .map_err(|error| VolumeError::Storage(format!("claim fresh volume directory: {error}")))?;
    create_empty_backing_inner(directory, size_mb, Some(filesystem_uuid)).await
}

async fn create_empty_backing_inner(
    directory: &Path,
    size_mb: u64,
    filesystem_uuid: Option<&str>,
) -> Result<PathBuf, VolumeError> {
    let raw = directory.join("base.ext4");
    let commit = directory.join("base.commit");
    let image_config = directory.join("image.json");
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(|error| VolumeError::Storage(error.to_string()))?;

    let result = async {
        let file = tokio::fs::File::create(&raw)
            .await
            .map_err(|error| VolumeError::Storage(error.to_string()))?;
        file.set_len(
            size_mb
                .checked_mul(BYTES_PER_MB)
                .ok_or(VolumeError::InvalidSize)?,
        )
        .await
        .map_err(|error| VolumeError::Storage(error.to_string()))?;
        drop(file);
        let mut formatter = Command::new("mkfs.ext4");
        formatter.args(["-q", "-F"]);
        if let Some(uuid) = filesystem_uuid {
            formatter.args(["-U", uuid]);
        }
        let status = formatter
            .arg(&raw)
            .status()
            .await
            .map_err(|error| VolumeError::Storage(error.to_string()))?;
        if !status.success() {
            return Err(VolumeError::Storage(format!(
                "mkfs.ext4 exited with status {status}"
            )));
        }
        overlaybd::tools::package_raw_as_overlaybd(&raw, &commit)
            .await
            .map_err(|error| VolumeError::Storage(error.to_string()))?;
        let config = ImageConfig {
            lowers: vec![LayerConfig {
                file: commit.to_string_lossy().into_owned(),
                ..LayerConfig::default()
            }],
            ..ImageConfig::default()
        };
        let bytes = serde_json::to_vec_pretty(&config)
            .map_err(|error| VolumeError::Storage(error.to_string()))?;
        tokio::fs::write(&image_config, bytes)
            .await
            .map_err(|error| VolumeError::Storage(error.to_string()))?;
        let _ = tokio::fs::remove_file(&raw).await;
        Ok(image_config)
    }
    .await;

    // Managed operations retain partial artifacts for inspection. The legacy
    // entrypoint preserves its original best-effort cleanup behavior.
    if result.is_err() && filesystem_uuid.is_none() {
        let _ = tokio::fs::remove_dir_all(&directory).await;
    }
    result
}

pub async fn create_child_backing(
    directory: &Path,
    parent_config: &Path,
) -> Result<PathBuf, VolumeError> {
    create_child_backing_inner(directory, parent_config, false).await
}
/// Claim a new managed clone directory and retain partial artifacts on errors.
pub async fn create_identified_child_backing(
    directory: &Path,
    parent_config: &Path,
) -> Result<PathBuf, VolumeError> {
    tokio::fs::create_dir(directory)
        .await
        .map_err(|e| VolumeError::Storage(format!("claim managed clone directory: {e}")))?;
    create_child_backing_inner(directory, parent_config, true).await
}
async fn create_child_backing_inner(
    directory: &Path,
    parent_config: &Path,
    retain_on_failure: bool,
) -> Result<PathBuf, VolumeError> {
    let image_config = directory.join("image.json");
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(|error| VolumeError::Storage(error.to_string()))?;

    let result = async {
        let bytes = tokio::fs::read(parent_config)
            .await
            .map_err(|error| VolumeError::Storage(error.to_string()))?;
        let mut config: ImageConfig = serde_json::from_slice(&bytes)
            .map_err(|error| VolumeError::Storage(error.to_string()))?;
        for (index, layer) in config.lowers.iter_mut().enumerate() {
            if layer.file.is_empty() {
                continue;
            }
            let source = Path::new(&layer.file);
            let extension = source
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or("layer");
            let destination = directory.join(format!("lower-{index}.{extension}"));
            if let Err(link_error) = tokio::fs::hard_link(source, &destination).await {
                tokio::fs::copy(source, &destination).await.map_err(|copy_error| {
                    VolumeError::Storage(format!(
                        "clone volume layer '{}' (hard link failed: {link_error}; copy failed: {copy_error})",
                        source.display()
                    ))
                })?;
            }
            layer.file = destination.to_string_lossy().into_owned();
        }
        let bytes = serde_json::to_vec_pretty(&config)
            .map_err(|error| VolumeError::Storage(error.to_string()))?;
        tokio::fs::write(&image_config, bytes)
            .await
            .map_err(|error| VolumeError::Storage(error.to_string()))?;
        Ok(image_config.clone())
    }
    .await;
    if result.is_err() && !retain_on_failure {
        let _ = tokio::fs::remove_dir_all(&directory).await;
    }
    result
}
