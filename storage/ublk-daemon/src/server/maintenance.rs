//! Offline storage maintenance reusing the native runtime and restack engine.
//! No sandbox/run identities are manufactured and no device is borrowed by VMM.
use super::*;
use crate::protocol::{ManagedExpansionPlan, ManagedExpansionView};
use aenv_volume_core::{
    is_valid_volume_component,
    journal::{LockedOperation, NativeIntent, NativeOperationState},
};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::os::unix::fs::{FileTypeExt, MetadataExt};

#[derive(Serialize, Deserialize)]
struct MaintenanceDevice {
    id: u32,
    path: PathBuf,
    config: PathBuf,
    boot: String,
    daemon: String,
    identity: String,
    rdev: u64,
}

fn owned_device_view(
    plan: &ManagedExpansionPlan,
    state: &managed::ManagedSessions,
    device: &ManagedDevice,
) -> Result<MaintenanceDevice> {
    anyhow::ensure!(
        device.managed_session.as_deref()
            == Some(format!("maintenance_{}", plan.operation_id).as_str()),
        "maintenance native owner mismatch"
    );
    anyhow::ensure!(
        device.image.size_bytes() == plan.target_capacity,
        "maintenance native capacity mismatch"
    );
    let id = device.dev.dev_id();
    let path = device.dev.device_path().to_path_buf();
    let metadata = std::fs::symlink_metadata(&path)?;
    anyhow::ensure!(
        metadata.file_type().is_block_device(),
        "maintenance endpoint is not a block device"
    );
    let identity = aenv_volume_core::digest::sha256_digest(&serde_json::to_vec(&(
        &plan.operation_id,
        &state.boot,
        &state.instance,
        id,
        metadata.rdev(),
    ))?);
    Ok(MaintenanceDevice {
        id,
        path,
        config: plan.runtime_dir.join("image.json"),
        boot: state.boot.clone(),
        daemon: state.instance.clone(),
        identity,
        rdev: metadata.rdev(),
    })
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|v| v.is_ascii_hexdigit())
}

fn validate(plan: &ManagedExpansionPlan) -> Result<()> {
    anyhow::ensure!(
        [&plan.operation_id, &plan.repository_id, &plan.volume_id]
            .iter()
            .all(|v| is_valid_volume_component(v)),
        "invalid maintenance identity"
    );
    anyhow::ensure!(
        valid_digest(&plan.authorization_digest) && valid_digest(&plan.expected_head),
        "maintenance authorization and source head required"
    );
    anyhow::ensure!(
        plan.source_capacity > 0
            && plan.target_capacity > plan.source_capacity
            && plan.target_capacity % (1 << 30) == 0,
        "maintenance must grow to a GiB-aligned capacity"
    );
    anyhow::ensure!(
        !uuid::Uuid::parse_str(&plan.filesystem_uuid)?.is_nil(),
        "filesystem identity required"
    );
    anyhow::ensure!(
        plan.source_image_config.is_absolute()
            && plan.runtime_dir.is_absolute()
            && plan.global_config.is_absolute(),
        "absolute configured maintenance paths required"
    );
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct ToolProcess {
    boot: String,
    pid: u32,
    start: String,
}

fn process_start(pid: u32) -> Result<Option<String>> {
    let text = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    parse_process_start(&text)
}
fn parse_process_start(text: &str) -> Result<Option<String>> {
    let (_, tail) = text.rsplit_once(')').context("invalid process identity")?;
    let fields: Vec<_> = tail.split_whitespace().collect();
    anyhow::ensure!(fields.len() > 19, "short process identity");
    if fields[0] == "Z" || fields[0] == "X" {
        return Ok(None);
    }
    anyhow::ensure!(
        fields[19].bytes().all(|b| b.is_ascii_digit()) && !fields[19].is_empty(),
        "invalid process start time"
    );
    Ok(Some(fields[19].to_owned()))
}
fn next_tool_attempt(operation: &LockedOperation, label: &str) -> Result<usize> {
    // Records from the old ungated implementation cannot prove child absence.
    anyhow::ensure!(
        !operation
            .record()
            .checkpoints
            .contains_key(&format!("tool-{label}")),
        "legacy filesystem tool outcome remains unknown"
    );
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    for attempt in 0..1024 {
        let key = format!("tool-v2-{label}-{attempt}");
        if !operation.record().checkpoints.contains_key(&key) {
            return Ok(attempt);
        }
        if let Some(value) = operation
            .record()
            .checkpoints
            .get(&format!("{key}-process"))
        {
            let prior: ToolProcess = serde_json::from_value(value.clone())?;
            if prior.boot == boot.trim() {
                anyhow::ensure!(
                    process_start(prior.pid)?.as_deref() != Some(prior.start.as_str()),
                    "original filesystem mutator is still alive"
                );
            }
        }
        // With no process checkpoint the gate could not have opened. A dead
        // parent closes the pipe; the waiting shell exits without executing.
    }
    bail!("maintenance retry budget exhausted; preserve the original upper")
}

async fn run_tool(
    operation: &mut LockedOperation,
    label: &str,
    name: &str,
    args: &[&str],
    path: &Path,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let done = format!("tool-{label}-complete");
    if operation.record().checkpoints.contains_key(&done) {
        return Ok(());
    }
    let attempt = next_tool_attempt(operation, label)?;
    let key = format!("tool-v2-{label}-{attempt}");
    operation.checkpoint(
        &key,
        serde_json::json!({"device":path,"args":args,"command":name}),
    )?;
    // The shell is only an exec gate, not a command interpolator: every tool
    // argument is passed separately. No mutating executable starts until its
    // PID/start/boot identity is durable. The shell exec retains that identity.
    let mut child = tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("IFS= read -r token && [ \"$token\" = start ] || exit 125; exec \"$@\"")
        .arg("ssf-maintenance-gate")
        .arg(name)
        .args(args)
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let pid = child.id().context("filesystem tool PID missing")?;
    let proof = ToolProcess {
        pid,
        start: process_start(pid)?.context("filesystem gate exited early")?,
        boot: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .to_owned(),
    };
    operation.checkpoint(&format!("{key}-process"), serde_json::to_value(&proof)?)?;
    let mut gate = child.stdin.take().context("filesystem gate unavailable")?;
    gate.write_all(b"start\n").await?;
    drop(gate);
    let status = match tokio::time::timeout(Duration::from_secs(120), child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            child.kill().await?;
            let _ = child.wait().await;
            bail!("filesystem tool timed out; retry retains the original operation");
        }
    };
    anyhow::ensure!(
        status.success() || (name == "e2fsck" && status.code() == Some(1)),
        "filesystem maintenance tool {name} failed: {status}"
    );
    operation.checkpoint(&done, serde_json::json!(true))?;
    Ok(())
}

async fn filesystem_tool(
    operation: &mut LockedOperation,
    name: &str,
    args: &[&str],
    path: &Path,
) -> Result<()> {
    if operation
        .record()
        .checkpoints
        .contains_key(&format!("tool-{name}-complete"))
    {
        return Ok(());
    }
    let attempt = next_tool_attempt(operation, name)?;
    if name == "resize2fs" && attempt > 0 {
        // Recover the same offline filesystem after an interrupted resize;
        // never initialize it, and never run repair concurrently with a child.
        run_tool(
            operation,
            &format!("e2fsck-recovery-{attempt}"),
            "e2fsck",
            &["-p", "-f"],
            path,
        )
        .await?;
    }
    run_tool(operation, name, name, args, path).await
}

fn require_device_absent(device: &MaintenanceDevice) -> Result<()> {
    for path in [
        device.path.clone(),
        PathBuf::from(format!("/sys/block/ublkb{}", device.id)),
    ] {
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => bail!("original maintenance device may still exist; release is not proven"),
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct SealOrigin {
    device: u64,
    inode: u64,
}

fn record_restack_intent(
    operation: &mut LockedOperation,
    config: &ImageConfig,
    output: &Path,
) -> Result<()> {
    match std::fs::symlink_metadata(output) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        _ => bail!("maintenance seal output already exists or cannot be inspected"),
    }
    let metadata = std::fs::symlink_metadata(&config.upper.data)?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "restack upper must be a regular file"
    );
    operation.checkpoint("restack-source-config", serde_json::to_value(config)?)?;
    operation.checkpoint(
        "restack-upper-origin",
        serde_json::to_value(SealOrigin {
            device: metadata.dev(),
            inode: metadata.ino(),
        })?,
    )?;
    operation.checkpoint("restack-started", serde_json::json!(true))?;
    Ok(())
}

fn recover_moved_layer(
    plan: &ManagedExpansionPlan,
    source: &ImageConfig,
    origin: &SealOrigin,
) -> Result<ImageConfig> {
    let output = plan.runtime_dir.join("expanded.commit");
    let metadata = std::fs::symlink_metadata(&output)?;
    anyhow::ensure!(
        metadata.file_type().is_file()
            && metadata.dev() == origin.device
            && metadata.ino() == origin.inode,
        "maintenance output is not the original upper inode"
    );
    anyhow::ensure!(
        overlaybd::layer_metadata::read_overlaybd_layer_virtual_size(&output)?
            == plan.target_capacity,
        "maintenance sealed layer capacity mismatch"
    );
    let uuid = overlaybd::layer_metadata::read_overlaybd_layer_uuid(&output)?;
    anyhow::ensure!(!uuid.is_nil(), "maintenance sealed layer identity missing");
    let mut config = source.clone();
    config.lowers.push(overlaybd::config::LayerConfig {
        file: output.display().to_string(),
        size: metadata.len(),
        uuid: uuid.to_string(),
        ..Default::default()
    });
    config.upper = Default::default();
    // The caller verifies the complete read-only image and ext4 UUID before
    // publishing. Do not recreate/truncate the possibly partial fresh upper.
    Ok(config)
}

fn completed_restack_config(
    plan: &ManagedExpansionPlan,
    device: &MaintenanceDevice,
    source: &ImageConfig,
) -> Result<ImageConfig> {
    anyhow::ensure!(
        device.config == plan.runtime_dir.join("image.json"),
        "maintenance runtime path changed"
    );
    let output = plan.runtime_dir.join("expanded.commit");
    let mut config = overlaybd::config::load_image_config(&device.config)?;
    // Only the exact appended layer over the originally pinned source is a
    // completed restack. A file existing at the output path alone proves nothing.
    anyhow::ensure!(
        config.lowers.len() == source.lowers.len() + 1
            && serde_json::to_value(&config.lowers[..source.lowers.len()])?
                == serde_json::to_value(&source.lowers)?
            && config
                .lowers
                .last()
                .is_some_and(|layer| Path::new(&layer.file) == output),
        "maintenance restack has no durable matching config"
    );
    let metadata = std::fs::symlink_metadata(&output)?;
    anyhow::ensure!(
        metadata.file_type().is_file() && metadata.len() > 0,
        "invalid sealed maintenance layer"
    );
    // Recovery reads only immutable layers, never opens or recreates the old
    // writable upper and never exposes a replacement endpoint to a VM.
    config.upper = Default::default();
    Ok(config)
}

async fn verify_expanded_image(plan: &ManagedExpansionPlan, image: &Arc<ImageFile>) -> Result<()> {
    anyhow::ensure!(
        image.size_bytes() == plan.target_capacity,
        "maintenance capacity changed"
    );
    let file: Arc<dyn overlaybd::virtual_file::VirtualFile> = image.clone();
    let geometry = overlaybd::ext4_stat::ext4_geometry(&file).await?;
    anyhow::ensure!(
        geometry.capacity_bytes == plan.target_capacity
            && geometry.uuid == *uuid::Uuid::parse_str(&plan.filesystem_uuid)?.as_bytes(),
        "maintenance filesystem identity changed"
    );
    Ok(())
}

async fn recover_maintenance_seal(
    operation: &mut LockedOperation,
    plan: &ManagedExpansionPlan,
    device: &MaintenanceDevice,
    cache: &ImageServiceCache,
) -> Result<PathBuf> {
    anyhow::ensure!(
        device.config == plan.runtime_dir.join("image.json"),
        "maintenance runtime path changed"
    );
    require_device_absent(device)?;
    for key in ["tool-e2fsck-complete", "tool-resize2fs-complete"] {
        anyhow::ensure!(
            operation.record().checkpoints.get(key) == Some(&serde_json::json!(true)),
            "maintenance did not durably finish its filesystem phase"
        );
    }
    let started = operation
        .record()
        .checkpoints
        .contains_key("restack-started");
    let output = plan.runtime_dir.join("expanded.commit");
    let unmoved = match std::fs::symlink_metadata(&output) {
        Ok(_) => false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(error.into()),
    };
    if !started || unmoved {
        anyhow::ensure!(unmoved, "unrecorded maintenance output exists");
        anyhow::ensure!(
            device.config == plan.runtime_dir.join("image.json"),
            "maintenance runtime path changed"
        );
        let config: ImageConfig = if started {
            serde_json::from_value(
                operation
                    .record()
                    .checkpoints
                    .get("restack-source-config")
                    .context("maintenance restack source missing")?
                    .clone(),
            )?
        } else {
            overlaybd::config::load_image_config(&device.config)?
        };
        let data = plan.runtime_dir.join("upper.data");
        let index = plan.runtime_dir.join("upper.index");
        anyhow::ensure!(
            Path::new(&config.upper.data) == data
                && Path::new(&config.upper.index) == index
                && config.upper.writable_mode() == UpperMode::LogStructured,
            "maintenance original upper identity changed"
        );
        for path in [&data, &index] {
            anyhow::ensure!(
                std::fs::symlink_metadata(path)?.file_type().is_file(),
                "maintenance upper is not a regular file"
            );
        }
        if started {
            let origin: SealOrigin = serde_json::from_value(
                operation
                    .record()
                    .checkpoints
                    .get("restack-upper-origin")
                    .context("maintenance seal origin missing")?
                    .clone(),
            )?;
            let metadata = std::fs::symlink_metadata(&data)?;
            anyhow::ensure!(
                metadata.dev() == origin.device && metadata.ino() == origin.inode,
                "unmoved maintenance upper was replaced"
            );
        }
        overlaybd::index_file::validate_rw_header_pair_paths(
            &data,
            Some(&index),
            plan.target_capacity,
            overlaybd::index_file::RwLayout::from(UpperMode::LogStructured),
        )?;
        if started && overlaybd::layer_metadata::read_overlaybd_layer_virtual_size(&data).is_ok() {
            anyhow::ensure!(
                overlaybd::layer_metadata::read_overlaybd_layer_virtual_size(&data)?
                    == plan.target_capacity,
                "unmoved sealed upper has wrong capacity"
            );
            let _sealed = overlaybd::index_file::LSMTReadOnlyFile::open(Arc::new(
                overlaybd::backend::local::LocalFile::open_ro(&data)?,
            ))
            .await?;
            // A complete seal cannot be opened RW. Link the exact original inode
            // without overwriting/removing either file; then recover the readonly
            // stack below. This maintenance upper was never borrowed by VMM.
            File::open(&data)?.sync_all()?;
            std::fs::hard_link(&data, &output)?;
            File::open(&plan.runtime_dir)?.sync_all()?;
        } else {
            let service = cache.get_or_create(&plan.global_config).await?;
            let image = Arc::new(ImageFile::open(config, service, None).await?);
            verify_expanded_image(plan, &image).await?;
            // Both filesystem tools completed and their children were waited for.
            // The old endpoint is absent, and this maintenance upper was never
            // borrowed by a VM. Reuse it without creating a replacement device.
            let current = image.runtime_config().await;
            record_restack_intent(
                operation,
                &current,
                &plan.runtime_dir.join("expanded.commit"),
            )?;
            image
                .create_snapshot_and_restack_checkpointed(
                    &plan.runtime_dir.join("expanded.commit"),
                    |config| managed::write_config(&device.config, config),
                )
                .await?;
        }
    }
    let source: ImageConfig = serde_json::from_value(
        operation
            .record()
            .checkpoints
            .get("restack-source-config")
            .context("maintenance restack source missing")?
            .clone(),
    )?;
    let config = match completed_restack_config(plan, device, &source) {
        Ok(config) => config,
        Err(_) => {
            let origin: SealOrigin = serde_json::from_value(
                operation
                    .record()
                    .checkpoints
                    .get("restack-upper-origin")
                    .context("maintenance seal origin missing")?
                    .clone(),
            )?;
            // The original upper has already become a valid sealed layer but
            // the process may have died before persisting the new runtime config.
            // Maintenance never borrowed this upper to VMM; the old endpoint is
            // absent and both filesystem mutators are proven complete.
            recover_moved_layer(plan, &source, &origin)?
        }
    };
    let service = cache.get_or_create(&plan.global_config).await?;
    let image = Arc::new(ImageFile::open(config.clone(), service, None).await?);
    verify_expanded_image(plan, &image).await?;
    drop(image);
    // The original process may have died before the checkpointed path synced
    // the rename. A recovered receipt must not outlive its sealed file.
    File::open(plan.runtime_dir.join("expanded.commit"))?.sync_all()?;
    File::open(&plan.runtime_dir)?.sync_all()?;
    let path = plan.runtime_dir.join("expanded.json");
    managed::write_config(&path, &config)?;
    operation.checkpoint("sealed", serde_json::to_value(&path)?)?;
    Ok(path)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn expand(
    state: &managed::ManagedSessions,
    plan: ManagedExpansionPlan,
    devices: &DashMap<u32, ManagedDevice>,
    ctrl_ring: IoRingHandle<io_uring::squeue::Entry128>,
    cache: &ImageServiceCache,
    resize_tool: Option<&ResizeToolSpec>,
    resize_global_config: &Path,
    resize_permit: Arc<Mutex<()>>,
    recordings: &DashMap<u32, Arc<PackRecordingHandle>>,
) -> Result<DaemonResponse> {
    validate(&plan)?;
    let digest = aenv_volume_core::digest::sha256_digest(&serde_json::to_vec(&plan)?);
    let mut operation = LockedOperation::open(
        &state.root.join("maintenance"),
        NativeIntent {
            operation_id: plan.operation_id.clone(),
            repository_id: plan.repository_id.clone(),
            volume_id: plan.volume_id.clone(),
            input_digest: digest.trim_start_matches("sha256:").into(),
            kind: "offline-expansion-v1".into(),
        },
    )?;
    operation.checkpoint("plan", serde_json::to_value(&plan)?)?;
    if operation.record().state == NativeOperationState::Completed {
        let view = serde_json::from_value(
            operation
                .record()
                .receipt
                .clone()
                .context("maintenance receipt missing")?,
        )?;
        return Ok(DaemonResponse::ManagedExpansion { view });
    }
    if !operation.record().checkpoints.contains_key("device") {
        let owner = format!("maintenance_{}", plan.operation_id);
        let recovered: Vec<_> = devices
            .iter()
            .filter(|device| device.managed_session.as_deref() == Some(owner.as_str()))
            .map(|device| owned_device_view(&plan, state, &device))
            .collect::<Result<_>>()?;
        anyhow::ensure!(recovered.len() <= 1, "duplicate maintenance devices");
        if let Some(view) = recovered.into_iter().next() {
            operation.checkpoint("device", serde_json::to_value(&view)?)?;
        }
    }
    let device: MaintenanceDevice =
        if let Some(value) = operation.record().checkpoints.get("device") {
            serde_json::from_value(value.clone())?
        } else {
            anyhow::ensure!(
                operation.record().state == NativeOperationState::Intent,
                "maintenance device creation outcome unknown"
            );
            operation.begin()?;
            let response = handle_create_overlaybd_runtime_device(
                OverlaybdRuntimeDeviceRequest {
                    source_image_config: &plan.source_image_config,
                    global_config: &plan.global_config,
                    runtime_dir: &plan.runtime_dir,
                    read_only: false,
                    runtime_upper_mode: UpperMode::LogStructured,
                    requested_virtual_size: Some(plan.target_capacity),
                    known_source_virtual_size: Some(plan.source_capacity),
                    resize_tool,
                    resize_global_config,
                    resize_permit,
                    allow_shrink: false,
                    retain_on_error: true,
                    managed_session: Some(&format!("maintenance_{}", plan.operation_id)),
                },
                ctrl_ring.clone(),
                devices,
                cache,
                &None,
            )
            .await?;
            let DaemonResponse::OverlaybdRuntimeDeviceCreated {
                dev_id,
                device_path,
                actual_virtual_size,
                runtime_image_config_path,
            } = response
            else {
                bail!("unexpected maintenance device receipt")
            };
            anyhow::ensure!(
                actual_virtual_size == plan.target_capacity,
                "maintenance device capacity mismatch"
            );
            anyhow::ensure!(
                runtime_image_config_path == plan.runtime_dir.join("image.json")
                    && device_path == PathBuf::from(format!("/dev/ublkb{dev_id}")),
                "maintenance receipt differs from fixed runtime"
            );
            let view = owned_device_view(
                &plan,
                state,
                &*devices.get(&dev_id).context("maintenance device missing")?,
            )?;
            operation.checkpoint("device", serde_json::to_value(&view)?)?;
            view
        };
    let owner = format!("maintenance_{}", plan.operation_id);
    let live = device.boot == state.boot
        && device.daemon == state.instance
        && devices
            .get(&device.id)
            .is_some_and(|d| d.managed_session.as_deref() == Some(owner.as_str()));
    let sealed: PathBuf = if let Some(value) = operation.record().checkpoints.get("sealed") {
        serde_json::from_value(value.clone())?
    } else if !live {
        recover_maintenance_seal(&mut operation, &plan, &device, cache).await?
    } else {
        let metadata = std::fs::symlink_metadata(&device.path)?;
        anyhow::ensure!(
            metadata.file_type().is_block_device() && metadata.rdev() == device.rdev,
            "maintenance device was replaced"
        );
        // Verify original UUID before running any filesystem mutator.
        let file: Arc<dyn overlaybd::virtual_file::VirtualFile> =
            Arc::new(overlaybd::backend::local::LocalFile::open_ro(&device.path)?);
        let before = overlaybd::ext4_stat::ext4_geometry(&file).await?;
        anyhow::ensure!(
            before.uuid == *uuid::Uuid::parse_str(&plan.filesystem_uuid)?.as_bytes()
                && before.capacity_bytes <= plan.target_capacity,
            "maintenance filesystem identity mismatch"
        );
        drop(file);
        filesystem_tool(&mut operation, "e2fsck", &["-p", "-f"], &device.path).await?;
        filesystem_tool(&mut operation, "resize2fs", &[], &device.path).await?;
        let file: Arc<dyn overlaybd::virtual_file::VirtualFile> =
            Arc::new(overlaybd::backend::local::LocalFile::open_ro(&device.path)?);
        let after = overlaybd::ext4_stat::ext4_geometry(&file).await?;
        anyhow::ensure!(
            after.uuid == *uuid::Uuid::parse_str(&plan.filesystem_uuid)?.as_bytes()
                && after.capacity_bytes == plan.target_capacity,
            "expanded ext4 geometry mismatch"
        );
        drop(file);
        let output = plan.runtime_dir.join("expanded.commit");
        let image = Arc::clone(
            &devices
                .get(&device.id)
                .context("maintenance device disappeared")?
                .image,
        );
        let current = image.runtime_config().await;
        let already_sealed = current.lowers.iter().any(|v| Path::new(&v.file) == output);
        if !already_sealed {
            anyhow::ensure!(
                !operation
                    .record()
                    .checkpoints
                    .contains_key("restack-started"),
                "maintenance restack outcome unknown"
            );
            record_restack_intent(&mut operation, &current, &output)?;
            image
                .create_snapshot_and_restack_checkpointed(&output, |config| {
                    managed::write_config(&device.config, config)
                })
                .await?;
        }
        let mut config = image.runtime_config().await;
        managed::write_config(&device.config, &config)?;
        config.upper = Default::default();
        let path = plan.runtime_dir.join("expanded.json");
        managed::write_config(&path, &config)?;
        operation.checkpoint("sealed", serde_json::to_value(&path)?)?;
        path
    };
    operation.checkpoint("release-started", serde_json::json!(device.identity))?;
    if live {
        handle_delete(devices, ctrl_ring, recordings, device.id).await?;
    } else {
        require_device_absent(&device)?;
    }
    let view = ManagedExpansionView {
        operation_id: plan.operation_id,
        image_config_path: sealed,
        capacity_bytes: plan.target_capacity,
        filesystem_uuid: plan.filesystem_uuid,
        device_identity: device.identity,
        device_released: true,
    };
    operation.complete(serde_json::to_value(&view)?)?;
    Ok(DaemonResponse::ManagedExpansion { view })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn interrupted_maintenance_move_recovers_only_original_sealed_inode() {
        use overlaybd::index_file::{LSMTReadOnlyFile, LayerInfo, create_file_rw};
        use overlaybd::virtual_file::VirtualFile;
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("upper.data");
        let index = root.path().join("upper.index");
        let layer = create_file_rw(LayerInfo::new(
            Arc::new(overlaybd::backend::local::LocalFile::new(&data).unwrap()),
            Some(Arc::new(
                overlaybd::backend::local::LocalFile::new(&index).unwrap(),
            )),
            8192,
        ))
        .await
        .unwrap();
        layer.write_at(0, &vec![0x42; 8192]).await.unwrap();
        let metadata = data.metadata().unwrap();
        let origin = SealOrigin {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        layer.close_seal().await.unwrap();
        drop(layer);
        let output = root.path().join("expanded.commit");
        std::fs::rename(&data, &output).unwrap();
        // Crash after rename, during fresh-upper creation: retain these bytes.
        std::fs::write(&data, b"partial fresh upper").unwrap();
        let plan = ManagedExpansionPlan {
            operation_id: "op".into(),
            repository_id: "repo".into(),
            volume_id: "volume".into(),
            authorization_digest: "a".repeat(64),
            expected_head: "b".repeat(64),
            source_capacity: 4096,
            target_capacity: 8192,
            filesystem_uuid: "29aff887-2fe8-431a-a1a5-ad5766b7bf0a".into(),
            source_image_config: root.path().join("source.json"),
            runtime_dir: root.path().to_owned(),
            global_config: root.path().join("global.json"),
        };
        let config = recover_moved_layer(&plan, &ImageConfig::default(), &origin).unwrap();
        assert!(config.upper.data.is_empty());
        assert_eq!(config.lowers.len(), 1);
        let reader = LSMTReadOnlyFile::open(Arc::new(
            overlaybd::backend::local::LocalFile::open_ro(&output).unwrap(),
        ))
        .await
        .unwrap();
        assert_eq!(
            reader.read_at(0, 8192).await.unwrap().as_ref(),
            &vec![0x42; 8192]
        );
        assert_eq!(std::fs::read(&data).unwrap(), b"partial fresh upper");
        let other = SealOrigin {
            device: origin.device,
            inode: origin.inode + 1,
        };
        assert!(recover_moved_layer(&plan, &ImageConfig::default(), &other).is_err());
    }

    #[test]
    fn sealed_recovery_requires_exact_persisted_lower_stack_and_real_output() {
        let root = tempfile::tempdir().unwrap();
        let mut source = ImageConfig::default();
        source.lowers.push(overlaybd::config::LayerConfig {
            file: root.path().join("base.commit").display().to_string(),
            ..Default::default()
        });
        let plan = ManagedExpansionPlan {
            operation_id: "op".into(),
            repository_id: "repo".into(),
            volume_id: "vol".into(),
            authorization_digest: "a".repeat(64),
            expected_head: "b".repeat(64),
            source_capacity: 1 << 30,
            target_capacity: 2 << 30,
            filesystem_uuid: "29aff887-2fe8-431a-a1a5-ad5766b7bf0a".into(),
            source_image_config: root.path().join("source.json"),
            runtime_dir: root.path().to_owned(),
            global_config: root.path().join("global.json"),
        };
        let device = MaintenanceDevice {
            id: 1,
            path: root.path().join("absent-device"),
            config: root.path().join("image.json"),
            boot: "boot".into(),
            daemon: "daemon".into(),
            identity: "identity".into(),
            rdev: 1,
        };
        let output = root.path().join("expanded.commit");
        // This test validates routing/identity only; layer/FS verification is
        // performed separately by recover_maintenance_seal before accepting it.
        std::fs::write(&output, b"not-a-real-layer").unwrap();
        managed::write_config(&device.config, &source).unwrap();
        assert!(completed_restack_config(&plan, &device, &source).is_err());
        let mut next = source.clone();
        next.lowers.push(overlaybd::config::LayerConfig {
            file: output.display().to_string(),
            ..Default::default()
        });
        next.upper = overlaybd::config::UpperConfig {
            data: root.path().join("upper.data").display().to_string(),
            index: root.path().join("upper.index").display().to_string(),
            ..Default::default()
        };
        managed::write_config(&device.config, &next).unwrap();
        let sealed = completed_restack_config(&plan, &device, &source).unwrap();
        assert!(sealed.upper.data.is_empty());
        let mut changed = source.clone();
        changed.lowers[0].file = root.path().join("other.commit").display().to_string();
        assert!(completed_restack_config(&plan, &device, &changed).is_err());
        std::fs::remove_file(&output).unwrap();
        std::os::unix::fs::symlink(&device.config, &output).unwrap();
        assert!(completed_restack_config(&plan, &device, &source).is_err());
    }

    #[test]
    fn process_identity_handles_parentheses_pid_reuse_and_zombies() {
        let mut fields = vec!["0"; 20];
        fields[0] = "S";
        fields[19] = "1234";
        assert_eq!(
            parse_process_start(&format!("10 (name with ) brackets) {}", fields.join(" ")))
                .unwrap(),
            Some("1234".into())
        );
        fields[0] = "Z";
        assert_eq!(
            parse_process_start(&format!("10 (name) {}", fields.join(" "))).unwrap(),
            None
        );
        assert!(parse_process_start("10 (name) S 0").is_err());
    }

    #[tokio::test]
    async fn gated_child_is_persisted_before_execution_and_replay_does_not_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let mut operation = LockedOperation::open(
            dir.path(),
            NativeIntent {
                operation_id: "op_a".into(),
                repository_id: "repo".into(),
                volume_id: "vol_a".into(),
                input_digest: "a".repeat(64),
                kind: "test-maintenance".into(),
            },
        )
        .unwrap();
        operation.begin().unwrap();
        run_tool(&mut operation, "fixture", "/bin/true", &[], dir.path())
            .await
            .unwrap();
        assert!(
            operation
                .record()
                .checkpoints
                .contains_key("tool-v2-fixture-0-process")
        );
        let count = operation.record().checkpoints.len();
        run_tool(&mut operation, "fixture", "/bin/true", &[], dir.path())
            .await
            .unwrap();
        assert_eq!(operation.record().checkpoints.len(), count);
        let proof = ToolProcess {
            boot: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .unwrap()
                .trim()
                .into(),
            pid: std::process::id(),
            start: process_start(std::process::id()).unwrap().unwrap(),
        };
        operation
            .checkpoint("tool-v2-alive-0", serde_json::json!(true))
            .unwrap();
        operation
            .checkpoint(
                "tool-v2-alive-0-process",
                serde_json::to_value(proof).unwrap(),
            )
            .unwrap();
        assert!(next_tool_attempt(&operation, "alive").is_err());
    }
    #[test]
    fn maintenance_plan_has_no_runtime_authority_and_cannot_shrink() {
        let mut p = ManagedExpansionPlan {
            operation_id: "expand_a".into(),
            repository_id: "repo".into(),
            volume_id: "vol_a".into(),
            authorization_digest: "a".repeat(64),
            expected_head: "b".repeat(64),
            source_capacity: 1 << 30,
            target_capacity: 2 << 30,
            filesystem_uuid: "29aff887-2fe8-431a-a1a5-ad5766b7bf0a".into(),
            source_image_config: "/private/source.json".into(),
            runtime_dir: "/private/runtime".into(),
            global_config: "/private/global.json".into(),
        };
        validate(&p).unwrap();
        p.target_capacity = p.source_capacity;
        assert!(validate(&p).is_err());
        p.target_capacity = 2 << 30;
        p.authorization_digest.clear();
        assert!(validate(&p).is_err());
        p.authorization_digest = "a".repeat(64);
        p.filesystem_uuid = uuid::Uuid::nil().to_string();
        assert!(validate(&p).is_err());
    }
}
