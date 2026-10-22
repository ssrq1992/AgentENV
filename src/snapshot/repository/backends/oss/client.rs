//! Native snapshot projection over the shared original OSS transport.
use crate::snapshot::SnapshotId;
pub(crate) use aenv_volume_core::oss_client::OssUploadArtifact;
use anyhow::Result;
use object_store_operator::{AddressingStyle, CredentialSource};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub(crate) struct OssClient(Arc<aenv_volume_core::oss_client::OssClient>);
impl std::ops::Deref for OssClient {
    type Target = aenv_volume_core::oss_client::OssClient;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl OssClient {
    pub(crate) fn new(
        bucket: String,
        endpoint: String,
        region: String,
        prefix: String,
        credential_source: CredentialSource,
        addressing_override: Option<AddressingStyle>,
    ) -> Result<Self> {
        Ok(Self(Arc::new(
            aenv_volume_core::oss_client::OssClient::new(
                bucket,
                endpoint,
                region,
                prefix,
                credential_source,
                addressing_override,
            )?,
        )))
    }
    pub(crate) fn startup_pack_url(&self, snapshot_id: &SnapshotId) -> String {
        self.object_url(&format!(
            "artifacts/{snapshot_id}/{}",
            crate::snapshot::MEMORY_STARTUP_PACK_ARTIFACT
        ))
    }
    pub(crate) fn is_not_found_error(error: &anyhow::Error) -> bool {
        aenv_volume_core::oss_client::OssClient::is_not_found_error(error)
    }
}
#[async_trait::async_trait]
impl aenv_volume_core::object::VolumeObjectStore for OssClient {
    fn is_not_found_error(error: &anyhow::Error) -> bool {
        OssClient::is_not_found_error(error)
    }
    async fn get_bytes(&self, key: &str) -> Result<Vec<u8>> {
        Ok(self.0.get_bytes(key).await?.to_vec())
    }
    async fn get_bytes_with_etag(&self, key: &str) -> Result<(Vec<u8>, Option<String>)> {
        let (bytes, etag) = self.0.get_bytes_with_etag(key).await?;
        Ok((bytes.to_vec(), etag))
    }
    async fn exists(&self, key: &str) -> Result<bool> {
        self.0.exists(key).await
    }
    async fn list_keys_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>> {
        self.0.list_keys_page(prefix, start_after, limit).await
    }
    async fn put_bytes_conditionally(
        &self,
        key: &str,
        bytes: Vec<u8>,
        etag: Option<&str>,
    ) -> Result<bool> {
        self.0.put_bytes_conditionally(key, bytes, etag).await
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.0.delete(key).await
    }
}
