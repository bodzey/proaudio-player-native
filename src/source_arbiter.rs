use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::audio::AudioEngine;
use crate::audio_backend::StreamState;
use crate::command;
use crate::config::AppConfig;
use crate::dlna;

pub type SharedSourceState = Arc<RwLock<Option<String>>>;

#[derive(Clone)]
pub struct SourceArbiter {
    config: Arc<AppConfig>,
    audio: AudioEngine,
    active_streams: HashSet<u32>,
    suppressed_streams: HashSet<u32>,
    winner: Option<String>,
    shared_winner: SharedSourceState,
}

impl SourceArbiter {
    pub fn new(
        config: Arc<AppConfig>,
        audio: AudioEngine,
        shared_winner: SharedSourceState,
    ) -> Self {
        Self {
            config,
            audio,
            active_streams: HashSet::new(),
            suppressed_streams: HashSet::new(),
            winner: None,
            shared_winner,
        }
    }

    fn source_key(stream: &StreamState) -> String {
        let get = |key: &str| stream.property(key);
        let identity = format!(
            "{} {} {} {}",
            stream.name,
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
        if identity.contains("bluetooth") || identity.contains("bluez") {
            return "bluetooth".into();
        }

        let identity = [
            get("application.process.binary"),
            get("application.name"),
            get("application.process.id"),
        ]
        .into_iter()
        .find(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| stream.index.to_string());
        format!("other:{identity}")
    }

    async fn streams(&self) -> Result<Vec<StreamState>> {
        let music_index = self
            .audio
            .sink_state(&self.config.audio.music_sink)
            .await?
            .index;
        Ok(self
            .audio
            .list_sink_inputs()
            .await?
            .into_iter()
            .filter(|stream| stream.sink == music_index)
            .collect())
    }

    fn choose_winner(&self, grouped: &HashMap<String, Vec<StreamState>>) -> Option<String> {
        let eligible = |item: &StreamState| !self.suppressed_streams.contains(&item.index);

        let newcomers = grouped.iter().filter(|(_, items)| {
            items
                .iter()
                .any(|item| eligible(item) && !self.active_streams.contains(&item.index))
        });
        let newest = newcomers.max_by_key(|(_, items)| {
            items
                .iter()
                .filter(|item| eligible(item))
                .map(|item| item.index)
                .max()
                .unwrap_or_default()
        });
        if let Some((key, _)) = newest {
            return Some(key.clone());
        }

        if self.winner.as_ref().is_some_and(|winner| {
            grouped
                .get(winner)
                .is_some_and(|items| items.iter().any(eligible))
        }) {
            return self.winner.clone();
        }

        grouped
            .iter()
            .filter(|(_, items)| items.iter().any(eligible))
            .max_by_key(|(_, items)| {
                items
                    .iter()
                    .filter(|item| eligible(item))
                    .map(|item| item.index)
                    .max()
                    .unwrap_or_default()
            })
            .map(|(key, _)| key.clone())
    }

    fn audible_stream_index(
        streams: &[StreamState],
        suppressed_streams: &HashSet<u32>,
    ) -> Option<u32> {
        streams
            .iter()
            .filter(|stream| !suppressed_streams.contains(&stream.index))
            .map(|stream| stream.index)
            .max()
    }

    async fn stop_source(&self, key: &str) -> Result<()> {
        if key == "mpd" {
            let out = command::run("mpc", &["stop"], false, 8).await?;
            if out.code != 0 {
                debug!("Не вдалося зупинити MPD: {}", out.stderr);
            }
            return Ok(());
        }

        if key == "dlna" {
            dlna::client().control("stop").await?;
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
        Ok(())
    }

    pub async fn reconcile(&mut self) -> Result<()> {
        let streams = self.streams().await?;
        let mut grouped: HashMap<String, Vec<StreamState>> = HashMap::new();
        for stream in streams {
            if stream.corked {
                continue;
            }
            grouped
                .entry(Self::source_key(&stream))
                .or_default()
                .push(stream);
        }

        let present_streams = grouped
            .values()
            .flatten()
            .map(|stream| stream.index)
            .collect::<HashSet<_>>();
        self.suppressed_streams
            .retain(|index| present_streams.contains(index));

        let previous = self.winner.clone();
        self.winner = self.choose_winner(&grouped);
        let changed = self.winner != previous;
        if changed {
            *self.shared_winner.write().await = self.winner.clone();
            info!(winner = ?self.winner, "Активне джерело змінено");
        }

        let audible_stream = self
            .winner
            .as_ref()
            .and_then(|winner| grouped.get(winner))
            .and_then(|streams| Self::audible_stream_index(streams, &self.suppressed_streams));

        for (key, items) in &grouped {
            let is_winner = Some(key) == self.winner.as_ref();
            for item in items {
                // A receiver normally owns one stereo sink-input, but reconnects
                // can briefly leave duplicates behind. Exactly one programme
                // stream is allowed through the MUSIC bus so duplicate streams
                // cannot sum above unity.
                let is_audible = is_winner && Some(item.index) == audible_stream;
                let should_mute = !is_audible;
                if item.muted != should_mute {
                    if let Err(err) = self
                        .audio
                        .set_sink_input_mute(item.index, should_mute)
                        .await
                    {
                        warn!(
                            stream = item.index,
                            source = key,
                            error = %err,
                            "Не вдалося змінити mute потоку"
                        );
                    }
                }

                // Source receivers are transports, never user gain stages. Keep
                // the winning stream at unity. MUSIC bus owns user volume and ducking.
                if is_audible && item.has_volume && item.volume_writable && !item.volume_is_unity()
                {
                    if let Err(err) = self.audio.set_sink_input_percent(item.index, 100.0).await {
                        warn!(
                            stream = item.index,
                            source = key,
                            error = %err,
                            "Не вдалося встановити unity gain"
                        );
                    }
                }
            }
        }

        if changed && self.winner.is_some() {
            for (key, items) in &grouped {
                if Some(key) != self.winner.as_ref() {
                    self.suppressed_streams
                        .extend(items.iter().map(|stream| stream.index));
                    if let Err(err) = self.stop_source(key).await {
                        debug!(
                            source = key,
                            error = %err,
                            "Не вдалося зупинити неактивний transport; потік лишено приглушеним"
                        );
                    }
                }
            }
        }

        self.active_streams = present_streams;
        Ok(())
    }

    pub async fn run_forever(mut self) -> Result<()> {
        let mut changes = self.audio.subscribe_changes();

        loop {
            if let Err(err) = self.reconcile().await {
                error!("Помилка арбітра аудіоджерел: {err:#}");
            }

            tokio::select! {
                changed = changes.changed() => {
                    if changed.is_err() {
                        warn!("Підписка audio backend завершилась; повторне підключення");
                        changes = self.audio.subscribe_changes();
                        sleep(Duration::from_secs(1)).await;
                    } else {
                        // Coalesce the burst of server events produced by one topology change.
                        sleep(Duration::from_millis(80)).await;
                    }
                }
                _ = sleep(Duration::from_secs(2)) => {
                    // Heartbeat catches a backend/server restart and any missed event.
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(index: u32) -> StreamState {
        StreamState {
            index,
            sink: 1,
            name: String::new(),
            properties: HashMap::new(),
            volumes_percent: vec![100.0, 100.0],
            muted: false,
            corked: false,
            has_volume: true,
            volume_writable: true,
        }
    }

    #[test]
    fn newest_stream_is_the_only_audible_stream_for_a_source() {
        let suppressed = HashSet::new();
        assert_eq!(
            SourceArbiter::audible_stream_index(&[stream(7), stream(12), stream(9)], &suppressed),
            Some(12)
        );
        assert_eq!(SourceArbiter::audible_stream_index(&[], &suppressed), None);
    }

    #[test]
    fn suppressed_stream_is_not_selected_again() {
        let suppressed = HashSet::from([12]);
        assert_eq!(
            SourceArbiter::audible_stream_index(&[stream(7), stream(12), stream(9)], &suppressed),
            Some(9)
        );
    }

    #[test]
    fn bluetooth_bridge_is_classified_as_bluetooth() {
        let mut bluetooth = stream(5);
        bluetooth.name = "proaudio-player-bluetooth-to-music".into();
        assert_eq!(SourceArbiter::source_key(&bluetooth), "bluetooth");
    }
}
