use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use url::Url;

use crate::radio_directory;

use super::{
    api_error, map_bad_request, map_conflict, map_internal, ApiResult, WebController,
    PLAYER_ACTIONS,
};

pub(super) fn routes() -> Router<WebController> {
    Router::new()
        .route("/player", post(player))
        .route("/library", get(library))
        .route("/library/update", post(refresh_library))
        .route("/library/play", post(play_file))
        .route("/streams/play", post(play_stream))
        .route("/radio/stations", get(radio_stations))
        .route("/playlists", get(playlists))
        .route("/playlists/load", post(load_playlist))
        .route("/queue", get(queue))
        .route("/queue/play", post(play_queue))
        .route("/queue/remove", post(remove_queue))
        .route("/queue/clear", post(clear_queue))
}

#[derive(Deserialize)]
struct PlayerBody {
    action: String,
}
#[derive(Deserialize)]
struct PathBody {
    path: String,
}
#[derive(Deserialize)]
struct PlaylistBody {
    name: String,
}
#[derive(Deserialize)]
struct StreamBody {
    url: String,
}
#[derive(Deserialize)]
struct QueueBody {
    position: u32,
}

fn safe_mpd_path(value: &str, playlist: bool) -> Result<String> {
    let value = value.trim();
    let path = Path::new(value);
    if value.is_empty()
        || value.contains('\0')
        || value.contains('\n')
        || value.starts_with('-')
        || path.is_absolute()
        || path
            .components()
            .any(|component| component.as_os_str() == "..")
        || (playlist && value.contains('/'))
    {
        bail!("Некоректний шлях");
    }
    Ok(value.to_owned())
}

async fn player(
    State(controller): State<WebController>,
    Json(body): Json<PlayerBody>,
) -> ApiResult {
    if !PLAYER_ACTIONS.contains(&body.action.as_str()) {
        return Err(api_error(StatusCode::BAD_REQUEST, "Невідома дія"));
    }
    controller
        .ensure_controls_available()
        .await
        .map_err(map_conflict)?;
    controller
        .control_active_player(&body.action)
        .await
        .map(Json)
        .map_err(map_internal)
}

async fn library(State(controller): State<WebController>) -> ApiResult {
    controller
        .library()
        .await
        .map(|items| Json(json!({ "items": items })))
        .map_err(map_internal)
}

async fn refresh_library(State(controller): State<WebController>) -> ApiResult {
    controller
        .ensure_controls_available()
        .await
        .map_err(map_conflict)?;
    controller
        .run("mpc", &["update"], true, 60)
        .await
        .map_err(map_internal)?;
    Ok(Json(json!({ "updating": true })))
}

async fn play_file(
    State(controller): State<WebController>,
    Json(body): Json<PathBody>,
) -> ApiResult {
    let path = safe_mpd_path(&body.path, false).map_err(map_bad_request)?;
    controller
        .ensure_controls_available()
        .await
        .map_err(map_conflict)?;
    if !controller
        .library()
        .await
        .map_err(map_internal)?
        .contains(&path)
    {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "Файл відсутній у бібліотеці",
        ));
    }
    controller
        .run("mpc", &["clear"], true, 8)
        .await
        .map_err(map_internal)?;
    controller
        .run("mpc", &["add", &path], true, 8)
        .await
        .map_err(map_internal)?;
    controller
        .run("mpc", &["play"], true, 8)
        .await
        .map_err(map_internal)?;
    Ok(Json(json!({ "playing": path })))
}

fn unsafe_ipv4_stream_address(value: Ipv4Addr) -> bool {
    value.is_private()
        || value.is_loopback()
        || value.is_link_local()
        || value.is_multicast()
        || value == Ipv4Addr::UNSPECIFIED
        || value.octets()[0] == 0
}

fn unsafe_stream_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(value) => unsafe_ipv4_stream_address(value),
        IpAddr::V6(value) => {
            if let Some(mapped) = value.to_ipv4_mapped() {
                return unsafe_ipv4_stream_address(mapped);
            }
            value.is_loopback()
                || value.is_unspecified()
                || value.is_multicast()
                || value.is_unique_local()
                || value.is_unicast_link_local()
                || value == Ipv6Addr::UNSPECIFIED
        }
    }
}

async fn validate_stream_url(value: &str) -> Result<String> {
    let value = value.trim();
    let parsed = Url::parse(value).context("Некоректна адреса потоку")?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        bail!("Підтримуються лише HTTP/HTTPS-потоки без облікових даних у URL");
    }

    let host = parsed.host_str().unwrap_or_default();
    let host_lower = host.to_ascii_lowercase();
    if matches!(host_lower.as_str(), "localhost" | "localhost.localdomain")
        || host_lower.ends_with(".local")
    {
        bail!("Локальні адреси потоків заборонені");
    }

    if let Ok(address) = host.parse::<IpAddr>() {
        if unsafe_stream_address(address) {
            bail!("Локальні та службові IP-адреси потоків заборонені");
        }
    } else {
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| anyhow!("Не вдалося визначити порт потоку"))?;
        let addresses = tokio::net::lookup_host((host, port))
            .await
            .context("Не вдалося визначити IP-адресу потоку")?
            .map(|socket| socket.ip())
            .collect::<BTreeSet<_>>();
        if addresses.is_empty() {
            bail!("Домен потоку не має IP-адрес");
        }
        if addresses.into_iter().any(unsafe_stream_address) {
            bail!("Домен потоку резолвиться у локальну або службову IP-адресу");
        }
    }

    Ok(parsed.to_string())
}

async fn radio_stations() -> ApiResult {
    radio_directory::ukrainian_stations()
        .await
        .map(|items| Json(json!({ "source": "radio-browser", "items": items })))
        .map_err(map_internal)
}

async fn play_stream(
    State(controller): State<WebController>,
    Json(body): Json<StreamBody>,
) -> ApiResult {
    let url = validate_stream_url(&body.url)
        .await
        .map_err(map_bad_request)?;
    controller
        .ensure_controls_available()
        .await
        .map_err(map_conflict)?;
    controller
        .run("mpc", &["clear"], true, 8)
        .await
        .map_err(map_internal)?;
    controller
        .run("mpc", &["add", &url], true, 15)
        .await
        .map_err(map_internal)?;
    controller
        .run("mpc", &["play"], true, 8)
        .await
        .map_err(map_internal)?;
    Ok(Json(json!({ "playing": url, "source": "network_stream" })))
}

async fn playlists(State(controller): State<WebController>) -> ApiResult {
    controller
        .playlists()
        .await
        .map(|items| Json(json!({ "items": items })))
        .map_err(map_internal)
}

async fn load_playlist(
    State(controller): State<WebController>,
    Json(body): Json<PlaylistBody>,
) -> ApiResult {
    let name = safe_mpd_path(&body.name, true).map_err(map_bad_request)?;
    controller
        .ensure_controls_available()
        .await
        .map_err(map_conflict)?;
    if !controller
        .playlists()
        .await
        .map_err(map_internal)?
        .contains(&name)
    {
        return Err(api_error(StatusCode::NOT_FOUND, "Плейліст не знайдено"));
    }
    controller
        .run("mpc", &["clear"], true, 8)
        .await
        .map_err(map_internal)?;
    controller
        .run("mpc", &["load", &name], true, 8)
        .await
        .map_err(map_internal)?;
    controller
        .run("mpc", &["play"], true, 8)
        .await
        .map_err(map_internal)?;
    Ok(Json(json!({ "playing_playlist": name })))
}

async fn queue(State(controller): State<WebController>) -> ApiResult {
    controller
        .queue()
        .await
        .map(|items| Json(json!({ "items": items })))
        .map_err(map_internal)
}

async fn play_queue(
    State(controller): State<WebController>,
    Json(body): Json<QueueBody>,
) -> ApiResult {
    controller
        .ensure_controls_available()
        .await
        .map_err(map_conflict)?;
    if body.position == 0
        || !controller
            .queue()
            .await
            .map_err(map_internal)?
            .iter()
            .any(|item| item.get("position").and_then(Value::as_u64) == Some(body.position as u64))
    {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "Позицію у черзі не знайдено",
        ));
    }
    let position = body.position.to_string();
    controller
        .run("mpc", &["play", &position], true, 8)
        .await
        .map_err(map_internal)?;
    Ok(Json(json!({ "playing_position": body.position })))
}

async fn remove_queue(
    State(controller): State<WebController>,
    Json(body): Json<QueueBody>,
) -> ApiResult {
    controller
        .ensure_controls_available()
        .await
        .map_err(map_conflict)?;
    if body.position == 0
        || !controller
            .queue()
            .await
            .map_err(map_internal)?
            .iter()
            .any(|item| item.get("position").and_then(Value::as_u64) == Some(body.position as u64))
    {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "Позицію у черзі не знайдено",
        ));
    }
    let position = body.position.to_string();
    controller
        .run("mpc", &["del", &position], true, 8)
        .await
        .map_err(map_internal)?;
    Ok(Json(json!({ "removed_position": body.position })))
}

async fn clear_queue(State(controller): State<WebController>) -> ApiResult {
    controller
        .ensure_controls_available()
        .await
        .map_err(map_conflict)?;
    controller
        .run("mpc", &["clear"], true, 8)
        .await
        .map_err(map_internal)?;
    Ok(Json(json!({ "cleared": true })))
}

#[cfg(test)]
mod tests {
    #[test]
    fn stream_url_guard_rejects_ipv4_mapped_private_ipv6() {
        assert!(super::unsafe_stream_address(
            "::ffff:192.168.1.10".parse().unwrap()
        ));
        assert!(super::unsafe_stream_address(
            "::ffff:127.0.0.1".parse().unwrap()
        ));
        assert!(!super::unsafe_stream_address(
            "::ffff:8.8.8.8".parse().unwrap()
        ));
    }
}
