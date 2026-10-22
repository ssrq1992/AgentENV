//! Native OSS volume CAS operations. The transport is supplied by the host.
use crate::catalog::{CatalogError as RepositoryError, CatalogResult as RepositoryResult};
use crate::{is_valid_volume_component, VolumeMode, VolumeRecord, VolumeRecordPage, VolumeStatus};
use std::sync::Arc;
const MAX_VOLUME_CAS_ATTEMPTS: usize = 5;
pub struct ObjectVolumeLayout;
impl ObjectVolumeLayout {
    pub fn volume_record_key(volume_id: &str) -> String {
        format!("volumes/records/{volume_id}.json")
    }

    pub fn volume_records_prefix() -> &'static str {
        "volumes/records/"
    }

    pub fn volume_alias_key(alias: &str) -> String {
        format!("volumes/aliases/{alias}.json")
    }
}
#[async_trait::async_trait]
pub trait VolumeObjectStore: Send + Sync {
    fn is_not_found_error(error: &anyhow::Error) -> bool;
    async fn get_bytes(&self, key: &str) -> anyhow::Result<Vec<u8>>;
    async fn get_bytes_with_etag(&self, key: &str) -> anyhow::Result<(Vec<u8>, Option<String>)>;
    async fn exists(&self, key: &str) -> anyhow::Result<bool>;
    async fn list_keys_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<String>>;
    async fn put_bytes_conditionally(
        &self,
        key: &str,
        bytes: Vec<u8>,
        etag: Option<&str>,
    ) -> anyhow::Result<bool>;
    async fn delete(&self, key: &str) -> anyhow::Result<()>;
}
pub struct ObjectVolumeCatalog<C: VolumeObjectStore> {
    client: Arc<C>,
}
impl<C: VolumeObjectStore> ObjectVolumeCatalog<C> {
    pub fn new(client: Arc<C>) -> Self {
        Self { client }
    }
    pub async fn get_volume(&self, reference: &str) -> RepositoryResult<Option<VolumeRecord>> {
        validate_volume_component(reference, "reference")?;
        if let Some(record) = self.read_volume_record(reference).await? {
            return Ok(Some(record));
        }
        let alias_key = ObjectVolumeLayout::volume_alias_key(reference);
        let volume_id = match self.client.get_bytes(&alias_key).await {
            Ok(bytes) => serde_json::from_slice::<String>(&bytes).map_err(|error| {
                RepositoryError::backend(format!("parse volume alias '{alias_key}'"), error)
            })?,
            Err(error) if C::is_not_found_error(&error) => return Ok(None),
            Err(error) => {
                return Err(RepositoryError::backend(
                    format!("read volume alias '{alias_key}'"),
                    error,
                ))
            }
        };
        self.read_volume_record(&volume_id).await
    }

    pub async fn list_volumes_page(
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
            validate_volume_id(volume_id)?;
        }
        let start_after = after_volume_id.map(ObjectVolumeLayout::volume_record_key);
        let mut keys = self
            .client
            .list_keys_page(
                ObjectVolumeLayout::volume_records_prefix(),
                start_after.as_deref(),
                limit.saturating_add(1),
            )
            .await
            .map_err(|error| RepositoryError::backend("list volume records", error))?;
        let has_more = keys.len() > limit;
        keys.truncate(limit);
        let mut records = Vec::with_capacity(keys.len());
        for key in keys {
            let volume_id = volume_id_from_record_key(&key)?;
            let record = self.read_volume_record(&volume_id).await?.ok_or_else(|| {
                RepositoryError::VolumeNotFound {
                    lookup: volume_id.clone(),
                }
            })?;
            records.push(record);
        }
        let next_volume_id = if has_more {
            records.last().map(|record| record.id.clone())
        } else {
            None
        };
        Ok(VolumeRecordPage {
            records,
            next_volume_id,
        })
    }

    pub async fn create_volume(&self, record: VolumeRecord) -> RepositoryResult<()> {
        validate_volume_id(&record.id)?;
        validate_volume_component(&record.name, "name")?;
        let mut record = record;
        record.backing_image_config = None;
        if self.read_volume_record(&record.name).await?.is_some()
            || self
                .client
                .exists(&ObjectVolumeLayout::volume_alias_key(&record.id))
                .await
                .map_err(|error| RepositoryError::backend("check volume ID namespace", error))?
        {
            return Err(RepositoryError::VolumeNameConflict {
                name: record.name.clone(),
            });
        }
        if !self.claim_volume_alias(&record.name, &record.id).await? {
            return Err(RepositoryError::VolumeNameConflict {
                name: record.name.clone(),
            });
        }
        match self.write_volume_record_conditionally(&record, None).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(RepositoryError::InvalidRequest {
                reason: format!("volume '{}' already exists", record.id),
            }),
            Err(error) => Err(error),
        }
    }

    pub async fn put_volume(&self, record: VolumeRecord) -> RepositoryResult<()> {
        validate_volume_id(&record.id)?;
        let mut record = record;
        record.backing_image_config = None;
        for _attempt in 0..MAX_VOLUME_CAS_ATTEMPTS {
            let (next, etag) = match self.read_volume_record_versioned(&record.id).await? {
                Some((existing, etag)) => {
                    existing
                        .validate_catalog_update(&record)
                        .map_err(|reason| RepositoryError::InvalidRequest { reason })?;
                    let mut next = record.clone();
                    // Reservation state is owned exclusively by the
                    // reservation APIs. A concurrent backing/status update
                    // must not restore a released owner.
                    next.reserved_by_sandbox_id = existing.reserved_by_sandbox_id;
                    next.read_only_mounts = existing.read_only_mounts;
                    (next, etag)
                }
                None => {
                    return Err(RepositoryError::VolumeNotFound {
                        lookup: record.id.clone(),
                    })
                }
            };
            if self
                .write_volume_record_conditionally(&next, etag.as_deref())
                .await?
            {
                return Ok(());
            }
        }
        Err(RepositoryError::Backend {
            message: format!(
                "volume record '{}' changed too often while publishing",
                record.id
            ),
            source: None,
        })
    }

    pub async fn commit_expansion(
        &self,
        id: &str,
        owner: &str,
        expected_size_mb: u64,
        expected_layers: &[crate::OverlaybdLayerRef],
        size_mb: u64,
        layers: &[crate::OverlaybdLayerRef],
    ) -> RepositoryResult<()> {
        validate_volume_id(id)?;
        for _attempt in 0..MAX_VOLUME_CAS_ATTEMPTS {
            let (record, etag) = self
                .read_volume_record_versioned(id)
                .await?
                .ok_or_else(|| RepositoryError::VolumeNotFound {
                    lookup: id.to_owned(),
                })?;
            let next = record
                .expanded_record(owner, expected_size_mb, expected_layers, size_mb, layers)
                .map_err(|reason| RepositoryError::InvalidRequest { reason })?;
            if self
                .write_volume_record_conditionally(&next, etag.as_deref())
                .await?
            {
                return Ok(());
            }
        }
        Err(RepositoryError::Backend {
            message: format!("volume '{id}' changed too often during maintenance publication"),
            source: None,
        })
    }

    pub async fn delete_volume(&self, volume_id: &str) -> RepositoryResult<()> {
        validate_volume_id(volume_id)?;
        let mut deleted_record = None;
        for _attempt in 0..MAX_VOLUME_CAS_ATTEMPTS {
            let Some((mut record, etag)) = self.read_volume_record_versioned(volume_id).await?
            else {
                return Ok(());
            };
            if let Some(owner) = record.reserved_by_sandbox_id.as_deref() {
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
            if !record.deleting {
                record.deleting = true;
                record.status = VolumeStatus::Failed;
                if !self
                    .write_volume_record_conditionally(&record, etag.as_deref())
                    .await?
                {
                    continue;
                }
            }
            deleted_record = Some(record);
            break;
        }
        let record = deleted_record.ok_or_else(|| RepositoryError::Backend {
            message: format!("volume '{volume_id}' changed too often while deleting"),
            source: None,
        })?;
        self.client
            .delete(&ObjectVolumeLayout::volume_record_key(volume_id))
            .await
            .map_err(|error| RepositoryError::backend("delete volume record", error))?;
        let alias_key = ObjectVolumeLayout::volume_alias_key(&record.name);
        let owns_alias = match self.client.get_bytes(&alias_key).await {
            Ok(bytes) => {
                serde_json::from_slice::<String>(&bytes)
                    .map_err(|error| RepositoryError::backend("parse volume alias", error))?
                    == volume_id
            }
            Err(error) if C::is_not_found_error(&error) => false,
            Err(error) => {
                return Err(RepositoryError::backend(
                    "read volume alias before delete",
                    error,
                ))
            }
        };
        if owns_alias {
            self.client
                .delete(&alias_key)
                .await
                .map_err(|error| RepositoryError::backend("delete volume alias", error))?;
        }
        Ok(())
    }

    pub async fn reserve_volume(
        &self,
        volume_id: &str,
        owner: &str,
    ) -> RepositoryResult<Option<String>> {
        validate_volume_id(volume_id)?;
        validate_volume_component(owner, "owner")?;
        for _attempt in 0..MAX_VOLUME_CAS_ATTEMPTS {
            let Some((mut record, etag)) = self.read_volume_record_versioned(volume_id).await?
            else {
                return Err(RepositoryError::InvalidRequest {
                    reason: format!("volume '{volume_id}' does not exist in the repository"),
                });
            };
            if record.deleting || record.status != VolumeStatus::Ready {
                return Err(RepositoryError::InvalidRequest {
                    reason: format!("volume '{volume_id}' is not usable"),
                });
            }
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
            if self
                .write_volume_record_conditionally(&record, etag.as_deref())
                .await?
            {
                return Ok(None);
            }
        }
        Err(RepositoryError::Backend {
            message: format!("volume '{volume_id}' changed too often while reserving"),
            source: None,
        })
    }

    pub async fn reserve_read_only_volume(
        &self,
        volume_id: &str,
        owner: &str,
    ) -> RepositoryResult<()> {
        validate_volume_id(volume_id)?;
        validate_volume_component(owner, "owner")?;
        for _attempt in 0..MAX_VOLUME_CAS_ATTEMPTS {
            let Some((mut record, etag)) = self.read_volume_record_versioned(volume_id).await?
            else {
                return Err(RepositoryError::InvalidRequest {
                    reason: format!("volume '{volume_id}' does not exist in the repository"),
                });
            };
            if record.deleting || record.status != VolumeStatus::Ready {
                return Err(RepositoryError::InvalidRequest {
                    reason: format!("volume '{volume_id}' is not usable"),
                });
            }
            if record.mode != VolumeMode::ReadOnly {
                return Err(RepositoryError::InvalidRequest {
                    reason: format!("volume '{volume_id}' is not read-only"),
                });
            }
            if record.read_only_mounts.iter().any(|entry| entry == owner) {
                return Ok(());
            }
            record.read_only_mounts.push(owner.to_owned());
            if self
                .write_volume_record_conditionally(&record, etag.as_deref())
                .await?
            {
                return Ok(());
            }
        }
        Err(RepositoryError::Backend {
            message: format!("volume '{volume_id}' changed too often while reserving read-only"),
            source: None,
        })
    }

    pub async fn replace_volume_owner_for(
        &self,
        volume_id: &str,
        from: &str,
        to: Option<&str>,
    ) -> RepositoryResult<()> {
        validate_volume_id(volume_id)?;
        validate_volume_component(from, "owner")?;
        if let Some(to) = to {
            validate_volume_component(to, "owner")?;
        }
        if to == Some(from) {
            return Ok(());
        }
        self.replace_volume_record_owner(volume_id, from, to).await
    }

    pub async fn claim_volume_alias(&self, name: &str, volume_id: &str) -> RepositoryResult<bool> {
        let key = ObjectVolumeLayout::volume_alias_key(name);
        let bytes = serde_json::to_vec(volume_id)
            .map_err(|error| RepositoryError::backend("serialize volume alias", error))?;
        for _attempt in 0..MAX_VOLUME_CAS_ATTEMPTS {
            let etag: Option<String> = match self.client.get_bytes_with_etag(&key).await {
                Ok((existing, _etag)) => {
                    let existing_id = serde_json::from_slice::<String>(&existing)
                        .map_err(|error| RepositoryError::backend("parse volume alias", error))?;
                    validate_volume_id(&existing_id)?;
                    if existing_id == volume_id {
                        return Ok(true);
                    }
                    // An alias with no visible record can be an in-flight create.
                    // Only its recorded owner may finish that operation. Never steal
                    // it on a retry merely because the record is not yet visible.
                    return Ok(false);
                }
                Err(error) if C::is_not_found_error(&error) => None,
                Err(error) => return Err(RepositoryError::backend("read volume alias", error)),
            };
            if self
                .client
                .put_bytes_conditionally(&key, bytes.clone(), etag.as_deref())
                .await
                .map_err(|error| RepositoryError::backend("claim volume alias", error))?
            {
                return Ok(true);
            }
        }
        Err(RepositoryError::Backend {
            message: format!("volume alias '{name}' changed too often while claiming it"),
            source: None,
        })
    }

    pub async fn read_volume_record(
        &self,
        volume_id: &str,
    ) -> RepositoryResult<Option<VolumeRecord>> {
        validate_volume_id(volume_id)?;
        let key = ObjectVolumeLayout::volume_record_key(volume_id);
        match self.client.get_bytes(&key).await {
            Ok(bytes) => {
                let record: VolumeRecord = serde_json::from_slice(&bytes)
                    .map_err(|error| RepositoryError::backend("parse volume record", error))?;
                validate_volume_id(&record.id)?;
                if record.id != volume_id {
                    return Err(RepositoryError::InvalidRequest {
                        reason: format!(
                            "volume record id '{}' does not match key '{volume_id}'",
                            record.id
                        ),
                    });
                }
                Ok(Some(record))
            }
            Err(error) if C::is_not_found_error(&error) => Ok(None),
            Err(error) => Err(RepositoryError::backend("read volume record", error)),
        }
    }

    pub async fn replace_volume_record_owner(
        &self,
        volume_id: &str,
        from: &str,
        to: Option<&str>,
    ) -> RepositoryResult<()> {
        for _attempt in 0..MAX_VOLUME_CAS_ATTEMPTS {
            let Some((mut record, etag)) = self.read_volume_record_versioned(volume_id).await?
            else {
                return Ok(());
            };
            if record.deleting {
                return Ok(());
            }
            if !record.replace_owner(from, to) {
                return Ok(());
            }
            if self
                .write_volume_record_conditionally(&record, etag.as_deref())
                .await?
            {
                return Ok(());
            }
        }
        Err(RepositoryError::Backend {
            message: format!("volume '{volume_id}' changed too often while updating its owner"),
            source: None,
        })
    }

    pub async fn read_volume_record_versioned(
        &self,
        volume_id: &str,
    ) -> RepositoryResult<Option<(VolumeRecord, Option<String>)>> {
        let key = ObjectVolumeLayout::volume_record_key(volume_id);
        match self.client.get_bytes_with_etag(&key).await {
            Ok((bytes, etag)) => {
                let record: VolumeRecord = serde_json::from_slice(&bytes)
                    .map_err(|error| RepositoryError::backend("parse volume record", error))?;
                validate_volume_id(&record.id)?;
                if record.id != volume_id {
                    return Err(RepositoryError::InvalidRequest {
                        reason: format!(
                            "volume record id '{}' does not match key '{volume_id}'",
                            record.id
                        ),
                    });
                }
                let etag = etag.ok_or_else(|| RepositoryError::Unsupported {
                    feature: "OSS volume CAS requires ETags".to_string(),
                })?;
                Ok(Some((record, Some(etag))))
            }
            Err(error) if C::is_not_found_error(&error) => Ok(None),
            Err(error) => Err(RepositoryError::backend("read volume record", error)),
        }
    }

    pub async fn write_volume_record_conditionally(
        &self,
        record: &VolumeRecord,
        etag: Option<&str>,
    ) -> RepositoryResult<bool> {
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|error| RepositoryError::backend("serialize volume record", error))?;
        self.client
            .put_bytes_conditionally(
                &ObjectVolumeLayout::volume_record_key(&record.id),
                bytes,
                etag,
            )
            .await
            .map_err(|error| RepositoryError::backend("conditionally write volume record", error))
    }
}
fn validate_volume_id(volume_id: &str) -> RepositoryResult<()> {
    validate_volume_component(volume_id, "id")
}

fn validate_volume_component(value: &str, kind: &str) -> RepositoryResult<()> {
    if !is_valid_volume_component(value) {
        return Err(RepositoryError::InvalidRequest {
            reason: format!("invalid volume {kind} '{value}'"),
        });
    }
    Ok(())
}

fn volume_id_from_record_key(key: &str) -> RepositoryResult<String> {
    let file_name = key
        .strip_prefix(ObjectVolumeLayout::volume_records_prefix())
        .ok_or_else(|| RepositoryError::InvalidRequest {
            reason: format!("invalid volume record key '{key}'"),
        })?;
    if file_name.contains('/') {
        return Err(RepositoryError::InvalidRequest {
            reason: format!("invalid volume record key '{key}'"),
        });
    }
    let volume_id =
        file_name
            .strip_suffix(".json")
            .ok_or_else(|| RepositoryError::InvalidRequest {
                reason: format!("invalid volume record key '{key}'"),
            })?;
    validate_volume_id(volume_id)?;
    Ok(volume_id.to_string())
}
