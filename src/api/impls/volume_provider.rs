//! Native VolumeProviderMux helpers. Provider commands remain opaque to the
//! runtime; only the ordinary public Volume model crosses this boundary.
use agentenv_http_server::models;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

pub(super) fn enabled() -> bool {
    crate::cfg::ConfigManager::global_config()
        .storage_provider
        .enabled
}

pub(super) async fn lookup(name: &str) -> Result<Option<models::Volume>> {
    if !enabled() {
        return Ok(None);
    }
    #[derive(Deserialize)]
    struct Lookup {
        found: bool,
        volume: Option<models::Volume>,
    }
    let response: Lookup = serde_json::from_value(
        crate::sandbox::storage::volume_command("lookup", json!({"name":name})).await?,
    )?;
    if response.found {
        Ok(Some(
            response.volume.context("provider omitted known volume")?,
        ))
    } else {
        Ok(None)
    }
}

pub(super) async fn list() -> Result<Vec<models::Volume>> {
    if !enabled() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_value(
        crate::sandbox::storage::volume_command("list", json!({})).await?,
    )?)
}

pub(super) async fn claim_legacy(name: &str) -> Result<()> {
    if enabled() {
        crate::sandbox::storage::volume_command("claim_legacy", json!({"name":name})).await?;
    }
    Ok(())
}

pub(super) async fn create(body: &models::NewVolume) -> Result<models::Volume> {
    anyhow::ensure!(enabled(), "storage provider is disabled");
    anyhow::ensure!(
        body.image.is_none() && body.from_volume.is_none(),
        "provider image initialization and live volume clone require an explicit supported source"
    );
    let bytes = body
        .size_mb
        .unwrap_or(crate::volume::DEFAULT_VOLUME_SIZE_MB)
        .checked_mul(1024 * 1024)
        .filter(|n| *n <= i64::MAX as u64)
        .context("volume capacity exceeds provider range")?;
    let mut request = serde_json::Map::new();
    if let Some(options) = &body.storage {
        for (key, value) in options {
            request.insert(key.clone(), value.0.clone());
        }
    }
    // Public identity and access mode cannot be overridden by provider options.
    request.insert("provider".into(), json!(body.provider));
    request.insert("name".into(), json!(body.name));
    request.insert("bytes".into(), json!(bytes));
    request.insert("readonly".into(), json!(body.mode.as_deref() == Some("ro")));
    Ok(serde_json::from_value(
        crate::sandbox::storage::volume_command("create", Value::Object(request)).await?,
    )?)
}

pub(super) async fn delete(name: &str) -> Result<()> {
    anyhow::ensure!(enabled(), "storage provider is disabled");
    crate::sandbox::storage::volume_command("delete", json!({"name": name})).await?;
    Ok(())
}
