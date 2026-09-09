//! Voice: capture, opus, a jitter buffer per remote track, mixing and playback.
//!
//! Sources and outputs are traits so the same pipeline runs against a microphone and speakers,
//! or against a sine wave and a buffer in a test. Everything speaks mono f32 at 48 kHz inside;
//! devices at other rates are resampled at the edges. The codec is behind [`VoiceDecoder`] so
//! that libopus on a desktop and WebCodecs in a browser feed the same mixer.
//!
//! The buffering rules are substrate's, learned on real calls: a buffer is standing latency,
//! so it starts small and grows only when starved; an underrun is a whole buffer of silence
//! rather than a splice; backlog is spent by playing slightly fast rather than kept; and every
//! silent failure has a counter, because "a subscription that connects and then delivers
//! nothing looks exactly like a person who is not talking".

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
};

use bytes::Bytes;

use super::transport::MediaHub;

pub const RATE: u32 = 48_000;
/// 20 ms at 48 kHz: opus's sweet spot for voice.
pub const FRAME: usize = 960;
/// The longest frame opus allows, 120 ms: a peer on another build may send them.
pub(crate) const MAX_DECODED: usize = 5760;
/// Opus at 64 kbps is transparent for speech.
pub(crate) const BITRATE: u32 = 64_000;

/// Where microphone samples come from: mono f32 at `sample_rate`.
pub trait AudioSource: Send + 'static {
    fn sample_rate(&self) -> u32;
    /// Fill `out` with what has arrived since the last call. Returns how many were written;
    /// fewer than asked means wait, not end. May block briefly waiting for the device on a
    /// desktop; never in a browser, which polls it from a timer.
    fn read(&mut self, out: &mut [f32]) -> usize;
}

/// Where the mix goes. `start` must drive `mixer.render` on its own clock until the returned
/// handle is dropped.
pub trait AudioOutput: Send + 'static {
    fn start(self: Box<Self>, mixer: Arc<Mutex<Mixer>>) -> Result<Running, String>;
}

/// Something running until this is dropped: a device thread, a browser audio graph.
///
/// Dropping sets the flag and then runs the closure, which on a desktop unparks and joins the
/// thread so the device is released before the next open can race it. A browser has no
/// threads; there the flag is enough and a system takes the graph apart on a later frame.
pub struct Running {
    stop: Arc<AtomicBool>,
    finish: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl Running {
    pub fn new(stop: Arc<AtomicBool>, finish: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self {
            stop,
            finish: Some(Box::new(finish)),
        }
    }

    /// A flag and nothing to wait for.
    pub fn flag(stop: Arc<AtomicBool>) -> Self {
        Self { stop, finish: None }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(finish) = self.finish.take() {
            finish();
        }
    }
}

// -- the encoder ---------------------------------------------------------------------------

/// What the encoder shares with the Bevy side: a stop flag and two meters.
pub(crate) struct EncoderShared {
    pub stop: AtomicBool,
    /// RMS of the last 20 ms frame, as f32 bits.
    pub level: AtomicU32,
    /// Peak absolute sample since the last look, as f32 bits. Taken with a swap to zero, so
    /// a meter read once a frame sees the loudest moment in between rather than a sample.
    pub peak: AtomicU32,
}

impl Default for EncoderShared {
    fn default() -> Self {
        Self {
            stop: AtomicBool::new(false),
            level: AtomicU32::new(0),
            peak: AtomicU32::new(0),
        }
    }
}

impl EncoderShared {
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Ordering::Relaxed))
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    pub(crate) fn meter(&self, frame: &[f32]) {
        self.level.store(rms(frame).to_bits(), Ordering::Relaxed);
        let peak = frame.iter().fold(0f32, |m, s| m.max(s.abs()));
        self.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
    }
}

/// Cuts a stream of samples into exact frames.
///
/// A device delivering 512-sample blocks and an encoder wanting 960 are never in step, and
/// encoding a short fill padded with last time's tail is a splice in every frame. Samples
/// accumulate here and leave `FRAME` at a time.
pub(crate) struct Framer {
    resampler: Resampler,
    pcm: Vec<f32>,
}

impl Framer {
    pub fn new(from_rate: u32) -> Self {
        Self {
            resampler: Resampler::new(from_rate, RATE),
            pcm: Vec::with_capacity(FRAME * 4),
        }
    }

    pub fn push(&mut self, samples: &[f32]) {
        self.resampler.push(samples, &mut self.pcm);
    }

    pub fn next_frame(&mut self) -> Option<Vec<f32>> {
        (self.pcm.len() >= FRAME).then(|| self.pcm.drain(..FRAME).collect())
    }
}

/// Per published track: the next sequence number.
#[derive(Default)]
pub(crate) struct Seqs(std::collections::HashMap<u64, u32>);

impl Seqs {
    pub fn next(&mut self, track: u64) -> u32 {
        let seq = self.0.entry(track).or_default();
        let n = *seq;
        *seq = seq.wrapping_add(1);
        n
    }
}

/// Reads a source, encodes 20 ms frames, and sends each to every published track. Runs on
/// its own thread until `shared.stop`; muted tracks get silence rather than nothing, so a
/// muted peer reads as quiet and not as gone.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn run_encoder(
    mut source: Box<dyn AudioSource>,
    hub: Arc<MediaHub>,
    shared: Arc<EncoderShared>,
) {
    let mut encoder = match opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip)
    {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("bevy_iroh: opus encoder: {e}");
            return;
        }
    };
    let _ = encoder.set_bitrate(opus::Bitrate::Bits(BITRATE as i32));
    let _ = encoder.set_inband_fec(true);
    let _ = encoder.set_packet_loss_perc(10);
    let mut framer = Framer::new(source.sample_rate());
    let mut raw = vec![0f32; FRAME * 4];
    let mut packet = vec![0u8; 1275];
    let silence = vec![0f32; FRAME];
    let mut seqs = Seqs::default();
    while !shared.stop.load(Ordering::Relaxed) {
        let n = source.read(&mut raw);
        if n == 0 {
            std::thread::sleep(std::time::Duration::from_millis(2));
            continue;
        }
        framer.push(&raw[..n]);
        while let Some(frame) = framer.next_frame() {
            let published = hub.published();
            let all_muted = !published.is_empty()
                && published
                    .iter()
                    .all(|(_, muted)| muted.load(Ordering::Relaxed));
            // The meter reads what would go out: nothing, while everything is muted.
            if all_muted {
                shared.meter(&silence);
            } else {
                shared.meter(&frame);
            }
            if published.is_empty() {
                continue;
            }
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
                let seq = seqs.next(track);
                hub.send_audio(track, seq, &packet[..len]);
            }
        }
    }
}

pub(crate) fn rms(frame: &[f32]) -> f32 {
    (frame.iter().map(|s| s * s).sum::<f32>() / frame.len().max(1) as f32).sqrt()
}

/// Linear resampling. Voice does not need better, and it needs no dependency.
pub(crate) struct Resampler {
    from: u32,
    to: u32,
    pos: f64,
    last: f32,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self {
            from,
            to,
            pos: 0.0,
            last: 0.0,
        }
    }

    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
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
    /// Peak since the last look, as f32 bits.
    peak: AtomicU32,
    received: AtomicU64,
}

impl Default for RemoteTrack {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteTrack {
    pub(crate) fn new() -> Self {
        Self {
            jitter: Mutex::new(Jitter::default()),
            gain: AtomicU32::new(1.0f32.to_bits()),
            pan: AtomicU32::new(0.0f32.to_bits()),
            level: AtomicU32::new(0.0f32.to_bits()),
            peak: AtomicU32::new(0),
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

    /// Loudest sample decoded since the last call.
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Ordering::Relaxed))
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

    fn meter(&self, decoded: &[f32]) {
        self.level.store(rms(decoded).to_bits(), Ordering::Relaxed);
        let peak = decoded.iter().fold(0f32, |m, s| m.max(s.abs()));
        self.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
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

// -- the decoder seam ----------------------------------------------------------------------

/// One packet's worth of input to a decoder.
pub enum Packet<'a> {
    /// A frame that arrived.
    Data(&'a [u8]),
    /// A frame that did not; the next one did and carries a low-bitrate copy of it.
    Fec(&'a [u8]),
    /// A frame that did not, with nothing to reconstruct it from: extrapolate.
    Lost,
}

macro_rules! decoder_trait {
    ($($bound:tt)*) => {
        /// Turns opus packets back into 48 kHz mono. libopus answers at once; a browser's
        /// WebCodecs answers on a later task, which is why output is drained rather than
        /// returned. Lives on the output device's thread, where there is one.
        pub trait VoiceDecoder $($bound)* {
            fn push(&mut self, packet: Packet<'_>);
            /// Move what has decoded since the last call to the end of `pcm`. Returns how many.
            fn drain(&mut self, pcm: &mut VecDeque<f32>) -> usize;
            /// Samples pushed and not yet answered for. Zero on a synchronous decoder.
            fn in_flight(&self) -> usize;
        }
    };
}
#[cfg(not(target_arch = "wasm32"))]
decoder_trait!(: Send);
#[cfg(target_arch = "wasm32")]
decoder_trait!();

#[cfg(not(target_arch = "wasm32"))]
struct OpusDecoder {
    decoder: opus::Decoder,
    scratch: Vec<f32>,
    out: Vec<f32>,
}

#[cfg(not(target_arch = "wasm32"))]
impl VoiceDecoder for OpusDecoder {
    fn push(&mut self, packet: Packet<'_>) {
        let decoded = match packet {
            Packet::Data(bytes) => self.decoder.decode_float(bytes, &mut self.scratch, false),
            Packet::Fec(bytes) => self.decoder.decode_float(bytes, &mut self.scratch, true),
            Packet::Lost => self.decoder.decode_float(&[], &mut self.scratch, false),
        }
        .unwrap_or(0);
        self.out.extend_from_slice(&self.scratch[..decoded]);
    }

    fn drain(&mut self, pcm: &mut VecDeque<f32>) -> usize {
        let n = self.out.len();
        pcm.extend(self.out.drain(..));
        n
    }

    fn in_flight(&self) -> usize {
        0
    }
}

/// The platform's decoder: libopus here, WebCodecs in a page.
pub(crate) fn new_decoder() -> Option<Box<dyn VoiceDecoder>> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let decoder = opus::Decoder::new(RATE, opus::Channels::Mono).ok()?;
        Some(Box::new(OpusDecoder {
            decoder,
            scratch: vec![0.0; MAX_DECODED],
            out: Vec::with_capacity(MAX_DECODED),
        }))
    }
    #[cfg(target_arch = "wasm32")]
    {
        super::web::audio::new_decoder()
    }
}

// -- the mixer -----------------------------------------------------------------------------

struct Playing {
    track: Arc<RemoteTrack>,
    decoder: Box<dyn VoiceDecoder>,
    /// Decoded, at 48 kHz, waiting to be rendered.
    pcm: VecDeque<f32>,
    /// Where this buffer starts between two source samples.
    pos: f64,
    /// Samples of the last drain, for the meter.
    fresh: Vec<f32>,
}

/// Sums every remote track into an output buffer. Owned by the output device's thread.
pub struct Mixer {
    hub: Arc<MediaHub>,
    playing: Vec<(u64, Playing)>,
    /// Master gain, as f32 bits, shared with the settings.
    volume: Arc<AtomicU32>,
}

impl Mixer {
    pub(crate) fn new(hub: Arc<MediaHub>, volume: Arc<AtomicU32>) -> Self {
        Self {
            hub,
            playing: Vec::new(),
            volume,
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
        let volume = f32::from_bits(self.volume.load(Ordering::Relaxed)).clamp(0.0, 4.0);
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
            Self::top_up(playing, needed);
            if playing.pcm.len() < needed {
                // Whole buffer of silence; what is banked plays once there is enough.
                playing.pos = 0.0;
                continue;
            }
            let gain = playing.track.gain() * volume;
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
            if let Some(decoder) = new_decoder() {
                self.playing.push((
                    id,
                    Playing {
                        track,
                        decoder,
                        pcm: Default::default(),
                        pos: 0.0,
                        fresh: Vec::with_capacity(MAX_DECODED),
                    },
                ));
            }
        }
    }

    fn top_up(playing: &mut Playing, needed: usize) {
        loop {
            let before = playing.pcm.len();
            if playing.decoder.drain(&mut playing.pcm) > 0 {
                playing.fresh.clear();
                playing.fresh.extend(playing.pcm.range(before..));
                playing.track.meter(&playing.fresh);
            }
            if playing.pcm.len() + playing.decoder.in_flight() >= needed {
                return;
            }
            let pull = playing
                .track
                .jitter
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pull();
            match pull {
                Pull::Frame(bytes) => playing.decoder.push(Packet::Data(&bytes)),
                // The next frame's FEC data reconstructs this one; without it, the decoder
                // extrapolates from what it last heard.
                Pull::Lost { next: Some(bytes) } => playing.decoder.push(Packet::Fec(&bytes)),
                Pull::Lost { next: None } => playing.decoder.push(Packet::Lost),
                Pull::Idle => {
                    playing.track.level.store(0f32.to_bits(), Ordering::Relaxed);
                    return;
                }
            }
            // An asynchronous decoder answers on a later task: what was just pushed cannot
            // be drained now, and pushing until `needed` is covered by in-flight samples is
            // what the loop condition does.
            if playing.decoder.in_flight() > 0
                && playing.pcm.len() + playing.decoder.in_flight() >= needed
            {
                return;
            }
        }
    }
}

/// Constant-power panning.
fn pan_gains(pan: f32) -> (f32, f32) {
    let angle = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
    (angle.cos(), angle.sin())
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
    fn framer_cuts_exact_frames() {
        let mut f = Framer::new(48_000);
        f.push(&[0.1; 500]);
        assert!(f.next_frame().is_none());
        f.push(&[0.1; 500]);
        assert_eq!(f.next_frame().map(|v| v.len()), Some(FRAME));
        assert!(f.next_frame().is_none());
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
