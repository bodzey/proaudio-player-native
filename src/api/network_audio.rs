use std::process::Stdio;
use std::sync::{Arc, LazyLock};

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::json;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, watch, Mutex, Semaphore};
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
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

static NETWORK_SESSION: LazyLock<Arc<Mutex<Option<SessionHandle>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(None)));
static FRAME_REQUESTS: Semaphore = Semaphore::const_new(8);

#[derive(Clone)]
struct SessionHandle {
    id: u64,
    sender: mpsc::Sender<Bytes>,
    cancel: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
}

fn random_session_id() -> Result<u64, &'static str> {
    let mut bytes = [0; 8];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "Не вдалося створити ідентифікатор мережевого потоку")?;
    // Keep the wire ID exactly representable in JavaScript numbers.
    Ok((u64::from_le_bytes(bytes) & ((1 << 53) - 1)).max(1))
}

fn valid_pcm_frame(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && bytes.len() % BYTES_PER_FRAME == 0
        && bytes.chunks_exact(4).all(|sample| {
            f32::from_le_bytes(sample.try_into().expect("four-byte PCM sample")).is_finite()
        })
}

async fn write_frame(
    stdin: &mut (impl AsyncWrite + Unpin),
    bytes: &[u8],
    cancel: &mut watch::Receiver<bool>,
) -> std::io::Result<bool> {
    tokio::select! {
        biased;
        _ = cancel.wait_for(|cancelled| *cancelled) => Ok(false),
        result = timeout(WRITE_TIMEOUT, stdin.write_all(bytes)) => {
            result.map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "PCM sink write timed out"))??;
            Ok(true)
        }
    }
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
        .is_some_and(|value| value.eq_ignore_ascii_case(CONTENT_TYPE))
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
    mut receiver: mpsc::Receiver<Bytes>,
    mut child: Child,
    mut stdin: ChildStdin,
    mut cancel: watch::Receiver<bool>,
    finished: watch::Sender<bool>,
) {
    let mut bytes_received: u64 = 0;
    loop {
        let data = tokio::select! {
            biased;
            _ = cancel.wait_for(|cancelled| *cancelled) => break,
            data = timeout(Duration::from_secs(SESSION_IDLE_SECONDS), receiver.recv()) => data,
        };
        match data {
            Ok(Some(data)) => {
                match write_frame(&mut stdin, &data, &mut cancel).await {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(error) => {
                        warn!(%error, "Network audio playback process перестав приймати PCM");
                        let _ = child.start_kill();
                        break;
                    }
                }
                bytes_received = bytes_received.saturating_add(data.len() as u64);
            }
            Ok(None) => break,
            Err(_) => {
                info!(
                    session_id,
                    "Network audio session завершено через idle timeout"
                );
                break;
            }
        }
    }

    drop(stdin);
    drop(receiver);
    if *cancel.borrow() {
        let _ = child.start_kill();
    }

    match timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) if status.success() => {}
        Ok(Ok(status)) => warn!(%status, "Network audio sink завершився з помилкою"),
        Ok(Err(error)) => warn!(%error, "Не вдалося дочекатися network audio sink"),
        Err(_) => {
            let _ = child.start_kill();
            let _ = timeout(Duration::from_secs(1), child.wait()).await;
        }
    }

    let mut active = NETWORK_SESSION.lock().await;
    if active
        .as_ref()
        .is_some_and(|session| session.id == session_id)
    {
        *active = None;
    }
    finished.send_replace(true);
    info!(
        session_id,
        bytes_received, "Network audio session завершено"
    );
}

async fn start(State(controller): State<WebController>) -> Response {
    let mut active = NETWORK_SESSION.lock().await;
    if active.is_some() {
        return json_error(StatusCode::CONFLICT, "Мережевий аудіопотік уже активний");
    }

    let sink = controller.config.audio.music_sink.trim();
    if sink.is_empty() {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "MUSIC sink не налаштовано");
    }

    let session_id = match random_session_id() {
        Ok(value) => value,
        Err(message) => return json_error(StatusCode::SERVICE_UNAVAILABLE, message),
    };

    let (child, stdin) = match spawn_pacat(sink) {
        Ok(process) => process,
        Err(error) => {
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Не вдалося запустити network audio sink: {error}"),
            );
        }
    };

    let (sender, receiver) = mpsc::channel::<Bytes>(4);
    let (cancel, cancellation) = watch::channel(false);
    let (completed, finished) = watch::channel(false);
    *active = Some(SessionHandle {
        id: session_id,
        sender,
        cancel,
        finished,
    });
    drop(active);

    tokio::spawn(playback_task(
        session_id,
        receiver,
        child,
        stdin,
        cancellation,
        completed,
    ));
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
    let Ok(_request) = FRAME_REQUESTS.try_acquire() else {
        return json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Забагато одночасних PCM frames",
        );
    };
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

    let sender = {
        let active = NETWORK_SESSION.lock().await;
        match active.as_ref() {
            Some(session) if session.id == requested_session && !*session.cancel.borrow() => {
                session.sender.clone()
            }
            _ => return json_error(StatusCode::NOT_FOUND, "Network audio session не знайдено"),
        }
    };

    let bytes = match timeout(Duration::from_secs(5), to_bytes(body, MAX_CHUNK_BYTES)).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => {
            return json_error(StatusCode::PAYLOAD_TOO_LARGE, "PCM frame перевищує 128 KiB");
        }
        Err(_) => {
            return json_error(
                StatusCode::REQUEST_TIMEOUT,
                "Перевищено час передавання PCM frame",
            )
        }
    };
    if !valid_pcm_frame(&bytes) {
        return json_error(
            StatusCode::BAD_REQUEST,
            "PCM frame має містити цілі stereo float32 frames зі скінченними значеннями",
        );
    }

    match timeout(Duration::from_secs(1), sender.send(bytes)).await {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(_)) => json_error(StatusCode::GONE, "Network audio session вже завершено"),
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

    let mut finished = {
        let active = NETWORK_SESSION.lock().await;
        match active.as_ref() {
            Some(session) if session.id == requested_session => {
                session.cancel.send_replace(true);
                session.finished.clone()
            }
            _ => return json_error(StatusCode::NOT_FOUND, "Network audio session не знайдено"),
        }
    };
    let stopped = matches!(
        timeout(
            Duration::from_secs(4),
            finished.wait_for(|complete| *complete)
        )
        .await,
        Ok(Ok(_))
    );
    if stopped {
        StatusCode::NO_CONTENT.into_response()
    } else {
        json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Не вдалося завершити network audio session",
        )
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
        let recommended_chunk_bytes = (48_000usize * 2 * 4) / 10;
        assert_eq!(BYTES_PER_FRAME, 8);
        assert_eq!(recommended_chunk_bytes, 38_400);
        assert!(recommended_chunk_bytes < MAX_CHUNK_BYTES);
        assert!(valid_pcm_frame(&vec![0; recommended_chunk_bytes]));
        assert!(!valid_pcm_frame(&[]));
        assert!(!valid_pcm_frame(&[0; 7]));
        for sample in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let bytes = [sample.to_le_bytes(), 0.0f32.to_le_bytes()].concat();
            assert!(!valid_pcm_frame(&bytes));
        }
    }

    #[test]
    fn accepts_only_pcm_content_type() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", CONTENT_TYPE.parse().unwrap());
        assert!(valid_pcm_content_type(&headers));

        headers.insert(
            "content-type",
            "Application/X-ProAudio-PCM; charset=binary"
                .parse()
                .unwrap(),
        );
        assert!(valid_pcm_content_type(&headers));

        headers.insert("content-type", "audio/wav".parse().unwrap());
        assert!(!valid_pcm_content_type(&headers));
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_blocked_pcm_writer() {
        let (mut writer, _reader) = tokio::io::duplex(1);
        let (cancel, mut cancellation) = watch::channel(false);
        let task =
            tokio::spawn(async move { write_frame(&mut writer, &[0; 8], &mut cancellation).await });
        tokio::task::yield_now().await;
        cancel.send_replace(true);
        assert!(!timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap());
    }

    #[test]
    fn session_ids_are_unpredictable_and_exact_in_browser_clients() {
        let ids = (0..32)
            .map(|_| random_session_id().unwrap())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), 32);
        assert!(ids.iter().all(|value| *value > 0 && *value < (1 << 53)));
    }
}
