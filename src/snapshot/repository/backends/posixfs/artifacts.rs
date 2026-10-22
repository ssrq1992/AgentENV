use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::layout::PosixFsSnapshotArtifactLayout;
use crate::digest::{self, FileDigest};
use crate::sandbox::SandboxSnapshotManifest;
use crate::snapshot::{
    CommittedAttachedDrive, ManagedLayer, OverlaybdLayerRef, RepositoryError, RepositoryResult,
    SnapshotId, SNAPSHOT_ARTIFACT_LAYOUT,
};

/// Artifact store backed by files in a POSIX-compatible shared filesystem.
pub struct PosixFsArtifactStore {
    root: PathBuf,
}

impl PosixFsArtifactStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub(crate) fn managed_layer_path(&self, digest: &str) -> PathBuf {
        PosixFsSnapshotArtifactLayout::managed_layer_path(&self.root, digest)
    }

    fn committed_layout(&self, snapshot_id: &SnapshotId) -> PosixFsSnapshotArtifactLayout {
        PosixFsSnapshotArtifactLayout::new(&self.root, snapshot_id)
    }

    pub(crate) fn publish_volume_backing(
        &self,
        image_config_path: &Path,
    ) -> RepositoryResult<Vec<OverlaybdLayerRef>> {
        self.derive_rootfs_layers(image_config_path, true)
    }

    /// Imports manager-owned local build artifacts into committed repository storage.
    ///
    /// Flow:
    /// 1. copy committed fixed-layout runtime files into the snapshot directory
    ///    (currently only `vm_state.bin`)
    /// 2. derive committed logical layer manifests from the local build-time image configs
    /// 3. import any managed lower layers into the shared managed-layer store
    /// 4. convert attached-drive publish input into committed attached-drive metadata
    ///
    /// Note that committed snapshot metadata intentionally does not persist the
    /// build-time `image.json` files. Runtime resolvers regenerate those files
    /// from the committed manifest plus backend layout conventions.
    pub(crate) fn import_built_artifacts(
        &self,
        snapshot_id: &SnapshotId,
        manifest: &SandboxSnapshotManifest,
    ) -> RepositoryResult<CollectedBuiltArtifacts> {
        let committed_layout = self.committed_layout(snapshot_id);

        self.copy_local_artifact(
            committed_layout.path(SNAPSHOT_ARTIFACT_LAYOUT.vm_state),
            &manifest.vm_state.path,
        )?;
        self.persist_firecracker_manifest(
            committed_layout.path(SNAPSHOT_ARTIFACT_LAYOUT.firecracker_manifest),
            manifest,
        )?;
        let memory_layers = self.derive_memory_layers(&manifest.memory.image_config_path)?;

        let attached_drives = manifest
            .attached_drives
            .iter()
            .map(|drive| {
                // The committed attached-drive manifest stores logical layers only.
                // The build-time image config is used here as an input for deriving
                // those layers, but is not copied into the committed repository.
                let rootfs_layers = self.derive_rootfs_layers(&drive.image_config_path, false)?;
                Ok(CommittedAttachedDrive::Overlaybd {
                    drive_id: drive.drive_id.clone(),
                    layers: rootfs_layers,
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
                })
            })
            .collect::<RepositoryResult<Vec<_>>>()?;

        let rootfs_layers = self.derive_rootfs_layers(&manifest.rootfs.image_config_path, false)?;

        Ok(CollectedBuiltArtifacts {
            rootfs_layers,
            memory_layers,
            attached_drives,
        })
    }

    fn persist_firecracker_manifest(
        &self,
        destination: PathBuf,
        manifest: &crate::sandbox::SandboxSnapshotManifest,
    ) -> RepositoryResult<()> {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                RepositoryError::backend(
                    format!("create firecracker manifest dir '{}'", parent.display()),
                    error,
                )
            })?;
        }
        let bytes = serde_json::to_vec_pretty(manifest)
            .map_err(|error| RepositoryError::backend("serialize firecracker manifest", error))?;
        fs::write(&destination, bytes).map_err(|error| {
            RepositoryError::backend(
                format!("write firecracker manifest '{}'", destination.display()),
                error,
            )
        })?;
        Ok(())
    }

    fn copy_local_artifact(&self, destination: PathBuf, source: &Path) -> RepositoryResult<()> {
        let source_metadata = fs::metadata(source).map_err(|error| {
            RepositoryError::backend(
                format!("read source artifact metadata '{}'", source.display()),
                error,
            )
        })?;

        let Some(parent) = destination.parent() else {
            return Err(RepositoryError::Backend {
                message: format!(
                    "resolve parent dir for artifact '{}'",
                    destination.display()
                ),
                source: None,
            });
        };
        fs::create_dir_all(parent).map_err(|error| {
            RepositoryError::backend(format!("create artifact dir '{}'", parent.display()), error)
        })?;
        if !same_file(&source_metadata, &destination)? {
            hard_link_or_copy_file_with_sha256(source, &destination)?;
            sync_dir(parent)?;
        }
        let destination_metadata = fs::metadata(&destination).map_err(|error| {
            RepositoryError::backend(
                format!("read artifact metadata '{}'", destination.display()),
                error,
            )
        })?;
        if destination_metadata.len() != source_metadata.len() {
            return Err(RepositoryError::Backend {
                message: format!(
                    "copied artifact size mismatch for '{}' -> '{}': expected {} bytes, found {} bytes",
                    source.display(),
                    destination.display(),
                    source_metadata.len(),
                    destination_metadata.len()
                ),
                source: None,
            });
        }
        Ok(())
    }

    fn derive_rootfs_layers(
        &self,
        image_config_path: &Path,
        allow_descriptorless: bool,
    ) -> RepositoryResult<Vec<OverlaybdLayerRef>> {
        aenv_volume_core::layers::PosixLayerStore::new(self.root.clone())
            .derive_rootfs_layers(
                image_config_path,
                allow_descriptorless,
                crate::image::local_layer::rootfs_layer_is_runtime_generated_delta,
            )
            .map_err(Into::into)
    }

    /// Derive memory layers from a committed memory image config.
    ///
    /// Each lower with a local `file` path is imported as a managed layer,
    /// exactly like `derive_rootfs_layout` does for disk layers.
    fn derive_memory_layers(
        &self,
        mem_image_config_path: &Path,
    ) -> RepositoryResult<Vec<ManagedLayer>> {
        aenv_volume_core::layers::PosixLayerStore::new(self.root.clone())
            .derive_memory_layers(mem_image_config_path)
            .map_err(Into::into)
    }
}

fn same_file(source_metadata: &fs::Metadata, destination: &Path) -> RepositoryResult<bool> {
    match fs::metadata(destination) {
        Ok(destination_metadata) => Ok(source_metadata.dev() == destination_metadata.dev()
            && source_metadata.ino() == destination_metadata.ino()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(RepositoryError::backend(
            format!(
                "read destination artifact metadata '{}'",
                destination.display()
            ),
            error,
        )),
    }
}

fn hard_link_or_copy_file_with_sha256(source: &Path, destination: &Path) -> RepositoryResult<()> {
    match fs::hard_link(source, destination) {
        Ok(()) => {
            finalize_hard_linked_file(destination)?;
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => {}
    }
    copy_file_with_sha256(source, destination)
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

fn copy_file_with_sha256(source: &Path, destination: &Path) -> RepositoryResult<()> {
    // We compute the source digest while copying, then re-read the
    // destination and compare digests after sync_all(). The second pass is
    // intentional: it verifies the persisted bytes rather than only trusting
    // the write loop's in-memory byte stream.
    let mut source_file = fs::File::open(source).map_err(|error| {
        RepositoryError::backend(
            format!("open source artifact '{}'", source.display()),
            error,
        )
    })?;
    let mut destination_file = fs::File::create(destination).map_err(|error| {
        RepositoryError::backend(
            format!("create destination artifact '{}'", destination.display()),
            error,
        )
    })?;
    let (source_digest_hex, copied) =
        digest::copy_with_sha256_hex(&mut source_file, &mut destination_file).map_err(|error| {
            RepositoryError::backend(
                format!(
                    "copy source artifact '{}' to destination artifact '{}'",
                    source.display(),
                    destination.display()
                ),
                error,
            )
        })?;
    let source_digest = FileDigest {
        size: copied,
        sha256: format!("sha256:{source_digest_hex}"),
    };

    destination_file.sync_all().map_err(|error| {
        RepositoryError::backend(
            format!("sync destination artifact '{}'", destination.display()),
            error,
        )
    })?;

    let destination_digest = FileDigest::describe_blocking(destination).map_err(|error| {
        RepositoryError::backend(
            format!("describe destination artifact '{}'", destination.display()),
            error,
        )
    })?;
    if destination_digest != source_digest {
        return Err(RepositoryError::Backend {
            message: format!(
                "copied artifact digest mismatch for '{}' -> '{}': expected {:?}, found {:?}",
                source.display(),
                destination.display(),
                source_digest,
                destination_digest
            ),
            source: None,
        });
    }

    Ok(())
}

#[derive(Debug)]
pub(crate) struct CollectedBuiltArtifacts {
    pub(crate) rootfs_layers: Vec<OverlaybdLayerRef>,
    pub(crate) memory_layers: Vec<ManagedLayer>,
    pub(crate) attached_drives: Vec<CommittedAttachedDrive>,
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;
    use std::sync::Arc;

    use overlaybd::backend::local::LocalFile;
    use overlaybd::index_file::{CommitArgs, LSMTFile};
    use overlaybd::virtual_file::VirtualFile;
    use overlaybd::zfile::{is_zfile, CompressArgs, CompressOptions, ZFileCompactWriter};
    use serde_json::json;
    use tempfile::TempDir;

    use super::super::layout::{managed_layer_file_name, PosixFsSnapshotArtifactLayout};
    use super::PosixFsArtifactStore;
    use crate::digest::FileDigest;
    use crate::snapshot::mock::write_mock_built_artifacts;
    use crate::snapshot::{
        CommittedAttachedDrive, ExternalLayer, OverlaybdLayerRef, SnapshotId,
        SNAPSHOT_ARTIFACT_LAYOUT,
    };

    fn test_store(root: &std::path::Path) -> PosixFsArtifactStore {
        PosixFsArtifactStore::new(root.to_path_buf())
    }

    fn assert_same_inode(left: &std::path::Path, right: &std::path::Path) {
        let left_metadata = fs::metadata(left).expect("left metadata");
        let right_metadata = fs::metadata(right).expect("right metadata");
        assert_eq!(left_metadata.dev(), right_metadata.dev());
        assert_eq!(left_metadata.ino(), right_metadata.ino());
    }

    #[test]
    fn derives_external_layers_from_layer_repo_blob_urls() {
        let tempdir = TempDir::new().expect("tempdir");
        let store = test_store(tempdir.path());
        let image = tempdir.path().join("image.json");
        fs::write(
            &image,
            serde_json::to_vec_pretty(&json!({
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
                "resultFile": ""
            }))
            .expect("serialize image config"),
        )
        .expect("write image config");

        let layers = store
            .derive_rootfs_layers(&image, false)
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

    #[test]
    fn attached_drive_rootfs_layers_are_derived_from_resolved_lower_paths() {
        let tempdir = TempDir::new().expect("tempdir");
        let store = test_store(tempdir.path());
        let snapshot_id = SnapshotId::generate();
        let local_dir = tempdir.path().join("local");
        let (_, _, mut manifest) = write_mock_built_artifacts(local_dir.as_path())
            .expect("mock built artifacts should write");

        // Create drive directory structure: local_dir/drives/data/
        let drive_dir = local_dir.join("drives").join("data");
        fs::create_dir_all(&drive_dir).expect("create drive dir");

        // Create layers dir through the layout API used by drive image configs.
        let layers_dir = local_dir.join(SNAPSHOT_ARTIFACT_LAYOUT.drive_layers_dir);
        fs::create_dir_all(&layers_dir).expect("create layers dir");
        let drive_layer = layers_dir.join("drive.commit");
        fs::write(&drive_layer, b"drive-layer").expect("drive lower");
        let drive_descriptor =
            FileDigest::describe_blocking(&drive_layer).expect("describe drive lower");

        // Write drive image.json with relative path to layers
        fs::write(
            drive_dir.join(SNAPSHOT_ARTIFACT_LAYOUT.overlaybd_image_config_file),
            format!(
                r#"{{
  "repoBlobUrl": "",
  "lowers": [{{ "file": "../layers/drive.commit", "digest": "{}", "size": {} }}],
  "upper": {{}},
  "resultFile": ""
}}"#,
                drive_descriptor.sha256, drive_descriptor.size
            ),
        )
        .expect("write drive image config");

        manifest = manifest
            .with_extra_drives(&[crate::sandbox::ExtraDrive::Overlaybd {
                drive_id: "data".to_string(),
                image_config_path: local_dir
                    .join(SNAPSHOT_ARTIFACT_LAYOUT.drives_dir)
                    .join("data")
                    .join(SNAPSHOT_ARTIFACT_LAYOUT.overlaybd_image_config_file),
                read_only: true,
                mount_path: crate::sandbox::ExtraDrive::default_mount_path("data"),
                virtual_size: Some(4096),
                sub_path: None,
                snapshot_output_dir: None,
                volume: false,
            }])
            .expect("attached drive manifest should include virtual size");

        let built = store
            .import_built_artifacts(&snapshot_id, &manifest)
            .expect("import built artifacts");

        let path = match &built.attached_drives[0] {
            CommittedAttachedDrive::Overlaybd { layers, .. } => {
                assert_eq!(layers.len(), 1);
                match &layers[0] {
                    crate::snapshot::OverlaybdLayerRef::Managed(layer) => {
                        PathBuf::from("managed-layers")
                            .join(managed_layer_file_name(&layer.digest))
                            .display()
                            .to_string()
                    }
                    crate::snapshot::OverlaybdLayerRef::External(_) => {
                        panic!("expected managed rootfs layer")
                    }
                }
            }
        };
        assert!(path.contains("managed-layers"));
        assert!(tempdir.path().join(&path).exists());
    }

    #[test]
    fn import_built_artifacts_extracts_rootfs_layers_from_image_config() {
        let tempdir = TempDir::new().expect("tempdir");
        let store = test_store(tempdir.path());
        let snapshot_id = SnapshotId::generate();
        let local_dir = tempdir.path().join("local");
        let (_, _, manifest) = write_mock_built_artifacts(local_dir.as_path())
            .expect("mock built artifacts should write");
        let built = store
            .import_built_artifacts(&snapshot_id, &manifest)
            .expect("collect built artifacts");

        assert_eq!(built.rootfs_layers.len(), 1);
        match &built.rootfs_layers[0] {
            crate::snapshot::OverlaybdLayerRef::Managed(layer) => {
                let path =
                    PathBuf::from("managed-layers").join(managed_layer_file_name(&layer.digest));
                assert!(path.display().to_string().contains("managed-layers"));
                assert!(tempdir.path().join(path).exists());
            }
            crate::snapshot::OverlaybdLayerRef::External(_) => {
                panic!("expected managed rootfs layer")
            }
        }
    }

    #[test]
    fn import_built_artifacts_hard_links_local_publish_artifacts_when_possible() {
        let tempdir = TempDir::new().expect("tempdir");
        let store = test_store(tempdir.path());
        let snapshot_id = SnapshotId::generate();
        let local_dir = tempdir.path().join("local");
        let (rootfs_lower, memory_lower, manifest) =
            write_mock_built_artifacts(local_dir.as_path())
                .expect("mock built artifacts should write");

        let built = store
            .import_built_artifacts(&snapshot_id, &manifest)
            .expect("collect built artifacts");

        let committed_layout = PosixFsSnapshotArtifactLayout::new(tempdir.path(), &snapshot_id);
        assert_same_inode(
            &manifest.vm_state.path,
            &committed_layout.path(SNAPSHOT_ARTIFACT_LAYOUT.vm_state),
        );

        match &built.rootfs_layers[0] {
            crate::snapshot::OverlaybdLayerRef::Managed(layer) => {
                assert_same_inode(
                    &rootfs_lower,
                    &tempdir
                        .path()
                        .join("managed-layers")
                        .join(managed_layer_file_name(&layer.digest)),
                );
            }
            crate::snapshot::OverlaybdLayerRef::External(_) => {
                panic!("expected managed rootfs layer")
            }
        }
        assert_same_inode(
            &memory_lower,
            &tempdir
                .path()
                .join("managed-layers")
                .join(managed_layer_file_name(&built.memory_layers[0].digest)),
        );
    }

    #[test]
    fn import_built_artifacts_uses_declared_local_layer_descriptor() {
        let tempdir = TempDir::new().expect("tempdir");
        let store = test_store(tempdir.path());
        let snapshot_id = SnapshotId::generate();
        let local_dir = tempdir.path().join("local");
        let (rootfs_lower, _, manifest) = write_mock_built_artifacts(local_dir.as_path())
            .expect("mock built artifacts should write");
        // Deliberately not the content digest: this verifies that descriptor
        // backed import trusts the descriptor instead of re-hashing the file.
        let declared_digest = "sha256:declared-rootfs";
        fs::write(
            &manifest.rootfs.image_config_path,
            format!(
                r#"{{
  "repoBlobUrl": "",
  "lowers": [{{ "file": "{}", "digest": "{}", "size": 10 }}],
  "upper": {{}},
  "resultFile": ""
}}"#,
                rootfs_lower.display(),
                declared_digest
            ),
        )
        .expect("write descriptor rootfs image config");

        let built = store
            .import_built_artifacts(&snapshot_id, &manifest)
            .expect("collect built artifacts");

        match &built.rootfs_layers[0] {
            crate::snapshot::OverlaybdLayerRef::Managed(layer) => {
                assert_eq!(layer.digest, declared_digest);
                assert_eq!(layer.size, 10);
                let path =
                    PathBuf::from("managed-layers").join(managed_layer_file_name(&layer.digest));
                assert!(tempdir.path().join(path).exists());
            }
            crate::snapshot::OverlaybdLayerRef::External(_) => {
                panic!("expected managed rootfs layer")
            }
        }
    }

    #[test]
    fn import_built_artifacts_keeps_mem_image_config_local_only() {
        let tempdir = TempDir::new().expect("tempdir");
        let store = test_store(tempdir.path());
        let snapshot_id = SnapshotId::generate();
        let local_dir = tempdir.path().join("local");
        let (_, _, manifest) = write_mock_built_artifacts(local_dir.as_path())
            .expect("mock built artifacts should write");
        let built = store
            .import_built_artifacts(&snapshot_id, &manifest)
            .expect("collect built artifacts");

        assert_eq!(built.memory_layers.len(), 1);
        let committed_layout = PosixFsSnapshotArtifactLayout::new(tempdir.path(), &snapshot_id);
        let persisted_manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(committed_layout.path(SNAPSHOT_ARTIFACT_LAYOUT.firecracker_manifest))
                .expect("read committed firecracker manifest"),
        )
        .expect("parse committed firecracker manifest");
        assert!(persisted_manifest["vmState"].get("path").is_none());
        assert!(persisted_manifest["memory"]
            .get("imageConfigPath")
            .is_none());
        assert!(persisted_manifest["rootfs"]
            .get("imageConfigPath")
            .is_none());
        assert!(
            !committed_layout
                .path(SNAPSHOT_ARTIFACT_LAYOUT.memory_image_config)
                .exists(),
            "committed snapshot dir should not persist mem_image.json"
        );
    }

    /// Write a real ZFile-compressed sealed LSMT layer, mirroring a compressed
    /// memory lower as found in repositories published while capture-time
    /// compression existed (or produced by publish-time compression).
    async fn write_zfile_lsmt_layer(path: &std::path::Path) {
        let data =
            Arc::new(LocalFile::new(path.with_extension("data")).expect("create layer data file"));
        let index = Arc::new(
            LocalFile::new(path.with_extension("index")).expect("create layer index file"),
        );
        let layer = LSMTFile::create(data, Some(index), 2 * 4096, false)
            .await
            .expect("create layer");
        layer
            .write_at(0, &[0x5A; 4096])
            .await
            .expect("write layer page 0");
        layer
            .write_at(4096, &[0xA5; 4096])
            .await
            .expect("write layer page 1");
        let output = Arc::new(LocalFile::new(path).expect("create zfile layer output"));
        let compress_args = CompressArgs::new(CompressOptions::new(
            CompressOptions::LZ4,
            CompressOptions::DEFAULT_BLOCK_SIZE,
            0,
        ));
        let writer = Arc::new(
            ZFileCompactWriter::new(output, &compress_args)
                .await
                .expect("create zfile compact writer"),
        );
        layer
            .commit_with_args(CommitArgs::from_writer(writer))
            .await
            .expect("commit zfile layer");
    }

    #[tokio::test]
    async fn import_built_artifacts_preserves_zfile_memory_lower_bytes() {
        let tempdir = TempDir::new().expect("tempdir");
        let store = test_store(tempdir.path());
        let snapshot_id = SnapshotId::generate();
        let local_dir = tempdir.path().join("local");
        let (_, _, manifest) = write_mock_built_artifacts(local_dir.as_path())
            .expect("mock built artifacts should write");

        // Replace the mock memory lower with a real ZFile layer. Production
        // mem image configs carry no digest/size for the freshly written
        // lower, so the import hashes the physical file.
        let zfile_lower = local_dir.join("mem.zfile.commit");
        write_zfile_lsmt_layer(&zfile_lower).await;
        let zfile_descriptor =
            FileDigest::describe_blocking(&zfile_lower).expect("describe zfile lower");
        fs::write(
            &manifest.memory.image_config_path,
            format!(r#"{{"lowers":[{{"file":"{}"}}]}}"#, zfile_lower.display()),
        )
        .expect("write zfile memory image config");

        let built = store
            .import_built_artifacts(&snapshot_id, &manifest)
            .expect("import built artifacts");

        assert_eq!(built.memory_layers.len(), 1);
        let managed = &built.memory_layers[0];
        assert_eq!(managed.digest, zfile_descriptor.sha256);
        assert_eq!(managed.size, zfile_descriptor.size);
        // ZFile wraps the raw LSMT header, so no layer UUID is exposed.
        assert!(managed.uuid.is_none());

        let managed_path = tempdir
            .path()
            .join("managed-layers")
            .join(managed_layer_file_name(&managed.digest));
        assert_eq!(
            fs::read(&managed_path).expect("read managed layer"),
            fs::read(&zfile_lower).expect("read source zfile lower"),
            "managed layer must preserve the physical ZFile bytes"
        );
        assert_eq!(
            fs::metadata(&managed_path)
                .expect("managed layer metadata")
                .len(),
            fs::metadata(&zfile_lower)
                .expect("source zfile metadata")
                .len()
        );
        // The stored bytes still probe as a ZFile with the source algorithm.
        let managed_file = Arc::new(LocalFile::open_ro(&managed_path).expect("open managed layer"));
        assert_eq!(
            is_zfile(managed_file).await.expect("probe managed layer"),
            1
        );
    }
}
