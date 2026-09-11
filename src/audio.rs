use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::sleep;

use crate::audio_backend::{
    linear_to_percent, percent_to_linear, AudioBackend, SinkState, StreamState,
};
use crate::config::{effective_audio, AppConfig};
use crate::output_gain::{BackendOutputGain, OutputGain};
use crate::output_router::{ExternalOutputRouter, OutputDescriptor, OutputRouter};
use crate::state::AudioSnapshot;

#[derive(Clone)]
pub struct AudioEngine {
    config: Arc<AppConfig>,
    backend: Arc<dyn AudioBackend>,
    output_gain: Arc<dyn OutputGain>,
    output_router: Arc<dyn OutputRouter>,
}

impl AudioEngine {
    pub fn new(config: Arc<AppConfig>, backend: Arc<dyn AudioBackend>) -> Self {
        let output_gain: Arc<dyn OutputGain> = Arc::new(BackendOutputGain::new(backend.clone()));
        let output_router: Arc<dyn OutputRouter> = Arc::new(ExternalOutputRouter::new(
            backend.clone(),
            config.audio.music_sink.clone(),
            config.audio.alert_sink.clone(),
        ));
        Self::with_components(config, backend, output_gain, output_router)
    }

    pub fn with_components(
        config: Arc<AppConfig>,
        backend: Arc<dyn AudioBackend>,
        output_gain: Arc<dyn OutputGain>,
        output_router: Arc<dyn OutputRouter>,
    ) -> Self {
        Self {
            config,
            backend,
            output_gain,
            output_router,
        }
    }

    pub fn config(&self) -> Result<crate::config::AudioConfig> {
        Ok(effective_audio(&self.config.audio, &self.config.minute_silence)?.0)
    }

    pub fn minute_config(&self) -> Result<crate::config::MinuteSilenceConfig> {
        Ok(effective_audio(&self.config.audio, &self.config.minute_silence)?.1)
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.backend_name()
    }

    pub fn master_backend_name(&self) -> &'static str {
        self.output_gain.backend_name()
    }

    pub fn output_router_name(&self) -> &'static str {
        self.output_router.backend_name()
    }

    pub fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.backend.subscribe_changes()
    }

    pub fn subscribe_output_changes(&self) -> watch::Receiver<u64> {
        self.output_router.subscribe_changes()
    }

    pub async fn sink_state(&self, sink: &str) -> Result<SinkState> {
        self.backend.sink_state(sink).await
    }

    pub async fn list_outputs(&self) -> Result<Vec<OutputDescriptor>> {
        self.output_router.list_outputs().await
    }

    pub async fn active_output(&self) -> Result<OutputDescriptor> {
        self.output_router.active_output().await
    }

    pub async fn select_output(&self, id: &str) -> Result<OutputDescriptor> {
        self.output_router.select_output(id).await
    }

    pub async fn list_sink_inputs(&self) -> Result<Vec<StreamState>> {
        self.backend.list_sink_inputs().await
    }

    pub async fn set_sink_input_percent(&self, index: u32, percent: f64) -> Result<()> {
        if !(0.0..=100.0).contains(&percent) {
            bail!("гучність має бути 0..100");
        }
        self.backend.set_sink_input_percent(index, percent).await?;
        Ok(())
    }

    pub async fn set_sink_input_mute(&self, index: u32, muted: bool) -> Result<()> {
        self.backend.set_sink_input_mute(index, muted).await?;
        Ok(())
    }

    pub async fn master_state(&self, sink: &str) -> Result<SinkState> {
        self.output_gain.state(sink).await
    }

    pub async fn set_master_percent(&self, sink: &str, percent: f64) -> Result<()> {
        if !(0.0..=100.0).contains(&percent) {
            bail!("гучність має бути 0..100");
        }
        self.output_gain.set_percent(sink, percent).await?;
        self.output_gain.set_mute(sink, percent <= 0.0).await?;
        Ok(())
    }

    pub async fn set_master_db(&self, sink: &str, db: f64) -> Result<()> {
        if !(-60.0..=0.0).contains(&db) {
            bail!("рівень має бути в межах -60..0 dB");
        }
        self.output_gain.set_db(sink, db).await?;
        Ok(())
    }

    pub async fn set_master_mute(&self, sink: &str, muted: bool) -> Result<()> {
        self.output_gain.set_mute(sink, muted).await?;
        Ok(())
    }

    async fn snapshot_sink(&self, sink: &str) -> Result<AudioSnapshot> {
        let state = self.sink_state(sink).await?;
        if state.volumes_percent.is_empty() {
            bail!("не вдалося прочитати гучність {sink}");
        }
        Ok(AudioSnapshot {
            volumes_percent: state.volumes_percent,
            muted: state.muted,
        })
    }

    pub async fn snapshot(&self) -> Result<AudioSnapshot> {
        let cfg = self.config()?;
        self.snapshot_sink(&cfg.music_sink).await
    }

    async fn set_volume(&self, sink: &str, volumes: &[f64]) -> Result<()> {
        self.backend
            .set_sink_percent_channels(sink, volumes)
            .await?;
        Ok(())
    }

    pub async fn set_sink_mute(&self, sink: &str, muted: bool) -> Result<()> {
        self.backend.set_sink_mute(sink, muted).await?;
        Ok(())
    }

    pub async fn set_sink_db(&self, sink: &str, db: f64) -> Result<()> {
        if !(-60.0..=0.0).contains(&db) {
            bail!("рівень має бути в межах -60..0 dB");
        }
        self.backend.set_sink_db(sink, db).await?;
        Ok(())
    }

    pub async fn set_music_volume(&self, percent: f64) -> Result<()> {
        if !(0.0..=100.0).contains(&percent) {
            bail!("гучність має бути 0..100");
        }
        let cfg = self.config()?;
        let values = [percent];
        self.backend
            .set_sink_percent_channels(&cfg.music_sink, &values)
            .await?;
        Ok(())
    }

    pub async fn set_music_mute(&self, muted: bool) -> Result<()> {
        let cfg = self.config()?;
        self.set_sink_mute(&cfg.music_sink, muted).await
    }

    pub async fn set_sink_percent(&self, sink: &str, percent: f64) -> Result<()> {
        if !(0.0..=100.0).contains(&percent) {
            bail!("гучність має бути 0..100");
        }
        let values = [percent];
        self.backend
            .set_sink_percent_channels(sink, &values)
            .await?;
        self.set_sink_mute(sink, percent <= 0.0).await
    }

    async fn fade(&self, sink: &str, start: &[f64], target: &[f64], duration: f64) -> Result<()> {
        let target = if target.len() == start.len() {
            target.to_vec()
        } else {
            vec![*target.first().unwrap_or(&0.0); start.len()]
        };
        if duration <= 0.0 {
            return self.set_volume(sink, &target).await;
        }

        let steps = ((duration * 20.0).floor() as usize).max(1);
        for index in 1..=steps {
            let ratio = index as f64 / steps as f64;
            let values = start
                .iter()
                .zip(target.iter())
                .map(|(a, b)| {
                    let start_linear = percent_to_linear(*a);
                    let target_linear = percent_to_linear(*b);
                    linear_to_percent(start_linear + (target_linear - start_linear) * ratio)
                })
                .collect::<Vec<_>>();
            self.set_volume(sink, &values).await?;
            sleep(Duration::from_secs_f64(duration / steps as f64)).await;
        }
        Ok(())
    }

    pub fn ducked_volumes(&self, snapshot: &AudioSnapshot) -> Result<Vec<f64>> {
        let factor = 10f64.powf(self.config()?.duck_db / 20.0);
        Ok(snapshot
            .volumes_percent
            .iter()
            .map(|percent| linear_to_percent(percent_to_linear(*percent) * factor))
            .collect())
    }

    pub async fn enter_alert(&self, snapshot: &AudioSnapshot) -> Result<()> {
        let cfg = self.config()?;
        let current = self.snapshot().await?;
        let target = self.ducked_volumes(snapshot)?;
        if !snapshot.muted {
            self.set_sink_mute(&cfg.music_sink, false).await?;
        }
        self.fade(
            &cfg.music_sink,
            &current.volumes_percent,
            &target,
            cfg.duck_fade_seconds,
        )
        .await
    }

    pub async fn enter_silence(&self, snapshot: &AudioSnapshot, duration: f64) -> Result<()> {
        if snapshot.muted {
            return Ok(());
        }
        let cfg = self.config()?;
        self.fade(
            &cfg.music_sink,
            &snapshot.volumes_percent,
            &vec![0.0; snapshot.volumes_percent.len()],
            duration,
        )
        .await
    }

    pub async fn ensure_alert(&self, snapshot: &AudioSnapshot) -> Result<()> {
        let cfg = self.config()?;
        self.set_volume(&cfg.music_sink, &self.ducked_volumes(snapshot)?)
            .await?;
        self.set_sink_mute(&cfg.music_sink, snapshot.muted).await
    }

    pub async fn restore(&self, snapshot: Option<&AudioSnapshot>) -> Result<()> {
        let cfg = self.config()?;
        let current = self.snapshot().await?;
        let (target, muted) = match snapshot {
            Some(s) if !s.volumes_percent.is_empty() => (s.volumes_percent.clone(), s.muted),
            _ => (
                vec![cfg.default_restore_volume_percent; current.volumes_percent.len()],
                false,
            ),
        };
        if !muted {
            self.set_sink_mute(&cfg.music_sink, false).await?;
        }
        self.fade(
            &cfg.music_sink,
            &current.volumes_percent,
            &target,
            cfg.restore_fade_seconds,
        )
        .await?;
        self.set_sink_mute(&cfg.music_sink, muted).await
    }

    pub async fn play(
        &self,
        media_file: &std::path::Path,
        volume_percent: Option<f64>,
    ) -> Result<()> {
        if !media_file.is_file() {
            bail!("файл оповіщення відсутній: {}", media_file.display());
        }
        let cfg = self.config()?;
        self.set_sink_mute(&cfg.alert_sink, false).await?;
        let volume = volume_percent.unwrap_or(cfg.alert_volume_percent);
        let values = [volume];
        self.backend
            .set_sink_percent_channels(&cfg.alert_sink, &values)
            .await?;

        let output = Command::new(&cfg.player_binary)
            .args(["--no-video", "--really-quiet", "--ao=pulse", "--volume=100"])
            .arg(media_file)
            .env("PULSE_SINK", &cfg.alert_sink)
            .env("LC_ALL", "C")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("не вдалося запустити {}", cfg.player_binary))?
            .wait_with_output()
            .await?;
        if !output.status.success() {
            return Err(anyhow!(
                "не вдалося відтворити {}: {}",
                media_file.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }
}
