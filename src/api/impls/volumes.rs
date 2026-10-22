use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use async_trait::async_trait;
use axum_extra::extract::CookieJar;
use base64::Engine as _;
use headers::Host;
use http::Method;

use agentenv_http_server::apis::volumes::*;
use agentenv_http_server::models;

use crate::sandbox::{normalize_mount_path, ExtraDrive};
use crate::volume::{
    VolumeError, VolumeManager, VolumeMode, VolumeRecord, VolumeStatus, DEFAULT_VOLUME_PAGE_SIZE,
    DEFAULT_VOLUME_SIZE_MB,
};

use super::ApiImpl;

pub(super) async fn resolve_volume_mounts(
    manager: &VolumeManager,
    mounts: &HashMap<String, String>,
    owner: &str,
) -> Result<(Vec<ExtraDrive>, HashMap<String, String>), models::Error> {
    let mut reserved = Vec::with_capacity(mounts.len());
    let result = resolve_volume_mounts_inner(manager, mounts, owner, &mut reserved).await;
    if result.is_err() {
        if let Err(error) = manager.replace_owner_for(owner, None, &reserved).await {
            tracing::warn!(%owner, %error, "failed to clean up volume reservations after mount resolution failed");
        }
    }
    result
}

async fn resolve_volume_mounts_inner(
    manager: &VolumeManager,
    mounts: &HashMap<String, String>,
    owner: &str,
    reserved: &mut Vec<String>,
) -> Result<(Vec<ExtraDrive>, HashMap<String, String>), models::Error> {
    let mut drives = Vec::with_capacity(mounts.len());
    let limits = manager.limits();
    if mounts.len() > limits.max_mounts {
        return Err(error_response(VolumeError::TooManyMountedVolumes {
            max_count: limits.max_mounts,
        })
        .1);
    }
    let mut volume_ids = HashSet::with_capacity(mounts.len());
    let mut normalized_mounts = HashMap::with_capacity(mounts.len());

    let mut mounts = mounts.iter().collect::<Vec<_>>();
    mounts.sort_unstable_by_key(|(mount_path, _)| *mount_path);
    for (mount_path, reference) in mounts {
        let mount_path = normalize_mount_path(PathBuf::from(mount_path))
            .map_err(|error| ApiImpl::error(400, error.to_string()))?;
        if normalized_mounts.keys().any(|existing| {
            let existing = PathBuf::from(existing);
            existing.starts_with(&mount_path) || mount_path.starts_with(existing)
        }) {
            return Err(ApiImpl::error(
                400,
                format!(
                    "volume mount path overlaps another mount: {}",
                    mount_path.display()
                ),
            ));
        }
        let volume = manager
            .get(reference)
            .await
            .map_err(|error| error_response(error).1)?;
        if volume.size_mb > limits.max_size_mb {
            return Err(error_response(VolumeError::SizeLimitExceeded {
                max_size_mb: limits.max_size_mb,
            })
            .1);
        }
        if !volume_ids.insert(volume.id.clone()) {
            return Err(ApiImpl::error(
                400,
                format!("volume {} is mounted more than once", volume.id),
            ));
        }
        manager
            .reserve(&volume.id, owner)
            .await
            .map_err(|error| error_response(error).1)?;
        reserved.push(volume.id.clone());

        let volume = manager
            .materialize_backing(&volume.id)
            .await
            .map_err(|error| error_response(error).1)?;
        let image_config_path = volume.backing_image_config.clone().ok_or_else(|| {
            ApiImpl::error(500, format!("volume {} has no backing image", volume.id))
        })?;

        let normalized_path = mount_path.to_string_lossy().into_owned();
        let drive = ExtraDrive::try_new_overlaybd_with_mount_path(
            volume.id.clone(),
            image_config_path,
            volume.mode == VolumeMode::ReadOnly,
            mount_path,
            None::<PathBuf>,
        )
        .and_then(|drive| {
            drive.try_with_virtual_size(
                volume
                    .size_mb
                    .checked_mul(1024 * 1024)
                    .ok_or_else(|| anyhow::anyhow!("volume size is too large"))?,
            )
        })
        .map_err(|error| ApiImpl::error(400, error.to_string()))?;
        let snapshot_output_dir =
            (volume.mode == VolumeMode::Exclusive).then(|| manager.data_dir(&volume.id));
        drives.push(drive.with_volume_snapshot_output_dir(snapshot_output_dir));
        normalized_mounts.insert(normalized_path, volume.id);
    }

    Ok((drives, normalized_mounts))
}

fn volume_response(record: VolumeRecord) -> models::Volume {
    let status = match record.status {
        VolumeStatus::Ready => "ready",
        VolumeStatus::Uploading => "uploading",
        VolumeStatus::Failed => "failed",
    }
    .to_string();
    let mode = match record.mode {
        VolumeMode::ReadOnly => "ro".to_owned(),
        VolumeMode::Exclusive => "exclusive".to_owned(),
    };
    models::Volume {
        provider: Some("legacy".into()),
        capabilities: None,
        volume_id: record.id,
        name: record.name,
        mode,
        size_mb: record.size_mb,
        status,
    }
}

pub(super) fn error_response(error: VolumeError) -> (i32, models::Error) {
    let code = match error {
        VolumeError::InvalidName
        | VolumeError::MultipleSources
        | VolumeError::SourceNotFound(_)
        | VolumeError::InvalidSize
        | VolumeError::InvalidNextToken
        | VolumeError::InvalidPageLimit
        | VolumeError::SizeLimitExceeded { .. }
        | VolumeError::SizeMismatch => 400,
        VolumeError::TooManyMountedVolumes { .. } => 400,
        VolumeError::NotFound(_) => 404,
        VolumeError::NameConflict(_)
        | VolumeError::Reserved(_)
        | VolumeError::Uploading(_)
        | VolumeError::Failed(_) => 409,
        VolumeError::Storage(_) => 500,
    };
    (code, ApiImpl::error(code, error.to_string()))
}

fn provider_volume_page(
    mut volumes: Vec<models::Volume>,
    token: &str,
    limit: usize,
) -> Result<VolumesGetResponse, models::Error> {
    const PREFIX: &str = "storage-v1:";
    let encoded = token
        .strip_prefix(PREFIX)
        .ok_or_else(|| ApiImpl::error(400, "invalid provider cursor"))?;
    let after = String::from_utf8(
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| ApiImpl::error(400, "invalid provider cursor"))?,
    )
    .map_err(|_| ApiImpl::error(400, "invalid provider cursor"))?;
    if limit == 0 {
        return Err(ApiImpl::error(400, "invalid volume page limit"));
    }
    volumes.sort_by(|a, b| a.volume_id.cmp(&b.volume_id));
    volumes.retain(|volume| volume.volume_id > after);
    let more = volumes.len() > limit;
    volumes.truncate(limit);
    let next = if more {
        volumes.last().map(|v| {
            format!(
                "{PREFIX}{}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.volume_id.as_bytes())
            )
        })
    } else {
        None
    };
    Ok(
        VolumesGetResponse::Status200_SuccessfullyListedTeamVolumes {
            body: volumes,
            x_next_token: next,
        },
    )
}

#[async_trait]
impl Volumes<()> for ApiImpl {
    type Claims = super::Claims;

    async fn volumes_volume_id_expand_post(
        &self,
        _method: &Method,
        _host: &Host,
        _cookies: &CookieJar,
        _claims: &Self::Claims,
        path: &models::VolumesVolumeIdExpandPostPathParams,
        body: &models::ExpandVolume,
    ) -> Result<VolumesVolumeIdExpandPostResponse, ()> {
        use VolumesVolumeIdExpandPostResponse as Response;
        let Some(bytes) = body
            .size_mb
            .checked_mul(1024 * 1024)
            .filter(|bytes| *bytes > 0 && *bytes <= i64::MAX as u64)
        else {
            return Ok(Response::Status400_BadRequest(ApiImpl::error(
                400,
                "invalid expansion capacity",
            )));
        };
        match super::volume_provider::lookup(&path.volume_id).await {
            Ok(Some(_)) => (),
            Ok(None) => {
                return Ok(
                    Response::Status501_VolumeProviderDoesNotSupportOfflineExpansion(
                        ApiImpl::error(501, "volume provider does not support offline expansion"),
                    ),
                )
            }
            Err(error) => {
                return Ok(Response::Status409_Conflict(ApiImpl::error(
                    409,
                    error.to_string(),
                )))
            }
        }
        let result = async {
            let value = crate::sandbox::storage::volume_command(
                "expand",
                serde_json::json!({
                    "name": path.volume_id, "bytes": bytes,
                }),
            )
            .await?;
            Ok::<models::Volume, anyhow::Error>(serde_json::from_value(value)?)
        }
        .await;
        Ok(match result {
            Ok(volume) if volume.status == "ready" => {
                Response::Status200_ControllerExpansionComplete(volume)
            }
            Ok(volume) => Response::Status202_OfflineExpansionPending(volume),
            Err(error) => Response::Status409_Conflict(ApiImpl::error(409, error.to_string())),
        })
    }

    async fn volumes_get(
        &self,
        _method: &Method,
        _host: &Host,
        _cookies: &CookieJar,
        _claims: &Self::Claims,
        query_params: &models::VolumesGetQueryParams,
    ) -> Result<VolumesGetResponse, ()> {
        let external = match super::volume_provider::list().await {
            Ok(volumes) => volumes,
            Err(error) => {
                return Ok(VolumesGetResponse::Status500_ServerError(ApiImpl::error(
                    500,
                    format!("storage provider list failed: {error:#}"),
                )))
            }
        };
        let limit = query_params
            .limit
            .map(|value| value as usize)
            .unwrap_or(DEFAULT_VOLUME_PAGE_SIZE);
        if let Some(token) = query_params
            .next_token
            .as_deref()
            .filter(|t| t.starts_with("storage-v1:"))
        {
            return Ok(provider_volume_page(external, token, limit)
                .unwrap_or_else(VolumesGetResponse::Status400_BadRequest));
        }
        match self
            .volume_manager
            .list_page(
                query_params.next_token.as_deref(),
                query_params
                    .limit
                    .map(|limit| limit as usize)
                    .unwrap_or(DEFAULT_VOLUME_PAGE_SIZE),
            )
            .await
        {
            Ok(page) => {
                if page.records.is_empty() && page.next_token.is_none() && !external.is_empty() {
                    return Ok(provider_volume_page(external, "storage-v1:", limit)
                        .unwrap_or_else(VolumesGetResponse::Status400_BadRequest));
                }
                Ok(
                    VolumesGetResponse::Status200_SuccessfullyListedTeamVolumes {
                        body: page.records.into_iter().map(volume_response).collect(),
                        x_next_token: page
                            .next_token
                            .or_else(|| (!external.is_empty()).then(|| "storage-v1:".into())),
                    },
                )
            }
            Err(error @ (VolumeError::InvalidNextToken | VolumeError::InvalidPageLimit)) => Ok(
                VolumesGetResponse::Status400_BadRequest(error_response(error).1),
            ),
            Err(error) => Ok(VolumesGetResponse::Status500_ServerError(ApiImpl::error(
                500,
                error.to_string(),
            ))),
        }
    }

    async fn volumes_post(
        &self,
        _method: &Method,
        _host: &Host,
        _cookies: &CookieJar,
        _claims: &Self::Claims,
        body: &models::NewVolume,
    ) -> Result<VolumesPostResponse, ()> {
        let mode = match body.mode.as_deref() {
            None | Some("exclusive") => VolumeMode::Exclusive,
            Some("ro") => VolumeMode::ReadOnly,
            Some(mode) => {
                return Ok(VolumesPostResponse::Status400_BadRequest(ApiImpl::error(
                    400,
                    format!("unsupported volume mode: {mode}"),
                )))
            }
        };
        if body
            .provider
            .as_deref()
            .is_some_and(|provider| provider != "legacy")
        {
            match self.volume_manager.get(&body.name).await {
                Ok(_) => {
                    return Ok(VolumesPostResponse::Status409_Conflict(ApiImpl::error(
                        409,
                        "volume name is already owned by legacy storage",
                    )))
                }
                Err(VolumeError::NotFound(_)) => {}
                Err(error) => {
                    return Ok(VolumesPostResponse::Status500_ServerError(
                        error_response(error).1,
                    ))
                }
            }
            return Ok(match super::volume_provider::create(body).await {
                Ok(volume) => {
                    VolumesPostResponse::Status201_SuccessfullyCreatedANewTeamVolume(volume)
                }
                Err(error) => VolumesPostResponse::Status409_Conflict(ApiImpl::error(
                    409,
                    format!("storage provider create failed: {error:#}"),
                )),
            });
        }
        if body.storage.is_some() {
            return Ok(VolumesPostResponse::Status400_BadRequest(ApiImpl::error(
                400,
                "provider storage options require an explicit provider",
            )));
        }
        if let Err(error) = super::volume_provider::claim_legacy(&body.name).await {
            return Ok(VolumesPostResponse::Status409_Conflict(ApiImpl::error(
                409,
                format!("reserve legacy volume name: {error:#}"),
            )));
        }
        let resolved_image = match (body.from_volume.is_none(), body.image.as_deref()) {
            (true, Some(image)) => match self.image_resolver.resolve(image).await {
                Ok(resolved) => Some(resolved.overlaybd_config_path),
                Err(error) => {
                    let code = if error.is_user_error() { 400 } else { 500 };
                    return Ok(match code {
                        400 => VolumesPostResponse::Status400_BadRequest(ApiImpl::error(
                            code,
                            format!("resolve volume image: {error:#}"),
                        )),
                        _ => VolumesPostResponse::Status500_ServerError(ApiImpl::error(
                            code,
                            format!("resolve volume image: {error:#}"),
                        )),
                    });
                }
            },
            _ => None,
        };

        match self
            .volume_manager
            .create(
                body.name.clone(),
                mode,
                body.from_volume.clone(),
                resolved_image,
                body.size_mb.unwrap_or(DEFAULT_VOLUME_SIZE_MB),
            )
            .await
        {
            Ok(record) => Ok(
                VolumesPostResponse::Status201_SuccessfullyCreatedANewTeamVolume(volume_response(
                    record,
                )),
            ),
            Err(error) => {
                let (code, error) = error_response(error);
                match code {
                    400 => Ok(VolumesPostResponse::Status400_BadRequest(error)),
                    409 => Ok(VolumesPostResponse::Status409_Conflict(error)),
                    _ => Ok(VolumesPostResponse::Status500_ServerError(error)),
                }
            }
        }
    }

    async fn volumes_volume_id_delete(
        &self,
        _method: &Method,
        _host: &Host,
        _cookies: &CookieJar,
        _claims: &Self::Claims,
        path_params: &models::VolumesVolumeIdDeletePathParams,
    ) -> Result<VolumesVolumeIdDeleteResponse, ()> {
        match super::volume_provider::lookup(&path_params.volume_id).await {
            Ok(Some(volume)) if volume.status == "deleted" => {
                return Ok(VolumesVolumeIdDeleteResponse::Status204_SuccessfullyDeletedATeamVolume)
            }
            Ok(Some(_)) => {
                return Ok(
                    match super::volume_provider::delete(&path_params.volume_id).await {
                        Ok(()) => {
                            VolumesVolumeIdDeleteResponse::Status204_SuccessfullyDeletedATeamVolume
                        }
                        Err(error) => {
                            VolumesVolumeIdDeleteResponse::Status409_Conflict(ApiImpl::error(
                                409,
                                format!("storage provider deletion pending or rejected: {error:#}"),
                            ))
                        }
                    },
                )
            }
            Ok(None) => {}
            Err(error) => {
                return Ok(VolumesVolumeIdDeleteResponse::Status500_ServerError(
                    ApiImpl::error(500, format!("storage provider lookup failed: {error:#}")),
                ))
            }
        }
        match self.volume_manager.delete(&path_params.volume_id).await {
            Ok(()) => Ok(VolumesVolumeIdDeleteResponse::Status204_SuccessfullyDeletedATeamVolume),
            Err(error) => {
                let (code, error) = error_response(error);
                match code {
                    404 => Ok(VolumesVolumeIdDeleteResponse::Status404_NotFound(error)),
                    409 => Ok(VolumesVolumeIdDeleteResponse::Status409_Conflict(error)),
                    _ => Ok(VolumesVolumeIdDeleteResponse::Status500_ServerError(error)),
                }
            }
        }
    }

    async fn volumes_volume_id_get(
        &self,
        _method: &Method,
        _host: &Host,
        _cookies: &CookieJar,
        _claims: &Self::Claims,
        path_params: &models::VolumesVolumeIdGetPathParams,
    ) -> Result<VolumesVolumeIdGetResponse, ()> {
        match super::volume_provider::lookup(&path_params.volume_id).await {
            Ok(Some(volume)) if volume.status == "deleted" => {
                return Ok(VolumesVolumeIdGetResponse::Status404_NotFound(
                    ApiImpl::error(404, "volume deleted"),
                ))
            }
            Ok(Some(volume)) => {
                return Ok(
                    VolumesVolumeIdGetResponse::Status200_SuccessfullyRetrievedATeamVolume(volume),
                )
            }
            Ok(None) => {}
            Err(error) => {
                return Ok(VolumesVolumeIdGetResponse::Status500_ServerError(
                    ApiImpl::error(500, format!("storage provider lookup failed: {error:#}")),
                ))
            }
        }
        match self.volume_manager.get(&path_params.volume_id).await {
            Ok(record) => Ok(
                VolumesVolumeIdGetResponse::Status200_SuccessfullyRetrievedATeamVolume(
                    volume_response(record),
                ),
            ),
            Err(error) => {
                let (code, error) = error_response(error);
                match code {
                    404 => Ok(VolumesVolumeIdGetResponse::Status404_NotFound(error)),
                    _ => Ok(VolumesVolumeIdGetResponse::Status500_ServerError(error)),
                }
            }
        }
    }
}
