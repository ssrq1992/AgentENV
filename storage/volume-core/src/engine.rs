//! Native volume operations shared by the API facade and managed storage host.
use crate::catalog::{CatalogError as RepositoryError, CatalogResult};
use crate::{
    is_valid_volume_component, OverlaybdLayerRef, VolumeError, VolumeMode, VolumePage,
    VolumeRecord, VolumeRecordPage, VolumeStatus, MAX_VOLUME_MOUNTS,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;
#[async_trait::async_trait]
pub trait VolumeRepository: Send + Sync {
    async fn get_volume(&self, reference: &str) -> CatalogResult<Option<VolumeRecord>>;
    async fn list_volumes_page(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> CatalogResult<VolumeRecordPage>;
    async fn create_volume(&self, record: VolumeRecord) -> CatalogResult<()>;
    async fn put_volume(&self, record: VolumeRecord) -> CatalogResult<()>;
    /// Dedicated maintenance CAS. Legacy repository implementations remain
    /// usable, but must explicitly implement this before supporting expansion.
    async fn commit_expansion(
        &self,
        _id: &str,
        _owner: &str,
        _expected_size_mb: u64,
        _expected_layers: &[OverlaybdLayerRef],
        _size_mb: u64,
        _layers: &[OverlaybdLayerRef],
    ) -> CatalogResult<()> {
        Err(RepositoryError::InvalidRequest {
            reason: "repository does not support maintenance publication".into(),
        })
    }
    async fn delete_volume(&self, id: &str) -> CatalogResult<()>;
    async fn reserve_volume(&self, id: &str, owner: &str) -> CatalogResult<Option<String>>;
    async fn reserve_read_only_volume(&self, id: &str, owner: &str) -> CatalogResult<()>;
    async fn replace_volume_owner_for(
        &self,
        id: &str,
        from: &str,
        to: Option<&str>,
    ) -> CatalogResult<()>;
    async fn publish_volume_backing(
        &self,
        id: &str,
        path: &Path,
    ) -> CatalogResult<Vec<OverlaybdLayerRef>>;
    async fn materialize_volume_backing(
        &self,
        id: &str,
        layers: &[OverlaybdLayerRef],
        destination: &Path,
    ) -> CatalogResult<PathBuf>;
}
#[derive(Clone, Copy, Debug)]
pub struct VolumeLimits {
    pub max_size_mb: u64,
    pub max_mounts: usize,
}
#[derive(Clone)]
pub struct VolumeEngine {
    records: Arc<RwLock<HashMap<String, VolumeRecord>>>,
    root: PathBuf,
    repository: Arc<dyn VolumeRepository>,
    limits: VolumeLimits,
}
impl VolumeEngine {
    pub fn new(
        root: PathBuf,
        repository: Arc<dyn VolumeRepository>,
        mut limits: VolumeLimits,
    ) -> Self {
        limits.max_mounts = limits.max_mounts.min(MAX_VOLUME_MOUNTS);
        Self {
            records: Arc::new(RwLock::new(HashMap::new())),
            root,
            repository,
            limits,
        }
    }
    pub async fn get(&self, reference: &str) -> Result<VolumeRecord, VolumeError> {
        let mut record = self
            .repository
            .get_volume(reference)
            .await
            .map_err(repository_error)?
            .ok_or_else(|| VolumeError::NotFound(reference.to_owned()))?;
        validate_volume_id(&record.id)?;
        let local = self.records.read().await;
        record.backing_image_config = current_local_backing(&record, &local);
        Ok(record)
    }
    pub async fn list_page(
        &self,
        next_token: Option<&str>,
        limit: usize,
    ) -> Result<VolumePage, VolumeError> {
        if limit == 0 {
            return Err(VolumeError::InvalidPageLimit);
        }
        let after_volume_id = next_token.map(decode_volume_cursor).transpose()?;
        let mut page = self
            .repository
            .list_volumes_page(after_volume_id.as_deref(), limit)
            .await
            .map_err(repository_error)?;
        let local = self.records.read().await;
        for record in &mut page.records {
            validate_volume_id(&record.id)?;
            record.backing_image_config = current_local_backing(record, &local);
        }
        Ok(VolumePage {
            records: page.records,
            next_token: page.next_volume_id.map(encode_volume_cursor),
        })
    }
    pub fn data_dir(&self, volume_id: &str) -> PathBuf {
        self.root.join("data").join(volume_id)
    }
    pub fn max_size_mb(&self) -> u64 {
        self.limits.max_size_mb
    }
    pub async fn commit_expansion(
        &self,
        id: &str,
        owner: &str,
        expected_size_mb: u64,
        expected_layers: &[OverlaybdLayerRef],
        size_mb: u64,
        layers: &[OverlaybdLayerRef],
    ) -> Result<(), VolumeError> {
        validate_volume_id(id)?;
        if size_mb > self.limits.max_size_mb {
            return Err(VolumeError::SizeLimitExceeded {
                max_size_mb: self.limits.max_size_mb,
            });
        }
        self.repository
            .commit_expansion(
                id,
                owner,
                expected_size_mb,
                expected_layers,
                size_mb,
                layers,
            )
            .await
            .map_err(repository_error)?;
        // Cached image configs represent the old capacity/content generation.
        self.records.write().await.remove(id);
        Ok(())
    }
    pub async fn materialize_captured_backing(
        &self,
        id: &str,
        layers: &[OverlaybdLayerRef],
        destination: &Path,
    ) -> Result<PathBuf, VolumeError> {
        validate_volume_id(id)?;
        self.repository
            .materialize_volume_backing(id, layers, destination)
            .await
            .map_err(repository_error)
    }
    pub async fn materialize_backing(&self, reference: &str) -> Result<VolumeRecord, VolumeError> {
        let mut record = self.get(reference).await?;
        if record
            .backing_image_config
            .as_ref()
            .is_some_and(|path| path.exists())
            || record.backing_layers.is_empty()
        {
            return Ok(record);
        }
        let destination = self.data_dir(&record.id).join("image.json");
        debug!(
            volume_id = %record.id,
            layer_count = record.backing_layers.len(),
            "materializing volume backing"
        );
        let path = self
            .repository
            .materialize_volume_backing(&record.id, &record.backing_layers, &destination)
            .await
            .map_err(repository_error)?;
        debug!(
            volume_id = %record.id,
            path = %path.display(),
            "volume backing materialized"
        );
        record.backing_image_config = Some(path);
        self.cache_record(record.clone()).await;
        Ok(record)
    }
    pub async fn create(
        &self,
        name: String,
        mode: VolumeMode,
        from_volume: Option<String>,
        source_config: Option<PathBuf>,
        size_mb: u64,
    ) -> Result<VolumeRecord, VolumeError> {
        self.create_with_source_owner(
            name,
            mode,
            from_volume.map(|reference| (reference, None)),
            source_config,
            size_mb,
            None,
        )
        .await
    }
    /// Creates a private writable child without publishing its unchanged backing first.
    /// The caller holds a read lease on the immutable seed until this child is released.
    pub async fn create_build_cache(
        &self,
        name: String,
        seed: Option<String>,
        size_mb: u64,
        owner: &str,
    ) -> Result<VolumeRecord, VolumeError> {
        self.create_with_source_owner(
            name,
            VolumeMode::Exclusive,
            seed.map(|id| (id, None)),
            None,
            size_mb,
            Some(owner),
        )
        .await
    }
    /// Register an immutable captured backing, preserving its layers instead
    /// of formatting or materializing a replacement empty disk.
    pub async fn register_snapshot(
        &self,
        record: VolumeRecord,
    ) -> Result<VolumeRecord, VolumeError> {
        validate_volume_id(&record.id)?;
        validate_name(&record.name)?;
        if record.size_mb == 0
            || record.backing_layers.is_empty()
            || record.backing_image_config.is_some()
            || record.status != VolumeStatus::Ready
            || record.deleting
            || record.reserved_by_sandbox_id.is_some()
            || !record.read_only_mounts.is_empty()
        {
            return Err(VolumeError::InvalidSize);
        }
        if record.size_mb > self.limits.max_size_mb {
            return Err(VolumeError::SizeLimitExceeded {
                max_size_mb: self.limits.max_size_mb,
            });
        }
        self.repository
            .create_volume(record.clone())
            .await
            .map_err(repository_error)?;
        self.cache_record(record.clone()).await;
        Ok(record)
    }
    /// Execute one already-journaled managed create. The caller owns the
    /// operation lock and pins source_config to an immutable captured head.
    /// Unlike legacy create, failures retain artifacts for same-operation recovery.
    pub async fn create_identified(
        &self,
        id: &str,
        filesystem_uuid: &str,
        mode: VolumeMode,
        source_config: Option<PathBuf>,
        size_mb: u64,
    ) -> Result<VolumeRecord, VolumeError> {
        validate_volume_id(id)?;
        let uuid =
            Uuid::parse_str(filesystem_uuid).map_err(|e| VolumeError::Storage(e.to_string()))?;
        if uuid.is_nil() {
            return Err(VolumeError::Storage("nil filesystem identity".into()));
        }
        if mode == VolumeMode::ReadOnly && source_config.is_none() {
            return Err(VolumeError::Storage(
                "readonly blank volume cannot be initialized".into(),
            ));
        }
        self.create_inner(
            id.to_owned(),
            mode,
            None,
            source_config,
            size_mb,
            None,
            Some((id, filesystem_uuid)),
        )
        .await
    }
    async fn create_with_source_owner(
        &self,
        name: String,
        mode: VolumeMode,
        from_volume: Option<(String, Option<&str>)>,
        source_config: Option<PathBuf>,
        size_mb: u64,
        reserved_owner: Option<&str>,
    ) -> Result<VolumeRecord, VolumeError> {
        self.create_inner(
            name,
            mode,
            from_volume,
            source_config,
            size_mb,
            reserved_owner,
            None,
        )
        .await
    }
    async fn create_inner(
        &self,
        name: String,
        mode: VolumeMode,
        from_volume: Option<(String, Option<&str>)>,
        source_config: Option<PathBuf>,
        size_mb: u64,
        reserved_owner: Option<&str>,
        allocated: Option<(&str, &str)>,
    ) -> Result<VolumeRecord, VolumeError> {
        validate_name(&name)?;
        if size_mb == 0 {
            return Err(VolumeError::InvalidSize);
        }
        if size_mb > self.limits.max_size_mb {
            return Err(VolumeError::SizeLimitExceeded {
                max_size_mb: self.limits.max_size_mb,
            });
        }
        if from_volume.is_some() && source_config.is_some() {
            return Err(VolumeError::MultipleSources);
        }
        match self.get(&name).await {
            Ok(_) => return Err(VolumeError::NameConflict(name)),
            Err(VolumeError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }

        let id = allocated
            .map(|(id, _)| id.to_owned())
            .unwrap_or_else(|| format!("vol_{}", Uuid::now_v7().simple()));
        if allocated.is_some() {
            match self.get(&id).await {
                Ok(_) => return Err(VolumeError::NameConflict(id)),
                Err(VolumeError::NotFound(_)) => (),
                Err(error) => return Err(error),
            }
            if tokio::fs::try_exists(self.data_dir(&id))
                .await
                .map_err(|e| VolumeError::Storage(e.to_string()))?
            {
                return Err(VolumeError::Storage(
                    "managed create already has artifacts; inspect the original operation".into(),
                ));
            }
        }
        debug!(
            volume_id = %id,
            mode = ?mode,
            size_mb,
            "creating volume"
        );
        let mut backing_layers = Vec::new();
        let backing_image_config = if let Some((reference, source_owner)) = from_volume {
            let mut parent = match self.get(&reference).await {
                Ok(parent) => parent,
                Err(VolumeError::NotFound(_)) => {
                    return Err(VolumeError::SourceNotFound(reference))
                }
                Err(error) => return Err(error),
            };
            match parent.status {
                VolumeStatus::Uploading => return Err(VolumeError::Uploading(parent.id.clone())),
                VolumeStatus::Failed => return Err(VolumeError::Failed(parent.id.clone())),
                VolumeStatus::Ready => {}
            }
            if let Some(owner) = parent.reserved_by_sandbox_id.as_deref() {
                if source_owner != Some(owner) {
                    return Err(VolumeError::Reserved(owner.to_owned()));
                }
            }
            if parent.size_mb != size_mb {
                return Err(VolumeError::SizeMismatch);
            }
            if parent.reserved_by_sandbox_id.is_none() && !parent.backing_layers.is_empty() {
                // Committed layers are immutable and repository-owned. Materializing
                // the child on mount preserves their descriptors without copying or
                // republishing the parent's local files. Reserved parents may have
                // newer local writes, so they still need the local clone below.
                backing_layers = parent.backing_layers;
                None
            } else {
                if parent.backing_image_config.is_none() && !parent.backing_layers.is_empty() {
                    parent = self.materialize_backing(&parent.id).await?;
                }
                match parent.backing_image_config.as_ref() {
                    Some(path) => Some(self.create_child_backing(&id, path).await?),
                    None => None,
                }
            }
        } else if let Some(source_config) = source_config {
            if allocated.is_some() {
                let directory = self.data_dir(&id);
                if let Some(parent) = directory.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| VolumeError::Storage(e.to_string()))?;
                }
                Some(
                    crate::content::create_identified_child_backing(&directory, &source_config)
                        .await?,
                )
            } else {
                Some(self.create_child_backing(&id, &source_config).await?)
            }
        } else if let Some((_, uuid)) = allocated {
            let directory = self.data_dir(&id);
            if let Some(parent) = directory.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| VolumeError::Storage(e.to_string()))?;
            }
            Some(crate::content::create_identified_empty_backing(&directory, size_mb, uuid).await?)
        } else {
            Some(self.create_empty_backing(&id, size_mb).await?)
        };

        let mut record = VolumeRecord {
            id,
            name,
            mode,
            size_mb,
            status: VolumeStatus::Ready,
            reserved_by_sandbox_id: reserved_owner.map(str::to_owned),
            backing_image_config,
            backing_layers,
            read_only_mounts: Vec::new(),
            deleting: false,
        };
        let create_result = async {
            if reserved_owner.is_none() {
                self.publish_backing(&mut record).await?;
            }
            self.repository
                .create_volume(record.clone())
                .await
                .map_err(repository_error)?;
            self.cache_record(record.clone()).await;
            Ok(())
        }
        .await;
        if let Err(error) = create_result {
            if allocated.is_some() {
                return Err(error);
            }
            if let Some(config) = record.backing_image_config.as_ref() {
                if let Some(directory) = config.parent() {
                    let _ = tokio::fs::remove_dir_all(directory).await;
                }
            }
            return Err(error);
        }
        info!(
            volume_id = %record.id,
            mode = ?record.mode,
            size_mb = record.size_mb,
            "volume created"
        );
        Ok(record)
    }
    pub async fn delete(&self, reference: &str) -> Result<(), VolumeError> {
        let record = self.get(reference).await?;
        if let Some(owner) = record.reserved_by_sandbox_id.as_deref() {
            return Err(VolumeError::Reserved(owner.to_owned()));
        }
        if let Some(owner) = record.read_only_mounts.first() {
            return Err(VolumeError::Reserved(owner.clone()));
        }
        debug!(volume_id = %record.id, "deleting volume");
        self.repository
            .delete_volume(&record.id)
            .await
            .map_err(repository_error)?;
        let backing_directory = self.data_dir(&record.id);
        self.records.write().await.remove(&record.id);
        if let Err(error) = tokio::fs::remove_dir_all(&backing_directory).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                warn!(
                    volume_id = %record.id,
                    path = %backing_directory.display(),
                    %error,
                    "failed to clean deleted volume's node-local backing"
                );
            }
        }
        info!(volume_id = %record.id, "volume deleted");
        Ok(())
    }
    pub async fn create_child_for_owner(
        &self,
        reference: &str,
        name: String,
        mode: VolumeMode,
        size_mb: u64,
        source_owner: &str,
        child_owner: &str,
    ) -> Result<VolumeRecord, VolumeError> {
        self.create_with_source_owner(
            name,
            mode,
            Some((reference.to_owned(), Some(source_owner))),
            None,
            size_mb,
            Some(child_owner),
        )
        .await
    }
    /// Publish exactly the immutable config saved inside a joint capture.
    /// This must not update the live volume catalog or re-read its current head.
    pub async fn commit_captured_layers(
        &self,
        id: &str,
        owner: &str,
        layers: Vec<OverlaybdLayerRef>,
    ) -> Result<(), VolumeError> {
        let mut record = self.get(id).await?;
        if record.mode != VolumeMode::Exclusive
            || record.reserved_by_sandbox_id.as_deref() != Some(owner)
        {
            return Err(VolumeError::Reserved(
                record.reserved_by_sandbox_id.unwrap_or_default(),
            ));
        }
        if layers.is_empty() {
            return Err(VolumeError::Storage(
                "captured native backing is empty".into(),
            ));
        }
        if record.backing_layers == layers && record.status == VolumeStatus::Ready {
            return Ok(());
        }
        record.status = VolumeStatus::Uploading;
        self.persist_catalog(&record).await?;
        record.backing_layers = layers;
        record.backing_image_config = None;
        record.status = VolumeStatus::Ready;
        self.persist_catalog(&record).await?;
        self.cache_record(record).await;
        Ok(())
    }
    pub async fn publish_captured_backing(
        &self,
        volume_id: &str,
        image_config_path: &Path,
    ) -> Result<Vec<OverlaybdLayerRef>, VolumeError> {
        self.repository
            .publish_volume_backing(volume_id, image_config_path)
            .await
            .map_err(repository_error)
    }
    pub async fn snapshot_volume_state(
        &self,
        reference: &str,
    ) -> Result<VolumeRecord, VolumeError> {
        let mut parent = self.get(reference).await?;
        // A running sandbox restacks its volume upper into this same local
        // image config before capture. Publish that latest state before making
        // the logical volume snapshot.
        let owner = parent.reserved_by_sandbox_id.clone();
        if let Some(owner) = owner.as_deref() {
            self.recover_and_publish_backings(owner, std::slice::from_ref(&parent.id))
                .await?;
            parent = self.get(reference).await?;
        } else {
            self.publish_backing(&mut parent).await?;
            self.persist_catalog(&parent).await?;
            self.cache_record(parent.clone()).await;
        }
        Ok(parent)
    }
    pub async fn reserve(&self, reference: &str, owner: &str) -> Result<(), VolumeError> {
        let mut record = self.get(reference).await?;
        match record.status {
            VolumeStatus::Uploading => return Err(VolumeError::Uploading(record.id.clone())),
            VolumeStatus::Failed => return Err(VolumeError::Failed(record.id.clone())),
            VolumeStatus::Ready => {}
        }
        if record.mode == VolumeMode::ReadOnly {
            self.repository
                .reserve_read_only_volume(&record.id, owner)
                .await
                .map_err(repository_error)?;
            if !record.read_only_mounts.iter().any(|entry| entry == owner) {
                record.read_only_mounts.push(owner.to_owned());
            }
            debug!(volume_id = %record.id, "read-only volume reserved");
            self.cache_record(record).await;
            return Ok(());
        }
        if let Some(existing) = self
            .repository
            .reserve_volume(&record.id, owner)
            .await
            .map_err(repository_error)?
        {
            return Err(VolumeError::Reserved(existing));
        }
        record.reserved_by_sandbox_id = Some(owner.to_owned());
        debug!(volume_id = %record.id, "exclusive volume reserved");
        self.cache_record(record).await;
        Ok(())
    }
    pub async fn replace_owner_for(
        &self,
        owner: &str,
        new_owner: Option<&str>,
        volume_ids: &[String],
    ) -> Result<(), VolumeError> {
        for volume_id in volume_ids {
            self.repository
                .replace_volume_owner_for(volume_id, owner, new_owner)
                .await
                .map_err(repository_error)?;
            if let Some(record) = self.records.write().await.get_mut(volume_id) {
                record.replace_owner(owner, new_owner);
            }
            match new_owner {
                Some(new_owner) => debug!(
                    volume_id = %volume_id,
                    previous_owner = %owner,
                    new_owner = %new_owner,
                    "volume reservation owner replaced"
                ),
                None => debug!(volume_id = %volume_id, %owner, "volume reservation released"),
            }
        }
        Ok(())
    }
    /// Keep an incomplete capture unavailable after its sandbox is stopped.
    pub async fn fail_backings(
        &self,
        owner: &str,
        volume_ids: &[String],
    ) -> Result<(), VolumeError> {
        for volume_id in volume_ids {
            let mut record = self.get(volume_id).await?;
            if record.reserved_by_sandbox_id.as_deref() == Some(owner) {
                record.status = VolumeStatus::Failed;
                self.persist_catalog(&record).await?;
                self.cache_record(record).await;
            }
        }
        Ok(())
    }
    pub async fn publish_backings(
        &self,
        owner: &str,
        volume_ids: &[String],
    ) -> Result<(), VolumeError> {
        let mut records = Vec::with_capacity(volume_ids.len());
        for volume_id in volume_ids {
            let record = self.get(volume_id).await?;
            if record.mounted_by(owner) {
                records.push(record);
            }
        }
        self.publish_records(owner, records).await
    }
    /// Restores deterministic node-local config paths after a server restart.
    pub async fn recover_backings(
        &self,
        owner: &str,
        volume_ids: &[String],
    ) -> Result<(), VolumeError> {
        for volume_id in volume_ids {
            let mut record = self.get(volume_id).await?;
            if record.reserved_by_sandbox_id.as_deref() != Some(owner) {
                continue;
            }
            if record.backing_image_config.is_none() {
                let path = self.data_dir(&record.id).join("image.json");
                if !path.exists() {
                    record.status = VolumeStatus::Failed;
                    if let Err(error) = self.persist_catalog(&record).await {
                        warn!(
                            volume_id = %record.id,
                            %error,
                            "failed to persist volume failure status"
                        );
                    }
                    self.cache_record(record.clone()).await;
                    warn!(
                        volume_id = %record.id,
                        path = %path.display(),
                        "reserved volume backing is missing"
                    );
                    return Err(VolumeError::Storage(format!(
                        "local backing for reserved volume '{}' is missing",
                        record.id
                    )));
                }
                record.backing_image_config = Some(path);
                debug!(volume_id = %record.id, %owner, "volume backing recovered");
                self.cache_record(record).await;
            }
        }
        Ok(())
    }
    pub async fn recover_and_publish_backings(
        &self,
        owner: &str,
        volume_ids: &[String],
    ) -> Result<(), VolumeError> {
        self.recover_backings(owner, volume_ids).await?;
        self.publish_backings(owner, volume_ids).await
    }
    async fn publish_records(
        &self,
        owner: &str,
        records: Vec<VolumeRecord>,
    ) -> Result<(), VolumeError> {
        for mut record in records {
            if record.reserved_by_sandbox_id.as_deref() != Some(owner) {
                continue;
            }
            // Publish the state transition before uploading any writable
            // upper layer so other nodes cannot mount stale content.
            record.status = VolumeStatus::Uploading;
            if let Err(error) = self.persist_catalog(&record).await {
                self.mark_publication_failed(record, "mark_uploading", &error)
                    .await;
                return Err(error);
            }
            self.cache_record(record.clone()).await;
            debug!(volume_id = %record.id, "publishing volume backing");
            if let Err(error) = self.publish_backing(&mut record).await {
                self.mark_publication_failed(record, "publish_backing", &error)
                    .await;
                return Err(error);
            }
            record.status = VolumeStatus::Ready;
            if let Err(error) = self.persist_catalog(&record).await {
                self.mark_publication_failed(record, "mark_ready", &error)
                    .await;
                return Err(error);
            }
            info!(
                volume_id = %record.id,
                layer_count = record.backing_layers.len(),
                "volume backing published"
            );
            self.cache_record(record).await;
        }
        Ok(())
    }
    async fn mark_publication_failed(
        &self,
        mut record: VolumeRecord,
        stage: &'static str,
        error: &VolumeError,
    ) {
        record.status = VolumeStatus::Failed;
        if let Err(status_error) = self.persist_catalog(&record).await {
            warn!(
                volume_id = %record.id,
                error = %status_error,
                "failed to persist volume failure status"
            );
        }
        warn!(
            volume_id = %record.id,
            stage,
            %error,
            "volume backing publication failed"
        );
        self.cache_record(record).await;
    }
    pub async fn cache_record(&self, record: VolumeRecord) {
        self.records.write().await.insert(record.id.clone(), record);
    }
    async fn publish_backing(&self, record: &mut VolumeRecord) -> Result<(), VolumeError> {
        if let Some(path) = record.backing_image_config.as_deref() {
            record.backing_layers = self
                .repository
                .publish_volume_backing(&record.id, path)
                .await
                .map_err(repository_error)?;
        }
        Ok(())
    }
    async fn persist_catalog(&self, record: &VolumeRecord) -> Result<(), VolumeError> {
        self.repository
            .put_volume(record.clone())
            .await
            .map_err(repository_error)
    }
    async fn create_empty_backing(
        &self,
        volume_id: &str,
        size_mb: u64,
    ) -> Result<PathBuf, VolumeError> {
        crate::content::create_empty_backing(&self.data_dir(volume_id), size_mb).await
    }
    async fn create_child_backing(
        &self,
        volume_id: &str,
        parent_config: &Path,
    ) -> Result<PathBuf, VolumeError> {
        crate::content::create_child_backing(&self.data_dir(volume_id), parent_config).await
    }
}
fn current_local_backing(
    remote: &VolumeRecord,
    local: &HashMap<String, VolumeRecord>,
) -> Option<PathBuf> {
    local
        .get(&remote.id)
        .filter(|record| record.backing_layers == remote.backing_layers)
        .and_then(|record| record.backing_image_config.clone())
        .filter(|path| path.exists())
}
fn repository_error(error: RepositoryError) -> VolumeError {
    match error {
        RepositoryError::VolumeNotFound { lookup } => VolumeError::NotFound(lookup),
        RepositoryError::VolumeNameConflict { name } => VolumeError::NameConflict(name),
        error => VolumeError::Storage(error.to_string()),
    }
}
fn encode_volume_cursor(volume_id: String) -> String {
    URL_SAFE_NO_PAD.encode(volume_id)
}
fn decode_volume_cursor(token: &str) -> Result<String, VolumeError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|_| VolumeError::InvalidNextToken)?;
    let volume_id = String::from_utf8(bytes).map_err(|_| VolumeError::InvalidNextToken)?;
    validate_volume_id(&volume_id).map_err(|_| VolumeError::InvalidNextToken)?;
    Ok(volume_id)
}
fn validate_name(name: &str) -> Result<(), VolumeError> {
    if is_valid_volume_component(name) {
        Ok(())
    } else {
        Err(VolumeError::InvalidName)
    }
}
fn validate_volume_id(id: &str) -> Result<(), VolumeError> {
    if is_valid_volume_component(id) {
        Ok(())
    } else {
        Err(VolumeError::Storage(format!("invalid volume id '{id}'")))
    }
}
