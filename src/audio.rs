use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::process::Command;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::audio_backend::{
    AudioBackend, SinkState, StreamState, linear_to_percent, percent_to_linear,
};
use crate::config::{AppConfig, effective_audio};
use crate::output_gain::{BackendOutputGain, OutputGain};
use crate::output_router::{ExternalOutputRouter, OutputDescriptor, OutputRouter};
use crate::state::{AudioSnapshot, MixerStateRuntime};

const MIXER_STATE_FILE: &str = "/var/lib/proaudio-player-alert/mixer-state.json";

#[derive(Clone)]
pub struct AudioEngine {
    config: Arc<AppConfig>,
    backend: Arc<dyn AudioBackend>,
    output_gain: Arc<dyn OutputGain>,
    output_router: Arc<dyn OutputRouter>,
    mixer_state: MixerStateRuntime,
    mix_policy_lock: Arc<Mutex<()>>,
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
            mixer_state: MixerStateRuntime::new(MIXER_STATE_FILE),
            mix_policy_lock: Arc::new(Mutex::new(())),
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

    pub fn start_mixer_state_writer(&self) -> JoinHandle<()> {
        self.mixer_state.start_writer()
    }

    pub async fn restore_user_mixer(&self) -> Result<()> {
        let _policy_guard = self.mix_policy_lock.lock().await;
        let cfg = self.config()?;
        let state = self.mixer_state.snapshot();

        let music_percent = state
            .music_percent
            .unwrap_or(cfg.default_restore_volume_percent);
        self.backend
            .set_sink_percent_channels(&cfg.music_sink, &[music_percent])
            .await?;
        self.backend
            .set_sink_mute(&cfg.music_sink, state.music_muted.unwrap_or(false))
            .await?;

        let master_percent = state.master_percent.unwrap_or(100.0);
        self.output_gain
            .set_percent(crate::output_router::DEFAULT_MASTER_SINK, master_percent)
            .await?;
        self.output_gain
            .set_mute(
                crate::output_router::DEFAULT_MASTER_SINK,
                state.master_muted.unwrap_or(false),
            )
            .await?;

        let alert_percent = state.alert_percent.unwrap_or(cfg.alert_volume_percent);
        self.backend
            .set_sink_percent_channels(&cfg.alert_sink, &[alert_percent])
            .await?;
        self.backend
            .set_sink_mute(&cfg.alert_sink, state.alert_muted.unwrap_or(false))
            .await?;
        Ok(())
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
        let muted = percent <= 0.0;
        self.output_gain.set_mute(sink, muted).await?;
        self.mixer_state.set_master_percent(percent);
        self.mixer_state.set_master_muted(muted);
        Ok(())
    }

    pub async fn set_master_db(&self, sink: &str, db: f64) -> Result<()> {
        if !(-60.0..=0.0).contains(&db) {
            bail!("рівень має бути в межах -60..0 dB");
        }
        self.output_gain.set_db(sink, db).await?;
        self.mixer_state.set_master_percent(db_to_percent(db));
        Ok(())
    }

    pub async fn set_master_mute(&self, sink: &str, muted: bool) -> Result<()> {
        self.output_gain.set_mute(sink, muted).await?;
        self.mixer_state.set_master_muted(muted);
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
        let _policy_guard = self.mix_policy_lock.lock().await;
        self.backend.set_sink_mute(sink, muted).await?;
        let cfg = self.config()?;
        if sink == cfg.alert_sink {
            self.mixer_state.set_alert_muted(muted);
        }
        Ok(())
    }

    pub async fn set_sink_db(&self, sink: &str, db: f64) -> Result<()> {
        if !(-60.0..=0.0).contains(&db) {
            bail!("рівень має бути в межах -60..0 dB");
        }
        let _policy_guard = self.mix_policy_lock.lock().await;
        self.backend.set_sink_db(sink, db).await?;
        let cfg = self.config()?;
        let percent = db_to_percent(db);
        if sink == cfg.music_sink {
            self.mixer_state.set_music_percent(percent);
        } else if sink == cfg.alert_sink {
            self.mixer_state.set_alert_percent(percent);
        }
        Ok(())
    }

    pub async fn set_music_volume(&self, percent: f64) -> Result<()> {
        if !(0.0..=100.0).contains(&percent) {
            bail!("гучність має бути 0..100");
        }
        let _policy_guard = self.mix_policy_lock.lock().await;
        let cfg = self.config()?;
        self.backend
            .set_sink_percent_channels(&cfg.music_sink, &[percent])
            .await?;
        self.mixer_state.set_music_percent(percent);
        Ok(())
    }

    pub async fn set_music_mute(&self, muted: bool) -> Result<()> {
        let _policy_guard = self.mix_policy_lock.lock().await;
        let cfg = self.config()?;
        self.backend.set_sink_mute(&cfg.music_sink, muted).await?;
        self.mixer_state.set_music_muted(muted);
        Ok(())
    }

    pub async fn set_sink_percent(&self, sink: &str, percent: f64) -> Result<()> {
        if !(0.0..=100.0).contains(&percent) {
            bail!("гучність має бути 0..100");
        }
        let _policy_guard = self.mix_policy_lock.lock().await;
        self.backend
            .set_sink_percent_channels(sink, &[percent])
            .await?;
        let muted = percent <= 0.0;
        self.backend.set_sink_mute(sink, muted).await?;

        let cfg = self.config()?;
        if sink == cfg.music_sink {
            self.mixer_state.set_music_percent(percent);
            self.mixer_state.set_music_muted(muted);
        } else if sink == cfg.alert_sink {
            self.mixer_state.set_alert_percent(percent);
            self.mixer_state.set_alert_muted(muted);
        }
        Ok(())
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

    fn mix_safe_alert_volumes(requested: &[f64], music: &AudioSnapshot) -> Vec<f64> {
        let fallback_music = music.volumes_percent.first().copied().unwrap_or(0.0);
        requested
            .iter()
            .enumerate()
            .map(|(channel, requested_percent)| {
                let music_percent = if music.muted {
                    0.0
                } else {
                    music
                        .volumes_percent
                        .get(channel)
                        .copied()
                        .unwrap_or(fallback_music)
                };
                let available = (1.0 - percent_to_linear(music_percent)).max(0.0);
                linear_to_percent(percent_to_linear(*requested_percent).min(available))
            })
            .collect()
    }

    async fn enter_alert_locked(&self, snapshot: &AudioSnapshot) -> Result<()> {
        let cfg = self.config()?;
        let current = self.snapshot().await?;
        let target = self.ducked_volumes(snapshot)?;
        if !snapshot.muted {
            self.backend.set_sink_mute(&cfg.music_sink, false).await?;
        }
        self.fade(
            &cfg.music_sink,
            &current.volumes_percent,
            &target,
            cfg.duck_fade_seconds,
        )
        .await
    }

    pub async fn enter_alert(&self, snapshot: &AudioSnapshot) -> Result<()> {
        let _policy_guard = self.mix_policy_lock.lock().await;
        self.enter_alert_locked(snapshot).await
    }

    async fn enter_silence_locked(&self, snapshot: &AudioSnapshot, duration: f64) -> Result<()> {
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

    pub async fn enter_silence(&self, snapshot: &AudioSnapshot, duration: f64) -> Result<()> {
        let _policy_guard = self.mix_policy_lock.lock().await;
        self.enter_silence_locked(snapshot, duration).await
    }

    async fn ensure_alert_locked(&self, snapshot: &AudioSnapshot) -> Result<()> {
        let cfg = self.config()?;
        self.set_volume(&cfg.music_sink, &self.ducked_volumes(snapshot)?)
            .await?;
        self.backend
            .set_sink_mute(&cfg.music_sink, snapshot.muted)
            .await?;
        Ok(())
    }

    pub async fn ensure_alert(&self, snapshot: &AudioSnapshot) -> Result<()> {
        let _policy_guard = self.mix_policy_lock.lock().await;
        self.ensure_alert_locked(snapshot).await
    }

    async fn restore_locked(&self, snapshot: Option<&AudioSnapshot>) -> Result<()> {
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
            self.backend.set_sink_mute(&cfg.music_sink, false).await?;
        }
        self.fade(
            &cfg.music_sink,
            &current.volumes_percent,
            &target,
            cfg.restore_fade_seconds,
        )
        .await?;
        self.backend.set_sink_mute(&cfg.music_sink, muted).await?;
        Ok(())
    }

    pub async fn restore(&self, snapshot: Option<&AudioSnapshot>) -> Result<()> {
        let _policy_guard = self.mix_policy_lock.lock().await;
        self.restore_locked(snapshot).await
    }

    pub async fn play(
        &self,
        media_file: &std::path::Path,
        volume_percent: Option<f64>,
    ) -> Result<()> {
        let _policy_guard = self.mix_policy_lock.lock().await;
        self.play_with_locked_policy(media_file, volume_percent).await
    }

    pub async fn play_talkover(
        &self,
        media_file: &std::path::Path,
        snapshot: &AudioSnapshot,
    ) -> Result<()> {
        // Duck, sample the remaining peak budget, play and restore as one policy
        // transaction. Web/API MUSIC or ALERT changes wait until this finishes,
        // so a concurrent fader move cannot invalidate the mix-safe cap or be
        // overwritten by restoring the pre-announcement snapshot.
        let _policy_guard = self.mix_policy_lock.lock().await;
        self.enter_alert_locked(snapshot).await?;
        let playback = self.play_with_locked_policy(media_file, None).await;
        let restore = self.restore_locked(Some(snapshot)).await;
        playback?;
        restore
    }

    async fn play_with_locked_policy(
        &self,
        media_file: &std::path::Path,
        volume_percent: Option<f64>,
    ) -> Result<()> {
        if !media_file.is_file() {
            bail!("файл оповіщення відсутній: {}", media_file.display());
        }
        let cfg = self.config()?;

        if let Some(volume) = volume_percent {
            if !(0.0..=100.0).contains(&volume) {
                bail!("гучність має бути 0..100");
            }
        }

        let alert_snapshot = self.snapshot_sink(&cfg.alert_sink).await?;
        let music_snapshot = self.snapshot_sink(&cfg.music_sink).await?;
        let requested = match volume_percent {
            Some(volume) => vec![volume; alert_snapshot.volumes_percent.len()],
            None => alert_snapshot.volumes_percent.clone(),
        };
        let playback_volumes = Self::mix_safe_alert_volumes(&requested, &music_snapshot);
        let temporary_gain = volume_percent.is_some()
            || playback_volumes
                .iter()
                .zip(&alert_snapshot.volumes_percent)
                .any(|(left, right)| (left - right).abs() > 0.000_001);

        // ALERT remains a user fader, but during actual playback its effective
        // gain is capped to the linear headroom left by the already-ducked MUSIC
        // bus. This guarantees |music + alert| <= 1 for full-scale input samples
        // without permanently attenuating normal playback or adding a nonlinear
        // limiter/another clock domain. The persisted fader is always restored.
        let apply = async {
            self.backend
                .set_sink_percent_channels(&cfg.alert_sink, &playback_volumes)
                .await?;
            if volume_percent.is_some() {
                self.backend.set_sink_mute(&cfg.alert_sink, false).await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        if let Err(error) = apply {
            if temporary_gain {
                let _ = self
                    .set_volume(&cfg.alert_sink, &alert_snapshot.volumes_percent)
                    .await;
                let _ = self
                    .backend
                    .set_sink_mute(&cfg.alert_sink, alert_snapshot.muted)
                    .await;
            }
            return Err(error);
        }

        let playback = async {
            let playback = Command::new(&cfg.player_binary)
                .args(["--no-video", "--really-quiet", "--ao=pulse", "--volume=100"])
                .arg(media_file)
                .env("PULSE_SINK", &cfg.alert_sink)
                .env("LC_ALL", "C")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .with_context(|| format!("не вдалося запустити {}", cfg.player_binary))?;
            let output = tokio::time::timeout(
                Duration::from_secs(15 * 60),
                playback.wait_with_output(),
            )
            .await
            .context("тайм-аут відтворення оповіщення")??;
            if !output.status.success() {
                return Err(anyhow!(
                    "не вдалося відтворити {}: {}",
                    media_file.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        let restore = async {
            if temporary_gain {
                self.set_volume(&cfg.alert_sink, &alert_snapshot.volumes_percent)
                    .await?;
                self.backend
                    .set_sink_mute(&cfg.alert_sink, alert_snapshot.muted)
                    .await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        playback?;
        restore
    }
}

fn db_to_percent(db: f64) -> f64 {
    (100.0 * 10f64.powf(db / 60.0)).clamp(0.0, 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulse_db_percent_conversion_matches_cubic_volume_scale() {
        assert!((db_to_percent(0.0) - 100.0).abs() < 1.0e-9);
        assert!((db_to_percent(-6.0) - 79.432_823_472_428_14).abs() < 1.0e-9);
        assert!((db_to_percent(-60.0) - 10.0).abs() < 1.0e-9);
    }

    #[test]
    fn alert_mix_uses_only_the_linear_headroom_left_by_music() {
        let music = AudioSnapshot {
            volumes_percent: vec![linear_to_percent(10f64.powf(-12.0 / 20.0)); 2],
            muted: false,
        };
        let alert = AudioEngine::mix_safe_alert_volumes(&[100.0, 100.0], &music);

        for (music_percent, alert_percent) in music.volumes_percent.iter().zip(alert) {
            let sum = percent_to_linear(*music_percent) + percent_to_linear(alert_percent);
            assert!((sum - 1.0).abs() < 1.0e-12);
        }
    }

    #[test]
    fn alert_mix_preserves_unity_when_music_is_muted() {
        let music = AudioSnapshot {
            volumes_percent: vec![100.0, 100.0],
            muted: true,
        };
        assert_eq!(
            AudioEngine::mix_safe_alert_volumes(&[100.0, 80.0], &music),
            vec![100.0, 80.0]
        );
    }

    #[test]
    fn alert_mix_does_not_raise_the_requested_alert_level() {
        let music = AudioSnapshot {
            volumes_percent: vec![50.0],
            muted: false,
        };
        let alert = AudioEngine::mix_safe_alert_volumes(&[60.0, 40.0], &music);
        assert!((alert[0] - 60.0).abs() < 1.0e-12);
        assert!((alert[1] - 40.0).abs() < 1.0e-12);
    }

    #[test]
    fn alert_mix_never_exceeds_unity_across_the_full_control_range() {
        for music_percent in (0..=100).map(f64::from) {
            for requested_percent in (0..=100).map(f64::from) {
                let music = AudioSnapshot {
                    volumes_percent: vec![music_percent],
                    muted: false,
                };
                let effective =
                    AudioEngine::mix_safe_alert_volumes(&[requested_percent], &music)[0];
                let sum = percent_to_linear(music_percent) + percent_to_linear(effective);
                assert!(sum <= 1.0 + 1.0e-12);
                assert!(effective <= requested_percent + 1.0e-12);
            }
        }
    }
}
