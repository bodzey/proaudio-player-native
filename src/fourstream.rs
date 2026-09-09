use std::collections::HashMap;
use std::fs;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket as StdUdpSocket};
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use chrono_tz::OffsetComponents;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};
use url::Url;

use crate::web::WebController;

const SSDP_ADDRESS: &str = "239.255.255.250";
const SSDP_PORT: u16 = 1900;
const DEVICE_TYPE: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
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
    let digest = Sha256::digest(format!("proaudio-player:{identity}").as_bytes());
    let hex = hex::encode(digest);
    format!(
        "uuid:{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn linkplay_uuid(udn: &str) -> String {
    let digest = Sha256::digest(udn.as_bytes());
    format!("FF{}", &hex::encode_upper(digest)[0..22])
}

fn hex_text(value: Option<&str>) -> String {
    hex::encode_upper(value.unwrap_or("").as_bytes())
}

fn primary_mac() -> String {
    for interface in ["end0", "eth0", "wlan0"] {
        let path = format!("/sys/class/net/{interface}/address");
        if let Ok(value) = fs::read_to_string(path) {
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
    let target = peer.unwrap_or_else(|| SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), 53)));
    if socket.connect(target).is_err() {
        return "127.0.0.1".into();
    }
    socket
        .local_addr()
        .map(|address| address.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".into())
}

fn value_f64(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(number)) => number.as_f64().unwrap_or(0.0),
        Some(Value::String(text)) => text.parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn value_u64(value: Option<&Value>) -> u64 {
    match value {
        Some(Value::Number(number)) => number.as_u64().unwrap_or(0),
        Some(Value::String(text)) => text.parse::<u64>().unwrap_or(0),
        _ => 0,
    }
}

async fn player_status(controller: &WebController) -> Result<Map<String, Value>> {
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
    let state = player.get("state").and_then(Value::as_str).unwrap_or("stopped");
    let position = (value_f64(player.get("position_seconds")) * 1000.0).round() as u64;
    let duration = (value_f64(player.get("duration_seconds")) * 1000.0).round() as u64;
    let source = player.get("source").and_then(Value::as_str).unwrap_or("");
    let mut result = Map::new();
    for (key, value) in [
        ("type", "0".into()),
        ("ch", "0".into()),
        ("mode", source_mode(source).into()),
        ("loop", "0".into()),
        ("eq", "0".into()),
        (
            "status",
            match state {
                "playing" => "play",
                "paused" => "pause",
                _ => "stop",
            }
            .into(),
        ),
        ("curpos", position.to_string()),
        ("offset_pts", position.to_string()),
        ("totlen", duration.to_string()),
        ("Title", hex_text(player.get("title").and_then(Value::as_str))),
        ("Artist", hex_text(player.get("artist").and_then(Value::as_str))),
        ("Album", hex_text(player.get("album").and_then(Value::as_str))),
        (
            "alarmflag",
            if priority.get("active").and_then(Value::as_bool).unwrap_or(false) {
                "1"
            } else {
                "0"
            }
            .into(),
        ),
        ("plicount", value_u64(mpd.get("queue_length")).to_string()),
        ("plicurr", value_u64(mpd.get("queue_position")).to_string()),
        (
            "vol",
            value_f64(status.get("volume")).round().max(0.0).to_string(),
        ),
        (
            "mute",
            if status.get("muted").and_then(Value::as_bool).unwrap_or(false) {
                "1"
            } else {
                "0"
            }
            .into(),
        ),
    ] {
        result.insert(key.into(), Value::String(value));
    }
    Ok(result)
}

fn device_status(controller: &WebController) -> Map<String, Value> {
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
    let utc_offset = now.offset().base_utc_offset() + now.offset().dst_offset();
    let mac = primary_mac();
    let mut result = Map::new();
    let entries = [
        ("uuid", linkplay.clone()),
        ("DeviceName", NAME.into()),
        ("GroupName", NAME.into()),
        ("ssid", NAME.replace(' ', "_")),
        ("language", "en_us".into()),
        ("firmware", "proaudio-native-dev".into()),
        ("hardware", "Linux".into()),
        ("build", "debug".into()),
        ("project", "PROAUDIO_PLAYER".into()),
        ("priv_prj", "PROAUDIO_PLAYER".into()),
        ("project_build_name", "proaudio".into()),
        ("Release", now.format("%Y%m%d").to_string()),
        ("temp_uuid", linkplay.chars().rev().take(16).collect::<String>().chars().rev().collect()),
        ("hideSSID", "1".into()),
        ("SSIDStrategy", "2".into()),
        ("branch", "dev".into()),
        ("group", "0".into()),
        ("wmrm_version", "0".into()),
        ("internet", "1".into()),
        ("MAC", mac.clone()),
        ("STA_MAC", mac),
        ("CountryCode", "UA".into()),
        ("CountryRegion", "1".into()),
        ("netstat", "2".into()),
        ("essid", "".into()),
        ("apcli0", ip.clone()),
        ("eth2", ip),
        ("eth_dhcp", "1".into()),
        ("VersionUpdate", "0".into()),
        ("NewVer", "0".into()),
        ("date", now.format("%Y:%m:%d").to_string()),
        ("time", now.format("%H:%M:%S").to_string()),
        ("tz", format!("{:.4}", utc_offset.num_seconds() as f64 / 3600.0)),
        (
            "dst_enable",
            if now.offset().dst_offset().num_seconds() != 0 { "1" } else { "0" }.into(),
        ),
        ("region", "UA".into()),
        ("prompt_status", "0".into()),
        ("upnp_version", "1005".into()),
        ("cap1", "0x0".into()),
        ("capability", "0x0".into()),
        ("streams_all", "0x3".into()),
        ("streams", "0x3".into()),
        ("external", "0x0".into()),
        ("plm_support", "0x0".into()),
        ("preset_key", "0".into()),
        ("spotify_active", "1".into()),
        ("battery", "0".into()),
        ("battery_percent", "0".into()),
        ("securemode", "1".into()),
        ("upnp_uuid", udn),
        ("uart_pass_port", "0".into()),
        ("communication_port", controller.config.web.port.to_string()),
    ];
    for (key, value) in entries {
        result.insert(key.into(), Value::String(value));
    }
    result
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
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
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
        return Ok(Json(Value::Object(device_status(&controller))).into_response());
    }
    if matches!(lowered.as_str(), "getplayerstatus" | "getmetainfo" | "gettrackinfo") {
        return player_status(&controller)
            .await
            .map(|value| Json(Value::Object(value)).into_response())
            .map_err(service_error);
    }
    if lowered == "getdevicename" {
        return Ok(text_response(NAME, "text/plain; charset=utf-8"));
    }
    if matches!(lowered.as_str(), "getnewver" | "getmvremoteupdatestartcheck") {
        return Ok(Json(json!({ "status": "0", "NewVer": "0" })).into_response());
    }
    if lowered == "wlangetconnectstate" {
        return Ok(text_response("OK", "text/plain; charset=utf-8"));
    }
    if lowered == "getequalizer" {
        return Ok(text_response("0", "text/plain; charset=utf-8"));
    }
    if lowered == "multiroom:getslavelist" {
        return Ok(Json(json!({ "slaves": "0", "slave_list": [] })).into_response());
    }
    if lowered.starts_with("setplayercmd:play:http://")
        || lowered.starts_with("setplayercmd:play:https://")
        || lowered.starts_with("setplayercmd:playlist:http://")
        || lowered.starts_with("setplayercmd:playlist:https://")
    {
        let value = command.splitn(3, ':').nth(2).unwrap_or("");
        play_url(&controller, value).await.map_err(service_error)?;
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
        let action = command.split_once(':').map(|(_, value)| value).unwrap_or("");
        transport(&controller, action).await.map_err(service_error)?;
        return Ok(text_response("OK", "text/plain; charset=utf-8"));
    }
    Err(bad_request("unknown command"))
}

fn soap_value(body: &str, name: &str) -> String {
    for marker in [format!("<{name}"), format!(":{name}")] {
        let Some(position) = body.find(&marker) else { continue; };
        let Some(tag_end_relative) = body[position..].find('>') else { continue; };
        let value_start = position + tag_end_relative + 1;
        let Some(value_end_relative) = body[value_start..].find('<') else { continue; };
        return body[value_start..value_start + value_end_relative].to_owned();
    }
    String::new()
}

fn clock(milliseconds: &str) -> String {
    let total = milliseconds.parse::<u64>().unwrap_or(0) / 1000;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
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
        "GetTransportInfo" => {
            values.push((
                "CurrentTransportState",
                match get("status") {
                    "play" => "PLAYING",
                    "pause" => "PAUSED_PLAYBACK",
                    _ => "STOPPED",
                }
                .into(),
            ));
            values.push(("CurrentTransportStatus", "OK".into()));
            values.push(("CurrentSpeed", "1".into()));
        }
        "GetPositionInfo" => {
            values.extend([
                ("Track", "1".into()),
                ("TrackDuration", clock(get("totlen"))),
                ("TrackMetaData", "".into()),
                ("TrackURI", "".into()),
                ("RelTime", clock(get("curpos"))),
                ("AbsTime", clock(get("curpos"))),
                ("RelCount", "0".into()),
                ("AbsCount", "0".into()),
            ]);
        }
        "GetMediaInfo" => {
            values.extend([
                ("NrTracks", get("plicount").into()),
                ("MediaDuration", clock(get("totlen"))),
                ("CurrentURI", "".into()),
                ("CurrentURIMetaData", "".into()),
                ("NextURI", "".into()),
                ("NextURIMetaData", "".into()),
                ("PlayMedium", "NETWORK".into()),
                ("RecordMedium", "NOT_IMPLEMENTED".into()),
                ("WriteStatus", "NOT_IMPLEMENTED".into()),
            ]);
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
            let transport_action = if action == "Previous" { "prev" } else { &action.to_ascii_lowercase() };
            transport(&controller, transport_action).await.map_err(service_error)?;
        }
        _ => return Err(bad_request(format!("Unsupported SOAP action: {action}"))),
    }

    let fallback = if matches!(action, "GetVolume" | "GetMute" | "SetVolume" | "SetMute") {
        "urn:schemas-upnp-org:service:RenderingControl:1"
    } else {
        "urn:schemas-upnp-org:service:AVTransport:1"
    };
    let namespace = if service.is_empty() { fallback } else { service };
    let fields = values
        .iter()
        .map(|(key, value)| format!("<{key}>{}</{key}>", xml_escape(value)))
        .collect::<String>();
    let envelope = format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action}Response xmlns:u=\"{}\">{fields}</u:{action}Response></s:Body></s:Envelope>",
        xml_escape(namespace)
    );
    Ok(text_response(envelope, "text/xml; charset=utf-8"))
}

async fn description(State(controller): State<WebController>, headers: HeaderMap) -> Response {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("127.0.0.1:{}", controller.config.web.port));
    let udn = device_uuid();
    let linkplay = linkplay_uuid(&udn);
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
 <specVersion><major>1</major><minor>0</minor></specVersion>
 <URLBase>http://{host}/</URLBase>
 <device>
  <deviceType>{DEVICE_TYPE}</deviceType>
  <friendlyName>{NAME}</friendlyName>
  <manufacturer>Rakoit Technology(SZ) Co., Ltd.</manufacturer>
  <manufacturerURL>https://github.com/bodzey/proaudio-player-native</manufacturerURL>
  <modelDescription>LinkPlay compatible network audio renderer</modelDescription>
  <modelName>ProAudio Player</modelName><modelNumber>dev</modelNumber>
  <serialNumber>{linkplay}</serialNumber><UDN>{udn}</UDN>
  <serviceList>
   <service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType><serviceId>urn:upnp-org:serviceId:AVTransport</serviceId><SCPDURL>/upnp/service.xml</SCPDURL><controlURL>/upnp/control</controlURL><eventSubURL>/upnp/event</eventSubURL></service>
   <service><serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType><serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId><SCPDURL>/upnp/service.xml</SCPDURL><controlURL>/upnp/control</controlURL><eventSubURL>/upnp/event</eventSubURL></service>
   <service><serviceType>{WIIMU_SERVICE_TYPE}</serviceType><serviceId>urn:wiimu-com:serviceId:PlayQueue</serviceId><SCPDURL>/upnp/playqueue.xml</SCPDURL><controlURL>/upnp/control</controlURL><eventSubURL>/upnp/event</eventSubURL></service>
  </serviceList>
 </device>
</root>"#
    );
    text_response(xml, "text/xml; charset=utf-8")
}

async fn service_description() -> Response {
    let actions = [
        "GetTransportInfo",
        "GetPositionInfo",
        "GetMediaInfo",
        "Play",
        "Pause",
        "Stop",
        "Next",
        "Previous",
        "GetVolume",
        "SetVolume",
        "GetMute",
        "SetMute",
    ];
    let action_list = actions
        .iter()
        .map(|name| format!("<action><name>{name}</name></action>"))
        .collect::<String>();
    text_response(
        format!("<?xml version=\"1.0\"?><scpd xmlns=\"urn:schemas-upnp-org:service-1-0\"><specVersion><major>1</major><minor>0</minor></specVersion><actionList>{action_list}</actionList><serviceStateTable></serviceStateTable></scpd>"),
        "text/xml; charset=utf-8",
    )
}

pub fn router() -> Router<WebController> {
    Router::new()
        .route("/httpapi.asp", get(http_api))
        .route("/upnp/device.xml", get(description))
        .route("/description.xml", get(description))
        .route("/upnp/service.xml", get(service_description))
        .route("/upnp/playqueue.xml", get(service_description))
        .route("/upnp/control", post(soap_control))
}

fn alive_messages(port: u16, udn: &str) -> Vec<Vec<u8>> {
    let location = format!("http://{}:{port}/upnp/device.xml", local_ip(None));
    [
        ("upnp:rootdevice".to_owned(), format!("{udn}::upnp:rootdevice")),
        (udn.to_owned(), udn.to_owned()),
        (DEVICE_TYPE.to_owned(), format!("{udn}::{DEVICE_TYPE}")),
        (WIIMU_SERVICE_TYPE.to_owned(), format!("{udn}::{WIIMU_SERVICE_TYPE}")),
    ]
    .into_iter()
    .map(|(notification_type, usn)| {
        [
            "NOTIFY * HTTP/1.1".into(),
            format!("HOST: {SSDP_ADDRESS}:{SSDP_PORT}"),
            "CACHE-CONTROL: max-age=120".into(),
            format!("LOCATION: {location}"),
            format!("NT: {notification_type}"),
            "NTS: ssdp:alive".into(),
            "SERVER: Linux/5.x UPnP/1.0 Linkplay/4.8 ProAudioPlayer/dev".into(),
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
    let mut headers = HashMap::new();
    for line in message.lines().skip(1) {
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_uppercase(), value.trim().to_owned());
        }
    }
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
        && requested != WIIMU_SERVICE_TYPE
    {
        return None;
    }
    let st = if requested == "ssdp:all" { DEVICE_TYPE } else { requested };
    let usn = if st == udn { udn.to_owned() } else { format!("{udn}::{st}") };
    Some(
        [
            "HTTP/1.1 200 OK".into(),
            "CACHE-CONTROL: max-age=120".into(),
            "EXT:".into(),
            format!("LOCATION: http://{}:{port}/upnp/device.xml", local_ip(Some(peer))),
            "SERVER: Linux/5.x UPnP/1.0 Linkplay/4.8 ProAudioPlayer/dev".into(),
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
    let std_socket: StdUdpSocket = socket.into();
    let socket = UdpSocket::from_std(std_socket)?;
    let udn = device_uuid();
    let destination = SocketAddr::V4(SocketAddrV4::new(multicast, SSDP_PORT));
    let mut ticker = interval(Duration::from_secs(30));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut buffer = vec![0u8; 8192];
    info!("4STREAM compatibility discovery active on SSDP");

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                for message in alive_messages(port, &udn) {
                    if let Err(err) = socket.send_to(&message, destination).await {
                        debug!("4STREAM ssdp:alive send failed: {err}");
                    }
                }
            }
            received = socket.recv_from(&mut buffer) => {
                match received {
                    Ok((length, peer)) => {
                        let message = String::from_utf8_lossy(&buffer[..length]);
                        if let Some(response) = ssdp_response(&message, peer, port, &udn) {
                            if let Err(err) = socket.send_to(&response, peer).await {
                                debug!("4STREAM SSDP reply failed: {err}");
                            }
                        }
                    }
                    Err(err) => {
                        warn!("4STREAM SSDP receive failed: {err}");
                    }
                }
            }
        }
    }
}
