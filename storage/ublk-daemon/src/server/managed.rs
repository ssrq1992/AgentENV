//! CSI-only session ownership around the original native device engine.
use super::*;
use crate::protocol::{
    ManagedCaptureView, ManagedDeviceAction, ManagedDevicePlan, ManagedDeviceView,
};
use aenv_volume_core::{
    is_valid_volume_component,
    journal::{LockedOperation, NativeIntent, NativeOperationState},
};
use nix::fcntl::{Flock, FlockArg};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
};

// Managed sockets and journals are private capabilities; no legacy daemon
// path or mode is changed by this helper.
pub(super) fn private_directory(path: &Path) -> Result<()> {
    anyhow::ensure!(path.is_absolute(), "absolute managed directory required");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
        "private nonsymlink managed directory required"
    );
    Ok(())
}

pub(super) struct ManagedSessions {
    active: std::sync::atomic::AtomicUsize,
    pub(super) root: PathBuf,
    pub(super) boot: String,
    pub(super) instance: String,
    _lock: Flock<File>,
}

// A replaced pod's previous daemon can keep daemon.lock until its termination
// grace period expires. The kernel releases the flock when that process
// exits, so wait for it instead of failing the new daemon's startup.
fn daemon_lock_wait() -> std::time::Duration {
    std::env::var("AENV_DAEMON_LOCK_WAIT_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|secs| *secs > 0 && *secs <= 3600)
        .map(std::time::Duration::from_secs)
        .unwrap_or(std::time::Duration::from_secs(120))
}

fn lock_exclusive_with_wait(file: File) -> Result<Flock<File>> {
    let wait = daemon_lock_wait();
    let deadline = std::time::Instant::now() + wait;
    let mut file = file;
    let mut warned = false;
    loop {
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => return Ok(lock),
            Err((held, nix::errno::Errno::EAGAIN)) => {
                file = held;
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!(
                        "managed daemon state is still owned by another process after {}s",
                        wait.as_secs()
                    );
                }
                if !warned {
                    warned = true;
                    tracing::warn!(
                        wait_secs = wait.as_secs(),
                        "managed daemon state is owned by the previous daemon; waiting for it to exit"
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err((_, e)) => return Err(anyhow::anyhow!("managed daemon lock failed: {e}")),
        }
    }
}
pub(super) struct ManagedActivity<'a>(&'a ManagedSessions);
impl Drop for ManagedActivity<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}
impl ManagedSessions {
    pub(super) fn idle(&self) -> bool {
        self.active.load(Ordering::SeqCst) == 0
    }
    pub(super) fn enter(&self) -> ManagedActivity<'_> {
        self.active.fetch_add(1, Ordering::SeqCst);
        ManagedActivity(self)
    }
    pub(super) fn open(root: PathBuf) -> Result<Self> {
        anyhow::ensure!(
            root.is_absolute(),
            "absolute managed daemon state root required"
        );
        private_directory(&root)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(root.join("daemon.lock"))?;
        let lock = lock_exclusive_with_wait(file)?;
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .to_owned();
        anyhow::ensure!(!boot.is_empty(), "physical boot identity unavailable");
        Ok(Self {
            active: std::sync::atomic::AtomicUsize::new(0),
            root,
            boot,
            instance: uuid::Uuid::now_v7().to_string(),
            _lock: lock,
        })
    }
    fn operation(&self, plan: &ManagedDevicePlan) -> Result<LockedOperation> {
        anyhow::ensure!(
            [&plan.session_id, &plan.operation_id, &plan.volume_id]
                .iter()
                .all(|id| is_valid_volume_component(id))
                && !plan.repository_id.is_empty()
                && !plan.owner_id.is_empty()
                && !plan.run_id.is_empty()
                && !plan.node_id.is_empty()
                && plan.epoch > 0
                && plan.virtual_size > 0
                && plan.virtual_size % 512 == 0
                && !plan.head_revision.is_empty(),
            "incomplete managed device plan"
        );
        anyhow::ensure!(
            matches!(plan.purpose.as_str(), "runtime" | "preparation"),
            "unsupported native session purpose"
        );
        anyhow::ensure!(
            plan.runtime_dir.is_absolute()
                && plan.source_image_config.is_absolute()
                && plan.global_config.is_absolute(),
            "absolute native device paths required"
        );
        let digest = aenv_volume_core::digest::sha256_digest(&serde_json::to_vec(plan)?)
            .trim_start_matches("sha256:")
            .to_owned();
        Ok(LockedOperation::open(
            &self.root.join("sessions"),
            NativeIntent {
                operation_id: plan.session_id.clone(),
                input_digest: digest,
                volume_id: plan.volume_id.clone(),
                repository_id: plan.repository_id.clone(),
                kind: "managed-device-session-v1".into(),
            },
        )?)
    }
    fn pending(&self, plan: &ManagedDevicePlan, state: &str) -> ManagedDeviceView {
        ManagedDeviceView {
            session_id: plan.session_id.clone(),
            state: state.into(),
            node_boot_id: self.boot.clone(),
            daemon_instance_id: self.instance.clone(),
            device_identity: String::new(),
            dev_id: None,
            device_path: None,
            major: 0,
            minor: 0,
            actual_virtual_size: 0,
            runtime_image_config_path: None,
        }
    }
    // Ownership is installed atomically with the in-memory native device, before
    // returning from create. Recover a lost response from that original handle;
    // never infer ownership from a reusable /dev number after daemon restart.
    fn device_view(
        &self,
        plan: &ManagedDevicePlan,
        device: &ManagedDevice,
    ) -> Result<ManagedDeviceView> {
        anyhow::ensure!(
            device.managed_session.as_deref() == Some(plan.session_id.as_str()),
            "native session owner mismatch"
        );
        let dev_id = device.dev.dev_id();
        let path = device.dev.device_path().to_path_buf();
        let metadata = std::fs::symlink_metadata(&path)?;
        anyhow::ensure!(
            metadata.file_type().is_block_device(),
            "native device is not a block endpoint"
        );
        anyhow::ensure!(
            device.image.size_bytes() == plan.virtual_size,
            "native device capacity changed"
        );
        let config = plan.runtime_dir.join("image.json");
        let identity = aenv_volume_core::digest::sha256_digest(&serde_json::to_vec(&(
            &plan.session_id,
            &self.boot,
            &self.instance,
            dev_id,
            metadata.rdev(),
            &config,
        ))?);
        Ok(ManagedDeviceView {
            session_id: plan.session_id.clone(),
            state: "Staged".into(),
            node_boot_id: self.boot.clone(),
            daemon_instance_id: self.instance.clone(),
            device_identity: identity,
            dev_id: Some(dev_id),
            device_path: Some(path),
            major: libc::major(metadata.rdev()) as u64,
            minor: libc::minor(metadata.rdev()) as u64,
            actual_virtual_size: device.image.size_bytes(),
            runtime_image_config_path: Some(config),
        })
    }
    fn observed(
        &self,
        plan: &ManagedDevicePlan,
        view: &ManagedDeviceView,
        devices: &DashMap<u32, ManagedDevice>,
    ) -> Result<bool> {
        if view.node_boot_id != self.boot || view.daemon_instance_id != self.instance {
            return Ok(false);
        }
        let Some(id) = view.dev_id else {
            return Ok(false);
        };
        let Some(device) = devices.get(&id) else {
            return Ok(false);
        };
        if device.managed_session.as_deref() != Some(plan.session_id.as_str()) {
            return Ok(false);
        }
        let Some(path) = &view.device_path else {
            return Ok(false);
        };
        let metadata = std::fs::symlink_metadata(path)?;
        Ok(metadata.file_type().is_block_device()
            && libc::major(metadata.rdev()) == view.major as u32
            && libc::minor(metadata.rdev()) == view.minor as u32)
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle(
    state: &ManagedSessions,
    plan: ManagedDevicePlan,
    action: ManagedDeviceAction,
    devices: &DashMap<u32, ManagedDevice>,
    ctrl_ring: IoRingHandle<io_uring::squeue::Entry128>,
    cache: &ImageServiceCache,
    resize_tool: Option<&ResizeToolSpec>,
    resize_global_config: &Path,
    resize_permit: Arc<Mutex<()>>,
    recordings: &DashMap<u32, Arc<PackRecordingHandle>>,
) -> Result<DaemonResponse> {
    let mut operation = state.operation(&plan)?;
    operation.checkpoint("plan", serde_json::to_value(&plan)?)?;
    let mut view: Option<ManagedDeviceView> = operation
        .record()
        .checkpoints
        .get("device")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?;
    if view.is_none() {
        let candidates: Vec<_> = devices
            .iter()
            .filter(|device| device.managed_session.as_deref() == Some(plan.session_id.as_str()))
            .map(|device| state.device_view(&plan, &device))
            .collect::<Result<_>>()?;
        anyhow::ensure!(candidates.len() <= 1, "duplicate managed native devices");
        if let Some(recovered) = candidates.into_iter().next() {
            operation.checkpoint("device", serde_json::to_value(&recovered)?)?;
            view = Some(recovered);
        }
    }
    if operation.record().state == NativeOperationState::Completed {
        return Ok(DaemonResponse::ManagedDevice {
            view: state.pending(&plan, "Released"),
        });
    }
    match action {
        ManagedDeviceAction::Prepare => {
            anyhow::ensure!(
                !operation.record().checkpoints.contains_key("release"),
                "session release has started"
            );
            if let Some(ref existing) = view {
                anyhow::ensure!(
                    state.observed(&plan, existing, devices)?,
                    "managed device requires recovery; device identity changed"
                );
                return Ok(DaemonResponse::ManagedDevice {
                    view: existing.clone(),
                });
            }
            anyhow::ensure!(
                operation.record().state == NativeOperationState::Intent,
                "device creation result unknown; inspect original session"
            );
            operation.begin()?;
            let result = handle_create_overlaybd_runtime_device(
                OverlaybdRuntimeDeviceRequest {
                    source_image_config: &plan.source_image_config,
                    global_config: &plan.global_config,
                    runtime_dir: &plan.runtime_dir,
                    read_only: plan.read_only,
                    runtime_upper_mode: overlaybd::config::UpperMode::LogStructured,
                    requested_virtual_size: Some(plan.virtual_size),
                    known_source_virtual_size: Some(plan.virtual_size),
                    resize_tool,
                    resize_global_config,
                    resize_permit,
                    allow_shrink: false,
                    retain_on_error: true,
                    managed_session: Some(&plan.session_id),
                },
                ctrl_ring,
                devices,
                cache,
                &None,
            )
            .await;
            let DaemonResponse::OverlaybdRuntimeDeviceCreated {
                dev_id,
                device_path,
                actual_virtual_size,
                runtime_image_config_path,
            } = result?
            else {
                bail!("unexpected native managed-device create response")
            };
            anyhow::ensure!(
                actual_virtual_size == plan.virtual_size
                    && runtime_image_config_path == plan.runtime_dir.join("image.json")
                    && device_path == PathBuf::from(format!("/dev/ublkb{dev_id}")),
                "native create receipt differs from its fixed plan"
            );
            let ready = state.device_view(
                &plan,
                &*devices
                    .get(&dev_id)
                    .context("created managed device is missing")?,
            )?;
            devices
                .get_mut(&dev_id)
                .context("created managed device disappeared")?
                .managed_view = Some(ready.clone());
            operation.checkpoint("device", serde_json::to_value(&ready)?)?;
            Ok(DaemonResponse::ManagedDevice { view: ready })
        }
        ManagedDeviceAction::Inspect => {
            if operation.record().checkpoints.contains_key("release") {
                return Ok(DaemonResponse::ManagedDevice {
                    view: state.pending(&plan, "Releasing"),
                });
            }
            if let Some(ref existing) = view {
                if state.observed(&plan, existing, devices)? {
                    return Ok(DaemonResponse::ManagedDevice {
                        view: existing.clone(),
                    });
                }
            }
            Ok(DaemonResponse::ManagedDevice {
                view: state.pending(
                    &plan,
                    if operation.record().state == NativeOperationState::Intent {
                        "Pending"
                    } else {
                        "NeedsRecovery"
                    },
                ),
            })
        }
        ManagedDeviceAction::Capture {
            capture_id,
            barrier_id,
            freeze_evidence_digest,
            stopped_evidence_digest,
        } => {
            if plan.purpose != "runtime" {
                anyhow::bail!("preparation sessions cannot capture runtime content");
            }

            anyhow::ensure!(
                is_valid_volume_component(&capture_id)
                    && capture_id.len() <= 96
                    && !barrier_id.is_empty()
                    && digest_valid(&freeze_evidence_digest),
                "capture identity and freeze evidence required"
            );
            anyhow::ensure!(
                !operation.record().checkpoints.contains_key("release"),
                "session release already started"
            );
            let intent_key = format!("capture-intent-{capture_id}");
            let result_key = format!("capture-result-{capture_id}");
            let capture_intent =
                serde_json::json!({"barrier":barrier_id,"freeze":freeze_evidence_digest});
            // A durable sealed result remains usable after daemon restart. It
            // is not authority to reopen an upper or operate a replacement
            // device with the same number. Match the original barrier before
            // returning the immutable receipt without touching live devices.
            if let Some(value) = operation.record().checkpoints.get(&result_key) {
                anyhow::ensure!(
                    operation.record().checkpoints.get(&intent_key) == Some(&capture_intent),
                    "capture replay barrier changed"
                );
                return Ok(DaemonResponse::ManagedCapture {
                    capture: serde_json::from_value(value.clone())?,
                });
            }
            let ready = view.as_ref().context("capture device identity unknown")?;
            let observed = state.observed(&plan, ready, devices)?;
            if !observed {
                anyhow::ensure!(
                    digest_valid(&stopped_evidence_digest)
                        && stopped_evidence_digest == freeze_evidence_digest,
                    "offline upper capture requires independently verified stop and FD return"
                );
                let path = ready
                    .device_path
                    .as_ref()
                    .context("original device path missing")?;
                let id = ready.dev_id.context("original device ID missing")?;
                anyhow::ensure!(
                    absent(path)? && absent(&PathBuf::from(format!("/sys/block/ublkb{id}")))?,
                    "old kernel device still exists; refuse offline upper access"
                );
                anyhow::ensure!(
                    !plan.read_only,
                    "offline recovery is for stopped writable sessions"
                );
            }
            let version_key = format!("capture-phase-v1-{capture_id}");
            let started_key = format!("capture-restack-started-{capture_id}");
            let had_intent = operation.record().checkpoints.contains_key(&intent_key);
            let prior = had_intent
                && (operation.record().checkpoints.contains_key(&started_key)
                    || !operation.record().checkpoints.contains_key(&version_key));
            if !had_intent {
                operation.checkpoint(&version_key, serde_json::json!(1))?;
            }
            operation.checkpoint(&intent_key, capture_intent)?;
            // Do not restack a second capture while the first has unknown outcome.
            for key in operation.record().checkpoints.keys() {
                if let Some(id) = key.strip_prefix("capture-intent-") {
                    anyhow::ensure!(
                        id == capture_id
                            || operation
                                .record()
                                .checkpoints
                                .contains_key(&format!("capture-result-{id}")),
                        "earlier capture requires recovery"
                    );
                }
            }
            let output = plan
                .runtime_dir
                .join("captures")
                .join(format!("{capture_id}.commit"));
            let image = if observed {
                let device = devices
                    .get(&ready.dev_id.context("capture device number missing")?)
                    .context("capture device disappeared")?;
                Arc::clone(&device.image)
            } else {
                // Reuse the native image engine on the exact persistent upper.
                // This never materializes a new upper or exposes a replacement
                // block device. Unknown interrupted restacks remain fenced.
                let path = validate_recovery_image(&plan, ready)?;
                operation.checkpoint("offline-stop",serde_json::json!({"stop":stopped_evidence_digest,"device":ready.device_identity}))?;
                let service = cache.get_or_create(&plan.global_config).await?;
                let image = Arc::new(service.create_image_file(&path).await?);
                anyhow::ensure!(
                    image.size_bytes() == plan.virtual_size,
                    "recovered image capacity changed"
                );
                image
            };
            if !plan.read_only {
                let live = image.runtime_config().await;
                let sealed = live
                    .lowers
                    .iter()
                    .any(|layer| Path::new(&layer.file) == output);
                if prior && !sealed {
                    return Err(RestackSnapshotTerminalFailure::new(
                        "capture result unknown; refusing to restack again",
                    )
                    .into());
                }
                if !sealed {
                    operation.checkpoint(&started_key, serde_json::json!(true))?;
                    let config_path = ready
                        .runtime_image_config_path
                        .as_ref()
                        .context("runtime config missing")?;
                    image
                        .create_snapshot_and_restack_checkpointed(&output, |config| {
                            write_config(config_path, config)
                        })
                        .await?;
                }
            }
            let result: Result<ManagedCaptureView> = async {
                let runtime = image.runtime_config().await;
                // Persist the post-restack runtime before returning a capture;
                // otherwise daemon restart could reopen an old lower stack.
                write_config(
                    ready
                        .runtime_image_config_path
                        .as_ref()
                        .context("runtime config missing")?,
                    &runtime,
                )?;
                let mut sealed = runtime;
                sealed.upper = Default::default();
                let path = plan
                    .runtime_dir
                    .join("captures")
                    .join(format!("{capture_id}.json"));
                write_config(&path, &sealed)?;
                let content_digest =
                    aenv_volume_core::digest::sha256_digest(&serde_json::to_vec(&sealed)?);
                let capture = ManagedCaptureView {
                    capture_id,
                    barrier_id,
                    session_id: plan.session_id.clone(),
                    device_identity: ready.device_identity.clone(),
                    image_config_path: path,
                    content_digest,
                };
                operation.checkpoint(&result_key, serde_json::to_value(&capture)?)?;
                Ok(capture)
            }
            .await;
            result
                .map(|capture| DaemonResponse::ManagedCapture { capture })
                .map_err(|e| {
                    RestackSnapshotTerminalFailure::new(format!(
                        "capture persistence incomplete: {e:#}"
                    ))
                    .into()
                })
        }
        ManagedDeviceAction::Release {
            preparation_release_digest,
            stopped_evidence_digest,
            borrowed_refs_released_digest,
        } => {
            let valid = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
            anyhow::ensure!(
                match plan.purpose.as_str() {
                    "preparation" =>
                        valid(&preparation_release_digest) && stopped_evidence_digest.is_empty(),
                    "runtime" =>
                        valid(&stopped_evidence_digest)
                            && valid(&borrowed_refs_released_digest)
                            && preparation_release_digest.is_empty(),
                    _ => false,
                },
                "purpose-specific trusted release evidence required"
            );
            let ready = view.take().context("managed device identity is unknown")?;
            let observed = state.observed(&plan, &ready, devices)?;
            if !observed {
                let path = ready
                    .device_path
                    .as_ref()
                    .context("release device path missing")?;
                let id = ready.dev_id.context("release device number missing")?;
                anyhow::ensure!(
                    absent(path)? && absent(&PathBuf::from(format!("/sys/block/ublkb{id}")))?,
                    "managed device identity changed; release requires recovery"
                );
            }
            operation.checkpoint("release",serde_json::json!({"preparation":preparation_release_digest,"stop":stopped_evidence_digest,"borrowed":borrowed_refs_released_digest}))?;
            if observed {
                handle_delete(
                    devices,
                    ctrl_ring,
                    recordings,
                    ready.dev_id.context("managed device number missing")?,
                )
                .await?;
            }
            operation.complete(
                serde_json::json!({"released":true,"device_identity":ready.device_identity}),
            )?;
            Ok(DaemonResponse::ManagedDevice {
                view: state.pending(&plan, "Released"),
            })
        }
    }
}

fn digest_valid(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn absent(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e.into()),
    }
}
pub(super) fn write_config(path: &Path, config: &ImageConfig) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("config parent missing")?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&serde_json::to_vec(config)?)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|e| e.error)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod private_path_tests {
    use super::*;
    #[test]
    fn socket_parent_is_private_and_never_follows_a_symlink() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("private");
        private_directory(&parent).unwrap();
        assert_eq!(
            std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&parent, &alias).unwrap();
        assert!(private_directory(&alias).is_err());
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_directory(&parent).is_err());
    }
}

// Recover only the original persisted runtime configuration and existing upper.
// Never fall back to the base image, create missing files, or follow symlinks.
fn validate_recovery_image(plan: &ManagedDevicePlan, view: &ManagedDeviceView) -> Result<PathBuf> {
    let expected = plan.runtime_dir.join("image.json");
    anyhow::ensure!(
        view.runtime_image_config_path.as_ref() == Some(&expected),
        "recovery runtime configuration identity changed"
    );
    anyhow::ensure!(
        plan.runtime_dir.canonicalize()? == plan.runtime_dir,
        "recovery runtime directory is redirected"
    );
    let metadata = std::fs::symlink_metadata(&expected)?;
    anyhow::ensure!(
        metadata.is_file(),
        "recovery image must be an existing nonsymlink file"
    );
    let config: ImageConfig = serde_json::from_slice(&std::fs::read(&expected)?)?;
    anyhow::ensure!(
        !config.upper.data.is_empty() && !config.lowers.is_empty(),
        "recovery requires original upper and lower stack"
    );
    for file in [
        &config.upper.data,
        &config.upper.index,
        &config.upper.target,
        &config.upper.gzip_index,
    ] {
        if file.is_empty() {
            continue;
        }
        let path = overlaybd::helper::resolve_config_path(&plan.runtime_dir, file)?;
        let metadata = std::fs::symlink_metadata(&path)?;
        anyhow::ensure!(
            path.starts_with(&plan.runtime_dir)
                && path.canonicalize()? == path
                && metadata.is_file()
                && metadata.len() > 0,
            "recovery upper is missing, redirected, empty or outside its session"
        );
    }
    Ok(expected)
}

#[cfg(test)]
mod recovery_path_tests {
    use super::*;
    #[test]
    fn stopped_recovery_requires_existing_session_upper_without_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let upper = root.join("upper.data");
        std::fs::write(&upper, b"preserved upper fixture").unwrap();
        let config = ImageConfig {
            upper: UpperConfig {
                data: "upper.data".into(),
                ..Default::default()
            },
            lowers: vec![overlaybd::config::LayerConfig {
                file: "lower.commit".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        write_config(&root.join("image.json"), &config).unwrap();
        let plan = ManagedDevicePlan {
            purpose: "runtime".into(),
            session_id: "session".into(),
            operation_id: "operation".into(),
            repository_id: "repository".into(),
            volume_id: "volume".into(),
            owner_id: "owner".into(),
            run_id: "run".into(),
            epoch: 1,
            node_id: "node".into(),
            head_revision: "head".into(),
            source_image_config: root.join("source.json"),
            global_config: root.join("global.json"),
            runtime_dir: root.clone(),
            read_only: false,
            virtual_size: 4096,
        };
        let mut view = ManagedDeviceView {
            session_id: "session".into(),
            state: "Staged".into(),
            node_boot_id: "old-boot".into(),
            daemon_instance_id: "old-daemon".into(),
            device_identity: "original-device".into(),
            dev_id: Some(7),
            device_path: Some(PathBuf::from("/dev/ublkb7")),
            major: 1,
            minor: 7,
            actual_virtual_size: 4096,
            runtime_image_config_path: Some(root.join("image.json")),
        };
        assert!(validate_recovery_image(&plan, &view).is_ok());
        view.runtime_image_config_path = Some(root.join("source.json"));
        assert!(validate_recovery_image(&plan, &view).is_err());
        view.runtime_image_config_path = Some(root.join("image.json"));
        std::fs::remove_file(&upper).unwrap();
        assert!(validate_recovery_image(&plan, &view).is_err());
        assert!(!upper.exists()); // No blank upper was created.
        std::fs::write(&upper, b"").unwrap();
        assert!(validate_recovery_image(&plan, &view).is_err());
        std::fs::remove_file(&upper).unwrap();
        let elsewhere = directory.path().join("different-upper");
        std::fs::write(&elsewhere, b"do not touch").unwrap();
        std::os::unix::fs::symlink(&elsewhere, &upper).unwrap();
        assert!(validate_recovery_image(&plan, &view).is_err());
        assert_eq!(std::fs::read(&elsewhere).unwrap(), b"do not touch");
    }
}
