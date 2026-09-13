#![recursion_limit = "256"]

mod alerts;
mod api;
mod atomic_file;
mod audio;
mod audio_backend;
mod command;
mod config;
mod dlna;
mod fourstream;
mod media_time;
mod output_gain;
mod output_router;
mod processing_domain;
mod provider;
mod pulse;
mod source_arbiter;
mod state;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinSet;
use tokio::time::sleep;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use alerts::{AlertController, SharedRuntimeState};
use api::ApiController;
use audio::AudioEngine;
use audio_backend::AudioBackend;
use config::{load_config, validate_config, AppConfig};
use provider::AlertsProvider;
use pulse::PulseControl;
use source_arbiter::SourceArbiter;
use state::StateStore;

const DEFAULT_CONFIG: &str = "/etc/proaudio-player-alert/config.yaml";

#[derive(Parser)]
#[command(
    name = "proaudio-player-native",
    version,
    about = "Native ProAudio network audio player"
)]
struct Cli {
    #[arg(long, default_value = DEFAULT_CONFIG)]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Запустити постійний native daemon.
    Run,
    /// Виконати одну перевірку API повітряної тривоги.
    Once,
    /// Показати збережений runtime state.
    Status,
    /// Перевірити вступне повідомлення.
    TestStart,
    /// Перевірити повідомлення відбою.
    TestEnd,
    /// Перевірити хвилину мовчання.
    TestSilence,
    /// Повний безпечний тест start/hold/end із відновленням музики.
    TestCycle {
        #[arg(long, default_value_t = 5.0)]
        hold: f64,
    },
}

fn init_logging(config: &AppConfig) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(config.log_level.to_ascii_lowercase()));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn audio_engine(config: Arc<AppConfig>) -> Result<AudioEngine> {
    let backend: Arc<dyn AudioBackend> = Arc::new(PulseControl::new()?);
    Ok(AudioEngine::new(config, backend))
}

fn runtime(
    config: Arc<AppConfig>,
) -> Result<(StateStore, SharedRuntimeState, AudioEngine, AlertsProvider)> {
    let store = StateStore::new(config.state_file.clone());
    let state = Arc::new(Mutex::new(store.load()));
    let audio = audio_engine(config.clone())?;
    let provider = AlertsProvider::new(config)?;
    Ok((store, state, audio, provider))
}

async fn logical_mixer_topology(audio: &AudioEngine) -> Result<(u32, u32, u32)> {
    let cfg = audio.config()?;
    let music = audio.sink_state(&cfg.music_sink).await?;
    let alert = audio.sink_state(&cfg.alert_sink).await?;
    let master = audio
        .master_state(output_router::DEFAULT_MASTER_SINK)
        .await?;
    Ok((music.index, alert.index, master.index))
}

async fn keep_user_mixer_restored(audio: AudioEngine) -> Result<()> {
    let mut changes = audio.subscribe_changes();
    let mut restored_topology = None;
    let mut graph_unavailable = false;

    loop {
        match logical_mixer_topology(&audio).await {
            Ok(topology) if restored_topology != Some(topology) => {
                match audio.restore_user_mixer().await {
                    Ok(()) => {
                        restored_topology = Some(topology);
                        graph_unavailable = false;
                        info!(?topology, "User mixer restored for logical audio graph");
                    }
                    Err(err) => {
                        restored_topology = None;
                        warn!(error = %err, "Logical audio graph exists but mixer restore failed");
                    }
                }
            }
            Ok(_) => {
                graph_unavailable = false;
            }
            Err(err) => {
                // PipeWire and the logical buses are independently supervised.
                // Keep the HTTP/control plane alive while they restart, then
                // restore persisted gain once for the new sink identities.
                restored_topology = None;
                if !graph_unavailable {
                    warn!(error = %err, "Logical audio graph is unavailable; waiting for recovery");
                    graph_unavailable = true;
                }
            }
        }

        tokio::select! {
            changed = changes.changed() => {
                if changed.is_err() {
                    sleep(Duration::from_secs(1)).await;
                    changes = audio.subscribe_changes();
                }
            }
            _ = sleep(Duration::from_secs(2)) => {}
        }
    }
}

async fn run_daemon(config: Arc<AppConfig>) -> Result<()> {
    let (store, state, audio, provider) = runtime(config.clone())?;

    let _mixer_state_writer = audio.start_mixer_state_writer();

    let alert_controller = AlertController::new(
        config.clone(),
        provider,
        audio.clone(),
        store,
        state.clone(),
    );
    let source_state = Arc::new(RwLock::new(None));
    let arbiter = SourceArbiter::new(config.clone(), audio.clone(), source_state.clone());
    let mixer_audio = audio.clone();
    let api_controller = ApiController::new(config.clone(), audio, state, source_state);

    let mut tasks: JoinSet<Result<()>> = JoinSet::new();
    tasks.spawn(keep_user_mixer_restored(mixer_audio));
    tasks.spawn(async move { alert_controller.run_forever().await });
    tasks.spawn(async move { arbiter.run_forever().await });
    if config.api.enabled {
        tasks.spawn(async move {
            loop {
                if let Err(err) = api::serve(api_controller.clone()).await {
                    error!(error = %err, "Control API stopped; retrying");
                }
                sleep(Duration::from_secs(2)).await;
            }
            #[allow(unreachable_code)]
            Ok(())
        });
    }

    info!("ProAudio Player native control plane started");
    let first = tasks
        .join_next()
        .await
        .ok_or_else(|| anyhow!("native daemon не запустив жодного runtime task"))?;
    tasks.abort_all();
    match first {
        Ok(Ok(())) => Err(anyhow!("критичний runtime task неочікувано завершився")),
        Ok(Err(err)) => Err(err),
        Err(err) => Err(anyhow!("критичний runtime task завершився: {err}")),
    }
}

async fn run_once(config: Arc<AppConfig>) -> Result<()> {
    let (store, state, audio, provider) = runtime(config.clone())?;
    let mut controller = AlertController::new(config, provider, audio, store, state);
    controller.recover().await?;
    controller.run_once().await?;
    Ok(())
}

async fn test_alert(config: Arc<AppConfig>, start: bool, end: bool, hold: f64) -> Result<()> {
    if !hold.is_finite() || !(0.0..=3_600.0).contains(&hold) {
        return Err(anyhow!("--hold має бути скінченним числом у межах 0..3600 секунд"));
    }
    let audio = audio_engine(config)?;
    let snapshot = audio.snapshot().await?;
    let result = async {
        audio.enter_alert(&snapshot).await?;
        let audio_config = audio.config()?;
        if start {
            audio.play(&audio_config.start_file, None).await?;
        }
        if hold > 0.0 {
            sleep(Duration::from_secs_f64(hold)).await;
        }
        if end {
            audio.play(&audio_config.end_file, None).await?;
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let restore = audio.restore(Some(&snapshot)).await;
    result?;
    restore
}

async fn test_silence(config: Arc<AppConfig>) -> Result<()> {
    let audio = audio_engine(config)?;
    let snapshot = audio.snapshot().await?;
    let result = async {
        let minute = audio.minute_config()?;
        audio
            .enter_silence(&snapshot, minute.music_fade_seconds)
            .await?;
        audio
            .play(&minute.file, Some(minute.volume_percent))
            .await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let restore = audio.restore(Some(&snapshot)).await;
    result?;
    restore
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut config = load_config(&cli.config)?;
    processing_domain::apply(&mut config)?;
    validate_config(&config)?;
    let config = Arc::new(config);
    init_logging(&config);

    match cli.command {
        Command::Run => run_daemon(config).await,
        Command::Once => run_once(config).await,
        Command::Status => {
            let store = StateStore::new(config.state_file.clone());
            println!("{}", serde_json::to_string_pretty(&store.load())?);
            Ok(())
        }
        Command::TestStart => test_alert(config, true, false, 0.0).await,
        Command::TestEnd => test_alert(config, false, true, 0.0).await,
        Command::TestSilence => test_silence(config).await,
        Command::TestCycle { hold } => test_alert(config, true, true, hold.max(0.0)).await,
    }
}
