use std::collections::HashSet;
use std::env;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use url::Url;

const AVTRANSPORT_SERVICE: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const AVTRANSPORT_ENDPOINTS: &[&str] = &[
    "http://169.254.253.1:49494/upnp/control/rendertransport1",
    "http://127.0.0.1:49494/upnp/control/rendertransport1",
];

fn configured_endpoint() -> Option<String> {
    env::var("PROAUDIO_DLNA_ENDPOINT")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

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
            .timeout(Duration::from_secs(2))
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

        // The firmware uses a private transport worker, while containerized
        // runtimes may provide a LAN-bound worker in the same container.
        // The explicit endpoint is transport integration state, not hardware policy.
        let mut endpoints = Vec::with_capacity(AVTRANSPORT_ENDPOINTS.len() + 1);
        if let Some(endpoint) = configured_endpoint() {
            endpoints.push(endpoint);
        }
        endpoints.extend(AVTRANSPORT_ENDPOINTS.iter().map(|value| (*value).to_owned()));

        for endpoint in endpoints {
            if self.soap_at(&endpoint, "GetTransportInfo", "").await.is_ok() {
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::timeout;

    use super::*;

    async fn read_http_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];

        loop {
            let read = timeout(Duration::from_secs(2), stream.read(&mut buffer))
                .await
                .expect("mock transport read timeout")
                .expect("mock transport read failed");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);

            let Some(header_end) = request.windows(4).position(|chunk| chunk == b"\r\n\r\n") else {
                continue;
            };
            let header_end = header_end + 4;
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);

            if request.len() >= header_end + content_length {
                break;
            }
        }

        String::from_utf8(request).expect("mock transport request must be UTF-8")
    }

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

    #[tokio::test]
    async fn forwards_cast_uri_and_play_to_transport_worker() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock DLNA transport worker");
        let address = listener.local_addr().expect("mock worker address");

        let server = tokio::spawn(async move {
            for action in ["SetAVTransportURI", "Play"] {
                let (mut stream, _) = listener.accept().await.expect("accept mock SOAP request");
                let request = read_http_request(&mut stream).await;
                assert!(
                    request.contains(action),
                    "SOAP request does not contain expected action {action}: {request}"
                );
                if action == "SetAVTransportURI" {
                    assert!(request.contains(
                        "<CurrentURI>http://192.168.88.116:10246/music/test.mp3?x=1&amp;y=2</CurrentURI>"
                    ));
                }
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await
                    .expect("write mock SOAP response");
            }
        });

        let client = DlnaClient::new();
        *client.endpoint.lock().await = Some(format!("http://{address}/upnp/control"));
        client
            .set_uri("http://192.168.88.116:10246/music/test.mp3?x=1&y=2", "")
            .await
            .expect("forward cast URI");
        client.control("play").await.expect("forward play command");

        server.await.expect("mock transport task");
    }
}
