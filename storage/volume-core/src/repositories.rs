//! Repository adapters composing the original catalogs and layer algorithms.
use crate::{
    catalog::{CatalogError, CatalogResult},
    compact::CompactOutput,
    engine::VolumeRepository,
    layers::PosixLayerStore,
    object::{ObjectVolumeCatalog, VolumeObjectStore},
    object_layers::{ObjectLayerPublisher, ObjectLayerTransport},
    posix::PosixVolumeCatalog,
    OverlaybdLayerRef, VolumeRecord, VolumeRecordPage,
};
use async_trait::async_trait;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
#[derive(Clone)]
pub struct PosixVolumeRepository {
    root: PathBuf,
}
impl PosixVolumeRepository {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    async fn catalog<
        T: Send + 'static,
        F: FnOnce(&PosixVolumeCatalog) -> CatalogResult<T> + Send + 'static,
    >(
        &self,
        operation: F,
    ) -> CatalogResult<T> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || operation(&PosixVolumeCatalog::new(root)))
            .await
            .map_err(|e| CatalogError::backend("native POSIX catalog task", e))?
    }
}
#[async_trait]
impl VolumeRepository for PosixVolumeRepository {
    async fn commit_expansion(
        &self,
        id: &str,
        owner: &str,
        expected_size_mb: u64,
        expected_layers: &[OverlaybdLayerRef],
        size_mb: u64,
        layers: &[OverlaybdLayerRef],
    ) -> CatalogResult<()> {
        let (id, owner, expected_layers, layers) = (
            id.to_owned(),
            owner.to_owned(),
            expected_layers.to_vec(),
            layers.to_vec(),
        );
        self.catalog(move |store| {
            store.commit_expansion(
                &id,
                &owner,
                expected_size_mb,
                &expected_layers,
                size_mb,
                &layers,
            )
        })
        .await
    }
    async fn get_volume(&self, reference: &str) -> CatalogResult<Option<VolumeRecord>> {
        let reference = reference.to_owned();
        self.catalog(move |store| store.get_volume(&reference))
            .await
    }
    async fn list_volumes_page(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> CatalogResult<VolumeRecordPage> {
        let after = after.map(str::to_owned);
        self.catalog(move |store| store.list_volumes_page(after.as_deref(), limit))
            .await
    }
    async fn create_volume(&self, record: VolumeRecord) -> CatalogResult<()> {
        self.catalog(move |store| store.create_volume(&record))
            .await
    }
    async fn put_volume(&self, record: VolumeRecord) -> CatalogResult<()> {
        self.catalog(move |store| store.put_volume(&record)).await
    }
    async fn delete_volume(&self, id: &str) -> CatalogResult<()> {
        let id = id.to_owned();
        self.catalog(move |store| store.delete_volume(&id)).await
    }
    async fn reserve_volume(&self, id: &str, owner: &str) -> CatalogResult<Option<String>> {
        let id = id.to_owned();
        let owner = owner.to_owned();
        self.catalog(move |store| store.reserve_volume(&id, &owner))
            .await
    }
    async fn reserve_read_only_volume(&self, id: &str, owner: &str) -> CatalogResult<()> {
        let id = id.to_owned();
        let owner = owner.to_owned();
        self.catalog(move |store| store.reserve_read_only_volume(&id, &owner))
            .await
    }
    async fn replace_volume_owner_for(
        &self,
        id: &str,
        from: &str,
        to: Option<&str>,
    ) -> CatalogResult<()> {
        let id = id.to_owned();
        let from = from.to_owned();
        let to = to.map(str::to_owned);
        self.catalog(move |store| store.replace_volume_owner_for(&id, &from, to.as_deref()))
            .await
    }

    async fn publish_volume_backing(
        &self,
        _id: &str,
        path: &Path,
    ) -> CatalogResult<Vec<OverlaybdLayerRef>> {
        let root = self.root.clone();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || {
            PosixLayerStore::new(root).publish_volume_backing(&path)
        })
        .await
        .map_err(|e| CatalogError::backend("publish native POSIX layers", e))?
    }
    async fn materialize_volume_backing(
        &self,
        _id: &str,
        layers: &[OverlaybdLayerRef],
        destination: &Path,
    ) -> CatalogResult<PathBuf> {
        crate::materialize::materialize_volume_image_config(layers, destination, |layer| {
            overlaybd::config::LayerConfig {
                file: PosixLayerStore::new(self.root.clone())
                    .managed_layer_path(&layer.digest)
                    .to_string_lossy()
                    .into_owned(),
                digest: layer.digest.clone(),
                size: layer.size,
                uuid: layer.uuid.clone().unwrap_or_default(),
                ..Default::default()
            }
        })
        .await
    }
}
pub struct ObjectVolumeRepository<C: VolumeObjectStore + ObjectLayerTransport> {
    client: Arc<C>,
    managed_layers_url: String,
    compression: CompactOutput,
}
impl<C: VolumeObjectStore + ObjectLayerTransport> ObjectVolumeRepository<C> {
    pub fn new(client: Arc<C>, managed_layers_url: String, compression: CompactOutput) -> Self {
        Self {
            client,
            managed_layers_url,
            compression,
        }
    }
}
#[async_trait]
impl<C: VolumeObjectStore + ObjectLayerTransport> VolumeRepository for ObjectVolumeRepository<C> {
    async fn commit_expansion(
        &self,
        id: &str,
        owner: &str,
        expected_size_mb: u64,
        expected_layers: &[OverlaybdLayerRef],
        size_mb: u64,
        layers: &[OverlaybdLayerRef],
    ) -> CatalogResult<()> {
        ObjectVolumeCatalog::new(self.client.clone())
            .commit_expansion(
                id,
                owner,
                expected_size_mb,
                expected_layers,
                size_mb,
                layers,
            )
            .await
    }
    async fn get_volume(&self, reference: &str) -> CatalogResult<Option<VolumeRecord>> {
        ObjectVolumeCatalog::new(self.client.clone())
            .get_volume(reference)
            .await
    }
    async fn list_volumes_page(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> CatalogResult<VolumeRecordPage> {
        ObjectVolumeCatalog::new(self.client.clone())
            .list_volumes_page(after, limit)
            .await
    }
    async fn create_volume(&self, record: VolumeRecord) -> CatalogResult<()> {
        ObjectVolumeCatalog::new(self.client.clone())
            .create_volume(record)
            .await
    }
    async fn put_volume(&self, record: VolumeRecord) -> CatalogResult<()> {
        ObjectVolumeCatalog::new(self.client.clone())
            .put_volume(record)
            .await
    }
    async fn delete_volume(&self, id: &str) -> CatalogResult<()> {
        ObjectVolumeCatalog::new(self.client.clone())
            .delete_volume(id)
            .await
    }
    async fn reserve_volume(&self, id: &str, owner: &str) -> CatalogResult<Option<String>> {
        ObjectVolumeCatalog::new(self.client.clone())
            .reserve_volume(id, owner)
            .await
    }
    async fn reserve_read_only_volume(&self, id: &str, owner: &str) -> CatalogResult<()> {
        ObjectVolumeCatalog::new(self.client.clone())
            .reserve_read_only_volume(id, owner)
            .await
    }
    async fn replace_volume_owner_for(
        &self,
        id: &str,
        from: &str,
        to: Option<&str>,
    ) -> CatalogResult<()> {
        ObjectVolumeCatalog::new(self.client.clone())
            .replace_volume_owner_for(id, from, to)
            .await
    }

    async fn publish_volume_backing(
        &self,
        _id: &str,
        path: &Path,
    ) -> CatalogResult<Vec<OverlaybdLayerRef>> {
        ObjectLayerPublisher::new(self.client.as_ref(), self.compression)
            .derive_and_upload_disk_image_layers_mode(path, true, |_| false, &mut Vec::new())
            .await
    }
    async fn materialize_volume_backing(
        &self,
        _id: &str,
        layers: &[OverlaybdLayerRef],
        destination: &Path,
    ) -> CatalogResult<PathBuf> {
        crate::materialize::materialize_volume_image_config(layers, destination, |layer| {
            overlaybd::config::LayerConfig {
                repo_blob_url: self.managed_layers_url.clone(),
                digest: layer.digest.clone(),
                size: layer.size,
                uuid: layer.uuid.clone().unwrap_or_default(),
                ..Default::default()
            }
        })
        .await
    }
}
