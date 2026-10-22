//! Head metadata transport uses the same conditional object operations as the
//! native volume catalog; object content and current-head CAS are separate.
use crate::{
    catalog::{CatalogError, CatalogResult},
    heads::{PosixHeadStore, PublishedHead},
    is_valid_volume_component,
    object::VolumeObjectStore,
};
use async_trait::async_trait;
use std::sync::Arc;
#[async_trait]
pub trait HeadRepository: Send + Sync {
    async fn read(&self, id: &str, revision: &str) -> CatalogResult<Option<PublishedHead>>;
    async fn current(&self, id: &str) -> CatalogResult<Option<PublishedHead>>;
    async fn publish(&self, expected: Option<&str>, head: &PublishedHead) -> CatalogResult<()>;
}
#[async_trait]
impl HeadRepository for PosixHeadStore {
    async fn read(&self, id: &str, revision: &str) -> CatalogResult<Option<PublishedHead>> {
        let store = self.clone();
        let id = id.to_owned();
        let revision = revision.to_owned();
        tokio::task::spawn_blocking(move || store.read(&id, &revision))
            .await
            .map_err(error)?
    }
    async fn current(&self, id: &str) -> CatalogResult<Option<PublishedHead>> {
        let store = self.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || store.current(&id))
            .await
            .map_err(error)?
    }
    async fn publish(&self, expected: Option<&str>, head: &PublishedHead) -> CatalogResult<()> {
        let store = self.clone();
        let expected = expected.map(str::to_owned);
        let head = head.clone();
        tokio::task::spawn_blocking(move || store.publish(expected.as_deref(), &head))
            .await
            .map_err(error)?
    }
}
pub struct ObjectHeadStore<C: VolumeObjectStore> {
    client: Arc<C>,
}
impl<C: VolumeObjectStore> ObjectHeadStore<C> {
    pub fn new(client: Arc<C>) -> Self {
        Self { client }
    }
    async fn load(&self, id: &str, key: &str) -> CatalogResult<Option<(PublishedHead, String)>> {
        let (bytes, etag) = match self.client.get_bytes_with_etag(key).await {
            Ok(value) => value,
            Err(e) if C::is_not_found_error(&e) => return Ok(None),
            Err(e) => return Err(error(e)),
        };
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(invalid("oversized head metadata"));
        }
        let head: PublishedHead = serde_json::from_slice(&bytes).map_err(error)?;
        head.validate()?;
        if head.volume_id != id {
            return Err(invalid("object head identity mismatch"));
        }
        let etag = etag
            .filter(|v| !v.is_empty())
            .ok_or_else(|| invalid("object head CAS requires ETag"))?;
        Ok(Some((head, etag)))
    }
}
#[async_trait]
impl<C: VolumeObjectStore> HeadRepository for ObjectHeadStore<C> {
    async fn read(&self, id: &str, revision: &str) -> CatalogResult<Option<PublishedHead>> {
        valid_id(id)?;
        if revision.len() != 64 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid("invalid immutable head revision"));
        }
        let head = self
            .load(id, &format!("managed-heads/{id}/heads/{revision}.json"))
            .await?
            .map(|(head, _)| head);
        if head.as_ref().is_some_and(|h| h.revision != revision) {
            return Err(invalid("immutable object head revision mismatch"));
        }
        Ok(head)
    }
    async fn current(&self, id: &str) -> CatalogResult<Option<PublishedHead>> {
        valid_id(id)?;
        Ok(self
            .load(id, &format!("managed-heads/{id}/current.json"))
            .await?
            .map(|(head, _)| head))
    }
    async fn publish(&self, expected: Option<&str>, head: &PublishedHead) -> CatalogResult<()> {
        head.validate()?;
        let id = &head.volume_id;
        valid_id(id)?;
        let current = format!("managed-heads/{id}/current.json");
        let prior = self.load(id, &current).await?;
        if prior.as_ref().is_some_and(|(prior, _)| prior == head) {
            return Ok(());
        }
        head.validate_transition(prior.as_ref().map(|(head, _)| head), expected)?;
        let raw = serde_json::to_vec(head).map_err(error)?;
        let immutable = format!("managed-heads/{id}/heads/{}.json", head.revision);
        if !self
            .client
            .put_bytes_conditionally(&immutable, raw.clone(), None)
            .await
            .map_err(error)?
        {
            if self.load(id, &immutable).await?.map(|(h, _)| h).as_ref() != Some(head) {
                return Err(invalid("immutable head object conflict"));
            }
        }
        if !self
            .client
            .put_bytes_conditionally(&current, raw, prior.as_ref().map(|(_, etag)| etag.as_str()))
            .await
            .map_err(error)?
        {
            if self.load(id, &current).await?.map(|(h, _)| h).as_ref() != Some(head) {
                return Err(invalid("current head conditional publication conflict"));
            }
        }
        Ok(())
    }
}
fn valid_id(id: &str) -> CatalogResult<()> {
    if is_valid_volume_component(id) {
        Ok(())
    } else {
        Err(invalid("invalid volume identity"))
    }
}
fn invalid(reason: &str) -> CatalogError {
    CatalogError::InvalidRequest {
        reason: reason.into(),
    }
}
fn error(e: impl Into<anyhow::Error>) -> CatalogError {
    CatalogError::backend("native head transport", e)
}
