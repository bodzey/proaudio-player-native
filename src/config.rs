use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use url::Url;

fn default_endpoint() -> String {
    "http://192.168.88.122/v1/iot/active_air_raid_alerts/{uid}.json".into()
}
fn default_token_file() -> PathBuf { "/etc/proaudio-player-alert/alerts-token".into() }
fn default_provider_settings() -> PathBuf { "/var/lib/proaudio-player-alert/provider-settings.yaml".into() }
fn default_audio_settings() -> PathBuf { "/var/lib/proaudio-player-alert/audio-settings.yaml".into() }
fn default_music_sink() -> String { "proaudio_player_music".into() }
fn default_alert_sink() -> String { "proaudio_player_alert".into() }
fn default_start_file() -> PathBuf { "/var/lib/proaudio-player-alert/media/alarm_start.mp3".into() }
fn default_end_file() -> PathBuf { "/var/lib/proaudio-player-alert/media/alarm_end.mp3".into() }
fn default_silence_file() -> PathBuf { "/var/lib/proaudio-player-alert/media/minute_silence.mp3".into() }
fn default_player_binary() -> String { "/usr/bin/mpv".into() }
fn default_state_file() -> PathBuf { "/var/lib/proaudio-player-alert/state.json".into() }
fn default_location_uid() -> u32 { 1133 }
fn default_location_type() -> String { "hromada".into() }
fn default_poll() -> f64 { 8.0 }
fn default_timeout() -> f64 { 7.0 }
fn default_backoff() -> f64 { 60.0 }
fn default_clear_confirmations() -> u32 { 2 }
fn default_duck_db() -> f64 { -18.0 }
fn default_duck_fade() -> f64 { 1.0 }
fn default_restore_fade() -> f64 { 3.0 }
fn default_volume() -> f64 { 100.0 }
fn default_timezone() -> String { "Europe/Kyiv".into() }
fn default_silence_time() -> String { "08:59:50".into() }
fn default_catchup() -> u64 { 120 }
fn default_true() -> bool { true }
fn default_host() -> String { "0.0.0.0".into() }
fn default_port() -> u16 { 8080 }
fn default_library_items() -> usize { 5000 }
fn default_log_level() -> String { "INFO".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    pub token: Option<String>,
    #[serde(default = "default_token_file")]
    pub token_file: PathBuf,
    #[serde(default = "default_provider_settings")]
    pub settings_file: PathBuf,
    #[serde(default = "default_location_uid")]
    pub location_uid: u32,
    #[serde(default = "default_location_type")]
    pub location_type: String,
    #[serde(default = "default_poll")]
    pub poll_interval_seconds: f64,
    #[serde(default = "default_timeout")]
    pub request_timeout_seconds: f64,
    #[serde(default = "default_backoff")]
    pub rate_limit_backoff_seconds: f64,
    #[serde(default = "default_clear_confirmations")]
    pub clear_confirmations: u32,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            endpoint: default_endpoint(), token: None, token_file: default_token_file(),
            settings_file: default_provider_settings(), location_uid: default_location_uid(),
            location_type: default_location_type(), poll_interval_seconds: default_poll(),
            request_timeout_seconds: default_timeout(), rate_limit_backoff_seconds: default_backoff(),
            clear_confirmations: default_clear_confirmations(),
        }
    }
}

impl ProviderConfig {
    pub fn status_endpoint(&self) -> String {
        self.endpoint.replace("{uid}", &self.location_uid.to_string())
    }

    pub fn partial_status_is_active(&self) -> bool {
        matches!(self.location_type.as_str(), "raion" | "oblast")
    }

    pub fn resolve_token(&self) -> Result<String> {
        if let Some(token) = self.token.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            return Ok(token.to_owned());
        }
        if let Ok(value) = fs::read_to_string(&self.token_file) {
            let value = value.trim();
            if !value.is_empty() { return Ok(value.to_owned()); }
        }
        if let Ok(value) = env::var("ALERTS_API_TOKEN") {
            let value = value.trim();
            if !value.is_empty() { return Ok(value.to_owned()); }
        }
        bail!("не задано токен alerts.in.ua")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    #[serde(default = "default_music_sink")]
    pub music_sink: String,
    #[serde(default = "default_alert_sink")]
    pub alert_sink: String,
    #[serde(default = "default_duck_db")]
    pub duck_db: f64,
    #[serde(default = "default_duck_fade")]
    pub duck_fade_seconds: f64,
    #[serde(default = "default_restore_fade")]
    pub restore_fade_seconds: f64,
    #[serde(default = "default_volume")]
    pub alert_volume_percent: f64,
    #[serde(default = "default_volume")]
    pub default_restore_volume_percent: f64,
    #[serde(default = "default_start_file")]
    pub start_file: PathBuf,
    #[serde(default = "default_end_file")]
    pub end_file: PathBuf,
    #[serde(default = "default_player_binary")]
    pub player_binary: String,
    #[serde(default = "default_audio_settings")]
    pub settings_file: PathBuf,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            music_sink: default_music_sink(), alert_sink: default_alert_sink(),
            duck_db: default_duck_db(), duck_fade_seconds: default_duck_fade(),
            restore_fade_seconds: default_restore_fade(), alert_volume_percent: default_volume(),
            default_restore_volume_percent: default_volume(), start_file: default_start_file(),
            end_file: default_end_file(), player_binary: default_player_binary(),
            settings_file: default_audio_settings(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MinuteSilenceConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default = "default_silence_time")]
    pub start_time: String,
    #[serde(default = "default_catchup")]
    pub catch_up_seconds: u64,
    #[serde(default = "default_duck_fade")]
    pub music_fade_seconds: f64,
    #[serde(default = "default_volume")]
    pub volume_percent: f64,
    #[serde(default = "default_silence_file")]
    pub file: PathBuf,
}

impl Default for MinuteSilenceConfig {
    fn default() -> Self {
        Self { enabled: true, timezone: default_timezone(), start_time: default_silence_time(),
            catch_up_seconds: default_catchup(), music_fade_seconds: default_duck_fade(),
            volume_percent: default_volume(), file: default_silence_file() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_library_items")]
    pub max_library_items: usize,
}
impl Default for WebConfig {
    fn default() -> Self { Self { enabled: true, host: default_host(), port: default_port(), max_library_items: default_library_items() } }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub provider: ProviderConfig,
    pub audio: AudioConfig,
    pub minute_silence: MinuteSilenceConfig,
    pub web: WebConfig,
    #[serde(default = "default_state_file")]
    pub state_file: PathBuf,
    #[serde(default = "default_log_level")]
    pub log_level: String,
}
impl Default for AppConfig {
    fn default() -> Self {
        Self { provider: ProviderConfig::default(), audio: AudioConfig::default(), minute_silence: MinuteSilenceConfig::default(), web: WebConfig::default(), state_file: default_state_file(), log_level: default_log_level() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ProviderSettingsFile { provider: Option<ProviderConfigPatch> }
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ProviderConfigPatch {
    endpoint: Option<String>, location_uid: Option<u32>, location_type: Option<String>,
    poll_interval_seconds: Option<f64>, request_timeout_seconds: Option<f64>,
    rate_limit_backoff_seconds: Option<f64>, clear_confirmations: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct AudioSettingsFile { audio: Option<AudioSettingsPatch> }
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AudioSettingsPatch {
    pub duck_db: Option<f64>, pub duck_fade_seconds: Option<f64>, pub restore_fade_seconds: Option<f64>,
    pub alert_volume_percent: Option<f64>, pub default_restore_volume_percent: Option<f64>,
    pub minute_silence_volume_percent: Option<f64>,
}

pub fn load_config(path: impl AsRef<Path>) -> Result<AppConfig> {
    let text = fs::read_to_string(path.as_ref())
        .with_context(|| format!("файл конфігурації не знайдено: {}", path.as_ref().display()))?;
    let config: AppConfig = serde_yaml::from_str(&text).context("некоректний YAML конфігурації")?;
    validate_config(&config)?;
    Ok(config)
}

pub fn effective_provider(base: &ProviderConfig) -> Result<ProviderConfig> {
    let mut out = base.clone();
    if !base.settings_file.exists() { return Ok(out); }
    let raw = fs::read_to_string(&base.settings_file)?;
    if raw.trim().is_empty() { return Ok(out); }
    let file: ProviderSettingsFile = serde_yaml::from_str(&raw)?;
    if let Some(p) = file.provider {
        if let Some(v) = p.endpoint { out.endpoint = v; }
        if let Some(v) = p.location_uid { out.location_uid = v; }
        if let Some(v) = p.location_type { out.location_type = v; }
        if let Some(v) = p.poll_interval_seconds { out.poll_interval_seconds = v; }
        if let Some(v) = p.request_timeout_seconds { out.request_timeout_seconds = v; }
        if let Some(v) = p.rate_limit_backoff_seconds { out.rate_limit_backoff_seconds = v; }
        if let Some(v) = p.clear_confirmations { out.clear_confirmations = v; }
    }
    validate_provider(&out)?;
    Ok(out)
}

pub fn effective_audio(base: &AudioConfig, minute: &MinuteSilenceConfig) -> Result<(AudioConfig, MinuteSilenceConfig)> {
    let mut audio = base.clone();
    let mut silence = minute.clone();
    if !base.settings_file.exists() { return Ok((audio, silence)); }
    let raw = fs::read_to_string(&base.settings_file)?;
    if raw.trim().is_empty() { return Ok((audio, silence)); }
    let file: AudioSettingsFile = serde_yaml::from_str(&raw)?;
    if let Some(a) = file.audio {
        if let Some(v) = a.duck_db { audio.duck_db = v; }
        if let Some(v) = a.duck_fade_seconds { audio.duck_fade_seconds = v; }
        if let Some(v) = a.restore_fade_seconds { audio.restore_fade_seconds = v; }
        if let Some(v) = a.alert_volume_percent { audio.alert_volume_percent = v; }
        if let Some(v) = a.default_restore_volume_percent { audio.default_restore_volume_percent = v; }
        if let Some(v) = a.minute_silence_volume_percent { silence.volume_percent = v; }
    }
    validate_audio(&audio, &silence)?;
    Ok((audio, silence))
}

pub fn save_provider_settings(config: &ProviderConfig) -> Result<()> {
    let payload = serde_yaml::to_string(&serde_json::json!({"provider": {
        "endpoint": config.endpoint, "location_uid": config.location_uid,
        "location_type": config.location_type, "poll_interval_seconds": config.poll_interval_seconds,
        "request_timeout_seconds": config.request_timeout_seconds,
        "rate_limit_backoff_seconds": config.rate_limit_backoff_seconds,
        "clear_confirmations": config.clear_confirmations
    }}))?;
    atomic_write(&config.settings_file, payload.as_bytes(), 0o600)
}

pub fn save_provider_token(config: &ProviderConfig, token: &str) -> Result<()> {
    let token = token.trim();
    if token.is_empty() { bail!("API-токен не може бути порожнім"); }
    if let Some(parent) = config.token_file.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&config.token_file)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(format!("{token}\n").as_bytes())?;
    file.sync_all()?;
    Ok(())
}

pub fn save_audio_settings(audio: &AudioConfig, minute: &MinuteSilenceConfig) -> Result<()> {
    validate_audio(audio, minute)?;
    let payload = serde_yaml::to_string(&serde_json::json!({"audio": {
        "duck_db": audio.duck_db, "duck_fade_seconds": audio.duck_fade_seconds,
        "restore_fade_seconds": audio.restore_fade_seconds,
        "alert_volume_percent": audio.alert_volume_percent,
        "default_restore_volume_percent": audio.default_restore_volume_percent,
        "minute_silence_volume_percent": minute.volume_percent
    }}))?;
    atomic_write(&audio.settings_file, payload.as_bytes(), 0o600)
}

fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
    let tmp = path.with_extension(format!("{}.tmp", path.extension().and_then(|v| v.to_str()).unwrap_or("")));
    {
        let mut file = fs::File::create(&tmp)?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn validate_provider(p: &ProviderConfig) -> Result<()> {
    if p.location_uid == 0 { bail!("provider.location_uid має бути додатним"); }
    if !matches!(p.location_type.as_str(), "hromada" | "raion" | "oblast" | "standalone" | "city") {
        bail!("невідомий provider.location_type");
    }
    if !p.endpoint.contains("{uid}") { bail!("provider.endpoint має містити шаблон {uid}"); }
    let parsed = Url::parse(&p.status_endpoint()).context("provider.endpoint має бути коректною HTTP(S)-адресою")?;
    if !matches!(parsed.scheme(), "http" | "https") { bail!("provider.endpoint має бути HTTP(S)"); }
    if p.poll_interval_seconds < 8.0 { bail!("poll_interval_seconds має бути не менше 8 секунд"); }
    if p.request_timeout_seconds <= 0.0 { bail!("request_timeout_seconds має бути більшим за нуль"); }
    if p.rate_limit_backoff_seconds < 60.0 { bail!("rate_limit_backoff_seconds має бути не менше 60 секунд"); }
    if p.clear_confirmations == 0 { bail!("clear_confirmations має бути не менше 1"); }
    Ok(())
}

pub fn validate_audio(a: &AudioConfig, m: &MinuteSilenceConfig) -> Result<()> {
    if !(-60.0..=0.0).contains(&a.duck_db) { bail!("duck_db має бути в межах -60..0"); }
    for (name, value) in [
        ("alert_volume_percent", a.alert_volume_percent),
        ("default_restore_volume_percent", a.default_restore_volume_percent),
        ("minute_silence.volume_percent", m.volume_percent),
    ] {
        if !(0.0..=150.0).contains(&value) { bail!("{name} має бути в межах 0..150"); }
    }
    if a.duck_fade_seconds < 0.0 || a.restore_fade_seconds < 0.0 || m.music_fade_seconds < 0.0 {
        bail!("час fade не може бути від'ємним");
    }
    Ok(())
}

pub fn validate_config(c: &AppConfig) -> Result<()> {
    validate_provider(&c.provider)?;
    validate_audio(&c.audio, &c.minute_silence)?;
    chrono::NaiveTime::parse_from_str(&c.minute_silence.start_time, "%H:%M:%S")
        .map_err(|_| anyhow!("minute_silence.start_time має формат HH:MM:SS"))?;
    c.minute_silence.timezone.parse::<chrono_tz::Tz>()
        .map_err(|_| anyhow!("невідомий часовий пояс minute_silence.timezone"))?;
    if c.web.max_library_items == 0 || c.web.max_library_items > 50_000 { bail!("web.max_library_items має бути 1..50000"); }
    Ok(())
}
