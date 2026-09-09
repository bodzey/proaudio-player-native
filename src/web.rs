use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tracing::{debug, info};
use url::Url;

use crate::alerts::SharedRuntimeState;
use crate::audio::AudioEngine;
use crate::command;
use crate::config::{
    effective_audio, effective_provider, save_audio_settings, save_provider_settings,
    save_provider_token, validate_audio, validate_provider, AppConfig, ProviderConfig,
};
use crate::fourstream;
use crate::source_arbiter::SharedSourceState;

const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_CSS: &str = include_str!("../static/app.css");
const APP_JS: &str = include_str!("../static/app.js");

const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const PLAYER_ACTIONS: &[&str] = &["play", "pause", "stop", "next", "prev"];
const AUDIO_OUTPUT_FILE: &str = "/var/lib/proaudio-player-alert/audio-output.env";
const AUDIO_BUS_STATE_FILE: &str = "/run/proaudio-player/proaudio-player-bus-modules";

type ApiError = (StatusCode, Json<Value>);
type ApiResult = std::result::Result<Json<Value>, ApiError>;

fn api_error(status: StatusCode, message: impl Into<String>) -> ApiError {
    (status, Json(json!({ "error": message.into() })))
}

fn map_internal(err: anyhow::Error) -> ApiError {
    api_error(StatusCode::SERVICE_UNAVAILABLE, err.to_string())
}

fn content_response(content_type: &'static str, body: &'static str) -> Response {
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

fn unwrap_dbus(value: Value) -> Value {
    match value {
        Value::Object(mut map) => {
            if map.contains_key("type") && map.contains_key("data") {
                return unwrap_dbus(map.remove("data").unwrap_or(Value::Null));
            }
            Value::Object(
                map.into_iter()
                    .map(|(key, value)| (key, unwrap_dbus(value)))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.into_iter().map(unwrap_dbus).collect()),
        other => other,
    }
}

fn collapse_single(mut value: Value) -> Value {
    loop {
        match value {
            Value::Array(mut items)
                if items.len() == 1
                    && matches!(items.first(), Some(Value::Array(_) | Value::Object(_))) =>
            {
                value = items.remove(0);
            }
            _ => return value,
        }
    }
}

fn microseconds_to_seconds(value: &Value) -> Option<f64> {
    match value {
        Value::Number(v) => v.as_f64().map(|n| (n / 1_000_000.0).max(0.0)),
        Value::String(v) => v
            .parse::<f64>()
            .ok()
            .map(|n| (n / 1_000_000.0).max(0.0)),
        _ => None,
    }
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

fn clock_to_seconds(value: Option<&str>) -> Option<f64> {
    let value = value?;
    let parts = value
        .split(':')
        .filter_map(|v| v.parse::<u64>().ok())
        .collect::<Vec<_>>();
    match parts.as_slice() {
        [minutes, seconds] => Some((minutes * 60 + seconds) as f64),
        [hours, minutes, seconds] => Some((hours * 3600 + minutes * 60 + seconds) as f64),
        _ => None,
    }
}

#[derive(Clone)]
pub struct WebController {
    pub config: Arc<AppConfig>,
    pub audio: AudioEngine,
    pub state: SharedRuntimeState,
    pub source_state: SharedSourceState,
}

impl WebController {
    pub fn new(config: Arc<AppConfig>, state: SharedRuntimeState, source_state: SharedSourceState) -> Self {
        Self {
            audio: AudioEngine::new(config.clone()),
            config,
            state,
            source_state,
        }
    }

    pub async fn priority_state(&self) -> Value {
        let state = self.state.lock().await;
        let talkover = effective_audio(&self.config.audio, &self.config.minute_silence)
            .map(|(audio, _)| audio.duck_only_during_announcement)
            .unwrap_or(false);
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
        let talkover = effective_audio(&self.config.audio, &self.config.minute_silence)?
            .0.duck_only_during_announcement;
        if state.minute_silence_active || (state.mode == "alert" && !talkover) {
            bail!("Керування музикою заблоковано пріоритетним оповіщенням");
        }
        Ok(())
    }

    pub async fn run(&self, program: &str, args: &[&str], check: bool, timeout: u64) -> Result<command::CommandOutput> {
        command::run(program, args, check, timeout).await
    }

    pub async fn mpd_status(&self) -> Result<Value> {
        let current = self
            .run(
                "mpc",
                &["--format", "%file%\t%title%\t%artist%\t%album%\t%name%", "current"],
                false,
                8,
            )
            .await?;
        let status = self.run("mpc", &["status"], false, 8).await?;
        if current.code != 0 || status.code != 0 {
            return Ok(json!({
                "available": false,
                "state": "unavailable",
                "error": if !current.stderr.is_empty() { current.stderr } else { status.stderr },
            }));
        }

        let mut fields = current.stdout.split('\t').map(str::to_owned).collect::<Vec<_>>();
        fields.resize(5, String::new());
        let state_re = Regex::new(r"\[(playing|paused)\]")?;
        let volume_re = Regex::new(r"volume:\s*(\d+)%")?;
        let queue_re = Regex::new(r"#(\d+)/(\d+)")?;
        let progress_re = Regex::new(r"(\d+:\d+(?::\d+)?)/(\d+:\d+(?::\d+)?)\s+\((\d+)%\)")?;
        let state = state_re
            .captures(&status.stdout)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("stopped");
        let volume = volume_re
            .captures(&status.stdout)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<u32>().ok());
        let queue = queue_re.captures(&status.stdout);
        let progress = progress_re.captures(&status.stdout);
        let file = fields.first().cloned().unwrap_or_default();
        let is_stream = file.starts_with("http://") || file.starts_with("https://");
        let station = fields.get(4).cloned().unwrap_or_default();
        let title = fields.get(1).filter(|v| !v.is_empty()).cloned()
            .or_else(|| (!station.is_empty()).then(|| station.clone()))
            .or_else(|| Path::new(&file).file_name().and_then(|v| v.to_str()).map(str::to_owned))
            .unwrap_or_else(|| if is_stream { "Мережевий потік".into() } else { String::new() });

        Ok(json!({
            "available": true,
            "state": state,
            "file": file,
            "title": title,
            "artist": fields.get(2).cloned().unwrap_or_default(),
            "album": fields.get(3).cloned().unwrap_or_default(),
            "station": station,
            "is_stream": is_stream,
            "stream_url": if is_stream { Some(file.clone()) } else { None },
            "volume": volume,
            "queue_position": queue.as_ref().and_then(|c| c.get(1)).and_then(|m| m.as_str().parse::<u32>().ok()),
            "queue_length": queue.as_ref().and_then(|c| c.get(2)).and_then(|m| m.as_str().parse::<u32>().ok()).unwrap_or(0),
            "elapsed": progress.as_ref().and_then(|c| c.get(1)).map(|m| m.as_str()),
            "duration": progress.as_ref().and_then(|c| c.get(2)).map(|m| m.as_str()),
            "progress": progress.as_ref().and_then(|c| c.get(3)).and_then(|m| m.as_str().parse::<u32>().ok()).unwrap_or(0),
        }))
    }

    pub async fn active_sources(&self) -> Result<Vec<Value>> {
        let sinks = self.run("pactl", &["-f", "json", "list", "sinks"], false, 8).await?;
        let inputs = self
            .run("pactl", &["-f", "json", "list", "sink-inputs"], false, 8)
            .await?;
        let sinks: Value = serde_json::from_str(&sinks.stdout).unwrap_or_else(|_| json!([]));
        let inputs: Value = serde_json::from_str(&inputs.stdout).unwrap_or_else(|_| json!([]));
        let music_index = sinks.as_array().and_then(|items| {
            items
                .iter()
                .find(|sink| {
                    sink.get("name").and_then(Value::as_str)
                        == Some(self.config.audio.music_sink.as_str())
                })
                .and_then(|sink| sink.get("index"))
                .and_then(Value::as_i64)
        });
        let winner = self.source_state.read().await.clone();
        let mut result = Vec::new();
        for item in inputs.as_array().cloned().unwrap_or_default() {
            if music_index.is_some()
                && item.get("sink").and_then(Value::as_i64) != music_index
            {
                continue;
            }
            if item.get("corked").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let properties = item
                .get("properties")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let property = |name: &str| {
                properties
                    .get(name)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned()
            };
            let binary = property("application.process.binary");
            let application = {
                let v = property("application.name");
                if !v.is_empty() {
                    v
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
                    if name.is_empty() { "Аудіопотік".into() } else { name }
                }
            };
            let identity = format!("{application} {binary}").to_ascii_lowercase();
            let (source_key, source_type) = if identity.contains("spotify") {
                ("spotify".to_owned(), "Spotify Connect")
            } else if identity.contains("shairport") || identity.contains("airplay") {
                ("airplay".to_owned(), "AirPlay")
            } else if identity.contains("gmediarender")
                || identity.contains("gstreamer")
                || identity.contains("dlna")
            {
                ("dlna".to_owned(), "DLNA / UPnP")
            } else if identity.contains("mpd") {
                ("mpd".to_owned(), "Локальна бібліотека")
            } else {
                (format!("other:{binary}"), application.as_str())
            };
            result.push(json!({
                "key": source_key,
                "active": winner.as_deref() == Some(source_key.as_str()),
                "type": source_type,
                "application": application,
                "media": media,
            }));
        }
        Ok(result)
    }

    pub async fn sink_state(&self, sink: &str) -> Result<Value> {
        let volume = self
            .run("pactl", &["get-sink-volume", sink], true, 8)
            .await?;
        let mute = self
            .run("pactl", &["get-sink-mute", sink], true, 8)
            .await?;
        let re = Regex::new(r"(\d+(?:\.\d+)?)%")?;
        let db_re = Regex::new(r"(-?\d+(?:\.\d+)?)\s*dB")?;
        let first = volume.stdout.lines().next().unwrap_or_default();
        let values = re
            .captures_iter(first)
            .filter_map(|c| c.get(1))
            .filter_map(|m| m.as_str().parse::<f64>().ok())
            .collect::<Vec<_>>();
        let average = if values.is_empty() {
            0.0
        } else {
            values.iter().sum::<f64>() / values.len() as f64
        };
        let db_values = db_re.captures_iter(first).filter_map(|c| c.get(1))
            .filter_map(|m| m.as_str().parse::<f64>().ok()).collect::<Vec<_>>();
        let db = if db_values.is_empty() {
            if average <= 0.0 { -60.0 } else { 20.0 * (average / 100.0).log10() }
        } else {
            db_values.iter().sum::<f64>() / db_values.len() as f64
        };
        Ok(json!({
            "name": sink,
            "volume": (average * 10.0).round() / 10.0,
            "db": (db * 100.0).round() / 100.0,
            "muted": mute.stdout.to_ascii_lowercase().ends_with("yes"),
        }))
    }

    pub async fn physical_sink(&self) -> Result<String> {
        let output = self.run("pactl", &["list", "short", "sinks"], true, 8).await?;
        let mut candidates = output
            .stdout
            .lines()
            .filter_map(|line| line.split_whitespace().nth(1))
            .filter(|name| {
                *name != self.config.audio.music_sink
                    && *name != self.config.audio.alert_sink
                    && *name != "auto_null"
                    && !name.starts_with("proaudio_player_")
            })
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            bail!("Фізичний аудіовихід не знайдено");
        }

        // The bus state is authoritative: it identifies the sink that actually
        // receives both loopbacks. Do not guess from USB priority after hotplug.
        if let Ok(state) = fs::read_to_string(AUDIO_BUS_STATE_FILE) {
            if let Some(active) = state.lines()
                .find_map(|line| line.strip_prefix("PHYSICAL="))
                .map(str::trim)
            {
                if let Some(position) = candidates.iter().position(|name| name == active) {
                    return Ok(candidates.remove(position));
                }
            }
        }
        if let Some(configured) = Self::configured_output() {
            if let Some(position) = candidates.iter().position(|name| name == &configured) {
                return Ok(candidates.remove(position));
            }
        }
        if let Some(position) = candidates.iter().position(|name| name.starts_with("alsa_output.usb-")) {
            return Ok(candidates.remove(position));
        }
        Ok(candidates.remove(0))
    }

    fn configured_output() -> Option<String> {
        fs::read_to_string(AUDIO_OUTPUT_FILE).ok().and_then(|text| {
            text.lines().find_map(|line| line.strip_prefix("PHYSICAL_SINK="))
                .map(str::trim).filter(|value| !value.is_empty() && *value != "AUTO").map(str::to_owned)
        })
    }

    pub async fn audio_outputs(&self) -> Result<Vec<Value>> {
        let output = self.run("pactl", &["-f", "json", "list", "sinks"], true, 8).await?;
        let sinks: Value = serde_json::from_str(&output.stdout).context("Некоректна відповідь PipeWire")?;
        let selected = Self::configured_output();
        let current = self.physical_sink().await.ok();
        Ok(sinks.as_array().into_iter().flatten().filter_map(|sink| {
            let name = sink.get("name")?.as_str()?;
            if name == "auto_null" || name == self.config.audio.music_sink
                || name == self.config.audio.alert_sink || name.starts_with("proaudio_player_") {
                return None;
            }
            let properties = sink.get("properties").and_then(Value::as_object);
            let description = properties.and_then(|p| p.get("device.description"))
                .and_then(Value::as_str).filter(|v| !v.is_empty()).unwrap_or(name);
            let device_class = properties.and_then(|p| p.get("device.class"))
                .and_then(Value::as_str).unwrap_or("");
            let alsa_card = properties.and_then(|p| p.get("alsa.card"))
                .and_then(Value::as_str).and_then(|value| value.parse::<u32>().ok());
            Some(json!({
                "id": name,
                "name": description,
                "state": sink.get("state").and_then(Value::as_str).unwrap_or("UNKNOWN").to_ascii_lowercase(),
                "device_class": device_class,
                "alsa_card": alsa_card,
                "selected": selected.as_deref().or(current.as_deref()) == Some(name),
                "available": true
            }))
        }).collect())
    }

    pub async fn select_audio_output(&self, requested: &str) -> Result<Value> {
        let requested = requested.trim();
        let outputs = self.audio_outputs().await?;
        let selected = outputs.iter().find(|item| item.get("id").and_then(Value::as_str) == Some(requested))
            .ok_or_else(|| anyhow!("Вибраний аудіовихід зараз недоступний"))?;
        let path = Path::new(AUDIO_OUTPUT_FILE);
        if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
        let temporary = path.with_extension("env.tmp");
        fs::write(&temporary, format!("PHYSICAL_SINK={requested}\n"))?;
        fs::rename(&temporary, path)?;
        Ok(json!({ "selected": selected, "applying": true }))
    }

    pub async fn hardware_mixers(&self) -> Result<Vec<Value>> {
        let cards_text = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
        let card_re = Regex::new(r"(?m)^\s*(\d+)\s+\[([^]]+)\]")?;
        let control_re = Regex::new(r"Simple mixer control '([^']+)'")?;
        let percent_re = Regex::new(r"Playback[^\n]*\[(\d+)%\]")?;
        let db_re = Regex::new(r"\[(-?\d+(?:\.\d+)?)dB\]")?;
        let limits_re = Regex::new(r"Limits:\s+Playback\s+(-?\d+)\s+-\s+(-?\d+)")?;
        let mut result = Vec::new();

        for capture in card_re.captures_iter(&cards_text) {
            let card = capture.get(1).and_then(|m| m.as_str().parse::<u32>().ok()).unwrap_or(0);
            let card_name = capture.get(2).map(|m| m.as_str()).unwrap_or("");
            let card_text = card.to_string();
            let controls = self
                .run("amixer", &["-c", &card_text, "scontrols"], false, 8)
                .await?;
            if controls.code != 0 {
                continue;
            }
            for control_capture in control_re.captures_iter(&controls.stdout) {
                let control = control_capture.get(1).map(|m| m.as_str()).unwrap_or("");
                let details = self
                    .run("amixer", &["-c", &card_text, "sget", control], false, 8)
                    .await?;
                if details.code != 0 || !details.stdout.contains("Playback") {
                    continue;
                }
                let values = percent_re
                    .captures_iter(&details.stdout)
                    .filter_map(|c| c.get(1))
                    .filter_map(|m| m.as_str().parse::<f64>().ok())
                    .collect::<Vec<_>>();
                if values.is_empty() {
                    continue;
                }
                let db_values = db_re
                    .captures_iter(&details.stdout)
                    .filter_map(|c| c.get(1))
                    .filter_map(|m| m.as_str().parse::<f64>().ok())
                    .collect::<Vec<_>>();
                let limits = limits_re.captures(&details.stdout);
                result.push(json!({
                    "card": card,
                    "card_name": card_name,
                    "control": control,
                    "volume": (values.iter().sum::<f64>() / values.len() as f64).round(),
                    "muted": details.stdout.contains("[off]"),
                    "db": if db_values.is_empty() { Value::Null } else { json!(((db_values.iter().sum::<f64>() / db_values.len() as f64) * 100.0).round() / 100.0) },
                    "raw_min": limits.as_ref().and_then(|c| c.get(1)).and_then(|m| m.as_str().parse::<i64>().ok()),
                    "raw_max": limits.as_ref().and_then(|c| c.get(2)).and_then(|m| m.as_str().parse::<i64>().ok()),
                }));
            }
        }
        Ok(result)
    }

    pub async fn primary_hardware_mixer(&self) -> Result<Value> {
        let mixers = self.hardware_mixers().await?;
        let selected_card = self.audio_outputs().await.ok().and_then(|outputs| {
            outputs.into_iter()
                .find(|item| item.get("selected").and_then(Value::as_bool) == Some(true))
                .and_then(|item| item.get("alsa_card").and_then(Value::as_u64))
        });
        let candidates = mixers.iter().filter(|item| {
            selected_card.is_none() || item.get("card").and_then(Value::as_u64) == selected_card
        }).collect::<Vec<_>>();
        ["Master", "Headphone", "PCM", "Speaker"].iter()
            .find_map(|preferred| candidates.iter().find(|item| item.get("control").and_then(Value::as_str) == Some(*preferred)).copied())
            .or_else(|| candidates.first().copied())
            .cloned()
            .ok_or_else(|| {
                if let Some(card) = selected_card {
                    anyhow!("Вибраний ALSA-пристрій card {card} не має апаратного регулятора гучності")
                } else {
                    anyhow!("Апаратний ALSA-регулятор не знайдено")
                }
            })
    }

    pub async fn mixer_state(&self) -> Result<Value> {
        let music = self.sink_state(&self.config.audio.music_sink).await?;
        let alert = self.sink_state(&self.config.audio.alert_sink).await?;
        let master = self.primary_hardware_mixer().await?;
        Ok(json!({ "music": music, "alert": alert, "master": master }))
    }

    pub async fn set_mixer_db(&self, target: &str, db: f64, muted: Option<bool>) -> Result<Value> {
        if !(-60.0..=0.0).contains(&db) { bail!("Рівень має бути в межах -60..0 dB"); }
        if target == "master" {
            let mixer = self.primary_hardware_mixer().await?;
            let card = mixer.get("card").and_then(Value::as_u64).ok_or_else(|| anyhow!("Некоректна ALSA-карта"))?.to_string();
            let control = mixer.get("control").and_then(Value::as_str).ok_or_else(|| anyhow!("Некоректний ALSA-регулятор"))?;
            if muted == Some(true) || db <= -60.0 {
                self.run("amixer", &["-c", &card, "sset", control, "mute"], true, 8).await?;
            } else {
                let value = format!("{db:.1}dB");
                self.run("amixer", &["-c", &card, "sset", control, "--", &value, "unmute"], true, 8).await?;
            }
        } else {
            let sink = match target {
                "music" => self.config.audio.music_sink.as_str(),
                "alert" => self.config.audio.alert_sink.as_str(),
                _ => bail!("Невідомий канал мікшера"),
            };
            let is_muted = muted == Some(true) || db <= -60.0;
            let percent = if is_muted { 0.0 } else { 100.0 * 10_f64.powf(db / 20.0) };
            self.audio.set_sink_percent(sink, percent).await?;
            self.run("pactl", &["set-sink-mute", sink, if is_muted { "1" } else { "0" }], true, 8).await?;
        }
        self.mixer_state().await
    }

    pub fn audio_settings(&self) -> Result<Value> {
        let (audio, minute) = effective_audio(&self.config.audio, &self.config.minute_silence)?;
        Ok(json!({
            "duck_db": audio.duck_db,
            "duck_fade_seconds": audio.duck_fade_seconds,
            "restore_fade_seconds": audio.restore_fade_seconds,
            "alert_volume_percent": audio.alert_volume_percent,
            "default_restore_volume_percent": audio.default_restore_volume_percent,
            "minute_silence_volume_percent": minute.volume_percent,
            "alert_repeat_interval_minutes": audio.alert_repeat_interval_minutes,
            "duck_only_during_announcement": audio.duck_only_during_announcement,
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

    async fn busctl_json(&self, arguments: &[&str]) -> Result<Option<Value>> {
        let mut args = vec!["--system", "--json=short"];
        args.extend_from_slice(arguments);
        let output = self.run("busctl", &args, false, 3).await?;
        if output.code != 0 || output.stdout.is_empty() {
            return Ok(None);
        }
        let value: Value = serde_json::from_str(&output.stdout)?;
        Ok(Some(collapse_single(unwrap_dbus(value))))
    }

    async fn mpris_names(&self) -> Result<Vec<String>> {
        let value = self
            .busctl_json(&[
                "call",
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "ListNames",
            ])
            .await?;
        Ok(value
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect())
    }

    async fn mpris_service(&self, source: &str, names: &[String]) -> Option<String> {
        let prefix = match source {
            "Spotify Connect" => "org.mpris.MediaPlayer2.spotifyd",
            "AirPlay" => "org.mpris.MediaPlayer2.ShairportSync",
            _ => return None,
        };
        names
            .iter()
            .find(|name| name.as_str() == prefix)
            .cloned()
            .or_else(|| names.iter().find(|name| name.starts_with(&format!("{prefix}."))).cloned())
    }

    async fn mpris_player(&self, source: &str, service: &str) -> Result<Option<Value>> {
        let Some(properties) = self
            .busctl_json(&[
                "call",
                service,
                MPRIS_PATH,
                "org.freedesktop.DBus.Properties",
                "GetAll",
                "s",
                MPRIS_PLAYER_INTERFACE,
            ])
            .await?
        else {
            return Ok(None);
        };
        let Some(properties) = properties.as_object() else {
            return Ok(None);
        };
        let metadata = properties
            .get("Metadata")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let artist = match metadata.get("xesam:artist") {
            Some(Value::Array(values)) => values
                .iter()
                .filter_map(Value::as_str)
                .filter(|v| !v.is_empty())
                .collect::<Vec<_>>()
                .join(", "),
            Some(Value::String(value)) => value.clone(),
            _ => String::new(),
        };
        let mut state = properties
            .get("PlaybackStatus")
            .and_then(Value::as_str)
            .unwrap_or("stopped")
            .to_ascii_lowercase();
        if !matches!(state.as_str(), "playing" | "paused" | "stopped") {
            state = "stopped".into();
        }
        let position = properties.get("Position").and_then(microseconds_to_seconds);
        let duration = metadata.get("mpris:length").and_then(microseconds_to_seconds);
        let progress = match (position, duration) {
            (Some(p), Some(d)) if d > 0.0 => (p * 100.0 / d).clamp(0.0, 100.0).round() as u32,
            _ => 0,
        };
        let can_control = properties.get("CanControl").and_then(Value::as_bool).unwrap_or(false);
        let control = |name: &str| can_control && properties.get(name).and_then(Value::as_bool).unwrap_or(false);
        let backend = if source == "Spotify Connect" { "spotify-mpris" } else { "airplay-mpris" };
        let art_url = metadata
            .get("mpris:artUrl")
            .and_then(Value::as_str)
            .filter(|value| Url::parse(value).ok().is_some_and(|u| matches!(u.scheme(), "http" | "https")));
        Ok(Some(json!({
            "source": source,
            "backend": backend,
            "state": state,
            "title": metadata.get("xesam:title").and_then(Value::as_str).unwrap_or(""),
            "artist": artist,
            "album": metadata.get("xesam:album").and_then(Value::as_str).unwrap_or(""),
            "art_url": art_url,
            "position_seconds": position,
            "duration_seconds": duration,
            "elapsed": format_seconds(position),
            "duration": format_seconds(duration),
            "progress": progress,
            "controls": {
                "play": control("CanPlay"),
                "pause": control("CanPause"),
                "stop": can_control,
                "next": control("CanGoNext"),
                "prev": control("CanGoPrevious"),
            },
            "_service": service,
        })))
    }

    fn local_player(&self, mpd: &Value) -> Value {
        let state = mpd.get("state").and_then(Value::as_str).unwrap_or("stopped");
        let queue_length = mpd.get("queue_length").and_then(Value::as_u64).unwrap_or(0);
        let has_track = mpd.get("file").and_then(Value::as_str).is_some_and(|v| !v.is_empty());
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
            "position_seconds": clock_to_seconds(elapsed),
            "duration_seconds": clock_to_seconds(duration),
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
        if winner.as_deref() == Some("mpd") {
            return Ok(self.local_player(mpd));
        }
        let external = winner.as_deref().and_then(|key| {
            sources.iter().find(|source| source.get("key").and_then(Value::as_str) == Some(key))
        });
        if let Some(external) = external {
            let source_type = external.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(source_type, "Spotify Connect" | "AirPlay") {
                let names = self.mpris_names().await?;
                if let Some(service) = self.mpris_service(source_type, &names).await {
                    if let Some(player) = self.mpris_player(source_type, &service).await? {
                        return Ok(player);
                    }
                }
            }
            return Ok(self.external_fallback(external));
        }

        let names = self.mpris_names().await.unwrap_or_default();
        for source_type in ["Spotify Connect", "AirPlay"] {
            if let Some(service) = self.mpris_service(source_type, &names).await {
                if let Some(player) = self.mpris_player(source_type, &service).await? {
                    if matches!(player.get("state").and_then(Value::as_str), Some("playing" | "paused")) {
                        return Ok(player);
                    }
                }
            }
        }

        if mpd.get("available").and_then(Value::as_bool) == Some(true)
            && (matches!(mpd.get("state").and_then(Value::as_str), Some("playing" | "paused"))
                || mpd.get("queue_length").and_then(Value::as_u64).unwrap_or(0) > 0
                || mpd.get("file").and_then(Value::as_str).is_some_and(|v| !v.is_empty()))
        {
            return Ok(self.local_player(mpd));
        }
        Ok(self.idle_player())
    }

    pub async fn status(&self) -> Result<Value> {
        let snapshot = self.audio.snapshot().await?;
        let music_volume = snapshot.volumes_percent.iter().sum::<f64>() / snapshot.volumes_percent.len().max(1) as f64;
        let hardware = self.primary_hardware_mixer().await.ok();
        let volume = music_volume;
        let muted = snapshot.muted;
        let physical = match self.physical_sink().await {
            Ok(sink) => self.sink_state(&sink).await.ok(),
            Err(_) => None,
        };
        let alert_bus = self.sink_state(&self.config.audio.alert_sink).await.ok();
        let mpd = self.mpd_status().await.unwrap_or_else(|_| json!({ "available": false, "state": "unavailable" }));
        let sources = self.active_sources().await.unwrap_or_default();
        let mut player = self.resolve_active_player(&sources, &mpd).await.unwrap_or_else(|_| self.idle_player());
        if let Some(object) = player.as_object_mut() {
            object.remove("_service");
        }
        Ok(json!({
            "name": "ProAudio Player",
            "volume": (volume * 10.0).round() / 10.0,
            "muted": muted,
            "priority": self.priority_state().await,
            "mpd": mpd,
            "sources": sources,
            "audio_levels": {
                "music_bus": (music_volume * 10.0).round() / 10.0,
                "physical": physical,
                "hardware": hardware,
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
        let sources = self.active_sources().await.unwrap_or_default();
        let mpd = self.mpd_status().await?;
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
                player.get("source").and_then(Value::as_str).unwrap_or("Активне джерело")
            );
        }
        let backend = player.get("backend").and_then(Value::as_str).unwrap_or("none");
        if backend == "mpd" {
            self.run("mpc", &[action], true, 8).await?;
        } else if matches!(backend, "spotify-mpris" | "airplay-mpris") {
            let service = player
                .get("_service")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("MPRIS-сервіс активного джерела не знайдено"))?;
            let method = match action {
                "play" => "Play",
                "pause" => "Pause",
                "stop" => "Stop",
                "next" => "Next",
                "prev" => "Previous",
                _ => unreachable!(),
            };
            let output = self
                .run(
                    "busctl",
                    &["--system", "call", service, MPRIS_PATH, MPRIS_PLAYER_INTERFACE, method],
                    false,
                    4,
                )
                .await?;
            if output.code != 0 {
                bail!("MPRIS-команда {method} не виконана: {}", output.stderr);
            }
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
            .take(self.config.web.max_library_items)
            .map(str::to_owned)
            .collect())
    }

    pub async fn playlists(&self) -> Result<Vec<String>> {
        let output = self.run("mpc", &["lsplaylists"], true, 8).await?;
        Ok(output.stdout.lines().filter(|line| !line.is_empty()).map(str::to_owned).collect())
    }

    pub async fn queue(&self) -> Result<Vec<Value>> {
        let output = self
            .run(
                "mpc",
                &["--format", "%position%\t%file%\t%title%\t%artist%\t%album%", "playlist"],
                true,
                8,
            )
            .await?;
        let mut result = Vec::new();
        for line in output.stdout.lines() {
            let mut fields = line.split('\t').map(str::to_owned).collect::<Vec<_>>();
            fields.resize(5, String::new());
            let Ok(position) = fields[0].parse::<u32>() else { continue; };
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
struct VolumeBody { percent: f64 }
#[derive(Deserialize)]
struct MuteBody { muted: bool }
#[derive(Deserialize)]
struct AudioLevelBody { target: String, percent: f64 }
#[derive(Deserialize)]
struct MixerBody { target: String, db: f64, muted: Option<bool> }
#[derive(Deserialize)]
struct HardwareBody { card: u32, control: String, percent: u32 }
#[derive(Deserialize)]
struct AudioOutputBody { id: String }
#[derive(Deserialize, Default)]
struct AudioSettingsBody {
    duck_db: Option<f64>,
    alert_volume_percent: Option<f64>,
    minute_silence_volume_percent: Option<f64>,
    default_restore_volume_percent: Option<f64>,
    duck_fade_seconds: Option<f64>,
    restore_fade_seconds: Option<f64>,
    alert_repeat_interval_minutes: Option<u64>,
    duck_only_during_announcement: Option<bool>,
}
#[derive(Deserialize)]
struct PlayerBody { action: String }
#[derive(Deserialize)]
struct PathBody { path: String }
#[derive(Deserialize)]
struct PlaylistBody { name: String }
#[derive(Deserialize)]
struct StreamBody { url: String }
#[derive(Deserialize)]
struct QueueBody { position: u32 }
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
        || path.components().any(|component| component.as_os_str() == "..")
        || (playlist && value.contains('/'))
    {
        bail!("Некоректний шлях");
    }
    Ok(value.to_owned())
}

fn apply_provider_body(base: ProviderConfig, body: &ProviderBody) -> Result<ProviderConfig> {
    let mut config = base;
    if let Some(v) = body.endpoint.as_deref() { config.endpoint = v.trim().to_owned(); }
    if let Some(v) = body.location_uid { config.location_uid = v; }
    if let Some(v) = body.location_type.as_deref() { config.location_type = v.trim().to_ascii_lowercase(); }
    if let Some(v) = body.poll_interval_seconds { config.poll_interval_seconds = v; }
    if let Some(v) = body.request_timeout_seconds { config.request_timeout_seconds = v; }
    if let Some(v) = body.rate_limit_backoff_seconds { config.rate_limit_backoff_seconds = v; }
    if let Some(v) = body.clear_confirmations { config.clear_confirmations = v; }
    validate_provider(&config)?;
    Ok(config)
}

async fn index() -> Html<&'static str> { Html(INDEX_HTML) }
async fn css() -> Response { content_response("text/css; charset=utf-8", APP_CSS) }
async fn js() -> Response { content_response("application/javascript; charset=utf-8", APP_JS) }

async fn status(State(controller): State<WebController>) -> ApiResult {
    controller.status().await.map(Json).map_err(map_internal)
}

async fn set_volume(State(controller): State<WebController>, Json(body): Json<VolumeBody>) -> ApiResult {
    controller.ensure_controls_available().await.map_err(map_internal)?;
    if !(0.0..=100.0).contains(&body.percent) {
        return Err(api_error(StatusCode::BAD_REQUEST, "Гучність має бути 0..100"));
    }
    controller.audio.set_music_volume(body.percent).await.map_err(map_internal)?;
    controller.audio.set_music_mute(body.percent <= 0.0).await.map_err(map_internal)?;
    Ok(Json(json!({ "volume": body.percent, "muted": body.percent == 0.0 })))
}

async fn set_mute(State(controller): State<WebController>, Json(body): Json<MuteBody>) -> ApiResult {
    controller.ensure_controls_available().await.map_err(map_internal)?;
    controller.audio.set_music_mute(body.muted).await.map_err(map_internal)?;
    Ok(Json(json!({ "muted": body.muted })))
}

async fn set_audio_level(State(controller): State<WebController>, Json(body): Json<AudioLevelBody>) -> ApiResult {
    if !(0.0..=100.0).contains(&body.percent) {
        return Err(api_error(StatusCode::BAD_REQUEST, "Гучність має бути 0..100"));
    }
    let sink = match body.target.as_str() {
        "master" => controller.physical_sink().await.map_err(map_internal)?,
        "music" => controller.config.audio.music_sink.clone(),
        "alert" => controller.config.audio.alert_sink.clone(),
        _ => return Err(api_error(StatusCode::BAD_REQUEST, "Невідомий аудіорівень")),
    };
    controller.audio.set_sink_percent(&sink, body.percent).await.map_err(map_internal)?;
    controller.sink_state(&sink).await.map(Json).map_err(map_internal)
}

async fn get_mixer(State(controller): State<WebController>) -> ApiResult {
    controller.mixer_state().await.map(Json).map_err(map_internal)
}

async fn set_mixer(State(controller): State<WebController>, Json(body): Json<MixerBody>) -> ApiResult {
    controller.ensure_controls_available().await.map_err(map_internal)?;
    controller.set_mixer_db(&body.target, body.db, body.muted).await.map(Json).map_err(map_internal)
}

async fn audio_outputs(State(controller): State<WebController>) -> ApiResult {
    controller.audio_outputs().await.map(|items| Json(json!({ "items": items }))).map_err(map_internal)
}

async fn select_audio_output(State(controller): State<WebController>, Json(body): Json<AudioOutputBody>) -> ApiResult {
    controller.ensure_controls_available().await.map_err(map_internal)?;
    controller.select_audio_output(&body.id).await.map(Json).map_err(map_internal)
}

async fn hardware(State(controller): State<WebController>) -> ApiResult {
    controller.hardware_mixers().await.map(|items| Json(json!({ "items": items }))).map_err(map_internal)
}

async fn set_hardware(State(controller): State<WebController>, Json(body): Json<HardwareBody>) -> ApiResult {
    if body.percent > 100 {
        return Err(api_error(StatusCode::BAD_REQUEST, "Некоректний ALSA-регулятор"));
    }
    let available = controller.hardware_mixers().await.map_err(map_internal)?;
    let exists = available.iter().any(|item| {
        item.get("card").and_then(Value::as_u64) == Some(body.card as u64)
            && item.get("control").and_then(Value::as_str) == Some(body.control.as_str())
    });
    if !exists {
        return Err(api_error(StatusCode::BAD_REQUEST, "ALSA-регулятор не знайдено"));
    }
    let card = body.card.to_string();
    let percent = format!("{}%", body.percent);
    controller.run("amixer", &["-c", &card, "sset", &body.control, &percent, "unmute"], true, 8).await.map_err(map_internal)?;
    Ok(Json(json!({ "card": body.card, "control": body.control, "volume": body.percent })))
}

async fn get_audio_settings(State(controller): State<WebController>) -> ApiResult {
    controller.audio_settings().map(Json).map_err(map_internal)
}

async fn put_audio_settings(State(controller): State<WebController>, Json(body): Json<AudioSettingsBody>) -> ApiResult {
    let (mut audio, mut minute) = effective_audio(&controller.config.audio, &controller.config.minute_silence).map_err(map_internal)?;
    if let Some(v) = body.duck_db { audio.duck_db = v; }
    if let Some(v) = body.alert_volume_percent { audio.alert_volume_percent = v; }
    if let Some(v) = body.minute_silence_volume_percent { minute.volume_percent = v; }
    if let Some(v) = body.default_restore_volume_percent { audio.default_restore_volume_percent = v; }
    if let Some(v) = body.duck_fade_seconds { audio.duck_fade_seconds = v; }
    if let Some(v) = body.restore_fade_seconds { audio.restore_fade_seconds = v; }
    if let Some(v) = body.alert_repeat_interval_minutes { audio.alert_repeat_interval_minutes = v; }
    if let Some(v) = body.duck_only_during_announcement { audio.duck_only_during_announcement = v; }
    validate_audio(&audio, &minute).map_err(map_internal)?;
    save_audio_settings(&audio, &minute).map_err(map_internal)?;
    controller.audio_settings().map(Json).map_err(map_internal)
}

async fn player(State(controller): State<WebController>, Json(body): Json<PlayerBody>) -> ApiResult {
    controller.control_active_player(&body.action).await.map(Json).map_err(map_internal)
}

async fn library(State(controller): State<WebController>) -> ApiResult {
    controller.library().await.map(|items| Json(json!({ "items": items }))).map_err(map_internal)
}

async fn refresh_library(State(controller): State<WebController>) -> ApiResult {
    controller.ensure_controls_available().await.map_err(map_internal)?;
    controller.run("mpc", &["update"], true, 60).await.map_err(map_internal)?;
    Ok(Json(json!({ "updating": true })))
}

async fn play_file(State(controller): State<WebController>, Json(body): Json<PathBody>) -> ApiResult {
    controller.ensure_controls_available().await.map_err(map_internal)?;
    let path = safe_mpd_path(&body.path, false).map_err(map_internal)?;
    if !controller.library().await.map_err(map_internal)?.contains(&path) {
        return Err(api_error(StatusCode::NOT_FOUND, "Файл відсутній у бібліотеці"));
    }
    controller.run("mpc", &["clear"], true, 8).await.map_err(map_internal)?;
    controller.run("mpc", &["add", &path], true, 8).await.map_err(map_internal)?;
    controller.run("mpc", &["play"], true, 8).await.map_err(map_internal)?;
    Ok(Json(json!({ "playing": path })))
}

fn validate_stream_url(value: &str) -> Result<String> {
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
    if matches!(host.to_ascii_lowercase().as_str(), "localhost" | "localhost.localdomain")
        || host.to_ascii_lowercase().ends_with(".local")
    {
        bail!("Локальні адреси потоків заборонені");
    }
    if let Ok(address) = host.parse::<IpAddr>() {
        let unsafe_address = match address {
            IpAddr::V4(value) => {
                value.is_private()
                    || value.is_loopback()
                    || value.is_link_local()
                    || value.is_multicast()
                    || value == Ipv4Addr::UNSPECIFIED
                    || value.octets()[0] == 0
            }
            IpAddr::V6(value) => {
                value.is_loopback()
                    || value.is_unspecified()
                    || value.is_multicast()
                    || value.is_unique_local()
                    || value.is_unicast_link_local()
                    || value == Ipv6Addr::UNSPECIFIED
            }
        };
        if unsafe_address {
            bail!("Локальні та службові IP-адреси потоків заборонені");
        }
    }
    Ok(parsed.to_string())
}

async fn play_stream(
    State(controller): State<WebController>,
    Json(body): Json<StreamBody>,
) -> ApiResult {
    controller.ensure_controls_available().await.map_err(map_internal)?;
    let url = validate_stream_url(&body.url)
        .map_err(|error| api_error(StatusCode::BAD_REQUEST, error.to_string()))?;
    controller.run("mpc", &["clear"], true, 8).await.map_err(map_internal)?;
    controller.run("mpc", &["add", &url], true, 15).await.map_err(map_internal)?;
    controller.run("mpc", &["play"], true, 8).await.map_err(map_internal)?;
    Ok(Json(json!({ "playing": url, "source": "network_stream" })))
}

async fn playlists(State(controller): State<WebController>) -> ApiResult {
    controller.playlists().await.map(|items| Json(json!({ "items": items }))).map_err(map_internal)
}

async fn load_playlist(State(controller): State<WebController>, Json(body): Json<PlaylistBody>) -> ApiResult {
    controller.ensure_controls_available().await.map_err(map_internal)?;
    let name = safe_mpd_path(&body.name, true).map_err(map_internal)?;
    if !controller.playlists().await.map_err(map_internal)?.contains(&name) {
        return Err(api_error(StatusCode::NOT_FOUND, "Плейліст не знайдено"));
    }
    controller.run("mpc", &["clear"], true, 8).await.map_err(map_internal)?;
    controller.run("mpc", &["load", &name], true, 8).await.map_err(map_internal)?;
    controller.run("mpc", &["play"], true, 8).await.map_err(map_internal)?;
    Ok(Json(json!({ "playing_playlist": name })))
}

async fn queue(State(controller): State<WebController>) -> ApiResult {
    controller.queue().await.map(|items| Json(json!({ "items": items }))).map_err(map_internal)
}

async fn play_queue(State(controller): State<WebController>, Json(body): Json<QueueBody>) -> ApiResult {
    controller.ensure_controls_available().await.map_err(map_internal)?;
    if body.position == 0 || !controller.queue().await.map_err(map_internal)?.iter().any(|item| item.get("position").and_then(Value::as_u64) == Some(body.position as u64)) {
        return Err(api_error(StatusCode::NOT_FOUND, "Позицію у черзі не знайдено"));
    }
    let position = body.position.to_string();
    controller.run("mpc", &["play", &position], true, 8).await.map_err(map_internal)?;
    Ok(Json(json!({ "playing_position": body.position })))
}

async fn remove_queue(State(controller): State<WebController>, Json(body): Json<QueueBody>) -> ApiResult {
    controller.ensure_controls_available().await.map_err(map_internal)?;
    if body.position == 0 || !controller.queue().await.map_err(map_internal)?.iter().any(|item| item.get("position").and_then(Value::as_u64) == Some(body.position as u64)) {
        return Err(api_error(StatusCode::NOT_FOUND, "Позицію у черзі не знайдено"));
    }
    let position = body.position.to_string();
    controller.run("mpc", &["del", &position], true, 8).await.map_err(map_internal)?;
    Ok(Json(json!({ "removed_position": body.position })))
}

async fn clear_queue(State(controller): State<WebController>) -> ApiResult {
    controller.ensure_controls_available().await.map_err(map_internal)?;
    controller.run("mpc", &["clear"], true, 8).await.map_err(map_internal)?;
    Ok(Json(json!({ "cleared": true })))
}

async fn get_alert_settings(State(controller): State<WebController>) -> ApiResult {
    controller.alert_settings().map(Json).map_err(map_internal)
}

async fn put_alert_settings(State(controller): State<WebController>, Json(body): Json<ProviderBody>) -> ApiResult {
    let current = effective_provider(&controller.config.provider).map_err(map_internal)?;
    let candidate = apply_provider_body(current, &body).map_err(map_internal)?;
    if let Some(token) = body.token.as_deref() {
        if !token.trim().is_empty() {
            save_provider_token(&candidate, token).map_err(map_internal)?;
        }
    }
    save_provider_settings(&candidate).map_err(map_internal)?;
    controller.alert_settings().map(Json).map_err(map_internal)
}

async fn test_alert_settings(State(controller): State<WebController>, Json(body): Json<ProviderBody>) -> ApiResult {
    let current = effective_provider(&controller.config.provider).map_err(map_internal)?;
    let candidate = apply_provider_body(current, &body).map_err(map_internal)?;
    let token = match body.token.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        Some(value) => value.to_owned(),
        None => candidate.resolve_token().map_err(map_internal)?,
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
        return Err(api_error(StatusCode::SERVICE_UNAVAILABLE, format!("API повернув HTTP {}", response.status())));
    }
    let payload = response.json::<String>().await.map_err(|e| map_internal(e.into()))?;
    if !matches!(payload.as_str(), "A" | "P" | "N") {
        return Err(api_error(StatusCode::SERVICE_UNAVAILABLE, "API повернув невідомий статус"));
    }
    let active = payload == "A" || (payload == "P" && candidate.partial_status_is_active());
    Ok(Json(json!({ "ok": true, "active": active, "state": if active { "active" } else { "clear" }, "location_uid": candidate.location_uid })))
}

pub fn router(controller: WebController) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/static/app.css", get(css))
        .route("/static/app.js", get(js))
        .route("/api/status", get(status))
        .route("/api/volume", post(set_volume))
        .route("/api/mute", post(set_mute))
        .route("/api/audio/level", post(set_audio_level))
        .route("/api/audio/mixer", get(get_mixer).post(set_mixer))
        .route("/api/audio/outputs", get(audio_outputs).post(select_audio_output))
        .route("/api/audio/hardware", get(hardware).post(set_hardware))
        .route("/api/settings/audio", get(get_audio_settings).put(put_audio_settings))
        .route("/api/player", post(player))
        .route("/api/library", get(library))
        .route("/api/library/update", post(refresh_library))
        .route("/api/library/play", post(play_file))
        .route("/api/streams/play", post(play_stream))
        .route("/api/playlists", get(playlists))
        .route("/api/playlists/load", post(load_playlist))
        .route("/api/queue", get(queue))
        .route("/api/queue/play", post(play_queue))
        .route("/api/queue/remove", post(remove_queue))
        .route("/api/queue/clear", post(clear_queue))
        .route("/api/settings/alerts", get(get_alert_settings).put(put_alert_settings))
        .route("/api/settings/alerts/test", post(test_alert_settings))
        .merge(fourstream::router())
        .with_state(controller)
}

pub async fn serve(controller: WebController) -> Result<()> {
    let host = controller.config.web.host.clone();
    let port = controller.config.web.port;
    let address = format!("{host}:{port}");
    let listener = TcpListener::bind(&address).await?;
    info!(%address, "Native Web UI started");
    let ssdp_controller = controller.clone();
    tokio::spawn(async move {
        if let Err(err) = fourstream::run_ssdp(ssdp_controller, port).await {
            debug!("4STREAM SSDP discovery unavailable: {err:#}");
        }
    });
    axum::serve(listener, router(controller)).await?;
    Ok(())
}
