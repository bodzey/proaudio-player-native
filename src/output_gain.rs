use std::sync::Arc;

use crate::audio_backend::{AudioBackend, BackendFuture, SinkState};

const DEFAULT_MASTER_SINK: &str = "proaudio_player_master";

pub trait OutputGain: Send + Sync {
    fn backend_name(&self) -> &'static str;
    fn state<'a>(&'a self, sink: &'a str) -> BackendFuture<'a, SinkState>;
    fn set_percent<'a>(&'a self, sink: &'a str, percent: f64) -> BackendFuture<'a, SinkState>;
    fn set_db<'a>(&'a self, sink: &'a str, db: f64) -> BackendFuture<'a, SinkState>;
    fn set_mute<'a>(&'a self, sink: &'a str, muted: bool) -> BackendFuture<'a, SinkState>;
}

/// Software master gain placed on the final logical mix bus.
///
/// The public OutputGain trait still accepts a sink argument for API compatibility,
/// but the default implementation deliberately owns one fixed logical master sink.
/// This prevents hardware/output selection from changing the gain stage and keeps
/// the processing order deterministic:
/// MUSIC + ALERT -> MASTER gain -> safety limiter -> physical sink.
pub struct BackendOutputGain {
    backend: Arc<dyn AudioBackend>,
    master_sink: String,
}

impl BackendOutputGain {
    pub fn new(backend: Arc<dyn AudioBackend>) -> Self {
        Self::with_master_sink(backend, DEFAULT_MASTER_SINK)
    }

    pub fn with_master_sink(backend: Arc<dyn AudioBackend>, master_sink: impl Into<String>) -> Self {
        Self {
            backend,
            master_sink: master_sink.into(),
        }
    }
}

impl OutputGain for BackendOutputGain {
    fn backend_name(&self) -> &'static str {
        "logical-master"
    }

    fn state<'a>(&'a self, _sink: &'a str) -> BackendFuture<'a, SinkState> {
        self.backend.sink_state(&self.master_sink)
    }

    fn set_percent<'a>(&'a self, _sink: &'a str, percent: f64) -> BackendFuture<'a, SinkState> {
        Box::pin(async move {
            let values = [percent];
            self.backend
                .set_sink_percent_channels(&self.master_sink, &values)
                .await
        })
    }

    fn set_db<'a>(&'a self, _sink: &'a str, db: f64) -> BackendFuture<'a, SinkState> {
        self.backend.set_sink_db(&self.master_sink, db)
    }

    fn set_mute<'a>(&'a self, _sink: &'a str, muted: bool) -> BackendFuture<'a, SinkState> {
        self.backend.set_sink_mute(&self.master_sink, muted)
    }
}
