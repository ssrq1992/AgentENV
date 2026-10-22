use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::{stream, StreamExt, TryStreamExt};
use overlaybd::config::{load_image_config as load_overlaybd_image_config, LayerConfig};
use tracing::{debug, info, warn};

use super::client::{OssClient, OssUploadArtifact};
use super::layout::OssSnapshotArtifactLayout;
use crate::cfg::{SnapshotImageStoragePolicy, SnapshotPublishCompressionConfig};
use crate::sandbox::{OverlaybdCompactOutput, SandboxSnapshotManifest};
use crate::snapshot::repository::backends::common::acr::{
    AcrDiskImageExporter, DiskImageExportOutcome, DiskImageSubject, SnapshotOciConfigInput,
};
use crate::snapshot::repository::backends::common::materialize_volume_image_config;
use crate::snapshot::repository::backends::common::recontainerize::PreparedLayerUpload;
use crate::snapshot::repository::interfaces::{SnapshotPublication, SnapshotRepository};
use crate::snapshot::repository::{
    BuildCacheState, RepositoryError, RepositoryResult, VolumeRecordPage,
};
use crate::snapshot::{
    CommittedAttachedDrive, CommittedSnapshot, ManagedLayer, MemoryStartupPackInfo,
    OverlaybdLayerRef, PersistedDiskImagePublication, SnapshotAlias, SnapshotId,
    SnapshotListFilter, SnapshotPublishMetadata, SnapshotPublishSource, SnapshotRecord,
    SnapshotSource, SnapshotSourceKind, TemplateBuildErrorReason, TemplateBuildInfo,
    TemplateBuildStatus, MEMORY_STARTUP_PACK_ARTIFACT, SNAPSHOT_ARTIFACT_LAYOUT,
};
use crate::volume::VolumeRecord;
#[cfg(test)]
use crate::{snapshot::ExternalLayer, volume::is_valid_volume_component};

/// Manages the committed‐state layer of the OSS snapshot repository.
///
/// Object layout under the configured prefix:
///
/// ```text
/// catalog/aliases/{name}.json              → "snapshot-id"
/// volumes/records/{volume-id}.json         → VolumeRecord
/// volumes/aliases/{volume-name}.json       → "volume-id"
/// artifacts/{id}/firecracker-manifest.json → SandboxSnapshotManifest (paths omitted)
/// artifacts/{id}/vm_state.bin
/// managed-layers/{digest}
/// ```
pub(crate) struct OssSnapshotRepository {
    client: Arc<OssClient>,
    snapshot_image_storage: SnapshotImageStoragePolicy,
    acr_exporter: AcrDiskImageExporter,
    publish_compression: OverlaybdCompactOutput,
}

const MAX_ALIAS_BIND_ATTEMPTS: usize = 5;
const MAX_VOLUME_CAS_ATTEMPTS: usize = 5;

impl OssSnapshotRepository {
    pub(crate) fn new(
        client: Arc<OssClient>,
        snapshot_image_storage: SnapshotImageStoragePolicy,
        publish_compression: &SnapshotPublishCompressionConfig,
    ) -> Self {
        let publish_compression =
            OverlaybdCompactOutput::from_publish_compression_config(publish_compression);
        Self {
            client,
            snapshot_image_storage,
            acr_exporter: AcrDiskImageExporter::new(publish_compression),
            publish_compression,
        }
    }

    fn layout<'a>(&self, id: &'a SnapshotId) -> OssSnapshotArtifactLayout<'a> {
        OssSnapshotArtifactLayout::new(id)
    }

    async fn snapshot_exists(&self, id: &SnapshotId) -> RepositoryResult<bool> {
        self.client
            .exists(&OssSnapshotArtifactLayout::record_key(id))
            .await
            .map_err(|e| RepositoryError::backend(format!("check snapshot record '{id}'"), e))
    }

    async fn update_build_cache<T>(
        &self,
        update: impl Fn(&mut BuildCacheState) -> RepositoryResult<T>,
    ) -> RepositoryResult<T> {
        let key = "template-build/cache-head.json";
        for _ in 0..MAX_VOLUME_CAS_ATTEMPTS {
            let (mut state, etag) = match self.client.get_bytes_with_etag(key).await {
                Ok((bytes, Some(etag))) => (BuildCacheState::decode(&bytes)?, Some(etag)),
                Ok((_, None)) => {
                    return Err(RepositoryError::InvalidRequest {
                        reason: "build cache publication requires object storage ETags".to_owned(),
                    })
                }
                Err(error) if OssClient::is_not_found_error(&error) => {
                    (BuildCacheState::default(), None)
                }
                Err(error) => {
                    return Err(RepositoryError::backend("read build cache state", error))
                }
            };
            let result = update(&mut state)?;
            let bytes = serde_json::to_vec(&state)
                .map_err(|error| RepositoryError::backend("encode build cache state", error))?;
            if self
                .client
                .put_bytes_conditionally(key, bytes, etag.as_deref())
                .await
                .map_err(|error| RepositoryError::backend("update build cache state", error))?
            {
                return Ok(result);
            }
        }
        Err(RepositoryError::InvalidRequest {
            reason: "build cache state changed too often during publication".to_owned(),
        })
    }
}

fn validated_alias_key(alias: &str) -> RepositoryResult<String> {
    SnapshotAlias::parse(alias).map_err(|e| RepositoryError::InvalidRequest {
        reason: format!("invalid alias '{alias}': {e}"),
    })?;
    Ok(OssSnapshotArtifactLayout::alias_key(alias))
}

#[cfg(test)]
fn validate_volume_id(volume_id: &str) -> RepositoryResult<()> {
    validate_volume_component(volume_id, "id")
}

#[cfg(test)]
fn validate_volume_component(value: &str, kind: &str) -> RepositoryResult<()> {
    if !is_valid_volume_component(value) {
        return Err(RepositoryError::InvalidRequest {
            reason: format!("invalid volume {kind} '{value}'"),
        });
    }
    Ok(())
}

#[cfg(test)]
fn volume_id_from_record_key(key: &str) -> RepositoryResult<String> {
    let file_name = key
        .strip_prefix(OssSnapshotArtifactLayout::volume_records_prefix())
        .ok_or_else(|| RepositoryError::InvalidRequest {
            reason: format!("invalid volume record key '{key}'"),
        })?;
    if file_name.contains('/') {
        return Err(RepositoryError::InvalidRequest {
            reason: format!("invalid volume record key '{key}'"),
        });
    }
    let volume_id =
        file_name
            .strip_suffix(".json")
            .ok_or_else(|| RepositoryError::InvalidRequest {
                reason: format!("invalid volume record key '{key}'"),
            })?;
    validate_volume_id(volume_id)?;
    Ok(volume_id.to_string())
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn same_repo_blob_url(left: &str, right: &str) -> bool {
    !left.is_empty() && left.trim_end_matches('/') == right.trim_end_matches('/')
}

fn managed_memory_layer_from_remote_lower(
    index: usize,
    layer: LayerConfig,
    repo_blob_url: &str,
    managed_layers_repo_blob_url: &str,
) -> RepositoryResult<ManagedLayer> {
    if !same_repo_blob_url(repo_blob_url, managed_layers_repo_blob_url) {
        return Err(RepositoryError::Unsupported {
            feature: format!("memory layer {index} uses non-OSS managed repoBlobUrl"),
        });
    }
    let digest = if !layer.digest.is_empty() {
        layer.digest
    } else if !layer.target_digest.is_empty() {
        layer.target_digest
    } else {
        return Err(RepositoryError::Unsupported {
            feature: format!("memory layer {index} without digest"),
        });
    };
    Ok(ManagedLayer {
        digest,
        size: layer.size,
        uuid: None,
    })
}

fn fallback_to_object_storage_would_mix_sources(
    image_config_path: &Path,
    managed_layers_repo_blob_url: &str,
) -> RepositoryResult<bool> {
    let image_config = load_overlaybd_image_config(image_config_path).map_err(|e| {
        RepositoryError::backend(
            format!(
                "load overlaybd image config '{}'",
                image_config_path.display()
            ),
            e,
        )
    })?;
    Ok(image_config.lowers.iter().any(|layer| {
        layer.file.is_empty()
            && !same_repo_blob_url(
                layer.effective_repo_blob_url(&image_config.repo_blob_url),
                managed_layers_repo_blob_url,
            )
    }))
}

// ── SnapshotRepository impl ────────────────────────────────────────────

#[async_trait]
impl SnapshotRepository for OssSnapshotRepository {
    async fn storage_identity(&self) -> RepositoryResult<String> {
        Ok(self.client.storage_identity())
    }

    async fn create(&self, record: SnapshotRecord) -> RepositoryResult<SnapshotRecord> {
        if !matches!(record.source, SnapshotSource::Template { .. }) {
            return Err(RepositoryError::InvalidRequest {
                reason: "only template snapshots can be pre-created".to_string(),
            });
        }
        if record.committed.is_some() {
            return Err(RepositoryError::InvalidRequest {
                reason: "pre-created template snapshots must not already be committed".to_string(),
            });
        }
        if self.snapshot_exists(&record.id).await? {
            return Err(RepositoryError::InvalidRequest {
                reason: format!("snapshot '{}' already exists", record.id),
            });
        }
        if let Some(alias) = record.alias.as_ref() {
            if let Some(existing) = self.load_alias_target(alias.as_ref()).await? {
                if existing != record.id && self.snapshot_exists(&existing).await? {
                    return Err(RepositoryError::AliasConflict {
                        alias: alias.to_string(),
                        existing,
                        new_id: record.id.clone(),
                    });
                }
            }
        }
        self.write_record(&record).await?;
        if let Some(alias) = record.alias.as_ref() {
            if let Err(error) = self.bind_alias(alias.as_ref(), &record.id).await {
                let _ = self
                    .client
                    .delete(&OssSnapshotArtifactLayout::record_key(&record.id))
                    .await;
                return Err(error);
            }
        }
        Ok(record)
    }

    async fn publish(
        &self,
        metadata: SnapshotPublishMetadata,
        manifest: SandboxSnapshotManifest,
        recording: Option<crate::snapshot::StartupRecording>,
    ) -> RepositoryResult<SnapshotRecord> {
        Ok(self
            .publish_with_local_layers(metadata, manifest, recording)
            .await?
            .record)
    }

    async fn publish_with_local_layers(
        &self,
        metadata: SnapshotPublishMetadata,
        manifest: SandboxSnapshotManifest,
        recording: Option<crate::snapshot::StartupRecording>,
    ) -> RepositoryResult<SnapshotPublication> {
        let id = &metadata.id;
        let layout = self.layout(id);

        // 0. Validate no duplicate drive ids.
        let mut drive_ids_set = HashSet::new();
        for drive in &manifest.attached_drives {
            if !drive_ids_set.insert(drive.drive_id.clone()) {
                return Err(RepositoryError::InvalidRequest {
                    reason: format!(
                        "duplicate attached drive id in publish request: {}",
                        drive.drive_id
                    ),
                });
            }
            if drive.virtual_size == 0 {
                return Err(RepositoryError::InvalidRequest {
                    reason: format!(
                        "attached drive '{}' virtual_size must be non-zero",
                        drive.drive_id
                    ),
                });
            }
        }

        let mut disk_publications = Vec::new();
        let mut local_layers = Vec::new();

        let publish_result = async {
            validate_publish_manifest_image_configs(&manifest)?;

            // 1. Export rootfs disk image with the effective runtime config.
            let rootfs_config = SnapshotOciConfigInput::new(
                &metadata.context,
                metadata.image_configs.rootfs_config(),
            );
            let rootfs_outcome = self
                .export_disk_image(
                    id,
                    DiskImageSubject::Rootfs,
                    &manifest.rootfs.image_config_path,
                    Some(rootfs_config),
                )
                .await?;
            if let Some(publication) = rootfs_outcome.publication.clone() {
                disk_publications.push(publication);
            }
            local_layers.extend(rootfs_outcome.local_layers);
            let rootfs_layers = rootfs_outcome.layers;

            let memory_layers = self
                .derive_and_upload_memory_layers(&manifest.memory.image_config_path, &mut local_layers)
                .await?;

            // 2. Upload per-snapshot fixed artifacts.
            let vm_state_local_path = manifest.vm_state.path.as_path();
            self.client
                .put_file(
                    &layout.artifact_key(SNAPSHOT_ARTIFACT_LAYOUT.vm_state),
                    vm_state_local_path,
                    OssUploadArtifact::VmState,
                )
                .await
                .map_err(|e| {
                    RepositoryError::backend(
                        format!(
                            "upload artifact '{}' from '{}' for snapshot '{}'",
                            SNAPSHOT_ARTIFACT_LAYOUT.vm_state,
                            vm_state_local_path.display(),
                            id
                        ),
                        e,
                    )
                })?;

            let persisted_manifest_bytes = serde_json::to_vec_pretty(&manifest)
                .map_err(|e| RepositoryError::backend("serialize firecracker manifest", e))?;
            self.client
                .put_bytes(
                    &layout.artifact_key(SNAPSHOT_ARTIFACT_LAYOUT.firecracker_manifest),
                    persisted_manifest_bytes,
                    OssUploadArtifact::FirecrackerManifest,
                )
                .await
                .map_err(|e| RepositoryError::backend("write firecracker manifest to oss", e))?;

            // The startup manifest is fully decoupled from this flow: the
            // detached continuation spawned after the record commits builds
            // and uploads it, then attaches the descriptor to the record.
            let memory_startup = None;

            // 3. Export attached-drive disk images and derive their committed metadata.
            let attached_drives = self
                .export_attached_drives(id, &manifest, &mut disk_publications, &mut local_layers)
                .await?;

            // 4. Construct committed CommittedSnapshot.
            let committed = CommittedSnapshot {
                storage_checkpoint: manifest.storage_checkpoint.clone(),
                context: metadata.context.clone(),
                startup: metadata.startup.clone(),
                runtime_versions: metadata.runtime_versions.clone(),
                virtualization_mode: metadata.virtualization_mode,
                image_configs: metadata.image_configs.clone(),
                custom_extension_params: metadata.custom_extension_params.clone(),
                rootfs_layers,
                attached_drives,
                volume_snapshots: metadata.volume_snapshots.clone(),
                memory_layers,
                disk_publications: disk_publications.clone(),
                memory_startup,
            };

            // 5. Bind alias (if present) with conflict detection.
            if let Some(ref alias) = metadata.alias {
                if let Err(e) = self.bind_alias(alias.as_ref(), id).await {
                    // Best-effort rollback. Content-addressed managed layers are intentionally left
                    // in place; they are shared across snapshots and require separate GC.
                    if let Err(error) = self.client.delete_prefix(&layout.artifact_prefix()).await {
                        warn!(snapshot_id = %id, error = %error, "failed to roll back snapshot artifacts after alias bind failure");
                    }
                    return Err(e);
                }
            }

            self.write_committed_record(
                metadata.id.clone(),
                metadata.alias.clone(),
                metadata.resources,
                committed,
                metadata.source.clone(),
            )
            .await
        }
        .await;

        let record = match publish_result {
            Ok(record) => record,
            Err(error) => {
                // Best-effort rollback. Content-addressed managed layers are intentionally left
                // in place; they are shared across snapshots and require separate GC.
                if let Err(error) = self.client.delete_prefix(&layout.artifact_prefix()).await {
                    warn!(snapshot_id = %id, error = %error, "failed to roll back snapshot artifacts after publish failure");
                }
                for publication in disk_publications.iter().rev() {
                    if let Err(rollback_error) =
                        self.acr_exporter.rollback_publication(publication).await
                    {
                        warn!(
                            snapshot_id = %id,
                            image_ref = %publication.image_ref,
                            manifest_digest = %publication.manifest_digest,
                            error = %rollback_error,
                            "failed to roll back ACR snapshot publication; leaving cleanup to registry GC"
                        );
                    }
                }
                return Err(error);
            }
        };

        // Fully decoupled startup manifest: the continuation joins the
        // recording, builds and uploads the manifest, then attaches its
        // descriptor to the record. Publish never waits on any of it.
        if crate::cfg::ConfigManager::global_config()
            .snapshot
            .memory_startup_pack
            .enabled
        {
            if let Some(recording) = recording {
                if let Some(guard) =
                    crate::snapshot::startup_pack::StartupManifestTaskGuard::try_register()
                {
                    tokio::spawn(finish_startup_manifest(
                        self.client.clone(),
                        metadata.id.clone(),
                        recording,
                        guard,
                    ));
                } else {
                    recording.detach();
                }
            }
        }

        debug!(snapshot_id = %id, "published snapshot to oss");
        Ok(SnapshotPublication {
            record,
            local_layers,
        })
    }

    async fn get(&self, id_or_alias: &str) -> RepositoryResult<Option<SnapshotRecord>> {
        // Try by id first.
        if let Ok(direct_id) = crate::snapshot::SnapshotId::parse(id_or_alias) {
            if let Some(record) = self.read_record(&direct_id).await? {
                return Ok(Some(record));
            }
        }

        // Try by alias.
        let resolved_id = match self.resolve_alias(id_or_alias).await {
            Ok(id) => id,
            Err(error) => return Err(error),
        };
        let Some(resolved_id) = resolved_id else {
            return Ok(None);
        };
        self.read_record(&resolved_id).await
    }

    async fn list(&self, filter: SnapshotListFilter) -> RepositoryResult<Vec<SnapshotRecord>> {
        let keys = self
            .client
            .list_keys_recursive("catalog/records/")
            .await
            .map_err(|e| RepositoryError::backend("list snapshot records", e))?;

        let mut records: Vec<SnapshotRecord> = stream::iter(keys)
            .map(|key| async move {
                let bytes = self.client.get_bytes(&key).await.map_err(|e| {
                    RepositoryError::backend(format!("read snapshot record '{key}'"), e)
                })?;
                serde_json::from_slice::<SnapshotRecord>(&bytes).map_err(|e| {
                    RepositoryError::backend(format!("parse snapshot record '{key}'"), e)
                })
            })
            .buffer_unordered(16)
            .try_collect()
            .await?;

        records.retain(|record| Self::matches_record_filter(record, &filter));
        records.sort_by(|a, b| {
            b.created_at_unix_ms
                .cmp(&a.created_at_unix_ms)
                .then_with(|| a.id.to_string().cmp(&b.id.to_string()))
        });

        Ok(records)
    }

    async fn delete(&self, id_or_alias: &str) -> RepositoryResult<()> {
        // Resolve the actual id + metadata.
        let record = match self.get(id_or_alias).await? {
            Some(record) => record,
            None => {
                if let Ok(id) = SnapshotId::parse(id_or_alias) {
                    self.client
                        .delete(&OssSnapshotArtifactLayout::build_log_key(&id))
                        .await
                        .map_err(|error| RepositoryError::backend("delete build logs", error))?;
                }
                return Ok(());
            }
        };
        let id = &record.id;
        let layout = self.layout(id);

        // 1. Delete alias binding.
        if let Some(ref alias) = record.alias {
            if self.load_alias_target(alias.as_ref()).await?.as_ref() == Some(id) {
                if let Err(error) = self
                    .client
                    .delete(&OssSnapshotArtifactLayout::alias_key(alias.as_ref()))
                    .await
                {
                    warn!(snapshot_id = %id, alias = %alias, error = %error, "failed to delete oss alias during snapshot removal");
                }
            }
        }

        self.client
            .delete(&OssSnapshotArtifactLayout::build_log_key(id))
            .await
            .map_err(|error| RepositoryError::backend("delete build logs", error))?;

        // 2. Delete the catalog record.
        self.client
            .delete(&OssSnapshotArtifactLayout::record_key(id))
            .await
            .map_err(|e| RepositoryError::backend("delete snapshot record from oss", e))?;

        // 3. Delete external disk publications on a best-effort basis.
        if let Some(committed) = record.committed.as_ref() {
            self.delete_disk_publications(id, &committed.disk_publications)
                .await;
        }

        // 4. Delete artifacts.
        if let Err(error) = self.client.delete_prefix(&layout.artifact_prefix()).await {
            warn!(snapshot_id = %id, error = %error, "failed to delete oss snapshot artifacts");
        }

        debug!(snapshot_id = %id, "deleted snapshot from oss");
        Ok(())
    }

    async fn resolve_alias(&self, alias: &str) -> RepositoryResult<Option<SnapshotId>> {
        let key = validated_alias_key(alias)?;
        let data = match self.client.get_bytes(&key).await {
            Ok(d) => d,
            Err(e) if OssClient::is_not_found_error(&e) => return Ok(None),
            Err(e) => {
                return Err(RepositoryError::backend(format!("read alias '{alias}'"), e));
            }
        };

        let id: SnapshotId = serde_json::from_slice(&data)
            .map_err(|e| RepositoryError::backend(format!("parse alias '{alias}'"), e))?;

        // Stale-alias cleanup: if the snapshot record doesn't exist, delete the alias
        // and return None (mirrors PosixFs catalog.rs:236 behavior).
        let snapshot_exists = self.snapshot_exists(&id).await?;
        if !snapshot_exists {
            warn!(alias = %alias, snapshot_id = %id, "cleaning up stale alias pointing to missing snapshot");
            if let Err(error) = self.client.delete(&key).await {
                warn!(alias = %alias, snapshot_id = %id, error = %error, "failed to delete stale oss alias");
            }
            return Ok(None);
        }

        Ok(Some(id))
    }

    async fn try_start_build(&self, id: &SnapshotId) -> RepositoryResult<SnapshotRecord> {
        let mut record =
            self.read_record(id)
                .await?
                .ok_or_else(|| RepositoryError::SnapshotNotFound {
                    lookup: id.to_string(),
                })?;
        let now = now_unix_ms();
        let SnapshotSource::Template { build } = &mut record.source else {
            return Err(RepositoryError::InvalidRequest {
                reason: format!("snapshot '{id}' is not a template build"),
            });
        };
        if build.status != TemplateBuildStatus::Waiting {
            return Err(RepositoryError::InvalidRequest {
                reason: format!("template build '{id}' is not in waiting state"),
            });
        }
        build.status = TemplateBuildStatus::Building;
        build.started_at_unix_ms = Some(now);
        build.error_reason = None;
        record.updated_at_unix_ms = now;
        self.write_record(&record).await?;
        Ok(record)
    }

    async fn mark_build_error(
        &self,
        id: &SnapshotId,
        reason: TemplateBuildErrorReason,
    ) -> RepositoryResult<()> {
        let mut record =
            self.read_record(id)
                .await?
                .ok_or_else(|| RepositoryError::SnapshotNotFound {
                    lookup: id.to_string(),
                })?;
        let now = now_unix_ms();
        let SnapshotSource::Template { build } = &mut record.source else {
            return Err(RepositoryError::InvalidRequest {
                reason: format!("snapshot '{id}' is not a template build"),
            });
        };
        build.status = TemplateBuildStatus::Error;
        build.finished_at_unix_ms = Some(now);
        build.error_reason = Some(reason);
        record.updated_at_unix_ms = now;
        self.write_record(&record).await
    }

    async fn get_volume(&self, reference: &str) -> RepositoryResult<Option<VolumeRecord>> {
        aenv_volume_core::object::ObjectVolumeCatalog::new(self.client.clone())
            .get_volume(reference)
            .await
            .map_err(Into::into)
    }

    async fn list_volumes_page(
        &self,
        after_volume_id: Option<&str>,
        limit: usize,
    ) -> RepositoryResult<VolumeRecordPage> {
        aenv_volume_core::object::ObjectVolumeCatalog::new(self.client.clone())
            .list_volumes_page(after_volume_id, limit)
            .await
            .map_err(Into::into)
    }

    async fn create_volume(&self, record: VolumeRecord) -> RepositoryResult<()> {
        aenv_volume_core::object::ObjectVolumeCatalog::new(self.client.clone())
            .create_volume(record)
            .await
            .map_err(Into::into)
    }

    async fn put_volume(&self, record: VolumeRecord) -> RepositoryResult<()> {
        aenv_volume_core::object::ObjectVolumeCatalog::new(self.client.clone())
            .put_volume(record)
            .await
            .map_err(Into::into)
    }

    async fn write_build_logs(
        &self,
        id: &SnapshotId,
        entries: Vec<crate::template::logs::BuildLogEntry>,
    ) -> RepositoryResult<()> {
        let bytes = serde_json::to_vec(&entries)
            .map_err(|error| RepositoryError::backend("encode build logs", error))?;
        self.client
            .put_bytes(
                &OssSnapshotArtifactLayout::build_log_key(id),
                bytes,
                OssUploadArtifact::BuildLog,
            )
            .await
            .map_err(|error| RepositoryError::backend("write build logs", error))
    }

    async fn read_build_logs(
        &self,
        id: &SnapshotId,
    ) -> RepositoryResult<Vec<crate::template::logs::BuildLogEntry>> {
        let key = OssSnapshotArtifactLayout::build_log_key(id);
        let bytes = match self.client.get_bytes(&key).await {
            Ok(bytes) => bytes,
            Err(error) if OssClient::is_not_found_error(&error) => return Ok(Vec::new()),
            Err(error) => return Err(RepositoryError::backend("read build logs", error)),
        };
        serde_json::from_slice(&bytes)
            .map_err(|error| RepositoryError::backend("decode build logs", error))
    }

    async fn get_build_cache_state(&self) -> RepositoryResult<BuildCacheState> {
        match self
            .client
            .get_bytes("template-build/cache-head.json")
            .await
        {
            Ok(bytes) => BuildCacheState::decode(&bytes),
            Err(error) if OssClient::is_not_found_error(&error) => Ok(BuildCacheState::default()),
            Err(error) => Err(RepositoryError::backend("read build cache head", error)),
        }
    }

    async fn replace_build_cache_head(&self, volume_id: &str) -> RepositoryResult<Option<String>> {
        self.update_build_cache(|state| state.replace(volume_id))
            .await
    }

    async fn forget_retired_build_cache(&self, volume_id: &str) -> RepositoryResult<()> {
        self.update_build_cache(|state| {
            state.retired.remove(volume_id);
            Ok(())
        })
        .await
    }

    async fn publish_volume_backing(
        &self,
        _volume_id: &str,
        image_config_path: &std::path::Path,
    ) -> RepositoryResult<Vec<OverlaybdLayerRef>> {
        self.derive_and_upload_volume_layers(image_config_path)
            .await
    }

    async fn materialize_volume_backing(
        &self,
        _volume_id: &str,
        layers: &[OverlaybdLayerRef],
        destination: &std::path::Path,
    ) -> RepositoryResult<std::path::PathBuf> {
        let managed_url = self.client.managed_layers_repo_blob_url();
        materialize_volume_image_config(layers, destination, |layer| LayerConfig {
            repo_blob_url: managed_url.clone(),
            digest: layer.digest.clone(),
            size: layer.size,
            uuid: layer.uuid.clone().unwrap_or_default(),
            ..LayerConfig::default()
        })
        .await
    }

    async fn delete_volume(&self, volume_id: &str) -> RepositoryResult<()> {
        aenv_volume_core::object::ObjectVolumeCatalog::new(self.client.clone())
            .delete_volume(volume_id)
            .await
            .map_err(Into::into)
    }

    async fn reserve_volume(
        &self,
        volume_id: &str,
        owner: &str,
    ) -> RepositoryResult<Option<String>> {
        aenv_volume_core::object::ObjectVolumeCatalog::new(self.client.clone())
            .reserve_volume(volume_id, owner)
            .await
            .map_err(Into::into)
    }

    async fn reserve_read_only_volume(&self, volume_id: &str, owner: &str) -> RepositoryResult<()> {
        aenv_volume_core::object::ObjectVolumeCatalog::new(self.client.clone())
            .reserve_read_only_volume(volume_id, owner)
            .await
            .map_err(Into::into)
    }

    async fn replace_volume_owner_for(
        &self,
        volume_id: &str,
        from: &str,
        to: Option<&str>,
    ) -> RepositoryResult<()> {
        aenv_volume_core::object::ObjectVolumeCatalog::new(self.client.clone())
            .replace_volume_owner_for(volume_id, from, to)
            .await
            .map_err(Into::into)
    }
}

// ── private helpers ────────────────────────────────────────────────────

impl OssSnapshotRepository {
    async fn read_record(&self, id: &SnapshotId) -> RepositoryResult<Option<SnapshotRecord>> {
        let key = OssSnapshotArtifactLayout::record_key(id);
        match self.client.get_bytes(&key).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| RepositoryError::backend(format!("parse snapshot record '{id}'"), e)),
            Err(e) if OssClient::is_not_found_error(&e) => Ok(None),
            Err(e) => Err(RepositoryError::backend(
                format!("read snapshot record '{id}'"),
                e,
            )),
        }
    }

    async fn write_record(&self, record: &SnapshotRecord) -> RepositoryResult<()> {
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|e| RepositoryError::backend("serialize snapshot record", e))?;
        self.client
            .put_bytes(
                &OssSnapshotArtifactLayout::record_key(&record.id),
                bytes,
                OssUploadArtifact::CatalogRecord,
            )
            .await
            .map_err(|e| RepositoryError::backend("write snapshot record", e))
    }

    async fn write_committed_record(
        &self,
        id: SnapshotId,
        alias: Option<SnapshotAlias>,
        resources: crate::types::SandboxResources,
        committed: CommittedSnapshot,
        source: SnapshotPublishSource,
    ) -> RepositoryResult<SnapshotRecord> {
        let now = now_unix_ms();
        let record = if let Some(mut record) = self.read_record(&id).await? {
            record.mark_committed(alias, resources, committed, source, now);
            record
        } else {
            let source = match source {
                SnapshotPublishSource::Template => SnapshotSource::Template {
                    build: TemplateBuildInfo {
                        status: TemplateBuildStatus::Ready,
                        started_at_unix_ms: None,
                        finished_at_unix_ms: Some(now),
                        error_reason: None,
                    },
                },
                SnapshotPublishSource::Sandbox { source_sandbox_id } => {
                    SnapshotSource::Sandbox { source_sandbox_id }
                }
            };
            SnapshotRecord {
                id,
                alias,
                source,
                resources,
                created_at_unix_ms: now,
                updated_at_unix_ms: now,
                committed: Some(committed),
            }
        };
        self.write_record(&record).await?;
        Ok(record)
    }

    /// Bind an alias to a snapshot id with best-effort conflict detection.
    ///
    /// Alibaba Cloud OSS does not support conditional write headers
    /// (`If-None-Match`, `x-oss-forbid-overwrite`) on any S3-compatible
    /// write path, so we cannot use a true atomic `put_if_not_exists`.
    ///
    /// Instead the algorithm is:
    ///   1. Read the current alias target.
    ///   2. If it already points to `id`, return success (idempotent).
    ///   3. If it points to a live snapshot, return `AliasConflict`.
    ///   4. If it points to a deleted snapshot, remove the stale alias.
    ///   5. Write our binding unconditionally.
    ///   6. Read back and verify we won the race.  If someone else wrote a
    ///      different binding between steps 5 and 6, detect it here and
    ///      either retry or report a conflict.
    ///
    /// The read-back verification (step 6) narrows the race window to the
    /// interval between our write and the subsequent read.  This is weaker
    /// than a true CAS but sufficient for the current deployment model
    /// where concurrent publishes for the *same alias* are rare.
    async fn bind_alias(&self, alias: &str, id: &SnapshotId) -> RepositoryResult<()> {
        let key = validated_alias_key(alias)?;
        let payload = serde_json::to_vec(id)
            .map_err(|e| RepositoryError::backend("serialize alias binding", e))?;

        for _attempt in 0..MAX_ALIAS_BIND_ATTEMPTS {
            // Step 1-4: check current state and clean up stale bindings.
            if let Some(existing_id) = self.load_alias_target(alias).await? {
                if existing_id == *id {
                    return Ok(());
                }

                let still_exists = self.snapshot_exists(&existing_id).await?;
                if still_exists {
                    return Err(RepositoryError::AliasConflict {
                        alias: alias.to_string(),
                        existing: existing_id,
                        new_id: id.clone(),
                    });
                }

                self.client
                    .delete(&key)
                    .await
                    .map_err(|e| RepositoryError::backend("delete stale alias", e))?;
            }

            // Step 5: write our binding (unconditional — OSS does not
            // support conditional headers on S3-compatible writes).
            self.client
                .put_bytes(&key, payload.clone(), OssUploadArtifact::Alias)
                .await
                .map_err(|e| RepositoryError::backend("write alias binding", e))?;

            // Step 6: read back and verify we won.
            match self.load_alias_target(alias).await? {
                Some(bound_id) if bound_id == *id => return Ok(()),
                Some(existing_id) => {
                    // A concurrent writer overwrote our binding.
                    let still_exists = self.snapshot_exists(&existing_id).await?;
                    if still_exists {
                        return Err(RepositoryError::AliasConflict {
                            alias: alias.to_string(),
                            existing: existing_id,
                            new_id: id.clone(),
                        });
                    }

                    // The concurrent binding points to a deleted snapshot;
                    // clean it up and retry.
                    self.client
                        .delete(&key)
                        .await
                        .map_err(|e| RepositoryError::backend("delete stale alias", e))?;
                }
                None => {
                    warn!(
                        alias,
                        snapshot_id = %id,
                        "alias disappeared after bind attempt; retrying"
                    );
                    continue;
                }
            }
        }

        Err(RepositoryError::Backend {
            message: format!(
                "alias bind for '{alias}' exceeded {MAX_ALIAS_BIND_ATTEMPTS} attempts"
            ),
            source: None,
        })
    }

    async fn export_managed_disk_image(
        &self,
        image_config_path: &Path,
        artifact: OssUploadArtifact,
    ) -> RepositoryResult<DiskImageExportOutcome> {
        let mut local_layers = Vec::new();
        let layers = self
            .derive_and_upload_disk_image_layers_mode(
                image_config_path,
                artifact,
                false,
                &mut local_layers,
            )
            .await?;
        Ok(DiskImageExportOutcome {
            layers,
            publication: None,
            local_layers,
        })
    }

    #[cfg(test)]
    async fn derive_and_upload_disk_image_layers(
        &self,
        image_config_path: &Path,
        artifact: OssUploadArtifact,
    ) -> RepositoryResult<Vec<OverlaybdLayerRef>> {
        self.derive_and_upload_disk_image_layers_mode(
            image_config_path,
            artifact,
            false,
            &mut Vec::new(),
        )
        .await
    }

    async fn derive_and_upload_volume_layers(
        &self,
        image_config_path: &Path,
    ) -> RepositoryResult<Vec<OverlaybdLayerRef>> {
        self.derive_and_upload_disk_image_layers_mode(
            image_config_path,
            OssUploadArtifact::RootfsLayer,
            true,
            &mut Vec::new(),
        )
        .await
    }

    async fn derive_and_upload_disk_image_layers_mode(
        &self,
        image_config_path: &Path,
        artifact: OssUploadArtifact,
        allow_descriptorless: bool,
        local_layers: &mut Vec<PreparedLayerUpload>,
    ) -> RepositoryResult<Vec<OverlaybdLayerRef>> {
        let transport = NativeLayerTransport {
            client: &self.client,
            artifact,
        };
        aenv_volume_core::object_layers::ObjectLayerPublisher::new(
            &transport,
            self.publish_compression.into(),
        )
        .derive_and_upload_disk_image_layers_mode(
            image_config_path,
            allow_descriptorless,
            crate::image::local_layer::rootfs_layer_is_runtime_generated_delta,
            local_layers,
        )
        .await
        .map_err(Into::into)
    }

    async fn derive_and_upload_memory_layers(
        &self,
        mem_image_config_path: &Path,
        local_layers: &mut Vec<PreparedLayerUpload>,
    ) -> RepositoryResult<Vec<ManagedLayer>> {
        let image_config = load_overlaybd_image_config(mem_image_config_path).map_err(|e| {
            RepositoryError::backend(
                format!(
                    "load mem image config '{}'",
                    mem_image_config_path.display()
                ),
                e,
            )
        })?;

        let mut layers = Vec::with_capacity(image_config.lowers.len());
        for (index, layer) in image_config.lowers.into_iter().enumerate() {
            if layer.file.is_empty() {
                let repo_blob_url = layer
                    .effective_repo_blob_url(&image_config.repo_blob_url)
                    .to_string();
                if !repo_blob_url.is_empty() {
                    layers.push(managed_memory_layer_from_remote_lower(
                        index,
                        layer,
                        &repo_blob_url,
                        &self.client.managed_layers_repo_blob_url(),
                    )?);
                    continue;
                }
                return Err(RepositoryError::Unsupported {
                    feature: format!("memory layer {index} without local file path"),
                });
            }
            let layer_path = Path::new(&layer.file);
            if !layer.digest.is_empty() && layer.size > 0 {
                layers.push(
                    self.import_managed_layer_with_descriptor(
                        layer_path,
                        &layer.digest,
                        layer.size,
                        OssUploadArtifact::MemoryLayer,
                        local_layers,
                    )
                    .await?,
                );
                continue;
            }
            layers.push(
                self.import_managed_layer_by_hash(
                    layer_path,
                    OssUploadArtifact::MemoryLayer,
                    local_layers,
                )
                .await?,
            );
        }

        Ok(layers)
    }

    async fn export_disk_image(
        &self,
        snapshot_id: &SnapshotId,
        subject: DiskImageSubject,
        image_config_path: &Path,
        config: Option<SnapshotOciConfigInput<'_>>,
    ) -> RepositoryResult<DiskImageExportOutcome> {
        let artifact = match &subject {
            DiskImageSubject::Rootfs => OssUploadArtifact::RootfsLayer,
            DiskImageSubject::AttachedDrive { .. } => OssUploadArtifact::AttachedDriveLayer,
        };
        if !matches!(
            self.snapshot_image_storage,
            SnapshotImageStoragePolicy::SourceRegistry
        ) {
            return self
                .export_managed_disk_image(image_config_path, artifact)
                .await;
        }
        match self
            .acr_exporter
            .export(snapshot_id, subject.clone(), image_config_path, config)
            .await
        {
            Err(RepositoryError::Unsupported { feature }) => {
                if fallback_to_object_storage_would_mix_sources(
                    image_config_path,
                    &self.client.managed_layers_repo_blob_url(),
                )? {
                    return Err(RepositoryError::Unsupported {
                        feature: format!(
                            "source-registry export is unsupported for this remote-backed disk image: {feature}"
                        ),
                    });
                }
                info!(
                    snapshot_id = %snapshot_id,
                    subject = subject.log_label(),
                    reason = %feature,
                    "falling back to managed disk image layers"
                );
                self.export_managed_disk_image(image_config_path, artifact)
                    .await
            }
            result => result,
        }
    }

    async fn delete_disk_publications(
        &self,
        snapshot_id: &SnapshotId,
        publications: &[PersistedDiskImagePublication],
    ) {
        for publication in publications.iter().rev() {
            if let Err(error) = self.acr_exporter.rollback_publication(publication).await {
                warn!(
                    snapshot_id = %snapshot_id,
                    image_ref = %publication.image_ref,
                    manifest_digest = %publication.manifest_digest,
                    error = %error,
                    "failed to delete ACR publication during snapshot removal; continuing OSS cleanup"
                );
            }
        }
    }

    /// Export attached-drive disk images and derive committed metadata.
    async fn export_attached_drives(
        &self,
        snapshot_id: &SnapshotId,
        manifest: &crate::sandbox::SandboxSnapshotManifest,
        publications: &mut Vec<PersistedDiskImagePublication>,
        local_layers: &mut Vec<PreparedLayerUpload>,
    ) -> RepositoryResult<Vec<CommittedAttachedDrive>> {
        let mut drives = Vec::new();

        for drive in &manifest.attached_drives {
            let outcome = self
                .export_disk_image(
                    snapshot_id,
                    DiskImageSubject::AttachedDrive {
                        drive_id: drive.drive_id.clone(),
                    },
                    &drive.image_config_path,
                    None,
                )
                .await?;
            if let Some(publication) = outcome.publication.clone() {
                publications.push(publication);
            }
            local_layers.extend(outcome.local_layers);
            drives.push(CommittedAttachedDrive::Overlaybd {
                drive_id: drive.drive_id.clone(),
                layers: outcome.layers,
                read_only: drive.read_only,
                virtual_size: drive.virtual_size,
                mount_path: crate::sandbox::normalize_mount_path_for_drive(
                    &drive.drive_id,
                    drive.mount_path.clone(),
                )
                .unwrap_or_else(|_| {
                    crate::sandbox::ExtraDrive::default_mount_path(&drive.drive_id)
                }),
                sub_path: drive.sub_path.clone(),
            });
        }

        Ok(drives)
    }

    async fn load_alias_target(&self, alias: &str) -> RepositoryResult<Option<SnapshotId>> {
        let key = validated_alias_key(alias)?;
        let data = match self.client.get_bytes(&key).await {
            Ok(data) => data,
            Err(e) if OssClient::is_not_found_error(&e) => return Ok(None),
            Err(e) => {
                return Err(RepositoryError::backend(
                    format!("read alias target '{alias}'"),
                    e,
                ));
            }
        };

        let target = serde_json::from_slice::<SnapshotId>(&data)
            .map_err(|e| RepositoryError::backend(format!("parse alias target '{alias}'"), e))?;
        Ok(Some(target))
    }

    fn matches_record_filter(record: &SnapshotRecord, filter: &SnapshotListFilter) -> bool {
        if let Some(alias_prefix) = filter.alias_prefix.as_deref() {
            match record.alias.as_ref() {
                Some(alias) if alias.to_string().starts_with(alias_prefix) => {}
                _ => return false,
            }
        }

        if let Some(ids) = filter.snapshot_ids.as_ref() {
            if !ids.iter().any(|id| id == &record.id) {
                return false;
            }
        }

        if let Some(id_or_alias) = filter.snapshot_id_or_alias.as_deref() {
            if record.id.to_string() != id_or_alias
                && record
                    .alias
                    .as_ref()
                    .is_none_or(|alias| alias.as_ref() != id_or_alias)
            {
                return false;
            }
        }

        if let Some(source_sandbox_id) = filter.source_sandbox_id.as_deref() {
            match &record.source {
                SnapshotSource::Sandbox {
                    source_sandbox_id: record_source_sandbox_id,
                } if record_source_sandbox_id == source_sandbox_id => {}
                _ => return false,
            }
        }

        if let Some(sources) = filter.sources.as_ref() {
            let source = match &record.source {
                SnapshotSource::Template { .. } => SnapshotSourceKind::Template,
                SnapshotSource::Sandbox { .. } => SnapshotSourceKind::Sandbox,
            };
            if !sources.contains(&source) {
                return false;
            }
        }

        if let Some(statuses) = filter.template_statuses.as_ref() {
            let SnapshotSource::Template { build } = &record.source else {
                return false;
            };
            if !statuses.contains(&build.status) {
                return false;
            };
        }

        true
    }
}

impl OssSnapshotRepository {
    async fn import_managed_layer_by_hash(
        &self,
        source: &Path,
        artifact: OssUploadArtifact,
        local_layers: &mut Vec<PreparedLayerUpload>,
    ) -> RepositoryResult<ManagedLayer> {
        let transport = NativeLayerTransport {
            client: &self.client,
            artifact,
        };
        aenv_volume_core::object_layers::ObjectLayerPublisher::new(
            &transport,
            self.publish_compression.into(),
        )
        .import_managed_layer_by_hash(source, local_layers)
        .await
        .map_err(Into::into)
    }

    async fn import_managed_layer_with_descriptor(
        &self,
        source: &Path,
        digest: &str,
        size: u64,
        artifact: OssUploadArtifact,
        local_layers: &mut Vec<PreparedLayerUpload>,
    ) -> RepositoryResult<ManagedLayer> {
        let transport = NativeLayerTransport {
            client: &self.client,
            artifact,
        };
        aenv_volume_core::object_layers::ObjectLayerPublisher::new(
            &transport,
            self.publish_compression.into(),
        )
        .import_managed_layer_with_descriptor(source, digest, size, local_layers)
        .await
        .map_err(Into::into)
    }
}

/// Upload a content-addressed managed layer if it is not already present.
///
/// Managed layers are keyed by `sha256:{digest}`, so concurrent writers
/// uploading the same digest always produce identical content.  This makes
/// the `exists() → put()` TOCTOU benign: the worst case is a redundant
/// upload of identical bytes, never data corruption.
///
/// We intentionally use an unconditional `put_file` instead of the
/// conditional `put_file_if_not_exists` here because Alibaba Cloud OSS
/// does not support conditional headers (`x-oss-forbid-overwrite`) on
/// multipart/streaming uploads — only on single-PUT operations.  OpenDAL's
/// `writer_with().if_not_exists(true)` triggers the multipart path for
/// large files, which causes a `NotImplemented` error on OSS.
struct NativeLayerTransport<'a> {
    client: &'a OssClient,
    artifact: OssUploadArtifact,
}
#[async_trait]
impl aenv_volume_core::object_layers::ObjectLayerTransport for NativeLayerTransport<'_> {
    async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        self.client.exists(key).await
    }
    async fn put_file(&self, key: &str, path: &Path) -> anyhow::Result<()> {
        self.client.put_file(key, path, self.artifact).await
    }
}

fn validate_publish_manifest_image_configs(
    manifest: &SandboxSnapshotManifest,
) -> RepositoryResult<()> {
    load_overlaybd_image_config(&manifest.rootfs.image_config_path).map_err(|e| {
        RepositoryError::backend(
            format!(
                "validate rootfs image config '{}'",
                manifest.rootfs.image_config_path.display()
            ),
            e,
        )
    })?;
    load_overlaybd_image_config(&manifest.memory.image_config_path).map_err(|e| {
        RepositoryError::backend(
            format!(
                "validate memory image config '{}'",
                manifest.memory.image_config_path.display()
            ),
            e,
        )
    })?;
    for drive in &manifest.attached_drives {
        load_overlaybd_image_config(&drive.image_config_path).map_err(|e| {
            RepositoryError::backend(
                format!(
                    "validate drive image config '{}' for drive '{}'",
                    drive.image_config_path.display(),
                    drive.drive_id
                ),
                e,
            )
        })?;
    }
    Ok(())
}

// ── Detached startup-manifest continuation ─────────────────────────────────

/// Detached startup-manifest continuation: joins the recording, builds and
/// uploads the manifest, then attaches its descriptor to the already
/// committed record. Best-effort throughout — any failure only means the
/// snapshot resumes on-demand.
async fn finish_startup_manifest(
    client: std::sync::Arc<OssClient>,
    id: SnapshotId,
    recording: crate::snapshot::StartupRecording,
    guard: crate::snapshot::startup_pack::StartupManifestTaskGuard,
) {
    finish_startup_manifest_with_abort(
        client,
        id,
        recording,
        guard,
        crate::snapshot::startup_pack::startup_manifest_abort_requested,
    )
    .await;
}

async fn finish_startup_manifest_with_abort(
    client: std::sync::Arc<OssClient>,
    id: SnapshotId,
    recording: crate::snapshot::StartupRecording,
    _guard: crate::snapshot::startup_pack::StartupManifestTaskGuard,
    abort_requested: impl Fn() -> bool + Send,
) {
    let crate::snapshot::StartupRecording {
        mut trace,
        keep_alive,
    } = recording;
    // Hold the captured artifacts alive until the manifest is uploaded, and
    // count this continuation for the shutdown drain.
    let _keep_alive = keep_alive;
    if abort_requested() {
        // The recorder may still be using captured VM/device artifacts.
        let _ = trace.await;
        return;
    }
    let trace_path = match tokio::select! {
        joined = &mut trace => joined,
        _ = crate::snapshot::startup_pack::startup_manifest_abort_notify() => {
            let _ = trace.await;
            return;
        },
    } {
        Ok(Some(path)) => path,
        Ok(None) => {
            debug!(snapshot_id = %id, "startup manifest recording produced no trace");
            return;
        }
        Err(error) => {
            warn!(%error, snapshot_id = %id, "startup manifest recording join failed");
            return;
        }
    };
    if abort_requested() {
        return;
    }
    let layout = OssSnapshotArtifactLayout::new(&id);
    let Some(info) = build_and_upload_manifest(&client, &layout, &id, &trace_path).await else {
        return;
    };
    if abort_requested() {
        return;
    }
    attach_memory_startup_descriptor(&client, &id, info).await;
}

/// Best-effort: build the v4 startup manifest (exact-order prefix pages
/// plus merged ranges) from the recorded first-touch trace and upload it.
/// No layer bytes are read, packed, or uploaded — the resume side prefetches
/// the listed positions through the normal read path.
async fn build_and_upload_manifest(
    client: &OssClient,
    layout: &OssSnapshotArtifactLayout<'_>,
    id: &SnapshotId,
    trace_path: &std::path::Path,
) -> Option<MemoryStartupPackInfo> {
    let pack_config = &crate::cfg::ConfigManager::global_config()
        .snapshot
        .memory_startup_pack;
    let build = async {
        let (info, manifest_bytes) =
            match crate::snapshot::startup_pack::build_startup_manifest(trace_path).await {
                Ok(built) => built,
                Err(error) => {
                    warn!(%error, snapshot_id = %id, "startup manifest build failed (best-effort)");
                    return None;
                }
            };
        info!(snapshot_id = %id, manifest_bytes = manifest_bytes.len(), "startup manifest built");
        if let Err(error) = client
            .put_bytes(
                &layout.artifact_key(MEMORY_STARTUP_PACK_ARTIFACT),
                manifest_bytes,
                OssUploadArtifact::StartupPack,
            )
            .await
        {
            warn!(
                %error,
                snapshot_id = %id,
                "upload startup manifest failed (best-effort)"
            );
            return None;
        }
        Some(info)
    }
    .await;

    if build.is_none() && pack_config.enabled {
        delete_stale_startup_pack(client, layout, id).await;
    }
    build
}

/// Attach the uploaded manifest's descriptor to the committed record via a
/// read-modify-write pass. Alibaba OSS has no conditional-write CAS on the
/// S3-compatible path (see `bind_alias`), so a delete racing this update can
/// resurrect the record — accepted with the same documented model as alias
/// binding. A missing record (deleted meanwhile) aborts and drops the
/// manifest instead of resurrecting anything.
async fn attach_memory_startup_descriptor(
    client: &OssClient,
    id: &SnapshotId,
    info: MemoryStartupPackInfo,
) {
    let layout = OssSnapshotArtifactLayout::new(id);
    let cleanup_manifest = || async {
        if let Err(error) = client
            .delete(&layout.artifact_key(MEMORY_STARTUP_PACK_ARTIFACT))
            .await
        {
            debug!(%error, snapshot_id = %id, "delete manifest after aborted attach failed");
        }
    };
    let record_key = OssSnapshotArtifactLayout::record_key(id);
    let bytes = match client.get_bytes(&record_key).await {
        Ok(bytes) => bytes,
        Err(error) if OssClient::is_not_found_error(&error) => {
            debug!(
                snapshot_id = %id,
                "record deleted while attaching startup manifest; dropping the manifest"
            );
            cleanup_manifest().await;
            return;
        }
        Err(error) => {
            warn!(%error, snapshot_id = %id, "read record for startup manifest attach failed");
            return;
        }
    };
    let mut record: SnapshotRecord = match serde_json::from_slice(&bytes) {
        Ok(record) => record,
        Err(error) => {
            warn!(%error, snapshot_id = %id, "parse record for startup manifest attach failed");
            return;
        }
    };
    let Some(committed) = record.committed.as_mut() else {
        cleanup_manifest().await;
        return;
    };
    if committed.memory_startup.is_some() {
        return;
    }
    committed.memory_startup = Some(info);
    record.updated_at_unix_ms = now_unix_ms();
    let bytes = match serde_json::to_vec_pretty(&record) {
        Ok(bytes) => bytes,
        Err(error) => {
            warn!(%error, snapshot_id = %id, "serialize record for startup manifest attach failed");
            return;
        }
    };
    if let Err(error) = client
        .put_bytes(&record_key, bytes, OssUploadArtifact::CatalogRecord)
        .await
    {
        warn!(
            %error,
            snapshot_id = %id,
            "attach startup manifest descriptor failed (best-effort)"
        );
    }
}

/// Clean a possible stale startup artifact when the feature expected one
/// (a publish retry after a crash with recording enabled); a manifest-less
/// publish with the feature off must not pay an extra OSS request. A None
/// `memory_startup` descriptor already prevents consumption either way.
async fn delete_stale_startup_pack(
    client: &OssClient,
    layout: &OssSnapshotArtifactLayout<'_>,
    id: &SnapshotId,
) {
    if let Err(error) = client
        .delete(&layout.artifact_key(MEMORY_STARTUP_PACK_ARTIFACT))
        .await
    {
        debug!(
            %error,
            snapshot_id = %id,
            "delete stale startup pack failed (best-effort)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store_operator::CredentialSource;
    use overlaybd::config::ImageConfig;
    use serde_json::json;

    fn write_test_image(path: &Path, value: serde_json::Value) {
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&value).expect("serialize image config"),
        )
        .expect("write image config");
    }

    fn test_repository() -> OssSnapshotRepository {
        let client = OssClient::new(
            "bucket".to_string(),
            "https://oss.example.com".to_string(),
            "region".to_string(),
            "prefix".to_string(),
            CredentialSource::Anonymous,
            None,
        )
        .expect("oss client");
        OssSnapshotRepository::new(
            Arc::new(client),
            SnapshotImageStoragePolicy::ObjectStorage,
            &SnapshotPublishCompressionConfig {
                enabled: false,
                ..Default::default()
            },
        )
    }

    #[tokio::test]
    async fn abort_before_oss_continuation_keeps_capture_lease_until_trace_finishes() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct LeaseProbe(Arc<AtomicBool>);
        impl Drop for LeaseProbe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let lease_dropped = Arc::new(AtomicBool::new(false));
        let trace_finished = Arc::new(AtomicBool::new(false));
        let (release_trace, trace_release) = tokio::sync::oneshot::channel::<()>();
        let trace_finished_in_task = Arc::clone(&trace_finished);
        let recording = crate::snapshot::StartupRecording {
            trace: tokio::spawn(async move {
                trace_release.await.expect("release trace");
                trace_finished_in_task.store(true, Ordering::SeqCst);
                None
            }),
            keep_alive: Box::new(LeaseProbe(Arc::clone(&lease_dropped))),
        };
        let guard = crate::snapshot::startup_pack::StartupManifestTaskGuard::try_register()
            .expect("register continuation");
        let repository = test_repository();
        let mut continuation = tokio::spawn(finish_startup_manifest_with_abort(
            Arc::clone(&repository.client),
            SnapshotId::generate(),
            recording,
            guard,
            || true,
        ));

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut continuation)
                .await
                .is_err(),
            "aborted continuation must wait for the pending recorder"
        );
        assert!(!trace_finished.load(Ordering::SeqCst));
        assert!(!lease_dropped.load(Ordering::SeqCst));

        release_trace.send(()).expect("release pending trace");
        continuation.await.expect("continuation");
        assert!(trace_finished.load(Ordering::SeqCst));
        assert!(lease_dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn publish_compression_config_resolves_into_compact_output() {
        let client = || {
            Arc::new(
                OssClient::new(
                    "bucket".to_string(),
                    "https://oss.example.com".to_string(),
                    "region".to_string(),
                    "prefix".to_string(),
                    CredentialSource::Anonymous,
                    None,
                )
                .expect("oss client"),
            )
        };

        let disabled = OssSnapshotRepository::new(
            client(),
            SnapshotImageStoragePolicy::ObjectStorage,
            &SnapshotPublishCompressionConfig {
                enabled: false,
                ..Default::default()
            },
        );
        assert_eq!(disabled.publish_compression, OverlaybdCompactOutput::Raw);

        let enabled = OssSnapshotRepository::new(
            client(),
            SnapshotImageStoragePolicy::ObjectStorage,
            &SnapshotPublishCompressionConfig {
                enabled: true,
                algorithm: crate::cfg::OverlaybdCompressionAlgorithm::Zstd,
                workers: 4,
            },
        );
        assert_eq!(
            enabled.publish_compression,
            OverlaybdCompactOutput::ZFile {
                algorithm: crate::cfg::OverlaybdCompressionAlgorithm::Zstd,
                workers: 4,
            }
        );
    }

    #[test]
    fn volume_keys_use_separate_flat_namespaces() {
        let key = OssSnapshotArtifactLayout::volume_record_key("vol_test");
        assert_eq!(key, "volumes/records/vol_test.json");
        assert_eq!(
            OssSnapshotArtifactLayout::volume_alias_key("test-data"),
            "volumes/aliases/test-data.json"
        );
        assert_eq!(volume_id_from_record_key(&key).unwrap(), "vol_test");
        assert!(volume_id_from_record_key("volumes/records/ab/vol_test.json").is_err());
    }

    #[test]
    fn publish_manifest_preflight_rejects_invalid_memory_image_config() {
        let temp = tempfile::tempdir().expect("tempdir");
        let rootfs_image_config = temp.path().join("rootfs-image.json");
        let memory_image_config = temp.path().join("mem-image.json");

        write_test_image(
            &rootfs_image_config,
            json!({
                "lowers": [
                    { "file": "rootfs.commit" }
                ],
                "upper": {},
                "resultFile": "",
                "download": {}
            }),
        );
        write_test_image(
            &memory_image_config,
            json!({
                "lowers": [
                    { "digest": "sha256:parent", "size": 4096 }
                ],
                "upper": {},
                "resultFile": "",
                "download": {}
            }),
        );

        let mut manifest = SandboxSnapshotManifest::for_test(1024, &[]);
        manifest.rootfs.image_config_path = rootfs_image_config;
        manifest.memory.image_config_path = memory_image_config;

        let err = validate_publish_manifest_image_configs(&manifest)
            .expect_err("missing memory repoBlobUrl should fail preflight");
        assert!(err.to_string().contains("validate memory image config"));
    }

    #[test]
    fn detects_source_registry_fallback_source_mixing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source_registry_image = temp.path().join("source-image.json");
        write_test_image(
            &source_registry_image,
            json!({
                "repoBlobUrl": "https://registry.example/v2/ns/image/blobs",
                "lowers": [
                    { "digest": "sha256:base", "size": 4096 },
                    { "file": "snapshot.commit", "digest": "sha256:delta", "size": 5 }
                ],
                "upper": {},
                "resultFile": "",
                "download": {}
            }),
        );
        assert!(fallback_to_object_storage_would_mix_sources(
            &source_registry_image,
            "s3://bucket/prefix/managed-layers"
        )
        .unwrap());

        let oss_image = temp.path().join("oss-image.json");
        write_test_image(
            &oss_image,
            json!({
                "repoBlobUrl": "s3://bucket/prefix/managed-layers",
                "lowers": [
                    { "digest": "sha256:base", "size": 4096 },
                    { "file": "snapshot.commit", "digest": "sha256:delta", "size": 5 }
                ],
                "upper": {},
                "resultFile": "",
                "download": {}
            }),
        );

        assert!(!fallback_to_object_storage_would_mix_sources(
            &oss_image,
            "s3://bucket/prefix/managed-layers"
        )
        .unwrap());

        let layer_level_image = temp.path().join("layer-level-image.json");
        write_test_image(
            &layer_level_image,
            json!({
                "repoBlobUrl": "",
                "lowers": [
                    {
                        "digest": "sha256:base",
                        "size": 4096,
                        "repoBlobUrl": "https://registry.example/v2/ns/image/blobs"
                    },
                    { "file": "snapshot.commit", "digest": "sha256:delta", "size": 5 }
                ],
                "upper": {},
                "resultFile": "",
                "download": {}
            }),
        );
        assert!(fallback_to_object_storage_would_mix_sources(
            &layer_level_image,
            "s3://bucket/prefix/managed-layers"
        )
        .unwrap());
    }

    #[tokio::test]
    async fn derives_external_layers_from_layer_repo_blob_urls() {
        let temp = tempfile::tempdir().expect("tempdir");
        let image = temp.path().join("image.json");
        write_test_image(
            &image,
            json!({
                "repoBlobUrl": "",
                "lowers": [
                    {
                        "digest": "sha256:base",
                        "size": 4096,
                        "repoBlobUrl": "https://registry.example/v2/ns/image/blobs"
                    },
                    {
                        "digest": "sha256:delta",
                        "size": 8192,
                        "repoBlobUrl": "s3://bucket/prefix/managed-layers"
                    }
                ],
                "upper": {},
                "resultFile": "",
                "download": {}
            }),
        );

        let layers = test_repository()
            .derive_and_upload_disk_image_layers(&image, OssUploadArtifact::RootfsLayer)
            .await
            .expect("derive layers");

        assert_eq!(
            layers,
            vec![
                OverlaybdLayerRef::External(ExternalLayer {
                    digest: "sha256:base".to_string(),
                    repo_blob_url: "https://registry.example/v2/ns/image/blobs".to_string(),
                    size: 4096,
                }),
                OverlaybdLayerRef::External(ExternalLayer {
                    digest: "sha256:delta".to_string(),
                    repo_blob_url: "s3://bucket/prefix/managed-layers".to_string(),
                    size: 8192,
                }),
            ]
        );
    }

    #[tokio::test]
    async fn materializes_volume_backing_with_backend_layer_urls() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repository = test_repository();
        let destination = temp.path().join("volume/image.json");
        let layers = vec![
            OverlaybdLayerRef::Managed(ManagedLayer {
                digest: "sha256:managed".to_string(),
                size: 12,
                uuid: None,
            }),
            OverlaybdLayerRef::External(ExternalLayer {
                digest: "sha256:external".to_string(),
                repo_blob_url: "https://registry.example/v2/data/blobs".to_string(),
                size: 24,
            }),
        ];

        repository
            .materialize_volume_backing("vol_test", &layers, &destination)
            .await
            .expect("materialize volume config");
        let config: ImageConfig =
            serde_json::from_slice(&tokio::fs::read(&destination).await.unwrap()).unwrap();
        assert_eq!(config.lowers.len(), 2);
        assert_eq!(
            config.lowers[0].repo_blob_url,
            "s3://bucket/prefix/managed-layers"
        );
        assert_eq!(
            config.lowers[1].repo_blob_url,
            "https://registry.example/v2/data/blobs"
        );
        assert!(config.lowers.iter().all(|layer| layer.file.is_empty()));
    }
}
