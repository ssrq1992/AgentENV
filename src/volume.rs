use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info};
use uuid::Uuid;

use crate::snapshot::repository::{RepositoryError, SnapshotRepository};
use crate::snapshot::OverlaybdLayerRef;

pub use aenv_volume_core::{
    is_valid_volume_component, VolumeError, VolumeMode, VolumePage, VolumeRecord, VolumeStatus,
    DEFAULT_VOLUME_PAGE_SIZE, DEFAULT_VOLUME_SIZE_MB, MAX_VOLUME_MOUNTS,
};

#[derive(Serialize, Deserialize)]
struct PrivateReservationCleanup {
    owner: String,
    volumes: Vec<String>,
    context: crate::sandbox::storage::StorageContext,
}

#[derive(Serialize, Deserialize)]
struct PrivateVolumeCleanup {
    version: u32,
    id: String,
    name: String,
    context: crate::sandbox::storage::StorageContext,
    created: bool,
    #[serde(default = "private_cleanup_default")]
    private: bool,
}
fn private_cleanup_default() -> bool {
    true
}

#[derive(Clone)]
pub struct VolumeManager {
    engine: aenv_volume_core::engine::VolumeEngine,
    root: PathBuf,
    #[cfg(test)]
    repository: Arc<dyn SnapshotRepository>,
    limits: VolumeLimits,
}

#[derive(Clone, Copy, Debug)]
pub struct VolumeLimits {
    pub max_size_mb: u64,
    pub max_mounts: usize,
}

impl Default for VolumeLimits {
    fn default() -> Self {
        let config = crate::cfg::VolumeConfig::default();
        Self {
            max_size_mb: config.max_size_mb,
            max_mounts: config.max_volume_count,
        }
    }
}

impl VolumeManager {
    pub async fn open_with_repository(
        path: impl Into<std::path::PathBuf>,
        repository: Arc<dyn SnapshotRepository>,
    ) -> anyhow::Result<Self> {
        Self::open_with_repository_and_limits(path, repository, VolumeLimits::default()).await
    }

    pub async fn open_with_repository_and_limits(
        path: impl Into<std::path::PathBuf>,
        repository: Arc<dyn SnapshotRepository>,
        mut limits: VolumeLimits,
    ) -> anyhow::Result<Self> {
        let path = path.into();
        let root = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("volume catalog path has no parent"))?
            .to_path_buf();
        limits.max_mounts = limits.max_mounts.min(MAX_VOLUME_MOUNTS);
        Ok(Self {
            engine: aenv_volume_core::engine::VolumeEngine::new(
                root.clone(),
                Arc::new(SnapshotVolumeRepository(Arc::clone(&repository))),
                aenv_volume_core::engine::VolumeLimits {
                    max_size_mb: limits.max_size_mb,
                    max_mounts: limits.max_mounts,
                },
            ),
            root,
            #[cfg(test)]
            repository,
            limits,
        })
    }

    #[cfg(test)]
    async fn persist_catalog(&self, record: &VolumeRecord) -> Result<(), VolumeError> {
        self.repository
            .put_volume(record.clone())
            .await
            .map_err(repository_error)
    }

    pub fn limits(&self) -> VolumeLimits {
        self.limits
    }

    pub(crate) async fn create_from_snapshot(
        &self,
        name: String,
        mode: VolumeMode,
        size_mb: u64,
        backing_layers: Vec<OverlaybdLayerRef>,
    ) -> Result<VolumeRecord, VolumeError> {
        self.create_snapshot_volume_scoped(name, mode, size_mb, backing_layers, None, false)
            .await
    }

    pub(crate) async fn create_consumer_snapshot_volume(
        &self,
        name: String,
        mode: VolumeMode,
        size_mb: u64,
        backing_layers: Vec<OverlaybdLayerRef>,
        context: &crate::sandbox::storage::StorageContext,
        private: bool,
    ) -> Result<VolumeRecord, VolumeError> {
        self.create_snapshot_volume_scoped(
            name,
            mode,
            size_mb,
            backing_layers,
            Some(context),
            private,
        )
        .await
    }

    async fn create_snapshot_volume_scoped(
        &self,
        name: String,
        mode: VolumeMode,
        size_mb: u64,
        backing_layers: Vec<OverlaybdLayerRef>,
        context: Option<&crate::sandbox::storage::StorageContext>,
        private: bool,
    ) -> Result<VolumeRecord, VolumeError> {
        validate_name(&name)?;
        if size_mb == 0 || backing_layers.is_empty() {
            return Err(VolumeError::InvalidSize);
        }
        if size_mb > self.limits.max_size_mb {
            return Err(VolumeError::SizeLimitExceeded {
                max_size_mb: self.limits.max_size_mb,
            });
        }
        match self.get(&name).await {
            Ok(_) => return Err(VolumeError::NameConflict(name)),
            Err(VolumeError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }

        let record = VolumeRecord {
            id: format!("vol_{}", Uuid::now_v7().simple()),
            name,
            mode,
            size_mb,
            status: VolumeStatus::Ready,
            reserved_by_sandbox_id: None,
            backing_image_config: None,
            backing_layers,
            read_only_mounts: Vec::new(),
            deleting: false,
        };
        debug!(
            volume_id = %record.id,
            mode = ?record.mode,
            size_mb = record.size_mb,
            "creating volume from snapshot"
        );
        let mut cleanup = context.map(|context| PrivateVolumeCleanup {
            version: 1,
            id: record.id.clone(),
            name: record.name.clone(),
            context: context.clone(),
            created: false,
            private,
        });
        if let Some(task) = cleanup.as_ref() {
            self.persist_private_volume_cleanup(task)?;
        }
        self.engine.register_snapshot(record.clone()).await?;
        if let Some(task) = cleanup.as_mut() {
            task.created = true;
            self.persist_private_volume_cleanup(task)?;
        }
        self.cache_record(record.clone()).await;
        info!(
            volume_id = %record.id,
            mode = ?record.mode,
            size_mb = record.size_mb,
            "volume created from snapshot"
        );
        Ok(record)
    }

    pub(crate) fn persist_storage_reservation_cleanup(
        &self,
        context: &crate::sandbox::storage::StorageContext,
        owner: &str,
        volumes: &[String],
    ) -> Result<(), VolumeError> {
        use std::io::Write;
        let task = PrivateReservationCleanup {
            owner: owner.to_owned(),
            volumes: volumes.to_vec(),
            context: context.clone(),
        };
        let result = (|| -> anyhow::Result<()> {
            anyhow::ensure!(
                is_valid_volume_component(&context.request_id),
                "invalid provider request identity"
            );
            let root = self.root.join("internal-reservations");
            std::fs::create_dir_all(&root)?;
            std::fs::File::open(&self.root)?.sync_all()?;
            let mut temporary = tempfile::NamedTempFile::new_in(&root)?;
            temporary.write_all(&serde_json::to_vec(&task)?)?;
            temporary.as_file().sync_all()?;
            temporary
                .persist(root.join(format!("{}.json", context.request_id)))
                .map_err(|error| error.error)?;
            std::fs::File::open(root)?.sync_all()?;
            Ok(())
        })();
        result.map_err(|error| VolumeError::Storage(error.to_string()))
    }

    async fn reap_private_reservation(&self, path: &Path) -> anyhow::Result<()> {
        let task: PrivateReservationCleanup = serde_json::from_slice(&std::fs::read(path)?)?;
        anyhow::ensure!(
            is_valid_volume_component(&task.context.request_id)
                && path
                    == self
                        .root
                        .join("internal-reservations")
                        .join(format!("{}.json", task.context.request_id)),
            "invalid reservation cleanup identity"
        );
        let status = task.context.control("cleanup.status").await?;
        if status.get("terminal").and_then(serde_json::Value::as_bool) != Some(true) {
            return Ok(());
        }
        let mut existing = Vec::new();
        for id in task.volumes {
            match self.get(&id).await {
                Ok(_) => existing.push(id),
                Err(VolumeError::NotFound(_)) => (),
                Err(error) => return Err(error.into()),
            }
        }
        // A failed create may still have written legacy data under its pending
        // owner. Publish that stopped backing before clearing the reservation.
        self.recover_and_publish_backings(&task.owner, &existing)
            .await?;
        self.replace_owner_for(&task.owner, None, &existing).await?;
        std::fs::remove_file(path)?;
        std::fs::File::open(self.root.join("internal-reservations"))?.sync_all()?;
        Ok(())
    }

    fn persist_private_volume_cleanup(
        &self,
        task: &PrivateVolumeCleanup,
    ) -> Result<(), VolumeError> {
        use std::io::Write;
        let result = (|| -> anyhow::Result<()> {
            validate_volume_id(&task.id)?;
            let root = self.root.join("internal-volumes");
            std::fs::create_dir_all(&root)?;
            std::fs::File::open(&self.root)?.sync_all()?;
            let mut temporary = tempfile::NamedTempFile::new_in(&root)?;
            temporary.write_all(&serde_json::to_vec(task)?)?;
            temporary.as_file().sync_all()?;
            temporary
                .persist(root.join(format!("{}.json", task.id)))
                .map_err(|error| error.error)?;
            std::fs::File::open(root)?.sync_all()?;
            Ok(())
        })();
        result.map_err(|error| VolumeError::Storage(error.to_string()))
    }

    async fn reap_private_volume(&self, path: &Path) -> anyhow::Result<()> {
        let task: PrivateVolumeCleanup = serde_json::from_slice(&std::fs::read(path)?)?;
        validate_volume_id(&task.id)?;
        anyhow::ensure!(
            task.version == 1
                && path
                    == self
                        .root
                        .join("internal-volumes")
                        .join(format!("{}.json", task.id)),
            "invalid internal volume cleanup identity"
        );
        let status = task.context.control("cleanup.status").await?;
        let exposed = status
            .get("ever_ready")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
            && status.get("ephemeral").and_then(serde_json::Value::as_bool) == Some(false);
        if !task.private && exposed {
            std::fs::remove_file(path)?;
            std::fs::File::open(self.root.join("internal-volumes"))?.sync_all()?;
            return Ok(());
        }
        if status.get("terminal").and_then(serde_json::Value::as_bool) != Some(true) {
            return Ok(());
        }
        match self.get(&task.id).await {
            Ok(record) => {
                anyhow::ensure!(record.name == task.name, "internal volume identity changed");
                if record.reserved_by_sandbox_id.is_some() || !record.read_only_mounts.is_empty() {
                    return Ok(());
                }
                let mut observed = task;
                observed.created = true;
                self.persist_private_volume_cleanup(&observed)?;
                self.delete(&record.id).await?;
            }
            Err(VolumeError::NotFound(_)) if task.created => (),
            Err(VolumeError::NotFound(_)) => return Ok(()), // unknown remote create may still complete
            Err(error) => return Err(error.into()),
        }
        std::fs::remove_file(path)?;
        std::fs::File::open(self.root.join("internal-volumes"))?.sync_all()?;
        Ok(())
    }

    /// Reap runtime-owned retained legacy clones after provider-confirmed stop.
    /// Public/default-inherited volumes have no cleanup task and are preserved.
    pub fn start_internal_volume_gc(&self) {
        let manager = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tick.tick().await;
                if let Ok(entries) = std::fs::read_dir(manager.root.join("internal-reservations")) {
                    for entry in entries {
                        let Ok(entry) = entry else {
                            continue;
                        };
                        if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                            if let Err(error) =
                                manager.reap_private_reservation(&entry.path()).await
                            {
                                debug!(%error, "native legacy reservation cleanup deferred");
                            }
                        }
                    }
                }
                let Ok(entries) = std::fs::read_dir(manager.root.join("internal-volumes")) else {
                    continue;
                };
                for entry in entries {
                    let Ok(entry) = entry else {
                        continue;
                    };
                    if entry.file_type().is_ok_and(|kind| kind.is_file())
                        && entry
                            .path()
                            .extension()
                            .is_some_and(|extension| extension == "json")
                    {
                        if let Err(error) = manager.reap_private_volume(&entry.path()).await {
                            debug!(%error,"retained legacy volume cleanup deferred");
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
fn repository_error(error: RepositoryError) -> VolumeError {
    match error {
        RepositoryError::VolumeNotFound { lookup } => VolumeError::NotFound(lookup),
        RepositoryError::VolumeNameConflict { name } => VolumeError::NameConflict(name),
        error => VolumeError::Storage(error.to_string()),
    }
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use overlaybd::config::{ImageConfig, LayerConfig};

    use super::{VolumeError, VolumeManager, VolumeMode, VolumeRecord, VolumeStatus};
    use crate::digest::sha256_digest;
    use crate::snapshot::repository::backends::{PosixFsBackend, PosixFsBackendConfig};
    use crate::snapshot::{ManagedLayer, OverlaybdLayerRef};

    async fn manager_at(root: &Path) -> VolumeManager {
        let backend = PosixFsBackend::new(PosixFsBackendConfig {
            root: root.join("repository"),
            cache_root: Some(root.join("cache")),
            runtime_cache_root: Some(root.join("runtime")),
        })
        .expect("POSIX snapshot backend");
        VolumeManager::open_with_repository(root.join("volumes/catalog"), backend.repository())
            .await
            .expect("volume manager")
    }

    async fn published_volume(root: &Path, manager: &VolumeManager) -> VolumeRecord {
        let layer = root.join("base.commit");
        std::fs::write(&layer, b"published volume layer").unwrap();
        let config = ImageConfig {
            lowers: vec![
                LayerConfig {
                    file: layer.to_string_lossy().into_owned(),
                    ..LayerConfig::default()
                },
                LayerConfig {
                    repo_blob_url: "https://registry.example.test/v2/cache/blobs".to_owned(),
                    digest: sha256_digest(b"external volume layer"),
                    size: 21,
                    ..LayerConfig::default()
                },
            ],
            ..ImageConfig::default()
        };
        let path = root.join("source.json");
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        manager
            .create(
                "parent".to_owned(),
                VolumeMode::Exclusive,
                None,
                Some(path),
                1024,
            )
            .await
            .expect("publish initial volume")
    }

    fn append_local_layer(record: &VolumeRecord) {
        let path = record.backing_image_config.as_ref().unwrap();
        let layer = path.parent().unwrap().join("unpublished.commit");
        std::fs::write(&layer, b"new volume writes").unwrap();
        let mut config = overlaybd::config::load_image_config(path).unwrap();
        config.lowers.push(LayerConfig {
            file: layer.to_string_lossy().into_owned(),
            ..LayerConfig::default()
        });
        std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn captured_backing_does_not_follow_or_rewind_the_live_volume_head() {
        let temp = tempfile::tempdir().unwrap();
        let manager = manager_at(temp.path()).await;
        let parent = published_volume(temp.path(), &manager).await;
        manager.reserve(&parent.id, "source-run").await.unwrap();
        let captured_config = temp.path().join("captured-volume.json");
        std::fs::copy(
            parent.backing_image_config.as_ref().unwrap(),
            &captured_config,
        )
        .unwrap();
        append_local_layer(&parent);
        manager
            .recover_and_publish_backings("source-run", std::slice::from_ref(&parent.id))
            .await
            .unwrap();
        let advanced = manager.get(&parent.id).await.unwrap();
        assert!(advanced.backing_layers.len() > parent.backing_layers.len());

        let captured = manager
            .publish_captured_backing(&parent.id, &captured_config)
            .await
            .unwrap();
        assert_eq!(captured, parent.backing_layers);
        assert_eq!(
            manager.get(&parent.id).await.unwrap().backing_layers,
            advanced.backing_layers
        );
    }

    #[tokio::test]
    async fn published_volume_clones_survive_parent_deletion_without_local_files() {
        let temp = tempfile::tempdir().unwrap();
        let manager = manager_at(temp.path()).await;
        let parent = published_volume(temp.path(), &manager).await;
        // A stale local config must not override the committed backing, even
        // when the manager still has the config path in its in-memory cache.
        std::fs::write(
            parent.backing_image_config.as_ref().unwrap(),
            b"stale config",
        )
        .unwrap();
        let cold_manager = manager_at(temp.path()).await;
        let mut children = Vec::new();
        for (index, (manager, mode)) in [
            (&manager, VolumeMode::ReadOnly),
            (&cold_manager, VolumeMode::Exclusive),
        ]
        .into_iter()
        .enumerate()
        {
            let child = manager
                .create(
                    format!("child-{index}"),
                    mode,
                    Some(parent.id.clone()),
                    None,
                    1024,
                )
                .await
                .expect("clone committed volume without reading local files");
            assert_eq!(child.backing_layers, parent.backing_layers);
            assert_eq!(child.mode, mode);
            assert!(child.backing_image_config.is_none());
            assert!(!manager.data_dir(&child.id).exists());
            children.push(child);
        }
        manager.delete(&parent.id).await.unwrap();
        let restarted = manager_at(temp.path()).await;
        for child in children {
            let child = restarted.materialize_backing(&child.id).await.unwrap();
            let config =
                overlaybd::config::load_image_config(child.backing_image_config.unwrap()).unwrap();
            assert_eq!(
                std::fs::read(&config.lowers[0].file).unwrap(),
                b"published volume layer"
            );
            assert_eq!(
                config.lowers[0].digest,
                sha256_digest(b"published volume layer")
            );
            assert_eq!(config.lowers[0].size, 22);
            assert!(config.lowers[1].file.is_empty());
            assert_eq!(
                config.lowers[1].repo_blob_url,
                "https://registry.example.test/v2/cache/blobs"
            );
            assert_eq!(
                config.lowers[1].digest,
                sha256_digest(b"external volume layer")
            );
        }
    }

    #[tokio::test]
    async fn volume_generations_preserve_inherited_descriptors_and_new_writes() {
        let temp = tempfile::tempdir().unwrap();
        let manager = manager_at(temp.path()).await;
        let parent = published_volume(temp.path(), &manager).await;
        let seed = manager
            .create(
                "seed".to_owned(),
                VolumeMode::ReadOnly,
                Some(parent.id.clone()),
                None,
                1024,
            )
            .await
            .unwrap();
        manager.reserve(&seed.id, "builder").await.unwrap();
        let child = manager
            .create_child_for_owner(
                &seed.id,
                "work".to_owned(),
                VolumeMode::Exclusive,
                1024,
                "builder",
                "builder",
            )
            .await
            .unwrap();
        assert_eq!(child.backing_layers, parent.backing_layers);
        let child = manager.materialize_backing(&child.id).await.unwrap();
        let config =
            overlaybd::config::load_image_config(child.backing_image_config.as_ref().unwrap())
                .unwrap();
        assert_eq!(
            config.lowers[0].digest,
            sha256_digest(b"published volume layer")
        );
        assert_eq!(config.lowers[0].size, 22);
        append_local_layer(&child);
        manager
            .recover_and_publish_backings("builder", std::slice::from_ref(&child.id))
            .await
            .unwrap();
        manager
            .replace_owner_for("builder", None, &[seed.id, child.id.clone()])
            .await
            .unwrap();
        let published = manager.get(&child.id).await.unwrap();
        assert_eq!(
            &published.backing_layers[..2],
            parent.backing_layers.as_slice()
        );
        assert_eq!(
            published.backing_layers[2],
            OverlaybdLayerRef::Managed(ManagedLayer {
                digest: sha256_digest(b"new volume writes"),
                size: 17,
                uuid: None,
            })
        );
        let next_seed = manager
            .create(
                "next-seed".to_owned(),
                VolumeMode::ReadOnly,
                Some(child.id),
                None,
                1024,
            )
            .await
            .unwrap();
        assert_eq!(next_seed.backing_layers, published.backing_layers);
        assert!(next_seed.backing_image_config.is_none());
    }

    #[tokio::test]
    async fn reserved_volume_clone_retains_unpublished_local_layers() {
        let temp = tempfile::tempdir().unwrap();
        let manager = manager_at(temp.path()).await;
        let parent = published_volume(temp.path(), &manager).await;
        manager.reserve(&parent.id, "source").await.unwrap();
        append_local_layer(&parent);
        let child = manager
            .create_child_for_owner(
                &parent.id,
                "fork".to_owned(),
                VolumeMode::Exclusive,
                1024,
                "source",
                "child",
            )
            .await
            .unwrap();
        assert!(child.backing_layers.is_empty());
        manager
            .replace_owner_for("source", None, std::slice::from_ref(&parent.id))
            .await
            .unwrap();
        manager.delete(&parent.id).await.unwrap();
        manager
            .recover_and_publish_backings("child", std::slice::from_ref(&child.id))
            .await
            .unwrap();
        let child = manager.get(&child.id).await.unwrap();
        assert_eq!(child.backing_layers.len(), 3);
        assert_eq!(
            child.backing_layers[2],
            OverlaybdLayerRef::Managed(ManagedLayer {
                digest: sha256_digest(b"new volume writes"),
                size: 17,
                uuid: None,
            })
        );
    }

    #[tokio::test]
    async fn published_volume_clone_validates_source_state_and_size() {
        let temp = tempfile::tempdir().unwrap();
        let manager = manager_at(temp.path()).await;
        let mut parent = published_volume(temp.path(), &manager).await;
        assert_eq!(
            manager
                .create(
                    "wrong-size".to_owned(),
                    VolumeMode::ReadOnly,
                    Some(parent.id.clone()),
                    None,
                    2048
                )
                .await
                .unwrap_err(),
            VolumeError::SizeMismatch
        );
        manager.reserve(&parent.id, "owner").await.unwrap();
        assert_eq!(
            manager
                .create(
                    "reserved".to_owned(),
                    VolumeMode::ReadOnly,
                    Some(parent.id.clone()),
                    None,
                    1024
                )
                .await
                .unwrap_err(),
            VolumeError::Reserved("owner".to_owned())
        );
        for status in [VolumeStatus::Uploading, VolumeStatus::Failed] {
            parent.status = status;
            manager.persist_catalog(&parent).await.unwrap();
            let error = manager
                .create(
                    "unready".to_owned(),
                    VolumeMode::ReadOnly,
                    Some(parent.id.clone()),
                    None,
                    1024,
                )
                .await
                .unwrap_err();
            assert_eq!(
                error,
                match status {
                    VolumeStatus::Uploading => VolumeError::Uploading(parent.id.clone()),
                    VolumeStatus::Failed => VolumeError::Failed(parent.id.clone()),
                    VolumeStatus::Ready => unreachable!(),
                }
            );
        }
    }

    #[tokio::test]
    async fn create_from_snapshot_preserves_read_only_mode() {
        let temp = tempfile::tempdir().expect("temporary volume repository");
        let manager = manager_at(temp.path()).await;

        let restored = manager
            .create_from_snapshot(
                "restored-read-only".to_owned(),
                VolumeMode::ReadOnly,
                1024,
                vec![OverlaybdLayerRef::Managed(ManagedLayer {
                    digest: "sha256:abc".to_owned(),
                    size: 4096,
                    uuid: None,
                })],
            )
            .await
            .expect("restore volume snapshot");

        assert_eq!(restored.mode, VolumeMode::ReadOnly);
        assert_eq!(
            manager
                .get(&restored.id)
                .await
                .expect("persisted volume")
                .mode,
            VolumeMode::ReadOnly
        );
    }
}

impl std::ops::Deref for VolumeManager {
    type Target = aenv_volume_core::engine::VolumeEngine;
    fn deref(&self) -> &Self::Target {
        &self.engine
    }
}
struct SnapshotVolumeRepository(Arc<dyn SnapshotRepository>);
fn core_repository_error(error: RepositoryError) -> aenv_volume_core::catalog::CatalogError {
    match error {
        RepositoryError::VolumeNotFound { lookup } => {
            aenv_volume_core::catalog::CatalogError::VolumeNotFound { lookup }
        }
        RepositoryError::VolumeNameConflict { name } => {
            aenv_volume_core::catalog::CatalogError::VolumeNameConflict { name }
        }
        error => {
            aenv_volume_core::catalog::CatalogError::backend("native volume repository", error)
        }
    }
}
#[async_trait::async_trait]
impl aenv_volume_core::engine::VolumeRepository for SnapshotVolumeRepository {
    async fn get_volume(
        &self,
        reference: &str,
    ) -> aenv_volume_core::catalog::CatalogResult<Option<VolumeRecord>> {
        self.0
            .get_volume(reference)
            .await
            .map_err(core_repository_error)
    }
    async fn list_volumes_page(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> aenv_volume_core::catalog::CatalogResult<crate::snapshot::repository::VolumeRecordPage>
    {
        self.0
            .list_volumes_page(after, limit)
            .await
            .map_err(core_repository_error)
    }
    async fn create_volume(
        &self,
        record: VolumeRecord,
    ) -> aenv_volume_core::catalog::CatalogResult<()> {
        self.0
            .create_volume(record)
            .await
            .map_err(core_repository_error)
    }
    async fn put_volume(
        &self,
        record: VolumeRecord,
    ) -> aenv_volume_core::catalog::CatalogResult<()> {
        self.0
            .put_volume(record)
            .await
            .map_err(core_repository_error)
    }
    async fn delete_volume(&self, id: &str) -> aenv_volume_core::catalog::CatalogResult<()> {
        self.0
            .delete_volume(id)
            .await
            .map_err(core_repository_error)
    }
    async fn reserve_volume(
        &self,
        id: &str,
        owner: &str,
    ) -> aenv_volume_core::catalog::CatalogResult<Option<String>> {
        self.0
            .reserve_volume(id, owner)
            .await
            .map_err(core_repository_error)
    }
    async fn reserve_read_only_volume(
        &self,
        id: &str,
        owner: &str,
    ) -> aenv_volume_core::catalog::CatalogResult<()> {
        self.0
            .reserve_read_only_volume(id, owner)
            .await
            .map_err(core_repository_error)
    }
    async fn replace_volume_owner_for(
        &self,
        id: &str,
        from: &str,
        to: Option<&str>,
    ) -> aenv_volume_core::catalog::CatalogResult<()> {
        self.0
            .replace_volume_owner_for(id, from, to)
            .await
            .map_err(core_repository_error)
    }
    async fn publish_volume_backing(
        &self,
        id: &str,
        path: &Path,
    ) -> aenv_volume_core::catalog::CatalogResult<Vec<OverlaybdLayerRef>> {
        self.0
            .publish_volume_backing(id, path)
            .await
            .map_err(core_repository_error)
    }
    async fn materialize_volume_backing(
        &self,
        id: &str,
        layers: &[OverlaybdLayerRef],
        destination: &Path,
    ) -> aenv_volume_core::catalog::CatalogResult<PathBuf> {
        self.0
            .materialize_volume_backing(id, layers, destination)
            .await
            .map_err(core_repository_error)
    }
}
