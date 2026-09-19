use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use anyhow::Result;
use tokio::sync::watch;

pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

#[derive(Debug, Clone)]
pub struct SinkState {
    pub name: String,
    pub index: u32,
    pub volumes_percent: Vec<f64>,
    pub volumes_db: Vec<f64>,
    pub muted: bool,
}

impl SinkState {
    pub fn average_percent(&self) -> f64 {
        average(&self.volumes_percent).unwrap_or(0.0)
    }

    pub fn average_db(&self) -> f64 {
        average(&self.volumes_db).unwrap_or(-60.0).clamp(-60.0, 0.0)
    }
}

#[derive(Debug, Clone)]
pub struct SinkDescriptor {
    pub state: SinkState,
    pub description: String,
    pub device_class: String,
    pub alsa_card: Option<u32>,
    pub alsa_device: Option<u32>,
    pub sample_format: String,
    pub sample_rate: u32,
    pub channels: u8,
    pub channel_map: Vec<String>,
    pub device_api: String,
    pub device_bus: String,
    pub state_name: String,
}

#[derive(Debug, Clone)]
pub struct StreamState {
    pub index: u32,
    pub sink: u32,
    pub name: String,
    pub properties: HashMap<String, String>,
    pub volumes_percent: Vec<f64>,
    pub muted: bool,
    pub corked: bool,
    pub has_volume: bool,
    pub volume_writable: bool,
}

impl StreamState {
    pub fn property(&self, key: &str) -> &str {
        self.properties.get(key).map(String::as_str).unwrap_or("")
    }

    pub fn volume_is_unity(&self) -> bool {
        self.has_volume
            && !self.volumes_percent.is_empty()
            && self
                .volumes_percent
                .iter()
                .all(|volume| (*volume - 100.0).abs() <= 0.1)
    }
}

pub trait AudioBackend: Send + Sync {
    fn backend_name(&self) -> &'static str;
    fn sink_state<'a>(&'a self, name: &'a str) -> BackendFuture<'a, SinkState>;
    fn list_sinks(&self) -> BackendFuture<'_, Vec<SinkDescriptor>>;
    fn list_sink_inputs(&self) -> BackendFuture<'_, Vec<StreamState>>;
    fn set_sink_percent_channels<'a>(
        &'a self,
        name: &'a str,
        values: &'a [f64],
    ) -> BackendFuture<'a, SinkState>;
    fn set_sink_db<'a>(&'a self, name: &'a str, db: f64) -> BackendFuture<'a, SinkState>;
    fn set_sink_mute<'a>(&'a self, name: &'a str, muted: bool) -> BackendFuture<'a, SinkState>;
    fn set_sink_input_percent(&self, index: u32, percent: f64) -> BackendFuture<'_, StreamState>;
    fn set_sink_input_mute(&self, index: u32, muted: bool) -> BackendFuture<'_, StreamState>;
    fn subscribe_changes(&self) -> watch::Receiver<u64>;
    fn subscribe_topology_changes(&self) -> watch::Receiver<u64> {
        self.subscribe_changes()
    }
}

pub fn percent_to_linear(percent: f64) -> f64 {
    let normalized = (percent / 100.0).clamp(0.0, 1.0);
    normalized * normalized * normalized
}

pub fn linear_to_percent(linear: f64) -> f64 {
    linear.clamp(0.0, 1.0).cbrt() * 100.0
}

fn average(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

#[cfg(test)]
mod tests {
    use super::{linear_to_percent, percent_to_linear};

    #[test]
    fn volume_curve_round_trips() {
        for percent in [0.0, 1.0, 25.0, 50.0, 75.0, 100.0] {
            let restored = linear_to_percent(percent_to_linear(percent));
            assert!((restored - percent).abs() < 1e-9);
        }
    }

    #[test]
    fn volume_curve_stays_in_unity_range() {
        assert_eq!(percent_to_linear(-10.0), 0.0);
        assert_eq!(percent_to_linear(120.0), 1.0);
        assert_eq!(linear_to_percent(-1.0), 0.0);
        assert_eq!(linear_to_percent(2.0), 100.0);
    }

    #[test]
    fn pulse_percentage_represents_the_requested_amplitude_db() {
        for db in [-60.0, -18.0, -12.0, -6.0, 0.0] {
            let percent = linear_to_percent(10f64.powf(db / 20.0));
            let restored_db = 20.0 * percent_to_linear(percent).log10();
            assert!((restored_db - db).abs() < 1.0e-10);
        }
    }
}
