use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, bail, Context, Result};
use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::Json;
use serde_json::{json, Value};

use crate::config::effective_audio;

use super::{api_error, map_bad_request, map_internal, ApiResult, WebController};

pub(super) const MAX_ALERT_MEDIA_BYTES: usize = 16 * 1024 * 1024;
const MIN_ALERT_MEDIA_BYTES: usize = 512;

struct AlertMediaSpec {
    kind: &'static str,
    label: &'static str,
    file_name: &'static str,
    active_path: PathBuf,
    factory_path: PathBuf,
}

fn alert_media_spec(controller: &WebController, kind: &str) -> Result<AlertMediaSpec> {
    let (audio, minute) =
        effective_audio(&controller.config.audio, &controller.config.minute_silence)?;
    let (kind, label, file_name, active_path) = match kind {
        "alarm_start" => (
            "alarm_start",
            "Повітряна тривога",
            "alarm_start.mp3",
            audio.start_file,
        ),
        "alarm_end" => (
            "alarm_end",
            "Відбій тривоги",
            "alarm_end.mp3",
            audio.end_file,
        ),
        "minute_silence" => (
            "minute_silence",
            "Хвилина мовчання",
            "minute_silence.mp3",
            minute.file,
        ),
        _ => bail!("Невідомий тип аудіосповіщення"),
    };
    Ok(AlertMediaSpec {
        kind,
        label,
        file_name,
        active_path,
        factory_path: Path::new("/usr/share/proaudio-player/announcements").join(file_name),
    })
}

fn mp3_frame_header(bytes: &[u8]) -> bool {
    bytes.len() >= 4
        && bytes[0] == 0xff
        && bytes[1] & 0xe0 == 0xe0
        && bytes[1] & 0x18 != 0x08
        && bytes[1] & 0x06 != 0
        && bytes[2] & 0xf0 != 0
        && bytes[2] & 0xf0 != 0xf0
        && bytes[2] & 0x0c != 0x0c
}

fn looks_like_mp3(bytes: &[u8]) -> bool {
    if bytes.len() < MIN_ALERT_MEDIA_BYTES {
        return false;
    }

    let start = if bytes.starts_with(b"ID3") && bytes.len() >= 10 {
        let size = bytes[6..10]
            .iter()
            .try_fold(0usize, |value, byte| {
                (*byte < 0x80).then_some((value << 7) | usize::from(*byte))
            });
        match size.and_then(|value| value.checked_add(10)) {
            Some(value) if value < bytes.len() => value,
            _ => return false,
        }
    } else {
        0
    };

    bytes[start..]
        .windows(4)
        .take(64 * 1024)
        .filter(|header| mp3_frame_header(header))
        .take(2)
        .count()
        == 2
}

fn alert_media_value(spec: &AlertMediaSpec) -> Value {
    let metadata = fs::metadata(&spec.active_path).ok();
    let modified_unix_seconds = metadata
        .as_ref()
        .and_then(|value| value.modified().ok())
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_secs());

    json!({
        "kind": spec.kind,
        "label": spec.label,
        "file_name": spec.file_name,
        "configured": metadata.as_ref().is_some_and(|value| value.is_file()),
        "size_bytes": metadata.as_ref().map(|value| value.len()),
        "modified_unix_seconds": modified_unix_seconds,
        "max_size_bytes": MAX_ALERT_MEDIA_BYTES,
        "content_type": "audio/mpeg",
    })
}

pub(super) async fn get_alert_media(State(controller): State<WebController>) -> ApiResult {
    let items = ["alarm_start", "alarm_end", "minute_silence"]
        .into_iter()
        .map(|kind| {
            alert_media_spec(&controller, kind)
                .map(|spec| alert_media_value(&spec))
                .map_err(map_internal)
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(Json(json!({
        "items": items,
        "accepted_content_types": ["audio/mpeg", "audio/mp3"],
        "max_size_bytes": MAX_ALERT_MEDIA_BYTES,
    })))
}

pub(super) async fn put_alert_media(
    State(controller): State<WebController>,
    AxumPath(kind): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    if let Some(content_type) = headers.get(header::CONTENT_TYPE) {
        let content_type = content_type
            .to_str()
            .map_err(|_| api_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Некоректний Content-Type"))?
            .split(';')
            .next()
            .unwrap_or_default()
            .trim();

        if !matches!(
            content_type,
            "audio/mpeg" | "audio/mp3" | "application/octet-stream"
        ) {
            return Err(api_error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Підтримуються лише MP3-файли",
            ));
        }
    }

    if body.len() > MAX_ALERT_MEDIA_BYTES {
        return Err(api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "MP3-файл перевищує дозволені 16 MiB",
        ));
    }

    if !looks_like_mp3(&body) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Файл не схожий на коректний MP3",
        ));
    }

    let spec = alert_media_spec(&controller, &kind).map_err(map_bad_request)?;
    let target = spec.active_path.clone();
    let payload = body.to_vec();
    tokio::task::spawn_blocking(move || crate::atomic_file::write(&target, &payload, 0o644))
        .await
        .map_err(|error| map_internal(anyhow!("Збій запису MP3: {error}")))?
        .map_err(map_internal)?;

    Ok(Json(alert_media_value(&spec)))
}

pub(super) async fn reset_alert_media(
    State(controller): State<WebController>,
    AxumPath(kind): AxumPath<String>,
) -> ApiResult {
    let spec = alert_media_spec(&controller, &kind).map_err(map_bad_request)?;
    let payload = tokio::fs::read(&spec.factory_path)
        .await
        .with_context(|| {
            format!(
                "Не вдалося прочитати заводський файл {}",
                spec.factory_path.display()
            )
        })
        .map_err(map_internal)?;

    if !looks_like_mp3(&payload) {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Заводський файл сповіщення пошкоджений",
        ));
    }

    let target = spec.active_path.clone();
    tokio::task::spawn_blocking(move || crate::atomic_file::write(&target, &payload, 0o644))
        .await
        .map_err(|error| map_internal(anyhow!("Збій відновлення MP3: {error}")))?
        .map_err(map_internal)?;

    Ok(Json(alert_media_value(&spec)))
}

#[cfg(test)]
mod tests {
    use super::{looks_like_mp3, MIN_ALERT_MEDIA_BYTES};

    #[test]
    fn accepts_mp3_frames_and_rejects_arbitrary_data() {
        let mut mp3 = vec![0_u8; MIN_ALERT_MEDIA_BYTES];
        mp3[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
        mp3[256..260].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);

        assert!(looks_like_mp3(&mp3));
        assert!(!looks_like_mp3(&[0_u8; MIN_ALERT_MEDIA_BYTES]));
        assert!(!looks_like_mp3(&[0xff, 0xfb, 0x90, 0x64]));
    }

    #[test]
    fn accepts_id3_before_the_first_audio_frame() {
        let mut mp3 = vec![0_u8; MIN_ALERT_MEDIA_BYTES];
        mp3[..10].copy_from_slice(b"ID3\x04\x00\x00\x00\x00\x00\x00");
        mp3[10..14].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
        mp3[266..270].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);

        assert!(looks_like_mp3(&mp3));
    }
}
