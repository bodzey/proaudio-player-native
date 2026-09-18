use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use axum::body::Body;
use axum::extract::Path as AxumPath;
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use super::WebController;

const DEFAULT_WEBUI_DIR: &str = "/usr/share/proaudio-player/webui";
const WEBUI_DIR_ENV: &str = "PROAUDIO_WEBUI_DIR";
const RELEASE_FILE: &str = "/etc/proaudio-release";
const THERMAL_ROOT: &str = "/sys/class/thermal";

fn webui_root() -> Option<PathBuf> {
    let value = env::var(WEBUI_DIR_ENV).unwrap_or_else(|_| DEFAULT_WEBUI_DIR.to_owned());
    let value = value.trim();
    let disabled = matches!(
        value.to_ascii_lowercase().as_str(),
        "" | "0" | "off" | "false" | "disabled" | "none"
    );
    (!disabled).then(|| PathBuf::from(value))
}

fn safe_relative_path(value: &str) -> Option<&Path> {
    let path = Path::new(value);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(path)
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|value| value.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("webmanifest") => "application/manifest+json; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn release_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (candidate, value) = line.split_once('=')?;
        (candidate == key).then(|| value.trim().to_owned())
    })
}

static RELEASE_INFO: LazyLock<Value> = LazyLock::new(|| {
    let text = fs::read_to_string(RELEASE_FILE).unwrap_or_default();
    json!({
        "version": release_value(&text, "PROAUDIO_VERSION"),
        "channel": release_value(&text, "PROAUDIO_CHANNEL"),
        "status": release_value(&text, "PROAUDIO_STATUS"),
        "build_id": release_value(&text, "PROAUDIO_BUILD_ID"),
        "firmware_sha": release_value(&text, "PROAUDIO_FIRMWARE_SHA"),
        "native_sha": release_value(&text, "PROAUDIO_NATIVE_SHA"),
        "webui_sha": release_value(&text, "PROAUDIO_WEBUI_SHA"),
    })
});

fn release_info() -> Value {
    RELEASE_INFO.clone()
}

fn temperature_celsius_from_millidegrees(text: &str) -> Option<f64> {
    let value = text.trim().parse::<f64>().ok()? / 1000.0;
    value.is_finite().then_some(value)
}

fn cpu_temperature_celsius() -> Option<f64> {
    let entries = fs::read_dir(THERMAL_ROOT).ok()?;
    let mut fallback = None;

    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("thermal_zone") {
            continue;
        }
        let path = entry.path();
        let Ok(raw_temp) = fs::read_to_string(path.join("temp")) else {
            continue;
        };
        let Some(temp) = temperature_celsius_from_millidegrees(&raw_temp) else {
            continue;
        };
        let kind = fs::read_to_string(path.join("type"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        if kind.contains("cpu") || kind.contains("soc") || kind.contains("package") {
            return Some(temp);
        }
        fallback.get_or_insert(temp);
    }

    fallback
}

async fn system_info() -> Json<Value> {
    Json(json!({
        "temperature_celsius": cpu_temperature_celsius(),
        "native_version": env!("CARGO_PKG_VERSION"),
        "release": release_info(),
    }))
}

async fn serve_asset(relative: &str) -> Response {
    let Some(root) = webui_root() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(relative) = safe_relative_path(relative) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = root.join(relative);
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let mut response = Response::new(Body::from(bytes));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(&path)),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response.headers_mut().insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn index() -> Response {
    serve_asset("index.html").await
}

async fn manifest() -> Response {
    serve_asset("manifest.webmanifest").await
}

async fn icon() -> Response {
    serve_asset("icon.svg").await
}

async fn service_worker() -> Response {
    let mut response = serve_asset("sw.js").await;
    if response.status().is_success() {
        response.headers_mut().insert(
            HeaderName::from_static("service-worker-allowed"),
            HeaderValue::from_static("/"),
        );
    }
    response
}

async fn bundled_asset(AxumPath(path): AxumPath<String>) -> Response {
    serve_asset(&format!("assets/{path}")).await
}

async fn legacy_static_asset(AxumPath(path): AxumPath<String>) -> Response {
    serve_asset(&format!("static/{path}")).await
}

pub(super) fn router() -> Router<WebController> {
    Router::<WebController>::new()
        .route("/", get(index))
        .route("/system-info.json", get(system_info))
        .route("/manifest.webmanifest", get(manifest))
        .route("/icon.svg", get(icon))
        .route("/sw.js", get(service_worker))
        .route("/assets/{*path}", get(bundled_asset))
        .route("/static/{*path}", get(legacy_static_asset))
        .merge(super::meters::router())
}

#[cfg(test)]
mod tests {
    use super::{release_value, safe_relative_path, temperature_celsius_from_millidegrees};

    #[test]
    fn accepts_only_relative_asset_paths() {
        assert!(safe_relative_path("assets/app.js").is_some());
        assert!(safe_relative_path("static/app.js").is_some());
        assert!(safe_relative_path("assets/icons/player.svg").is_some());
        assert!(safe_relative_path("../etc/passwd").is_none());
        assert!(safe_relative_path("assets/../../etc/passwd").is_none());
        assert!(safe_relative_path("/etc/passwd").is_none());
        assert!(safe_relative_path("").is_none());
    }

    #[test]
    fn parses_release_identity_without_shelling_out() {
        let release = "PROAUDIO_VERSION=0.1.0\nPROAUDIO_CHANNEL=development\n";
        assert_eq!(release_value(release, "PROAUDIO_VERSION").as_deref(), Some("0.1.0"));
        assert_eq!(
            release_value(release, "PROAUDIO_CHANNEL").as_deref(),
            Some("development")
        );
        assert_eq!(release_value(release, "MISSING"), None);
    }

    #[test]
    fn converts_linux_thermal_millidegrees() {
        assert_eq!(temperature_celsius_from_millidegrees("54530\n"), Some(54.53));
        assert_eq!(temperature_celsius_from_millidegrees("invalid"), None);
    }
}
