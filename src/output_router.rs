use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use tokio::sync::{watch, Mutex};
use tokio::time::sleep;

use crate::atomic_file;
use crate::audio_backend::{AudioBackend, BackendFuture, SinkDescriptor};

pub const DEFAULT_MASTER_SINK: &str = "proaudio_player_master";
const DEFAULT_OUTPUT_FILE: &str = "/var/lib/proaudio-player-alert/audio-output.env";
const DEFAULT_STATE_FILE: &str = "/run/proaudio-player/proaudio-player-bus-modules";

#[derive(Debug, Clone, Default)]
pub struct OutputCapabilities {
    pub sample_format: Option<String>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub channel_map: Vec<String>,
    pub alsa_device: Option<u32>,
    pub device_api: Option<String>,
    pub device_bus: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OutputDescriptor {
    pub id: String,
    pub name: String,
    pub state: String,
    pub device_class: String,
    pub alsa_card: Option<u32>,
    pub capabilities: OutputCapabilities,
    pub selected: bool,
    pub available: bool,
}

pub trait OutputRouter: Send + Sync {
    fn backend_name(&self) -> &'static str;
    fn list_outputs(&self) -> BackendFuture<'_, Vec<OutputDescriptor>>;
    fn active_output(&self) -> BackendFuture<'_, OutputDescriptor>;
    fn select_output<'a>(&'a self, id: &'a str) -> BackendFuture<'a, OutputDescriptor>;
    fn subscribe_changes(&self) -> watch::Receiver<u64>;
}

/// Adapter around the firmware routing contract.
///
/// The player core speaks only in logical output descriptors. Persistence and
/// hotplug application remain an implementation detail behind this trait, so a
/// future native PipeWire router can replace this adapter without touching the
/// player, source arbiter, mixer API, or Web UI.
pub struct ExternalOutputRouter {
    backend: Arc<dyn AudioBackend>,
    music_sink: String,
    alert_sink: String,
    master_sink: String,
    output_file: PathBuf,
    state_file: PathBuf,
    control_lock: Mutex<()>,
}

impl ExternalOutputRouter {
    pub fn new(
        backend: Arc<dyn AudioBackend>,
        music_sink: impl Into<String>,
        alert_sink: impl Into<String>,
    ) -> Self {
        Self::with_paths(
            backend,
            music_sink,
            alert_sink,
            DEFAULT_MASTER_SINK,
            DEFAULT_OUTPUT_FILE,
            DEFAULT_STATE_FILE,
        )
    }

    pub fn with_paths(
        backend: Arc<dyn AudioBackend>,
        music_sink: impl Into<String>,
        alert_sink: impl Into<String>,
        master_sink: impl Into<String>,
        output_file: impl Into<PathBuf>,
        state_file: impl Into<PathBuf>,
    ) -> Self {
        Self {
            backend,
            music_sink: music_sink.into(),
            alert_sink: alert_sink.into(),
            master_sink: master_sink.into(),
            output_file: output_file.into(),
            state_file: state_file.into(),
            control_lock: Mutex::new(()),
        }
    }

    fn is_physical(&self, sink: &SinkDescriptor) -> bool {
        let name = sink.state.name.as_str();
        name != self.music_sink
            && name != self.alert_sink
            && name != self.master_sink
            && name != "auto_null"
            && !name.starts_with("proaudio_player_")
    }

    async fn candidates(&self) -> Result<Vec<SinkDescriptor>> {
        let mut candidates = self
            .backend
            .list_sinks()
            .await?
            .into_iter()
            .filter(|sink| self.is_physical(sink))
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| left.state.name.cmp(&right.state.name));
        Ok(candidates)
    }

    async fn read_key(path: &Path, key: &str) -> Option<String> {
        let text = tokio::fs::read_to_string(path).await.ok()?;
        text.lines()
            .find_map(|line| line.strip_prefix(key))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }

    async fn configured_output(&self) -> Option<String> {
        Self::read_key(&self.output_file, "PHYSICAL_SINK=")
            .await
            .filter(|value| value != "AUTO")
    }

    async fn routed_output(&self) -> Option<String> {
        Self::read_key(&self.state_file, "PHYSICAL=").await
    }

    async fn write_configured_output(&self, output: Option<&str>) -> Result<()> {
        let path = self.output_file.clone();
        let value = output.unwrap_or("AUTO").to_owned();
        tokio::task::spawn_blocking(move || {
            atomic_file::write(&path, format!("PHYSICAL_SINK={value}\n").as_bytes(), 0o600)
        })
        .await??;
        Ok(())
    }

    async fn selected_name(&self, candidates: &[SinkDescriptor]) -> Option<String> {
        if let Some(routed) = self.routed_output().await {
            if candidates.iter().any(|sink| sink.state.name == routed) {
                return Some(routed);
            }
        }
        if let Some(configured) = self.configured_output().await {
            if candidates.iter().any(|sink| sink.state.name == configured) {
                return Some(configured);
            }
        }
        candidates
            .iter()
            .find(|sink| sink.state_name.eq_ignore_ascii_case("running"))
            .or_else(|| candidates.first())
            .map(|sink| sink.state.name.clone())
    }

    fn capabilities(sink: &SinkDescriptor) -> OutputCapabilities {
        OutputCapabilities {
            sample_format: (!sink.sample_format.is_empty()).then(|| sink.sample_format.clone()),
            sample_rate: (sink.sample_rate > 0).then_some(sink.sample_rate),
            channels: (sink.channels > 0).then_some(u32::from(sink.channels)),
            channel_map: sink.channel_map.clone(),
            alsa_device: sink.alsa_device,
            device_api: (!sink.device_api.is_empty()).then(|| sink.device_api.clone()),
            device_bus: (!sink.device_bus.is_empty()).then(|| sink.device_bus.clone()),
        }
    }

    fn describe(sink: SinkDescriptor, selected: bool) -> OutputDescriptor {
        let capabilities = Self::capabilities(&sink);
        OutputDescriptor {
            id: sink.state.name.clone(),
            name: if sink.description.is_empty() {
                sink.state.name
            } else {
                sink.description
            },
            state: sink.state_name,
            device_class: sink.device_class,
            alsa_card: sink.alsa_card,
            capabilities,
            selected,
            available: true,
        }
    }

    async fn wait_for_routed_output(&self, expected: &str) -> bool {
        for _ in 0..100 {
            if self.routed_output().await.as_deref() == Some(expected) {
                return true;
            }
            sleep(Duration::from_millis(100)).await;
        }
        false
    }
}

impl OutputRouter for ExternalOutputRouter {
    fn backend_name(&self) -> &'static str {
        "firmware-router"
    }

    fn list_outputs(&self) -> BackendFuture<'_, Vec<OutputDescriptor>> {
        Box::pin(async move {
            let candidates = self.candidates().await?;
            let selected = self.selected_name(&candidates).await;
            Ok(candidates
                .into_iter()
                .map(|sink| {
                    let is_selected = selected.as_deref() == Some(sink.state.name.as_str());
                    Self::describe(sink, is_selected)
                })
                .collect())
        })
    }

    fn active_output(&self) -> BackendFuture<'_, OutputDescriptor> {
        Box::pin(async move {
            let candidates = self.candidates().await?;
            let selected = self
                .selected_name(&candidates)
                .await
                .ok_or_else(|| anyhow!("Фізичний аудіовихід не знайдено"))?;
            let sink = candidates
                .into_iter()
                .find(|sink| sink.state.name == selected)
                .ok_or_else(|| anyhow!("Активний аудіовихід зник"))?;
            Ok(Self::describe(sink, true))
        })
    }

    fn select_output<'a>(&'a self, id: &'a str) -> BackendFuture<'a, OutputDescriptor> {
        Box::pin(async move {
            let _guard = self.control_lock.lock().await;
            let id = id.trim();
            if id.is_empty() || id.chars().any(|value| matches!(value, '\n' | '\r' | '\0')) {
                bail!("Некоректний ідентифікатор аудіовиходу");
            }

            let candidates = self.candidates().await?;
            let selected = candidates
                .iter()
                .find(|sink| sink.state.name == id)
                .cloned()
                .ok_or_else(|| anyhow!("Вибраний аудіовихід зараз недоступний"))?;

            if self.routed_output().await.as_deref() == Some(id) {
                self.write_configured_output(Some(id)).await?;
                return Ok(Self::describe(selected, true));
            }

            // Roll back the persisted selection, not the transient AUTO fallback.
            // If AUTO was active before this request, a failed switch must keep AUTO
            // instead of silently pinning the currently routed physical sink.
            let previous_configured = self.configured_output().await;
            self.write_configured_output(Some(id)).await?;
            if self.wait_for_routed_output(id).await {
                return Ok(Self::describe(selected, true));
            }

            self.write_configured_output(previous_configured.as_deref())
                .await?;
            bail!("Не вдалося підтвердити перемикання аудіовиходу; попередній вибір відновлено")
        })
    }

    fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.backend.subscribe_changes()
    }
}
