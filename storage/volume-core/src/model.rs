use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverlaybdLayerRef {
    Managed(ManagedLayer),
    External(ExternalLayer),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedLayer {
    pub digest: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalLayer {
    pub digest: String,
    pub repo_blob_url: String,
    pub size: u64,
}

pub const DEFAULT_VOLUME_SIZE_MB: u64 = 64 * 1024;
/// Firecracker exposes /dev/vdc through /dev/vdz for extra drives.
pub const MAX_VOLUME_MOUNTS: usize = 24;
pub const DEFAULT_VOLUME_PAGE_SIZE: usize = 100;
pub const BYTES_PER_MB: u64 = 1024 * 1024;

pub fn is_valid_volume_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VolumePage {
    pub records: Vec<VolumeRecord>,
    pub next_token: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum VolumeMode {
    ReadOnly,
    #[default]
    Exclusive,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VolumeStatus {
    #[default]
    Ready,
    Uploading,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeRecord {
    pub id: String,
    pub name: String,
    pub mode: VolumeMode,
    pub size_mb: u64,
    pub status: VolumeStatus,
    pub reserved_by_sandbox_id: Option<String>,
    #[serde(skip)]
    pub backing_image_config: Option<PathBuf>,
    /// Repository-owned layers; the image config path is only a node-local cache.
    pub backing_layers: Vec<OverlaybdLayerRef>,
    pub read_only_mounts: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleting: bool,
}

impl VolumeRecord {
    /// Maintenance-only compare-and-swap payload. Call under the repository
    /// record lock/ETag; ordinary put_volume deliberately cannot grow a volume.
    pub fn expanded_record(
        &self,
        owner: &str,
        expected_size_mb: u64,
        expected_layers: &[OverlaybdLayerRef],
        size_mb: u64,
        layers: &[OverlaybdLayerRef],
    ) -> Result<Self, String> {
        if !owner.starts_with("maintenance_")
            || owner.len() <= "maintenance_".len()
            || self.reserved_by_sandbox_id.as_deref() != Some(owner)
            || !self.read_only_mounts.is_empty()
            || self.deleting
            || self.mode != VolumeMode::Exclusive
            || self.status != VolumeStatus::Ready
            || expected_size_mb == 0
            || size_mb <= expected_size_mb
            || layers.is_empty()
        {
            return Err(
                "exclusive ready maintenance reservation and growing content required".into(),
            );
        }
        // A lost response may replay exactly the already committed content.
        if self.size_mb == size_mb && self.backing_layers == layers {
            return Ok(self.clone());
        }
        if self.size_mb != expected_size_mb || self.backing_layers != expected_layers {
            return Err("maintenance source capacity or backing changed".into());
        }
        let mut next = self.clone();
        next.size_mb = size_mb;
        next.backing_layers = layers.to_vec();
        next.backing_image_config = None;
        Ok(next)
    }

    pub fn mounted_by(&self, owner: &str) -> bool {
        self.reserved_by_sandbox_id.as_deref() == Some(owner)
            || self.read_only_mounts.iter().any(|entry| entry == owner)
    }

    pub fn replace_owner(&mut self, from: &str, to: Option<&str>) -> bool {
        let mut changed = false;
        if self.reserved_by_sandbox_id.as_deref() == Some(from) {
            self.reserved_by_sandbox_id = to.map(str::to_owned);
            changed = true;
        }
        if self.read_only_mounts.iter().any(|owner| owner == from) {
            self.read_only_mounts.retain(|owner| owner != from);
            if let Some(to) = to {
                if !self.read_only_mounts.iter().any(|owner| owner == to) {
                    self.read_only_mounts.push(to.to_owned());
                }
            }
            changed = true;
        }
        changed
    }

    pub fn validate_catalog_update(&self, next: &Self) -> Result<(), String> {
        if self.deleting {
            return Err(format!("volume '{}' is being deleted", self.id));
        }
        if self.name != next.name || self.mode != next.mode || self.size_mb != next.size_mb {
            return Err("volume identity fields cannot be changed".to_owned());
        }
        if self.backing_layers != next.backing_layers && self.status != VolumeStatus::Uploading {
            return Err(format!(
                "volume '{}' must enter uploading state before replacing its backing",
                self.id
            ));
        }
        if self.status == VolumeStatus::Failed && next.status == VolumeStatus::Ready {
            return Err(format!(
                "volume '{}' must enter uploading state before becoming ready",
                self.id
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum VolumeError {
    #[error("volume name must contain only letters, numbers, underscores, or hyphens")]
    InvalidName,
    #[error("volume name already exists: {0}")]
    NameConflict(String),
    #[error("volume not found: {0}")]
    NotFound(String),
    #[error("volume is reserved by sandbox {0}")]
    Reserved(String),
    #[error("volume is uploading and not usable: {0}")]
    Uploading(String),
    #[error("volume publication failed and is not usable: {0}")]
    Failed(String),
    #[error("fromVolume and image cannot be used together")]
    MultipleSources,
    #[error("volume size must be greater than zero")]
    InvalidSize,
    #[error("volume size exceeds the configured maximum of {max_size_mb} MiB")]
    SizeLimitExceeded { max_size_mb: u64 },
    #[error("volume child size must match its source size")]
    SizeMismatch,
    #[error("sandbox cannot mount more than {max_count} volumes")]
    TooManyMountedVolumes { max_count: usize },
    #[error("source volume not found: {0}")]
    SourceNotFound(String),
    #[error("invalid volume next token")]
    InvalidNextToken,
    #[error("volume page limit must be greater than zero")]
    InvalidPageLimit,
    #[error("volume catalog storage failed: {0}")]
    Storage(String),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VolumeRecordPage {
    pub records: Vec<VolumeRecord>,
    /// The last returned volume ID. Supplying it to the next request resumes
    /// after that record in the repository's stable shard-and-ID order.
    pub next_volume_id: Option<String>,
}
