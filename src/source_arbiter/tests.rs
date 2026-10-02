use std::sync::atomic::{AtomicU64, Ordering};
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
    fail_list: bool,
    reconnect_on_list: bool,
    reconnect_on_mute: bool,
    reconnect_on_sink: bool,
}

struct TestBackend {
    state: Mutex<BackendState>,
    changes: watch::Sender<u64>,
    epoch: AtomicU64,
}

impl TestBackend {
    fn new(streams: Vec<StreamState>) -> Self {
        Self {
            state: Mutex::new(BackendState {
                streams,
                ..BackendState::default()
            }),
            changes: watch::channel(0).0,
            epoch: AtomicU64::new(0),
        }
    }
}

impl AudioBackend for TestBackend {
    fn backend_name(&self) -> &'static str {
        "test"
    }

    fn connection_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    fn sink_state<'a>(&'a self, name: &'a str) -> BackendFuture<'a, SinkState> {
        Box::pin(async move {
            let mut state = self.state.lock().unwrap();
            if state.reconnect_on_sink {
                state.reconnect_on_sink = false;
                self.epoch.fetch_add(1, Ordering::SeqCst);
            }
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
        Box::pin(async move {
            let mut state = self.state.lock().unwrap();
            if state.fail_list {
                bail!("stream query failed");
            }
            if state.reconnect_on_list {
                state.reconnect_on_list = false;
                self.epoch.fetch_add(1, Ordering::SeqCst);
            }
            Ok(state.streams.clone())
        })
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
            let updated = stream.clone();
            if state.reconnect_on_mute {
                state.reconnect_on_mute = false;
                self.epoch.fetch_add(1, Ordering::SeqCst);
            }
            Ok(updated)
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
        .apply_mix_policy(&grouped, Some(5), 0)
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

#[tokio::test]
async fn reconnect_releases_a_reused_suppressed_stream_index() {
    let backend = Arc::new(TestBackend::new(vec![stream(1), stream(2)]));
    let mut arbiter = arbiter(backend.clone());
    arbiter.reconcile().await.unwrap();
    assert!(arbiter.suppressed_streams.contains(&1));

    // A fresh server reuses index 1 for a new, initially muted receiver.
    let mut fresh = stream(1);
    fresh.muted = true;
    backend.state.lock().unwrap().streams = vec![fresh];
    backend.epoch.store(1, Ordering::SeqCst);
    arbiter.reconcile().await.unwrap();

    assert_eq!(
        arbiter.shared_winner.read().await.as_deref(),
        Some("other:1")
    );
    assert!(!backend.state.lock().unwrap().streams[0].muted);
    assert!(arbiter.suppressed_streams.is_empty());
}

#[tokio::test]
async fn reconnect_treats_all_receivers_as_new_even_if_indexes_are_reused() {
    let mut previous = stream(2);
    previous
        .properties
        .insert("application.name".into(), "client-a".into());
    let backend = Arc::new(TestBackend::new(vec![previous]));
    let mut arbiter = arbiter(backend.clone());
    arbiter.reconcile().await.unwrap();

    let mut first = stream(1);
    first
        .properties
        .insert("application.name".into(), "client-a".into());
    let mut second = stream(2);
    second
        .properties
        .insert("application.name".into(), "client-b".into());
    backend.state.lock().unwrap().streams = vec![first, second];
    backend.epoch.store(1, Ordering::SeqCst);
    arbiter.reconcile().await.unwrap();

    assert_eq!(
        arbiter.shared_winner.read().await.as_deref(),
        Some("other:client-b")
    );
    let state = backend.state.lock().unwrap();
    assert!(state.streams[0].muted);
    assert!(!state.streams[1].muted);
}

#[tokio::test]
async fn reconnect_during_discovery_does_not_apply_a_mixed_snapshot() {
    let backend = Arc::new(TestBackend::new(vec![stream(1), stream(2)]));
    backend.state.lock().unwrap().reconnect_on_list = true;
    let mut arbiter = arbiter(backend.clone());
    assert!(arbiter.reconcile().await.is_err());
    assert!(backend.state.lock().unwrap().operations.is_empty());
    assert!(arbiter.shared_winner.read().await.is_none());

    arbiter.reconcile().await.unwrap();
    assert_eq!(
        arbiter.shared_winner.read().await.as_deref(),
        Some("other:2")
    );
}

#[tokio::test]
async fn temporary_query_failure_does_not_revive_a_suppressed_receiver() {
    let backend = Arc::new(TestBackend::new(vec![stream(1), stream(2)]));
    let mut arbiter = arbiter(backend.clone());
    arbiter.reconcile().await.unwrap();
    backend.state.lock().unwrap().fail_list = true;
    assert!(arbiter.reconcile().await.is_err());
    {
        let mut state = backend.state.lock().unwrap();
        state.fail_list = false;
        state.streams.retain(|stream| stream.index == 1);
    }
    arbiter.reconcile().await.unwrap();
    assert!(arbiter.shared_winner.read().await.is_none());
    assert!(backend.state.lock().unwrap().streams[0].muted);
}

#[tokio::test]
async fn reconnect_during_mute_does_not_unmute_a_winner_from_the_old_snapshot() {
    let mut winner = stream(2);
    winner.muted = true;
    let backend = Arc::new(TestBackend::new(vec![stream(1), winner]));
    backend.state.lock().unwrap().reconnect_on_mute = true;
    let mut arbiter = arbiter(backend.clone());

    assert!(arbiter.reconcile().await.is_err());
    assert_eq!(*arbiter.shared_winner.read().await, None);
    assert!(backend.state.lock().unwrap().streams[1].muted);
    assert_eq!(
        backend.state.lock().unwrap().operations,
        vec![Operation::Mute(1, true)]
    );

    arbiter.reconcile().await.unwrap();
    assert_eq!(arbiter.winner.as_deref(), Some("other:2"));
    assert!(!backend.state.lock().unwrap().streams[1].muted);
}

#[tokio::test]
async fn mixer_recovery_detects_a_reconnect_even_when_all_sink_indexes_are_reused() {
    let backend = Arc::new(TestBackend::new(Vec::new()));
    let audio = arbiter(backend.clone()).audio;
    let initial = crate::logical_mixer_topology(&audio).await.unwrap();
    backend.epoch.fetch_add(1, Ordering::SeqCst);
    let reconnected = crate::logical_mixer_topology(&audio).await.unwrap();

    assert_eq!(initial, (0, 1, 1, 1));
    assert_eq!(reconnected, (1, 1, 1, 1));
}

#[tokio::test]
async fn mixer_recovery_rejects_a_snapshot_spanning_two_connections() {
    let backend = Arc::new(TestBackend::new(Vec::new()));
    let audio = arbiter(backend.clone()).audio;
    backend.state.lock().unwrap().reconnect_on_sink = true;

    assert!(crate::logical_mixer_topology(&audio).await.is_err());
    assert_eq!(
        crate::logical_mixer_topology(&audio).await.unwrap(),
        (1, 1, 1, 1)
    );
}

#[tokio::test]
async fn reconnect_while_source_status_is_locked_does_not_publish_the_old_selection() {
    let backend = Arc::new(TestBackend::new(vec![stream(1), stream(2)]));
    let mut arbiter = arbiter(backend.clone());
    let shared = arbiter.shared_winner.clone();
    let held_status = shared.read().await;
    let task = tokio::spawn(async move {
        let result = arbiter.reconcile().await;
        (arbiter, result)
    });
    tokio::task::yield_now().await;
    assert!(backend.state.lock().unwrap().streams[0].muted);
    backend.epoch.fetch_add(1, Ordering::SeqCst);
    drop(held_status);

    let (mut arbiter, result) = task.await.unwrap();
    assert!(result.is_err());
    assert_eq!(*shared.read().await, None);
    assert_eq!(arbiter.winner, None);
    arbiter.reconcile().await.unwrap();
    assert_eq!(*shared.read().await, Some("other:2".into()));
}
