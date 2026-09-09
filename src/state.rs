use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

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
            mode: "normal".into(), clear_count: 0, entry_announced: false,
            clear_announced: false, audio_snapshot: None, last_success_at: None,
            last_change_at: None, last_error: None, matched_uids: Vec::new(),
            last_minute_silence_date: None, minute_silence_active: false,
            minute_silence_snapshot: None,
        }
    }
}

#[derive(Clone)]
pub struct StateStore { path: PathBuf }

impl StateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self { Self { path: path.into() } }
    pub fn path(&self) -> &Path { &self.path }

    pub fn load(&self) -> RuntimeState {
        let Ok(text) = fs::read_to_string(&self.path) else { return RuntimeState::default(); };
        let Ok(state) = serde_json::from_str::<RuntimeState>(&text) else { return RuntimeState::default(); };
        if matches!(state.mode.as_str(), "normal" | "alert") { state } else { RuntimeState::default() }
    }

    pub fn save(&self, state: &RuntimeState) -> Result<()> {
        if let Some(parent) = self.path.parent() { fs::create_dir_all(parent)?; }
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        fs::rename(tmp, &self.path)?;
        Ok(())
    }
}
