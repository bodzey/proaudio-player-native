use super::*;

impl WebController {
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

    pub(super) async fn mpris_names(&self) -> Result<Vec<String>> {
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

    pub(super) async fn mpris_service(&self, source: &str, names: &[String]) -> Option<String> {
        let prefix = match source {
            "Spotify Connect" => "org.mpris.MediaPlayer2.spotifyd",
            "AirPlay" => "org.mpris.MediaPlayer2.ShairportSync",
            _ => return None,
        };
        names
            .iter()
            .find(|name| name.as_str() == prefix)
            .cloned()
            .or_else(|| {
                names
                    .iter()
                    .find(|name| name.starts_with(&format!("{prefix}.")))
                    .cloned()
            })
    }

    pub(super) async fn mpris_player(&self, source: &str, service: &str) -> Result<Option<Value>> {
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
        let duration = metadata
            .get("mpris:length")
            .and_then(microseconds_to_seconds);
        let progress = match (position, duration) {
            (Some(p), Some(d)) if d > 0.0 => (p * 100.0 / d).clamp(0.0, 100.0).round() as u32,
            _ => 0,
        };
        let can_control = properties
            .get("CanControl")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let control = |name: &str| {
            can_control
                && properties
                    .get(name)
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        };
        let backend = if source == "Spotify Connect" {
            "spotify-mpris"
        } else {
            "airplay-mpris"
        };
        let art_url = metadata
            .get("mpris:artUrl")
            .and_then(Value::as_str)
            .filter(|value| {
                Url::parse(value)
                    .ok()
                    .is_some_and(|u| matches!(u.scheme(), "http" | "https"))
            });
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

}
