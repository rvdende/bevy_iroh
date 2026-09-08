//! Voice: capture, opus, a jitter buffer per remote track, mixing and playback.
//!
//! Sources and outputs are traits so the same pipeline runs against a microphone and speakers,
//! or against a sine wave and a buffer in a test. Everything speaks mono f32 at 48 kHz inside;
//! devices at other rates are resampled at the edges.
//!
//! The buffering rules are substrate's, learned on real calls: a buffer is standing latency,
//! so it starts small and grows only when starved; an underrun is a whole buffer of silence
//! rather than a splice; backlog is spent by playing slightly fast rather than kept; and every
//! silent failure has a counter, because "a subscription that connects and then delivers
//! nothing looks exactly like a person who is not talking".

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;

use super::transport::{MediaHub, Seq};

pub const RATE: u32 = 48_000;
/// 20 ms at 48 kHz: opus's sweet spot for voice.
pub const FRAME: usize = 960;
/// The longest frame opus allows, 120 ms: a peer on another build may send them.
const MAX_DECODED: usize = 5760;
const MAX_PACKET: usize = 1275;
/// Opus at 64 kbps is transparent for speech.
const BITRATE: i32 = 64_000;

/// Where microphone samples come from: mono f32 at `sample_rate`.
pub trait AudioSource: Send + 'static {
    fn sample_rate(&self) -> u32;
    /// Fill `out` with what has arrived since the last call. Returns how many were written;
    /// fewer than asked means wait, not end. May block briefly waiting for the device.
    fn read(&mut self, out: &mut [f32]) -> usize;
}

/// Where the mix goes. `start` must drive `mixer.render` on its own clock, from a thread it
/// owns, until `stop` is set.
pub trait AudioOutput: Send + 'static {
    fn start(
        self: Box<Self>,
        mixer: Arc<Mutex<Mixer>>,
        stop: Arc<AtomicBool>,
    ) -> Result<(), String>;
}

// -- the encoder ---------------------------------------------------------------------------

/// Reads a source, encodes 20 ms frames, and sends each to every published track.
///
/// Frames accumulate in one growing buffer and are cut at exactly `FRAME` samples: a device
/// delivering 512-sample blocks and an encoder wanting 960 are never in step, and encoding a
/// short fill padded with last time's tail is a splice in every frame. Muted sends silence
/// rather than nothing, so a muted peer reads as quiet and not as gone.
pub(crate) fn run_encoder(
    mut source: Box<dyn AudioSource>,
    hub: Arc<MediaHub>,
    stop: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
) {
    let mut encoder = match opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip)
    {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("bevy_iroh: opus encoder: {e}");
            return;
        }
    };
    let _ = encoder.set_bitrate(opus::Bitrate::Bits(BITRATE));
    let _ = encoder.set_inband_fec(true);
    let _ = encoder.set_packet_loss_perc(10);
    let mut resampler = Resampler::new(source.sample_rate(), RATE);
    let mut raw = vec![0f32; FRAME * 4];
    let mut pcm: Vec<f32> = Vec::with_capacity(FRAME * 4);
    let mut packet = vec![0u8; MAX_PACKET];
    let silence = vec![0f32; FRAME];
    let mut seqs: std::collections::HashMap<u64, Seq> = Default::default();
    while !stop.load(Ordering::Relaxed) {
        let n = source.read(&mut raw);
        if n == 0 {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        resampler.push(&raw[..n], &mut pcm);
        while pcm.len() >= FRAME {
            let frame: Vec<f32> = pcm.drain(..FRAME).collect();
            level.store(rms(&frame).to_bits(), Ordering::Relaxed);
            let published = hub.published();
            if published.is_empty() {
                continue;
            }
            let all_muted = published
                .iter()
                .all(|(_, muted)| muted.load(Ordering::Relaxed));
            let mut live: Option<usize> = None;
            let mut quiet: Option<usize> = None;
            for (track, muted) in published {
                let slot = if muted.load(Ordering::Relaxed) || all_muted {
                    &mut quiet
                } else {
                    &mut live
                };
                let len = match slot {
                    Some(len) => *len,
                    None => {
                        let src = if muted.load(Ordering::Relaxed) {
                            &silence
                        } else {
                            &frame
                        };
                        let Ok(len) = encoder.encode_float(src, &mut packet) else {
                            continue;
                        };
                        *slot = Some(len);
                        len
                    }
                };
                let seq = seqs.entry(track).or_default().next();
                hub.send_audio(track, seq, &packet[..len]);
            }
        }
    }
}

fn rms(frame: &[f32]) -> f32 {
    (frame.iter().map(|s| s * s).sum::<f32>() / frame.len().max(1) as f32).sqrt()
}

/// Linear resampling. Voice does not need better, and it needs no dependency.
struct Resampler {
    from: u32,
    to: u32,
    pos: f64,
    last: f32,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Self {
            from,
            to,
            pos: 0.0,
            last: 0.0,
        }
    }

    fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.from == self.to {
            out.extend_from_slice(input);
            return;
        }
        let step = self.from as f64 / self.to as f64;
        // Samples are indexed with `last` at -1 and `input[0]` at 0.
        while self.pos < input.len() as f64 {
            let i = self.pos.floor();
            let t = (self.pos - i) as f32;
            let a = if i < 0.0 {
                self.last
            } else {
                input[i as usize]
            };
            let b_index = i as isize + 1;
            let b = if b_index < input.len() as isize {
                if b_index < 0 {
                    self.last
                } else {
                    input[b_index as usize]
                }
            } else {
                a
            };
            out.push(a + (b - a) * t);
            self.pos += step;
        }
        self.pos -= input.len() as f64;
        if let Some(l) = input.last() {
            self.last = *l;
        }
    }
}

// -- remote tracks -------------------------------------------------------------------------

/// What a remote track has been through. Rates, not totals, are what to read: a total that
/// grew once at startup and one that grows every second look the same at a glance.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TrackStats {
    /// Frames that arrived.
    pub received: u64,
    /// Buffers the output asked for that had to be silence.
    pub starved: u64,
    /// Frames thrown away because the buffer was full.
    pub dropped: u64,
    /// Frames concealed by the decoder because they never came.
    pub concealed: u64,
    /// Times the target grew after an underrun.
    pub regrows: u64,
    /// Times the target shrank after clean play.
    pub shrinks: u64,
    /// The current jitter target, in milliseconds.
    pub target_ms: u32,
    /// Frames waiting to be played.
    pub depth: u32,
}

/// Frames from one remote track, and how to play them.
pub struct RemoteTrack {
    jitter: Mutex<Jitter>,
    /// Unit gain as f32 bits.
    gain: AtomicU32,
    /// -1 (left) to 1 (right), as f32 bits.
    pan: AtomicU32,
    /// RMS of the last decoded frame, as f32 bits.
    level: AtomicU32,
    received: AtomicU64,
}

impl RemoteTrack {
    pub(crate) fn new() -> Self {
        Self {
            jitter: Mutex::new(Jitter::default()),
            gain: AtomicU32::new(1.0f32.to_bits()),
            pan: AtomicU32::new(0.0f32.to_bits()),
            level: AtomicU32::new(0.0f32.to_bits()),
            received: AtomicU64::new(0),
        }
    }

    pub(crate) fn push(&self, seq: u32, frame: Bytes) {
        self.received.fetch_add(1, Ordering::Relaxed);
        self.jitter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(seq, frame);
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain
            .store(gain.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
    }

    pub fn set_pan(&self, pan: f32) {
        self.pan
            .store(pan.clamp(-1.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// RMS of the last 20 ms decoded, 0 to about 1.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    /// Frames that have arrived, ever. Advancing between two looks is the sign of life.
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> TrackStats {
        let j = self.jitter.lock().unwrap_or_else(|e| e.into_inner());
        TrackStats {
            received: self.received(),
            starved: j.starved,
            dropped: j.dropped,
            concealed: j.concealed,
            regrows: j.regrows,
            shrinks: j.shrinks,
            target_ms: (j.target * 20) as u32,
            depth: j.frames.len() as u32,
        }
    }

    fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }

    fn pan(&self) -> f32 {
        f32::from_bits(self.pan.load(Ordering::Relaxed))
    }
}

/// Frames the buffer holds before it starts playing: 60 ms.
const JITTER_START: usize = 3;
/// The most it will grow to for a conversation: 240 ms. More is a buffer that has stopped
/// being a conversation.
const JITTER_MAX: usize = 12;
/// Room above the target before frames are dropped.
const JITTER_SLACK: usize = 12;
/// Underruns in the first second of a track do not grow the target: subscribing, building
/// the decoder and the first burst starve it a few times before anyone has said a word.
const JITTER_GRACE_FRAMES: u64 = 50;
/// Clean play, in frames, that halves the target: ten seconds.
const JITTER_DECAY_FRAMES: u64 = 500;

/// Reorders frames and absorbs network jitter.
///
/// Waits for `target` frames before playing, and again after every underrun. An underrun grows
/// the target; ten seconds of clean play shrinks it back. Excess over the target is spent by
/// the mixer playing slightly fast (see [`Jitter::drift`]); dropping is the last resort.
#[derive(Default)]
struct Jitter {
    frames: BTreeMap<u32, Bytes>,
    next: Option<u32>,
    /// Frames to bank before playing.
    target: usize,
    /// Waiting to bank `target` frames.
    priming: bool,
    /// Frames played since the last underrun.
    clean: u64,
    /// Frames played, ever.
    played: u64,
    starved: u64,
    dropped: u64,
    concealed: u64,
    regrows: u64,
    shrinks: u64,
}

enum Pull {
    Frame(Bytes),
    /// Expected a frame and it is not here. The one after it, if it has arrived, carries a
    /// low-bitrate copy of it (opus in-band FEC).
    Lost {
        next: Option<Bytes>,
    },
    /// Nothing to play: silence, and prime again.
    Idle,
}

impl Jitter {
    fn target(&mut self) -> usize {
        if self.target == 0 {
            self.target = JITTER_START;
            self.priming = true;
        }
        self.target
    }

    fn push(&mut self, seq: u32, frame: Bytes) {
        let target = self.target();
        if let Some(next) = self.next
            && seq.wrapping_sub(next) > u32::MAX / 2
        {
            // Older than what we already played.
            self.dropped += 1;
            return;
        }
        self.frames.insert(seq, frame);
        // A peer can keep sending whether or not anything here plays; the buffer cannot grow
        // without bound. Keep the newest.
        while self.frames.len() > target + JITTER_SLACK {
            let oldest = *self.frames.keys().next().expect("non-empty");
            self.frames.remove(&oldest);
            self.dropped += 1;
            self.next = None;
        }
    }

    fn pull(&mut self) -> Pull {
        let target = self.target();
        if self.priming {
            if self.frames.len() < target {
                return Pull::Idle;
            }
            self.priming = false;
        }
        let next = match self.next {
            Some(next) => next,
            None => {
                let Some(first) = self.frames.keys().next().copied() else {
                    return self.underrun();
                };
                first
            }
        };
        if let Some(frame) = self.frames.remove(&next) {
            self.next = Some(next.wrapping_add(1));
            self.played_one();
            return Pull::Frame(frame);
        }
        if self.frames.is_empty() {
            return self.underrun();
        }
        // A gap with later frames behind it: conceal it and move on.
        self.next = Some(next.wrapping_add(1));
        self.concealed += 1;
        self.played_one();
        Pull::Lost {
            next: self.frames.get(&next.wrapping_add(1)).cloned(),
        }
    }

    fn played_one(&mut self) {
        self.clean += 1;
        self.played += 1;
        if self.clean >= JITTER_DECAY_FRAMES && self.target > JITTER_START {
            self.target = (self.target / 2).max(JITTER_START);
            self.shrinks += 1;
            self.clean = 0;
        }
    }

    /// Nothing to play. Prime again, and if this is not the track's first second, take it as
    /// a sign the network needs more cushion.
    fn underrun(&mut self) -> Pull {
        self.starved += 1;
        self.clean = 0;
        self.next = None;
        self.priming = true;
        if self.played >= JITTER_GRACE_FRAMES && self.target < JITTER_MAX {
            self.target = (self.target * 2).min(JITTER_MAX);
            self.regrows += 1;
        }
        Pull::Idle
    }

    /// The playback-rate bend that spends standing backlog: up to two percent faster when the
    /// buffer holds more than the target, slower when less. A third of a semitone, which
    /// speech carries without anyone noticing.
    fn drift(&mut self) -> f64 {
        const MAX_DRIFT: f64 = 0.02;
        let target = self.target() as f64;
        let excess = self.frames.len() as f64 - target;
        (1.0 + (excess / target) * MAX_DRIFT).clamp(1.0 - MAX_DRIFT, 1.0 + MAX_DRIFT)
    }
}

// -- the mixer -----------------------------------------------------------------------------

struct Playing {
    track: Arc<RemoteTrack>,
    decoder: opus::Decoder,
    /// Decoded, at 48 kHz, waiting to be rendered.
    pcm: std::collections::VecDeque<f32>,
    /// Where this buffer starts between two source samples.
    pos: f64,
}

/// Sums every remote track into an output buffer. Owned by the output device's thread.
pub struct Mixer {
    hub: Arc<MediaHub>,
    playing: Vec<(u64, Playing)>,
    scratch: Vec<f32>,
}

impl Mixer {
    pub(crate) fn new(hub: Arc<MediaHub>) -> Self {
        Self {
            hub,
            playing: Vec::new(),
            scratch: vec![0.0; MAX_DECODED],
        }
    }

    /// Fill `out`, interleaved with `channels` channels at `rate`, with the mix of every remote
    /// track. Silence where nothing is playing. A track that cannot fill the whole buffer
    /// contributes silence for the whole buffer: one clean gap, not a splice.
    pub fn render(&mut self, out: &mut [f32], channels: usize, rate: u32) {
        out.fill(0.0);
        let channels = channels.max(1);
        let frames_out = out.len() / channels;
        if frames_out == 0 {
            return;
        }
        self.sync_tracks();
        let base_step = RATE as f64 / rate.max(1) as f64;
        for (_, playing) in &mut self.playing {
            let drift = playing
                .track
                .jitter
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drift();
            let step = base_step * drift;
            let end = playing.pos + frames_out as f64 * step;
            let needed = end.ceil() as usize + 1;
            Self::top_up(playing, needed, &mut self.scratch);
            if playing.pcm.len() < needed {
                // Whole buffer of silence; what is banked plays once there is enough.
                playing.pos = 0.0;
                continue;
            }
            let gain = playing.track.gain();
            let (left, right) = pan_gains(playing.track.pan());
            let mut pos = playing.pos;
            for f in 0..frames_out {
                let i = pos.floor() as usize;
                let t = (pos - i as f64) as f32;
                let a = playing.pcm[i];
                let b = playing.pcm.get(i + 1).copied().unwrap_or(a);
                let s = (a + (b - a) * t) * gain;
                let base = f * channels;
                if channels >= 2 {
                    out[base] += s * left;
                    out[base + 1] += s * right;
                } else {
                    out[base] += s;
                }
                pos += step;
            }
            let consumed = end.floor() as usize;
            playing.pcm.drain(..consumed.min(playing.pcm.len()));
            playing.pos = end - consumed as f64;
        }
        for s in out.iter_mut() {
            *s = s.clamp(-1.0, 1.0);
        }
    }

    fn sync_tracks(&mut self) {
        let remotes = self.hub.remotes();
        self.playing
            .retain(|(id, _)| remotes.iter().any(|(r, _)| r == id));
        for (id, track) in remotes {
            if self.playing.iter().any(|(p, _)| *p == id) {
                continue;
            }
            if let Ok(decoder) = opus::Decoder::new(RATE, opus::Channels::Mono) {
                self.playing.push((
                    id,
                    Playing {
                        track,
                        decoder,
                        pcm: Default::default(),
                        pos: 0.0,
                    },
                ));
            }
        }
    }

    fn top_up(playing: &mut Playing, needed: usize, scratch: &mut [f32]) {
        while playing.pcm.len() < needed {
            let pull = playing
                .track
                .jitter
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pull();
            let decoded = match pull {
                Pull::Frame(bytes) => playing
                    .decoder
                    .decode_float(&bytes, scratch, false)
                    .unwrap_or(0),
                // The next frame's FEC data reconstructs this one; without it, the decoder
                // extrapolates from what it last heard.
                Pull::Lost { next: Some(bytes) } => playing
                    .decoder
                    .decode_float(&bytes, scratch, true)
                    .unwrap_or(0),
                Pull::Lost { next: None } => playing
                    .decoder
                    .decode_float(&[], scratch, false)
                    .unwrap_or(0),
                Pull::Idle => {
                    playing.track.level.store(0f32.to_bits(), Ordering::Relaxed);
                    return;
                }
            };
            if decoded == 0 {
                return;
            }
            playing
                .track
                .level
                .store(rms(&scratch[..decoded]).to_bits(), Ordering::Relaxed);
            playing.pcm.extend(&scratch[..decoded]);
        }
    }
}

/// Constant-power panning.
fn pan_gains(pan: f32) -> (f32, f32) {
    let angle = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
    (angle.cos(), angle.sin())
}

// -- cpal ----------------------------------------------------------------------------------

/// Samples from the capture callback, and a way to wait for them.
#[derive(Default)]
struct MicBuffer {
    samples: Mutex<std::collections::VecDeque<f32>>,
    arrived: Condvar,
}

/// The default input device, mono, at whatever rate it prefers.
pub struct Microphone {
    rate: u32,
    buffer: Arc<MicBuffer>,
    _keep: std::sync::mpsc::Sender<()>,
}

/// How much capture may bank before the reader is behind: three encoder frames. More is
/// sender-side latency that never comes back.
const MIC_BACKLOG_FRAMES: usize = 3;

impl Microphone {
    /// Opens the default input device. The stream lives on its own thread until this is dropped.
    pub fn default_device() -> Result<Self, String> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        let host = cpal::default_host();
        let device = host.default_input_device().ok_or("no input device")?;
        let config = device.default_input_config().map_err(|e| e.to_string())?;
        let rate = config.sample_rate();
        let channels = config.channels() as usize;
        let buffer = Arc::new(MicBuffer::default());
        let sink = buffer.clone();
        let cap = (rate as usize / 50) * MIC_BACKLOG_FRAMES;
        let (keep, gone) = std::sync::mpsc::channel::<()>();
        let (ready, started) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("bevy_iroh-mic".into())
            .spawn(move || {
                let stream = device.build_input_stream(
                    config.config(),
                    move |data: &[f32], _| {
                        {
                            let mut b = sink.samples.lock().unwrap_or_else(|e| e.into_inner());
                            for frame in data.chunks(channels) {
                                b.push_back(frame.iter().sum::<f32>() / channels as f32);
                            }
                            while b.len() > cap {
                                b.pop_front();
                            }
                        }
                        sink.arrived.notify_one();
                    },
                    |e| tracing::warn!("bevy_iroh: microphone: {e}"),
                    None,
                );
                let stream = match stream.and_then(|s| s.play().map(|_| s)) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready.send(Err(e.to_string()));
                        return;
                    }
                };
                let _ = ready.send(Ok(()));
                // Parked until the `Microphone` is dropped.
                let _ = gone.recv();
                drop(stream);
            })
            .map_err(|e| e.to_string())?;
        started
            .recv()
            .map_err(|_| "microphone thread died".to_string())??;
        Ok(Self {
            rate,
            buffer,
            _keep: keep,
        })
    }
}

impl AudioSource for Microphone {
    fn sample_rate(&self) -> u32 {
        self.rate
    }

    /// Waits up to 20 ms for the device rather than polling.
    fn read(&mut self, out: &mut [f32]) -> usize {
        let mut b = self
            .buffer
            .samples
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if b.is_empty() {
            let (guard, _) = self
                .buffer
                .arrived
                .wait_timeout(b, Duration::from_millis(20))
                .unwrap_or_else(|e| e.into_inner());
            b = guard;
        }
        let n = out.len().min(b.len());
        for s in out.iter_mut().take(n) {
            *s = b.pop_front().unwrap_or(0.0);
        }
        n
    }
}

/// The default output device.
pub struct Speaker;

impl AudioOutput for Speaker {
    fn start(
        self: Box<Self>,
        mixer: Arc<Mutex<Mixer>>,
        stop: Arc<AtomicBool>,
    ) -> Result<(), String> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("no output device")?;
        let config = device.default_output_config().map_err(|e| e.to_string())?;
        let rate = config.sample_rate();
        let channels = config.channels() as usize;
        tracing::info!("bevy_iroh: speaker at {rate} Hz, {channels} channels");
        let (ready, started) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("bevy_iroh-speaker".into())
            .spawn(move || {
                let stream = device.build_output_stream(
                    config.config(),
                    move |data: &mut [f32], _| {
                        mixer
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .render(data, channels, rate);
                    },
                    |e| tracing::warn!("bevy_iroh: speaker: {e}"),
                    None,
                );
                let stream = match stream.and_then(|s| s.play().map(|_| s)) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready.send(Err(e.to_string()));
                        return;
                    }
                };
                let _ = ready.send(Ok(()));
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                drop(stream);
            })
            .map_err(|e| e.to_string())?;
        started
            .recv()
            .map_err(|_| "speaker thread died".to_string())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(n: u8) -> Bytes {
        Bytes::from(vec![n])
    }

    #[test]
    fn resampler_keeps_length_proportional() {
        let mut r = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        for _ in 0..100 {
            r.push(&vec![0.5; 441], &mut out);
        }
        assert!((out.len() as i64 - 48_000).abs() < 4, "{}", out.len());
        assert!(out.iter().all(|s| (s - 0.5).abs() < 1e-6));
    }

    #[test]
    fn jitter_primes_then_plays_in_order_and_conceals_gaps() {
        let mut j = Jitter::default();
        assert!(matches!(j.pull(), Pull::Idle));
        for seq in [2u32, 0, 1] {
            j.push(seq, frame(seq as u8));
        }
        assert!(matches!(j.pull(), Pull::Frame(b) if b[0] == 0));
        assert!(matches!(j.pull(), Pull::Frame(b) if b[0] == 1));
        j.push(4, frame(4));
        assert!(matches!(j.pull(), Pull::Frame(b) if b[0] == 2));
        // 3 is missing and 4 is here: conceal 3 with 4's FEC.
        assert!(matches!(j.pull(), Pull::Lost { next: Some(b) } if b[0] == 4));
        assert!(matches!(j.pull(), Pull::Frame(b) if b[0] == 4));
        assert_eq!(j.concealed, 1);
        // Empty: an underrun, which primes again.
        assert!(matches!(j.pull(), Pull::Idle));
        assert_eq!(j.starved, 1);
        assert!(j.priming);
    }

    #[test]
    fn jitter_grows_on_underrun_after_grace_and_shrinks_after_clean_play() {
        let mut j = Jitter::default();
        // First second: underruns do not grow the target.
        for seq in 0..3u32 {
            j.push(seq, frame(0));
        }
        for _ in 0..3 {
            j.pull();
        }
        assert!(matches!(j.pull(), Pull::Idle));
        assert_eq!(j.target, JITTER_START);
        // Past the grace: an underrun doubles it.
        let mut seq = 3u32;
        while j.played < JITTER_GRACE_FRAMES {
            for _ in 0..3 {
                j.push(seq, frame(0));
                seq += 1;
            }
            for _ in 0..3 {
                j.pull();
            }
        }
        assert!(matches!(j.pull(), Pull::Idle));
        assert_eq!(j.target, JITTER_START * 2);
        assert_eq!(j.regrows, 1);
        // Ten seconds of clean play halves it back.
        for _ in 0..(JITTER_DECAY_FRAMES + 8) {
            j.push(seq, frame(0));
            seq += 1;
            if !j.priming || j.frames.len() >= j.target {
                j.pull();
            }
        }
        assert_eq!(j.target, JITTER_START);
        assert!(j.shrinks >= 1);
    }

    #[test]
    fn jitter_drops_when_nobody_plays() {
        let mut j = Jitter::default();
        for seq in 0..100u32 {
            j.push(seq, frame(0));
        }
        assert!(j.frames.len() <= JITTER_START + JITTER_SLACK);
        assert!(j.dropped > 0);
    }

    #[test]
    fn drift_is_bounded_and_centred() {
        let mut j = Jitter::default();
        for seq in 0..3u32 {
            j.push(seq, frame(0));
        }
        assert!((j.drift() - 1.0).abs() < 1e-9);
        for seq in 3..15u32 {
            j.push(seq, frame(0));
        }
        assert!((j.drift() - 1.02).abs() < 1e-9);
        j.frames.clear();
        assert!((j.drift() - 0.98).abs() < 1e-9);
    }
}
