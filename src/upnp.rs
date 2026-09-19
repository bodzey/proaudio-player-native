use std::collections::HashMap;
use std::fs;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket as StdUdpSocket};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::Value;
use sha2::{Digest, Sha256};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::api::WebController;
use crate::dlna;

const SSDP_ADDRESS: &str = "239.255.255.250";
const SSDP_PORT: u16 = 1900;
const DEVICE_TYPE: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
const AVTRANSPORT_SERVICE: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RENDERING_SERVICE: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const NAME: &str = "ProAudio Player";
const SERVER: &str =
    concat!("Linux UPnP/1.0 ProAudioPlayer/", env!("CARGO_PKG_VERSION"));

type UpnpError = (StatusCode, String);
type UpnpResult = std::result::Result<Response, UpnpError>;

#[derive(Debug)]
struct PlayerSnapshot {
    state: String,
    position_ms: u64,
    duration_ms: u64,
    track_uri: String,
    volume_percent: f64,
    muted: bool,
}

fn bad_request(message: impl Into<String>) -> UpnpError {
    (StatusCode::BAD_REQUEST, message.into())
}

fn service_error(err: anyhow::Error) -> UpnpError {
    let message = err.to_string();
    let status = if message.contains("заблоковано пріоритетним") {
        StatusCode::CONFLICT
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, message)
}

fn text_response(text: impl Into<String>, content_type: &'static str) -> Response {
    let mut response = text.into().into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

fn device_uuid() -> String {
    let identity = fs::read_to_string("/etc/machine-id")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            fs::read_to_string("/etc/hostname")
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| NAME.to_owned());
    let digest = hex::encode(Sha256::digest(
        format!("proaudio-player:{identity}").as_bytes(),
    ));
    format!(
        "uuid:{}-{}-{}-{}-{}",
        &digest[0..8],
        &digest[8..12],
        &digest[12..16],
        &digest[16..20],
        &digest[20..32]
    )
}

fn device_serial(udn: &str) -> String {
    udn.trim_start_matches("uuid:").replace('-', "")
}

fn local_ip(peer: Option<SocketAddr>) -> String {
    let Ok(socket) = StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
        return Ipv4Addr::LOCALHOST.to_string();
    };
    let target = peer.unwrap_or_else(|| {
        SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(239, 255, 255, 250),
            SSDP_PORT,
        ))
    });
    if socket.connect(target).is_err() {
        return Ipv4Addr::LOCALHOST.to_string();
    }
    socket
        .local_addr()
        .map(|address| address.ip().to_string())
        .unwrap_or_else(|_| Ipv4Addr::LOCALHOST.to_string())
}

fn number(value: Option<&Value>) -> f64 {
    value
        .and_then(|value| match value {
            Value::Number(number) => number.as_f64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        })
        .filter(|value| value.is_finite())
        .unwrap_or(0.0)
}

fn milliseconds(seconds: f64) -> u64 {
    if seconds.is_finite() && seconds > 0.0 {
        (seconds * 1000.0).round().clamp(0.0, u64::MAX as f64) as u64
    } else {
        0
    }
}

async fn player_snapshot(controller: &WebController) -> Result<PlayerSnapshot> {
    let status = controller.status().await?;
    let player = status
        .get("player")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mpd = status
        .get("mpd")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let track_uri = player
        .get("track_uri")
        .and_then(Value::as_str)
        .or_else(|| mpd.get("stream_url").and_then(Value::as_str))
        .unwrap_or("")
        .to_owned();

    Ok(PlayerSnapshot {
        state: player
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("stopped")
            .to_owned(),
        position_ms: milliseconds(number(player.get("position_seconds"))),
        duration_ms: milliseconds(number(player.get("duration_seconds"))),
        track_uri,
        volume_percent: number(status.get("volume")).clamp(0.0, 100.0),
        muted: status.get("muted").and_then(Value::as_bool).unwrap_or(false),
    })
}

async fn set_volume(controller: &WebController, value: &str) -> Result<()> {
    let percent = value.parse::<f64>().context("Invalid volume")?;
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        bail!("Invalid volume");
    }
    controller.ensure_controls_available().await?;
    controller.audio.set_music_volume(percent).await?;
    controller.audio.set_music_mute(percent == 0.0).await
}

async fn set_mute(controller: &WebController, value: &str) -> Result<()> {
    if !matches!(value, "0" | "1") {
        bail!("Invalid mute");
    }
    controller.ensure_controls_available().await?;
    controller.audio.set_music_mute(value == "1").await
}

fn xml_unescape(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn soap_value(body: &str, name: &str) -> String {
    for marker in [format!("<{name}"), format!(":{name}")] {
        let Some(position) = body.find(&marker) else {
            continue;
        };
        let Some(end) = body[position..].find('>') else {
            continue;
        };
        let start = position + end + 1;
        let Some(length) = body[start..].find('<') else {
            continue;
        };
        return xml_unescape(body[start..start + length].trim());
    }
    String::new()
}

fn clock(milliseconds: u64) -> String {
    let total = milliseconds / 1000;
    format!(
        "{:02}:{:02}:{:02}",
        total / 3600,
        (total % 3600) / 60,
        total % 60
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

async fn dlna_transport(controller: &WebController, action: &str) -> Result<()> {
    controller.ensure_controls_available().await?;
    dlna::client().control(action).await
}

async fn soap_control(
    State(controller): State<WebController>,
    headers: HeaderMap,
    body: Bytes,
) -> UpnpResult {
    let action_header = headers
        .get("SOAPACTION")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim_matches('"');
    let (service, action) = action_header
        .split_once('#')
        .ok_or_else(|| bad_request("Missing SOAPACTION"))?;
    if !matches!(service, AVTRANSPORT_SERVICE | RENDERING_SERVICE) {
        return Err(bad_request("Unsupported UPnP service"));
    }

    let body = std::str::from_utf8(&body).map_err(|_| bad_request("Invalid SOAP XML"))?;
    if !body.contains('<') || !body.contains('>') {
        return Err(bad_request("Invalid SOAP XML"));
    }

    let player = player_snapshot(&controller).await.map_err(service_error)?;
    let mut values: Vec<(&str, String)> = Vec::new();

    match action {
        "GetTransportInfo" if service == AVTRANSPORT_SERVICE => values.extend([
            (
                "CurrentTransportState",
                match player.state.as_str() {
                    "playing" => "PLAYING",
                    "paused" => "PAUSED_PLAYBACK",
                    _ => "STOPPED",
                }
                .into(),
            ),
            ("CurrentTransportStatus", "OK".into()),
            ("CurrentSpeed", "1".into()),
        ]),
        "GetPositionInfo" if service == AVTRANSPORT_SERVICE => values.extend([
            ("Track", "1".into()),
            ("TrackDuration", clock(player.duration_ms)),
            ("TrackMetaData", String::new()),
            ("TrackURI", player.track_uri.clone()),
            ("RelTime", clock(player.position_ms)),
            ("AbsTime", clock(player.position_ms)),
            ("RelCount", "0".into()),
            ("AbsCount", "0".into()),
        ]),
        "GetMediaInfo" if service == AVTRANSPORT_SERVICE => values.extend([
            ("NrTracks", "1".into()),
            ("MediaDuration", clock(player.duration_ms)),
            ("CurrentURI", player.track_uri.clone()),
            ("CurrentURIMetaData", String::new()),
            ("NextURI", String::new()),
            ("NextURIMetaData", String::new()),
            ("PlayMedium", "NETWORK".into()),
            ("RecordMedium", "NOT_IMPLEMENTED".into()),
            ("WriteStatus", "NOT_IMPLEMENTED".into()),
        ]),
        "GetCurrentTransportActions" if service == AVTRANSPORT_SERVICE => {
            values.push(("Actions", "Play,Pause,Stop,Seek,Next,Previous".into()));
        }
        "SetAVTransportURI" if service == AVTRANSPORT_SERVICE => {
            controller
                .ensure_controls_available()
                .await
                .map_err(service_error)?;
            let uri = soap_value(body, "CurrentURI");
            if uri.is_empty() {
                return Err(bad_request("Missing CurrentURI"));
            }
            let metadata = soap_value(body, "CurrentURIMetaData");
            dlna::client()
                .set_uri(&uri, &metadata)
                .await
                .map_err(service_error)?;
        }
        "SetNextAVTransportURI" if service == AVTRANSPORT_SERVICE => {
            controller
                .ensure_controls_available()
                .await
                .map_err(service_error)?;
            let uri = soap_value(body, "NextURI");
            if uri.is_empty() {
                return Err(bad_request("Missing NextURI"));
            }
            let metadata = soap_value(body, "NextURIMetaData");
            dlna::client()
                .set_next_uri(&uri, &metadata)
                .await
                .map_err(service_error)?;
        }
        "Seek" if service == AVTRANSPORT_SERVICE => {
            controller
                .ensure_controls_available()
                .await
                .map_err(service_error)?;
            let unit = soap_value(body, "Unit");
            if !unit.eq_ignore_ascii_case("REL_TIME") {
                return Err(bad_request("Only REL_TIME seek is supported"));
            }
            dlna::client()
                .seek_rel_time(&soap_value(body, "Target"))
                .await
                .map_err(service_error)?;
        }
        "GetVolume" if service == RENDERING_SERVICE => {
            values.push(("CurrentVolume", format!("{:.0}", player.volume_percent)));
        }
        "GetMute" if service == RENDERING_SERVICE => {
            values.push(("CurrentMute", if player.muted { "1" } else { "0" }.into()));
        }
        "SetVolume" if service == RENDERING_SERVICE => {
            set_volume(&controller, &soap_value(body, "DesiredVolume"))
                .await
                .map_err(service_error)?;
        }
        "SetMute" if service == RENDERING_SERVICE => {
            set_mute(&controller, &soap_value(body, "DesiredMute"))
                .await
                .map_err(service_error)?;
        }
        "Play" | "Pause" | "Stop" | "Next" | "Previous"
            if service == AVTRANSPORT_SERVICE =>
        {
            let transport_action = if action == "Previous" {
                "prev".to_owned()
            } else {
                action.to_ascii_lowercase()
            };
            dlna_transport(&controller, &transport_action)
                .await
                .map_err(service_error)?;
        }
        _ => return Err(bad_request(format!("Unsupported SOAP action: {action}"))),
    }

    let fields = values
        .iter()
        .map(|(key, value)| format!("<{key}>{}</{key}>", xml_escape(value)))
        .collect::<String>();
    Ok(text_response(
        format!(
            "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action}Response xmlns:u=\"{}\">{fields}</u:{action}Response></s:Body></s:Envelope>",
            xml_escape(service)
        ),
        "text/xml; charset=utf-8",
    ))
}

async fn description(State(controller): State<WebController>, headers: HeaderMap) -> Response {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("127.0.0.1:{}", controller.config.api.port));
    let host = xml_escape(&host);
    let udn = device_uuid();
    let serial = device_serial(&udn);
    let version = env!("CARGO_PKG_VERSION");

    text_response(
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
 <specVersion><major>1</major><minor>0</minor></specVersion><URLBase>http://{host}/</URLBase>
 <device><deviceType>{DEVICE_TYPE}</deviceType><friendlyName>{NAME}</friendlyName>
  <manufacturer>ProAudio Player</manufacturer>
  <modelDescription>ProAudio network audio renderer</modelDescription><modelName>ProAudio Player</modelName><modelNumber>{version}</modelNumber>
  <serialNumber>{serial}</serialNumber><UDN>{udn}</UDN><serviceList>
   <service><serviceType>{AVTRANSPORT_SERVICE}</serviceType><serviceId>urn:upnp-org:serviceId:AVTransport</serviceId><SCPDURL>/upnp/avtransport.xml</SCPDURL><controlURL>/upnp/control</controlURL><eventSubURL></eventSubURL></service>
   <service><serviceType>{RENDERING_SERVICE}</serviceType><serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId><SCPDURL>/upnp/renderingcontrol.xml</SCPDURL><controlURL>/upnp/control</controlURL><eventSubURL></eventSubURL></service>
  </serviceList></device></root>"#
        ),
        "text/xml; charset=utf-8",
    )
}

fn scpd(actions: &[&str]) -> Response {
    let action_list = actions
        .iter()
        .map(|name| format!("<action><name>{name}</name></action>"))
        .collect::<String>();
    text_response(
        format!(
            "<?xml version=\"1.0\"?><scpd xmlns=\"urn:schemas-upnp-org:service-1-0\"><specVersion><major>1</major><minor>0</minor></specVersion><actionList>{action_list}</actionList><serviceStateTable></serviceStateTable></scpd>"
        ),
        "text/xml; charset=utf-8",
    )
}

async fn avtransport_description() -> Response {
    scpd(&[
        "SetAVTransportURI",
        "SetNextAVTransportURI",
        "GetTransportInfo",
        "GetPositionInfo",
        "GetMediaInfo",
        "GetCurrentTransportActions",
        "Play",
        "Pause",
        "Stop",
        "Seek",
        "Next",
        "Previous",
    ])
}

async fn rendering_description() -> Response {
    scpd(&["GetVolume", "SetVolume", "GetMute", "SetMute"])
}

pub fn router() -> Router<WebController> {
    Router::new()
        .route("/upnp/device.xml", get(description))
        .route("/description.xml", get(description))
        .route("/upnp/avtransport.xml", get(avtransport_description))
        .route("/upnp/renderingcontrol.xml", get(rendering_description))
        .route("/upnp/control", post(soap_control))
}

fn alive_messages(port: u16, udn: &str) -> Vec<Vec<u8>> {
    let location = format!("http://{}:{port}/upnp/device.xml", local_ip(None));
    [
        (
            "upnp:rootdevice".to_owned(),
            format!("{udn}::upnp:rootdevice"),
        ),
        (udn.to_owned(), udn.to_owned()),
        (DEVICE_TYPE.to_owned(), format!("{udn}::{DEVICE_TYPE}")),
        (
            AVTRANSPORT_SERVICE.to_owned(),
            format!("{udn}::{AVTRANSPORT_SERVICE}"),
        ),
        (
            RENDERING_SERVICE.to_owned(),
            format!("{udn}::{RENDERING_SERVICE}"),
        ),
    ]
    .into_iter()
    .map(|(nt, usn)| {
        [
            "NOTIFY * HTTP/1.1".into(),
            format!("HOST: {SSDP_ADDRESS}:{SSDP_PORT}"),
            "CACHE-CONTROL: max-age=120".into(),
            format!("LOCATION: {location}"),
            format!("NT: {nt}"),
            "NTS: ssdp:alive".into(),
            format!("SERVER: {SERVER}"),
            format!("USN: {usn}"),
            String::new(),
            String::new(),
        ]
        .join("\r\n")
        .into_bytes()
    })
    .collect()
}

fn ssdp_response(message: &str, peer: SocketAddr, port: u16, udn: &str) -> Option<Vec<u8>> {
    if !message.to_ascii_uppercase().starts_with("M-SEARCH") {
        return None;
    }
    let headers = message
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_ascii_uppercase(), value.trim().to_owned()))
        .collect::<HashMap<_, _>>();
    if !headers
        .get("MAN")
        .is_some_and(|value| value.to_ascii_uppercase().contains("SSDP:DISCOVER"))
    {
        return None;
    }

    let requested = headers.get("ST").map(String::as_str).unwrap_or("ssdp:all");
    if !matches!(requested, "ssdp:all" | "upnp:rootdevice")
        && requested != udn
        && requested != DEVICE_TYPE
        && requested != AVTRANSPORT_SERVICE
        && requested != RENDERING_SERVICE
    {
        return None;
    }

    let st = if requested == "ssdp:all" {
        DEVICE_TYPE
    } else {
        requested
    };
    let usn = if st == udn {
        udn.to_owned()
    } else {
        format!("{udn}::{st}")
    };

    Some(
        [
            "HTTP/1.1 200 OK".into(),
            "CACHE-CONTROL: max-age=120".into(),
            "EXT:".into(),
            format!(
                "LOCATION: http://{}:{port}/upnp/device.xml",
                local_ip(Some(peer))
            ),
            format!("SERVER: {SERVER}"),
            format!("ST: {st}"),
            format!("USN: {usn}"),
            String::new(),
            String::new(),
        ]
        .join("\r\n")
        .into_bytes(),
    )
}

pub async fn run_ssdp(port: u16) -> Result<()> {
    let multicast: Ipv4Addr = SSDP_ADDRESS.parse()?;
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(target_os = "linux")]
    socket.set_reuse_port(true)?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SSDP_PORT).into())?;
    socket.join_multicast_v4(&multicast, &Ipv4Addr::UNSPECIFIED)?;
    socket.set_nonblocking(true)?;
    let socket = UdpSocket::from_std(socket.into())?;
    let udn = device_uuid();
    let destination = SocketAddr::V4(SocketAddrV4::new(multicast, SSDP_PORT));
    let mut ticker = interval(Duration::from_secs(30));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut buffer = vec![0_u8; 8192];

    info!("ProAudio Player UPnP/DLNA discovery active on SSDP");

    loop {
        tokio::select! {
            _ = ticker.tick() => for message in alive_messages(port, &udn) {
                if let Err(err) = socket.send_to(&message, destination).await {
                    debug!("SSDP alive send failed: {err}");
                }
            },
            result = socket.recv_from(&mut buffer) => match result {
                Ok((length, peer)) => {
                    if let Some(response) = ssdp_response(
                        &String::from_utf8_lossy(&buffer[..length]),
                        peer,
                        port,
                        &udn,
                    ) {
                        if let Err(err) = socket.send_to(&response, peer).await {
                            debug!("SSDP reply failed: {err}");
                        }
                    }
                }
                Err(err) => warn!("SSDP receive failed: {err}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soap_values_are_xml_unescaped() {
        let body = "<CurrentURI>http://host/a?x=1&amp;y=2</CurrentURI>";
        assert_eq!(soap_value(body, "CurrentURI"), "http://host/a?x=1&y=2");
    }

    #[test]
    fn ssdp_supports_renderer_services() {
        let response = ssdp_response(
            &format!(
                "M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nST: {AVTRANSPORT_SERVICE}\r\n\r\n"
            ),
            "192.0.2.10:1900".parse().expect("valid test peer"),
            8080,
            "uuid:test",
        )
        .expect("AVTransport must be discoverable");
        assert!(String::from_utf8_lossy(&response).contains(AVTRANSPORT_SERVICE));
    }
}
