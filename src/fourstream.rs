use std::collections::HashMap;
use std::fs;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket as StdUdpSocket};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use chrono_tz::OffsetComponents;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};
use url::Url;

use crate::api::WebController;
use crate::dlna;

const SSDP_ADDRESS: &str = "239.255.255.250";
const SSDP_PORT: u16 = 1900;
const DEVICE_TYPE: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
const AVTRANSPORT_SERVICE: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RENDERING_SERVICE: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const WIIMU_SERVICE_TYPE: &str = "urn:schemas-wiimu-com:service:PlayQueue:1";
const NAME: &str = "ProAudio Player";

type GatewayError = (StatusCode, String);
type GatewayResult = std::result::Result<Response, GatewayError>;

fn bad_request(message: impl Into<String>) -> GatewayError {
    (StatusCode::BAD_REQUEST, message.into())
}

fn service_error(err: anyhow::Error) -> GatewayError {
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
        })
        .unwrap_or_else(|| "proaudio-player".into());
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

fn linkplay_uuid(udn: &str) -> String {
    let digest = hex::encode_upper(Sha256::digest(udn.as_bytes()));
    format!("FF{}", &digest[..22])
}

fn primary_mac() -> String {
    for interface in ["end0", "eth0", "wlan0"] {
        if let Ok(value) = fs::read_to_string(format!("/sys/class/net/{interface}/address")) {
            let value = value.trim().to_ascii_uppercase();
            if !value.is_empty() && value != "00:00:00:00:00:00" {
                return value;
            }
        }
    }
    "00:00:00:00:00:00".into()
}

fn source_mode(source: &str) -> &'static str {
    match source {
        "AirPlay" => "1",
        "DLNA / UPnP" => "2",
        "Локальна бібліотека" => "11",
        "Spotify Connect" => "31",
        "" | "Немає потоку" => "0",
        _ => "10",
    }
}

fn local_ip(peer: Option<SocketAddr>) -> String {
    let Ok(socket) = StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
        return "127.0.0.1".into();
    };
    let target =
        peer.unwrap_or_else(|| SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), 53)));
    if socket.connect(target).is_err() {
        return "127.0.0.1".into();
    }
    socket
        .local_addr()
        .map(|address| address.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".into())
}

fn as_f64(value: Option<&Value>) -> f64 {
    value
        .and_then(|value| match value {
            Value::Number(number) => number.as_f64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        })
        .unwrap_or(0.0)
}

fn as_u64(value: Option<&Value>) -> u64 {
    value
        .and_then(|value| match value {
            Value::Number(number) => number.as_u64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        })
        .unwrap_or(0)
}

async fn player_status(controller: &WebController) -> Result<Value> {
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
    let priority = status
        .get("priority")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let state = player
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("stopped");
    let position = (as_f64(player.get("position_seconds")) * 1000.0).round() as u64;
    let duration = (as_f64(player.get("duration_seconds")) * 1000.0).round() as u64;
    let source = player.get("source").and_then(Value::as_str).unwrap_or("");
    let track_uri = player
        .get("track_uri")
        .and_then(Value::as_str)
        .or_else(|| mpd.get("stream_url").and_then(Value::as_str))
        .unwrap_or("");
    Ok(json!({
        "type": "0", "ch": "0", "mode": source_mode(source), "loop": "0", "eq": "0",
        "status": match state { "playing" => "play", "paused" => "pause", _ => "stop" },
        "curpos": position.to_string(), "offset_pts": position.to_string(), "totlen": duration.to_string(),
        "Title": hex::encode_upper(player.get("title").and_then(Value::as_str).unwrap_or("").as_bytes()),
        "Artist": hex::encode_upper(player.get("artist").and_then(Value::as_str).unwrap_or("").as_bytes()),
        "Album": hex::encode_upper(player.get("album").and_then(Value::as_str).unwrap_or("").as_bytes()),
        "track_uri": track_uri,
        "alarmflag": if priority.get("active").and_then(Value::as_bool).unwrap_or(false) { "1" } else { "0" },
        "plicount": as_u64(mpd.get("queue_length")).to_string(),
        "plicurr": as_u64(mpd.get("queue_position")).to_string(),
        "vol": as_f64(status.get("volume")).round().max(0.0).to_string(),
        "mute": if status.get("muted").and_then(Value::as_bool).unwrap_or(false) { "1" } else { "0" },
    }))
}

fn device_status(controller: &WebController) -> Value {
    let udn = device_uuid();
    let linkplay = linkplay_uuid(&udn);
    let ip = local_ip(None);
    let timezone: chrono_tz::Tz = controller
        .config
        .minute_silence
        .timezone
        .parse()
        .unwrap_or(chrono_tz::Europe::Kyiv);
    let now = Utc::now().with_timezone(&timezone);
    let offset = now.offset().base_utc_offset() + now.offset().dst_offset();
    let mac = primary_mac();
    let temp_uuid = linkplay
        .chars()
        .rev()
        .take(16)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    json!({
        "uuid": linkplay, "DeviceName": NAME, "GroupName": NAME, "ssid": NAME.replace(' ', "_"),
        "language": "en_us", "firmware": "proaudio-native-dev", "hardware": "Linux", "build": "debug",
        "project": "PROAUDIO_PLAYER", "priv_prj": "PROAUDIO_PLAYER", "project_build_name": "proaudio",
        "Release": now.format("%Y%m%d").to_string(), "temp_uuid": temp_uuid,
        "hideSSID": "1", "SSIDStrategy": "2", "branch": "dev", "group": "0", "wmrm_version": "0",
        "internet": "1", "MAC": mac, "STA_MAC": primary_mac(), "CountryCode": "UA", "CountryRegion": "1",
        "netstat": "2", "essid": "", "apcli0": ip, "eth2": local_ip(None), "eth_dhcp": "1",
        "VersionUpdate": "0", "NewVer": "0", "date": now.format("%Y:%m:%d").to_string(),
        "time": now.format("%H:%M:%S").to_string(), "tz": format!("{:.4}", offset.num_seconds() as f64 / 3600.0),
        "dst_enable": if now.offset().dst_offset().num_seconds() != 0 { "1" } else { "0" },
        "region": "UA", "prompt_status": "0", "upnp_version": "1005", "cap1": "0x0", "capability": "0x0",
        "streams_all": "0x3", "streams": "0x3", "external": "0x0", "plm_support": "0x0", "preset_key": "0",
        "spotify_active": "1", "battery": "0", "battery_percent": "0", "securemode": "1",
        "upnp_uuid": udn, "uart_pass_port": "0", "communication_port": controller.config.api.port.to_string(),
    })
}

async fn set_volume(controller: &WebController, value: &str) -> Result<()> {
    let percent = value.parse::<f64>().context("Invalid volume")?;
    if !(0.0..=100.0).contains(&percent) {
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

async fn transport(controller: &WebController, action: &str) -> Result<()> {
    controller.ensure_controls_available().await?;
    let mut action = action.to_ascii_lowercase();
    if action == "onepause" {
        let status = controller.status().await?;
        action = if status
            .get("player")
            .and_then(Value::as_object)
            .and_then(|player| player.get("state"))
            .and_then(Value::as_str)
            == Some("playing")
        {
            "pause".into()
        } else {
            "play".into()
        };
    }
    if action == "resume" {
        action = "play".into();
    }
    if !matches!(action.as_str(), "play" | "pause" | "stop" | "next" | "prev") {
        bail!("Unsupported player command");
    }
    controller.control_active_player(&action).await?;
    Ok(())
}

async fn play_url(controller: &WebController, value: &str) -> Result<()> {
    let parsed = Url::parse(value).context("Invalid stream URL")?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        bail!("Invalid stream URL");
    }
    controller.ensure_controls_available().await?;
    controller.run("mpc", &["clear"], true, 8).await?;
    controller.run("mpc", &["add", value], true, 8).await?;
    controller.run("mpc", &["play"], true, 8).await?;
    Ok(())
}

async fn http_api(
    State(controller): State<WebController>,
    Query(query): Query<HashMap<String, String>>,
) -> GatewayResult {
    let command = query.get("command").cloned().unwrap_or_default();
    let lowered = command.to_ascii_lowercase();
    if matches!(lowered.as_str(), "getstatus" | "getstatusex") {
        return Ok(Json(device_status(&controller)).into_response());
    }
    if matches!(
        lowered.as_str(),
        "getplayerstatus" | "getmetainfo" | "gettrackinfo"
    ) {
        return player_status(&controller)
            .await
            .map(|value| Json(value).into_response())
            .map_err(service_error);
    }
    if lowered == "getdevicename" {
        return Ok(text_response(NAME, "text/plain; charset=utf-8"));
    }
    if matches!(
        lowered.as_str(),
        "getnewver" | "getmvremoteupdatestartcheck"
    ) {
        return Ok(Json(json!({"status":"0","NewVer":"0"})).into_response());
    }
    if lowered == "wlangetconnectstate" {
        return Ok(text_response("OK", "text/plain; charset=utf-8"));
    }
    if lowered == "getequalizer" {
        return Ok(text_response("0", "text/plain; charset=utf-8"));
    }
    if lowered == "multiroom:getslavelist" {
        return Ok(Json(json!({"slaves":"0","slave_list":[]})).into_response());
    }
    if lowered.starts_with("setplayercmd:play:http://")
        || lowered.starts_with("setplayercmd:play:https://")
        || lowered.starts_with("setplayercmd:playlist:http://")
        || lowered.starts_with("setplayercmd:playlist:https://")
    {
        play_url(&controller, command.splitn(3, ':').nth(2).unwrap_or(""))
            .await
            .map_err(service_error)?;
        return Ok(text_response("OK", "text/plain; charset=utf-8"));
    }
    if lowered.starts_with("setplayercmd:vol:") {
        set_volume(&controller, command.rsplit(':').next().unwrap_or(""))
            .await
            .map_err(service_error)?;
        return Ok(text_response("OK", "text/plain; charset=utf-8"));
    }
    if lowered.starts_with("setplayercmd:mute:") {
        set_mute(&controller, command.rsplit(':').next().unwrap_or(""))
            .await
            .map_err(service_error)?;
        return Ok(text_response("OK", "text/plain; charset=utf-8"));
    }
    if lowered.starts_with("setplayercmd:") {
        transport(
            &controller,
            command
                .split_once(':')
                .map(|(_, value)| value)
                .unwrap_or(""),
        )
        .await
        .map_err(service_error)?;
        return Ok(text_response("OK", "text/plain; charset=utf-8"));
    }
    Err(bad_request("unknown command"))
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

fn clock(milliseconds: &str) -> String {
    let total = milliseconds.parse::<u64>().unwrap_or(0) / 1000;
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
) -> GatewayResult {
    let action_header = headers
        .get("SOAPACTION")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim_matches('"');
    let (service, action) = action_header
        .split_once('#')
        .ok_or_else(|| bad_request("Missing SOAPACTION"))?;
    let body = std::str::from_utf8(&body).map_err(|_| bad_request("Invalid SOAP XML"))?;
    if !body.contains('<') || !body.contains('>') {
        return Err(bad_request("Invalid SOAP XML"));
    }

    let player = player_status(&controller).await.map_err(service_error)?;
    let get = |name: &str| player.get(name).and_then(Value::as_str).unwrap_or("");
    let mut values: Vec<(&str, String)> = Vec::new();
    match action {
        "GetTransportInfo" => values.extend([
            (
                "CurrentTransportState",
                match get("status") {
                    "play" => "PLAYING",
                    "pause" => "PAUSED_PLAYBACK",
                    _ => "STOPPED",
                }
                .into(),
            ),
            ("CurrentTransportStatus", "OK".into()),
            ("CurrentSpeed", "1".into()),
        ]),
        "GetPositionInfo" => values.extend([
            ("Track", "1".into()),
            ("TrackDuration", clock(get("totlen"))),
            ("TrackMetaData", "".into()),
            ("TrackURI", get("track_uri").into()),
            ("RelTime", clock(get("curpos"))),
            ("AbsTime", clock(get("curpos"))),
            ("RelCount", "0".into()),
            ("AbsCount", "0".into()),
        ]),
        "GetMediaInfo" => values.extend([
            ("NrTracks", "1".into()),
            ("MediaDuration", clock(get("totlen"))),
            ("CurrentURI", get("track_uri").into()),
            ("CurrentURIMetaData", "".into()),
            ("NextURI", "".into()),
            ("NextURIMetaData", "".into()),
            ("PlayMedium", "NETWORK".into()),
            ("RecordMedium", "NOT_IMPLEMENTED".into()),
            ("WriteStatus", "NOT_IMPLEMENTED".into()),
        ]),
        "GetCurrentTransportActions" => values.push((
            "Actions",
            "Play,Pause,Stop,Seek,Next,Previous".into(),
        )),
        "SetAVTransportURI" => {
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
        "SetNextAVTransportURI" => {
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
        "Seek" => {
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
        "GetVolume" => values.push(("CurrentVolume", get("vol").into())),
        "GetMute" => values.push(("CurrentMute", get("mute").into())),
        "SetVolume" => set_volume(&controller, &soap_value(body, "DesiredVolume"))
            .await
            .map_err(service_error)?,
        "SetMute" => set_mute(&controller, &soap_value(body, "DesiredMute"))
            .await
            .map_err(service_error)?,
        "Play" | "Pause" | "Stop" | "Next" | "Previous" => {
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

    let fallback = if matches!(action, "GetVolume" | "GetMute" | "SetVolume" | "SetMute") {
        RENDERING_SERVICE
    } else {
        AVTRANSPORT_SERVICE
    };
    let namespace = if service.is_empty() { fallback } else { service };
    let fields = values
        .iter()
        .map(|(key, value)| format!("<{key}>{}</{key}>", xml_escape(value)))
        .collect::<String>();
    Ok(text_response(
        format!(
            "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action}Response xmlns:u=\"{}\">{fields}</u:{action}Response></s:Body></s:Envelope>",
            xml_escape(namespace)
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
    let udn = device_uuid();
    let linkplay = linkplay_uuid(&udn);
    text_response(
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
 <specVersion><major>1</major><minor>0</minor></specVersion><URLBase>http://{host}/</URLBase>
 <device><deviceType>{DEVICE_TYPE}</deviceType><friendlyName>{NAME}</friendlyName>
  <manufacturer>ProAudio Player</manufacturer><manufacturerURL>https://github.com/bodzey/proaudio-player-native</manufacturerURL>
  <modelDescription>ProAudio network audio renderer</modelDescription><modelName>ProAudio Player</modelName><modelNumber>dev</modelNumber>
  <serialNumber>{linkplay}</serialNumber><UDN>{udn}</UDN><serviceList>
   <service><serviceType>{AVTRANSPORT_SERVICE}</serviceType><serviceId>urn:upnp-org:serviceId:AVTransport</serviceId><SCPDURL>/upnp/avtransport.xml</SCPDURL><controlURL>/upnp/control</controlURL><eventSubURL>/upnp/event</eventSubURL></service>
   <service><serviceType>{RENDERING_SERVICE}</serviceType><serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId><SCPDURL>/upnp/renderingcontrol.xml</SCPDURL><controlURL>/upnp/control</controlURL><eventSubURL>/upnp/event</eventSubURL></service>
   <service><serviceType>{WIIMU_SERVICE_TYPE}</serviceType><serviceId>urn:wiimu-com:serviceId:PlayQueue</serviceId><SCPDURL>/upnp/playqueue.xml</SCPDURL><controlURL>/upnp/control</controlURL><eventSubURL>/upnp/event</eventSubURL></service>
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

async fn service_description() -> Response {
    scpd(&[
        "SetAVTransportURI",
        "GetTransportInfo",
        "GetPositionInfo",
        "Play",
        "Pause",
        "Stop",
        "GetVolume",
        "SetVolume",
        "GetMute",
        "SetMute",
    ])
}

pub fn router() -> Router<WebController> {
    Router::new()
        .route("/httpapi.asp", get(http_api))
        .route("/upnp/device.xml", get(description))
        .route("/description.xml", get(description))
        .route("/upnp/service.xml", get(service_description))
        .route("/upnp/avtransport.xml", get(avtransport_description))
        .route("/upnp/renderingcontrol.xml", get(rendering_description))
        .route("/upnp/playqueue.xml", get(service_description))
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
        (
            WIIMU_SERVICE_TYPE.to_owned(),
            format!("{udn}::{WIIMU_SERVICE_TYPE}"),
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
            "SERVER: Linux UPnP/1.0 ProAudioPlayer/dev".into(),
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
        && requested != WIIMU_SERVICE_TYPE
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
            "SERVER: Linux UPnP/1.0 ProAudioPlayer/dev".into(),
            format!("ST: {st}"),
            format!("USN: {usn}"),
            String::new(),
            String::new(),
        ]
        .join("\r\n")
        .into_bytes(),
    )
}

pub async fn run_ssdp(_controller: WebController, port: u16) -> Result<()> {
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
    let mut buffer = vec![0u8; 8192];
    info!("ProAudio Player UPnP/4STREAM discovery active on SSDP");
    loop {
        tokio::select! {
            _ = ticker.tick() => for message in alive_messages(port, &udn) {
                if let Err(err) = socket.send_to(&message, destination).await { debug!("ssdp:alive send failed: {err}"); }
            },
            result = socket.recv_from(&mut buffer) => match result {
                Ok((length, peer)) => if let Some(response) = ssdp_response(&String::from_utf8_lossy(&buffer[..length]), peer, port, &udn) {
                    if let Err(err) = socket.send_to(&response, peer).await { debug!("SSDP reply failed: {err}"); }
                },
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
            "192.0.2.10:1900".parse().unwrap(),
            8080,
            "uuid:test",
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&response).contains(AVTRANSPORT_SERVICE));
    }
}
