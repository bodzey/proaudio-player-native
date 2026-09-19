use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use url::Url;

const AVTRANSPORT_SERVICE: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const CONNECTION_MANAGER_SERVICE: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";
const AVTRANSPORT_ENDPOINTS: &[&str] = &[
    "http://169.254.253.1:49494/upnp/control/rendertransport1",
    "http://127.0.0.1:49494/upnp/control/rendertransport1",
];

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

    async fn soap_body_at(
        &self,
        endpoint: &str,
        service: &str,
        action: &str,
        body: &str,
    ) -> Result<String> {
        let response = self
            .client
            .post(endpoint)
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .header("SOAPACTION", format!("\"{service}#{action}\""))
            .body(body.to_owned())
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .with_context(|| format!("DLNA SOAP {service}#{action} недоступний"))?;
        let status = response.status();
        let payload = response
            .text()
            .await
            .with_context(|| format!("Не вдалося прочитати DLNA SOAP {service}#{action}"))?;
        if !status.is_success() {
            bail!("DLNA SOAP {service}#{action} повернув HTTP {status}");
        }
        Ok(payload)
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
        self.soap_body_at(endpoint, AVTRANSPORT_SERVICE, action, &body)
            .await
    }

    fn related_endpoint(endpoint: &str, path: &str) -> Result<String> {
        let mut url = Url::parse(endpoint).context("Некоректний внутрішній DLNA endpoint")?;
        url.set_path(path);
        url.set_query(None);
        url.set_fragment(None);
        Ok(url.to_string())
    }

    async fn discover_endpoint(&self) -> Result<Option<String>> {
        let cached = { self.endpoint.lock().await.clone() };
        if cached.is_some() {
            return Ok(cached);
        }

        // gmediarender is an internal decoder worker on a private dummy network.
        // The legacy loopback endpoint remains a compatibility fallback for an
        // already-running older image during an in-place native daemon upgrade.
        for endpoint in AVTRANSPORT_ENDPOINTS {
            if self.soap_at(endpoint, "GetTransportInfo", "").await.is_ok() {
                let endpoint = (*endpoint).to_owned();
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

    async fn endpoint(&self) -> Result<String> {
        self.discover_endpoint()
            .await?
            .ok_or_else(|| anyhow::anyhow!("DLNA AVTransport endpoint не знайдено"))
    }

    async fn invoke(&self, action: &str, arguments: &str) -> Result<String> {
        let endpoint = self.endpoint().await?;
        match self.soap_at(&endpoint, action, arguments).await {
            Ok(payload) => Ok(payload),
            Err(error) => {
                self.invalidate_endpoint(&endpoint).await;
                Err(error)
            }
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

    pub async fn connection_manager(&self, action: &str, body: &str) -> Result<String> {
        let transport_endpoint = self.endpoint().await?;
        let endpoint = Self::related_endpoint(&transport_endpoint, "/upnp/control/renderconnmgr1")?;
        match self
            .soap_body_at(&endpoint, CONNECTION_MANAGER_SERVICE, action, body)
            .await
        {
            Ok(payload) => Ok(payload),
            Err(error) => {
                self.invalidate_endpoint(&transport_endpoint).await;
                Err(error)
            }
        }
    }

    pub async fn service_description(&self, path: &str) -> Result<String> {
        if !matches!(
            path,
            "/upnp/rendertransportSCPD.xml"
                | "/upnp/rendercontrolSCPD.xml"
                | "/upnp/renderconnmgrSCPD.xml"
        ) {
            bail!("Невідомий внутрішній DLNA service descriptor");
        }

        let transport_endpoint = self.endpoint().await?;
        let endpoint = Self::related_endpoint(&transport_endpoint, path)?;
        let response = self
            .client
            .get(&endpoint)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .with_context(|| format!("DLNA service descriptor недоступний: {path}"))?;
        let status = response.status();
        let payload = response
            .text()
            .await
            .with_context(|| format!("Не вдалося прочитати DLNA service descriptor: {path}"))?;
        if !status.is_success() {
            bail!("DLNA service descriptor {path} повернув HTTP {status}");
        }
        Ok(payload)
    }

    pub async fn set_uri(&self, uri: &str, metadata: &str) -> Result<()> {
        let uri = validate_media_uri(uri)?;
        let arguments = format!(
            "<CurrentURI>{}</CurrentURI><CurrentURIMetaData>{}</CurrentURIMetaData>",
            xml_escape(uri.as_str()),
            xml_escape(metadata)
        );
        self.invoke("SetAVTransportURI", &arguments).await?;
        Ok(())
    }

    pub async fn set_next_uri(&self, uri: &str, metadata: &str) -> Result<()> {
        let uri = validate_media_uri(uri)?;
        let arguments = format!(
            "<NextURI>{}</NextURI><NextURIMetaData>{}</NextURIMetaData>",
            xml_escape(uri.as_str()),
            xml_escape(metadata)
        );
        self.invoke("SetNextAVTransportURI", &arguments).await?;
        Ok(())
    }

    pub async fn seek_rel_time(&self, target: &str) -> Result<()> {
        if crate::media_time::clock_to_seconds(Some(target)).is_none() {
            bail!("Некоректна DLNA позиція seek");
        }
        let arguments = format!(
            "<Unit>REL_TIME</Unit><Target>{}</Target>",
            xml_escape(target)
        );
        self.invoke("Seek", &arguments).await?;
        Ok(())
    }

    pub async fn control(&self, action: &str) -> Result<()> {
        let (soap_action, arguments) = match action {
            "play" => ("Play", "<Speed>1</Speed>"),
            "pause" => ("Pause", ""),
            "stop" => ("Stop", ""),
            "next" => ("Next", ""),
            "prev" => ("Previous", ""),
            _ => bail!("Невідома DLNA transport-команда"),
        };
        self.invoke(soap_action, arguments).await?;
        Ok(())
    }
}

impl Default for DlnaClient {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_media_uri(value: &str) -> Result<Url> {
    let url = Url::parse(value.trim()).context("Некоректний DLNA media URI")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("DLNA підтримує лише HTTP/HTTPS media URI без облікових даних");
    }
    Ok(url)
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
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
    let position_seconds = crate::media_time::clock_to_seconds(elapsed.as_deref());
    let duration_seconds = crate::media_time::clock_to_seconds(duration.as_deref());
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

    #[test]
    fn media_uri_allows_lan_http_but_rejects_credentials() {
        assert!(validate_media_uri("http://192.168.88.10/music.flac").is_ok());
        assert!(validate_media_uri("https://example.com/music.flac").is_ok());
        assert!(validate_media_uri("http://user:pass@example.com/music.flac").is_err());
        assert!(validate_media_uri("file:///tmp/music.flac").is_err());
    }

    #[test]
    fn seek_clock_rejects_invalid_ranges() {
        assert_eq!(
            crate::media_time::clock_to_seconds(Some("01:02:03")),
            Some(3723.0)
        );
        assert_eq!(crate::media_time::clock_to_seconds(Some("00:99:00")), None);
    }
}
