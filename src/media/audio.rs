//! Voice: capture, opus, a jitter buffer per remote track, mixing and playback.
//!
//! Sources and outputs are traits so the same pipeline runs against a microphone and speakers,
//! or against a sine wave and a buffer in a test. Everything speaks mono f32 at 48 kHz inside;
//! devices at other rates are resampled at the edges.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;

use super::transport::{MediaHub, Seq};

pub const RATE: u32 = 48_000;
/// 20 ms at 48 kHz: opus's sweet spot for voice.
pub const FRAME: usize = 960;
const MAX_PACKET: usize = 1275;

/// Where microphone samples come from: mono f32 at `sample_rate`.
pub trait AudioSource: Send + 'static {
    fn sample_rate(&self) -> u32;
    /// Fill `out` with what has arrived since the last call. Returns how many were written;
    /// fewer than asked means wait, not end.
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
    let _ = encoder.set_bitrate(opus::Bitrate::Bits(32_000));
    let _ = encoder.set_inband_fec(true);
    let mut resampler = Resampler::new(source.sample_rate(), RATE);
    let mut raw = vec![0f32; FRAME * 2];
    let mut pcm: Vec<f32> = Vec::with_capacity(FRAME * 4);
    let mut packet = vec![0u8; MAX_PACKET];
    let mut seqs: std::collections::HashMap<u64, Seq> = Default::default();
    while !stop.load(Ordering::Relaxed) {
        let n = source.read(&mut raw);
        if n == 0 {
            std::thread::sleep(Duration::from_millis(5));
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
            if all_muted {
                continue;
            }
            let Ok(len) = encoder.encode_float(&frame, &mut packet) else {
                continue;
            };
            for (track, muted) in published {
                if muted.load(Ordering::Relaxed) {
                    continue;
                }
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

/// Frames from one remote track, and how to play them.
pub struct RemoteTrack {
    jitter: Mutex<Jitter>,
    /// Unit gain as f32 bits.
    gain: AtomicU32,
    /// -1 (left) to 1 (right), as f32 bits.
    pan: AtomicU32,
    /// RMS of the last decoded frame, as f32 bits.
    level: AtomicU32,
}

impl RemoteTrack {
    pub(crate) fn new() -> Self {
        Self {
            jitter: Mutex::new(Jitter::default()),
            gain: AtomicU32::new(1.0f32.to_bits()),
            pan: AtomicU32::new(0.0f32.to_bits()),
            level: AtomicU32::new(0.0f32.to_bits()),
        }
    }

    pub(crate) fn push(&self, seq: u32, frame: Bytes) {
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

    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }

    fn pan(&self) -> f32 {
        f32::from_bits(self.pan.load(Ordering::Relaxed))
    }
}

/// Reorders frames and absorbs network jitter. Holds `target` frames before starting; a gap is
/// concealed by the decoder; a buffer that grows past `max` is skipped forward, since latency
/// that has crept in never leaves on its own.
#[derive(Default)]
struct Jitter {
    frames: BTreeMap<u32, Bytes>,
    next: Option<u32>,
    /// Frames the buffer waits to hold before playing.
    target: usize,
    received: u64,
}

const JITTER_TARGET: usize = 3;
const JITTER_MAX: usize = 12;

enum Pull {
    Frame(Bytes),
    /// Expected a frame and it is not here: conceal.
    Lost,
    /// Nothing to play: silence.
    Idle,
}

impl Jitter {
    fn push(&mut self, seq: u32, frame: Bytes) {
        self.received += 1;
        if let Some(next) = self.next
            && seq.wrapping_sub(next) > u32::MAX / 2
        {
            // Older than what we already played.
            return;
        }
        self.frames.insert(seq, frame);
    }

    fn pull(&mut self) -> Pull {
        let target = if self.target == 0 {
            JITTER_TARGET
        } else {
            self.target
        };
        let Some(next) = self.next else {
            if self.frames.len() < target {
                return Pull::Idle;
            }
            let first = *self.frames.keys().next().expect("non-empty");
            self.next = Some(first);
            return self.pull();
        };
        if self.frames.len() > JITTER_MAX {
            // Skip forward, keeping `target` frames of cushion.
            let keep_from = *self
                .frames
                .keys()
                .nth(self.frames.len() - target)
                .expect("in range");
            self.frames = self.frames.split_off(&keep_from);
            self.next = Some(keep_from);
            return self.pull();
        }
        if let Some(frame) = self.frames.remove(&next) {
            self.next = Some(next.wrapping_add(1));
            return Pull::Frame(frame);
        }
        if self.frames.is_empty() {
            // Starved: wait for a cushion again before continuing.
            self.next = None;
            return Pull::Idle;
        }
        self.next = Some(next.wrapping_add(1));
        Pull::Lost
    }
}

// -- the mixer -----------------------------------------------------------------------------

struct Playing {
    track: Arc<RemoteTrack>,
    decoder: opus::Decoder,
    /// Decoded, at 48 kHz, waiting to be rendered.
    pcm: std::collections::VecDeque<f32>,
}

/// Sums every remote track into an output buffer. Owned by the output device's thread.
pub struct Mixer {
    hub: Arc<MediaHub>,
    playing: Vec<(u64, Playing)>,
    frame: Vec<f32>,
    resample_pos: f64,
}

impl Mixer {
    pub(crate) fn new(hub: Arc<MediaHub>) -> Self {
        Self {
            hub,
            playing: Vec::new(),
            frame: vec![0.0; FRAME],
            resample_pos: 0.0,
        }
    }

    /// Fill `out`, interleaved with `channels` channels at `rate`, with the mix of every remote
    /// track. Silence where nothing is playing.
    pub fn render(&mut self, out: &mut [f32], channels: usize, rate: u32) {
        out.fill(0.0);
        let channels = channels.max(1);
        let frames_out = out.len() / channels;
        if frames_out == 0 {
            return;
        }
        self.sync_tracks();
        // Source samples are at 48 kHz; `resample_pos` is where this buffer starts between them.
        let step = RATE as f64 / rate.max(1) as f64;
        let end = self.resample_pos + frames_out as f64 * step;
        let needed = end.ceil() as usize + 1;
        for (_, playing) in &mut self.playing {
            Self::top_up(playing, needed, &mut self.frame);
            if playing.pcm.is_empty() {
                continue;
            }
            let gain = playing.track.gain();
            let (left, right) = pan_gains(playing.track.pan());
            let mut pos = self.resample_pos;
            for f in 0..frames_out {
                let i = pos.floor() as usize;
                let t = (pos - i as f64) as f32;
                let a = playing.pcm.get(i).copied().unwrap_or(0.0);
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
        }
        let consumed = end.floor() as usize;
        for (_, playing) in &mut self.playing {
            playing.pcm.drain(..consumed.min(playing.pcm.len()));
        }
        self.resample_pos = end - consumed as f64;
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
                    },
                ));
            }
        }
    }

    fn top_up(playing: &mut Playing, needed: usize, frame: &mut [f32]) {
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
                    .decode_float(&bytes, frame, false)
                    .unwrap_or(0),
                Pull::Lost => playing.decoder.decode_float(&[], frame, false).unwrap_or(0),
                Pull::Idle => {
                    playing.track.level.store(0f32.to_bits(), Ordering::Relaxed);
                    break;
                }
            };
            if decoded == 0 {
                break;
            }
            playing
                .track
                .level
                .store(rms(&frame[..decoded]).to_bits(), Ordering::Relaxed);
            playing.pcm.extend(&frame[..decoded]);
        }
    }
}

/// Constant-power panning.
fn pan_gains(pan: f32) -> (f32, f32) {
    let angle = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
    (angle.cos(), angle.sin())
}

// -- cpal ----------------------------------------------------------------------------------

/// The default input device, mono, at whatever rate it prefers.
pub struct Microphone {
    rate: u32,
    buffer: Arc<Mutex<std::collections::VecDeque<f32>>>,
    _keep: std::sync::mpsc::Sender<()>,
}

impl Microphone {
    /// Opens the default input device. The stream lives on its own thread until this is dropped.
    pub fn default_device() -> Result<Self, String> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        let host = cpal::default_host();
        let device = host.default_input_device().ok_or("no input device")?;
        let config = device.default_input_config().map_err(|e| e.to_string())?;
        let rate = config.sample_rate();
        let channels = config.channels() as usize;
        let buffer: Arc<Mutex<std::collections::VecDeque<f32>>> = Default::default();
        let sink = buffer.clone();
        let (keep, gone) = std::sync::mpsc::channel::<()>();
        let (ready, started) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("bevy_iroh-mic".into())
            .spawn(move || {
                let stream = device.build_input_stream(
                    config.config(),
                    move |data: &[f32], _| {
                        let mut b = sink.lock().unwrap_or_else(|e| e.into_inner());
                        for frame in data.chunks(channels) {
                            b.push_back(frame.iter().sum::<f32>() / channels as f32);
                        }
                        // A reader that fell behind gets the newest half second, not a backlog.
                        let cap = RATE as usize / 2;
                        while b.len() > cap {
                            b.pop_front();
                        }
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

    fn read(&mut self, out: &mut [f32]) -> usize {
        let mut b = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
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
    fn jitter_waits_then_plays_in_order_and_conceals_gaps() {
        let mut j = Jitter::default();
        assert!(matches!(j.pull(), Pull::Idle));
        for seq in [2u32, 0, 1] {
            j.push(seq, Bytes::from(vec![seq as u8]));
        }
        assert!(matches!(j.pull(), Pull::Frame(b) if b[0] == 0));
        assert!(matches!(j.pull(), Pull::Frame(b) if b[0] == 1));
        j.push(4, Bytes::from(vec![4]));
        assert!(matches!(j.pull(), Pull::Frame(b) if b[0] == 2));
        assert!(matches!(j.pull(), Pull::Lost));
        assert!(matches!(j.pull(), Pull::Frame(b) if b[0] == 4));
        assert!(matches!(j.pull(), Pull::Idle));
    }

    #[test]
    fn jitter_skips_forward_when_latency_creeps() {
        let mut j = Jitter::default();
        for seq in 0..20u32 {
            j.push(seq, Bytes::from(vec![seq as u8]));
        }
        let Pull::Frame(b) = j.pull() else {
            panic!("expected a frame")
        };
        assert!(b[0] >= 20 - JITTER_TARGET as u8, "skipped to {}", b[0]);
    }
}
