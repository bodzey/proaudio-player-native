use std::collections::{hash_map::DefaultHasher, HashMap};
use std::hash::{Hash, Hasher};
use std::process::Command as StdCommand;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{Map, Value};
use tokio::process::Command;
use tokio::sync::watch;
use tracing::debug;

use crate::audio_backend::{
    linear_to_percent, percent_to_linear, AudioBackend, BackendFuture, SinkDescriptor, SinkState,
    StreamState,
};

const TOPOLOGY_POLL_INTERVAL: Duration = Duration::from_millis(500);
const STREAM_PROPERTIES: &[&str] = &[
    "application.name",
    "application.process.binary",
    "application.process.id",
    "media.role",
    "media.title",
    "media.name",
];

#[derive(Clone)]
pub struct PipeWireControl {
    changes: watch::Sender<u64>,
}

impl PipeWireControl {
    pub fn new() -> Result<Self> {
        let (changes, _) = watch::channel(0_u64);
        spawn_topology_watcher(changes.clone());
        Ok(Self { changes })
    }

    async fn dump() -> Result<Vec<Value>> {
        let output = Command::new("pw-dump")
            .arg("-N")
            .output()
            .await
            .context("failed to execute pw-dump")?;
        if !output.status.success() {
            bail!(
                "pw-dump failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        serde_json::from_slice::<Vec<Value>>(&output.stdout)
            .context("failed to parse PipeWire graph from pw-dump")
    }

    async fn run_wpctl(args: &[String]) -> Result<String> {
        let output = Command::new("wpctl")
            .args(args)
            .output()
            .await
            .context("failed to execute wpctl")?;
        if !output.status.success() {
            bail!(
                "wpctl {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    async fn volume_state(id: u32) -> Result<(f64, bool)> {
        let text = Self::run_wpctl(&["get-volume".into(), id.to_string()]).await?;
        let mut words = text.split_whitespace();
        let label = words.next().unwrap_or_default().trim_end_matches(':');
        if label != "Volume" {
            bail!("unexpected wpctl volume response: {text}");
        }
        let linear = words
            .next()
            .ok_or_else(|| anyhow!("wpctl did not return a volume value"))?
            .parse::<f64>()
            .context("invalid wpctl volume value")?;
        let muted = text.contains("[MUTED]");
        Ok((linear.max(0.0), muted))
    }

    async fn set_linear(id: u32, linear: f64) -> Result<()> {
        Self::run_wpctl(&[
            "set-volume".into(),
            id.to_string(),
            format!("{:.9}", linear.clamp(0.0, 1.0)),
        ])
        .await?;
        Ok(())
    }

    async fn set_muted(id: u32, muted: bool) -> Result<()> {
        Self::run_wpctl(&[
            "set-mute".into(),
            id.to_string(),
            if muted { "1" } else { "0" }.into(),
        ])
        .await?;
        Ok(())
    }

    async fn sink_state_by_id(id: u32, name: String) -> Result<SinkState> {
        let (linear, muted) = Self::volume_state(id).await?;
        Ok(sink_state(id, name, linear, muted))
    }

    async fn sink_state(&self, name: &str) -> Result<SinkState> {
        let dump = Self::dump().await?;
        let node = find_node_by_name(&dump, name)
            .ok_or_else(|| anyhow!("PipeWire sink not found: {name}"))?;
        let id = object_id(node)?;
        Self::sink_state_by_id(id, name.to_owned()).await
    }

    async fn list_sinks(&self) -> Result<Vec<SinkDescriptor>> {
        let dump = Self::dump().await?;
        let mut sinks = Vec::new();

        for object in &dump {
            if object_type(object) != Some("PipeWire:Interface:Node") {
                continue;
            }
            let Some(props) = props(object) else {
                continue;
            };
            if prop(props, "media.class") != Some("Audio/Sink") {
                continue;
            }
            let Some(name) = prop(props, "node.name").map(str::to_owned) else {
                continue;
            };
            let id = object_id(object)?;
            let (linear, muted) = Self::volume_state(id).await.unwrap_or((1.0, false));
            sinks.push(descriptor_from_node(object, sink_state(id, name, linear, muted))?);
        }

        sinks.sort_by(|left, right| left.state.name.cmp(&right.state.name));
        Ok(sinks)
    }

    async fn list_sink_inputs(&self) -> Result<Vec<StreamState>> {
        let dump = Self::dump().await?;
        let targets = stream_targets(&dump);
        let mut streams = Vec::new();

        for object in &dump {
            if object_type(object) != Some("PipeWire:Interface:Node") {
                continue;
            }
            let Some(node_props) = props(object) else {
                continue;
            };
            if prop(node_props, "media.class") != Some("Stream/Output/Audio") {
                continue;
            }
            let id = object_id(object)?;
            let Some(target) = targets.get(&id).copied() else {
                continue;
            };
            let (linear, muted) = Self::volume_state(id).await.unwrap_or((1.0, false));
            streams.push(stream_from_node(object, target, linear, muted)?);
        }

        streams.sort_by_key(|stream| stream.index);
        Ok(streams)
    }

    async fn stream_state(&self, id: u32) -> Result<StreamState> {
        let dump = Self::dump().await?;
        let targets = stream_targets(&dump);
        let target = targets
            .get(&id)
            .copied()
            .ok_or_else(|| anyhow!("PipeWire stream {id} is not linked to an audio sink"))?;
        let object = dump
            .iter()
            .find(|object| {
                object_type(object) == Some("PipeWire:Interface:Node")
                    && object_id(object).ok() == Some(id)
            })
            .ok_or_else(|| anyhow!("PipeWire stream not found: {id}"))?;
        let (linear, muted) = Self::volume_state(id).await?;
        stream_from_node(object, target, linear, muted)
    }

    async fn set_percent_channels(&self, name: &str, values: &[f64]) -> Result<SinkState> {
        if values.is_empty() {
            bail!("volume channel list is empty");
        }
        let state = self.sink_state(name).await?;
        let average = values.iter().copied().sum::<f64>() / values.len() as f64;
        Self::set_linear(state.index, percent_to_linear(average)).await?;
        Self::sink_state_by_id(state.index, state.name).await
    }

    async fn set_db(&self, name: &str, db: f64) -> Result<SinkState> {
        let state = self.sink_state(name).await?;
        let linear = if db.is_finite() {
            10f64.powf(db / 20.0).clamp(0.0, 1.0)
        } else {
            0.0
        };
        Self::set_linear(state.index, linear).await?;
        Self::sink_state_by_id(state.index, state.name).await
    }

    async fn set_mute(&self, name: &str, muted: bool) -> Result<SinkState> {
        let state = self.sink_state(name).await?;
        Self::set_muted(state.index, muted).await?;
        Self::sink_state_by_id(state.index, state.name).await
    }

    async fn set_sink_input_percent(&self, index: u32, percent: f64) -> Result<StreamState> {
        Self::set_linear(index, percent_to_linear(percent)).await?;
        self.stream_state(index).await
    }

    async fn set_sink_input_mute(&self, index: u32, muted: bool) -> Result<StreamState> {
        Self::set_muted(index, muted).await?;
        self.stream_state(index).await
    }
}

impl AudioBackend for PipeWireControl {
    fn backend_name(&self) -> &'static str {
        "pipewire"
    }

    fn sink_state<'a>(&'a self, name: &'a str) -> BackendFuture<'a, SinkState> {
        Box::pin(PipeWireControl::sink_state(self, name))
    }

    fn list_sinks(&self) -> BackendFuture<'_, Vec<SinkDescriptor>> {
        Box::pin(PipeWireControl::list_sinks(self))
    }

    fn list_sink_inputs(&self) -> BackendFuture<'_, Vec<StreamState>> {
        Box::pin(PipeWireControl::list_sink_inputs(self))
    }

    fn set_sink_percent_channels<'a>(
        &'a self,
        name: &'a str,
        values: &'a [f64],
    ) -> BackendFuture<'a, SinkState> {
        Box::pin(PipeWireControl::set_percent_channels(self, name, values))
    }

    fn set_sink_db<'a>(&'a self, name: &'a str, db: f64) -> BackendFuture<'a, SinkState> {
        Box::pin(PipeWireControl::set_db(self, name, db))
    }

    fn set_sink_mute<'a>(&'a self, name: &'a str, muted: bool) -> BackendFuture<'a, SinkState> {
        Box::pin(PipeWireControl::set_mute(self, name, muted))
    }

    fn set_sink_input_percent(&self, index: u32, percent: f64) -> BackendFuture<'_, StreamState> {
        Box::pin(PipeWireControl::set_sink_input_percent(self, index, percent))
    }

    fn set_sink_input_mute(&self, index: u32, muted: bool) -> BackendFuture<'_, StreamState> {
        Box::pin(PipeWireControl::set_sink_input_mute(self, index, muted))
    }

    fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }
}

fn sink_state(id: u32, name: String, linear: f64, muted: bool) -> SinkState {
    let percent = linear_to_percent(linear);
    let db = amplitude_db(linear);
    SinkState {
        name,
        index: id,
        volumes_percent: vec![percent, percent],
        volumes_db: vec![db, db],
        muted,
    }
}

fn descriptor_from_node(object: &Value, state: SinkState) -> Result<SinkDescriptor> {
    let node_props = props(object).ok_or_else(|| anyhow!("PipeWire node has no properties"))?;
    let channel_map = prop(node_props, "audio.position")
        .map(parse_channel_map)
        .unwrap_or_default();
    let channels = prop_u32(node_props, "audio.channels")
        .or_else(|| (!channel_map.is_empty()).then_some(channel_map.len() as u32))
        .unwrap_or(0)
        .min(u8::MAX as u32) as u8;

    Ok(SinkDescriptor {
        description: prop(node_props, "node.description")
            .or_else(|| prop(node_props, "node.nick"))
            .unwrap_or(state.name.as_str())
            .to_owned(),
        device_class: prop(node_props, "device.class").unwrap_or_default().to_owned(),
        alsa_card: prop_u32(node_props, "alsa.card")
            .or_else(|| prop_u32(node_props, "api.alsa.card"))
            .or_else(|| prop_u32(node_props, "api.alsa.pcm.card")),
        alsa_device: prop_u32(node_props, "alsa.device")
            .or_else(|| prop_u32(node_props, "api.alsa.device"))
            .or_else(|| prop_u32(node_props, "api.alsa.pcm.device")),
        sample_format: prop(node_props, "audio.format").unwrap_or_default().to_owned(),
        sample_rate: prop_u32(node_props, "audio.rate").unwrap_or(0),
        channels,
        channel_map,
        device_api: prop(node_props, "device.api").unwrap_or_default().to_owned(),
        device_bus: prop(node_props, "device.bus").unwrap_or_default().to_owned(),
        state_name: info(object)
            .and_then(|value| value.get("state"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        state,
    })
}

fn stream_from_node(object: &Value, sink: u32, linear: f64, muted: bool) -> Result<StreamState> {
    let node_props = props(object).ok_or_else(|| anyhow!("PipeWire stream has no properties"))?;
    let properties = STREAM_PROPERTIES
        .iter()
        .filter_map(|key| prop(node_props, key).map(|value| ((*key).to_owned(), value.to_owned())))
        .collect();
    let index = object_id(object)?;
    let state_name = info(object)
        .and_then(|value| value.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let percent = linear_to_percent(linear);

    Ok(StreamState {
        index,
        sink,
        name: prop(node_props, "media.name")
            .or_else(|| prop(node_props, "node.description"))
            .or_else(|| prop(node_props, "node.name"))
            .unwrap_or_default()
            .to_owned(),
        properties,
        volumes_percent: vec![percent, percent],
        muted,
        corked: state_name != "running",
        has_volume: true,
        volume_writable: true,
    })
}

fn stream_targets(dump: &[Value]) -> HashMap<u32, u32> {
    let sink_ids = dump
        .iter()
        .filter(|object| object_type(object) == Some("PipeWire:Interface:Node"))
        .filter_map(|object| {
            let node_props = props(object)?;
            (prop(node_props, "media.class") == Some("Audio/Sink"))
                .then(|| object_id(object).ok())
                .flatten()
        })
        .collect::<std::collections::HashSet<_>>();

    let mut result = HashMap::new();
    for object in dump {
        if object_type(object) != Some("PipeWire:Interface:Link") {
            continue;
        }
        let Some(link_info) = info(object) else {
            continue;
        };
        let Some(output) = json_u32(link_info.get("output-node-id")) else {
            continue;
        };
        let Some(input) = json_u32(link_info.get("input-node-id")) else {
            continue;
        };
        if sink_ids.contains(&input) {
            result.entry(output).or_insert(input);
        }
    }
    result
}

fn find_node_by_name<'a>(dump: &'a [Value], name: &str) -> Option<&'a Value> {
    dump.iter().find(|object| {
        object_type(object) == Some("PipeWire:Interface:Node")
            && props(object).and_then(|node_props| prop(node_props, "node.name")) == Some(name)
    })
}

fn object_type(object: &Value) -> Option<&str> {
    object.get("type")?.as_str()
}

fn object_id(object: &Value) -> Result<u32> {
    json_u32(object.get("id")).ok_or_else(|| anyhow!("PipeWire object has no numeric id"))
}

fn info(object: &Value) -> Option<&Map<String, Value>> {
    object.get("info")?.as_object()
}

fn props(object: &Value) -> Option<&Map<String, Value>> {
    info(object)?.get("props")?.as_object()
}

fn prop<'a>(props: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    match props.get(key)? {
        Value::String(value) => Some(value.as_str()),
        _ => None,
    }
}

fn prop_u32(props: &Map<String, Value>, key: &str) -> Option<u32> {
    match props.get(key)? {
        Value::Number(value) => value.as_u64().and_then(|value| u32::try_from(value).ok()),
        Value::String(value) => value.parse::<u32>().ok(),
        _ => None,
    }
}

fn json_u32(value: Option<&Value>) -> Option<u32> {
    match value? {
        Value::Number(number) => number.as_u64().and_then(|value| u32::try_from(value).ok()),
        Value::String(value) => value.parse::<u32>().ok(),
        _ => None,
    }
}

fn parse_channel_map(text: &str) -> Vec<String> {
    text.trim_matches(|ch| matches!(ch, '[' | ']'))
        .split(|ch: char| ch == ',' || ch.is_whitespace())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
        .collect()
}

fn amplitude_db(linear: f64) -> f64 {
    if !linear.is_finite() || linear <= 0.0 {
        -200.0
    } else {
        20.0 * linear.log10()
    }
}

fn spawn_topology_watcher(changes: watch::Sender<u64>) {
    thread::Builder::new()
        .name("proaudio-pipewire-watch".into())
        .spawn(move || {
            let mut previous_hash = None;
            let mut generation = 0_u64;
            loop {
                let current_hash = StdCommand::new("pw-dump")
                    .arg("-N")
                    .output()
                    .ok()
                    .filter(|output| output.status.success())
                    .map(|output| {
                        let mut hasher = DefaultHasher::new();
                        output.stdout.hash(&mut hasher);
                        hasher.finish()
                    });

                if current_hash != previous_hash {
                    previous_hash = current_hash;
                    generation = generation.wrapping_add(1);
                    changes.send_replace(generation);
                    debug!(generation, "PipeWire graph changed");
                }
                thread::sleep(TOPOLOGY_POLL_INTERVAL);
            }
        })
        .expect("failed to spawn PipeWire topology watcher");
}

#[cfg(test)]
mod tests {
    use super::{amplitude_db, parse_channel_map};

    #[test]
    fn parses_pipewire_channel_positions() {
        assert_eq!(parse_channel_map("[ FL, FR ]"), vec!["fl", "fr"]);
        assert_eq!(parse_channel_map("FL FR"), vec!["fl", "fr"]);
    }

    #[test]
    fn linear_volume_maps_to_amplitude_db() {
        assert!((amplitude_db(1.0) - 0.0).abs() < 1.0e-12);
        assert!((amplitude_db(0.5) + 6.020_599_913).abs() < 1.0e-6);
        assert_eq!(amplitude_db(0.0), -200.0);
    }
}
