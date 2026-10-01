use std::sync::Mutex;

use anyhow::bail;
use tokio::sync::watch;

use super::*;
use crate::audio_backend::{AudioBackend, BackendFuture, SinkDescriptor, SinkState};

fn stream(index: u32) -> StreamState {
    StreamState {
        index,
        sink: 1,
        name: String::new(),
        properties: HashMap::new(),
        volumes_percent: vec![100.0, 100.0],
        muted: false,
        corked: false,
        has_volume: true,
        volume_writable: true,
    }
}

#[derive(Debug, PartialEq)]
enum Operation {
    Mute(u32, bool),
    Gain(u32, f64),
}

#[derive(Default)]
struct BackendState {
    streams: Vec<StreamState>,
    operations: Vec<Operation>,
    fail_mute: Option<u32>,
    fail_gain: bool,
}

struct TestBackend {
    state: Mutex<BackendState>,
    changes: watch::Sender<u64>,
}

impl TestBackend {
    fn new(streams: Vec<StreamState>) -> Self {
        Self {
            state: Mutex::new(BackendState {
                streams,
                ..BackendState::default()
            }),
            changes: watch::channel(0).0,
        }
    }
}

impl AudioBackend for TestBackend {
    fn backend_name(&self) -> &'static str {
        "test"
    }

    fn sink_state<'a>(&'a self, name: &'a str) -> BackendFuture<'a, SinkState> {
        Box::pin(async move {
            Ok(SinkState {
                name: name.into(),
                index: 1,
                volumes_percent: vec![100.0, 100.0],
                volumes_db: vec![0.0, 0.0],
                muted: false,
            })
        })
    }

    fn list_sink_inputs(&self) -> BackendFuture<'_, Vec<StreamState>> {
        Box::pin(async move { Ok(self.state.lock().unwrap().streams.clone()) })
    }

    fn set_sink_input_percent(&self, index: u32, percent: f64) -> BackendFuture<'_, StreamState> {
        Box::pin(async move {
            let mut state = self.state.lock().unwrap();
            state.operations.push(Operation::Gain(index, percent));
            if state.fail_gain {
                bail!("gain operation failed");
            }
            let stream = state.streams.iter_mut().find(|s| s.index == index).unwrap();
            stream.volumes_percent = vec![percent, percent];
            Ok(stream.clone())
        })
    }

    fn set_sink_input_mute(&self, index: u32, muted: bool) -> BackendFuture<'_, StreamState> {
        Box::pin(async move {
            let mut state = self.state.lock().unwrap();
            state.operations.push(Operation::Mute(index, muted));
            if state.fail_mute == Some(index) {
                bail!("mute operation failed");
            }
            if !muted {
                assert!(state.streams.iter().all(|s| s.index == index || s.muted));
            }
            let stream = state.streams.iter_mut().find(|s| s.index == index).unwrap();
            stream.muted = muted;
            Ok(stream.clone())
        })
    }

    fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn list_sinks(&self) -> BackendFuture<'_, Vec<SinkDescriptor>> {
        unreachable!()
    }

    fn set_sink_percent_channels<'a>(
        &'a self,
        _name: &'a str,
        _values: &'a [f64],
    ) -> BackendFuture<'a, SinkState> {
        unreachable!()
    }

    fn set_sink_db<'a>(&'a self, _name: &'a str, _db: f64) -> BackendFuture<'a, SinkState> {
        unreachable!()
    }

    fn set_sink_mute<'a>(&'a self, _name: &'a str, _muted: bool) -> BackendFuture<'a, SinkState> {
        unreachable!()
    }
}

fn arbiter(backend: Arc<TestBackend>) -> SourceArbiter {
    let config = Arc::new(AppConfig::default());
    let audio = AudioEngine::new(config.clone(), backend);
    SourceArbiter::new(config, audio, Arc::new(RwLock::new(None)))
}

#[test]
fn newest_stream_is_the_only_audible_stream_for_a_source() {
    let suppressed = HashSet::new();
    assert_eq!(
        SourceArbiter::audible_stream_index(&[stream(7), stream(12), stream(9)], &suppressed),
        Some(12)
    );
    assert_eq!(SourceArbiter::audible_stream_index(&[], &suppressed), None);
}

#[test]
fn suppressed_stream_is_not_selected_again() {
    let suppressed = HashSet::from([12]);
    assert_eq!(
        SourceArbiter::audible_stream_index(&[stream(7), stream(12), stream(9)], &suppressed),
        Some(9)
    );
}

#[tokio::test]
async fn losers_and_duplicates_are_muted_before_winner_gain_and_unmute() {
    let mut winner = stream(5);
    winner.muted = true;
    winner.volumes_percent = vec![150.0, 150.0];
    let grouped = HashMap::from([
        ("old".into(), vec![stream(1)]),
        ("new".into(), vec![stream(4), winner.clone()]),
    ]);
    let backend = Arc::new(TestBackend::new(vec![stream(1), stream(4), winner]));
    arbiter(backend.clone())
        .apply_mix_policy(&grouped, Some(5))
        .await
        .unwrap();
    let state = backend.state.lock().unwrap();
    assert_eq!(state.operations.len(), 4);
    assert!(state.operations[..2].contains(&Operation::Mute(1, true)));
    assert!(state.operations[..2].contains(&Operation::Mute(4, true)));
    assert_eq!(state.operations[2], Operation::Gain(5, 100.0));
    assert_eq!(state.operations[3], Operation::Mute(5, false));
}

#[tokio::test]
async fn failed_loser_mute_keeps_new_source_silent_and_retries_selection() {
    let backend = Arc::new(TestBackend::new(vec![stream(1), stream(2)]));
    backend.state.lock().unwrap().fail_mute = Some(1);
    let mut arbiter = arbiter(backend.clone());
    assert!(arbiter.reconcile().await.is_err());
    assert!(arbiter.winner.is_none());
    assert!(arbiter.shared_winner.read().await.is_none());
    assert!(arbiter.active_streams.is_empty());
    {
        let mut state = backend.state.lock().unwrap();
        assert!(state.streams.iter().find(|s| s.index == 2).unwrap().muted);
        assert!(!state.operations.contains(&Operation::Mute(2, false)));
        state.fail_mute = None;
    }
    arbiter.reconcile().await.unwrap();
    assert_eq!(
        arbiter.shared_winner.read().await.as_deref(),
        Some("other:2")
    );
    assert!(backend
        .state
        .lock()
        .unwrap()
        .operations
        .ends_with(&[Operation::Mute(1, true), Operation::Mute(2, false)]));
}

#[tokio::test]
async fn failed_gain_normalization_mutes_an_already_audible_winner() {
    let mut winner = stream(2);
    winner.volumes_percent = vec![150.0, 150.0];
    let backend = Arc::new(TestBackend::new(vec![stream(1), winner]));
    backend.state.lock().unwrap().fail_gain = true;
    let mut arbiter = arbiter(backend.clone());
    assert!(arbiter.reconcile().await.is_err());
    let state = backend.state.lock().unwrap();
    assert_eq!(
        state.operations,
        vec![
            Operation::Mute(1, true),
            Operation::Gain(2, 100.0),
            Operation::Mute(2, true)
        ]
    );
    assert!(state.streams.iter().all(|s| s.muted));
    assert!(arbiter.winner.is_none());
}
