#![recursion_limit = "256"]

mod alerts;
mod api;
mod audio;
mod audio_backend;
mod command;
mod config;
mod dlna;
mod fourstream;
mod output_gain;
mod output_router;
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
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use alerts::{AlertController, SharedRuntimeState};
use api::ApiController;
use audio::AudioEngine;
use audio_backend::AudioBackend;
use config::{load_config, AppConfig};
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

async fn restore_mixer_when_ready(audio: &AudioEngine) -> Result<()> {
    let mut last_error = None;
    for attempt in 1..=30 {
        match audio.restore_user_mixer().await {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_error = Some(err);
                if attempt < 30 {
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("не вдалося відновити mixer state")))
}

async fn run_daemon(config: Arc<AppConfig>) -> Result<()> {
    let (store, state, audio, provider) = runtime(config.clone())?;

    // Logical buses are systemd-owned and start before the daemon, but the Pulse
    // client can race their registration by a few hundred milliseconds. Restore
    // persisted user gain only after all three logical sinks are addressable.
    restore_mixer_when_ready(&audio).await?;
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
    let api_controller = ApiController::new(config.clone(), audio, state, source_state);

    let mut tasks: JoinSet<Result<()>> = JoinSet::new();
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
    let config = Arc::new(load_config(&cli.config)?);
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
