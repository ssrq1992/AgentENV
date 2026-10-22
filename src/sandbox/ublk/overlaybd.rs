use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
#[cfg(test)]
use overlaybd::backend::local::LocalFile;
use overlaybd::config::{LayerConfig, UpperMode};
use overlaybd::index_file::CommitArgs;
use overlaybd::virtual_file::VirtualFile;
#[cfg(test)]
use overlaybd::zfile::{CompressArgs, CompressOptions, ZFileCompactWriter};
use serde::{Deserialize, Serialize};

use super::device::UblkDevice;
use crate::cfg::{OverlaybdCompressionAlgorithm, SnapshotPublishCompressionConfig};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OverlaybdCompactOutput {
    Raw,
    ZFile {
        algorithm: OverlaybdCompressionAlgorithm,
        workers: usize,
    },
}

impl OverlaybdCompactOutput {
    /// Maximum persistent compression threads. Prevents absurd config values
    /// from spawning thousands of OS threads during layer upload.
    const MAX_COMPRESSION_WORKERS: usize = 64;

    /// Resolve the `[snapshot.publish_compression]` switch into the layer
    /// output mode applied when recontainerizing local raw layers for upload.
    /// Capture paths never consult this: local layers always stay raw.
    pub(crate) fn from_publish_compression_config(
        config: &SnapshotPublishCompressionConfig,
    ) -> Self {
        if config.enabled {
            Self::ZFile {
                algorithm: config.algorithm,
                workers: config.workers.clamp(1, Self::MAX_COMPRESSION_WORKERS),
            }
        } else {
            Self::Raw
        }
    }
}

impl From<OverlaybdCompactOutput> for aenv_volume_core::compact::CompactOutput {
    fn from(mode: OverlaybdCompactOutput) -> Self {
        match mode {
            OverlaybdCompactOutput::Raw => Self::Raw,
            OverlaybdCompactOutput::ZFile { algorithm, workers } => Self::ZFile {
                algorithm: match algorithm {
                    OverlaybdCompressionAlgorithm::Lz4 => {
                        aenv_volume_core::compact::CompressionAlgorithm::Lz4
                    }
                    OverlaybdCompressionAlgorithm::Zstd => {
                        aenv_volume_core::compact::CompressionAlgorithm::Zstd
                    }
                },
                workers,
            },
        }
    }
}
pub(crate) async fn create_commit_args(
    output: Arc<dyn VirtualFile>,
    mode: OverlaybdCompactOutput,
    concurrency: usize,
) -> Result<CommitArgs> {
    aenv_volume_core::compact::create_commit_args(output, mode.into(), concurrency).await
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OverlaybdConfig {
    pub image_config_path: PathBuf,
    pub read_only: bool,
    #[serde(default = "default_runtime_upper_mode")]
    pub runtime_upper_mode: UpperMode,
}

fn default_runtime_upper_mode() -> UpperMode {
    UpperMode::LogStructured
}

#[derive(Clone, Debug)]
pub(crate) struct OverlaybdRuntimeHandle {
    pub(crate) device: UblkDevice,
    pub(crate) image_config_path: PathBuf,
    pub(crate) actual_virtual_size: u64,
}

/// Compact multiple overlaybd layers into one sealed commit at `output_path`.
///
/// Each input layer is opened through the tar adaptor + switch file chain so
/// raw and ZFile-compressed layers can be mixed freely. The merged commit is
/// written directly in the requested `mode` to a sibling temp file and
/// atomically renamed into place; on failure the temp file is removed and any
/// pre-existing output is left untouched.
pub(crate) async fn compact_layers(
    layers: &[LayerConfig],
    output_path: &Path,
    mode: OverlaybdCompactOutput,
) -> Result<Option<PathBuf>> {
    aenv_volume_core::compact::compact_layers(layers, output_path, mode.into()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use overlaybd::backend::switch::new_switch_file;
    use overlaybd::backend::tar::new_tar_file_adaptor;
    use overlaybd::index_file::{LSMTFile, LSMTReadOnlyFile};
    use overlaybd::zfile::is_zfile;

    async fn create_sealed_layer(
        dir: &Path,
        name: &str,
        vsize: u64,
        writes: &[(u64, u8)],
        zfile_algo: Option<u8>,
    ) -> PathBuf {
        let data = Arc::new(
            LocalFile::new(dir.join(format!("{name}.data"))).expect("create layer data file"),
        );
        let index = Arc::new(
            LocalFile::new(dir.join(format!("{name}.index"))).expect("create layer index file"),
        );
        let layer = LSMTFile::create(data, Some(index), vsize, false)
            .await
            .expect("create layer");
        for (offset, byte) in writes {
            layer
                .write_at(*offset, &[*byte; 4096])
                .await
                .expect("write layer page");
        }
        let commit_path = dir.join(format!("{name}.commit"));
        let output: Arc<dyn VirtualFile> =
            Arc::new(LocalFile::new(&commit_path).expect("create layer commit output"));
        match zfile_algo {
            Some(algo) => {
                let compress_args = CompressArgs::new(CompressOptions::new(
                    algo,
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
            None => {
                layer
                    .commit_with_args(CommitArgs::new(output))
                    .await
                    .expect("commit raw layer");
            }
        }
        commit_path
    }

    async fn read_sealed_layer(path: &Path, len: usize) -> (i32, Vec<u8>) {
        let local: Arc<dyn VirtualFile> =
            Arc::new(LocalFile::open_ro(path).expect("open sealed layer"));
        let zfile_flag = is_zfile(local.clone()).await.expect("probe zfile header");
        let display = path.display().to_string();
        let tar_adapted = new_tar_file_adaptor(local).await.expect("tar adaptor");
        let switched = new_switch_file(tar_adapted, true, Some(&display))
            .await
            .expect("switch file");
        let layer = LSMTReadOnlyFile::open(switched)
            .await
            .expect("open sealed layer as LSMT");
        let data = layer.read_at(0, len).await.expect("read sealed layer");
        (zfile_flag, data.to_vec())
    }

    fn layer_config(path: &Path) -> LayerConfig {
        LayerConfig {
            file: path.display().to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn compact_layers_mixed_input_and_configured_output() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vsize = 3 * 4096u64;
        let bottom = create_sealed_layer(
            temp.path(),
            "bottom",
            vsize,
            &[(0, 0x11), (4096, 0x11), (8192, 0x11)],
            None,
        )
        .await;
        let middle = create_sealed_layer(
            temp.path(),
            "middle",
            vsize,
            &[(4096, 0x22)],
            Some(CompressOptions::LZ4),
        )
        .await;
        let top = create_sealed_layer(
            temp.path(),
            "top",
            vsize,
            &[(8192, 0x33)],
            Some(CompressOptions::ZSTD),
        )
        .await;
        let layers = [&bottom, &middle, &top]
            .into_iter()
            .map(|path| layer_config(path))
            .collect::<Vec<_>>();

        // Later layers take precedence over earlier ones on overlapping pages.
        let mut expected = vec![0u8; vsize as usize];
        expected[..4096].fill(0x11);
        expected[4096..8192].fill(0x22);
        expected[8192..].fill(0x33);

        for (name, mode, expected_zfile) in [
            ("raw", OverlaybdCompactOutput::Raw, 0),
            (
                "lz4",
                OverlaybdCompactOutput::ZFile {
                    algorithm: OverlaybdCompressionAlgorithm::Lz4,
                    // Exercise the segmented parallel compression path.
                    workers: 2,
                },
                1,
            ),
            (
                "zstd",
                OverlaybdCompactOutput::ZFile {
                    algorithm: OverlaybdCompressionAlgorithm::Zstd,
                    workers: 1,
                },
                1,
            ),
        ] {
            let output_path = temp.path().join(format!("compacted-{name}.commit"));
            let published = compact_layers(&layers, &output_path, mode)
                .await
                .expect("compact mixed raw/lz4/zstd layers");
            assert_eq!(published.as_deref(), Some(output_path.as_path()));
            assert!(!output_path.with_extension("commit.tmp").exists());

            let (zfile_flag, data) = read_sealed_layer(&output_path, vsize as usize).await;
            assert_eq!(zfile_flag, expected_zfile, "mode {name}");
            assert_eq!(data, expected, "mode {name}");
        }

        // A failed compaction must neither clobber a previously published
        // output nor leave the sibling temp file behind.
        let output_path = temp.path().join("compacted-raw.commit");
        let before = std::fs::read(&output_path).expect("read published output");
        let bogus = temp.path().join("bogus.commit");
        std::fs::write(&bogus, vec![0xAB; 4096]).expect("write bogus layer");
        let mut broken = layers.clone();
        broken.push(layer_config(&bogus));
        compact_layers(&broken, &output_path, OverlaybdCompactOutput::Raw)
            .await
            .expect_err("invalid input layer must fail");
        assert_eq!(
            std::fs::read(&output_path).expect("read published output"),
            before
        );
        assert!(!output_path.with_extension("commit.tmp").exists());
    }

    #[test]
    fn compact_output_from_publish_compression_config() {
        let disabled = SnapshotPublishCompressionConfig {
            enabled: false,
            algorithm: OverlaybdCompressionAlgorithm::Lz4,
            workers: 1,
        };
        assert_eq!(
            OverlaybdCompactOutput::from_publish_compression_config(&disabled),
            OverlaybdCompactOutput::Raw
        );

        let zstd = SnapshotPublishCompressionConfig {
            enabled: true,
            algorithm: OverlaybdCompressionAlgorithm::Zstd,
            // Zero workers clamps to sequential.
            workers: 0,
        };
        assert_eq!(
            OverlaybdCompactOutput::from_publish_compression_config(&zstd),
            OverlaybdCompactOutput::ZFile {
                algorithm: OverlaybdCompressionAlgorithm::Zstd,
                workers: 1,
            }
        );

        let clamped = SnapshotPublishCompressionConfig {
            enabled: true,
            algorithm: OverlaybdCompressionAlgorithm::Lz4,
            workers: usize::MAX,
        };
        assert_eq!(
            OverlaybdCompactOutput::from_publish_compression_config(&clamped),
            OverlaybdCompactOutput::ZFile {
                algorithm: OverlaybdCompressionAlgorithm::Lz4,
                workers: OverlaybdCompactOutput::MAX_COMPRESSION_WORKERS,
            }
        );
    }
}
