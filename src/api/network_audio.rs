use std::process::Stdio;
use std::time::Instant;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures_util::StreamExt;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

use super::WebController;

const CONTENT_TYPE: &str = "application/x-proaudio-pcm";
const SAMPLE_FORMAT: &str = "float32le";
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u8 = 2;
const PACAT_LATENCY_MSEC: &str = "80";
const PACAT_PROCESS_MSEC: &str = "20";
const MAX_FRAME_BYTES: usize = 256 * 1024;

static NETWORK_STREAM: Semaphore = Semaphore::const_new(1);

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok().map(str::trim)
}

fn validate_format(headers: &HeaderMap) -> Result<(), &'static str> {
    let media_type = header_text(headers, "content-type")
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if media_type != Some(CONTENT_TYPE) {
        return Err("Content-Type має бути application/x-proaudio-pcm");
    }

    let format = header_text(headers, "x-proaudio-sample-format").unwrap_or(SAMPLE_FORMAT);
    if format != SAMPLE_FORMAT {
        return Err("Підтримується лише x-proaudio-sample-format=float32le");
    }

    let rate = header_text(headers, "x-proaudio-sample-rate")
        .unwrap_or("48000")
        .parse::<u32>()
        .map_err(|_| "Некоректний x-proaudio-sample-rate")?;
    if rate != SAMPLE_RATE {
        return Err("Підтримується лише x-proaudio-sample-rate=48000");
    }

    let channels = header_text(headers, "x-proaudio-channels")
        .unwrap_or("2")
        .parse::<u8>()
        .map_err(|_| "Некоректний x-proaudio-channels")?;
    if channels != CHANNELS {
        return Err("Підтримується лише x-proaudio-channels=2");
    }

    Ok(())
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

async fn ingest(
    State(controller): State<WebController>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if let Err(message) = validate_format(&headers) {
        return json_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, message);
    }

    let Ok(_permit) = NETWORK_STREAM.try_acquire() else {
        return json_error(
            StatusCode::CONFLICT,
            "Мережевий аудіопотік уже активний",
        );
    };

    let sink = controller.config.audio.music_sink.trim().to_owned();
    if sink.is_empty() {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "MUSIC sink не налаштовано",
        );
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
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Не вдалося запустити network audio sink: {error}"),
            );
        }
    };

    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.start_kill();
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Не вдалося відкрити stdin network audio sink",
        );
    };

    let started = Instant::now();
    let mut bytes_received: u64 = 0;
    let mut stream = body.into_data_stream();

    info!(sink, "Мережевий PCM-потік підключено");

    while let Some(frame) = stream.next().await {
        let data = match frame {
            Ok(data) => data,
            Err(error) => {
                warn!(%error, "Помилка HTTP body мережевого аудіопотоку");
                let _ = child.start_kill();
                let _ = child.wait().await;
                return json_error(StatusCode::BAD_REQUEST, "Потік перервано під час передачі");
            }
        };

        if data.len() > MAX_FRAME_BYTES {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Окремий фрагмент PCM перевищує 256 KiB",
            );
        }

        if let Err(error) = stdin.write_all(&data).await {
            warn!(%error, "Network audio playback process перестав приймати PCM");
            let _ = child.start_kill();
            let _ = child.wait().await;
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Аудіовихід перестав приймати мережевий потік",
            );
        }
        bytes_received = bytes_received.saturating_add(data.len() as u64);
    }

    let _ = stdin.shutdown().await;
    drop(stdin);

    let status = match timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Не вдалося завершити network audio sink: {error}"),
            );
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Network audio sink не завершився вчасно",
            );
        }
    };

    if !status.success() {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Network audio sink завершився з {status}"),
        );
    }

    let elapsed_ms = started.elapsed().as_millis();
    info!(bytes_received, elapsed_ms, "Мережевий PCM-потік завершено");

    Json(json!({
        "ok": true,
        "bytes_received": bytes_received,
        "duration_ms": elapsed_ms,
        "format": SAMPLE_FORMAT,
        "sample_rate": SAMPLE_RATE,
        "channels": CHANNELS,
    }))
    .into_response()
}

pub(super) fn router() -> Router<WebController> {
    Router::new().route("/audio/network", post(ingest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", CONTENT_TYPE.parse().unwrap());
        headers.insert("x-proaudio-sample-format", SAMPLE_FORMAT.parse().unwrap());
        headers.insert("x-proaudio-sample-rate", "48000".parse().unwrap());
        headers.insert("x-proaudio-channels", "2".parse().unwrap());
        headers
    }

    #[test]
    fn accepts_canonical_network_pcm_format() {
        assert!(validate_format(&headers()).is_ok());
    }

    #[test]
    fn rejects_wrong_rate_and_media_type() {
        let mut wrong_rate = headers();
        wrong_rate.insert("x-proaudio-sample-rate", "44100".parse().unwrap());
        assert!(validate_format(&wrong_rate).is_err());

        let mut wrong_type = headers();
        wrong_type.insert("content-type", "audio/wav".parse().unwrap());
        assert!(validate_format(&wrong_type).is_err());
    }
}
