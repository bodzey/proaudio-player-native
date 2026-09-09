use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use tokio::process::Command;
use tokio::time::sleep;

use crate::command;
use crate::config::{effective_audio, AppConfig};
use crate::state::AudioSnapshot;

#[derive(Clone)]
pub struct AudioEngine { config: Arc<AppConfig> }

impl AudioEngine {
    pub fn new(config: Arc<AppConfig>) -> Self { Self { config } }
    pub fn config(&self) -> Result<crate::config::AudioConfig> { Ok(effective_audio(&self.config.audio, &self.config.minute_silence)?.0) }
    pub fn minute_config(&self) -> Result<crate::config::MinuteSilenceConfig> { Ok(effective_audio(&self.config.audio, &self.config.minute_silence)?.1) }

    pub async fn snapshot(&self) -> Result<AudioSnapshot> {
        let cfg = self.config()?;
        let volume = command::run("pactl", &["get-sink-volume", &cfg.music_sink], true, 8).await?;
        let first = volume.stdout.lines().next().unwrap_or_default();
        let re = Regex::new(r"(\d+(?:\.\d+)?)%")?;
        let volumes_percent = re.captures_iter(first).filter_map(|c| c.get(1)?.as_str().parse::<f64>().ok()).collect::<Vec<_>>();
        if volumes_percent.is_empty() { bail!("не вдалося прочитати гучність {}", cfg.music_sink); }
        let mute = command::run("pactl", &["get-sink-mute", &cfg.music_sink], true, 8).await?;
        Ok(AudioSnapshot { volumes_percent, muted: mute.stdout.to_ascii_lowercase().ends_with("yes") })
    }

    async fn set_volume(&self, sink: &str, volumes: &[f64]) -> Result<()> {
        let safe = volumes.iter().map(|v| v.clamp(0.0, 150.0)).collect::<Vec<_>>();
        let owned = safe.iter().map(|v| format!("{v:.3}%")).collect::<Vec<_>>();
        let mut args = vec!["set-sink-volume", sink];
        args.extend(owned.iter().map(String::as_str));
        command::run("pactl", &args, true, 8).await?;
        Ok(())
    }
    async fn set_mute(&self, sink: &str, muted: bool) -> Result<()> {
        command::run("pactl", &["set-sink-mute", sink, if muted { "1" } else { "0" }], true, 8).await?;
        Ok(())
    }
    pub async fn set_music_volume(&self, percent: f64) -> Result<()> {
        if !(0.0..=150.0).contains(&percent) { bail!("гучність має бути 0..150"); }
        let snap = self.snapshot().await?;
        let channels = snap.volumes_percent.len().max(1);
        let cfg = self.config()?;
        self.set_volume(&cfg.music_sink, &vec![percent; channels]).await
    }
    pub async fn set_music_mute(&self, muted: bool) -> Result<()> {
        let cfg = self.config()?;
        self.set_mute(&cfg.music_sink, muted).await
    }
    pub async fn set_sink_percent(&self, sink: &str, percent: f64) -> Result<()> {
        if !(0.0..=150.0).contains(&percent) { bail!("гучність має бути 0..150"); }
        self.set_volume(sink, &[percent, percent]).await?;
        self.set_mute(sink, percent <= 0.0).await
    }

    async fn fade(&self, sink: &str, start: &[f64], target: &[f64], duration: f64) -> Result<()> {
        let target = if target.len() == start.len() { target.to_vec() } else { vec![*target.first().unwrap_or(&0.0); start.len()] };
        if duration <= 0.0 { return self.set_volume(sink, &target).await; }
        let steps = ((duration * 20.0).floor() as usize).max(1);
        for index in 1..=steps {
            let ratio = index as f64 / steps as f64;
            let values = start.iter().zip(target.iter()).map(|(a,b)| a + (b-a)*ratio).collect::<Vec<_>>();
            self.set_volume(sink, &values).await?;
            sleep(Duration::from_secs_f64(duration / steps as f64)).await;
        }
        Ok(())
    }

    pub fn ducked_volumes(&self, snapshot: &AudioSnapshot) -> Result<Vec<f64>> {
        let factor = 10f64.powf(self.config()?.duck_db / 20.0);
        Ok(snapshot.volumes_percent.iter().map(|v| v * factor).collect())
    }
    pub async fn enter_alert(&self, snapshot: &AudioSnapshot) -> Result<()> {
        let cfg = self.config()?;
        let current = self.snapshot().await?;
        let target = self.ducked_volumes(snapshot)?;
        if !snapshot.muted { self.set_mute(&cfg.music_sink, false).await?; }
        self.fade(&cfg.music_sink, &current.volumes_percent, &target, cfg.duck_fade_seconds).await
    }
    pub async fn enter_silence(&self, snapshot: &AudioSnapshot, duration: f64) -> Result<()> {
        if snapshot.muted { return Ok(()); }
        let cfg = self.config()?;
        self.fade(&cfg.music_sink, &snapshot.volumes_percent, &vec![0.0; snapshot.volumes_percent.len()], duration).await
    }
    pub async fn ensure_alert(&self, snapshot: &AudioSnapshot) -> Result<()> {
        let cfg = self.config()?;
        self.set_volume(&cfg.music_sink, &self.ducked_volumes(snapshot)?).await?;
        self.set_mute(&cfg.music_sink, snapshot.muted).await
    }
    pub async fn restore(&self, snapshot: Option<&AudioSnapshot>) -> Result<()> {
        let cfg = self.config()?;
        let current = self.snapshot().await?;
        let (target, muted) = match snapshot {
            Some(s) if !s.volumes_percent.is_empty() => (s.volumes_percent.clone(), s.muted),
            _ => (vec![cfg.default_restore_volume_percent; current.volumes_percent.len()], false),
        };
        if !muted { self.set_mute(&cfg.music_sink, false).await?; }
        self.fade(&cfg.music_sink, &current.volumes_percent, &target, cfg.restore_fade_seconds).await?;
        self.set_mute(&cfg.music_sink, muted).await
    }

    pub async fn play(&self, media_file: &std::path::Path, volume_percent: Option<f64>) -> Result<()> {
        if !media_file.is_file() { bail!("файл оповіщення відсутній: {}", media_file.display()); }
        let cfg = self.config()?;
        self.set_mute(&cfg.alert_sink, false).await?;
        let volume = volume_percent.unwrap_or(cfg.alert_volume_percent);
        self.set_volume(&cfg.alert_sink, &[volume, volume]).await?;
        let output = Command::new(&cfg.player_binary)
            .args(["--no-video", "--really-quiet", "--ao=pulse", "--volume=100"])
            .arg(media_file)
            .env("PULSE_SINK", &cfg.alert_sink)
            .env("LC_ALL", "C")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn().with_context(|| format!("не вдалося запустити {}", cfg.player_binary))?
            .wait_with_output().await?;
        if !output.status.success() {
            return Err(anyhow!("не вдалося відтворити {}: {}", media_file.display(), String::from_utf8_lossy(&output.stderr).trim()));
        }
        Ok(())
    }
}
