use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::sleep;
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
}

impl StateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
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
    changes: watch::Sender<u64>,
}

impl MixerStateRuntime {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let store = MixerStateStore::new(path);
        let state = Arc::new(Mutex::new(store.load()));
        let (changes, _) = watch::channel(0_u64);
        Self {
            store,
            state,
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

    pub fn start_writer(&self) -> JoinHandle<()> {
        let state = self.state.clone();
        let store = self.store.clone();
        let mut changes = self.changes.subscribe();
        tokio::spawn(async move {
            loop {
                if changes.changed().await.is_err() {
                    return;
                }

                // Browser faders can emit ~30 updates/second. Coalesce them to one
                // durable write after the control settles so flash/eMMC is not
                // hammered by UI traffic.
                sleep(Duration::from_millis(750)).await;
                while changes.has_changed().unwrap_or(false) {
                    let _ = changes.borrow_and_update();
                    sleep(Duration::from_millis(100)).await;
                }

                let snapshot = state
                    .lock()
                    .map(|value| value.clone())
                    .unwrap_or_default();
                let writer = store.clone();
                match tokio::task::spawn_blocking(move || writer.save(&snapshot)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => error!("Не вдалося зберегти mixer state: {err:#}"),
                    Err(err) => error!("Mixer state writer завершився з помилкою: {err}"),
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
}
