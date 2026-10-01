use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{sleep, sleep_until, Instant};
use tracing::{error, warn};

use crate::atomic_file;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AudioSnapshot {
    #[serde(default)]
    pub volumes_percent: Vec<f64>,
    #[serde(default)]
    pub muted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeState {
    pub mode: String,
    pub clear_count: u32,
    pub entry_announced: bool,
    pub clear_announced: bool,
    pub audio_snapshot: Option<AudioSnapshot>,
    pub last_success_at: Option<String>,
    pub last_change_at: Option<String>,
    pub last_error: Option<String>,
    pub matched_uids: Vec<u32>,
    pub last_minute_silence_date: Option<String>,
    pub minute_silence_active: bool,
    pub minute_silence_snapshot: Option<AudioSnapshot>,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            mode: "normal".into(),
            clear_count: 0,
            entry_announced: false,
            clear_announced: false,
            audio_snapshot: None,
            last_success_at: None,
            last_change_at: None,
            last_error: None,
            matched_uids: Vec::new(),
            last_minute_silence_date: None,
            minute_silence_active: false,
            minute_silence_snapshot: None,
        }
    }
}

#[derive(Clone)]
pub struct StateStore {
    path: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

impl StateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn load(&self) -> RuntimeState {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return RuntimeState::default()
            }
            Err(error) => {
                warn!(path = %self.path.display(), %error, "Не вдалося прочитати runtime state");
                return RuntimeState::default();
            }
        };
        let state = match serde_json::from_str::<RuntimeState>(&text) {
            Ok(state) => state,
            Err(error) => {
                warn!(path = %self.path.display(), %error, "Пошкоджений runtime state проігноровано");
                return RuntimeState::default();
            }
        };
        if matches!(state.mode.as_str(), "normal" | "alert") {
            state
        } else {
            warn!(path = %self.path.display(), mode = %state.mode, "Невідомий runtime mode проігноровано");
            RuntimeState::default()
        }
    }

    pub fn save(&self, state: &RuntimeState) -> Result<()> {
        atomic_json_write(&self.path, state)
    }

    pub async fn save_current(&self, state: Arc<tokio::sync::Mutex<RuntimeState>>) -> Result<()> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            // Acquire the snapshot after serialization, so cancelled or concurrent
            // announcement tasks cannot write an older mode over a newer one.
            let _writer = store
                .write_lock
                .lock()
                .map_err(|_| anyhow!("runtime state writer lock poisoned"))?;
            let snapshot = state.blocking_lock().clone();
            store.save(&snapshot)
        })
        .await?
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct MixerState {
    pub music_percent: Option<f64>,
    pub music_muted: Option<bool>,
    pub master_percent: Option<f64>,
    pub master_muted: Option<bool>,
    pub alert_percent: Option<f64>,
    pub alert_muted: Option<bool>,
}

impl MixerState {
    fn sanitize(mut self) -> Self {
        self.music_percent = valid_percent(self.music_percent);
        self.master_percent = valid_percent(self.master_percent);
        self.alert_percent = valid_percent(self.alert_percent);
        self
    }
}

fn valid_percent(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
}

#[derive(Clone)]
pub struct MixerStateStore {
    path: PathBuf,
}

impl MixerStateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> MixerState {
        let Ok(text) = fs::read_to_string(&self.path) else {
            return MixerState::default();
        };
        serde_json::from_str::<MixerState>(&text)
            .unwrap_or_default()
            .sanitize()
    }

    pub fn save(&self, state: &MixerState) -> Result<()> {
        atomic_json_write(&self.path, state)
    }
}

#[derive(Clone)]
pub struct MixerStateRuntime {
    store: MixerStateStore,
    state: Arc<Mutex<MixerState>>,
    persisted: Arc<Mutex<MixerState>>,
    changes: watch::Sender<u64>,
}

const MIXER_QUIET_PERIOD: Duration = Duration::from_millis(750);
const MIXER_MAX_WRITE_DELAY: Duration = Duration::from_secs(2);
const MIXER_WRITE_RETRY: Duration = Duration::from_secs(1);

async fn coalesce_mixer_changes(changes: &mut watch::Receiver<u64>) {
    let deadline = Instant::now() + MIXER_MAX_WRITE_DELAY;
    let mut quiet = Instant::now() + MIXER_QUIET_PERIOD;
    loop {
        tokio::select! {
            biased;
            _ = sleep_until(quiet.min(deadline)) => return,
            changed = changes.changed() => {
                if changed.is_err() {
                    return;
                }
                quiet = Instant::now() + MIXER_QUIET_PERIOD;
            }
        }
    }
}

async fn save_current_mixer(
    store: MixerStateStore,
    state: Arc<Mutex<MixerState>>,
    persisted: Arc<Mutex<MixerState>>,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        // Serialize background writes and shutdown flushes, then snapshot the
        // current settings so an older queued write cannot undo a newer one.
        let mut saved = persisted
            .lock()
            .map_err(|_| anyhow!("mixer state writer lock poisoned"))?;
        let current = state
            .lock()
            .map_err(|_| anyhow!("mixer state lock poisoned"))?
            .clone();
        if current != *saved {
            store.save(&current)?;
            *saved = current;
        }
        Ok(())
    })
    .await?
}

impl MixerStateRuntime {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let store = MixerStateStore::new(path);
        let initial = store.load();
        let state = Arc::new(Mutex::new(initial.clone()));
        let (changes, _) = watch::channel(0_u64);
        Self {
            store,
            state,
            persisted: Arc::new(Mutex::new(initial)),
            changes,
        }
    }

    pub fn snapshot(&self) -> MixerState {
        self.state
            .lock()
            .map(|state| state.clone())
            .unwrap_or_default()
    }

    fn update(&self, mutate: impl FnOnce(&mut MixerState)) {
        let changed = if let Ok(mut state) = self.state.lock() {
            let before = state.clone();
            mutate(&mut state);
            *state != before
        } else {
            false
        };
        if changed {
            self.changes
                .send_modify(|generation| *generation = generation.wrapping_add(1));
        }
    }

    pub fn set_music_percent(&self, value: f64) {
        self.update(|state| state.music_percent = valid_percent(Some(value)));
    }

    pub fn set_music_muted(&self, value: bool) {
        self.update(|state| state.music_muted = Some(value));
    }

    pub fn set_master_percent(&self, value: f64) {
        self.update(|state| state.master_percent = valid_percent(Some(value)));
    }

    pub fn set_master_muted(&self, value: bool) {
        self.update(|state| state.master_muted = Some(value));
    }

    pub fn set_alert_percent(&self, value: f64) {
        self.update(|state| state.alert_percent = valid_percent(Some(value)));
    }

    pub fn set_alert_muted(&self, value: bool) {
        self.update(|state| state.alert_muted = Some(value));
    }

    pub async fn flush(&self) -> Result<()> {
        save_current_mixer(
            self.store.clone(),
            self.state.clone(),
            self.persisted.clone(),
        )
        .await
    }

    pub fn start_writer(&self) -> JoinHandle<()> {
        let state = self.state.clone();
        let store = self.store.clone();
        let persisted = self.persisted.clone();
        let mut changes = self.changes.subscribe();
        changes.mark_changed();
        tokio::spawn(async move {
            loop {
                if changes.changed().await.is_err() {
                    return;
                }

                // Coalesce fader traffic, but do not postpone persistence
                // indefinitely while a client keeps adjusting the controls.
                coalesce_mixer_changes(&mut changes).await;

                loop {
                    match save_current_mixer(store.clone(), state.clone(), persisted.clone()).await
                    {
                        Ok(()) => break,
                        Err(err) => {
                            error!("Не вдалося зберегти mixer state: {err:#}; повторна спроба");
                            if changes.has_changed().is_err() {
                                return;
                            }
                            // An unavailable/full filesystem must recover without
                            // requiring another UI update to wake this writer.
                            sleep(MIXER_WRITE_RETRY).await;
                        }
                    }
                }
            }
        })
    }
}

fn atomic_json_write<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let payload = serde_json::to_vec_pretty(value)?;
    atomic_file::write(path, &payload, 0o600)
}

#[cfg(test)]
mod mixer_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixer_state_sanitizes_invalid_percentages() {
        let state = MixerState {
            music_percent: Some(f64::NAN),
            master_percent: Some(101.0),
            alert_percent: Some(25.0),
            ..MixerState::default()
        }
        .sanitize();
        assert_eq!(state.music_percent, None);
        assert_eq!(state.master_percent, None);
        assert_eq!(state.alert_percent, Some(25.0));
    }

    #[tokio::test]
    async fn queued_runtime_write_persists_the_current_mode_after_serialization() {
        let path = std::env::temp_dir().join(format!(
            "proaudio-runtime-write-{}.json",
            std::process::id()
        ));
        let store = StateStore::new(&path);
        let state = Arc::new(tokio::sync::Mutex::new(RuntimeState::default()));
        let write_lock = store.write_lock.clone();
        let (locked, ready) = tokio::sync::oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let writer = tokio::task::spawn_blocking(move || {
            let _guard = write_lock.lock().unwrap();
            locked.send(()).unwrap();
            held.recv().unwrap();
        });
        ready.await.unwrap();
        let pending_store = store.clone();
        let pending_state = state.clone();
        let pending = tokio::spawn(async move { pending_store.save_current(pending_state).await });
        tokio::task::yield_now().await;
        state.lock().await.mode = "alert".into();
        release.send(()).unwrap();
        writer.await.unwrap();
        pending.await.unwrap().unwrap();
        assert_eq!(store.load().mode, "alert");
        fs::remove_file(path).unwrap();
    }
}
