use std::process::Stdio;
use std::time::Instant;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::StreamExt;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

use super::WebController;

const SUBPROTOCOL: &str = "proaudio-pcm-f32le-48000-stereo-v1";
const PACAT_LATENCY_MSEC: &str = "80";
const PACAT_PROCESS_MSEC: &str = "20";
const MAX_FRAME_BYTES: usize = 256 * 1024;

static NETWORK_STREAM: Semaphore = Semaphore::const_new(1);

fn accepts_subprotocol(headers: &HeaderMap) -> bool {
    headers
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .map(str::trim)
                .any(|protocol| protocol == SUBPROTOCOL)
        })
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

async fn upgrade(
    State(controller): State<WebController>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response {
    if !accepts_subprotocol(&headers) {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!("Потрібен WebSocket subprotocol {SUBPROTOCOL}"),
        );
    }

    let permit = match NETWORK_STREAM.try_acquire() {
        Ok(permit) => permit,
        Err(_) => {
            return json_error(
                StatusCode::CONFLICT,
                "Мережевий аудіопотік уже активний",
            );
        }
    };

    websocket
        .protocols([SUBPROTOCOL])
        .on_upgrade(move |socket| stream_audio(controller, socket, permit))
}

async fn stream_audio(
    controller: WebController,
    mut socket: WebSocket,
    _permit: SemaphorePermit<'static>,
) {
    let sink = controller.config.audio.music_sink.trim().to_owned();
    if sink.is_empty() {
        let _ = socket
            .send(Message::Close(None))
            .await;
        return;
    }

    let device = format!("--device={sink}");
    let mut child = match Command::new("pacat")
        .arg("--playback")
        .arg("--raw")
        .arg(device)
        .arg("--format=float32le")
        .arg("--rate=48000")
        .arg("--channels=2")
        .arg("--channel-map=front-left,front-right")
        .arg(format!("--latency-msec={PACAT_LATENCY_MSEC}"))
        .arg(format!("--process-time-msec={PACAT_PROCESS_MSEC}"))
        .arg("--volume=65536")
        .arg("--client-name=ProAudioNetworkInput")
        .arg("--stream-name=proaudio-network-input")
        .arg("--property=application.name=ProAudioNetworkInput")
        .arg("--property=media.name=ProAudio Network Audio")
        .arg("--property=media.role=music")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            warn!(%error, "Не вдалося запустити network audio sink");
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };

    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.start_kill();
        let _ = socket.send(Message::Close(None)).await;
        return;
    };

    let started = Instant::now();
    let mut bytes_received: u64 = 0;
    info!(sink, "Мережевий PCM WebSocket підключено");

    while let Some(message) = socket.next().await {
        match message {
            Ok(Message::Binary(data)) => {
                if data.is_empty() {
                    continue;
                }
                if data.len() > MAX_FRAME_BYTES {
                    warn!(bytes = data.len(), "Network audio frame перевищує ліміт");
                    break;
                }
                if stdin.write_all(&data).await.is_err() {
                    warn!("Network audio playback process перестав приймати PCM");
                    break;
                }
                bytes_received = bytes_received.saturating_add(data.len() as u64);
            }
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(_))
            | Ok(Message::Pong(_))
            | Ok(Message::Text(_)) => {}
            Err(error) => {
                warn!(%error, "Помилка WebSocket мережевого аудіопотоку");
                break;
            }
        }
    }

    let _ = stdin.shutdown().await;
    drop(stdin);

    match timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) if status.success() => {}
        Ok(Ok(status)) => {
            warn!(%status, "Network audio sink завершився з помилкою");
        }
        Ok(Err(error)) => {
            warn!(%error, "Не вдалося дочекатися network audio sink");
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }

    info!(
        bytes_received,
        elapsed_ms = started.elapsed().as_millis(),
        "Мережевий PCM WebSocket завершено"
    );
}

pub(super) fn router() -> Router<WebController> {
    Router::new().route("/audio/network", get(upgrade))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_canonical_pcm_subprotocol() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "sec-websocket-protocol",
            "other, proaudio-pcm-f32le-48000-stereo-v1"
                .parse()
                .unwrap(),
        );
        assert!(accepts_subprotocol(&headers));

        headers.insert("sec-websocket-protocol", "other".parse().unwrap());
        assert!(!accepts_subprotocol(&headers));
    }
}
