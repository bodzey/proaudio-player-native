use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use url::Url;

use crate::command;

const AVTRANSPORT_SERVICE: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const AVTRANSPORT_PORT: u16 = 49494;
const AVTRANSPORT_PATH: &str = "/upnp/control/rendertransport1";

static CLIENT: OnceLock<DlnaClient> = OnceLock::new();

pub fn client() -> &'static DlnaClient {
    CLIENT.get_or_init(DlnaClient::new)
}

pub struct DlnaClient {
    client: reqwest::Client,
    endpoint: Arc<Mutex<Option<String>>>,
}

impl DlnaClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint: Arc::new(Mutex::new(None)),
        }
    }

    async fn soap_at(&self, endpoint: &str, action: &str, arguments: &str) -> Result<String> {
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:{action} xmlns:u="{AVTRANSPORT_SERVICE}">
      <InstanceID>0</InstanceID>
      {arguments}
    </u:{action}>
  </s:Body>
</s:Envelope>"#
        );
        let response = self
            .client
            .post(endpoint)
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .header("SOAPACTION", format!("\"{AVTRANSPORT_SERVICE}#{action}\""))
            .body(body)
            .timeout(Duration::from_secs(1))
            .send()
            .await
            .with_context(|| format!("DLNA AVTransport {action} недоступний"))?;
        let status = response.status();
        let payload = response
            .text()
            .await
            .with_context(|| format!("Не вдалося прочитати DLNA AVTransport {action}"))?;
        if !status.is_success() {
            bail!("DLNA AVTransport {action} повернув HTTP {status}");
        }
        Ok(payload)
    }

    async fn discover_endpoint(&self) -> Result<Option<String>> {
        let cached = { self.endpoint.lock().await.clone() };
        if cached.is_some() {
            return Ok(cached);
        }

        let mut hosts = vec!["127.0.0.1".to_owned()];
        if let Ok(output) = command::run(
            "ip",
            &["-4", "-o", "addr", "show", "scope", "global"],
            false,
            3,
        )
        .await
        {
            if output.code == 0 {
                for line in output.stdout.lines() {
                    let mut fields = line.split_whitespace();
                    while let Some(field) = fields.next() {
                        if field == "inet" {
                            if let Some(cidr) = fields.next() {
                                if let Some(address) = cidr.split('/').next() {
                                    if !address.is_empty() {
                                        hosts.push(address.to_owned());
                                    }
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }
        hosts.sort();
        hosts.dedup();

        for host in hosts {
            let endpoint = format!("http://{host}:{AVTRANSPORT_PORT}{AVTRANSPORT_PATH}");
            if self
                .soap_at(&endpoint, "GetTransportInfo", "")
                .await
                .is_ok()
            {
                *self.endpoint.lock().await = Some(endpoint.clone());
                return Ok(Some(endpoint));
            }
        }
        Ok(None)
    }

    async fn invalidate_endpoint(&self, endpoint: &str) {
        let mut cached = self.endpoint.lock().await;
        if cached.as_deref() == Some(endpoint) {
            *cached = None;
        }
    }

    async fn player_at(&self, endpoint: &str) -> Result<Value> {
        let position = self.soap_at(endpoint, "GetPositionInfo", "").await?;
        let transport = self.soap_at(endpoint, "GetTransportInfo", "").await?;
        let actions = self
            .soap_at(endpoint, "GetCurrentTransportActions", "")
            .await?;
        Ok(parse_player(&position, &transport, &actions))
    }

    pub async fn player(&self) -> Result<Option<Value>> {
        let Some(endpoint) = self.discover_endpoint().await? else {
            return Ok(None);
        };
        match self.player_at(&endpoint).await {
            Ok(player) => Ok(Some(player)),
            Err(error) => {
                self.invalidate_endpoint(&endpoint).await;
                Err(error)
            }
        }
    }

    pub async fn known_player(&self) -> Result<Option<Value>> {
        let cached = { self.endpoint.lock().await.clone() };
        let Some(endpoint) = cached else {
            return Ok(None);
        };
        match self.player_at(&endpoint).await {
            Ok(player) => Ok(Some(player)),
            Err(error) => {
                self.invalidate_endpoint(&endpoint).await;
                Err(error)
            }
        }
    }

    pub async fn control(&self, action: &str) -> Result<()> {
        let Some(endpoint) = self.discover_endpoint().await? else {
            bail!("DLNA AVTransport endpoint не знайдено");
        };
        let (soap_action, arguments) = match action {
            "play" => ("Play", "<Speed>1</Speed>"),
            "pause" => ("Pause", ""),
            "stop" => ("Stop", ""),
            "next" => ("Next", ""),
            "prev" => ("Previous", ""),
            _ => bail!("Невідома DLNA transport-команда"),
        };
        if let Err(error) = self.soap_at(&endpoint, soap_action, arguments).await {
            self.invalidate_endpoint(&endpoint).await;
            return Err(error);
        }
        Ok(())
    }
}

impl Default for DlnaClient {
    fn default() -> Self {
        Self::new()
    }
}

fn xml_unescape(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn tag_value(document: &str, tag: &str) -> Option<String> {
    let opening = format!("<{tag}>");
    let closing = format!("</{tag}>");
    let start = document.find(&opening)? + opening.len();
    let end = start + document[start..].find(&closing)?;
    Some(xml_unescape(document[start..end].trim()))
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

fn clock_to_seconds(value: Option<&str>) -> Option<f64> {
    let value = value?.trim();
    if value.is_empty() || value == "NOT_IMPLEMENTED" {
        return None;
    }
    let parts = value
        .split(':')
        .map(str::parse::<u64>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .ok()?;
    match parts.as_slice() {
        [minutes, seconds] => Some((minutes * 60 + seconds) as f64),
        [hours, minutes, seconds] => Some((hours * 3600 + minutes * 60 + seconds) as f64),
        _ => None,
    }
}

fn media_server(track_uri: &str) -> Option<String> {
    Url::parse(track_uri)
        .ok()?
        .host_str()
        .filter(|host| !host.is_empty())
        .map(str::to_owned)
}

fn title_from_uri(track_uri: &str) -> Option<String> {
    let url = Url::parse(track_uri).ok()?;
    let mut segments = url.path_segments()?;
    nonempty(segments.next_back().map(str::to_owned))
}

fn parse_player(position: &str, transport: &str, actions: &str) -> Value {
    let metadata = tag_value(position, "TrackMetaData").unwrap_or_default();
    let track_uri = tag_value(position, "TrackURI").unwrap_or_default();
    let title = nonempty(tag_value(&metadata, "dc:title"))
        .or_else(|| nonempty(tag_value(&metadata, "title")))
        .or_else(|| title_from_uri(&track_uri))
        .unwrap_or_default();
    let artist = nonempty(tag_value(&metadata, "upnp:artist"))
        .or_else(|| nonempty(tag_value(&metadata, "dc:creator")))
        .unwrap_or_default();
    let album = nonempty(tag_value(&metadata, "upnp:album")).unwrap_or_default();
    let art_url = nonempty(tag_value(&metadata, "upnp:albumArtURI")).filter(|value| {
        Url::parse(value)
            .ok()
            .is_some_and(|url| matches!(url.scheme(), "http" | "https"))
    });

    let raw_state = tag_value(transport, "CurrentTransportState")
        .unwrap_or_default()
        .to_ascii_uppercase();
    let state = match raw_state.as_str() {
        "PLAYING" => "playing",
        "PAUSED_PLAYBACK" => "paused",
        _ => "stopped",
    };

    let elapsed =
        nonempty(tag_value(position, "RelTime")).filter(|value| value != "NOT_IMPLEMENTED");
    let duration =
        nonempty(tag_value(position, "TrackDuration")).filter(|value| value != "NOT_IMPLEMENTED");
    let position_seconds = clock_to_seconds(elapsed.as_deref());
    let duration_seconds = clock_to_seconds(duration.as_deref());
    let progress = match (position_seconds, duration_seconds) {
        (Some(position), Some(duration)) if duration > 0.0 => {
            (position * 100.0 / duration).clamp(0.0, 100.0).round() as u32
        }
        _ => 0,
    };

    let available_actions = tag_value(actions, "Actions")
        .unwrap_or_default()
        .split(',')
        .map(|action| action.trim().to_ascii_uppercase())
        .filter(|action| !action.is_empty())
        .collect::<HashSet<_>>();

    json!({
        "source": "DLNA / UPnP",
        "backend": "dlna-upnp",
        "state": state,
        "title": title,
        "artist": artist,
        "album": album,
        "art_url": art_url,
        "track_uri": track_uri,
        "media_server": media_server(&track_uri),
        "position_seconds": position_seconds,
        "duration_seconds": duration_seconds,
        "elapsed": elapsed,
        "duration": duration,
        "progress": progress,
        "controls": {
            "play": available_actions.contains("PLAY"),
            "pause": available_actions.contains("PAUSE"),
            "stop": available_actions.contains("STOP"),
            "next": available_actions.contains("NEXT"),
            "prev": available_actions.contains("PREVIOUS"),
            "seek": available_actions.contains("SEEK"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gmediarender_metadata_progress_and_controls() {
        let position = r#"<GetPositionInfoResponse>
<TrackDuration>2:34:20</TrackDuration>
<TrackMetaData>&lt;DIDL-Lite xmlns:dc=&quot;http://purl.org/dc/elements/1.1/&quot;&gt;&lt;item&gt;&lt;dc:title&gt;bodzey_polonne_2025-08-23&lt;/dc:title&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;</TrackMetaData>
<TrackURI>http://192.168.88.116:10246/MDEServer/test/1000.mp3</TrackURI>
<RelTime>0:23:17</RelTime>
</GetPositionInfoResponse>"#;
        let transport = r#"<GetTransportInfoResponse><CurrentTransportState>PLAYING</CurrentTransportState></GetTransportInfoResponse>"#;
        let actions = r#"<GetCurrentTransportActionsResponse><Actions>PAUSE,STOP,SEEK</Actions></GetCurrentTransportActionsResponse>"#;

        let player = parse_player(position, transport, actions);
        assert_eq!(
            player.get("title").and_then(Value::as_str),
            Some("bodzey_polonne_2025-08-23")
        );
        assert_eq!(
            player.get("media_server").and_then(Value::as_str),
            Some("192.168.88.116")
        );
        assert_eq!(player.get("state").and_then(Value::as_str), Some("playing"));
        assert_eq!(player.get("progress").and_then(Value::as_u64), Some(15));
        assert_eq!(
            player
                .get("controls")
                .and_then(|value| value.get("pause"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            player
                .get("controls")
                .and_then(|value| value.get("play"))
                .and_then(Value::as_bool),
            Some(false)
        );
    }
}
