use std::collections::HashMap;
use std::env;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio::time::{sleep, timeout};
use tracing::{debug, info};

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 6600;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
struct Endpoint {
    address: String,
    password: Option<String>,
}

impl Endpoint {
    fn from_env() -> Self {
        let raw_host = env::var("MPD_HOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_HOST.to_owned());
        let (password, host) = raw_host
            .rsplit_once('@')
            .map(|(password, host)| (Some(password.to_owned()), host.to_owned()))
            .unwrap_or((None, raw_host));
        let port = env::var("MPD_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(DEFAULT_PORT);
        let host = if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
            format!("[{host}]")
        } else {
            host
        };
        Self {
            address: format!("{host}:{port}"),
            password,
        }
    }
}

struct Connection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Connection {
    async fn connect(endpoint: &Endpoint) -> Result<Self> {
        let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(&endpoint.address))
            .await
            .map_err(|_| anyhow!("MPD connection timed out: {}", endpoint.address))?
            .with_context(|| format!("failed to connect to MPD at {}", endpoint.address))?;
        let (reader, writer) = stream.into_split();
        let mut connection = Self {
            reader: BufReader::new(reader),
            writer,
        };

        let greeting = timeout(COMMAND_TIMEOUT, connection.read_line())
            .await
            .map_err(|_| anyhow!("MPD greeting timed out"))??;
        if !greeting.starts_with("OK MPD ") {
            bail!("unexpected MPD greeting: {greeting}");
        }

        if let Some(password) = endpoint.password.as_deref() {
            let command = format!("password {}", quote_argument(password));
            timeout(COMMAND_TIMEOUT, connection.command(&command))
                .await
                .map_err(|_| anyhow!("MPD password command timed out"))??;
        }

        Ok(connection)
    }

    async fn read_line(&mut self) -> Result<String> {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line).await?;
        if read == 0 {
            bail!("MPD connection closed");
        }
        Ok(line.trim_end_matches(['\r', '\n']).to_owned())
    }

    async fn command(&mut self, command: &str) -> Result<Vec<String>> {
        self.writer.write_all(command.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;

        let mut lines = Vec::new();
        loop {
            let line = self.read_line().await?;
            if line == "OK" {
                return Ok(lines);
            }
            if line.starts_with("ACK ") {
                bail!("{line}");
            }
            lines.push(line);
        }
    }
}

fn quote_argument(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\"");
    format!("\"{escaped}\"")
}

fn fields(lines: &[String]) -> HashMap<String, Vec<String>> {
    let mut values = HashMap::<String, Vec<String>>::new();
    for line in lines {
        let Some((key, value)) = line.split_once(": ") else {
            continue;
        };
        values
            .entry(key.to_owned())
            .or_default()
            .push(value.to_owned());
    }
    values
}

fn first<'a>(values: &'a HashMap<String, Vec<String>>, key: &str) -> Option<&'a str> {
    values.get(key)?.first().map(String::as_str)
}

fn seconds_text(value: f64) -> String {
    let total = value.max(0.0).floor() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn parsed_snapshot(status_lines: &[String], song_lines: &[String]) -> CachedStatus {
    let status = fields(status_lines);
    let song = fields(song_lines);

    let file = first(&song, "file").unwrap_or("").to_owned();
    let is_stream = file.starts_with("http://") || file.starts_with("https://");
    let station = first(&song, "Name").unwrap_or("").to_owned();
    let title = first(&song, "Title")
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| (!station.is_empty()).then(|| station.clone()))
        .or_else(|| {
            Path::new(&file)
                .file_name()
                .and_then(|value| value.to_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| {
            if is_stream {
                "Мережевий потік".into()
            } else {
                String::new()
            }
        });
    let artist = song
        .get("Artist")
        .map(|values| {
            values
                .iter()
                .filter(|value| !value.is_empty())
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let state = match first(&status, "state") {
        Some("play") => "playing",
        Some("pause") => "paused",
        _ => "stopped",
    };
    let volume = first(&status, "volume")
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|value| *value >= 0)
        .map(|value| value as u32);
    let queue_position = first(&status, "song")
        .and_then(|value| value.parse::<u32>().ok())
        .map(|value| value.saturating_add(1));
    let queue_length = first(&status, "playlistlength")
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let elapsed_seconds = first(&status, "elapsed")
        .and_then(|value| value.parse::<f64>().ok())
        .or_else(|| {
            first(&status, "time")
                .and_then(|value| value.split_once(':'))
                .and_then(|(elapsed, _)| elapsed.parse::<f64>().ok())
        });
    let duration_seconds = first(&status, "duration")
        .and_then(|value| value.parse::<f64>().ok())
        .or_else(|| {
            first(&status, "time")
                .and_then(|value| value.split_once(':'))
                .and_then(|(_, duration)| duration.parse::<f64>().ok())
        });
    let progress = match (elapsed_seconds, duration_seconds) {
        (Some(elapsed), Some(duration)) if duration > 0.0 => {
            (elapsed * 100.0 / duration).clamp(0.0, 100.0).round() as u32
        }
        _ => 0,
    };

    CachedStatus {
        value: json!({
            "available": true,
            "state": state,
            "file": file,
            "title": title,
            "artist": artist,
            "album": first(&song, "Album").unwrap_or(""),
            "station": station,
            "is_stream": is_stream,
            "stream_url": if is_stream { Some(file.clone()) } else { None },
            "volume": volume,
            "queue_position": queue_position,
            "queue_length": queue_length,
            "elapsed": elapsed_seconds.map(seconds_text),
            "duration": duration_seconds.map(seconds_text),
            "progress": progress,
        }),
        refreshed_at: Instant::now(),
        elapsed_seconds,
        duration_seconds,
    }
}

#[derive(Clone)]
struct CachedStatus {
    value: Value,
    refreshed_at: Instant,
    elapsed_seconds: Option<f64>,
    duration_seconds: Option<f64>,
}

impl CachedStatus {
    fn unavailable(error: Option<&str>) -> Self {
        Self {
            value: json!({
                "available": false,
                "state": "unavailable",
                "error": error,
            }),
            refreshed_at: Instant::now(),
            elapsed_seconds: None,
            duration_seconds: None,
        }
    }

    fn snapshot(&self) -> Value {
        let mut value = self.value.clone();
        if value.get("state").and_then(Value::as_str) != Some("playing") {
            return value;
        }

        let Some(base_elapsed) = self.elapsed_seconds else {
            return value;
        };
        let mut elapsed = base_elapsed + self.refreshed_at.elapsed().as_secs_f64();
        if let Some(duration) = self.duration_seconds {
            elapsed = elapsed.min(duration);
        }
        let progress = match self.duration_seconds {
            Some(duration) if duration > 0.0 => {
                (elapsed * 100.0 / duration).clamp(0.0, 100.0).round() as u32
            }
            _ => 0,
        };

        if let Some(object) = value.as_object_mut() {
            object.insert("elapsed".into(), json!(seconds_text(elapsed)));
            object.insert("progress".into(), json!(progress));
        }
        value
    }
}

#[derive(Clone)]
pub struct MpdMonitor {
    endpoint: Endpoint,
    cache: Arc<RwLock<CachedStatus>>,
    started: Arc<AtomicBool>,
}

impl MpdMonitor {
    pub fn new() -> Self {
        Self {
            endpoint: Endpoint::from_env(),
            cache: Arc::new(RwLock::new(CachedStatus::unavailable(None))),
            started: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn start(&self) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let monitor = self.clone();
        tokio::spawn(async move {
            monitor.run().await;
        });
    }

    pub async fn snapshot(&self) -> Value {
        self.cache.read().await.snapshot()
    }

    async fn refresh(&self, connection: &mut Connection) -> Result<()> {
        let status = timeout(COMMAND_TIMEOUT, connection.command("status"))
            .await
            .map_err(|_| anyhow!("MPD status command timed out"))??;
        let song = timeout(COMMAND_TIMEOUT, connection.command("currentsong"))
            .await
            .map_err(|_| anyhow!("MPD currentsong command timed out"))??;
        *self.cache.write().await = parsed_snapshot(&status, &song);
        Ok(())
    }

    async fn mark_unavailable(&self, error: &anyhow::Error) {
        *self.cache.write().await = CachedStatus::unavailable(Some(&error.to_string()));
    }

    async fn run(self) {
        loop {
            let mut query = match Connection::connect(&self.endpoint).await {
                Ok(connection) => connection,
                Err(error) => {
                    self.mark_unavailable(&error).await;
                    debug!(error = %error, "MPD status connection unavailable");
                    sleep(RECONNECT_DELAY).await;
                    continue;
                }
            };
            let mut idle = match Connection::connect(&self.endpoint).await {
                Ok(connection) => connection,
                Err(error) => {
                    self.mark_unavailable(&error).await;
                    debug!(error = %error, "MPD idle connection unavailable");
                    sleep(RECONNECT_DELAY).await;
                    continue;
                }
            };

            if let Err(error) = self.refresh(&mut query).await {
                self.mark_unavailable(&error).await;
                debug!(error = %error, "Initial MPD status refresh failed");
                sleep(RECONNECT_DELAY).await;
                continue;
            }
            info!(endpoint = %self.endpoint.address, "Persistent MPD status monitor connected");

            loop {
                match idle.command("idle player playlist mixer options").await {
                    Ok(_) => {
                        if let Err(error) = self.refresh(&mut query).await {
                            self.mark_unavailable(&error).await;
                            debug!(error = %error, "MPD status refresh failed");
                            break;
                        }
                    }
                    Err(error) => {
                        self.mark_unavailable(&error).await;
                        debug!(error = %error, "MPD idle subscription disconnected");
                        break;
                    }
                }
            }

            sleep(RECONNECT_DELAY).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parsed_snapshot, quote_argument};

    fn lines(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_mpd_status_without_mpc_output_formatting() {
        let cached = parsed_snapshot(
            &lines(&[
                "volume: 73",
                "playlistlength: 3",
                "state: play",
                "song: 1",
                "elapsed: 12.500",
                "duration: 200.000",
            ]),
            &lines(&[
                "file: http://radio.example/stream",
                "Title: Track title",
                "Artist: Artist one",
                "Artist: Artist two",
                "Album: Album",
                "Name: Station",
            ]),
        );
        let value = cached.snapshot();

        assert_eq!(value["available"], true);
        assert_eq!(value["state"], "playing");
        assert_eq!(value["queue_position"], 2);
        assert_eq!(value["queue_length"], 3);
        assert_eq!(value["volume"], 73);
        assert_eq!(value["title"], "Track title");
        assert_eq!(value["artist"], "Artist one, Artist two");
        assert_eq!(value["station"], "Station");
        assert_eq!(value["is_stream"], true);
        assert_eq!(value["stream_url"], "http://radio.example/stream");
        assert_eq!(value["duration"], "3:20");
    }

    #[test]
    fn falls_back_to_file_name_for_local_tracks() {
        let cached = parsed_snapshot(
            &lines(&["playlistlength: 1", "state: pause", "song: 0"]),
            &lines(&["file: albums/example/track.flac"]),
        );
        let value = cached.snapshot();

        assert_eq!(value["state"], "paused");
        assert_eq!(value["title"], "track.flac");
        assert_eq!(value["is_stream"], false);
    }

    #[test]
    fn quotes_mpd_arguments_safely() {
        assert_eq!(quote_argument("a\\b\"c"), "\"a\\\\b\\\"c\"");
    }
}
