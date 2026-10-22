//! Native layer compaction shared by volume publication and runtime capture.
use anyhow::{Context, Result};
use overlaybd::{
    backend::local::LocalFile,
    config::LayerConfig,
    index_file::{merge_files_ro, CommitArgs},
    virtual_file::VirtualFile,
    zfile::{CompressArgs, CompressOptions, ZFileCompactWriter},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tracing::debug;
use uuid::Uuid;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompressionAlgorithm {
    Lz4,
    Zstd,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactOutput {
    Raw,
    ZFile {
        algorithm: CompressionAlgorithm,
        workers: usize,
    },
}
pub async fn create_commit_args(
    output: Arc<dyn VirtualFile>,
    mode: CompactOutput,
    concurrency: usize,
) -> Result<CommitArgs> {
    let mut args = match mode {
        CompactOutput::Raw => CommitArgs::new(output),
        CompactOutput::ZFile { algorithm, workers } => {
            let algorithm = match algorithm {
                CompressionAlgorithm::Lz4 => CompressOptions::LZ4,
                CompressionAlgorithm::Zstd => CompressOptions::ZSTD,
            };
            let mut compress_args = CompressArgs::new(CompressOptions::new(
                algorithm,
                CompressOptions::DEFAULT_BLOCK_SIZE,
                0,
            ));
            compress_args.workers = workers.max(1);
            CommitArgs::from_writer(Arc::new(
                ZFileCompactWriter::new(output, &compress_args).await?,
            ))
        }
    };
    args.concurrency = concurrency;
    Ok(args)
}
pub async fn compact_layers(
    layers: &[LayerConfig],
    output_path: &Path,
    mode: CompactOutput,
) -> Result<Option<PathBuf>> {
    if layers.is_empty() {
        return Ok(None);
    }

    let mut src_files: Vec<Arc<dyn VirtualFile>> = Vec::with_capacity(layers.len());
    for layer in layers {
        let path = Path::new(&layer.file);
        let local: Arc<dyn VirtualFile> = Arc::new(
            LocalFile::open_ro(path)
                .with_context(|| format!("open layer for compaction: {}", path.display()))?,
        );
        let tar_adapted = overlaybd::backend::tar::new_tar_file_adaptor(local)
            .await
            .with_context(|| format!("adapt layer as tar file: {}", path.display()))?;
        let display = path.display().to_string();
        let switched =
            overlaybd::backend::switch::new_switch_file(tar_adapted, true, Some(&display))
                .await
                .with_context(|| format!("open layer via switch file: {}", path.display()))?;
        src_files.push(switched);
    }

    let lower_tmp = output_path.with_extension(format!("commit.{}.tmp", Uuid::now_v7()));
    let build_result: Result<()> = async {
        let output_file: Arc<dyn VirtualFile> =
            Arc::new(LocalFile::new(&lower_tmp).context("create compacted layer output file")?);
        let commit_args = create_commit_args(output_file, mode, 32).await?;
        merge_files_ro(&src_files, commit_args)
            .await
            .context("merge layers")?;
        tokio::fs::rename(&lower_tmp, output_path)
            .await
            .with_context(|| {
                format!(
                    "publish compacted layer to {} failed",
                    output_path.display()
                )
            })?;
        Ok(())
    }
    .await;

    if build_result.is_err() {
        let _ = tokio::fs::remove_file(&lower_tmp).await;
    }
    build_result?;

    debug!(
        output = %output_path.display(),
        input_layers = layers.len(),
        "compacted overlaybd layers"
    );
    Ok(Some(output_path.to_path_buf()))
}
