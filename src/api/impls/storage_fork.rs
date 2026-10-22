//! Native fork resource preparation after one consistent runtime capture.
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;

use super::volumes::resolve_volume_mounts;
use crate::{
    orchestrator::SandboxMetadata,
    sandbox::{
        storage::StorageContext, ForkRuntimeMetadata, PreparedForkChild, SandboxForkPreparer,
        SandboxForkSpec, SandboxSnapshotManifest,
    },
    types::SandboxId,
    volume::{VolumeManager, VolumeMode},
};

struct ChildResources {
    context: StorageContext,
    owned: Vec<String>,
    reserved: Vec<String>,
}

pub(super) struct StorageForkPreparer {
    source: SandboxMetadata,
    volumes: Arc<VolumeManager>,
    children: Mutex<HashMap<SandboxId, ChildResources>>,
}
impl StorageForkPreparer {
    pub(super) fn new(source: SandboxMetadata, volumes: Arc<VolumeManager>) -> Self {
        Self {
            source,
            volumes,
            children: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl SandboxForkPreparer for StorageForkPreparer {
    async fn prepare(
        &self,
        manifest: &SandboxSnapshotManifest,
        child: &SandboxForkSpec,
    ) -> Result<PreparedForkChild> {
        let parent = self
            .source
            .storage_context
            .as_ref()
            .context("fork has no provider context")?;
        let context = parent
            .prepare_fork(child.sandbox_id, manifest.storage_checkpoint.as_ref())
            .await?;
        self.children.lock().await.insert(
            child.sandbox_id,
            ChildResources {
                context: context.clone(),
                owned: Vec::new(),
                reserved: Vec::new(),
            },
        );
        let mut spec = child.clone();
        let mut mounts = HashMap::new();
        for (path, reference) in &self.source.volume_mounts {
            let source = self.volumes.get(reference).await?;
            let captured = manifest
                .captured_volumes
                .iter()
                .find(|drive| {
                    drive.drive_id == source.id && drive.mount_path == std::path::Path::new(path)
                })
                .context("joint fork capture is missing a legacy volume")?;
            anyhow::ensure!(
                captured.read_only == (source.mode == VolumeMode::ReadOnly),
                "captured legacy mode changed"
            );
            let id = if captured.read_only {
                source.id
            } else {
                let layers = self
                    .volumes
                    .publish_captured_backing(&source.id, &captured.image_config_path)
                    .await?;
                let name = format!("volume-fork-{}", uuid::Uuid::now_v7().simple());
                let clone = self
                    .volumes
                    .create_consumer_snapshot_volume(
                        name,
                        VolumeMode::Exclusive,
                        captured.virtual_size.div_ceil(1024 * 1024),
                        layers,
                        &context,
                        crate::sandbox::storage::transition::retained(path),
                    )
                    .await?;
                self.children
                    .lock()
                    .await
                    .get_mut(&child.sandbox_id)
                    .context("fork child resource record disappeared")?
                    .owned
                    .push(clone.id.clone());
                spec.replace_drive_ids.push((source.id, clone.id.clone()));
                clone.id
            };
            mounts.insert(path.clone(), id);
        }
        // Record all possible reservations before resolving: cleanup also covers
        // a failure after a read-only share was reserved but before materialization.
        self.children
            .lock()
            .await
            .get_mut(&child.sandbox_id)
            .context("fork child resource record disappeared")?
            .reserved = mounts.values().cloned().collect();
        let (drives, mounts) =
            resolve_volume_mounts(&self.volumes, &mounts, &child.sandbox_id.to_string())
                .await
                .map_err(|error| anyhow::anyhow!("{}", error.message))?;
        spec.extra_drives = drives;
        Ok(PreparedForkChild {
            spec,
            metadata: ForkRuntimeMetadata {
                failed_start: None,
                storage_context: context,
                volume_mounts: mounts,
            },
        })
    }

    async fn cleanup_failed(&self, child: SandboxId) {
        let Some(resources) = self.children.lock().await.remove(&child) else {
            return;
        };
        // This records a durable cleanup task before contacting the provider.
        if let Err(error) = resources.context.control("delete").await {
            tracing::warn!(%child, %error, "fork provider cleanup queued for recovery");
        }
        if let Err(error) = self
            .volumes
            .replace_owner_for(&child.to_string(), None, &resources.reserved)
            .await
        {
            tracing::warn!(%child, %error, "retaining fork volumes after reservation cleanup failed");
            return;
        }
        for id in resources.owned {
            if let Err(error) = self.volumes.delete(&id).await {
                tracing::warn!(%child, %id, %error, "failed to delete unused fork volume");
            }
        }
    }
}
