//! Read-only projection for storage-aware scheduling; SnapshotManager remains
//! the authority for template aliases and immutable runnable versions.
use super::ApiImpl;
use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};

pub(crate) fn router<I>(api: I) -> Router
where
    I: AsRef<ApiImpl> + Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/internal/storage/defaults", get(project_defaults))
        .route("/internal/storage/capacity", get(project_capacity::<I>))
        .route(
            "/internal/storage/checkpoints/{reference}",
            get(project_checkpoint::<I>),
        )
        .route(
            "/internal/storage/templates/{reference}",
            get(project_template::<I>),
        )
        .with_state(api)
}
async fn project_template<I>(State(api): State<I>, Path(reference): Path<String>) -> Response
where
    I: AsRef<ApiImpl> + Clone + Send + Sync + 'static,
{
    if !crate::cfg::ConfigManager::global_config()
        .storage_provider
        .enabled
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let api = api.as_ref();
    match api.snapshot_manager.get(&reference).await {
        Ok(Some(record)) => {
            let Some(committed) = &record.committed else {
                return StatusCode::CONFLICT.into_response();
            };
            if committed.virtualization_mode
                != crate::cfg::ConfigManager::global_config().virtualization_mode
            {
                return StatusCode::CONFLICT.into_response();
            }
            let checkpoint = match &committed.storage_checkpoint {
                Some(value) => match value
                    .get("reference")
                    .and_then(serde_json::Value::as_str)
                    .filter(|id| !id.is_empty())
                {
                    Some(id) => id,
                    None => return StatusCode::CONFLICT.into_response(),
                },
                None => "",
            };
            axum::Json(serde_json::json!({
                "id": record.id.to_string(),
                "resources": {"cpu": record.resources.cpu_count, "memory_mib": record.resources.memory_mib},
                "volumeMounts": [], "checkpoint_id": checkpoint,
            })).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => (
            StatusCode::CONFLICT,
            axum::Json(serde_json::json!({"code":409,"message":err.to_string()})),
        )
            .into_response(),
    }
}

async fn project_defaults() -> Response {
    let config = crate::cfg::ConfigManager::global_config();
    if !config.storage_provider.enabled {
        return StatusCode::NOT_FOUND.into_response();
    }
    axum::Json(serde_json::json!({"cpu":config.machine.vcpu_count,"memory_mib":config.machine.mem_size_mib,"storage_protocol":"agentenv.storage.v2","storage_hook_version":4})).into_response()
}

// An explicit repository observation, unlike a routing/authentication 404.
async fn project_checkpoint<I>(State(api): State<I>, Path(reference): Path<String>) -> Response
where
    I: AsRef<ApiImpl> + Clone + Send + Sync + 'static,
{
    if !crate::cfg::ConfigManager::global_config()
        .storage_provider
        .enabled
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    if uuid::Uuid::parse_str(&reference).is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let repository_id = match api.as_ref().snapshot_manager.storage_repository_id().await {
        Ok(id) => id,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    match api.as_ref().snapshot_manager.get(&reference).await {
        Ok(record) => axum::Json(serde_json::json!({
            "id": reference,
            "repository_id": repository_id,
            "exists": record.is_some(),
            "committed": record.as_ref().is_some_and(|record| record.committed.is_some()),
        }))
        .into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

// Admission accounting is separate from routing/heartbeat IDs: those include
// dormant builders and are sampled independently of their resource totals.
async fn project_capacity<I>(State(api): State<I>) -> Response
where
    I: AsRef<ApiImpl> + Clone + Send + Sync + 'static,
{
    if !crate::cfg::ConfigManager::global_config()
        .storage_provider
        .enabled
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let api = api.as_ref();
    let result = async {
        let observability = api.observability().ok_or_else(|| anyhow::anyhow!("native capacity observation unavailable"))?;
        let host = observability.node_snapshot().await?;
        let (allocated, ids) = api.orchestrator.storage_allocation_snapshot().await?;
        Ok::<_, anyhow::Error>(serde_json::json!({
            "version": 1, "node_id": host.node_id,
            "capacity": {"cpu": host.metrics.cpu_count, "memory_mib": host.metrics.memory_total_bytes / (1024 * 1024)},
            "allocated": {"cpu": allocated.allocated_cpu, "memory_mib": allocated.allocated_memory_bytes.div_ceil(1024 * 1024)},
            "sandbox_ids": ids.iter().map(ToString::to_string).collect::<Vec<_>>(),
        }))
    }.await;
    match result {
        Ok(value) => axum::Json(value).into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}
