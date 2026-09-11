use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{TimeZone, Utc};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{error, warn};

use crate::audio::AudioEngine;
use crate::config::{effective_audio, AppConfig};
use crate::provider::{AlertStatus, AlertsProvider};
use crate::state::{AudioSnapshot, RuntimeState, StateStore};

pub type SharedRuntimeState = Arc<Mutex<RuntimeState>>;

pub struct AlertController {
    config: Arc<AppConfig>,
    provider: AlertsProvider,
    audio: AudioEngine,
    store: StateStore,
    pub state: SharedRuntimeState,
    minute_task: Option<JoinHandle<()>>,
    last_alert_announcement: Option<Instant>,
}

impl AlertController {
    pub fn new(
        config: Arc<AppConfig>,
        provider: AlertsProvider,
        audio: AudioEngine,
        store: StateStore,
        state: SharedRuntimeState,
    ) -> Self {
        Self {
            config,
            provider,
            audio,
            store,
            state,
            minute_task: None,
            last_alert_announcement: None,
        }
    }

    async fn persist(&self) -> Result<()> {
        let snapshot = self.state.lock().await.clone();
        self.store.save(&snapshot)
    }

    pub async fn recover(&mut self) -> Result<()> {
        let (was_active, minute_snapshot, mode, alert_snapshot) = {
            let mut state = self.state.lock().await;
            let values = (
                state.minute_silence_active,
                state.minute_silence_snapshot.clone(),
                state.mode.clone(),
                state.audio_snapshot.clone(),
            );
            if state.minute_silence_active {
                warn!("Відновлення після перерваної хвилини мовчання");
                state.minute_silence_active = false;
                state.minute_silence_snapshot = None;
            }
            values
        };
        if was_active {
            self.persist().await?;
        }
        if mode == "alert" {
            let cfg = self.audio.config()?;
            if let Some(snapshot) = alert_snapshot.as_ref() {
                if cfg.duck_only_during_announcement {
                    self.audio.restore(Some(snapshot)).await?;
                } else {
                    self.audio.ensure_alert(snapshot).await?;
                }
            }
            self.last_alert_announcement = Some(Instant::now());
        } else if was_active {
            self.audio.restore(minute_snapshot.as_ref()).await?;
        }
        Ok(())
    }

    async fn cancel_minute_silence(&mut self) -> Result<Option<AudioSnapshot>> {
        if let Some(handle) = self.minute_task.take() {
            handle.abort();
            let _ = handle.await;
        }
        let snapshot = {
            let mut state = self.state.lock().await;
            let snapshot = state.minute_silence_snapshot.clone();
            state.minute_silence_active = false;
            state.minute_silence_snapshot = None;
            snapshot
        };
        self.persist().await?;
        Ok(snapshot)
    }

    fn minute_due(&self, state: &RuntimeState) -> Result<bool> {
        let (_, minute) = effective_audio(&self.config.audio, &self.config.minute_silence)?;
        if !minute.enabled {
            return Ok(false);
        }
        let tz: chrono_tz::Tz = minute.timezone.parse()?;
        let local = Utc::now().with_timezone(&tz);
        let date = local.date_naive();
        let date_text = date.to_string();
        if state.last_minute_silence_date.as_deref() == Some(date_text.as_str()) {
            return Ok(false);
        }
        let time = chrono::NaiveTime::parse_from_str(&minute.start_time, "%H:%M:%S")?;
        let naive = date.and_time(time);
        let scheduled = tz
            .from_local_datetime(&naive)
            .single()
            .or_else(|| tz.from_local_datetime(&naive).earliest())
            .ok_or_else(|| anyhow::anyhow!("не вдалося визначити час хвилини мовчання"))?;
        let latest = scheduled + chrono::Duration::seconds(minute.catch_up_seconds as i64);
        Ok(local >= scheduled && local <= latest)
    }

    async fn maybe_start_minute_silence(&mut self) -> Result<bool> {
        if self.minute_task.as_ref().is_some_and(|t| !t.is_finished()) {
            return Ok(false);
        }
        if self.minute_task.as_ref().is_some_and(|t| t.is_finished()) {
            self.minute_task.take();
        }
        let current_state = self.state.lock().await.clone();
        if !self.minute_due(&current_state)? {
            return Ok(false);
        }

        let snapshot = self.audio.snapshot().await?;
        let (audio_cfg, minute) = effective_audio(&self.config.audio, &self.config.minute_silence)?;
        let talkover = audio_cfg.duck_only_during_announcement;
        let tz: chrono_tz::Tz = minute.timezone.parse()?;
        let date = Utc::now().with_timezone(&tz).date_naive().to_string();
        {
            let mut state = self.state.lock().await;
            state.last_minute_silence_date = Some(date);
            state.minute_silence_active = true;
            state.minute_silence_snapshot = Some(snapshot.clone());
        }
        self.persist().await?;

        let audio = self.audio.clone();
        let state = self.state.clone();
        let store = self.store.clone();
        self.minute_task = Some(tokio::spawn(async move {
            warn!("Початок щоденної хвилини мовчання");
            let result = async {
                audio
                    .enter_silence(&snapshot, minute.music_fade_seconds)
                    .await?;
                audio
                    .play(&minute.file, Some(minute.volume_percent))
                    .await?;
                let (mode, alert_snapshot) = {
                    let state = state.lock().await;
                    (state.mode.clone(), state.audio_snapshot.clone())
                };
                if mode == "alert" {
                    if let Some(alert) = alert_snapshot.as_ref() {
                        if talkover {
                            audio.restore(Some(alert)).await?;
                        } else {
                            audio.ensure_alert(alert).await?;
                        }
                    }
                } else {
                    audio.restore(Some(&snapshot)).await?;
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            if let Err(err) = result {
                error!("Помилка хвилини мовчання: {err:#}");
                state.lock().await.last_error = Some(format!("Хвилина мовчання: {err}"));
                let (mode, alert_snapshot) = {
                    let current = state.lock().await;
                    (current.mode.clone(), current.audio_snapshot.clone())
                };
                let recovery = if mode == "alert" {
                    match alert_snapshot.as_ref() {
                        Some(alert) if talkover => audio.restore(Some(alert)).await,
                        Some(alert) => audio.ensure_alert(alert).await,
                        None => Ok(()),
                    }
                } else {
                    audio.restore(Some(&snapshot)).await
                };
                if let Err(recovery_error) = recovery {
                    error!("Не вдалося відновити аудіо після помилки хвилини мовчання: {recovery_error:#}");
                }
            } else {
                warn!("Завершення щоденної хвилини мовчання");
            }
            let snapshot_to_save = {
                let mut s = state.lock().await;
                s.minute_silence_active = false;
                s.minute_silence_snapshot = None;
                s.clone()
            };
            let _ = store.save(&snapshot_to_save);
        }));
        Ok(true)
    }

    async fn play_alert_announcement(
        &self,
        file: &std::path::Path,
        snapshot: &AudioSnapshot,
        talkover: bool,
    ) -> Result<()> {
        if !talkover {
            return self.audio.play(file, None).await;
        }
        self.audio.enter_alert(snapshot).await?;
        let playback = self.audio.play(file, None).await;
        let restore = self.audio.restore(Some(snapshot)).await;
        playback?;
        restore
    }

    async fn begin_alert(
        &mut self,
        status: &AlertStatus,
        override_snapshot: Option<AudioSnapshot>,
    ) -> Result<()> {
        warn!(uids=?status.matched_uids, "Початок повітряної тривоги");
        let snapshot = match override_snapshot {
            Some(v) => v,
            None => self.audio.snapshot().await?,
        };
        {
            let mut state = self.state.lock().await;
            state.mode = "alert".into();
            state.clear_count = 0;
            state.entry_announced = false;
            state.clear_announced = false;
            state.audio_snapshot = Some(snapshot.clone());
            state.last_change_at = Some(Utc::now().to_rfc3339());
            state.matched_uids = status.matched_uids.clone();
        }
        self.persist().await?;
        let cfg = self.audio.config()?;
        if !cfg.duck_only_during_announcement {
            self.audio.enter_alert(&snapshot).await?;
        }
        self.play_alert_announcement(
            &cfg.start_file,
            &snapshot,
            cfg.duck_only_during_announcement,
        )
        .await?;
        self.last_alert_announcement = Some(Instant::now());
        self.state.lock().await.entry_announced = true;
        self.persist().await
    }

    async fn continue_alert(&mut self) -> Result<()> {
        let (snapshot, minute_active, entry_announced) = {
            let state = self.state.lock().await;
            (
                state.audio_snapshot.clone(),
                state.minute_silence_active,
                state.entry_announced,
            )
        };
        let cfg = self.audio.config()?;
        if !minute_active && !cfg.duck_only_during_announcement {
            if let Some(snapshot) = snapshot.as_ref() {
                self.audio.ensure_alert(snapshot).await?;
            }
        }
        let repeat_due = cfg.alert_repeat_interval_minutes > 0
            && self.last_alert_announcement.is_none_or(|last| {
                last.elapsed()
                    >= Duration::from_secs(cfg.alert_repeat_interval_minutes.saturating_mul(60))
            });
        if !minute_active && (!entry_announced || repeat_due) {
            if let Some(snapshot) = snapshot.as_ref() {
                self.play_alert_announcement(
                    &cfg.start_file,
                    snapshot,
                    cfg.duck_only_during_announcement,
                )
                .await?;
                self.last_alert_announcement = Some(Instant::now());
                self.state.lock().await.entry_announced = true;
                self.persist().await?;
            }
        }
        Ok(())
    }

    async fn clear_alert(&mut self) -> Result<()> {
        warn!("Відбій повітряної тривоги");
        let (clear_announced, snapshot, last_silence) = {
            let state = self.state.lock().await;
            (
                state.clear_announced,
                state.audio_snapshot.clone(),
                state.last_minute_silence_date.clone(),
            )
        };
        let announcement_result = if !clear_announced {
            let cfg = self.audio.config()?;
            let result = match snapshot.as_ref() {
                Some(snapshot) => {
                    self.play_alert_announcement(
                        &cfg.end_file,
                        snapshot,
                        cfg.duck_only_during_announcement,
                    )
                    .await
                }
                None => self.audio.play(&cfg.end_file, None).await,
            };
            if result.is_ok() {
                self.state.lock().await.clear_announced = true;
                self.persist().await?;
            }
            result
        } else {
            Ok(())
        };
        let restore_result = self.audio.restore(snapshot.as_ref()).await;
        {
            let mut state = self.state.lock().await;
            *state = RuntimeState {
                mode: "normal".into(),
                last_success_at: Some(Utc::now().to_rfc3339()),
                last_change_at: Some(Utc::now().to_rfc3339()),
                last_minute_silence_date: last_silence,
                ..RuntimeState::default()
            };
        }
        self.persist().await?;
        self.last_alert_announcement = None;
        announcement_result?;
        restore_result
    }

    async fn process(&mut self, status: AlertStatus) -> Result<()> {
        {
            let mut state = self.state.lock().await;
            state.last_success_at = Some(status.checked_at.to_rfc3339());
            state.last_error = None;
            state.matched_uids = status.matched_uids.clone();
        }
        let mode = self.state.lock().await.mode.clone();
        if status.active {
            self.state.lock().await.clear_count = 0;
            if mode == "normal" {
                let minute_snapshot = if self.state.lock().await.minute_silence_active {
                    self.cancel_minute_silence().await?
                } else {
                    None
                };
                self.begin_alert(&status, minute_snapshot).await?;
            } else {
                self.continue_alert().await?;
                self.persist().await?;
            }
            return Ok(());
        }
        if mode == "normal" {
            self.state.lock().await.clear_count = 0;
            self.persist().await?;
            return Ok(());
        }
        let clear_count = {
            let mut state = self.state.lock().await;
            state.clear_count += 1;
            state.clear_count
        };
        self.persist().await?;
        if clear_count >= self.provider.current_config()?.clear_confirmations {
            if self.state.lock().await.minute_silence_active {
                let _ = self.cancel_minute_silence().await?;
            }
            self.clear_alert().await?;
        }
        Ok(())
    }

    pub async fn run_once(&mut self) -> Result<f64> {
        match self.provider.fetch().await {
            Ok(result) => {
                let next = result.next_poll_seconds;
                self.process(result.status).await?;
                Ok(next)
            }
            Err(err) => {
                error!("{err:#}");
                self.state.lock().await.last_error = Some(err.to_string());
                self.persist().await?;
                let (mode, snapshot, minute_active) = {
                    let state = self.state.lock().await;
                    (
                        state.mode.clone(),
                        state.audio_snapshot.clone(),
                        state.minute_silence_active,
                    )
                };
                if mode == "alert" && !minute_active {
                    let cfg = self.audio.config()?;
                    if !cfg.duck_only_during_announcement {
                        if let Some(s) = snapshot.as_ref() {
                            self.audio.ensure_alert(s).await?;
                        }
                    }
                }
                let c = self.provider.current_config()?;
                Ok(if err.to_string().contains("HTTP 429") {
                    c.rate_limit_backoff_seconds
                } else {
                    c.poll_interval_seconds
                })
            }
        }
    }

    pub async fn run_forever(mut self) -> Result<()> {
        loop {
            match self.recover().await {
                Ok(()) => break,
                Err(err) => {
                    error!("Не вдалося відновити audio state: {err:#}; повторна спроба");
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
        let mut next_poll = Instant::now();
        loop {
            if let Err(err) = self.maybe_start_minute_silence().await {
                error!("minute silence scheduler: {err:#}");
            }
            if Instant::now() >= next_poll {
                let poll = self.run_once().await.unwrap_or_else(|err| {
                    error!("alert controller: {err:#}");
                    8.0
                });
                next_poll = Instant::now() + Duration::from_secs_f64(poll.max(0.25));
            }
            sleep(Duration::from_millis(250)).await;
        }
    }
}
