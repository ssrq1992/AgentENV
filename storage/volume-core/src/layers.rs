//! Layer publication reused by native snapshots and the CSI volume backend.
use crate::catalog::{CatalogError as RepositoryError, CatalogResult as RepositoryResult};
use crate::materialize::write_dense_overlaybd_layer_to_file_blocking;
use crate::{digest::FileDigest, posix::VolumeLayout, ManagedLayer, OverlaybdLayerRef};
use overlaybd::{
    config::load_image_config as load_overlaybd_image_config, dense_export,
    layer_metadata::read_overlaybd_layer_uuid,
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;
pub struct PosixLayerStore {
    root: PathBuf,
}
impl PosixLayerStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    pub fn managed_layer_path(&self, digest: &str) -> PathBuf {
        VolumeLayout::managed_layer_path(&self.root, digest)
    }
    pub fn publish_volume_backing(&self, path: &Path) -> RepositoryResult<Vec<OverlaybdLayerRef>> {
        self.derive_rootfs_layers(path, true, |_| false)
    }
    pub fn store_managed_layer(
        &self,
        source: &Path,
        digest: &str,
        size: u64,
        uuid: Option<String>,
    ) -> RepositoryResult<crate::ManagedLayer> {
        let destination = VolumeLayout::managed_layer_path(&self.root, digest);
        if destination.exists() {
            let destination_size = fs::metadata(&destination)
                .map_err(|error| {
                    RepositoryError::backend(
                        format!("read managed layer metadata '{}'", destination.display()),
                        error,
                    )
                })?
                .len();
            if destination_size != size {
                return Err(RepositoryError::Backend {
                    message: format!(
                        "managed layer '{}' size mismatch: descriptor says {}, existing file has {}",
                        digest, size, destination_size
                    ),
                    source: None,
                });
            }
            return Ok(crate::ManagedLayer {
                digest: digest.to_string(),
                size,
                uuid,
            });
        }

        let Some(parent) = destination.parent() else {
            return Err(RepositoryError::Backend {
                message: format!(
                    "resolve parent dir for managed layer '{}'",
                    destination.display()
                ),
                source: None,
            });
        };
        fs::create_dir_all(parent).map_err(|error| {
            RepositoryError::backend(
                format!("create managed layer dir '{}'", parent.display()),
                error,
            )
        })?;
        // Fast path for expected immutable overlaybd lower layers. This is
        // safe for layers produced by AgentENV/image-cache/snapshot restack
        // because they are sealed or content-addressed after publication.
        hard_link_or_copy_managed_layer(source, &destination, parent)?;
        sync_dir(parent)?;

        Ok(crate::ManagedLayer {
            digest: digest.to_string(),
            size,
            uuid,
        })
    }

    pub fn store_sparse_overlaybd_layer_dense(
        &self,
        source: &Path,
    ) -> RepositoryResult<crate::ManagedLayer> {
        let destination_parent = VolumeLayout::managed_layers_dir(&self.root);
        fs::create_dir_all(&destination_parent).map_err(|error| {
            RepositoryError::backend(
                format!(
                    "create managed layer dir '{}'",
                    destination_parent.display()
                ),
                error,
            )
        })?;

        let temp = NamedTempFile::new_in(&destination_parent).map_err(|error| {
            RepositoryError::backend(
                format!(
                    "create temp dense managed layer in '{}'",
                    destination_parent.display()
                ),
                error,
            )
        })?;
        let temp_path = temp.path().to_path_buf();
        let descriptor =
            write_dense_overlaybd_layer_to_file_blocking(source, &temp_path).map_err(|error| {
                RepositoryError::backend(
                    format!("dense-export sparse overlaybd layer '{}'", source.display()),
                    error,
                )
            })?;

        let destination = VolumeLayout::managed_layer_path(&self.root, &descriptor.digest);
        if destination.exists() {
            let destination_size = fs::metadata(&destination)
                .map_err(|error| {
                    RepositoryError::backend(
                        format!("read managed layer metadata '{}'", destination.display()),
                        error,
                    )
                })?
                .len();
            if destination_size != descriptor.size {
                return Err(RepositoryError::Backend {
                    message: format!(
                        "managed layer '{}' size mismatch: descriptor says {}, existing file has {}",
                        descriptor.digest, descriptor.size, destination_size
                    ),
                    source: None,
                });
            }
            return Ok(crate::ManagedLayer {
                digest: descriptor.digest,
                size: descriptor.size,
                uuid: None,
            });
        }

        temp.as_file().sync_all().map_err(|error| {
            RepositoryError::backend(
                format!("sync temp managed layer '{}'", temp_path.display()),
                error,
            )
        })?;
        match temp.persist_noclobber(&destination) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(RepositoryError::backend(
                    format!(
                        "persist dense managed layer temp '{}' -> '{}'",
                        error.file.path().display(),
                        destination.display()
                    ),
                    error.error,
                ));
            }
        }

        Ok(crate::ManagedLayer {
            digest: descriptor.digest,
            size: descriptor.size,
            uuid: None,
        })
    }

    pub fn import_managed_layer_with_descriptor(
        &self,
        source: &Path,
        digest: &str,
        size: u64,
    ) -> RepositoryResult<crate::ManagedLayer> {
        let canonical = fs::canonicalize(source).map_err(|error| {
            RepositoryError::backend(
                format!("canonicalize managed layer source '{}'", source.display()),
                error,
            )
        })?;
        let source_size = fs::metadata(&canonical)
            .map_err(|error| {
                RepositoryError::backend(
                    format!(
                        "read managed layer source metadata '{}'",
                        canonical.display()
                    ),
                    error,
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
        let managed =
            self.store_managed_layer(&canonical, digest, size, overlaybd_layer_uuid(&canonical))?;
        Ok(managed)
    }

    pub fn import_managed_layer_by_hash(
        &self,
        source: &Path,
    ) -> RepositoryResult<crate::ManagedLayer> {
        let canonical = fs::canonicalize(source).map_err(|error| {
            RepositoryError::backend(
                format!("canonicalize managed layer source '{}'", source.display()),
                error,
            )
        })?;
        let descriptor = FileDigest::describe_blocking(&canonical).map_err(|error| {
            RepositoryError::backend(
                format!("describe managed layer source '{}'", canonical.display()),
                error,
            )
        })?;
        let managed = self.store_managed_layer(
            &canonical,
            &descriptor.sha256,
            descriptor.size,
            overlaybd_layer_uuid(&canonical),
        )?;
        Ok(managed)
    }

    pub fn import_descriptorless_rootfs_layer(
        &self,
        source: &Path,
    ) -> RepositoryResult<crate::ManagedLayer> {
        let canonical = fs::canonicalize(source).map_err(|error| {
            RepositoryError::backend(
                format!("canonicalize managed layer source '{}'", source.display()),
                error,
            )
        })?;
        if dense_export::should_dense_export_layer(&canonical) {
            return self.store_sparse_overlaybd_layer_dense(&canonical);
        }
        self.import_managed_layer_by_hash(&canonical)
    }

    pub fn derive_rootfs_layers(
        &self,
        image_config_path: &Path,
        allow_descriptorless: bool,
        is_runtime_delta: impl Fn(&Path) -> bool,
    ) -> RepositoryResult<Vec<OverlaybdLayerRef>> {
        let image_config = load_overlaybd_image_config(image_config_path).map_err(|error| {
            RepositoryError::backend(
                format!(
                    "load overlaybd image config for rootfs layout '{}'",
                    image_config_path.display()
                ),
                error,
            )
        })?;

        let layers = image_config
            .lowers
            .into_iter()
            .enumerate()
            .map(|(index, layer)| {
                if !layer.file.is_empty() {
                    let layer_path = Path::new(&layer.file);
                    if !layer.digest.is_empty() && layer.size > 0 {
                        return self
                            .import_managed_layer_with_descriptor(
                                layer_path,
                                &layer.digest,
                                layer.size,
                            )
                            .map(OverlaybdLayerRef::Managed);
                    }
                    if allow_descriptorless || is_runtime_delta(layer_path) {
                        return self
                            .import_descriptorless_rootfs_layer(layer_path)
                            .map(OverlaybdLayerRef::Managed);
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
                    return Ok(OverlaybdLayerRef::External(crate::ExternalLayer {
                        digest: if !layer.digest.is_empty() {
                            layer.digest
                        } else if !layer.target_digest.is_empty() {
                            layer.target_digest
                        } else {
                            format!("external:{index}")
                        },
                        repo_blob_url: repo_blob_url.clone(),
                        size: layer.size,
                    }));
                }

                Err(RepositoryError::Unsupported {
                    feature: format!(
                        "overlaybd lower layer {index} without local file or repoBlobUrl"
                    ),
                })
            })
            .collect::<RepositoryResult<Vec<_>>>()?;

        Ok(layers)
    }

    pub fn derive_memory_layers(
        &self,
        mem_image_config_path: &Path,
    ) -> RepositoryResult<Vec<ManagedLayer>> {
        let image_config = load_overlaybd_image_config(mem_image_config_path).map_err(|error| {
            RepositoryError::backend(
                format!(
                    "load mem image config for memory layers '{}'",
                    mem_image_config_path.display()
                ),
                error,
            )
        })?;

        image_config
            .lowers
            .into_iter()
            .enumerate()
            .map(|(index, layer)| {
                if layer.file.is_empty() {
                    return Err(RepositoryError::Unsupported {
                        feature: format!("memory layer {index} without local file path"),
                    });
                }
                let layer_path = Path::new(&layer.file);
                if !layer.digest.is_empty() && layer.size > 0 {
                    return self.import_managed_layer_with_descriptor(
                        layer_path,
                        &layer.digest,
                        layer.size,
                    );
                }
                self.import_managed_layer_by_hash(layer_path)
            })
            .collect()
    }
}
fn overlaybd_layer_uuid(source: &Path) -> Option<String> {
    read_overlaybd_layer_uuid(source)
        .ok()
        .filter(|uuid| !uuid.is_nil())
        .map(|uuid| uuid.to_string())
}

fn hard_link_or_copy_managed_layer(
    source: &Path,
    destination: &Path,
    destination_parent: &Path,
) -> RepositoryResult<()> {
    match fs::hard_link(source, destination) {
        Ok(()) => {
            finalize_hard_linked_file(destination)?;
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Ok(());
        }
        Err(_) => {}
    }

    let temp = NamedTempFile::new_in(destination_parent).map_err(|error| {
        RepositoryError::backend(
            format!(
                "create temp managed layer next to '{}'",
                destination.display()
            ),
            error,
        )
    })?;
    fs::copy(source, temp.path()).map_err(|error| {
        RepositoryError::backend(
            format!(
                "copy managed layer from '{}' to temp '{}'",
                source.display(),
                temp.path().display()
            ),
            error,
        )
    })?;
    temp.as_file().sync_all().map_err(|error| {
        RepositoryError::backend(
            format!("sync temp managed layer '{}'", temp.path().display()),
            error,
        )
    })?;
    match temp.persist_noclobber(destination) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(RepositoryError::backend(
            format!(
                "persist managed layer temp '{}' -> '{}'",
                error.file.path().display(),
                destination.display()
            ),
            error.error,
        )),
    }
}

fn finalize_hard_linked_file(path: &Path) -> RepositoryResult<()> {
    chmod_read_only(path)?;
    fs::File::open(path)
        .map_err(|error| {
            RepositoryError::backend(format!("open hard-linked file '{}'", path.display()), error)
        })?
        .sync_all()
        .map_err(|error| {
            RepositoryError::backend(format!("sync hard-linked file '{}'", path.display()), error)
        })?;
    Ok(())
}

fn sync_dir(path: &Path) -> RepositoryResult<()> {
    fs::File::open(path)
        .map_err(|error| RepositoryError::backend(format!("open '{}'", path.display()), error))?
        .sync_all()
        .map_err(|error| RepositoryError::backend(format!("sync '{}'", path.display()), error))?;
    Ok(())
}

fn chmod_read_only(path: &Path) -> RepositoryResult<()> {
    let mut permissions = fs::metadata(path)
        .map_err(|error| {
            RepositoryError::backend(format!("read metadata of '{}'", path.display()), error)
        })?
        .permissions();
    permissions.set_mode(permissions.mode() & !0o222);
    fs::set_permissions(path, permissions).map_err(|error| {
        RepositoryError::backend(format!("chmod '{}' to read-only", path.display()), error)
    })
}
