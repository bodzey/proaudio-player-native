use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, Mutex};
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

use super::WebController;

const CONTENT_TYPE: &str = "application/x-proaudio-pcm";
const SAMPLE_FORMAT: &str = "float32le";
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u8 = 2;
const BYTES_PER_FRAME: usize = CHANNELS as usize * std::mem::size_of::<f32>();
const MAX_CHUNK_BYTES: usize = 128 * 1024;
const SESSION_IDLE_SECONDS: u64 = 4;
const PACAT_LATENCY_MSEC: &str = "80";
const PACAT_PROCESS_MSEC: &str = "20";

static SESSION_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static NETWORK_SESSION: LazyLock<Arc<Mutex<Option<SessionHandle>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(None)));

#[derive(Clone)]
struct SessionHandle {
    id: u64,
    sender: mpsc::Sender<Vec<u8>>,
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn session_id(headers: &HeaderMap) -> Result<u64, &'static str> {
    headers
        .get("x-proaudio-session")
        .and_then(|value| value.to_str().ok())
        .ok_or("Відсутній X-ProAudio-Session")?
        .parse::<u64>()
        .map_err(|_| "Некоректний X-ProAudio-Session")
}

fn valid_pcm_content_type(headers: &HeaderMap) -> bool {
    headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        == Some(CONTENT_TYPE)
}

fn spawn_pacat(sink: &str) -> std::io::Result<(Child, ChildStdin)> {
    let device = format!("--device={sink}");
    let mut child = Command::new("pacat")
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
        .spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("pacat stdin unavailable"))?;
    Ok((child, stdin))
}

async fn playback_task(
    session_id: u64,
    mut receiver: mpsc::Receiver<Vec<u8>>,
    mut child: Child,
    mut stdin: ChildStdin,
) {
    let mut bytes_received: u64 = 0;
    loop {
        match timeout(Duration::from_secs(SESSION_IDLE_SECONDS), receiver.recv()).await {
            Ok(Some(data)) => {
                if let Err(error) = stdin.write_all(&data).await {
                    warn!(%error, "Network audio playback process перестав приймати PCM");
                    break;
                }
                bytes_received = bytes_received.saturating_add(data.len() as u64);
            }
            Ok(None) => break,
            Err(_) => {
                info!(session_id, "Network audio session завершено через idle timeout");
                break;
            }
        }
    }

    let _ = stdin.shutdown().await;
    drop(stdin);

    match timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) if status.success() => {}
        Ok(Ok(status)) => warn!(%status, "Network audio sink завершився з помилкою"),
        Ok(Err(error)) => warn!(%error, "Не вдалося дочекатися network audio sink"),
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }

    let mut active = NETWORK_SESSION.lock().await;
    if active.as_ref().is_some_and(|session| session.id == session_id) {
        *active = None;
    }
    info!(session_id, bytes_received, "Network audio session завершено");
}

async fn start(State(controller): State<WebController>) -> Response {
    let mut active = NETWORK_SESSION.lock().await;
    if active.is_some() {
        return json_error(
            StatusCode::CONFLICT,
            "Мережевий аудіопотік уже активний",
        );
    }

    let sink = controller.config.audio.music_sink.trim();
    if sink.is_empty() {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "MUSIC sink не налаштовано");
    }

    let (child, stdin) = match spawn_pacat(sink) {
        Ok(process) => process,
        Err(error) => {
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Не вдалося запустити network audio sink: {error}"),
            );
        }
    };

    let session_id = SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = mpsc::channel::<Vec<u8>>(4);
    *active = Some(SessionHandle {
        id: session_id,
        sender,
    });
    drop(active);

    tokio::spawn(playback_task(session_id, receiver, child, stdin));
    info!(session_id, sink, "Network audio session створено");

    Json(json!({
        "session_id": session_id,
        "sample_format": SAMPLE_FORMAT,
        "sample_rate": SAMPLE_RATE,
        "channels": CHANNELS,
        "recommended_chunk_millis": 100,
        "idle_timeout_seconds": SESSION_IDLE_SECONDS,
    }))
    .into_response()
}

async fn frame(headers: HeaderMap, body: Body) -> Response {
    if !valid_pcm_content_type(&headers) {
        return json_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type має бути application/x-proaudio-pcm",
        );
    }

    let requested_session = match session_id(&headers) {
        Ok(value) => value,
        Err(message) => return json_error(StatusCode::BAD_REQUEST, message),
    };

    let bytes = match to_bytes(body, MAX_CHUNK_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "PCM frame перевищує 128 KiB",
            );
        }
    };
    if bytes.is_empty() || bytes.len() % BYTES_PER_FRAME != 0 {
        return json_error(
            StatusCode::BAD_REQUEST,
            "PCM frame має містити цілі stereo float32 frames",
        );
    }

    let sender = {
        let active = NETWORK_SESSION.lock().await;
        match active.as_ref() {
            Some(session) if session.id == requested_session => session.sender.clone(),
            _ => {
                return json_error(
                    StatusCode::NOT_FOUND,
                    "Network audio session не знайдено",
                );
            }
        }
    };

    match timeout(Duration::from_secs(1), sender.send(bytes.to_vec())).await {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(_)) => json_error(
            StatusCode::GONE,
            "Network audio session вже завершено",
        ),
        Err(_) => json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Network audio sender випереджає відтворення",
        ),
    }
}

async fn stop(headers: HeaderMap) -> Response {
    let requested_session = match session_id(&headers) {
        Ok(value) => value,
        Err(message) => return json_error(StatusCode::BAD_REQUEST, message),
    };

    let mut active = NETWORK_SESSION.lock().await;
    match active.as_ref() {
        Some(session) if session.id == requested_session => {
            *active = None;
            StatusCode::NO_CONTENT.into_response()
        }
        _ => json_error(
            StatusCode::NOT_FOUND,
            "Network audio session не знайдено",
        ),
    }
}

pub(super) fn router() -> Router<WebController> {
    Router::new()
        .route("/audio/network/start", post(start))
        .route("/audio/network/frame", post(frame))
        .route("/audio/network/stop", post(stop))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_pcm_frame_shape() {
        assert_eq!(BYTES_PER_FRAME, 8);
        assert_eq!((48_000usize * 2 * 4) / 10, 38_400);
        assert!(38_400 < MAX_CHUNK_BYTES);
    }

    #[test]
    fn accepts_only_pcm_content_type() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", CONTENT_TYPE.parse().unwrap());
        assert!(valid_pcm_content_type(&headers));

        headers.insert("content-type", "audio/wav".parse().unwrap());
        assert!(!valid_pcm_content_type(&headers));
    }
}
