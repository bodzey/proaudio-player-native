use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, Mutex, Notify};
use tokio::time::sleep;
use tracing::{debug, info};
use url::Url;

use crate::alerts::SharedRuntimeState;
use crate::audio::AudioEngine;
use crate::command;
use crate::config::{
    effective_provider, save_audio_settings, save_provider_settings, save_provider_token,
    validate_audio, validate_provider, AppConfig, ProviderConfig,
};
use crate::dlna;
use crate::mpd::MpdMonitor;
use crate::output_router::{OutputDescriptor, DEFAULT_MASTER_SINK};
use crate::radio_directory;
use crate::source_arbiter::SharedSourceState;
use crate::upnp;

use super::webui;

mod alert_media;
mod mpris;
mod network_audio;
use mpris::MprisMonitor;

const API_VERSION: &str = "1";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const PLAYER_ACTIONS: &[&str] = &["play", "pause", "stop", "next", "prev"];
const API_CONTROL_BODY_BYTES: usize = 64 * 1024;

static ALSA_CARD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*(\d+)\s+\[([^]]+)\]").expect("valid ALSA card regex"));
static ALSA_CONTROL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Simple mixer control '([^']+)'").expect("valid ALSA control regex")
});
static ALSA_PERCENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Playback[^\n]*\[(\d+)%\]").expect("valid ALSA percent regex"));
static ALSA_DB_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[(-?\d+(?:\.\d+)?)dB\]").expect("valid ALSA dB regex"));
static ALSA_LIMITS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Limits:\s+Playback\s+(-?\d+)\s+-\s+(-?\d+)").expect("valid ALSA limits regex")
});
static ALSA_DB_SCALE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"dBscale-min=(-?\d+(?:\.\d+)?)dB,step=(-?\d+(?:\.\d+)?)dB")
        .expect("valid ALSA dB scale regex")
});
static ALSA_DB_MINMAX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"dBminmax-min=(-?\d+(?:\.\d+)?)dB,max=(-?\d+(?:\.\d+)?)dB")
        .expect("valid ALSA dB min/max regex")
});

type ApiError = (StatusCode, Json<Value>);
type ApiResult = std::result::Result<Json<Value>, ApiError>;

struct EventStream {
    receiver: mpsc::Receiver<std::result::Result<Event, Infallible>>,
}

impl futures_core::Stream for EventStream {
    type Item = std::result::Result<Event, Infallible>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context)
    }
}

fn api_error(status: StatusCode, message: impl Into<String>) -> ApiError {
    (status, Json(json!({ "error": message.into() })))
}

fn map_internal(err: anyhow::Error) -> ApiError {
    api_error(StatusCode::SERVICE_UNAVAILABLE, err.to_string())
}

fn map_bad_request(err: anyhow::Error) -> ApiError {
    api_error(StatusCode::BAD_REQUEST, err.to_string())
}

fn map_conflict(err: anyhow::Error) -> ApiError {
    api_error(StatusCode::CONFLICT, err.to_string())
}

fn format_seconds(value: Option<f64>) -> Option<String> {
    let total = value?.max(0.0) as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    Some(if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    })
}

fn output_value(output: &OutputDescriptor) -> Value {
    json!({
        "id": output.id,
        "name": output.name,
        "state": output.state,
        "device_class": output.device_class,
        "alsa_card": output.alsa_card,
        "selected": output.selected,
        "available": output.available,
        "capabilities": {
            "sample_format": output.capabilities.sample_format,
            "sample_rate": output.capabilities.sample_rate,
            "channels": output.capabilities.channels,
            "channel_map": output.capabilities.channel_map,
            "alsa_device": output.capabilities.alsa_device,
            "device_api": output.capabilities.device_api,
            "device_bus": output.capabilities.device_bus,
        }
    })
}

#[derive(Clone)]
pub struct WebController {
    pub config: Arc<AppConfig>,
    pub audio: AudioEngine,
    pub state: SharedRuntimeState,
    pub source_state: SharedSourceState,
    events: broadcast::Sender<String>,
    event_demand: Arc<Notify>,
    audio_control_lock: Arc<Mutex<()>>,
    audio_topology_revision: Arc<AtomicU64>,
    mpd: MpdMonitor,
    mpris: MprisMonitor,
}

impl WebController {
    pub fn new(
        config: Arc<AppConfig>,
        audio: AudioEngine,
        state: SharedRuntimeState,
        source_state: SharedSourceState,
    ) -> Self {
        let (events, _) = broadcast::channel(8);
        Self {
            audio,
            config,
            state,
            source_state,
            events,
            event_demand: Arc::new(Notify::new()),
            audio_control_lock: Arc::new(Mutex::new(())),
            audio_topology_revision: Arc::new(AtomicU64::new(0)),
            mpd: MpdMonitor::new(),
            mpris: MprisMonitor::new(),
        }
    }

    pub async fn priority_state(&self) -> Value {
        let talkover = self
            .audio
            .config()
            .map(|audio| audio.duck_only_during_announcement)
            .unwrap_or(false);
        let state = self.state.lock().await;
        json!({
            "mode": state.mode,
            "active": state.mode == "alert" || state.minute_silence_active,
            "blocking": state.minute_silence_active || (state.mode == "alert" && !talkover),
            "duck_only_during_announcement": talkover,
            "minute_silence_active": state.minute_silence_active,
            "matched_uids": state.matched_uids,
            "last_success_at": state.last_success_at,
            "last_change_at": state.last_change_at,
            "last_error": state.last_error,
        })
    }

    pub async fn ensure_controls_available(&self) -> Result<()> {
        let state = self.state.lock().await;
        let talkover = self.audio.config()?.duck_only_during_announcement;
        if state.minute_silence_active || (state.mode == "alert" && !talkover) {
            bail!("Керування музикою заблоковано пріоритетним оповіщенням");
        }
        Ok(())
    }

    pub async fn run(
        &self,
        program: &str,
        args: &[&str],
        check: bool,
        timeout: u64,
    ) -> Result<command::CommandOutput> {
        command::run(program, args, check, timeout).await
    }

    pub async fn mpd_status(&self) -> Result<Value> {
        Ok(self.mpd.snapshot().await)
    }

    fn mpd_has_session(mpd: &Value) -> bool {
        mpd.get("available").and_then(Value::as_bool) == Some(true)
            && matches!(
                mpd.get("state").and_then(Value::as_str),
                Some("playing" | "paused")
            )
    }

    fn summarize_sources(
        items: Vec<crate::audio_backend::StreamState>,
        music_index: u32,
        winner: Option<&str>,
        mpd: &Value,
    ) -> Vec<Value> {
        // Pulse may briefly retain an old sink-input while MPD changes URLs.
        // The control API represents logical programme sources, not transport
        // implementation details, so retain only the newest stream per source.
        let mut sources = BTreeMap::<String, (u32, Value)>::new();

        for item in items {
            if item.sink != music_index || item.corked {
                continue;
            }

            let property = |name: &str| item.property(name).to_owned();
            let binary = property("application.process.binary");
            let application = {
                let value = property("application.name");
                if !value.is_empty() {
                    value
                } else if !binary.is_empty() {
                    binary.clone()
                } else {
                    "Мережеве джерело".into()
                }
            };
            let media = {
                let title = property("media.title");
                if !title.is_empty() {
                    title
                } else {
                    let name = property("media.name");
                    if name.is_empty() {
                        if item.name.is_empty() {
                            "Аудіопотік".into()
                        } else {
                            item.name.clone()
                        }
                    } else {
                        name
                    }
                }
            };

            let identity =
                format!("{application} {binary} {media} {}", item.name).to_ascii_lowercase();
            let (source_key, mut source_type) = if identity.contains("spotify") {
                ("spotify".to_owned(), "Spotify Connect".to_owned())
            } else if identity.contains("shairport") || identity.contains("airplay") {
                ("airplay".to_owned(), "AirPlay".to_owned())
            } else if identity.contains("gmediarender")
                || identity.contains("gstreamer")
                || identity.contains("dlna")
            {
                ("dlna".to_owned(), "DLNA / UPnP".to_owned())
            } else if identity.contains("mpd") {
                ("mpd".to_owned(), "Локальна бібліотека".to_owned())
            } else if identity.contains("proaudio-network")
                || identity.contains("proaudionetworkinput")
            {
                ("network".to_owned(), "Network Audio".to_owned())
            } else {
                (format!("other:{binary}"), application.clone())
            };

            let mut media = media;
            if source_key == "mpd" {
                // MPD keeps a silent Pulse sink-input after stopping. It is a
                // reusable transport object, not an active audio session.
                if !Self::mpd_has_session(mpd) {
                    continue;
                }
                source_type = if mpd.get("is_stream").and_then(Value::as_bool) == Some(true) {
                    "Інтернет-радіо".to_owned()
                } else {
                    "Локальна бібліотека".to_owned()
                };
                media = ["title", "station", "stream_url", "file"]
                    .into_iter()
                    .filter_map(|key| mpd.get(key).and_then(Value::as_str))
                    .find(|value| !value.is_empty())
                    .map(str::to_owned)
                    .unwrap_or(media);
            }

            let value = json!({
                "key": source_key,
                "active": winner == Some(source_key.as_str()),
                "type": source_type,
                "application": application,
                "media": media,
            });
            let replace = sources
                .get(&source_key)
                .is_none_or(|(current_index, _)| item.index > *current_index);
            if replace {
                sources.insert(source_key, (item.index, value));
            }
        }

        let mut result = sources.into_values().collect::<Vec<_>>();
        result.sort_by(|left, right| right.0.cmp(&left.0));
        result.into_iter().map(|(_, source)| source).collect()
    }

    pub async fn active_sources(&self, mpd: &Value) -> Result<Vec<Value>> {
        let music_index = self
            .audio
            .sink_state(&self.config.audio.music_sink)
            .await?
            .index;
        let winner = self.source_state.read().await.clone();
        Ok(Self::summarize_sources(
            self.audio.list_sink_inputs().await?,
            music_index,
            winner.as_deref(),
            mpd,
        ))
    }

    pub async fn sink_state(&self, sink: &str) -> Result<Value> {
        let state = self.audio.sink_state(sink).await?;
        Ok(json!({
            "name": sink,
            "volume": (state.average_percent() * 10.0).round() / 10.0,
            "db": (state.average_db() * 100.0).round() / 100.0,
            "muted": state.muted,
        }))
    }

    pub async fn physical_sink(&self) -> Result<String> {
        Ok(self.audio.active_output().await?.id)
    }

    pub async fn audio_outputs(&self) -> Result<Vec<Value>> {
        Ok(self
            .audio
            .list_outputs()
            .await?
            .iter()
            .map(output_value)
            .collect())
    }

    pub async fn select_audio_output(&self, requested: &str) -> Result<Value> {
        let _guard = self.audio_control_lock.lock().await;
        let selected = self.audio.select_output(requested.trim()).await?;
        Ok(json!({
            "selected": output_value(&selected),
            "applying": false,
            "applied": true,
        }))
    }

    pub async fn hardware_mixers(&self) -> Result<Vec<Value>> {
        let cards_text = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
        let mut result = Vec::new();

        for capture in ALSA_CARD_RE.captures_iter(&cards_text) {
            let card = capture
                .get(1)
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(0);
            let card_name = capture.get(2).map(|m| m.as_str().trim()).unwrap_or("");
            let card_text = card.to_string();
            let controls = self
                .run("amixer", &["-c", &card_text, "scontrols"], false, 8)
                .await?;
            let contents = self
                .run("amixer", &["-c", &card_text, "contents"], false, 8)
                .await?;
            if controls.code != 0 {
                continue;
            }

            for control_capture in ALSA_CONTROL_RE.captures_iter(&controls.stdout) {
                let control = control_capture.get(1).map(|m| m.as_str()).unwrap_or("");
                let details = self
                    .run("amixer", &["-c", &card_text, "sget", control], false, 8)
                    .await?;
                if details.code != 0 || !details.stdout.contains("Playback") {
                    continue;
                }

                let values = ALSA_PERCENT_RE
                    .captures_iter(&details.stdout)
                    .filter_map(|c| c.get(1))
                    .filter_map(|m| m.as_str().parse::<f64>().ok())
                    .collect::<Vec<_>>();
                if values.is_empty() {
                    continue;
                }
                let percent = values.iter().sum::<f64>() / values.len() as f64;
                let actual_db_values = ALSA_DB_RE
                    .captures_iter(&details.stdout)
                    .filter_map(|c| c.get(1))
                    .filter_map(|m| m.as_str().parse::<f64>().ok())
                    .collect::<Vec<_>>();
                let actual_db = (!actual_db_values.is_empty())
                    .then(|| actual_db_values.iter().sum::<f64>() / actual_db_values.len() as f64);

                let volume_marker = format!("name='{control} Playback Volume'");
                let content_block = contents
                    .stdout
                    .split("\nnumid=")
                    .find(|block| block.contains(&volume_marker))
                    .unwrap_or("");
                let (db_min, db_max) = if let Some(c) = ALSA_DB_MINMAX_RE.captures(content_block) {
                    (
                        c.get(1).and_then(|m| m.as_str().parse::<f64>().ok()),
                        c.get(2).and_then(|m| m.as_str().parse::<f64>().ok()),
                    )
                } else if let Some(c) = ALSA_DB_SCALE_RE.captures(content_block) {
                    let min = c.get(1).and_then(|m| m.as_str().parse::<f64>().ok());
                    let step = c.get(2).and_then(|m| m.as_str().parse::<f64>().ok());
                    let limits = ALSA_LIMITS_RE.captures(&details.stdout);
                    let raw_min = limits
                        .as_ref()
                        .and_then(|v| v.get(1))
                        .and_then(|m| m.as_str().parse::<f64>().ok());
                    let raw_max = limits
                        .as_ref()
                        .and_then(|v| v.get(2))
                        .and_then(|m| m.as_str().parse::<f64>().ok());
                    let max = match (min, step, raw_min, raw_max) {
                        (Some(min), Some(step), Some(raw_min), Some(raw_max)) => {
                            Some(min + (raw_max - raw_min) * step)
                        }
                        _ => None,
                    };
                    (min, max)
                } else {
                    (None, None)
                };
                let db_reference = match (db_min, db_max) {
                    (Some(min), Some(max)) if min <= 0.0 && max >= 0.0 => Some(0.0),
                    (_, Some(max)) => Some(max),
                    _ => None,
                };
                let normalized_db = match (actual_db, db_reference) {
                    (Some(actual), Some(reference)) => (actual - reference).clamp(-60.0, 0.0),
                    _ if percent > 0.0 => (20.0 * (percent / 100.0).log10()).clamp(-60.0, 0.0),
                    _ => -60.0,
                };
                let limits = ALSA_LIMITS_RE.captures(&details.stdout);
                result.push(json!({
                    "card": card,
                    "card_name": card_name,
                    "control": control,
                    "volume": percent.round(),
                    "muted": details.stdout.contains("[off]"),
                    "db": (normalized_db * 100.0).round() / 100.0,
                    "hardware_db": actual_db,
                    "db_min": db_min,
                    "db_max": db_max,
                    "db_reference": db_reference,
                    "raw_min": limits.as_ref().and_then(|c| c.get(1)).and_then(|m| m.as_str().parse::<i64>().ok()),
                    "raw_max": limits.as_ref().and_then(|c| c.get(2)).and_then(|m| m.as_str().parse::<i64>().ok()),
                }));
            }
        }
        Ok(result)
    }

    pub async fn mixer_state(&self) -> Result<Value> {
        let music = self.sink_state(&self.config.audio.music_sink).await?;
        let alert = self.sink_state(&self.config.audio.alert_sink).await?;
        let state = self.audio.master_state(DEFAULT_MASTER_SINK).await?;
        let master = json!({
            "name": state.name,
            "volume": (state.average_percent() * 10.0).round() / 10.0,
            "db": (state.average_db() * 100.0).round() / 100.0,
            "muted": state.muted,
            "card_name": "Master",
            "control": DEFAULT_MASTER_SINK,
            "backend": self.audio.master_backend_name(),
            "transport_backend": self.audio.backend_name(),
        });
        Ok(json!({ "music": music, "alert": alert, "master": master }))
    }

    pub async fn set_mixer_db(&self, target: &str, db: f64, muted: Option<bool>) -> Result<Value> {
        let _guard = self.audio_control_lock.lock().await;
        if !(-60.0..=0.0).contains(&db) {
            bail!("Рівень має бути в межах -60..0 dB");
        }

        let is_muted = muted == Some(true) || db <= -60.0;
        match target {
            "master" => {
                if muted != Some(true) {
                    self.audio.set_master_db(DEFAULT_MASTER_SINK, db).await?;
                }
                self.audio
                    .set_master_mute(DEFAULT_MASTER_SINK, is_muted)
                    .await?;
            }
            "music" => {
                if muted != Some(true) {
                    self.audio
                        .set_sink_db(&self.config.audio.music_sink, db)
                        .await?;
                }
                self.audio
                    .set_sink_mute(&self.config.audio.music_sink, is_muted)
                    .await?;
            }
            "alert" => {
                if muted != Some(true) {
                    self.audio
                        .set_sink_db(&self.config.audio.alert_sink, db)
                        .await?;
                }
                self.audio
                    .set_sink_mute(&self.config.audio.alert_sink, is_muted)
                    .await?;
            }
            _ => bail!("Невідомий канал мікшера"),
        }

        self.mixer_state().await
    }

    pub fn audio_settings(&self) -> Result<Value> {
        let (audio, minute) = self.audio.settings()?;
        Ok(json!({
            "air_raid_alerts_enabled": audio.notifications_enabled,
            "notifications_enabled": audio.notifications_enabled,
            "duck_db": audio.duck_db,
            "duck_fade_seconds": audio.duck_fade_seconds,
            "restore_fade_seconds": audio.restore_fade_seconds,
            "alert_volume_percent": audio.alert_volume_percent,
            "default_restore_volume_percent": audio.default_restore_volume_percent,
            "minute_silence_volume_percent": minute.volume_percent,
            "minute_silence_enabled": minute.enabled,
            "minute_silence_start_time": minute.start_time,
            "minute_silence_timezone": minute.timezone,
            "minute_silence_catch_up_seconds": minute.catch_up_seconds,
            "minute_silence_music_fade_seconds": minute.music_fade_seconds,
            "alert_repeat_interval_minutes": audio.alert_repeat_interval_minutes,
            "duck_only_during_announcement": audio.duck_only_during_announcement,
            "sample_rate_mode": audio.sample_rate_mode,
            "sample_rate": audio.sample_rate,
            "allowed_sample_rates": audio.allowed_sample_rates,
        }))
    }

    pub async fn audio_diagnostics(&self) -> Result<Value> {
        let active_output = self.audio.active_output().await.ok();
        let physical = match active_output.as_ref() {
            Some(output) => self.sink_state(&output.id).await.ok(),
            None => None,
        };
        let music = self.sink_state(&self.config.audio.music_sink).await.ok();
        let alert = self.sink_state(&self.config.audio.alert_sink).await.ok();
        let master = self
            .audio
            .master_state(DEFAULT_MASTER_SINK)
            .await
            .ok()
            .map(|state| {
                json!({
                    "name": state.name,
                    "volume": (state.average_percent() * 10.0).round() / 10.0,
                    "db": (state.average_db() * 100.0).round() / 100.0,
                    "muted": state.muted,
                })
            });
        let hardware = self.hardware_mixers().await.unwrap_or_default();
        let settings = self.audio_settings()?;
        Ok(json!({
            "transport_backend": self.audio.backend_name(),
            "master_backend": self.audio.master_backend_name(),
            "output_router_backend": self.audio.output_router_name(),
            "processing": {
                "master_sink": DEFAULT_MASTER_SINK,
                "sample_rate_mode": settings.get("sample_rate_mode"),
                "sample_rate": settings.get("sample_rate"),
                "allowed_sample_rates": settings.get("allowed_sample_rates"),
            },
            "active_output": active_output.as_ref().map(output_value),
            "logical_buses": {
                "music": music,
                "alert": alert,
                "master": master,
            },
            "physical_sink": physical,
            "hardware_mixers": hardware,
        }))
    }

    pub fn alert_settings(&self) -> Result<Value> {
        let config = effective_provider(&self.config.provider)?;
        Ok(json!({
            "endpoint": config.endpoint,
            "location_uid": config.location_uid,
            "location_type": config.location_type,
            "poll_interval_seconds": config.poll_interval_seconds,
            "request_timeout_seconds": config.request_timeout_seconds,
            "rate_limit_backoff_seconds": config.rate_limit_backoff_seconds,
            "clear_confirmations": config.clear_confirmations,
            "token_configured": config.resolve_token().is_ok(),
        }))
    }

    fn local_player(&self, mpd: &Value) -> Value {
        let state = mpd
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("stopped");
        let queue_length = mpd.get("queue_length").and_then(Value::as_u64).unwrap_or(0);
        let has_track = mpd
            .get("file")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty());
        let elapsed = mpd.get("elapsed").and_then(Value::as_str);
        let duration = mpd.get("duration").and_then(Value::as_str);
        json!({
            "source": if mpd.get("is_stream").and_then(Value::as_bool) == Some(true) { "Інтернет-радіо" } else { "Локальна бібліотека" },
            "backend": "mpd",
            "state": state,
            "title": mpd.get("title").and_then(Value::as_str).unwrap_or(""),
            "artist": mpd.get("artist").and_then(Value::as_str).filter(|v| !v.is_empty())
                .or_else(|| mpd.get("station").and_then(Value::as_str)).unwrap_or(""),
            "album": mpd.get("album").and_then(Value::as_str).unwrap_or(""),
            "art_url": Value::Null,
            "position_seconds": crate::media_time::clock_to_seconds(elapsed),
            "duration_seconds": crate::media_time::clock_to_seconds(duration),
            "elapsed": elapsed,
            "duration": duration,
            "progress": mpd.get("progress").and_then(Value::as_u64).unwrap_or(0),
            "controls": {
                "play": queue_length > 0 || has_track,
                "pause": state == "playing",
                "stop": matches!(state, "playing" | "paused"),
                "next": queue_length > 1,
                "prev": queue_length > 1,
            },
            "_service": Value::Null,
        })
    }

    fn external_fallback(&self, source: &Value) -> Value {
        json!({
            "source": source.get("type").and_then(Value::as_str).unwrap_or("Мережеве джерело"),
            "backend": "external",
            "state": "playing",
            "title": source.get("media").and_then(Value::as_str).unwrap_or("Аудіопотік"),
            "artist": source.get("application").and_then(Value::as_str).unwrap_or(""),
            "album": "",
            "art_url": Value::Null,
            "position_seconds": Value::Null,
            "duration_seconds": Value::Null,
            "elapsed": Value::Null,
            "duration": Value::Null,
            "progress": 0,
            "controls": { "play": false, "pause": false, "stop": false, "next": false, "prev": false },
            "_service": Value::Null,
        })
    }

    fn idle_player(&self) -> Value {
        json!({
            "source": "Немає потоку",
            "backend": "none",
            "state": "stopped",
            "title": "", "artist": "", "album": "",
            "art_url": Value::Null,
            "position_seconds": Value::Null,
            "duration_seconds": Value::Null,
            "elapsed": Value::Null,
            "duration": Value::Null,
            "progress": 0,
            "controls": { "play": false, "pause": false, "stop": false, "next": false, "prev": false },
            "_service": Value::Null,
        })
    }

    pub async fn resolve_active_player(&self, sources: &[Value], mpd: &Value) -> Result<Value> {
        let winner = self.source_state.read().await.clone();
        if winner.as_deref() == Some("mpd") && Self::mpd_has_session(mpd) {
            return Ok(self.local_player(mpd));
        }
        let external = winner.as_deref().and_then(|key| {
            sources
                .iter()
                .find(|source| source.get("key").and_then(Value::as_str) == Some(key))
        });
        if let Some(external) = external {
            let source_type = external.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(source_type, "Spotify Connect" | "AirPlay") {
                if let Some(player) = self.mpris_player(source_type).await {
                    return Ok(player);
                }
            }
            if source_type == "DLNA / UPnP" {
                match dlna::client().player().await {
                    Ok(Some(player)) => return Ok(player),
                    Ok(None) => {}
                    Err(err) => debug!(error = %err, "DLNA AVTransport metadata unavailable"),
                }
                if let Some(player) = dlna::client().cached_player().await {
                    return Ok(player);
                }
            }
            return Ok(self.external_fallback(external));
        }

        for source_type in ["Spotify Connect", "AirPlay"] {
            if let Some(player) = self.mpris_player(source_type).await {
                if matches!(
                    player.get("state").and_then(Value::as_str),
                    Some("playing" | "paused")
                ) {
                    return Ok(player);
                }
            }
        }

        match dlna::client().known_player().await {
            Ok(Some(player))
                if matches!(
                    player.get("state").and_then(Value::as_str),
                    Some("playing" | "paused")
                ) =>
            {
                return Ok(player);
            }
            Ok(_) => {}
            Err(err) => debug!(error = %err, "Cached DLNA AVTransport state unavailable"),
        }

        if Self::mpd_has_session(mpd) {
            return Ok(self.local_player(mpd));
        }
        Ok(self.idle_player())
    }

    pub async fn status(&self) -> Result<Value> {
        let snapshot = self.audio.snapshot().await?;
        let music_volume = snapshot.volumes_percent.iter().sum::<f64>()
            / snapshot.volumes_percent.len().max(1) as f64;
        let volume = music_volume;
        let muted = snapshot.muted;
        let physical = match self.physical_sink().await {
            Ok(sink) => self.sink_state(&sink).await.ok(),
            Err(_) => None,
        };
        let alert_bus = self.sink_state(&self.config.audio.alert_sink).await.ok();
        let master = self
            .audio
            .master_state(DEFAULT_MASTER_SINK)
            .await
            .ok()
            .map(|state| {
                json!({
                    "name": state.name,
                    "volume": (state.average_percent() * 10.0).round() / 10.0,
                    "db": (state.average_db() * 100.0).round() / 100.0,
                    "muted": state.muted,
                    "card_name": "Master",
                    "control": DEFAULT_MASTER_SINK,
                    "backend": self.audio.master_backend_name(),
                    "transport_backend": self.audio.backend_name(),
                })
            });
        let mpd = self
            .mpd_status()
            .await
            .unwrap_or_else(|_| json!({ "available": false, "state": "unavailable" }));
        let sources = self.active_sources(&mpd).await.unwrap_or_default();
        let mut player = self
            .resolve_active_player(&sources, &mpd)
            .await
            .unwrap_or_else(|_| self.idle_player());
        if let Some(object) = player.as_object_mut() {
            object.remove("_service");
        }
        Ok(json!({
            "name": "ProAudio Player",
            "volume": (volume * 10.0).round() / 10.0,
            "muted": muted,
            "audio_topology_revision": self.audio_topology_revision.load(Ordering::Relaxed),
            "priority": self.priority_state().await,
            "mpd": mpd,
            "sources": sources,
            "audio_levels": {
                "music_bus": (music_volume * 10.0).round() / 10.0,
                "master": master,
                "physical": physical,
                "hardware": Value::Null,
                "alert_bus": alert_bus,
            },
            "player": player,
        }))
    }

    pub async fn control_active_player(&self, action: &str) -> Result<Value> {
        if !PLAYER_ACTIONS.contains(&action) {
            bail!("Невідома дія");
        }
        self.ensure_controls_available().await?;
        let mpd = self.mpd_status().await?;
        let sources = self.active_sources(&mpd).await.unwrap_or_default();
        let player = self.resolve_active_player(&sources, &mpd).await?;
        if player
            .get("controls")
            .and_then(Value::as_object)
            .and_then(|v| v.get(action))
            .and_then(Value::as_bool)
            != Some(true)
        {
            bail!(
                "{} не підтримує цю команду через веб-інтерфейс",
                player
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("Активне джерело")
            );
        }
        let backend = player
            .get("backend")
            .and_then(Value::as_str)
            .unwrap_or("none");
        if backend == "mpd" {
            self.run("mpc", &[action], true, 8).await?;
        } else if matches!(backend, "spotify-mpris" | "airplay-mpris") {
            let service = player
                .get("_service")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("MPRIS-сервіс активного джерела не знайдено"))?;
            let mpris_method = match action {
                "play" => "Play",
                "pause" => "Pause",
                "stop" => "Stop",
                "next" => "Next",
                "prev" => "Previous",
                _ => unreachable!(),
            };

            if backend == "airplay-mpris" {
                let airplay_method = match action {
                    "play" => "Resume",
                    "pause" => "Pause",
                    "stop" => "Stop",
                    "next" => "Next",
                    "prev" => "Previous",
                    _ => unreachable!(),
                };
                if let Err(error) = self.airplay_control(airplay_method).await {
                    debug!(
                        action,
                        error = %error,
                        "Shairport native control unavailable; falling back to MPRIS"
                    );
                    self.mpris_control(service, mpris_method).await?;
                }
            } else {
                self.mpris_control(service, mpris_method).await?;
            }

        } else if backend == "dlna-upnp" {
            dlna::client().control(action).await?;
        } else {
            bail!("Активне джерело не має доступного керування");
        }
        Ok(json!({ "action": action, "source": player.get("source"), "backend": backend }))
    }

    pub async fn library(&self) -> Result<Vec<String>> {
        let output = self.run("mpc", &["listall"], true, 8).await?;
        Ok(output
            .stdout
            .lines()
            .filter(|line| !line.is_empty())
            .take(self.config.api.max_library_items)
            .map(str::to_owned)
            .collect())
    }

    pub async fn playlists(&self) -> Result<Vec<String>> {
        let output = self.run("mpc", &["lsplaylists"], true, 8).await?;
        Ok(output
            .stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect())
    }

    pub async fn queue(&self) -> Result<Vec<Value>> {
        let output = self
            .run(
                "mpc",
                &[
                    "--format",
                    "%position%\t%file%\t%title%\t%artist%\t%album%",
                    "playlist",
                ],
                true,
                8,
            )
            .await?;
        let mut result = Vec::new();
        for line in output.stdout.lines() {
            let mut fields = line.split('\t').map(str::to_owned).collect::<Vec<_>>();
            fields.resize(5, String::new());
            let Ok(position) = fields[0].parse::<u32>() else {
                continue;
            };
            let title = if fields[2].is_empty() {
                Path::new(&fields[1])
                    .file_name()
                    .and_then(|v| v.to_str())
                    .unwrap_or(&fields[1])
                    .to_owned()
            } else {
                fields[2].clone()
            };
            result.push(json!({
                "position": position,
                "file": fields[1],
                "title": title,
                "artist": fields[3],
                "album": fields[4],
            }));
        }
        Ok(result)
    }
}

#[derive(Deserialize)]
struct VolumeBody {
    percent: f64,
}
#[derive(Deserialize)]
struct MuteBody {
    muted: bool,
}
#[derive(Deserialize)]
struct AudioLevelBody {
    target: String,
    percent: f64,
}
#[derive(Deserialize)]
struct MixerBody {
    target: String,
    db: f64,
    muted: Option<bool>,
}
#[derive(Deserialize)]
struct AudioOutputBody {
    id: String,
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
#[derive(Deserialize)]
struct PlayerBody {
    action: String,
}
#[derive(Deserialize)]
struct PathBody {
    path: String,
}
#[derive(Deserialize)]
struct PlaylistBody {
    name: String,
}
#[derive(Deserialize)]
struct StreamBody {
    url: String,
}
#[derive(Deserialize)]
struct QueueBody {
    position: u32,
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

fn safe_mpd_path(value: &str, playlist: bool) -> Result<String> {
    let value = value.trim();
    let path = Path::new(value);
    if value.is_empty()
        || value.contains('\0')
        || value.contains('\n')
        || value.starts_with('-')
        || path.is_absolute()
        || path
            .components()
            .any(|component| component.as_os_str() == "..")
        || (playlist && value.contains('/'))
    {
        bail!("Некоректний шлях");
    }
    Ok(value.to_owned())
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

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "api_version": API_VERSION }))
}

async fn capabilities() -> Json<Value> {
    Json(json!({
        "api_version": API_VERSION,
        "events": "sse",
        "features": [
            "status", "player_control", "audio_mixer", "audio_outputs", "audio_diagnostics",
            "audio_hardware_read_only", "audio_settings", "meters", "library", "playlists",
            "queue", "network_streams", "network_audio_ingest", "alert_settings", "alert_media"
        ]
    }))
}

async fn events(State(controller): State<WebController>) -> impl IntoResponse {
    let mut receiver = controller.events.subscribe();
    controller.event_demand.notify_one();
    let (sender, stream) = mpsc::channel(4);
    tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(payload) => {
                    if sender
                        .send(Ok(Event::default().event("status").data(payload)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    Sse::new(EventStream { receiver: stream }).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    )
}

async fn status(State(controller): State<WebController>) -> ApiResult {
    controller.status().await.map(Json).map_err(map_internal)
}

async fn set_volume(
    State(controller): State<WebController>,
    Json(body): Json<VolumeBody>,
) -> ApiResult {
    if !(0.0..=100.0).contains(&body.percent) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Гучність має бути 0..100",
        ));
    }
    controller
        .ensure_controls_available()
        .await
        .map_err(map_conflict)?;
    controller
        .audio
        .set_music_volume(body.percent)
        .await
        .map_err(map_internal)?;
    controller
        .audio
        .set_music_mute(body.percent <= 0.0)
        .await
        .map_err(map_internal)?;
    Ok(Json(
        json!({ "volume": body.percent, "muted": body.percent == 0.0 }),
    ))
}

async fn set_mute(
    State(controller): State<WebController>,
    Json(body): Json<MuteBody>,
) -> ApiResult {
    controller
        .ensure_controls_available()
        .await
        .map_err(map_conflict)?;
    controller
        .audio
        .set_music_mute(body.muted)
        .await
        .map_err(map_internal)?;
    Ok(Json(json!({ "muted": body.muted })))
}

async fn set_audio_level(
    State(controller): State<WebController>,
    Json(body): Json<AudioLevelBody>,
) -> ApiResult {
    if !(0.0..=100.0).contains(&body.percent) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Гучність має бути 0..100",
        ));
    }
    if !matches!(body.target.as_str(), "master" | "music" | "alert") {
        return Err(api_error(StatusCode::BAD_REQUEST, "Невідомий аудіорівень"));
    }
    if body.target != "master" {
        controller
            .ensure_controls_available()
            .await
            .map_err(map_conflict)?;
    }
    let sink = match body.target.as_str() {
        "master" => {
            controller
                .audio
                .set_master_percent(DEFAULT_MASTER_SINK, body.percent)
                .await
                .map_err(map_internal)?;
            let state = controller
                .audio
                .master_state(DEFAULT_MASTER_SINK)
                .await
                .map_err(map_internal)?;
            return Ok(Json(json!({
                "name": state.name,
                "volume": (state.average_percent() * 10.0).round() / 10.0,
                "db": (state.average_db() * 100.0).round() / 100.0,
                "muted": state.muted,
            })));
        }
        "music" => controller.config.audio.music_sink.clone(),
        "alert" => controller.config.audio.alert_sink.clone(),
        _ => unreachable!(),
    };
    controller
        .audio
        .set_sink_percent(&sink, body.percent)
        .await
        .map_err(map_internal)?;
    controller
        .sink_state(&sink)
        .await
        .map(Json)
        .map_err(map_internal)
}

async fn get_mixer(State(controller): State<WebController>) -> ApiResult {
    controller
        .mixer_state()
        .await
        .map(Json)
        .map_err(map_internal)
}

async fn set_mixer(
    State(controller): State<WebController>,
    Json(body): Json<MixerBody>,
) -> ApiResult {
    if !matches!(body.target.as_str(), "master" | "music" | "alert") {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Невідомий канал мікшера",
        ));
    }
    if !body.db.is_finite() || !(-60.0..=0.0).contains(&body.db) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Рівень має бути в межах -60..0 dB",
        ));
    }
    // MASTER is the final logical user attenuation stage and remains available
    // even while priority audio owns the MUSIC bus.
    if body.target != "master" {
        controller
            .ensure_controls_available()
            .await
            .map_err(map_conflict)?;
    }
    controller
        .set_mixer_db(&body.target, body.db, body.muted)
        .await
        .map(Json)
        .map_err(map_internal)
}

async fn audio_outputs(State(controller): State<WebController>) -> ApiResult {
    controller
        .audio_outputs()
        .await
        .map(|items| Json(json!({ "items": items })))
        .map_err(map_internal)
}

async fn select_audio_output(
    State(controller): State<WebController>,
    Json(body): Json<AudioOutputBody>,
) -> ApiResult {
    let id = body.id.trim();
    if id.is_empty() || id.chars().any(|value| matches!(value, '\n' | '\r' | '\0')) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Некоректний ідентифікатор аудіовиходу",
        ));
    }
    controller
        .ensure_controls_available()
        .await
        .map_err(map_conflict)?;
    controller
        .select_audio_output(id)
        .await
        .map(Json)
        .map_err(map_conflict)
}

async fn hardware(State(controller): State<WebController>) -> ApiResult {
    controller
        .hardware_mixers()
        .await
        .map(|items| Json(json!({ "items": items, "read_only": true })))
        .map_err(map_internal)
}

async fn audio_diagnostics(State(controller): State<WebController>) -> ApiResult {
    controller
        .audio_diagnostics()
        .await
        .map(Json)
        .map_err(map_internal)
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

async fn player(
    State(controller): State<WebController>,
    Json(body): Json<PlayerBody>,
) -> ApiResult {
    if !PLAYER_ACTIONS.contains(&body.action.as_str()) {
        return Err(api_error(StatusCode::BAD_REQUEST, "Невідома дія"));
    }
    controller
        .ensure_controls_available()
        .await
        .map_err(map_conflict)?;
    controller
        .control_active_player(&body.action)
        .await
        .map(Json)
        .map_err(map_internal)
}

async fn library(State(controller): State<WebController>) -> ApiResult {
    controller
        .library()
        .await
        .map(|items| Json(json!({ "items": items })))
        .map_err(map_internal)
}

async fn refresh_library(State(controller): State<WebController>) -> ApiResult {
    controller
        .ensure_controls_available()
        .await
        .map_err(map_conflict)?;
    controller
        .run("mpc", &["update"], true, 60)
        .await
        .map_err(map_internal)?;
    Ok(Json(json!({ "updating": true })))
}

async fn play_file(
    State(controller): State<WebController>,
    Json(body): Json<PathBody>,
) -> ApiResult {
    let path = safe_mpd_path(&body.path, false).map_err(map_bad_request)?;
    controller
        .ensure_controls_available()
        .await
        .map_err(map_conflict)?;
    if !controller
        .library()
        .await
        .map_err(map_internal)?
        .contains(&path)
    {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "Файл відсутній у бібліотеці",
        ));
    }
    controller
        .run("mpc", &["clear"], true, 8)
        .await
        .map_err(map_internal)?;
    controller
        .run("mpc", &["add", &path], true, 8)
        .await
        .map_err(map_internal)?;
    controller
        .run("mpc", &["play"], true, 8)
        .await
        .map_err(map_internal)?;
    Ok(Json(json!({ "playing": path })))
}

fn unsafe_ipv4_stream_address(value: Ipv4Addr) -> bool {
    value.is_private()
        || value.is_loopback()
        || value.is_link_local()
        || value.is_multicast()
        || value == Ipv4Addr::UNSPECIFIED
        || value.octets()[0] == 0
}

fn unsafe_stream_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(value) => unsafe_ipv4_stream_address(value),
        IpAddr::V6(value) => {
            if let Some(mapped) = value.to_ipv4_mapped() {
                return unsafe_ipv4_stream_address(mapped);
            }
            value.is_loopback()
                || value.is_unspecified()
                || value.is_multicast()
                || value.is_unique_local()
                || value.is_unicast_link_local()
                || value == Ipv6Addr::UNSPECIFIED
        }
    }
}

async fn validate_stream_url(value: &str) -> Result<String> {
    let value = value.trim();
    let parsed = Url::parse(value).context("Некоректна адреса потоку")?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        bail!("Підтримуються лише HTTP/HTTPS-потоки без облікових даних у URL");
    }

    let host = parsed.host_str().unwrap_or_default();
    let host_lower = host.to_ascii_lowercase();
    if matches!(host_lower.as_str(), "localhost" | "localhost.localdomain")
        || host_lower.ends_with(".local")
    {
        bail!("Локальні адреси потоків заборонені");
    }

    if let Ok(address) = host.parse::<IpAddr>() {
        if unsafe_stream_address(address) {
            bail!("Локальні та службові IP-адреси потоків заборонені");
        }
    } else {
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| anyhow!("Не вдалося визначити порт потоку"))?;
        let addresses = tokio::net::lookup_host((host, port))
            .await
            .context("Не вдалося визначити IP-адресу потоку")?
            .map(|socket| socket.ip())
            .collect::<BTreeSet<_>>();
        if addresses.is_empty() {
            bail!("Домен потоку не має IP-адрес");
        }
        if addresses.into_iter().any(unsafe_stream_address) {
            bail!("Домен потоку резолвиться у локальну або службову IP-адресу");
        }
    }

    Ok(parsed.to_string())
}

async fn radio_stations() -> ApiResult {
    radio_directory::ukrainian_stations()
        .await
        .map(|items| Json(json!({ "source": "radio-browser", "items": items })))
        .map_err(map_internal)
}

async fn play_stream(
    State(controller): State<WebController>,
    Json(body): Json<StreamBody>,
) -> ApiResult {
    let url = validate_stream_url(&body.url)
        .await
        .map_err(map_bad_request)?;
    controller
        .ensure_controls_available()
        .await
        .map_err(map_conflict)?;
    controller
        .run("mpc", &["clear"], true, 8)
        .await
        .map_err(map_internal)?;
    controller
        .run("mpc", &["add", &url], true, 15)
        .await
        .map_err(map_internal)?;
    controller
        .run("mpc", &["play"], true, 8)
        .await
        .map_err(map_internal)?;
    Ok(Json(json!({ "playing": url, "source": "network_stream" })))
}

async fn playlists(State(controller): State<WebController>) -> ApiResult {
    controller
        .playlists()
        .await
        .map(|items| Json(json!({ "items": items })))
        .map_err(map_internal)
}

async fn load_playlist(
    State(controller): State<WebController>,
    Json(body): Json<PlaylistBody>,
) -> ApiResult {
    let name = safe_mpd_path(&body.name, true).map_err(map_bad_request)?;
    controller
        .ensure_controls_available()
        .await
        .map_err(map_conflict)?;
    if !controller
        .playlists()
        .await
        .map_err(map_internal)?
        .contains(&name)
    {
        return Err(api_error(StatusCode::NOT_FOUND, "Плейліст не знайдено"));
    }
    controller
        .run("mpc", &["clear"], true, 8)
        .await
        .map_err(map_internal)?;
    controller
        .run("mpc", &["load", &name], true, 8)
        .await
        .map_err(map_internal)?;
    controller
        .run("mpc", &["play"], true, 8)
        .await
        .map_err(map_internal)?;
    Ok(Json(json!({ "playing_playlist": name })))
}

async fn queue(State(controller): State<WebController>) -> ApiResult {
    controller
        .queue()
        .await
        .map(|items| Json(json!({ "items": items })))
        .map_err(map_internal)
}

async fn play_queue(
    State(controller): State<WebController>,
    Json(body): Json<QueueBody>,
) -> ApiResult {
    controller
        .ensure_controls_available()
        .await
        .map_err(map_conflict)?;
    if body.position == 0
        || !controller
            .queue()
            .await
            .map_err(map_internal)?
            .iter()
            .any(|item| item.get("position").and_then(Value::as_u64) == Some(body.position as u64))
    {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "Позицію у черзі не знайдено",
        ));
    }
    let position = body.position.to_string();
    controller
        .run("mpc", &["play", &position], true, 8)
        .await
        .map_err(map_internal)?;
    Ok(Json(json!({ "playing_position": body.position })))
}

async fn remove_queue(
    State(controller): State<WebController>,
    Json(body): Json<QueueBody>,
) -> ApiResult {
    controller
        .ensure_controls_available()
        .await
        .map_err(map_conflict)?;
    if body.position == 0
        || !controller
            .queue()
            .await
            .map_err(map_internal)?
            .iter()
            .any(|item| item.get("position").and_then(Value::as_u64) == Some(body.position as u64))
    {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "Позицію у черзі не знайдено",
        ));
    }
    let position = body.position.to_string();
    controller
        .run("mpc", &["del", &position], true, 8)
        .await
        .map_err(map_internal)?;
    Ok(Json(json!({ "removed_position": body.position })))
}

async fn clear_queue(State(controller): State<WebController>) -> ApiResult {
    controller
        .ensure_controls_available()
        .await
        .map_err(map_conflict)?;
    controller
        .run("mpc", &["clear"], true, 8)
        .await
        .map_err(map_internal)?;
    Ok(Json(json!({ "cleared": true })))
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

fn api_routes() -> Router<WebController> {
    let control = Router::new()
        .route("/health", get(health))
        .route("/capabilities", get(capabilities))
        .route("/status", get(status))
        .route("/events", get(events))
        .route("/volume", post(set_volume))
        .route("/mute", post(set_mute))
        .route("/audio/level", post(set_audio_level))
        .route("/audio/mixer", get(get_mixer).post(set_mixer))
        .route(
            "/audio/outputs",
            get(audio_outputs).post(select_audio_output),
        )
        .route("/audio/hardware", get(hardware))
        .route("/audio/diagnostics", get(audio_diagnostics))
        .route(
            "/settings/audio",
            get(get_audio_settings).put(put_audio_settings),
        )
        .route("/player", post(player))
        .route("/library", get(library))
        .route("/library/update", post(refresh_library))
        .route("/library/play", post(play_file))
        .route("/streams/play", post(play_stream))
        .route("/radio/stations", get(radio_stations))
        .route("/playlists", get(playlists))
        .route("/playlists/load", post(load_playlist))
        .route("/queue", get(queue))
        .route("/queue/play", post(play_queue))
        .route("/queue/remove", post(remove_queue))
        .route("/queue/clear", post(clear_queue))
        .route(
            "/settings/alerts",
            get(get_alert_settings).put(put_alert_settings),
        )
        .route("/settings/alerts/test", post(test_alert_settings))
        .layer(DefaultBodyLimit::max(API_CONTROL_BODY_BYTES));

    let alert_media = Router::new()
        .route("/settings/alerts/media", get(alert_media::get_alert_media))
        .route(
            "/settings/alerts/media/{kind}",
            axum::routing::put(alert_media::put_alert_media).delete(alert_media::reset_alert_media),
        )
        .layer(DefaultBodyLimit::max(alert_media::MAX_ALERT_MEDIA_BYTES));

    control.merge(alert_media).merge(network_audio::router())
}

pub fn router(controller: WebController) -> Router {
    let routes = api_routes();
    Router::new()
        .nest("/api", routes.clone())
        .nest("/api/v1", routes)
        .merge(upnp::router())
        .merge(webui::router())
        .with_state(controller)
}

pub async fn serve(controller: WebController) -> Result<()> {
    controller.mpd.start();
    controller.mpris.start();
    let host = controller.config.api.host.clone();
    let port = controller.config.api.port;
    let address = format!("{host}:{port}");
    let listener = TcpListener::bind(&address).await?;
    info!(%address, api_version = API_VERSION, "Native control API started");

    // One shared status producer serves every browser. This avoids each client
    // duplicating its own status/mpc/busctl polling workload.
    let event_controller = controller.clone();
    tokio::spawn(async move {
        loop {
            if event_controller.events.receiver_count() == 0 {
                let demanded = event_controller.event_demand.notified();
                if event_controller.events.receiver_count() == 0 {
                    demanded.await;
                }
                continue;
            }

            if let Ok(status) = event_controller.status().await {
                let _ = event_controller.events.send(status.to_string());
            }
            sleep(Duration::from_secs(1)).await;
        }
    });

    // Topology notifications provide an immediate refresh on hotplug/output changes;
    // the one-second producer remains as a low-rate metadata/status heartbeat.
    let mut output_changes = controller.audio.subscribe_output_changes();
    let output_event_controller = controller.clone();
    tokio::spawn(async move {
        while output_changes.changed().await.is_ok() {
            output_event_controller
                .audio_topology_revision
                .fetch_add(1, Ordering::Relaxed);
            if output_event_controller.events.receiver_count() == 0 {
                continue;
            }
            if let Ok(status) = output_event_controller.status().await {
                let _ = output_event_controller.events.send(status.to_string());
            }
        }
    });

    if upnp::public_enabled() {
        tokio::spawn(async move {
            if let Err(err) = upnp::run_ssdp(port).await {
                debug!("UPnP/DLNA SSDP discovery unavailable: {err:#}");
            }
        });
    }
    axum::serve(listener, router(controller)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::audio_backend::StreamState;

    use super::{AudioSettingsBody, WebController};

    #[test]
    fn stream_url_guard_rejects_ipv4_mapped_private_ipv6() {
        assert!(super::unsafe_stream_address(
            "::ffff:192.168.1.10".parse().unwrap()
        ));
        assert!(super::unsafe_stream_address(
            "::ffff:127.0.0.1".parse().unwrap()
        ));
        assert!(!super::unsafe_stream_address(
            "::ffff:8.8.8.8".parse().unwrap()
        ));
    }

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

    fn stream_at(index: u32, binary: &str, application: &str, pid: &str) -> StreamState {
        StreamState {
            index,
            sink: 1,
            name: String::new(),
            properties: HashMap::from([
                ("application.process.binary".into(), binary.into()),
                ("application.name".into(), application.into()),
                ("application.process.id".into(), pid.into()),
            ]),
            volumes_percent: vec![100.0],
            muted: false,
            corked: false,
            has_volume: true,
            volume_writable: true,
        }
    }

    fn stream(binary: &str, application: &str, pid: &str) -> StreamState {
        stream_at(1, binary, application, pid)
    }

    #[test]
    fn stopped_empty_mpd_sink_input_is_not_an_active_source() {
        let mpd = serde_json::json!({
            "available": true,
            "state": "stopped",
            "queue_length": 0,
            "file": "",
        });
        let sources = WebController::summarize_sources(
            vec![stream_at(41, "mpd", "Music Player Daemon", "700")],
            1,
            Some("mpd"),
            &mpd,
        );

        assert!(sources.is_empty());
        assert!(!WebController::mpd_has_session(&mpd));
    }

    #[test]
    fn stopped_remembered_mpd_queue_is_not_an_active_session() {
        let mpd = serde_json::json!({
            "available": true,
            "state": "stopped",
            "queue_length": 1,
            "file": "https://online.kissfm.ua/KissFM_HD",
            "is_stream": true,
            "title": "KISS FM",
        });

        assert!(!WebController::mpd_has_session(&mpd));
        assert!(WebController::summarize_sources(
            vec![stream_at(41, "mpd", "Music Player Daemon", "700")],
            1,
            Some("mpd"),
            &mpd,
        )
        .is_empty());
    }

    #[test]
    fn summarizes_duplicate_mpd_streams_as_one_radio_source() {
        let mpd = serde_json::json!({
            "available": true,
            "state": "playing",
            "is_stream": true,
            "title": "KISS FM",
            "stream_url": "https://online.kissfm.ua/KissFM_HD",
        });
        let sources = WebController::summarize_sources(
            vec![
                stream_at(41, "mpd", "Music Player Daemon", "700"),
                stream_at(47, "mpd", "Music Player Daemon", "700"),
                stream_at(52, "mpd", "Music Player Daemon", "700"),
            ],
            1,
            Some("mpd"),
            &mpd,
        );

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0]["key"], "mpd");
        assert_eq!(sources[0]["active"], true);
        assert_eq!(sources[0]["type"], "Інтернет-радіо");
        assert_eq!(sources[0]["media"], "KISS FM");
    }

    #[test]
    fn source_summary_ignores_corked_and_non_music_streams() {
        let mut corked = stream_at(2, "mpd", "Music Player Daemon", "700");
        corked.corked = true;
        let outside_music = StreamState {
            sink: 9,
            ..stream_at(3, "spotifyd", "Spotify", "701")
        };

        assert!(WebController::summarize_sources(
            vec![corked, outside_music],
            1,
            Some("mpd"),
            &serde_json::json!({}),
        )
        .is_empty());
    }
}
