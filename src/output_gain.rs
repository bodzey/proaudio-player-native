use std::sync::Arc;

use crate::audio_backend::{AudioBackend, BackendFuture, SinkState};

pub trait OutputGain: Send + Sync {
    fn backend_name(&self) -> &'static str;
    fn state<'a>(&'a self, sink: &'a str) -> BackendFuture<'a, SinkState>;
    fn set_percent<'a>(&'a self, sink: &'a str, percent: f64) -> BackendFuture<'a, SinkState>;
    fn set_db<'a>(&'a self, sink: &'a str, db: f64) -> BackendFuture<'a, SinkState>;
    fn set_mute<'a>(&'a self, sink: &'a str, muted: bool) -> BackendFuture<'a, SinkState>;
}

pub struct BackendOutputGain {
    backend: Arc<dyn AudioBackend>,
}

impl BackendOutputGain {
    pub fn new(backend: Arc<dyn AudioBackend>) -> Self {
        Self { backend }
    }
}

impl OutputGain for BackendOutputGain {
    fn backend_name(&self) -> &'static str {
        "logical-sink"
    }

    fn state<'a>(&'a self, sink: &'a str) -> BackendFuture<'a, SinkState> {
        self.backend.sink_state(sink)
    }

    fn set_percent<'a>(&'a self, sink: &'a str, percent: f64) -> BackendFuture<'a, SinkState> {
        Box::pin(async move {
            let values = [percent];
            self.backend.set_sink_percent_channels(sink, &values).await
        })
    }

    fn set_db<'a>(&'a self, sink: &'a str, db: f64) -> BackendFuture<'a, SinkState> {
        self.backend.set_sink_db(sink, db)
    }

    fn set_mute<'a>(&'a self, sink: &'a str, muted: bool) -> BackendFuture<'a, SinkState> {
        self.backend.set_sink_mute(sink, muted)
    }
}
