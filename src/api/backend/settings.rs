use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::config::{
    effective_provider, save_audio_settings, save_provider_settings, save_provider_token,
    validate_audio, validate_provider, ProviderConfig,
};

use super::{api_error, map_bad_request, map_internal, ApiResult, WebController};

pub(super) fn routes() -> Router<WebController> {
    Router::new()
        .route(
            "/settings/audio",
            get(get_audio_settings).put(put_audio_settings),
        )
        .route(
            "/settings/alerts",
            get(get_alert_settings).put(put_alert_settings),
        )
        .route("/settings/alerts/test", post(test_alert_settings))
}

#[derive(Deserialize, Default)]
struct AudioSettingsBody {
    air_raid_alerts_enabled: Option<bool>,
    notifications_enabled: Option<bool>,
    duck_db: Option<f64>,
    alert_volume_percent: Option<f64>,
    minute_silence_volume_percent: Option<f64>,
    minute_silence_enabled: Option<bool>,
    minute_silence_start_time: Option<String>,
    minute_silence_timezone: Option<String>,
    minute_silence_catch_up_seconds: Option<u64>,
    minute_silence_music_fade_seconds: Option<f64>,
    default_restore_volume_percent: Option<f64>,
    duck_fade_seconds: Option<f64>,
    restore_fade_seconds: Option<f64>,
    alert_repeat_interval_minutes: Option<u64>,
    duck_only_during_announcement: Option<bool>,
}

impl AudioSettingsBody {
    fn resolved_air_raid_alerts_enabled(&self) -> Result<Option<bool>> {
        match (self.air_raid_alerts_enabled, self.notifications_enabled) {
            (Some(canonical), Some(legacy)) if canonical != legacy => {
                bail!("air_raid_alerts_enabled і notifications_enabled не можуть суперечити одне одному")
            }
            (Some(value), _) | (_, Some(value)) => Ok(Some(value)),
            (None, None) => Ok(None),
        }
    }
}

#[derive(Deserialize, Default)]
struct ProviderBody {
    endpoint: Option<String>,
    location_uid: Option<u32>,
    location_type: Option<String>,
    poll_interval_seconds: Option<f64>,
    request_timeout_seconds: Option<f64>,
    rate_limit_backoff_seconds: Option<f64>,
    clear_confirmations: Option<u32>,
    token: Option<String>,
}

fn apply_provider_body(base: ProviderConfig, body: &ProviderBody) -> Result<ProviderConfig> {
    let mut config = base;
    if let Some(v) = body.endpoint.as_deref() {
        config.endpoint = v.trim().to_owned();
    }
    if let Some(v) = body.location_uid {
        config.location_uid = v;
    }
    if let Some(v) = body.location_type.as_deref() {
        config.location_type = v.trim().to_ascii_lowercase();
    }
    if let Some(v) = body.poll_interval_seconds {
        config.poll_interval_seconds = v;
    }
    if let Some(v) = body.request_timeout_seconds {
        config.request_timeout_seconds = v;
    }
    if let Some(v) = body.rate_limit_backoff_seconds {
        config.rate_limit_backoff_seconds = v;
    }
    if let Some(v) = body.clear_confirmations {
        config.clear_confirmations = v;
    }
    validate_provider(&config)?;
    Ok(config)
}

async fn get_audio_settings(State(controller): State<WebController>) -> ApiResult {
    controller.audio_settings().map(Json).map_err(map_internal)
}

async fn put_audio_settings(
    State(controller): State<WebController>,
    Json(body): Json<AudioSettingsBody>,
) -> ApiResult {
    let (mut audio, mut minute) = controller.audio.settings().map_err(map_internal)?;
    if let Some(v) = body
        .resolved_air_raid_alerts_enabled()
        .map_err(map_bad_request)?
    {
        audio.notifications_enabled = v;
    }
    if let Some(v) = body.duck_db {
        audio.duck_db = v;
    }
    if let Some(v) = body.alert_volume_percent {
        audio.alert_volume_percent = v;
    }
    if let Some(v) = body.minute_silence_volume_percent {
        minute.volume_percent = v;
    }
    if let Some(v) = body.minute_silence_enabled {
        minute.enabled = v;
    }
    if let Some(v) = body.minute_silence_start_time.as_deref() {
        let value = v.trim();
        minute.start_time = if value.matches(':').count() == 1 {
            format!("{value}:00")
        } else {
            value.to_owned()
        };
    }
    if let Some(v) = body.minute_silence_timezone.as_deref() {
        minute.timezone = v.trim().to_owned();
    }
    if let Some(v) = body.minute_silence_catch_up_seconds {
        minute.catch_up_seconds = v;
    }
    if let Some(v) = body.minute_silence_music_fade_seconds {
        minute.music_fade_seconds = v;
    }
    if let Some(v) = body.default_restore_volume_percent {
        audio.default_restore_volume_percent = v;
    }
    if let Some(v) = body.duck_fade_seconds {
        audio.duck_fade_seconds = v;
    }
    if let Some(v) = body.restore_fade_seconds {
        audio.restore_fade_seconds = v;
    }
    if let Some(v) = body.alert_repeat_interval_minutes {
        audio.alert_repeat_interval_minutes = v;
    }
    if let Some(v) = body.duck_only_during_announcement {
        audio.duck_only_during_announcement = v;
    }
    validate_audio(&audio, &minute).map_err(map_bad_request)?;
    save_audio_settings(&audio, &minute).map_err(map_internal)?;
    controller
        .audio
        .update_runtime_settings(audio.clone(), minute.clone())
        .map_err(map_internal)?;
    if !audio.notifications_enabled {
        controller.audio.cancel_alert_playback();
    }
    controller.audio_settings().map(Json).map_err(map_internal)
}

async fn get_alert_settings(State(controller): State<WebController>) -> ApiResult {
    controller.alert_settings().map(Json).map_err(map_internal)
}

async fn put_alert_settings(
    State(controller): State<WebController>,
    Json(body): Json<ProviderBody>,
) -> ApiResult {
    let current = effective_provider(&controller.config.provider).map_err(map_internal)?;
    let candidate = apply_provider_body(current, &body).map_err(map_bad_request)?;
    if let Some(token) = body.token.as_deref() {
        if !token.trim().is_empty() {
            save_provider_token(&candidate, token).map_err(map_internal)?;
        }
    }
    save_provider_settings(&candidate).map_err(map_internal)?;
    controller.alert_settings().map(Json).map_err(map_internal)
}

async fn test_alert_settings(
    State(controller): State<WebController>,
    Json(body): Json<ProviderBody>,
) -> ApiResult {
    let current = effective_provider(&controller.config.provider).map_err(map_internal)?;
    let candidate = apply_provider_body(current, &body).map_err(map_bad_request)?;
    let token = match body
        .token
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        Some(value) => value.to_owned(),
        None => candidate.resolve_token().map_err(map_bad_request)?,
    };
    let client = reqwest::Client::new();
    let response = client
        .get(candidate.status_endpoint())
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .timeout(Duration::from_secs_f64(candidate.request_timeout_seconds))
        .send()
        .await
        .context("Не вдалося отримати статус тривоги")
        .map_err(map_internal)?;
    if response.status() != StatusCode::OK {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("API повернув HTTP {}", response.status()),
        ));
    }
    let payload = response
        .json::<String>()
        .await
        .map_err(|e| map_internal(e.into()))?;
    if !matches!(payload.as_str(), "A" | "P" | "N") {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "API повернув невідомий статус",
        ));
    }
    let active = payload == "A" || (payload == "P" && candidate.partial_status_is_active());
    Ok(Json(
        json!({ "ok": true, "active": active, "state": if active { "active" } else { "clear" }, "location_uid": candidate.location_uid }),
    ))
}

#[cfg(test)]
mod tests {
    use super::AudioSettingsBody;

    #[test]
    fn audio_settings_accepts_canonical_and_legacy_air_raid_switches() {
        let canonical: AudioSettingsBody =
            serde_json::from_value(serde_json::json!({"air_raid_alerts_enabled": false})).unwrap();
        assert_eq!(
            canonical.resolved_air_raid_alerts_enabled().unwrap(),
            Some(false)
        );

        let legacy: AudioSettingsBody =
            serde_json::from_value(serde_json::json!({"notifications_enabled": true})).unwrap();
        assert_eq!(
            legacy.resolved_air_raid_alerts_enabled().unwrap(),
            Some(true)
        );

        let matching: AudioSettingsBody = serde_json::from_value(serde_json::json!({
            "air_raid_alerts_enabled": true,
            "notifications_enabled": true
        }))
        .unwrap();
        assert_eq!(
            matching.resolved_air_raid_alerts_enabled().unwrap(),
            Some(true)
        );

        let conflicting: AudioSettingsBody = serde_json::from_value(serde_json::json!({
            "air_raid_alerts_enabled": true,
            "notifications_enabled": false
        }))
        .unwrap();
        assert!(conflicting.resolved_air_raid_alerts_enabled().is_err());
    }
}
