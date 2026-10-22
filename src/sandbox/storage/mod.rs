//! Thin native bridge. Provider protocol, recovery and policy live in the
//! sibling storage plugin; native device and artifact authority stays here.
use crate::cfg::ConfigManager;
pub(crate) use agentenv_storage_plugin::{
    artifact_gc, artifacts, capture_request, current_context, devices, publish_checkpoint, repository,
    request_scope, scope, transition, volume_command, AdmissionCleanup, PreparedExternalDrives,
};
pub use agentenv_storage_plugin::{freeze, ManagedMount, StorageContext, VolumeReference};
use anyhow::{Context, Result};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

struct NativeStorageHost;
impl agentenv_storage_plugin::RuntimeHost for NativeStorageHost {
    fn config(&self) -> agentenv_storage_plugin::ProviderConfig {
        let cfg = &ConfigManager::global_config().storage_provider;
        agentenv_storage_plugin::ProviderConfig {
            enabled: cfg.enabled,
            standalone: cfg.standalone,
            url: cfg.url.clone(),
            token_file: cfg.token_file.clone(),
            state_dir: cfg.state_dir.clone(),
            device_root: cfg.device_root.clone(),
            timeout_ms: cfg.timeout_ms,
        }
    }
    fn validate_device(&self, d: &devices::ExternalBlockDevice) -> Result<()> {
        let root = ConfigManager::global_config()
            .storage_provider
            .device_root
            .canonicalize()?;
        anyhow::ensure!(
            d.uuid.len() == 36 && d.uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'),
            "invalid filesystem UUID"
        );
        anyhow::ensure!(
            !d.id.is_empty() && d.id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "invalid device ID"
        );
        let canonical = d.path.canonicalize()?;
        anyhow::ensure!(
            canonical == d.path && canonical.starts_with(&root),
            "external device outside provider root"
        );
        let metadata = canonical.metadata()?;
        anyhow::ensure!(
            metadata.file_type().is_block_device()
                && nix::libc::major(metadata.rdev()) == d.major
                && nix::libc::minor(metadata.rdev()) == d.minor,
            "external block identity differs"
        );
        anyhow::ensure!(
            super::extra_drive::normalize_mount_path(d.guest_path.clone())? == d.guest_path
                && d.bytes > 0
                && d.bytes % 512 == 0,
            "invalid external mount or capacity"
        );
        Ok(())
    }
    fn remove_capture_root(&self, root: &Path) -> Result<()> {
        let base = ConfigManager::global_config()
            .firecracker
            .work_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("aenv"))
            .join("managed-snapshots")
            .canonicalize()?;
        anyhow::ensure!(
            root.canonicalize()? == root && root.parent() == Some(base.as_path()),
            "capture root outside native managed directory"
        );
        uuid::Uuid::parse_str(
            root.file_name()
                .and_then(|n| n.to_str())
                .context("invalid capture identity")?,
        )?;
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
pub(crate) fn install_host() {
    agentenv_storage_plugin::install_host(std::sync::Arc::new(NativeStorageHost));
}
pub(crate) async fn admit_compute(
    id: crate::types::SandboxId,
    resources: crate::types::SandboxResources,
    checkpoint: Option<&serde_json::Value>,
) -> Result<StorageContext> {
    agentenv_storage_plugin::admit_compute(
        id,
        resources.cpu_count,
        resources.memory_mib,
        checkpoint,
    )
    .await
}

/// Convert a validated neutral device into the native jailer attachment DTO.
pub(crate) fn device_mount(device: &devices::ExternalBlockDevice) -> super::extra_drive::DriveMount {
    super::extra_drive::DriveMount {
        drive_id: device.id.clone(),
        attachment_path: device.path.clone(),
        read_only: device.readonly,
    }
}
