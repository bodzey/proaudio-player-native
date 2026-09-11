use std::collections::VecDeque;
use std::f32::consts::PI;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use libpulse_binding as pa;
use pa::context::{Context as PulseContext, FlagSet as ContextFlagSet, State as ContextState};
use pa::mainloop::standard::{IterateResult, Mainloop};
use pa::proplist::{properties::APPLICATION_NAME, Proplist};
use pa::sample::{Format, Spec};
use pa::stream::{FlagSet as StreamFlagSet, PeekResult, SeekMode, State as StreamState, Stream};
use pa::volume::{ChannelVolumes, Volume};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_SLEEP: Duration = Duration::from_millis(1);
const MAX_CHANNELS: usize = 8;
const FIR_TAPS: usize = 32;
const FIR_TARGET_INDEX: usize = FIR_TAPS / 2 - 1;
const DETECTOR_DELAY_FRAMES: usize = FIR_TAPS - 1 - FIR_TARGET_INDEX;
type Frame = [f32; MAX_CHANNELS];

#[derive(Debug, Parser)]
#[command(
    name = "proaudio-player-limiter",
    version,
    about = "Final linked safety limiter for the ProAudio Player output bus"
)]
struct Args {
    /// PulseAudio/PipeWire-Pulse monitor source carrying the final mix.
    #[arg(long, default_value = "proaudio_player_master.monitor")]
    source: String,

    /// Physical sink receiving the limited signal.
    #[arg(long)]
    sink: String,

    #[arg(long, default_value_t = 48_000)]
    rate: u32,

    #[arg(long, default_value_t = 2)]
    channels: u8,

    /// Enable limiting. False keeps the float PCM relay sample-transparent.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    enabled: bool,

    /// Maximum output level. The limiter never applies positive gain.
    #[arg(long, default_value_t = -1.0)]
    ceiling_db: f32,

    /// Lookahead used to make gain reduction effectively instantaneous.
    #[arg(long, default_value_t = 5.0)]
    lookahead_ms: f32,

    /// Gain recovery time constant.
    #[arg(long, default_value_t = 100.0)]
    release_ms: f32,

    /// FIR intersample peak estimator resolution. Supported: 1, 2, 4, 8.
    #[arg(long, default_value_t = 4)]
    oversample: usize,
}

#[derive(Clone, Copy)]
struct BufferedFrame {
    samples: Frame,
    sequence: u64,
}

struct PeakEstimator {
    channels: usize,
    phases: Vec<[f32; FIR_TAPS]>,
    history: VecDeque<Frame>,
}

impl PeakEstimator {
    fn new(channels: usize, oversample: usize) -> Self {
        let phases = (0..oversample)
            .map(|phase| interpolation_kernel(phase, oversample))
            .collect();
        let mut history = VecDeque::with_capacity(FIR_TAPS);
        for _ in 0..FIR_TAPS - 1 {
            history.push_back([0.0; MAX_CHANNELS]);
        }
        Self {
            channels,
            phases,
            history,
        }
    }

    fn push(&mut self, frame: Frame) -> f32 {
        self.history.push_back(frame);
        if self.history.len() > FIR_TAPS {
            self.history.pop_front();
        }
        debug_assert_eq!(self.history.len(), FIR_TAPS);

        let mut peak = 0.0_f32;
        for kernel in &self.phases {
            for channel in 0..self.channels {
                let mut value = 0.0_f32;
                for (tap, coefficients) in kernel.iter().enumerate() {
                    value += self.history[tap][channel] * coefficients;
                }
                peak = peak.max(value.abs());
            }
        }
        peak
    }
}

struct SafetyLimiter {
    enabled: bool,
    channels: usize,
    ceiling: f32,
    lookahead_frames: usize,
    total_delay_frames: usize,
    release_decay: f32,
    estimator: PeakEstimator,
    delayed: VecDeque<BufferedFrame>,
    peak_window: VecDeque<(u64, f32)>,
    next_sequence: u64,
    gain: f32,
}

impl SafetyLimiter {
    fn new(args: &Args) -> Result<Self> {
        if !(8_000..=384_000).contains(&args.rate) {
            bail!("sample rate must be 8000..384000 Hz");
        }
        if !(1..=MAX_CHANNELS as u8).contains(&args.channels) {
            bail!("channel count must be 1..{MAX_CHANNELS}");
        }
        if !(-12.0..=0.0).contains(&args.ceiling_db) {
            bail!("ceiling must be -12..0 dB");
        }
        if !(0.5..=20.0).contains(&args.lookahead_ms) {
            bail!("lookahead must be 0.5..20 ms");
        }
        if !(20.0..=2_000.0).contains(&args.release_ms) {
            bail!("release must be 20..2000 ms");
        }
        if !matches!(args.oversample, 1 | 2 | 4 | 8) {
            bail!("oversample must be one of 1, 2, 4, 8");
        }

        let lookahead_frames = ((args.rate as f32 * args.lookahead_ms / 1_000.0).round() as usize)
            .max(1);
        let release_frames = args.rate as f32 * args.release_ms / 1_000.0;
        let release_decay = (-1.0 / release_frames.max(1.0)).exp();
        let channels = args.channels as usize;
        let total_delay_frames = lookahead_frames + DETECTOR_DELAY_FRAMES;

        Ok(Self {
            enabled: args.enabled,
            channels,
            ceiling: 10.0_f32.powf(args.ceiling_db / 20.0),
            lookahead_frames,
            total_delay_frames,
            release_decay,
            estimator: PeakEstimator::new(channels, args.oversample),
            delayed: VecDeque::with_capacity(total_delay_frames + 8),
            peak_window: VecDeque::with_capacity(lookahead_frames + 8),
            next_sequence: 0,
            gain: 1.0,
        })
    }

    fn frame_bytes(&self) -> usize {
        self.channels * std::mem::size_of::<f32>()
    }

    fn process_bytes(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        if !self.enabled {
            return Ok(input.to_vec());
        }
        let frame_bytes = self.frame_bytes();
        if input.len() % frame_bytes != 0 {
            bail!("unaligned float PCM fragment: {} bytes", input.len());
        }

        let mut output = Vec::with_capacity(input.len());
        for encoded in input.chunks_exact(frame_bytes) {
            let mut frame = [0.0_f32; MAX_CHANNELS];
            for (channel, bytes) in encoded.chunks_exact(4).enumerate() {
                let value = f32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                frame[channel] = if value.is_finite() { value } else { 0.0 };
            }
            if let Some(processed) = self.push_frame(frame) {
                for sample in processed.iter().take(self.channels) {
                    output.extend_from_slice(&sample.to_ne_bytes());
                }
            }
        }
        Ok(output)
    }

    fn push_frame(&mut self, frame: Frame) -> Option<Frame> {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.delayed.push_back(BufferedFrame { samples: frame, sequence });

        // A 32-tap, phase-normalized windowed-sinc interpolator estimates the
        // continuous-time peak between PCM samples. Its fixed group delay is
        // absorbed into the limiter delay, so gain reduction still arrives before
        // the corresponding audio frame reaches the physical output.
        let peak = self.estimator.push(frame);
        if sequence >= DETECTOR_DELAY_FRAMES as u64 {
            let peak_sequence = sequence - DETECTOR_DELAY_FRAMES as u64;
            while self
                .peak_window
                .back()
                .is_some_and(|(_, buffered_peak)| *buffered_peak <= peak)
            {
                self.peak_window.pop_back();
            }
            self.peak_window.push_back((peak_sequence, peak));
        }

        if self.delayed.len() <= self.total_delay_frames {
            return None;
        }

        let buffered = *self.delayed.front()?;
        while self
            .peak_window
            .front()
            .is_some_and(|(peak_sequence, _)| *peak_sequence < buffered.sequence)
        {
            self.peak_window.pop_front();
        }

        let future_peak = self
            .peak_window
            .front()
            .map(|(_, peak)| *peak)
            .unwrap_or(0.0);
        let target = if future_peak > self.ceiling && future_peak > 0.0 {
            (self.ceiling / future_peak).clamp(0.0, 1.0)
        } else {
            1.0
        };

        // Linked limiter: every channel receives the same attenuation. Attack is
        // instantaneous through lookahead. During release, gain may approach the
        // currently safe target but never cross it and never exceed unity.
        if target < self.gain {
            self.gain = target;
        } else {
            let released = 1.0 - (1.0 - self.gain) * self.release_decay;
            self.gain = released.min(target).min(1.0);
        }

        let buffered = self.delayed.pop_front()?;
        let mut output = buffered.samples;
        for sample in output.iter_mut().take(self.channels) {
            // Sample clamp is only a final numerical fail-safe; normal protection
            // comes from the linked FIR/lookahead gain envelope above.
            *sample = (*sample * self.gain).clamp(-self.ceiling, self.ceiling);
        }
        Some(output)
    }
}

fn interpolation_kernel(phase: usize, oversample: usize) -> [f32; FIR_TAPS] {
    let fraction = phase as f32 / oversample as f32;
    let mut kernel = [0.0_f32; FIR_TAPS];
    let mut sum = 0.0_f32;

    for (tap, coefficient) in kernel.iter_mut().enumerate() {
        let offset = tap as isize - FIR_TARGET_INDEX as isize;
        let x = offset as f32 - fraction;
        let sinc = if x.abs() < 1.0e-7 {
            1.0
        } else {
            (PI * x).sin() / (PI * x)
        };
        let position = tap as f32 / (FIR_TAPS - 1) as f32;
        let window = 0.42 - 0.5 * (2.0 * PI * position).cos()
            + 0.08 * (4.0 * PI * position).cos();
        *coefficient = sinc * window;
        sum += *coefficient;
    }

    if sum.abs() > 1.0e-12 {
        for coefficient in &mut kernel {
            *coefficient /= sum;
        }
    }
    kernel
}

fn iterate_once(mainloop: &mut Mainloop, context: &PulseContext) -> Result<()> {
    match mainloop.iterate(false) {
        IterateResult::Success(_) => {}
        IterateResult::Quit(code) => bail!("PulseAudio mainloop quit: {code:?}"),
        IterateResult::Err(err) => bail!("PulseAudio mainloop error: {err}"),
    }
    match context.get_state() {
        ContextState::Failed | ContextState::Terminated => bail!("PulseAudio context disconnected"),
        _ => Ok(()),
    }
}

fn wait_for_context(mainloop: &mut Mainloop, context: &PulseContext) -> Result<()> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match context.get_state() {
            ContextState::Ready => return Ok(()),
            ContextState::Failed | ContextState::Terminated => {
                bail!("PulseAudio context connection failed")
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            bail!("PulseAudio context connection timed out");
        }
        iterate_once(mainloop, context)?;
        thread::sleep(IDLE_SLEEP);
    }
}

fn wait_for_streams(
    mainloop: &mut Mainloop,
    context: &PulseContext,
    record: &Stream,
    playback: &Stream,
) -> Result<()> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        let record_state = record.get_state();
        let playback_state = playback.get_state();
        if record_state == StreamState::Ready && playback_state == StreamState::Ready {
            return Ok(());
        }
        if matches!(record_state, StreamState::Failed | StreamState::Terminated)
            || matches!(playback_state, StreamState::Failed | StreamState::Terminated)
        {
            bail!(
                "PulseAudio limiter stream failed: record={record_state:?}, playback={playback_state:?}"
            );
        }
        if Instant::now() >= deadline {
            bail!(
                "PulseAudio limiter stream connection timed out: record={record_state:?}, playback={playback_state:?}"
            );
        }
        iterate_once(mainloop, context)?;
        thread::sleep(IDLE_SLEEP);
    }
}

fn write_all(
    mainloop: &mut Mainloop,
    context: &PulseContext,
    playback: &mut Stream,
    data: &[u8],
    frame_bytes: usize,
) -> Result<()> {
    let mut offset = 0;
    while offset < data.len() {
        iterate_once(mainloop, context)?;
        if matches!(playback.get_state(), StreamState::Failed | StreamState::Terminated) {
            bail!("PulseAudio limiter playback stream disconnected");
        }
        let writable = playback
            .writable_size()
            .ok_or_else(|| anyhow!("PulseAudio playback writable size unavailable"))?;
        let writable = writable - (writable % frame_bytes);
        if writable == 0 {
            thread::sleep(IDLE_SLEEP);
            continue;
        }
        let remaining = data.len() - offset;
        let amount = remaining.min(writable);
        let amount = amount - (amount % frame_bytes);
        if amount == 0 {
            thread::sleep(IDLE_SLEEP);
            continue;
        }
        playback
            .write_copy(&data[offset..offset + amount], 0, SeekMode::Relative)
            .context("failed to write limited PCM to PulseAudio")?;
        offset += amount;
    }
    Ok(())
}

fn run(args: Args) -> Result<()> {
    let mut limiter = SafetyLimiter::new(&args)?;
    let spec = Spec {
        format: Format::FLOAT32NE,
        channels: args.channels,
        rate: args.rate,
    };
    if !spec.is_valid() {
        bail!("invalid PulseAudio sample specification");
    }

    let mut mainloop = Mainloop::new().context("failed to create PulseAudio mainloop")?;
    let mut proplist = Proplist::new().context("failed to create PulseAudio property list")?;
    proplist
        .set_str(APPLICATION_NAME, "ProAudio Player Output Limiter")
        .map_err(|()| anyhow!("failed to set PulseAudio application name"))?;
    let mut context = PulseContext::new_with_proplist(
        &mainloop,
        "ProAudioPlayerLimiter",
        &proplist,
    )
    .context("failed to create PulseAudio context")?;
    context
        .connect(None, ContextFlagSet::NOFLAGS, None)
        .context("failed to connect to PulseAudio server")?;
    wait_for_context(&mut mainloop, &context)?;

    let mut record = Stream::new(&mut context, "final-mix-capture", &spec, None)
        .context("failed to create limiter capture stream")?;
    let mut playback = Stream::new(&mut context, "limited-output", &spec, None)
        .context("failed to create limiter playback stream")?;

    record
        .connect_record(Some(&args.source), None, StreamFlagSet::NOFLAGS)
        .with_context(|| format!("failed to connect limiter to source {}", args.source))?;

    // The limiter is an internal transport, not a user gain stage. Pin its
    // playback stream to unity so stream-restore cannot silently attenuate or
    // amplify the already-limited final mix.
    let mut unity = ChannelVolumes::default();
    unity.set(args.channels, Volume::NORMAL);
    playback
        .connect_playback(
            Some(&args.sink),
            None,
            StreamFlagSet::NOFLAGS,
            Some(&unity),
            None,
        )
        .with_context(|| format!("failed to connect limiter to sink {}", args.sink))?;
    wait_for_streams(&mut mainloop, &context, &record, &playback)?;

    eprintln!(
        "output limiter: source={} sink={} rate={} channels={} enabled={} ceiling={:.2}dBTP lookahead={:.2}ms release={:.1}ms FIR={}x/{}tap",
        args.source,
        args.sink,
        args.rate,
        args.channels,
        args.enabled,
        args.ceiling_db,
        args.lookahead_ms,
        args.release_ms,
        args.oversample,
        FIR_TAPS
    );

    let frame_bytes = limiter.frame_bytes();
    loop {
        iterate_once(&mut mainloop, &context)?;
        if matches!(record.get_state(), StreamState::Failed | StreamState::Terminated) {
            bail!("PulseAudio limiter capture stream disconnected");
        }

        if record.readable_size().unwrap_or(0) == 0 {
            thread::sleep(IDLE_SLEEP);
            continue;
        }

        let chunk = match record
            .peek()
            .context("failed to read final mix from PulseAudio")?
        {
            PeekResult::Empty => {
                thread::sleep(IDLE_SLEEP);
                continue;
            }
            PeekResult::Hole(bytes) => vec![0_u8; bytes - (bytes % frame_bytes)],
            PeekResult::Data(data) => data.to_vec(),
        };
        record
            .discard()
            .context("failed to discard captured PulseAudio fragment")?;
        if chunk.is_empty() {
            continue;
        }

        let output = limiter.process_bytes(&chunk)?;
        if !output.is_empty() {
            write_all(
                &mut mainloop,
                &context,
                &mut playback,
                &output,
                frame_bytes,
            )?;
        }
    }
}

fn main() -> Result<()> {
    run(Args::parse())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Args {
        Args {
            source: "test.monitor".into(),
            sink: "test".into(),
            rate: 48_000,
            channels: 2,
            enabled: true,
            ceiling_db: -1.0,
            lookahead_ms: 1.0,
            release_ms: 100.0,
            oversample: 4,
        }
    }

    fn stereo(left: f32, right: f32) -> Frame {
        let mut frame = [0.0; MAX_CHANNELS];
        frame[0] = left;
        frame[1] = right;
        frame
    }

    #[test]
    fn interpolation_kernels_preserve_dc() {
        for phase in 0..4 {
            let kernel = interpolation_kernel(phase, 4);
            let sum = kernel.iter().sum::<f32>();
            assert!((sum - 1.0).abs() < 1.0e-5);
        }
    }

    #[test]
    fn limiter_never_amplifies() {
        let mut limiter = SafetyLimiter::new(&args()).unwrap();
        let mut maximum = 0.0_f32;
        for _ in 0..512 {
            if let Some(frame) = limiter.push_frame(stereo(0.25, -0.25)) {
                maximum = maximum.max(frame[0].abs()).max(frame[1].abs());
            }
        }
        assert!(maximum <= 0.250_001);
    }

    #[test]
    fn limiter_caps_overrange_mix() {
        let cfg = args();
        let ceiling = 10.0_f32.powf(cfg.ceiling_db / 20.0);
        let mut limiter = SafetyLimiter::new(&cfg).unwrap();
        let mut seen = 0;
        for _ in 0..768 {
            if let Some(frame) = limiter.push_frame(stereo(1.5, -1.5)) {
                for sample in frame.iter().take(2) {
                    assert!(sample.abs() <= ceiling + 1e-6);
                }
                seen += 1;
            }
        }
        assert!(seen > 0);
    }

    #[test]
    fn linked_channels_share_gain_reduction() {
        let mut limiter = SafetyLimiter::new(&args()).unwrap();
        let mut observed = false;
        for index in 0..768 {
            let hot = if index > 100 { 1.5 } else { 0.25 };
            if let Some(frame) = limiter.push_frame(stereo(hot, 0.5)) {
                if frame[0].abs() > 0.5 {
                    assert!(frame[1].abs() < 0.5);
                    observed = true;
                    break;
                }
            }
        }
        assert!(observed, "expected a limited hot frame");
    }

    #[test]
    fn sustained_peak_never_releases_above_required_gain() {
        let cfg = args();
        let ceiling = 10.0_f32.powf(cfg.ceiling_db / 20.0);
        let mut limiter = SafetyLimiter::new(&cfg).unwrap();
        for _ in 0..2_000 {
            if let Some(frame) = limiter.push_frame(stereo(1.2, 1.2)) {
                assert!(frame[0].abs() <= ceiling + 1e-6);
                assert!(frame[1].abs() <= ceiling + 1e-6);
            }
        }
    }

    #[test]
    fn lookahead_storage_stays_bounded() {
        let mut limiter = SafetyLimiter::new(&args()).unwrap();
        for _ in 0..10_000 {
            let _ = limiter.push_frame(stereo(0.1, -0.1));
            assert!(limiter.delayed.len() <= limiter.total_delay_frames);
            assert!(limiter.peak_window.len() <= limiter.lookahead_frames + 1);
            assert!(limiter.estimator.history.len() <= FIR_TAPS);
        }
    }

    #[test]
    fn disabled_mode_is_bit_transparent() {
        let mut cfg = args();
        cfg.enabled = false;
        let mut limiter = SafetyLimiter::new(&cfg).unwrap();
        let input = [0x00, 0x00, 0x80, 0x3f, 0x00, 0x00, 0x80, 0xbf];
        assert_eq!(limiter.process_bytes(&input).unwrap(), input);
    }
}
