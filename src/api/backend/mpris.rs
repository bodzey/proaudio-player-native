use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tokio::time::sleep;
use tracing::{debug, info};
use url::Url;
use zbus::fdo::{DBusProxy, PropertiesProxy};
use zbus::names::InterfaceName;
use zbus::zvariant::OwnedValue;
use zbus::{Connection, Proxy};

use super::{format_seconds, WebController, MPRIS_PATH, MPRIS_PLAYER_INTERFACE};

const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const SPOTIFY_SOURCE: &str = "Spotify Connect";
const AIRPLAY_SOURCE: &str = "AirPlay";
const SPOTIFY_PREFIX: &str = "org.mpris.MediaPlayer2.spotifyd";
const AIRPLAY_PREFIX: &str = "org.mpris.MediaPlayer2.ShairportSync";

fn source_prefix(source: &str) -> Option<&'static str> {
    match source {
        SPOTIFY_SOURCE => Some(SPOTIFY_PREFIX),
        AIRPLAY_SOURCE => Some(AIRPLAY_PREFIX),
        _ => None,
    }
}

fn value_string(values: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    values
        .get(key)
        .and_then(|value| <&str>::try_from(value).ok())
        .map(str::to_owned)
}

fn value_bool(values: &HashMap<String, OwnedValue>, key: &str) -> bool {
    values
        .get(key)
        .and_then(|value| bool::try_from(value).ok())
        .unwrap_or(false)
}

fn value_i64(values: &HashMap<String, OwnedValue>, key: &str) -> Option<i64> {
    values.get(key).and_then(|value| i64::try_from(value).ok())
}

fn metadata_map(values: &HashMap<String, OwnedValue>) -> HashMap<String, OwnedValue> {
    values
        .get("Metadata")
        .and_then(|value| HashMap::<String, OwnedValue>::try_from(value.clone()).ok())
        .unwrap_or_default()
}

fn metadata_artists(metadata: &HashMap<String, OwnedValue>) -> String {
    metadata
        .get("xesam:artist")
        .and_then(|value| Vec::<String>::try_from(value.clone()).ok())
        .map(|values| {
            values
                .into_iter()
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn microseconds(value: Option<i64>) -> Option<f64> {
    value.map(|value| value.max(0) as f64 / 1_000_000.0)
}

#[derive(Clone)]
struct CachedPlayer {
    value: Value,
    refreshed_at: Instant,
    position_seconds: Option<f64>,
    duration_seconds: Option<f64>,
}

impl CachedPlayer {
    fn from_properties(
        source: &str,
        service: &str,
        properties: &HashMap<String, OwnedValue>,
    ) -> Self {
        let metadata = metadata_map(properties);
        let mut state = value_string(properties, "PlaybackStatus")
            .unwrap_or_else(|| "stopped".into())
            .to_ascii_lowercase();
        if !matches!(state.as_str(), "playing" | "paused" | "stopped") {
            state = "stopped".into();
        }

        let position = microseconds(value_i64(properties, "Position"));
        let duration = microseconds(value_i64(&metadata, "mpris:length"));
        let progress = match (position, duration) {
            (Some(position), Some(duration)) if duration > 0.0 => {
                (position * 100.0 / duration).clamp(0.0, 100.0).round() as u32
            }
            _ => 0,
        };
        let can_control = value_bool(properties, "CanControl");
        let control = |name: &str| can_control && value_bool(properties, name);
        let backend = if source == SPOTIFY_SOURCE {
            "spotify-mpris"
        } else {
            "airplay-mpris"
        };
        let art_url = value_string(&metadata, "mpris:artUrl").filter(|value| {
            Url::parse(value)
                .ok()
                .is_some_and(|url| matches!(url.scheme(), "http" | "https"))
        });

        Self {
            value: json!({
                "source": source,
                "backend": backend,
                "state": state,
                "title": value_string(&metadata, "xesam:title").unwrap_or_default(),
                "artist": metadata_artists(&metadata),
                "album": value_string(&metadata, "xesam:album").unwrap_or_default(),
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
            }),
            refreshed_at: Instant::now(),
            position_seconds: position,
            duration_seconds: duration,
        }
    }

    fn snapshot(&self) -> Value {
        let mut value = self.value.clone();
        if value.get("state").and_then(Value::as_str) != Some("playing") {
            return value;
        }

        let Some(base_position) = self.position_seconds else {
            return value;
        };
        let mut position = base_position + self.refreshed_at.elapsed().as_secs_f64();
        if let Some(duration) = self.duration_seconds {
            position = position.min(duration);
        }
        let progress = match self.duration_seconds {
            Some(duration) if duration > 0.0 => {
                (position * 100.0 / duration).clamp(0.0, 100.0).round() as u32
            }
            _ => 0,
        };
        if let Some(object) = value.as_object_mut() {
            object.insert("position_seconds".into(), json!(position));
            object.insert("elapsed".into(), json!(format_seconds(Some(position))));
            object.insert("progress".into(), json!(progress));
        }
        value
    }
}

#[derive(Clone)]
pub(super) struct MprisMonitor {
    cache: Arc<RwLock<HashMap<String, CachedPlayer>>>,
    connection: Arc<RwLock<Option<Connection>>>,
    started: Arc<AtomicBool>,
}

impl MprisMonitor {
    pub(super) fn new() -> Self {
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            connection: Arc::new(RwLock::new(None)),
            started: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn start(&self) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let monitor = self.clone();
        tokio::spawn(async move {
            monitor.run().await;
        });
    }

    pub(super) async fn snapshot(&self, source: &str) -> Option<Value> {
        self.cache
            .read()
            .await
            .get(source)
            .map(CachedPlayer::snapshot)
    }

    pub(super) async fn control(&self, service: &str, method: &str) -> Result<()> {
        let connection = self
            .connection
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("System D-Bus connection is unavailable"))?;
        let proxy = Proxy::new(
            &connection,
            service.to_owned(),
            MPRIS_PATH,
            MPRIS_PLAYER_INTERFACE,
        )
        .await
        .with_context(|| format!("failed to create MPRIS proxy for {service}"))?;
        proxy
            .call_method(method, &())
            .await
            .with_context(|| format!("MPRIS command {method} failed for {service}"))?;
        Ok(())
    }

    async fn clear(&self, source: &str) {
        self.cache.write().await.remove(source);
    }

    async fn discover_service(connection: &Connection, prefix: &str) -> Result<Option<String>> {
        let dbus = DBusProxy::new(connection).await?;
        let names = dbus.list_names().await?;
        Ok(names
            .iter()
            .map(|name| name.as_str())
            .find(|name| *name == prefix)
            .or_else(|| {
                names
                    .iter()
                    .map(|name| name.as_str())
                    .find(|name| name.starts_with(&format!("{prefix}.")))
            })
            .map(str::to_owned))
    }

    async fn refresh(
        &self,
        source: &str,
        service: &str,
        properties: &PropertiesProxy<'_>,
    ) -> Result<()> {
        let interface = InterfaceName::try_from(MPRIS_PLAYER_INTERFACE)?;
        let values = properties.get_all(interface).await?;
        self.cache.write().await.insert(
            source.to_owned(),
            CachedPlayer::from_properties(source, service, &values),
        );
        Ok(())
    }

    async fn watch_service(
        &self,
        connection: &Connection,
        source: &str,
        service: &str,
    ) -> Result<()> {
        let properties = PropertiesProxy::new(connection, service.to_owned(), MPRIS_PATH).await?;
        let mut changes = properties
            .receive_properties_changed_with_args(&[(0, MPRIS_PLAYER_INTERFACE)])
            .await?;
        let dbus = DBusProxy::new(connection).await?;
        let mut owners = dbus
            .receive_name_owner_changed_with_args(&[(0, service)])
            .await?;

        self.refresh(source, service, &properties).await?;
        info!(source, service, "Persistent MPRIS monitor connected");

        loop {
            tokio::select! {
                changed = changes.next() => {
                    if changed.is_none() {
                        break;
                    }
                    if let Err(error) = self.refresh(source, service, &properties).await {
                        debug!(source, service, error = %error, "MPRIS property refresh failed");
                        break;
                    }
                }
                owner = owners.next() => {
                    if owner.is_some() {
                        break;
                    }
                    break;
                }
            }
        }

        Ok(())
    }

    async fn run_source(&self, connection: Connection, source: &'static str) {
        let Some(prefix) = source_prefix(source) else {
            return;
        };

        loop {
            match Self::discover_service(&connection, prefix).await {
                Ok(Some(service)) => {
                    if let Err(error) = self.watch_service(&connection, source, &service).await {
                        debug!(source, service, error = %error, "MPRIS monitor disconnected");
                    }
                    self.clear(source).await;
                }
                Ok(None) => {
                    self.clear(source).await;
                }
                Err(error) => {
                    self.clear(source).await;
                    debug!(source, error = %error, "MPRIS service discovery failed");
                }
            }
            sleep(RECONNECT_DELAY).await;
        }
    }

    async fn run(self) {
        loop {
            match Connection::system().await {
                Ok(connection) => {
                    *self.connection.write().await = Some(connection.clone());
                    info!("Persistent system D-Bus connection established for MPRIS");

                    let spotify_monitor = self.clone();
                    let spotify_connection = connection.clone();
                    let spotify = tokio::spawn(async move {
                        spotify_monitor
                            .run_source(spotify_connection, SPOTIFY_SOURCE)
                            .await;
                    });
                    let airplay_monitor = self.clone();
                    let airplay_connection = connection.clone();
                    let airplay = tokio::spawn(async move {
                        airplay_monitor
                            .run_source(airplay_connection, AIRPLAY_SOURCE)
                            .await;
                    });

                    let _ = connection.closed().await;
                    spotify.abort();
                    airplay.abort();
                    *self.connection.write().await = None;
                    self.clear(SPOTIFY_SOURCE).await;
                    self.clear(AIRPLAY_SOURCE).await;
                    debug!("System D-Bus connection for MPRIS closed");
                }
                Err(error) => {
                    debug!(error = %error, "System D-Bus unavailable for MPRIS");
                }
            }
            sleep(RECONNECT_DELAY).await;
        }
    }
}

impl WebController {
    pub(super) async fn mpris_player(&self, source: &str) -> Option<Value> {
        self.mpris.snapshot(source).await
    }

    pub(super) async fn mpris_control(&self, service: &str, method: &str) -> Result<()> {
        self.mpris.control(service, method).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use zbus::zvariant::{OwnedValue, Value as ZValue};

    use super::{CachedPlayer, SPOTIFY_PREFIX, SPOTIFY_SOURCE};

    #[test]
    fn parses_mpris_properties_into_existing_player_shape() {
        let metadata = HashMap::from([
            (
                "xesam:title".to_owned(),
                ZValue::new("Track").try_to_owned().unwrap(),
            ),
            (
                "xesam:album".to_owned(),
                ZValue::new("Album").try_to_owned().unwrap(),
            ),
            (
                "xesam:artist".to_owned(),
                ZValue::new(vec!["Artist"]).try_to_owned().unwrap(),
            ),
            ("mpris:length".to_owned(), OwnedValue::from(200_000_000_i64)),
        ]);
        let properties = HashMap::from([
            (
                "PlaybackStatus".to_owned(),
                ZValue::new("Playing").try_to_owned().unwrap(),
            ),
            ("Position".to_owned(), OwnedValue::from(12_000_000_i64)),
            ("CanControl".to_owned(), OwnedValue::from(true)),
            ("CanPlay".to_owned(), OwnedValue::from(true)),
            ("CanPause".to_owned(), OwnedValue::from(true)),
            ("CanGoNext".to_owned(), OwnedValue::from(true)),
            ("CanGoPrevious".to_owned(), OwnedValue::from(false)),
            ("Metadata".to_owned(), OwnedValue::from(metadata)),
        ]);

        let player =
            CachedPlayer::from_properties(SPOTIFY_SOURCE, SPOTIFY_PREFIX, &properties).snapshot();
        assert_eq!(player["backend"], "spotify-mpris");
        assert_eq!(player["state"], "playing");
        assert_eq!(player["title"], "Track");
        assert_eq!(player["artist"], "Artist");
        assert_eq!(player["album"], "Album");
        assert_eq!(player["duration_seconds"], 200.0);
        assert_eq!(player["controls"]["next"], true);
        assert_eq!(player["controls"]["prev"], false);
    }
}
