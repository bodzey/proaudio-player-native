use std::convert::Infallible;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::{Result, anyhow};
use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval, sleep};
use tracing::debug;

use super::backend::WebController;

const METER_INTERVAL: Duration = Duration::from_millis(40);
const METER_WINDOW_MILLIS: u64 = 40;
const TOPOLOGY_INTERVAL: Duration = Duration::from_secs(1);
const RETRY_INTERVAL: Duration = Duration::from_secs(1);
const MIN_DB: f64 = -60.0;
const BYTES_PER_STEREO_FRAME: usize = 8;

struct EventStream {
    receiver: mpsc::Receiver<std::result::Result<Event, Infallible>>,
}

impl futures_core::Stream for EventStream {
    type Item = std::result::Result<Event, Infallible>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context)
    }
}

struct MeterHub {
    sender: broadcast::Sender<String>,
    started: AtomicBool,
}

static METER_HUB: OnceLock<MeterHub> = OnceLock::new();

fn hub() -> &'static MeterHub {
    METER_HUB.get_or_init(|| {
        let (sender, _) = broadcast::channel(16);
        MeterHub {
            sender,
            started: AtomicBool::new(false),
        }
    })
}

#[derive(Clone, Copy)]
enum MeterTarget {
    Master,
    Music,
    Alert,
}

impl MeterTarget {
    fn label(self) -> &'static str {
        match self {
            Self::Master => "master",
            Self::Music => "music",
            Self::Alert => "alert",
        }
    }
}

#[derive(Clone, Copy)]
struct StereoLevel {
    peak: [f64; 2],
    rms: [f64; 2],
    clip: [bool; 2],
    available: bool,
}

impl StereoLevel {
    const fn silence(available: bool) -> Self {
        Self {
            peak: [MIN_DB, MIN_DB],
            rms: [MIN_DB, MIN_DB],
            clip: [false, false],
            available,
        }
    }

    fn json(self) -> serde_json::Value {
        json!({
            "peak": self.peak,
            "rms": self.rms,
            "clip": self.clip,
            "available": self.available,
        })
    }
}

impl Default for StereoLevel {
    fn default() -> Self {
        Self::silence(false)
    }
}

#[derive(Default)]
struct MeterFrame {
    master: StereoLevel,
    music: StereoLevel,
    alert: StereoLevel,
}

impl MeterFrame {
    fn set(&mut self, target: MeterTarget, level: StereoLevel) {
        match target {
            MeterTarget::Master => self.master = level,
            MeterTarget::Music => self.music = level,
            MeterTarget::Alert => self.alert = level,
        }
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

struct MeterUpdate {
    target: MeterTarget,
    level: StereoLevel,
}

#[derive(Default)]
struct MeterWindow {
    peak: [f64; 2],
    sum_squares: [f64; 2],
    samples: usize,
    clip: [bool; 2],
}

impl MeterWindow {
    fn push(&mut self, left: f32, right: f32) {
        let values = [left as f64, right as f64];
        if !values[0].is_finite() || !values[1].is_finite() {
            return;
        }
        for (channel, value) in values.into_iter().enumerate() {
            let absolute = value.abs();
            self.peak[channel] = self.peak[channel].max(absolute);
            self.sum_squares[channel] += value * value;
            self.clip[channel] |= absolute >= 0.999;
        }
        self.samples += 1;
    }

    fn take(&mut self) -> Option<StereoLevel> {
        if self.samples == 0 {
            return None;
        }
        let count = self.samples as f64;
        let level = StereoLevel {
            peak: [amplitude_db(self.peak[0]), amplitude_db(self.peak[1])],
            rms: [
                amplitude_db((self.sum_squares[0] / count).sqrt()),
                amplitude_db((self.sum_squares[1] / count).sqrt()),
            ],
            clip: self.clip,
            available: true,
        };
        *self = Self::default();
        Some(level)
    }
}

fn amplitude_db(value: f64) -> f64 {
    if !value.is_finite() || value <= 0.001 {
        MIN_DB
    } else {
        (20.0 * value.log10()).clamp(MIN_DB, 0.0)
    }
}

fn meter_window_frames(sample_rate: u32) -> usize {
    ((u64::from(sample_rate) * METER_WINDOW_MILLIS) / 1_000).max(1) as usize
}

fn spawn_recorder(device: &str, sample_rate: u32) -> Result<Child> {
    let programs = [("parec", false), ("pacat", true)];
    let mut not_found = Vec::new();

    for (program, record_flag) in programs {
        let mut command = Command::new(program);
        if record_flag {
            command.arg("--record");
        }
        command
            .arg("--raw")
            .arg("--format=float32le")
            .arg(format!("--rate={sample_rate}"))
            .arg("--channels=2")
            .arg("--latency-msec=40")
            .arg(format!("--device={device}"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                not_found.push(program);
            }
            Err(error) => return Err(error.into()),
        }
    }

    Err(anyhow!(
        "meter capture unavailable: {} not found",
        not_found.join("/"),
    ))
}

async fn capture_once(
    target: MeterTarget,
    sink: &str,
    sample_rate: u32,
    updates: &mpsc::Sender<MeterUpdate>,
) -> Result<()> {
    let device = format!("{sink}.monitor");
    let mut child = spawn_recorder(&device, sample_rate)?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("meter recorder stdout unavailable"))?;

    let mut read_buffer = [0_u8; 8192];
    let mut pending = Vec::<u8>::with_capacity(16_384);
    let mut window = MeterWindow::default();

    loop {
        let read = stdout.read(&mut read_buffer).await?;
        if read == 0 {
            return Err(anyhow!("meter recorder stopped"));
        }

        pending.extend_from_slice(&read_buffer[..read]);
        let usable = pending.len() / BYTES_PER_STEREO_FRAME * BYTES_PER_STEREO_FRAME;

        for frame in pending[..usable].chunks_exact(BYTES_PER_STEREO_FRAME) {
            let left = f32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
            let right = f32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]);
            window.push(left, right);

            if window.samples >= meter_window_frames(sample_rate) {
                if let Some(level) = window.take() {
                    updates
                        .send(MeterUpdate { target, level })
                        .await
                        .map_err(|_| anyhow!("meter consumer stopped"))?;
                }
            }
        }

        if usable > 0 {
            pending.drain(..usable);
        }
    }
}

async fn monitor_forever(
    target: MeterTarget,
    sink: String,
    sample_rate: u32,
    updates: mpsc::Sender<MeterUpdate>,
) {
    loop {
        if let Err(error) = capture_once(target, &sink, sample_rate, &updates).await {
            debug!(
                target = target.label(),
                sink = %sink,
                error = %error,
                "Audio meter capture stopped; retrying"
            );
        }
        if updates
            .send(MeterUpdate {
                target,
                level: StereoLevel::silence(false),
            })
            .await
            .is_err()
        {
            return;
        }
        sleep(RETRY_INTERVAL).await;
    }
}

fn spawn_monitor(
    target: MeterTarget,
    sink: String,
    sample_rate: u32,
    updates: mpsc::Sender<MeterUpdate>,
) -> JoinHandle<()> {
    tokio::spawn(monitor_forever(target, sink, sample_rate, updates))
}

fn abort(task: &mut Option<JoinHandle<()>>) {
    if let Some(task) = task.take() {
        task.abort();
    }
}

async fn run_meter_runtime(controller: WebController, sender: broadcast::Sender<String>) {
    let (updates, mut receiver) = mpsc::channel::<MeterUpdate>(64);
    let mut frame = MeterFrame::default();
    let mut sequence = 0_u64;

    let mut music_task: Option<JoinHandle<()>> = None;
    let mut alert_task: Option<JoinHandle<()>> = None;
    let mut master_task: Option<JoinHandle<()>> = None;
    let mut master_sink: Option<String> = None;
    let sample_rate = controller.config.audio.sample_rate;

    let mut emit = interval(METER_INTERVAL);
    emit.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut topology = interval(TOPOLOGY_INTERVAL);
    topology.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            Some(update) = receiver.recv() => {
                frame.set(update.target, update.level);
            }
            _ = topology.tick() => {
                if sender.receiver_count() == 0 {
                    abort(&mut music_task);
                    abort(&mut alert_task);
                    abort(&mut master_task);
                    master_sink = None;
                    frame.reset();
                    continue;
                }

                if music_task.as_ref().is_none_or(|task| task.is_finished()) {
                    music_task = Some(spawn_monitor(
                        MeterTarget::Music,
                        controller.config.audio.music_sink.clone(),
                        sample_rate,
                        updates.clone(),
                    ));
                }
                if alert_task.as_ref().is_none_or(|task| task.is_finished()) {
                    alert_task = Some(spawn_monitor(
                        MeterTarget::Alert,
                        controller.config.audio.alert_sink.clone(),
                        sample_rate,
                        updates.clone(),
                    ));
                }

                let current_master = controller.physical_sink().await.ok();
                if current_master != master_sink {
                    abort(&mut master_task);
                    frame.master = StereoLevel::silence(false);
                    master_sink = current_master.clone();
                    if let Some(sink) = current_master {
                        master_task = Some(spawn_monitor(
                            MeterTarget::Master,
                            sink,
                            sample_rate,
                            updates.clone(),
                        ));
                    }
                } else if master_task.as_ref().is_some_and(|task| task.is_finished()) {
                    abort(&mut master_task);
                    if let Some(sink) = master_sink.clone() {
                        master_task = Some(spawn_monitor(
                            MeterTarget::Master,
                            sink,
                            sample_rate,
                            updates.clone(),
                        ));
                    }
                }
            }
            _ = emit.tick() => {
                if sender.receiver_count() == 0 {
                    continue;
                }
                sequence = sequence.wrapping_add(1);
                let payload = json!({
                    "sequence": sequence,
                    "sample_rate": sample_rate,
                    "interval_ms": METER_INTERVAL.as_millis(),
                    "master": frame.master.json(),
                    "music": frame.music.json(),
                    "alert": frame.alert.json(),
                });
                let _ = sender.send(payload.to_string());
            }
        }
    }
}

fn ensure_started(controller: WebController) {
    let meter_hub = hub();
    if meter_hub
        .started
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    let sender = meter_hub.sender.clone();
    tokio::spawn(async move {
        run_meter_runtime(controller, sender).await;
        hub().started.store(false, Ordering::Release);
    });
}

async fn meter_events(State(controller): State<WebController>) -> impl IntoResponse {
    ensure_started(controller);
    let mut receiver = hub().sender.subscribe();
    let (sender, stream) = mpsc::channel(8);

    tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(payload) => {
                    if sender
                        .send(Ok(Event::default().event("meter").data(payload)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    Sse::new(EventStream { receiver: stream }).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(5))
            .text("meter-keepalive"),
    )
}

pub(super) fn router() -> Router<WebController> {
    Router::<WebController>::new()
        .route("/api/meters", get(meter_events))
        .route("/api/v1/meters", get(meter_events))
}

#[cfg(test)]
mod tests {
    use super::meter_window_frames;

    #[test]
    fn meter_window_tracks_the_processing_rate() {
        assert_eq!(meter_window_frames(44_100), 1_764);
        assert_eq!(meter_window_frames(48_000), 1_920);
        assert_eq!(meter_window_frames(96_000), 3_840);
    }
}
