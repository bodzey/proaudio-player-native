use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::{AppConfig, SampleRateMode};

const DEFAULT_AUDIO_ENV: &str = "/etc/proaudio-player-alert/audio.env";

pub fn apply(config: &mut AppConfig) -> Result<()> {
    let path = std::env::var_os("PROAUDIO_AUDIO_ENV")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_AUDIO_ENV));
    apply_from(config, &path)
}

fn apply_from(config: &mut AppConfig, path: &Path) -> Result<()> {
    let Ok(text) = fs::read_to_string(path) else {
        // Host-side development and unit tests may not have the firmware env file.
        // The serde defaults remain the compatibility fallback; production images
        // always install audio.env and therefore take this branch only on damage.
        return Ok(());
    };

    let rate = env_value(&text, "SAMPLE_RATE")
        .ok_or_else(|| anyhow::anyhow!("{} не містить SAMPLE_RATE", path.display()))?
        .parse::<u32>()
        .with_context(|| format!("некоректний SAMPLE_RATE у {}", path.display()))?;
    if !(8_000..=384_000).contains(&rate) {
        bail!("SAMPLE_RATE має бути в межах 8000..384000 Hz");
    }

    // The firmware graph is the authority. YAML fields are retained only for API
    // compatibility with older clients and are overwritten before runtime starts.
    config.audio.sample_rate_mode = SampleRateMode::Fixed;
    config.audio.sample_rate = rate;
    config.audio.allowed_sample_rates = vec![rate];
    Ok(())
}

fn env_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let (name, value) = line.split_once('=')?;
        (name.trim() == key).then(|| value.trim().trim_matches('"'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_file(contents: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "proaudio-audio-env-{}-{stamp}",
            std::process::id()
        ));
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn processing_rate_comes_from_audio_env() {
        let path = temp_file("SAMPLE_RATE=96000\nAUDIO_CHANNELS=2\n");
        let mut config = AppConfig::default();
        apply_from(&mut config, &path).unwrap();
        let _ = fs::remove_file(path);
        assert_eq!(config.audio.sample_rate_mode, SampleRateMode::Fixed);
        assert_eq!(config.audio.sample_rate, 96_000);
        assert_eq!(config.audio.allowed_sample_rates, vec![96_000]);
    }

    #[test]
    fn invalid_processing_rate_fails_closed() {
        let path = temp_file("SAMPLE_RATE=4000\n");
        let mut config = AppConfig::default();
        assert!(apply_from(&mut config, &path).is_err());
        let _ = fs::remove_file(path);
    }
}
