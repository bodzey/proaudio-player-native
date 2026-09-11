use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use tokio::sync::{watch, Mutex};
use tokio::time::sleep;

use crate::audio_backend::{AudioBackend, BackendFuture, SinkDescriptor};
use crate::command;

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
        if let Some(output) = output {
            if let Some(parent) = self.output_file.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let temporary = self.output_file.with_extension("env.tmp");
            tokio::fs::write(&temporary, format!("PHYSICAL_SINK={output}\n")).await?;
            tokio::fs::rename(&temporary, &self.output_file).await?;
        } else if tokio::fs::try_exists(&self.output_file).await.unwrap_or(false) {
            tokio::fs::remove_file(&self.output_file).await?;
        }
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

    async fn runtime_capabilities() -> HashMap<String, OutputCapabilities> {
        let Ok(output) = command::run("pactl", &["-f", "json", "list", "sinks"], false, 3).await
        else {
            return HashMap::new();
        };
        if output.code != 0 || output.stdout.trim().is_empty() {
            return HashMap::new();
        }
        let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&output.stdout) else {
            return HashMap::new();
        };

        let mut result = HashMap::new();
        for item in items {
            let Some(name) = item.get("name").and_then(Value::as_str) else {
                continue;
            };
            let mut capabilities = OutputCapabilities::default();
            if let Some(spec) = item
                .get("sample_specification")
                .and_then(Value::as_str)
            {
                let fields = spec.split_whitespace().collect::<Vec<_>>();
                capabilities.sample_format = fields.first().map(|value| (*value).to_owned());
                capabilities.channels = fields
                    .iter()
                    .find_map(|value| value.strip_suffix("ch"))
                    .and_then(|value| value.parse::<u32>().ok());
                capabilities.sample_rate = fields
                    .iter()
                    .find_map(|value| value.strip_suffix("Hz"))
                    .and_then(|value| value.parse::<u32>().ok());
            }
            if let Some(map) = item.get("channel_map").and_then(Value::as_str) {
                capabilities.channel_map = map
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
            if let Some(properties) = item.get("properties").and_then(Value::as_object) {
                let property = |key: &str| {
                    properties
                        .get(key)
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                };
                capabilities.alsa_device = property("alsa.device")
                    .and_then(|value| value.parse::<u32>().ok());
                capabilities.device_api = property("device.api");
                capabilities.device_bus = property("device.bus");
            }
            result.insert(name.to_owned(), capabilities);
        }
        result
    }

    fn describe(
        sink: SinkDescriptor,
        selected: bool,
        capabilities: OutputCapabilities,
    ) -> OutputDescriptor {
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
            let mut capabilities = Self::runtime_capabilities().await;
            Ok(candidates
                .into_iter()
                .map(|sink| {
                    let is_selected = selected.as_deref() == Some(sink.state.name.as_str());
                    let caps = capabilities.remove(&sink.state.name).unwrap_or_default();
                    Self::describe(sink, is_selected, caps)
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
            let caps = Self::runtime_capabilities()
                .await
                .remove(&sink.state.name)
                .unwrap_or_default();
            Ok(Self::describe(sink, true, caps))
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
                let caps = Self::runtime_capabilities()
                    .await
                    .remove(id)
                    .unwrap_or_default();
                return Ok(Self::describe(selected, true, caps));
            }

            let previous = self.configured_output().await.or(self.routed_output().await);
            self.write_configured_output(Some(id)).await?;
            if self.wait_for_routed_output(id).await {
                let caps = Self::runtime_capabilities()
                    .await
                    .remove(id)
                    .unwrap_or_default();
                return Ok(Self::describe(selected, true, caps));
            }

            self.write_configured_output(previous.as_deref()).await?;
            bail!("Не вдалося підтвердити перемикання аудіовиходу; попередній вибір відновлено")
        })
    }

    fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.backend.subscribe_changes()
    }
}
