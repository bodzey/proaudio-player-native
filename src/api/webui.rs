use std::env;
use std::path::{Component, Path, PathBuf};

use axum::body::Body;
use axum::extract::Path as AxumPath;
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use super::WebController;

const DEFAULT_WEBUI_DIR: &str = "/usr/share/proaudio-player/webui";
const WEBUI_DIR_ENV: &str = "PROAUDIO_WEBUI_DIR";

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

async fn static_asset(AxumPath(path): AxumPath<String>) -> Response {
    serve_asset(&format!("static/{path}")).await
}

pub(super) fn router() -> Router<WebController> {
    Router::<WebController>::new()
        .route("/", get(index))
        .route("/manifest.webmanifest", get(manifest))
        .route("/sw.js", get(service_worker))
        .route("/static/{*path}", get(static_asset))
}

#[cfg(test)]
mod tests {
    use super::safe_relative_path;

    #[test]
    fn accepts_only_relative_asset_paths() {
        assert!(safe_relative_path("static/app.js").is_some());
        assert!(safe_relative_path("assets/icons/player.svg").is_some());
        assert!(safe_relative_path("../etc/passwd").is_none());
        assert!(safe_relative_path("static/../../etc/passwd").is_none());
        assert!(safe_relative_path("/etc/passwd").is_none());
        assert!(safe_relative_path("").is_none());
    }
}
