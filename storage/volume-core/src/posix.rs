//! Native POSIX volume catalog extracted from the snapshot catalog.
use crate::catalog::{CatalogError as RepositoryError, CatalogResult as RepositoryResult};
use crate::{is_valid_volume_component, VolumeMode, VolumeRecord, VolumeRecordPage};
use nix::{
    errno::Errno,
    fcntl::{Flock, FlockArg},
};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
const FILE_LOCK_TIMEOUT: Option<Duration> = Some(Duration::from_secs(10));
const LOCK_SUFFIX: &str = ".lock";
pub struct VolumeLayout;
impl VolumeLayout {
    pub fn managed_layers_dir(root: &Path) -> PathBuf {
        root.join("managed-layers")
    }
    pub fn managed_layer_path(root: &Path, digest: &str) -> PathBuf {
        Self::managed_layers_dir(root).join(format!(
            "{}.overlaybd.commit",
            digest.replace([':', '/'], "_")
        ))
    }

    pub fn volumes_dir(root: &Path) -> PathBuf {
        root.join("volumes")
    }

    pub fn volume_aliases_dir(root: &Path) -> PathBuf {
        Self::volumes_dir(root).join("aliases")
    }

    pub fn volume_records_dir(root: &Path) -> PathBuf {
        Self::volumes_dir(root).join("records")
    }

    pub fn volume_record_path(root: &Path, volume_id: &str) -> PathBuf {
        Self::volume_records_dir(root).join(format!("{volume_id}.json"))
    }

    pub fn volume_alias_path(root: &Path, alias: &str) -> PathBuf {
        Self::volume_aliases_dir(root).join(alias)
    }

    pub fn volume_alias_lock_path(root: &Path, alias: &str) -> PathBuf {
        Self::volume_aliases_dir(root).join(format!("{alias}{LOCK_SUFFIX}"))
    }

    pub fn volume_record_lock_path(root: &Path, volume_id: &str) -> PathBuf {
        Self::volume_records_dir(root).join(format!("{volume_id}{LOCK_SUFFIX}"))
    }
}
#[derive(Debug)]
struct PosixFileLockGuard {
    _file: Flock<fs::File>,
}
pub struct PosixVolumeCatalog {
    root: PathBuf,
}
impl PosixVolumeCatalog {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    pub fn get_volume(&self, reference: &str) -> RepositoryResult<Option<VolumeRecord>> {
        self.ensure_volume_component(reference, "reference")?;
        if let Some(record) = self.load_volume_by_id_unlocked(reference)? {
            return Ok(Some(record));
        }
        self.with_volume_alias_lock(reference, |store| {
            let alias_path = VolumeLayout::volume_alias_path(&store.root, reference);
            if !alias_path.exists() {
                return Ok(None);
            }
            let volume_id: String = store.read_json(&alias_path)?;
            store.ensure_volume_id(&volume_id)?;
            match store.load_volume_by_id_unlocked(&volume_id)? {
                Some(record) => Ok(Some(record)),
                None => {
                    store.remove_file_if_exists(&alias_path)?;
                    Ok(None)
                }
            }
        })
    }

    pub fn list_volumes_page(
        &self,
        after_volume_id: Option<&str>,
        limit: usize,
    ) -> RepositoryResult<VolumeRecordPage> {
        if limit == 0 {
            return Err(RepositoryError::InvalidRequest {
                reason: "volume page limit must be greater than zero".to_string(),
            });
        }
        if let Some(volume_id) = after_volume_id {
            self.ensure_volume_id(volume_id)?;
        }
        let mut selected = self.volume_ids_unlocked()?;
        if let Some(after) = after_volume_id {
            selected.retain(|volume_id| volume_id.as_str() > after);
        }

        let has_more = selected.len() > limit;
        selected.truncate(limit);
        let mut records = Vec::with_capacity(selected.len());
        for volume_id in selected {
            if let Some(record) = self.load_volume_by_id_unlocked(&volume_id)? {
                records.push(record);
            }
        }
        let next_volume_id = has_more
            .then(|| records.last().map(|record| record.id.clone()))
            .flatten();
        Ok(VolumeRecordPage {
            records,
            next_volume_id,
        })
    }

    pub fn create_volume(&self, record: &VolumeRecord) -> RepositoryResult<()> {
        self.ensure_volume_id(&record.id)?;
        self.ensure_volume_component(&record.name, "name")?;
        self.with_volume_alias_lock(&record.name, |store| {
            let _record_guard = store.acquire_volume_record_lock(&record.id)?;
            let record_path = VolumeLayout::volume_record_path(&store.root, &record.id);
            if record_path.exists() {
                return Err(RepositoryError::InvalidRequest {
                    reason: format!("volume '{}' already exists", record.id),
                });
            }
            let alias_path = VolumeLayout::volume_alias_path(&store.root, &record.name);
            if alias_path.exists() {
                let existing_id = store.read_json::<String>(&alias_path)?;
                store.ensure_volume_id(&existing_id)?;
                if VolumeLayout::volume_record_path(&store.root, &existing_id).exists() {
                    return Err(RepositoryError::VolumeNameConflict {
                        name: record.name.clone(),
                    });
                }
                store.remove_file_if_exists(&alias_path)?;
            }
            if VolumeLayout::volume_record_path(&store.root, &record.name).exists()
                || VolumeLayout::volume_alias_path(&store.root, &record.id).exists()
            {
                return Err(RepositoryError::VolumeNameConflict {
                    name: record.name.clone(),
                });
            }

            let mut durable_record = record.clone();
            durable_record.backing_image_config = None;
            store.write_json(&record_path, &durable_record)?;
            if let Err(error) = store.write_json(&alias_path, &record.id) {
                let _ = store.remove_file_if_exists(&record_path);
                return Err(error);
            }
            Ok(())
        })
    }

    pub fn put_volume(&self, record: &VolumeRecord) -> RepositoryResult<()> {
        self.ensure_volume_id(&record.id)?;
        self.ensure_volume_component(&record.name, "name")?;
        let _guard = self.acquire_volume_record_lock(&record.id)?;
        let path = VolumeLayout::volume_record_path(&self.root, &record.id);
        if !path.exists() {
            return Err(RepositoryError::VolumeNotFound {
                lookup: record.id.clone(),
            });
        }
        let existing = self.read_json::<VolumeRecord>(&path)?;
        self.ensure_volume_id(&existing.id)?;
        existing
            .validate_catalog_update(record)
            .map_err(|reason| RepositoryError::InvalidRequest { reason })?;
        let mut durable_record = record.clone();
        durable_record.backing_image_config = None;
        // Reservation state is owned exclusively by the reservation APIs. A
        // concurrent backing/status update must not restore a released owner.
        durable_record.reserved_by_sandbox_id = existing.reserved_by_sandbox_id;
        durable_record.read_only_mounts = existing.read_only_mounts;
        self.write_json(&path, &durable_record)
    }

    pub fn commit_expansion(
        &self,
        id: &str,
        owner: &str,
        expected_size_mb: u64,
        expected_layers: &[crate::OverlaybdLayerRef],
        size_mb: u64,
        layers: &[crate::OverlaybdLayerRef],
    ) -> RepositoryResult<()> {
        self.ensure_volume_id(id)?;
        let _guard = self.acquire_volume_record_lock(id)?;
        let existing = self.load_volume_by_id_unlocked(id)?.ok_or_else(|| {
            RepositoryError::VolumeNotFound {
                lookup: id.to_owned(),
            }
        })?;
        let next = existing
            .expanded_record(owner, expected_size_mb, expected_layers, size_mb, layers)
            .map_err(|reason| RepositoryError::InvalidRequest { reason })?;
        self.write_json(&VolumeLayout::volume_record_path(&self.root, id), &next)
    }

    pub fn delete_volume(&self, volume_id: &str) -> RepositoryResult<()> {
        self.ensure_volume_id(volume_id)?;
        let Some(existing) = self.load_volume_by_id_unlocked(volume_id)? else {
            return Ok(());
        };
        self.with_volume_alias_lock(&existing.name, |store| {
            let _record_guard = store.acquire_volume_record_lock(volume_id)?;
            let Some(record) = store.load_volume_by_id_unlocked(volume_id)? else {
                return Ok(());
            };
            if let Some(owner) = record.reserved_by_sandbox_id {
                return Err(RepositoryError::InvalidRequest {
                    reason: format!("volume '{volume_id}' is reserved by sandbox '{owner}'"),
                });
            }
            if let Some(owner) = record.read_only_mounts.first() {
                return Err(RepositoryError::InvalidRequest {
                    reason: format!(
                        "volume '{volume_id}' is mounted read-only by sandbox '{owner}'"
                    ),
                });
            }
            store
                .remove_file_if_exists(&VolumeLayout::volume_record_path(&store.root, volume_id))?;
            let alias_path = VolumeLayout::volume_alias_path(&store.root, &record.name);
            if alias_path.exists() && store.read_json::<String>(&alias_path)? == volume_id {
                store.remove_file_if_exists(&alias_path)?;
            }
            Ok(())
        })
    }

    pub fn reserve_volume(&self, volume_id: &str, owner: &str) -> RepositoryResult<Option<String>> {
        self.ensure_volume_id(volume_id)?;
        self.ensure_volume_component(owner, "owner")?;
        let _guard = self.acquire_volume_record_lock(volume_id)?;
        let path = VolumeLayout::volume_record_path(&self.root, volume_id);
        let mut record = self.load_volume_by_id_unlocked(volume_id)?.ok_or_else(|| {
            RepositoryError::VolumeNotFound {
                lookup: volume_id.to_string(),
            }
        })?;
        if record.mode == VolumeMode::ReadOnly {
            return Ok(None);
        }
        if let Some(existing) = record.reserved_by_sandbox_id.as_deref() {
            if existing != owner {
                return Ok(Some(existing.to_owned()));
            }
            return Ok(None);
        }
        record.reserved_by_sandbox_id = Some(owner.to_owned());
        self.write_json(&path, &record)?;
        Ok(None)
    }

    pub fn reserve_read_only_volume(&self, volume_id: &str, owner: &str) -> RepositoryResult<()> {
        self.ensure_volume_id(volume_id)?;
        self.ensure_volume_component(owner, "owner")?;
        let _guard = self.acquire_volume_record_lock(volume_id)?;
        let path = VolumeLayout::volume_record_path(&self.root, volume_id);
        let mut record = self.load_volume_by_id_unlocked(volume_id)?.ok_or_else(|| {
            RepositoryError::VolumeNotFound {
                lookup: volume_id.to_string(),
            }
        })?;
        if record.mode != VolumeMode::ReadOnly {
            return Err(RepositoryError::InvalidRequest {
                reason: format!("volume '{volume_id}' is not read-only"),
            });
        }
        if record.read_only_mounts.iter().any(|entry| entry == owner) {
            return Ok(());
        }
        record.read_only_mounts.push(owner.to_owned());
        self.write_json(&path, &record)?;
        Ok(())
    }

    pub fn replace_volume_owner_for(
        &self,
        volume_id: &str,
        from: &str,
        to: Option<&str>,
    ) -> RepositoryResult<()> {
        self.ensure_volume_id(volume_id)?;
        self.ensure_volume_component(from, "owner")?;
        if let Some(to) = to {
            self.ensure_volume_component(to, "owner")?;
        }
        if to == Some(from) {
            return Ok(());
        }
        let _guard = self.acquire_volume_record_lock(volume_id)?;
        let path = VolumeLayout::volume_record_path(&self.root, volume_id);
        let mut record = self.load_volume_by_id_unlocked(volume_id)?.ok_or_else(|| {
            RepositoryError::VolumeNotFound {
                lookup: volume_id.to_owned(),
            }
        })?;
        if record.replace_owner(from, to) {
            self.write_json(&path, &record)?;
        }
        Ok(())
    }

    fn volume_ids_unlocked(&self) -> RepositoryResult<Vec<String>> {
        let directory = VolumeLayout::volume_records_dir(&self.root);
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(RepositoryError::backend(
                    format!("read volume directory '{}'", directory.display()),
                    error,
                ))
            }
        };
        let mut volume_ids = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|error| RepositoryError::backend("read volume entry", error))?;
            if !entry
                .file_type()
                .map_err(|error| RepositoryError::backend("inspect volume entry", error))?
                .is_file()
                || entry.path().extension().and_then(|value| value.to_str()) != Some("json")
            {
                continue;
            }
            let path = entry.path();
            let Some(volume_id) = path.file_stem().and_then(|value| value.to_str()) else {
                return Err(RepositoryError::InvalidRequest {
                    reason: format!("invalid volume record path '{}'", path.display()),
                });
            };
            self.ensure_volume_id(volume_id)?;
            volume_ids.push(volume_id.to_owned());
        }
        volume_ids.sort_unstable();
        Ok(volume_ids)
    }

    fn load_volume_by_id_unlocked(
        &self,
        volume_id: &str,
    ) -> RepositoryResult<Option<VolumeRecord>> {
        self.ensure_volume_id(volume_id)?;
        let path = VolumeLayout::volume_record_path(&self.root, volume_id);
        if !path.exists() {
            return Ok(None);
        }
        let record: VolumeRecord = self.read_json(&path)?;
        self.ensure_volume_id(&record.id)?;
        if record.id != volume_id {
            return Err(RepositoryError::InvalidRequest {
                reason: format!(
                    "volume record id '{}' does not match path '{}'",
                    record.id,
                    path.display()
                ),
            });
        }
        Ok(Some(record))
    }

    fn ensure_volume_id(&self, volume_id: &str) -> RepositoryResult<()> {
        self.ensure_volume_component(volume_id, "id")
    }

    fn ensure_volume_component(&self, value: &str, kind: &str) -> RepositoryResult<()> {
        if !is_valid_volume_component(value) {
            return Err(RepositoryError::InvalidRequest {
                reason: format!("invalid volume {kind} '{value}'"),
            });
        }
        Ok(())
    }

    fn read_json<T>(&self, path: &Path) -> RepositoryResult<T>
    where
        T: DeserializeOwned,
    {
        let bytes = fs::read(path).map_err(|error| {
            RepositoryError::backend(format!("read '{}'", path.display()), error)
        })?;
        serde_json::from_slice(&bytes).map_err(|error| {
            RepositoryError::backend(format!("parse json '{}'", path.display()), error)
        })
    }

    fn write_json<T>(&self, path: &Path, value: &T) -> RepositoryResult<()>
    where
        T: Serialize,
    {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                RepositoryError::backend(format!("create '{}'", parent.display()), error)
            })?;
        }
        let bytes = serde_json::to_vec_pretty(value).map_err(|error| {
            RepositoryError::backend(format!("serialize json '{}'", path.display()), error)
        })?;
        let parent = path.parent().ok_or_else(|| RepositoryError::Backend {
            message: format!("resolve parent for '{}'", path.display()),
            source: None,
        })?;
        let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
            RepositoryError::backend(format!("create temp file in '{}'", parent.display()), error)
        })?;
        temp.write_all(&bytes).map_err(|error| {
            RepositoryError::backend(
                format!("write temp json '{}'", temp.path().display()),
                error,
            )
        })?;
        temp.as_file().sync_all().map_err(|error| {
            RepositoryError::backend(format!("sync temp json '{}'", temp.path().display()), error)
        })?;
        let tmp_path = temp.path().to_path_buf();
        temp.persist(path).map_err(|error| {
            RepositoryError::backend(
                format!(
                    "persist json '{}' -> '{}'",
                    tmp_path.display(),
                    path.display()
                ),
                error.error,
            )
        })?;
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| RepositoryError::backend("sync catalog directory", error))?;
        Ok(())
    }

    fn remove_file_if_exists(&self, path: &Path) -> RepositoryResult<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(RepositoryError::backend(
                format!("remove '{}'", path.display()),
                error,
            )),
        }
    }

    fn acquire_file_lock(
        &self,
        lock_path: PathBuf,
        contents: String,
        label: &'static str,
        on_locked: impl Fn() -> RepositoryResult<PosixFileLockGuard>,
    ) -> RepositoryResult<PosixFileLockGuard> {
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                RepositoryError::backend(
                    format!("create {label} lock dir '{}'", parent.display()),
                    error,
                )
            })?;
        }

        let deadline = FILE_LOCK_TIMEOUT.map(|timeout| Instant::now() + timeout);
        loop {
            let file = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&lock_path)
                .map_err(|error| {
                    RepositoryError::backend(
                        format!("open {label} lock '{}'", lock_path.display()),
                        error,
                    )
                })?;
            match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(mut file) => {
                    file.set_len(0).map_err(|error| {
                        RepositoryError::backend(
                            format!("truncate {label} lock '{}'", lock_path.display()),
                            error,
                        )
                    })?;
                    file.write_all(contents.as_bytes()).map_err(|error| {
                        RepositoryError::backend(
                            format!("write {label} lock '{}'", lock_path.display()),
                            error,
                        )
                    })?;
                    return Ok(PosixFileLockGuard { _file: file });
                }
                Err((_file, error)) if error == Errno::EAGAIN || error == Errno::EWOULDBLOCK => {
                    if let Some(deadline) = deadline {
                        if Instant::now() < deadline {
                            thread::sleep(Duration::from_millis(25));
                            continue;
                        }
                    }
                    return on_locked();
                }
                Err((_file, error)) => {
                    return Err(RepositoryError::backend(
                        format!("lock {label} lock '{}'", lock_path.display()),
                        error,
                    ));
                }
            }
        }
    }

    fn acquire_volume_alias_lock(&self, alias: &str) -> RepositoryResult<PosixFileLockGuard> {
        self.acquire_catalog_lock(
            VolumeLayout::volume_alias_lock_path(&self.root, alias),
            "volume alias",
        )
    }

    fn acquire_volume_record_lock(&self, id: &str) -> RepositoryResult<PosixFileLockGuard> {
        self.acquire_catalog_lock(
            VolumeLayout::volume_record_lock_path(&self.root, id),
            "volume record",
        )
    }

    fn acquire_catalog_lock(
        &self,
        lock_path: PathBuf,
        label: &'static str,
    ) -> RepositoryResult<PosixFileLockGuard> {
        self.acquire_file_lock(
            lock_path.clone(),
            std::process::id().to_string(),
            label,
            || {
                Err(RepositoryError::Backend {
                    message: format!(
                        "timed out waiting for {label} lock '{}'",
                        lock_path.display()
                    ),
                    source: None,
                })
            },
        )
    }

    fn with_volume_alias_lock<T>(
        &self,
        alias: &str,
        action: impl FnOnce(&Self) -> RepositoryResult<T>,
    ) -> RepositoryResult<T> {
        let _guard = self.acquire_volume_alias_lock(alias)?;
        action(self)
    }
}
