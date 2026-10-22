//! Original OSS layer publication, independent of the native API and transport.
use crate::catalog::{CatalogError as RepositoryError, CatalogResult as RepositoryResult};
use crate::{
    compact::CompactOutput,
    materialize::write_dense_overlaybd_layer_to_file,
    recontainerize::{prepare_layer_upload, PreparedLayerUpload},
    ExternalLayer, ManagedLayer, OverlaybdLayerRef,
};
use async_trait::async_trait;
use overlaybd::{
    config::load_image_config as load_overlaybd_image_config, dense_export,
    layer_metadata::read_overlaybd_layer_uuid,
};
use std::path::Path;
#[async_trait]
pub trait ObjectLayerTransport: Send + Sync {
    async fn exists(&self, key: &str) -> anyhow::Result<bool>;
    async fn put_file(&self, key: &str, path: &Path) -> anyhow::Result<()>;
}
pub struct ObjectLayerPublisher<'a, T: ObjectLayerTransport> {
    transport: &'a T,
    publish_compression: CompactOutput,
}
impl<'a, T: ObjectLayerTransport> ObjectLayerPublisher<'a, T> {
    pub fn new(transport: &'a T, publish_compression: CompactOutput) -> Self {
        Self {
            transport,
            publish_compression,
        }
    }
    pub async fn derive_and_upload_disk_image_layers_mode(
        &self,
        image_config_path: &Path,
        allow_descriptorless: bool,
        is_runtime_delta: impl Fn(&Path) -> bool,
        local_layers: &mut Vec<PreparedLayerUpload>,
    ) -> RepositoryResult<Vec<OverlaybdLayerRef>> {
        let image_config = load_overlaybd_image_config(image_config_path).map_err(|e| {
            RepositoryError::backend(
                format!(
                    "load overlaybd image config '{}'",
                    image_config_path.display()
                ),
                e,
            )
        })?;

        let mut layers = Vec::with_capacity(image_config.lowers.len());

        for (index, layer) in image_config.lowers.into_iter().enumerate() {
            if !layer.file.is_empty() {
                let layer_path = Path::new(&layer.file);
                if !layer.digest.is_empty() && layer.size > 0 {
                    let managed = self
                        .import_managed_layer_with_descriptor(
                            layer_path,
                            &layer.digest,
                            layer.size,
                            local_layers,
                        )
                        .await?;
                    layers.push(OverlaybdLayerRef::Managed(managed));
                    continue;
                }
                if is_runtime_delta(layer_path) {
                    let managed = self
                        .import_descriptorless_rootfs_layer(layer_path, local_layers)
                        .await?;
                    layers.push(OverlaybdLayerRef::Managed(managed));
                    continue;
                }
                if allow_descriptorless {
                    let managed = self
                        .import_descriptorless_rootfs_layer(layer_path, local_layers)
                        .await?;
                    layers.push(OverlaybdLayerRef::Managed(managed));
                    continue;
                }
                return Err(RepositoryError::Unsupported {
                    feature: format!(
                        "local overlaybd lower layer {index} '{}' missing digest/size",
                        layer_path.display()
                    ),
                });
            }
            let repo_blob_url = layer
                .effective_repo_blob_url(&image_config.repo_blob_url)
                .to_string();
            if !repo_blob_url.is_empty() {
                let digest = if !layer.digest.is_empty() {
                    layer.digest
                } else if !layer.target_digest.is_empty() {
                    layer.target_digest
                } else {
                    format!("external:{index}")
                };
                layers.push(OverlaybdLayerRef::External(ExternalLayer {
                    digest,
                    repo_blob_url: repo_blob_url.clone(),
                    size: layer.size,
                }));
                continue;
            }
            return Err(RepositoryError::Unsupported {
                feature: format!("overlaybd lower layer {index} without local file or repoBlobUrl"),
            });
        }

        Ok(layers)
    }
    pub async fn import_descriptorless_rootfs_layer(
        &self,
        source: &Path,
        local_layers: &mut Vec<PreparedLayerUpload>,
    ) -> RepositoryResult<ManagedLayer> {
        let canonical = std::fs::canonicalize(source).map_err(|e| {
            RepositoryError::backend(
                format!("canonicalize managed layer '{}'", source.display()),
                e,
            )
        })?;
        if dense_export::should_dense_export_layer(&canonical) {
            return self
                .import_sparse_overlaybd_layer_dense(&canonical, local_layers)
                .await;
        }
        self.import_managed_layer_by_hash(&canonical, local_layers)
            .await
    }
    pub async fn import_sparse_overlaybd_layer_dense(
        &self,
        canonical: &Path,
        local_layers: &mut Vec<PreparedLayerUpload>,
    ) -> RepositoryResult<ManagedLayer> {
        let dense_temp = tempfile::NamedTempFile::new().map_err(|e| {
            RepositoryError::backend(
                format!(
                    "create temp dense overlaybd layer for '{}'",
                    canonical.display()
                ),
                e,
            )
        })?;
        let dense_path = dense_temp.path().to_path_buf();
        let descriptor = write_dense_overlaybd_layer_to_file(canonical, &dense_path)
            .await
            .map_err(|e| {
                RepositoryError::backend(
                    format!(
                        "dense-export sparse overlaybd layer '{}'",
                        canonical.display()
                    ),
                    e,
                )
            })?;
        let mut upload = prepare_layer_upload(
            &dense_path,
            self.publish_compression,
            Some((&descriptor.digest, descriptor.size)),
        )
        .await?;
        upload.retain_source(dense_temp);
        // Dense-exported layers never carry a layer uuid, recontainerized or not.
        self.upload_prepared_layer(upload, None, local_layers).await
    }
    pub async fn import_managed_layer_by_hash(
        &self,
        source: &Path,
        local_layers: &mut Vec<PreparedLayerUpload>,
    ) -> RepositoryResult<ManagedLayer> {
        let canonical = std::fs::canonicalize(source).map_err(|e| {
            RepositoryError::backend(
                format!("canonicalize managed layer '{}'", source.display()),
                e,
            )
        })?;
        let upload = prepare_layer_upload(&canonical, self.publish_compression, None).await?;
        let uuid = overlaybd_layer_uuid(upload.path());
        self.upload_prepared_layer(upload, uuid, local_layers).await
    }
    pub async fn import_managed_layer_with_descriptor(
        &self,
        source: &Path,
        digest: &str,
        size: u64,
        local_layers: &mut Vec<PreparedLayerUpload>,
    ) -> RepositoryResult<ManagedLayer> {
        let canonical = std::fs::canonicalize(source).map_err(|e| {
            RepositoryError::backend(
                format!("canonicalize managed layer '{}'", source.display()),
                e,
            )
        })?;
        let source_size = std::fs::metadata(&canonical)
            .map_err(|e| {
                RepositoryError::backend(
                    format!("read managed layer metadata '{}'", canonical.display()),
                    e,
                )
            })?
            .len();
        if source_size != size {
            return Err(RepositoryError::Backend {
                message: format!(
                    "managed layer descriptor size mismatch for '{}': descriptor says {}, file has {}",
                    canonical.display(),
                    size,
                    source_size
                ),
                source: None,
            });
        }

        // Descriptor-backed imports intentionally trust internally generated
        // content digests and only validate the cheap size invariant here.
        // Publish compression recontainerizes raw layers as zfile, which
        // changes the physical bytes, so `prepare_layer_upload` re-hashes the
        // compressed output instead of trusting this descriptor.
        let upload =
            prepare_layer_upload(&canonical, self.publish_compression, Some((digest, size)))
                .await?;
        let uuid = overlaybd_layer_uuid(upload.path());
        self.upload_prepared_layer(upload, uuid, local_layers).await
    }
    pub async fn upload_prepared_layer(
        &self,
        upload: PreparedLayerUpload,
        uuid: Option<String>,
        local_layers: &mut Vec<PreparedLayerUpload>,
    ) -> RepositoryResult<ManagedLayer> {
        let oss_key = format!("managed-layers/{}", upload.digest());
        upload_managed_layer_if_missing(self.transport, &oss_key, upload.path()).await?;
        let layer = ManagedLayer {
            digest: upload.digest().to_string(),
            size: upload.size(),
            uuid,
        };
        local_layers.push(upload);
        Ok(layer)
    }
}
fn overlaybd_layer_uuid(source: &Path) -> Option<String> {
    read_overlaybd_layer_uuid(source)
        .ok()
        .filter(|uuid| !uuid.is_nil())
        .map(|uuid| uuid.to_string())
}
async fn upload_managed_layer_if_missing<T: ObjectLayerTransport>(
    client: &T,
    key: &str,
    canonical: &Path,
) -> RepositoryResult<()> {
    let already_exists = client
        .exists(key)
        .await
        .map_err(|e| RepositoryError::backend("check managed layer existence", e))?;

    if !already_exists {
        client.put_file(key, canonical).await.map_err(|e| {
            RepositoryError::backend(format!("upload managed layer '{}'", canonical.display()), e)
        })?;
    }

    Ok(())
}
