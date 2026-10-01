use std::fs::File;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use url::Url;

use crate::config::ApiConfig;

const MAX_TOKEN_BYTES: usize = 256;

#[derive(Clone)]
pub(super) struct ApiAccess {
    token_digest: Option<Arc<[u8; 32]>>,
}

impl ApiAccess {
    pub(super) fn load(config: &ApiConfig) -> Result<Self> {
        let Some(path) = config.auth_token_file.as_ref() else {
            if cfg!(feature = "appliance") && config.enabled {
                bail!("закрита збірка вимагає api.auth_token_file для API керування");
            }
            return Ok(Self { token_digest: None });
        };
        let file = File::open(path).context("не вдалося прочитати токен керування")?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
            bail!("файл токена керування має бути звичайним файлом із правами 0600 або 0400");
        }
        if metadata.len() > MAX_TOKEN_BYTES as u64 + 2 {
            bail!("файл токена керування перевищує допустимий розмір");
        }
        let mut bytes = Vec::new();
        file.take(MAX_TOKEN_BYTES as u64 + 2)
            .read_to_end(&mut bytes)?;
        let token = std::str::from_utf8(&bytes)
            .context("некоректний формат токена керування")?
            .trim();
        if !(32..=MAX_TOKEN_BYTES).contains(&token.len())
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            bail!("токен керування має містити 32..256 символів: літери, цифри, '-' або '_'");
        }
        Ok(Self {
            token_digest: Some(Arc::new(Sha256::digest(token.as_bytes()).into())),
        })
    }

    pub(super) fn enabled(&self) -> bool {
        self.token_digest.is_some()
    }

    fn accepts(&self, headers: &HeaderMap) -> bool {
        let Some(expected) = self.token_digest.as_ref() else {
            return true;
        };
        let mut values = headers.get_all(header::AUTHORIZATION).iter();
        let Some(value) = values.next().and_then(|value| value.to_str().ok()) else {
            return false;
        };
        if values.next().is_some() || value.len() > 512 {
            return false;
        }
        let Some((scheme, credentials)) = value.split_once(' ') else {
            return false;
        };
        let token = if scheme.eq_ignore_ascii_case("bearer") {
            credentials.as_bytes().to_vec()
        } else if scheme.eq_ignore_ascii_case("basic") {
            let Ok(decoded) = STANDARD.decode(credentials) else {
                return false;
            };
            let Some(password) = decoded.strip_prefix(b"proaudio:") else {
                return false;
            };
            password.to_vec()
        } else {
            return false;
        };
        let actual: [u8; 32] = Sha256::digest(&token).into();
        bool::from(actual.ct_eq(expected.as_ref()))
    }
}

fn public_resource(path: &str) -> bool {
    matches!(
        path,
        "/api/health"
            | "/api/v1/health"
            | "/api/capabilities"
            | "/api/v1/capabilities"
            | "/manifest.webmanifest"
            | "/icon.svg"
            | "/sw.js"
    ) || path.starts_with("/assets/")
        || path.starts_with("/static/")
        || path.starts_with("/upnp/")
        || path == "/description.xml"
}

fn browser_origin_allowed(headers: &HeaderMap) -> bool {
    if headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| matches!(value, "cross-site" | "same-site"))
    {
        return false;
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Some(origin) = origin
        .to_str()
        .ok()
        .and_then(|value| Url::parse(value).ok())
    else {
        return false;
    };
    if !matches!(origin.scheme(), "http" | "https")
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return false;
    }
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Ok(target) = Url::parse(&format!("{}://{host}", origin.scheme())) else {
        return false;
    };
    target.username().is_empty()
        && target.password().is_none()
        && target.path() == "/"
        && target.query().is_none()
        && target.fragment().is_none()
        && origin.host_str() == target.host_str()
        && origin.port_or_known_default() == target.port_or_known_default()
}

pub(super) async fn guard(
    State(access): State<ApiAccess>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let public = public_resource(path);
    let management = !public || path.starts_with("/upnp/") || path == "/description.xml";
    let mut response = if management && !browser_origin_allowed(request.headers()) {
        (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Керування з іншого вебсайту заборонено" })),
        )
            .into_response()
    } else if !public && !access.accepts(request.headers()) {
        let mut response = (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Потрібна авторизація керування" })),
        )
            .into_response();
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"ProAudio Player\", charset=\"UTF-8\""),
        );
        response
    } else {
        next.run(request).await
    };
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
        .headers_mut()
        .insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if !public {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::middleware;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn appliance_requires_credentials_only_for_an_enabled_management_api() {
        let config = ApiConfig::default();
        assert_eq!(
            ApiAccess::load(&config).is_err(),
            cfg!(feature = "appliance")
        );
        let disabled = ApiConfig {
            enabled: false,
            ..config
        };
        assert!(!ApiAccess::load(&disabled).unwrap().enabled());
    }

    fn access() -> ApiAccess {
        ApiAccess {
            token_digest: Some(Arc::new(Sha256::digest(TOKEN.as_bytes()).into())),
        }
    }

    #[tokio::test]
    async fn protects_both_api_versions_meters_and_the_web_entry_point() {
        for path in [
            "/",
            "/api/status",
            "/api/v1/status",
            "/api/meters",
            "/api/v1/meters",
            "/system-info.json",
        ] {
            let app = Router::new()
                .route(path, get(|| async { StatusCode::NO_CONTENT }))
                .layer(middleware::from_fn_with_state(access(), guard));
            let denied = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(denied.status(), StatusCode::UNAUTHORIZED, "{path}");
            assert!(denied.headers().contains_key(header::WWW_AUTHENTICATE));
            for authorization in [
                format!("Bearer {TOKEN}"),
                format!("Basic {}", STANDARD.encode(format!("proaudio:{TOKEN}"))),
            ] {
                let accepted = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .uri(path)
                            .header(header::AUTHORIZATION, authorization)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(accepted.status(), StatusCode::NO_CONTENT, "{path}");
                assert_eq!(accepted.headers()[header::CACHE_CONTROL], "no-store");
            }
        }
    }

    #[tokio::test]
    async fn keeps_discovery_available_and_rejects_query_string_tokens() {
        let app = Router::new()
            .route("/api/v1/health", get(|| async { StatusCode::NO_CONTENT }))
            .route("/api/v1/status", get(|| async { StatusCode::NO_CONTENT }))
            .layer(middleware::from_fn_with_state(access(), guard));
        let health = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::NO_CONTENT);
        let query = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/status?token={TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(query.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn rejects_wrong_credentials_and_duplicate_authorization_headers() {
        let access = access();
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
        assert!(!access.accepts(&headers));
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {TOKEN}").parse().unwrap(),
        );
        headers.append(
            header::AUTHORIZATION,
            format!("Bearer {TOKEN}").parse().unwrap(),
        );
        assert!(!access.accepts(&headers));
    }

    #[test]
    fn browser_origins_must_match_the_request_host_and_port() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "player.local:8080".parse().unwrap());
        for value in [
            "null",
            "https://attacker.example",
            "http://player.local:8081",
            "http://player.local:8080@attacker.example",
        ] {
            headers.insert(header::ORIGIN, value.parse().unwrap());
            assert!(!browser_origin_allowed(&headers));
        }
        headers.insert(header::ORIGIN, "http://player.local:8080".parse().unwrap());
        assert!(browser_origin_allowed(&headers));
        headers.insert("sec-fetch-site", "cross-site".parse().unwrap());
        assert!(!browser_origin_allowed(&headers));
    }

    #[test]
    fn refuses_missing_weak_or_world_readable_control_credentials() {
        use std::fs;
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "proaudio-api-token-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let config = ApiConfig {
            auth_token_file: Some(path.clone()),
            ..ApiConfig::default()
        };
        assert!(ApiAccess::load(&config).is_err());
        fs::write(&path, "weak").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(ApiAccess::load(&config).is_err());
        fs::write(&path, format!("{TOKEN}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(ApiAccess::load(&config).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(ApiAccess::load(&config).unwrap().enabled());
        fs::remove_file(path).unwrap();
    }
}
