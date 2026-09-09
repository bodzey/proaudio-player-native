use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::time::{sleep, timeout};
use tracing::{debug, error, info, warn};

use crate::command;
use crate::config::AppConfig;

pub type SharedSourceState = Arc<RwLock<Option<String>>>;

#[derive(Clone)]
pub struct SourceArbiter {
    config: Arc<AppConfig>,
    active_streams: HashSet<i64>,
    winner: Option<String>,
    shared_winner: SharedSourceState,
}

impl SourceArbiter {
    pub fn new(config: Arc<AppConfig>, shared_winner: SharedSourceState) -> Self {
        Self {
            config,
            active_streams: HashSet::new(),
            winner: None,
            shared_winner,
        }
    }

    fn source_key(stream: &Value) -> String {
        let props = stream.get("properties").and_then(Value::as_object);
        let get = |key: &str| {
            props
                .and_then(|p| p.get(key))
                .and_then(Value::as_str)
                .unwrap_or("")
        };
        let identity = format!(
            "{} {} {}",
            get("application.name"),
            get("application.process.binary"),
            get("media.role")
        )
        .to_ascii_lowercase();

        if identity.contains("spotify") {
            return "spotify".into();
        }
        if identity.contains("shairport") || identity.contains("airplay") {
            return "airplay".into();
        }
        if identity.contains("gmediarender")
            || identity.contains("gmrender")
            || identity.contains("gstreamer")
        {
            return "dlna".into();
        }
        if identity.contains("mpd") {
            return "mpd".into();
        }

        let index = stream.get("index").and_then(Value::as_i64).unwrap_or(-1);
        format!(
            "other:{}",
            [
                get("application.process.binary"),
                get("application.name"),
                get("application.process.id"),
            ]
            .into_iter()
            .find(|v| !v.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| index.to_string())
        )
    }

    fn flag(stream: &Value, key: &str) -> bool {
        match stream.get(key) {
            Some(Value::Bool(v)) => *v,
            Some(Value::String(v)) => {
                matches!(v.to_ascii_lowercase().as_str(), "yes" | "true" | "1")
            }
            Some(Value::Number(v)) => v.as_i64().unwrap_or_default() != 0,
            _ => false,
        }
    }

    fn volume_is_unity(stream: &Value) -> bool {
        let Some(channels) = stream.get("volume").and_then(Value::as_object) else {
            return false;
        };
        !channels.is_empty()
            && channels.values().all(|channel| {
                channel.get("value").and_then(Value::as_u64) == Some(65_536)
                    || channel.get("value_percent").and_then(Value::as_str) == Some("100%")
            })
    }

    async fn music_sink_index(&self) -> Result<Option<i64>> {
        let out = command::run("pactl", &["-f", "json", "list", "sinks"], false, 8).await?;
        if out.code != 0 {
            return Ok(None);
        }
        let sinks: Value = serde_json::from_str(&out.stdout).unwrap_or(Value::Array(Vec::new()));
        let Some(items) = sinks.as_array() else {
            return Ok(None);
        };
        Ok(items
            .iter()
            .find(|sink| {
                sink.get("name").and_then(Value::as_str)
                    == Some(self.config.audio.music_sink.as_str())
            })
            .and_then(|sink| sink.get("index"))
            .and_then(Value::as_i64))
    }

    async fn streams(&self) -> Result<Vec<Value>> {
        let Some(index) = self.music_sink_index().await? else {
            return Ok(Vec::new());
        };
        let out = command::run(
            "pactl",
            &["-f", "json", "list", "sink-inputs"],
            false,
            8,
        )
        .await?;
        if out.code != 0 {
            return Ok(Vec::new());
        }
        let value: Value = serde_json::from_str(&out.stdout).unwrap_or(Value::Array(Vec::new()));
        Ok(value
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|stream| stream.get("sink").and_then(Value::as_i64) == Some(index))
            .collect())
    }

    fn choose_winner(&self, grouped: &HashMap<String, Vec<Value>>) -> Option<String> {
        let newcomers = grouped.iter().filter(|(_, items)| {
            items.iter().any(|item| {
                item.get("index")
                    .and_then(Value::as_i64)
                    .is_some_and(|i| !self.active_streams.contains(&i))
            })
        });
        let newest = newcomers.max_by_key(|(_, items)| {
            items
                .iter()
                .filter_map(|v| v.get("index").and_then(Value::as_i64))
                .max()
                .unwrap_or(-1)
        });
        if let Some((key, _)) = newest {
            return Some(key.clone());
        }
        if self
            .winner
            .as_ref()
            .is_some_and(|winner| grouped.contains_key(winner))
        {
            return self.winner.clone();
        }
        grouped
            .iter()
            .max_by_key(|(_, items)| {
                items
                    .iter()
                    .filter_map(|v| v.get("index").and_then(Value::as_i64))
                    .max()
                    .unwrap_or(-1)
            })
            .map(|(key, _)| key.clone())
    }

    async fn stop_source(&self, key: &str, streams: &[Value]) -> Result<()> {
        if key == "mpd" {
            let out = command::run("mpc", &["stop"], false, 8).await?;
            if out.code != 0 {
                debug!("Не вдалося зупинити MPD: {}", out.stderr);
            }
            return Ok(());
        }

        let prefix = match key {
            "spotify" => Some("org.mpris.MediaPlayer2.spotifyd"),
            "airplay" => Some("org.mpris.MediaPlayer2.ShairportSync"),
            _ => None,
        };
        if let Some(prefix) = prefix {
            let list = command::run(
                "busctl",
                &["--system", "--no-pager", "--no-legend", "list"],
                false,
                8,
            )
            .await?;
            let service = list
                .stdout
                .lines()
                .filter_map(|line| line.split_whitespace().next())
                .find(|name| name.starts_with(prefix));
            if let Some(service) = service {
                let out = command::run(
                    "busctl",
                    &[
                        "--system",
                        "call",
                        service,
                        "/org/mpris/MediaPlayer2",
                        "org.mpris.MediaPlayer2.Player",
                        "Stop",
                    ],
                    false,
                    8,
                )
                .await?;
                if out.code != 0 {
                    debug!(source = key, "Не вдалося зупинити MPRIS: {}", out.stderr);
                }
            }
        }

        // AirPlay and DLNA receivers may keep an uncorked session after Stop.
        // Terminating their unprivileged receiver process disconnects the sender;
        // systemd immediately starts a clean receiver instance.
        if matches!(key, "airplay" | "dlna") {
            for stream in streams {
                let pid = stream.get("properties").and_then(Value::as_object)
                    .and_then(|p| p.get("application.process.id")).and_then(Value::as_str);
                if let Some(pid) = pid.filter(|value| value.chars().all(|c| c.is_ascii_digit())) {
                    let out = command::run("kill", &["-TERM", pid], false, 3).await?;
                    if out.code != 0 {
                        debug!(source = key, pid, "Не вдалося завершити receiver: {}", out.stderr);
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn reconcile(&mut self) -> Result<()> {
        let streams = self.streams().await?;
        let mut grouped: HashMap<String, Vec<Value>> = HashMap::new();
        for stream in streams {
            if Self::flag(&stream, "corked") {
                continue;
            }
            grouped
                .entry(Self::source_key(&stream))
                .or_default()
                .push(stream);
        }

        let previous = self.winner.clone();
        self.winner = self.choose_winner(&grouped);
        let changed = self.winner != previous;
        if changed {
            *self.shared_winner.write().await = self.winner.clone();
            info!(winner = ?self.winner, "Активне джерело змінено");
        }

        for (key, items) in &grouped {
            let is_winner = Some(key) == self.winner.as_ref();
            let should_mute = !is_winner;
            for item in items {
                let Some(index) = item.get("index").and_then(Value::as_i64) else {
                    continue;
                };
                let index_text = index.to_string();

                if Self::flag(item, "mute") != should_mute {
                    let out = command::run(
                        "pactl",
                        &[
                            "set-sink-input-mute",
                            &index_text,
                            if should_mute { "1" } else { "0" },
                        ],
                        false,
                        8,
                    )
                    .await?;
                    if out.code != 0 {
                        warn!(stream = index, source = key, "Не вдалося змінити mute: {}", out.stderr);
                    }
                }

                // Source receivers are transport inputs, not gain stages. Keep the
                // winning input at unity and perform user volume/ducking only on
                // the music bus. This also repairs persisted Spotify softvol=0.
                if is_winner && !Self::volume_is_unity(item) {
                    let out = command::run(
                        "pactl",
                        &["set-sink-input-volume", &index_text, "100%"],
                        false,
                        8,
                    )
                    .await?;
                    if out.code != 0 {
                        warn!(stream = index, source = key, "Не вдалося встановити unity gain: {}", out.stderr);
                    }
                }
            }
        }

        if changed && self.winner.is_some() {
            for key in grouped.keys() {
                if Some(key) != self.winner.as_ref() {
                    let _ = self.stop_source(key, &grouped[key]).await;
                }
            }
        }

        self.active_streams = grouped
            .values()
            .flatten()
            .filter_map(|v| v.get("index").and_then(Value::as_i64))
            .collect();
        Ok(())
    }

    pub async fn run_forever(mut self) -> Result<()> {
        loop {
            if let Err(err) = self.reconcile().await {
                error!("Помилка арбітра аудіоджерел: {err:#}");
            }

            let mut child = match Command::new("pactl")
                .arg("subscribe")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
            {
                Ok(v) => v,
                Err(err) => {
                    error!("Не вдалося запустити pactl subscribe: {err}");
                    sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };

            let Some(stdout) = child.stdout.take() else {
                bail!("pactl subscribe stdout unavailable");
            };
            let mut lines = BufReader::new(stdout).lines();

            loop {
                match timeout(Duration::from_secs(2), lines.next_line()).await {
                    Ok(Ok(Some(line))) => {
                        let lower = line.to_ascii_lowercase();
                        if lower.contains("sink-input") || lower.contains("sink ") {
                            sleep(Duration::from_millis(80)).await;
                            if let Err(err) = self.reconcile().await {
                                error!("Помилка reconcile: {err:#}");
                            }
                        }
                    }
                    Ok(Ok(None)) => break,
                    Ok(Err(err)) => {
                        warn!("pactl subscribe read failed: {err}");
                        break;
                    }
                    Err(_) => {
                        // Heartbeat reconciliation catches missed Pulse/PipeWire events,
                        // stalled subscriptions and externally changed stream state.
                        if let Err(err) = self.reconcile().await {
                            error!("Помилка heartbeat reconcile: {err:#}");
                        }
                    }
                }
            }

            let _ = child.kill().await;
            warn!("pactl subscribe завершився; повторний запуск");
            sleep(Duration::from_secs(2)).await;
        }
    }
}
