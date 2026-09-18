use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use url::Url;

const CACHE_TTL: Duration = Duration::from_secs(30 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const REMOTE_LIMIT: usize = 120;
const PUBLIC_LIMIT: usize = 64;
const BOOTSTRAP_SERVERS: &[&str] = &[
    "https://de1.api.radio-browser.info",
    "https://nl1.api.radio-browser.info",
    "https://at1.api.radio-browser.info",
];

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct DirectoryStation {
    pub id: String,
    pub name: String,
    pub url: String,
    pub homepage: Option<String>,
    pub favicon: Option<String>,
    pub tags: Vec<String>,
    pub codec: Option<String>,
    pub bitrate: Option<u32>,
    pub votes: u64,
}

#[derive(Debug, Deserialize)]
struct RadioBrowserStation {
    stationuuid: String,
    name: String,
    url_resolved: String,
    #[serde(default)]
    homepage: String,
    #[serde(default)]
    favicon: String,
    #[serde(default)]
    tags: String,
    #[serde(default)]
    codec: String,
    #[serde(default)]
    bitrate: u32,
    #[serde(default)]
    votes: u64,
}

#[derive(Debug, Deserialize)]
struct RadioBrowserServer {
    name: String,
}

#[derive(Default)]
struct DirectoryCache {
    loaded_at: Option<Instant>,
    items: Vec<DirectoryStation>,
}

static CACHE: LazyLock<Mutex<DirectoryCache>> =
    LazyLock::new(|| Mutex::new(DirectoryCache::default()));

fn clean_http_url(value: &str) -> Option<String> {
    let parsed = Url::parse(value.trim()).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return None;
    }
    Some(parsed.to_string())
}

fn normalize_name(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn normalize_station(value: RadioBrowserStation) -> Option<DirectoryStation> {
    let name = value.name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        return None;
    }

    let url = clean_http_url(&value.url_resolved)?;
    let homepage = clean_http_url(&value.homepage);
    let favicon = clean_http_url(&value.favicon);
    let tags = value
        .tags
        .split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .take(4)
        .map(str::to_owned)
        .collect();

    Some(DirectoryStation {
        id: value.stationuuid,
        name,
        url,
        homepage,
        favicon,
        tags,
        codec: (!value.codec.trim().is_empty()).then(|| value.codec.trim().to_owned()),
        bitrate: (value.bitrate > 0).then_some(value.bitrate),
        votes: value.votes,
    })
}

fn normalize_server_name(value: &str) -> Option<String> {
    let name = value.trim().to_ascii_lowercase();
    if !name.ends_with(".api.radio-browser.info")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        return None;
    }
    Some(format!("https://{name}"))
}

async fn discover_servers(client: &Client) -> Vec<String> {
    for base in BOOTSTRAP_SERVERS {
        let response = client
            .get(format!("{base}/json/servers"))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status);
        let Ok(response) = response else {
            continue;
        };
        let Ok(items) = response.json::<Vec<RadioBrowserServer>>().await else {
            continue;
        };

        let mut servers = items
            .into_iter()
            .filter_map(|item| normalize_server_name(&item.name))
            .collect::<Vec<_>>();
        servers.sort();
        servers.dedup();
        if !servers.is_empty() {
            return servers;
        }
    }

    BOOTSTRAP_SERVERS.iter().map(|value| (*value).to_owned()).collect()
}

async fn fetch_directory() -> Result<Vec<DirectoryStation>> {
    let client = Client::builder()
        .user_agent("proaudio-player-native/0.1")
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("Не вдалося створити HTTP-клієнт каталогу радіо")?;

    let mut servers = discover_servers(&client).await;
    for bootstrap in BOOTSTRAP_SERVERS {
        if !servers.iter().any(|value| value == bootstrap) {
            servers.push((*bootstrap).to_owned());
        }
    }

    let mut last_error = None;
    for base in servers {
        let response = match client
            .get(format!("{base}/json/stations/search"))
            .query(&[
                ("countrycode", "UA"),
                ("hidebroken", "true"),
                ("order", "votes"),
                ("reverse", "true"),
                ("limit", &REMOTE_LIMIT.to_string()),
            ])
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
        {
            Ok(value) => value,
            Err(err) => {
                last_error = Some(err);
                continue;
            }
        };

        let raw = match response.json::<Vec<RadioBrowserStation>>().await {
            Ok(value) => value,
            Err(err) => {
                last_error = Some(err);
                continue;
            }
        };

        let mut items = raw.into_iter().filter_map(normalize_station).collect::<Vec<_>>();
        items.sort_by(|left, right| {
            right
                .votes
                .cmp(&left.votes)
                .then_with(|| right.favicon.is_some().cmp(&left.favicon.is_some()))
                .then_with(|| right.bitrate.unwrap_or_default().cmp(&left.bitrate.unwrap_or_default()))
                .then_with(|| left.name.cmp(&right.name))
        });

        let mut names = HashSet::new();
        items.retain(|station| names.insert(normalize_name(&station.name)));
        items.truncate(PUBLIC_LIMIT);

        if !items.is_empty() {
            return Ok(items);
        }
    }

    Err(last_error
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("Каталог Radio Browser не повернув станцій")))
}

pub async fn ukrainian_stations() -> Result<Vec<DirectoryStation>> {
    let mut cache = CACHE.lock().await;
    if cache
        .loaded_at
        .is_some_and(|loaded_at| loaded_at.elapsed() < CACHE_TTL)
        && !cache.items.is_empty()
    {
        return Ok(cache.items.clone());
    }

    match fetch_directory().await {
        Ok(items) => {
            cache.items = items;
            cache.loaded_at = Some(Instant::now());
            Ok(cache.items.clone())
        }
        Err(err) if !cache.items.is_empty() => Ok(cache.items.clone()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_server_name, normalize_station, RadioBrowserStation};

    #[test]
    fn station_uses_resolved_stream_and_real_favicon() {
        let station = normalize_station(RadioBrowserStation {
            stationuuid: "abc".into(),
            name: "  Test   Radio ".into(),
            url_resolved: "https://stream.example.org/live".into(),
            homepage: "https://example.org".into(),
            favicon: "https://example.org/logo.png".into(),
            tags: "rock, ukrainian, rock".into(),
            codec: "MP3".into(),
            bitrate: 192,
            votes: 42,
        })
        .unwrap();

        assert_eq!(station.name, "Test Radio");
        assert_eq!(station.url, "https://stream.example.org/live");
        assert_eq!(station.favicon.as_deref(), Some("https://example.org/logo.png"));
        assert_eq!(station.codec.as_deref(), Some("MP3"));
        assert_eq!(station.bitrate, Some(192));
    }

    #[test]
    fn mirror_names_are_restricted_to_radio_browser_domain() {
        assert_eq!(
            normalize_server_name("de1.api.radio-browser.info").as_deref(),
            Some("https://de1.api.radio-browser.info")
        );
        assert!(normalize_server_name("evil.example.org").is_none());
        assert!(normalize_server_name("de1.api.radio-browser.info.evil.org").is_none());
    }
}
