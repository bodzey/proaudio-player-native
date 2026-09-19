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
const AIRPLAY_DBUS_PREFIX: &str = "org.gnome.ShairportSync";
const AIRPLAY_DBUS_PATH: &str = "/org/gnome/ShairportSync";
const AIRPLAY_REMOTE_INTERFACE: &str = "org.gnome.ShairportSync.RemoteControl";

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

fn normalized_player_state(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "playing" => Some("playing"),
        "paused" => Some("paused"),
        "stopped" => Some("stopped"),
        _ => None,
    }
}

fn airplay_position_from_progress(progress: &str, duration_seconds: f64) -> Option<f64> {
    if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
        return None;
    }
    let mut fields = progress.trim().split('/');
    let start = fields.next()?.parse::<u32>().ok()?;
    let current = fields.next()?.parse::<u32>().ok()?;
    let end = fields.next()?.parse::<u32>().ok()?;
    if fields.next().is_some() {
        return None;
    }

    let total = end.wrapping_sub(start);
    if total == 0 {
        return None;
    }
    let elapsed = current.wrapping_sub(start);
    if elapsed > total {
        return None;
    }

    Some(duration_seconds * f64::from(elapsed) / f64::from(total))
}

#[derive(Clone)]
struct CachedPlayer {
    value: Value,
    refreshed_at: Instant,
    position_seconds: Option<f64>,
    duration_seconds: Option<f64>,
    airplay_progress: Option<String>,
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
            airplay_progress: None,
        }
    }

    fn position_at(&self, now: Instant) -> Option<f64> {
        let mut position = self.position_seconds?;
        if self.value.get("state").and_then(Value::as_str) == Some("playing") {
            position += now
                .saturating_duration_since(self.refreshed_at)
                .as_secs_f64();
        }
        if let Some(duration) = self.duration_seconds {
            position = position.min(duration);
        }
        Some(position)
    }

    fn apply_airplay_overlay(&mut self, properties: &HashMap<String, OwnedValue>) {
        let now = Instant::now();

        if let Some(state) = value_string(properties, "PlayerState")
            .as_deref()
            .and_then(normalized_player_state)
        {
            let previous_state = self.value.get("state").and_then(Value::as_str);
            if previous_state != Some(state) {
                self.position_seconds = self.position_at(now);
                self.refreshed_at = now;
                if let Some(object) = self.value.as_object_mut() {
                    object.insert("state".into(), json!(state));
                }
            }
        }

        if let Some(progress) = value_string(properties, "ProgressString").filter(|v| !v.is_empty())
        {
            if self.airplay_progress.as_deref() != Some(progress.as_str()) {
                if let Some(duration) = self.duration_seconds {
                    if let Some(position) = airplay_position_from_progress(&progress, duration) {
                        self.position_seconds = Some(position);
                        self.refreshed_at = now;
                    }
                }
                self.airplay_progress = Some(progress);
            }
        }
    }

    fn snapshot(&self) -> Value {
        let mut value = self.value.clone();
        let Some(position) = self.position_at(Instant::now()) else {
            return value;
        };
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
        if source == AIRPLAY_SOURCE {
            match self.airplay_properties().await {
                Ok(Some(properties)) => {
                    let mut cache = self.cache.write().await;
                    if let Some(player) = cache.get_mut(source) {
                        player.apply_airplay_overlay(&properties);
                        return Some(player.snapshot());
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    debug!(error = %error, "Shairport native D-Bus state unavailable");
                }
            }
        }

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

    async fn airplay_service(&self) -> Result<Option<(Connection, String)>> {
        let connection = self
            .connection
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("System D-Bus connection is unavailable"))?;
        let Some(service) = Self::discover_service(&connection, AIRPLAY_DBUS_PREFIX).await? else {
            return Ok(None);
        };
        Ok(Some((connection, service)))
    }

    async fn airplay_properties(&self) -> Result<Option<HashMap<String, OwnedValue>>> {
        let Some((connection, service)) = self.airplay_service().await? else {
            return Ok(None);
        };
        let properties = PropertiesProxy::new(&connection, service, AIRPLAY_DBUS_PATH).await?;
        let interface = InterfaceName::try_from(AIRPLAY_REMOTE_INTERFACE)?;
        Ok(Some(properties.get_all(interface).await?))
    }

    pub(super) async fn airplay_control(&self, method: &str) -> Result<()> {
        let (connection, service) = self
            .airplay_service()
            .await?
            .ok_or_else(|| anyhow!("Shairport native D-Bus service is unavailable"))?;
        let proxy = Proxy::new(
            &connection,
            service.to_owned(),
            AIRPLAY_DBUS_PATH,
            AIRPLAY_REMOTE_INTERFACE,
        )
        .await
        .with_context(|| format!("failed to create Shairport D-Bus proxy for {service}"))?;
        proxy
            .call_method(method, &())
            .await
            .with_context(|| format!("Shairport D-Bus command {method} failed for {service}"))?;
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

    pub(super) async fn airplay_control(&self, method: &str) -> Result<()> {
        self.mpris.airplay_control(method).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use zbus::zvariant::{OwnedValue, Value as ZValue};

    use super::{
        airplay_position_from_progress, normalized_player_state, CachedPlayer, SPOTIFY_PREFIX,
        SPOTIFY_SOURCE,
    };

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

    #[test]
    fn maps_shairport_player_states() {
        assert_eq!(normalized_player_state("Playing"), Some("playing"));
        assert_eq!(normalized_player_state("Paused"), Some("paused"));
        assert_eq!(normalized_player_state("Stopped"), Some("stopped"));
        assert_eq!(normalized_player_state("Not Available"), None);
    }

    #[test]
    fn derives_airplay_position_from_wrapping_progress_timestamps() {
        assert_eq!(
            airplay_position_from_progress("100/150/300", 200.0),
            Some(50.0)
        );
        assert_eq!(
            airplay_position_from_progress("4294967270/10/74", 100.0),
            Some(36.0)
        );
    }
}
