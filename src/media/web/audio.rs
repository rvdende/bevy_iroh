//! The microphone and the speakers in a browser.
//!
//! Capture is a `ScriptProcessorNode` on a 48 kHz `AudioContext`: deprecated, and still the
//! only way to get samples into Rust without a shared-memory build and the cross-origin
//! headers that come with it. Its callback is the encoder's clock: no timer, because a hidden
//! tab clamps timers to a second and a call in a background tab has to keep working. The
//! graph ends in a silent gain, since Chrome will not run a processor that reaches nothing.
//! Opus is the browser's own, through WebCodecs, which answers on a later task; the packets
//! go out from that callback.
//!
//! Playback is one `AudioContext` for the page (a browser caps how many a page may have) with
//! one stereo `ScriptProcessorNode` running the shared [`Mixer`]. Decoding is WebCodecs again,
//! behind the [`VoiceDecoder`] seam, so the jitter buffer and the mixer do not know.

use std::{
    cell::RefCell,
    collections::VecDeque,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AudioContext, AudioContextOptions, AudioContextState, AudioData, AudioDataInit,
    AudioProcessingEvent, AudioSampleFormat, EncodedAudioChunk, EncodedAudioChunkInit,
    EncodedAudioChunkType, GainNode, MediaStream, MediaStreamAudioSourceNode,
    MediaStreamConstraints, ScriptProcessorNode,
};

use super::{describe, exactly, media_devices, stop_stream};
use crate::media::{
    MicrophoneChoice,
    audio::{
        AudioOutput, AudioSource, BITRATE, EncoderShared, FRAME, Framer, Mixer, Packet, RATE,
        Running, Seqs, VoiceDecoder,
    },
    transport::MediaHub,
};

/// Frames the processor hands over at a time: 1024 at 48 kHz is about 21 ms. One of the eight
/// sizes the spec allows; smaller is how a `ScriptProcessorNode` starts glitching under
/// main-thread work, which in this app is a whole renderer.
const BLOCK: u32 = 1024;

const OPUS: &str = "opus";

struct Callbacks {
    _output: Closure<dyn FnMut(JsValue, JsValue)>,
    _error: Closure<dyn FnMut(JsValue)>,
}

// -- capture and encode --------------------------------------------------------------------

/// The encoder: frames in from wherever, opus packets out to every published track.
struct WebEncoder {
    inner: web_sys::AudioEncoder,
    framer: Framer,
    shared: Arc<EncoderShared>,
    hub: Arc<MediaHub>,
    /// Samples handed to the encoder, for its timestamps.
    sent: u64,
    silence: Vec<f32>,
    _callbacks: Callbacks,
}

impl WebEncoder {
    fn new(rate: u32, hub: Arc<MediaHub>, shared: Arc<EncoderShared>) -> Result<Self, String> {
        let seqs = Rc::new(RefCell::new(Seqs::default()));
        let out_hub = hub.clone();
        let on_output = Closure::wrap(Box::new(move |chunk: JsValue, _meta: JsValue| {
            let chunk: EncodedAudioChunk = chunk.unchecked_into();
            let mut payload = vec![0u8; chunk.byte_length() as usize];
            if chunk.copy_to_with_u8_slice(&mut payload).is_err() {
                return;
            }
            let mut seqs = seqs.borrow_mut();
            for (track, _muted) in out_hub.published() {
                let seq = seqs.next(track);
                out_hub.send_audio(track, seq, &payload);
            }
        }) as Box<dyn FnMut(JsValue, JsValue)>);
        let on_error = Closure::wrap(Box::new(|error: JsValue| {
            bevy::log::error!("bevy_iroh: audio encoder: {}", describe(error));
        }) as Box<dyn FnMut(JsValue)>);
        let init = web_sys::AudioEncoderInit::new(
            on_error.as_ref().unchecked_ref(),
            on_output.as_ref().unchecked_ref(),
        );
        let inner = web_sys::AudioEncoder::new(&init).map_err(describe)?;
        let config = web_sys::AudioEncoderConfig::new(OPUS, 1, RATE);
        config.set_bitrate(BITRATE);
        inner.configure(&config).map_err(describe)?;
        Ok(Self {
            inner,
            framer: Framer::new(rate),
            shared,
            hub,
            sent: 0,
            silence: vec![0.0; FRAME],
            _callbacks: Callbacks {
                _output: on_output,
                _error: on_error,
            },
        })
    }

    /// Samples from the device, at the rate the framer was built for.
    fn feed(&mut self, samples: &[f32]) {
        self.framer.push(samples);
        while let Some(frame) = self.framer.next_frame() {
            let published = self.hub.published();
            let all_muted = !published.is_empty()
                && published
                    .iter()
                    .all(|(_, muted)| muted.load(Ordering::Relaxed));
            // One encoder, so a muted track gets what everyone gets: silence when everyone
            // is muted, and the live frame otherwise.
            let src = if all_muted { &self.silence } else { &frame };
            self.shared.meter(src);
            if published.is_empty() {
                continue;
            }
            let data = js_sys::Float32Array::from(src.as_slice());
            let init = AudioDataInit::new(
                data.as_ref(),
                AudioSampleFormat::F32,
                1,
                FRAME as u32,
                RATE as f32,
                0,
            );
            init.set_timestamp_f64(self.sent as f64 * 1e6 / RATE as f64);
            self.sent += FRAME as u64;
            let Ok(audio) = AudioData::new(&init) else {
                continue;
            };
            let _ = self.inner.encode(&audio);
            audio.close();
        }
    }
}

impl Drop for WebEncoder {
    fn drop(&mut self) {
        let _ = self.inner.close();
    }
}

/// One open microphone: the JS handles, parked here because they cannot leave the thread.
struct Session {
    context: AudioContext,
    parts: Rc<RefCell<Parts>>,
    shared: Arc<EncoderShared>,
}

#[derive(Default)]
struct Parts {
    stream: Option<MediaStream>,
    source: Option<MediaStreamAudioSourceNode>,
    processor: Option<ScriptProcessorNode>,
    gain: Option<GainNode>,
    /// Kept alive because JS holds only a pointer into it.
    _callback: Option<Closure<dyn FnMut(AudioProcessingEvent)>>,
}

thread_local! {
    static SESSIONS: RefCell<Vec<Session>> = const { RefCell::new(Vec::new()) };
    static OUTPUT: RefCell<Option<AudioContext>> = const { RefCell::new(None) };
    static PLAYING: RefCell<Vec<Playback>> = const { RefCell::new(Vec::new()) };
}

/// Start capturing and encoding from what the settings name. `Ok(false)` is nothing to open
/// yet (no microphone wanted, or a custom slot still empty).
pub(crate) fn start_encoder(
    choice: &MicrophoneChoice,
    hub: Arc<MediaHub>,
    shared: Arc<EncoderShared>,
) -> Result<bool, String> {
    match choice {
        MicrophoneChoice::None => Ok(false),
        MicrophoneChoice::Custom(slot) => {
            let Some(source) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() else {
                return Ok(false);
            };
            poll_custom(source, hub, shared);
            Ok(true)
        }
        MicrophoneChoice::Default | MicrophoneChoice::Device(_) => {
            let id = match choice {
                MicrophoneChoice::Device(id) => Some(id.as_str()),
                _ => None,
            };
            open_microphone(id, hub, shared)?;
            Ok(true)
        }
    }
}

/// A source with no callback of its own is read from a timer. Fine for a test tone; the
/// microphone does not go this way.
fn poll_custom(mut source: Box<dyn AudioSource>, hub: Arc<MediaHub>, shared: Arc<EncoderShared>) {
    let rate = source.sample_rate();
    wasm_bindgen_futures::spawn_local(async move {
        let mut encoder = match WebEncoder::new(rate, hub, shared.clone()) {
            Ok(e) => e,
            Err(e) => {
                bevy::log::error!("bevy_iroh: opus: {e}");
                return;
            }
        };
        let mut raw = vec![0f32; FRAME * 4];
        while !shared.stop.load(Ordering::Relaxed) {
            let n = source.read(&mut raw);
            if n > 0 {
                encoder.feed(&raw[..n]);
            }
            n0_future::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    });
}

fn open_microphone(
    id: Option<&str>,
    hub: Arc<MediaHub>,
    shared: Arc<EncoderShared>,
) -> Result<(), String> {
    let options = AudioContextOptions::new();
    // Pinned to 48 kHz: opus is only defined at 8, 12, 16, 24 and 48 kHz and a 44.1 kHz
    // machine would otherwise hand the encoder a rate it does not have. The browser resamples
    // the device into the graph.
    options.set_sample_rate(RATE as f32);
    let context = AudioContext::new_with_context_options(&options).map_err(|e| {
        format!(
            "cannot start audio: {}: this needs https, or localhost",
            describe(e)
        )
    })?;
    let rate = context.sample_rate() as u32;
    let parts = Rc::new(RefCell::new(Parts::default()));
    SESSIONS.with_borrow_mut(|sessions| {
        sessions.push(Session {
            context: context.clone(),
            parts: parts.clone(),
            shared: shared.clone(),
        })
    });
    let id = id.map(str::to_string);
    wasm_bindgen_futures::spawn_local(async move {
        match attach(&context, id.as_deref(), rate, hub, shared).await {
            Ok(built) => {
                *parts.borrow_mut() = built;
                bevy::log::info!("bevy_iroh: microphone at {rate} Hz");
            }
            Err(why) => bevy::log::error!("bevy_iroh: microphone: {why}"),
        }
    });
    Ok(())
}

async fn attach(
    context: &AudioContext,
    id: Option<&str>,
    rate: u32,
    hub: Arc<MediaHub>,
    shared: Arc<EncoderShared>,
) -> Result<Parts, String> {
    let devices = media_devices()?;
    let constraints = MediaStreamConstraints::new();
    constraints.set_audio(&exactly(id)?);
    let stream: MediaStream = JsFuture::from(
        devices
            .get_user_media_with_constraints(&constraints)
            .map_err(describe)?,
    )
    .await
    .map_err(describe)?
    .unchecked_into();
    let source = context
        .create_media_stream_source(&stream)
        .map_err(describe)?;
    let processor = context
        .create_script_processor_with_buffer_size_and_number_of_input_channels_and_number_of_output_channels(BLOCK, 1, 1)
        .map_err(describe)?;
    let gain = context.create_gain().map_err(describe)?;
    // Silent: the processor must reach the destination to run, and the microphone must not
    // come out of the speakers.
    gain.gain().set_value(0.0);

    let mut encoder = WebEncoder::new(rate, hub, shared)?;
    let mut mono: Vec<f32> = Vec::new();
    let callback = Closure::wrap(Box::new(move |event: AudioProcessingEvent| {
        let Ok(buffer) = event.input_buffer() else {
            return;
        };
        let channels = buffer.number_of_channels();
        mono.clear();
        for channel in 0..channels {
            let Ok(samples) = buffer.get_channel_data(channel) else {
                return;
            };
            if channel == 0 {
                mono.extend_from_slice(&samples);
            } else {
                for (sum, s) in mono.iter_mut().zip(samples.iter()) {
                    *sum += s;
                }
            }
        }
        if channels > 1 {
            for s in &mut mono {
                *s /= channels as f32;
            }
        }
        encoder.feed(&mono);
    }) as Box<dyn FnMut(AudioProcessingEvent)>);
    processor.set_onaudioprocess(Some(callback.as_ref().unchecked_ref()));
    source
        .connect_with_audio_node(&processor)
        .map_err(describe)?;
    processor.connect_with_audio_node(&gain).map_err(describe)?;
    gain.connect_with_audio_node(&context.destination())
        .map_err(describe)?;
    Ok(Parts {
        stream: Some(stream),
        source: Some(source),
        processor: Some(processor),
        gain: Some(gain),
        _callback: Some(callback),
    })
}

impl Session {
    fn close(&mut self) {
        let mut parts = self.parts.borrow_mut();
        if let Some(processor) = &parts.processor {
            processor.set_onaudioprocess(None);
            let _ = processor.disconnect();
        }
        if let Some(source) = &parts.source {
            let _ = source.disconnect();
        }
        if let Some(gain) = &parts.gain {
            let _ = gain.disconnect();
        }
        if let Some(stream) = &parts.stream {
            stop_stream(stream);
        }
        // Frees the audio thread the context holds; browsers cap how many a page may have.
        let _ = self.context.close();
        *parts = Parts::default();
        bevy::log::info!("bevy_iroh: microphone stopped");
    }
}

// -- playback ------------------------------------------------------------------------------

/// The page's output: one shared context, one stereo processor running the mixer.
pub struct Speaker;

struct Playback {
    processor: ScriptProcessorNode,
    gain: GainNode,
    callback: Option<Closure<dyn FnMut(AudioProcessingEvent)>>,
    stop: Arc<AtomicBool>,
}

fn output_context() -> Result<AudioContext, String> {
    OUTPUT.with_borrow_mut(|slot| {
        if let Some(context) = slot {
            return Ok(context.clone());
        }
        let context =
            AudioContext::new().map_err(|e| format!("cannot start audio: {}", describe(e)))?;
        *slot = Some(context.clone());
        Ok(context)
    })
}

impl AudioOutput for Speaker {
    fn start(self: Box<Self>, mixer: Arc<Mutex<Mixer>>) -> Result<Running, String> {
        let context = output_context()?;
        let rate = context.sample_rate() as u32;
        let processor = context
            .create_script_processor_with_buffer_size_and_number_of_input_channels_and_number_of_output_channels(BLOCK, 1, 2)
            .map_err(describe)?;
        let gain = context.create_gain().map_err(describe)?;
        gain.gain().set_value(1.0);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let mut interleaved: Vec<f32> = Vec::new();
        let mut left: Vec<f32> = Vec::new();
        let mut right: Vec<f32> = Vec::new();
        let callback = Closure::wrap(Box::new(move |event: AudioProcessingEvent| {
            let Ok(buffer) = event.output_buffer() else {
                return;
            };
            let frames = buffer.length() as usize;
            interleaved.clear();
            interleaved.resize(frames * 2, 0.0);
            if !flag.load(Ordering::Relaxed) {
                mixer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .render(&mut interleaved, 2, rate);
            }
            left.clear();
            right.clear();
            for pair in interleaved.chunks_exact(2) {
                left.push(pair[0]);
                right.push(pair[1]);
            }
            let channels = buffer.number_of_channels();
            let _ = buffer.copy_to_channel(&mut left, 0);
            if channels > 1 {
                let _ = buffer.copy_to_channel(&mut right, 1);
            }
        }) as Box<dyn FnMut(AudioProcessingEvent)>);
        processor.set_onaudioprocess(Some(callback.as_ref().unchecked_ref()));
        processor.connect_with_audio_node(&gain).map_err(describe)?;
        gain.connect_with_audio_node(&context.destination())
            .map_err(describe)?;
        bevy::log::info!("bevy_iroh: speaker at {rate} Hz");
        PLAYING.with_borrow_mut(|playing| {
            playing.push(Playback {
                processor,
                gain,
                callback: Some(callback),
                stop: stop.clone(),
            })
        });
        Ok(Running::flag(stop))
    }
}

/// Route the page's output to a device, where the browser allows it (Chrome does; Firefox
/// does not, and says so in the console).
pub(crate) fn set_sink(id: Option<&str>) {
    let Ok(context) = output_context() else {
        return;
    };
    let promise = context.set_sink_id_with_str(id.unwrap_or(""));
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = JsFuture::from(promise).await {
            bevy::log::warn!("bevy_iroh: speaker: {}", describe(e));
        }
    });
}

// -- decode --------------------------------------------------------------------------------

struct WebDecoder {
    inner: web_sys::AudioDecoder,
    out: Rc<RefCell<Vec<f32>>>,
    /// Packets handed in and not yet answered.
    pending: Rc<RefCell<usize>>,
    /// Samples fed, for timestamps.
    fed: u64,
    _callbacks: Callbacks,
}

impl WebDecoder {
    fn new() -> Result<Self, String> {
        let out: Rc<RefCell<Vec<f32>>> = Rc::new(RefCell::new(Vec::new()));
        let pending = Rc::new(RefCell::new(0usize));
        let (o, p) = (out.clone(), pending.clone());
        let on_output = Closure::wrap(Box::new(move |data: JsValue, _extra: JsValue| {
            let data: AudioData = data.unchecked_into();
            let frames = data.number_of_frames() as usize;
            let options = web_sys::AudioDataCopyToOptions::new(0);
            options.set_format(AudioSampleFormat::F32Planar);
            let mut bytes = vec![0u8; frames * 4];
            if data.copy_to_with_u8_slice(&mut bytes, &options).is_ok() {
                let mut out = o.borrow_mut();
                out.extend(
                    bytes
                        .chunks_exact(4)
                        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])),
                );
            }
            data.close();
            let mut p = p.borrow_mut();
            *p = p.saturating_sub(1);
        }) as Box<dyn FnMut(JsValue, JsValue)>);
        let on_error = Closure::wrap(Box::new(|error: JsValue| {
            bevy::log::error!("bevy_iroh: audio decoder: {}", describe(error));
        }) as Box<dyn FnMut(JsValue)>);
        let init = web_sys::AudioDecoderInit::new(
            on_error.as_ref().unchecked_ref(),
            on_output.as_ref().unchecked_ref(),
        );
        let inner = web_sys::AudioDecoder::new(&init).map_err(describe)?;
        let config = web_sys::AudioDecoderConfig::new(OPUS, 1, RATE);
        inner.configure(&config).map_err(describe)?;
        Ok(Self {
            inner,
            out,
            pending,
            fed: 0,
            _callbacks: Callbacks {
                _output: on_output,
                _error: on_error,
            },
        })
    }
}

impl VoiceDecoder for WebDecoder {
    fn push(&mut self, packet: Packet<'_>) {
        let bytes = match packet {
            Packet::Data(bytes) => bytes,
            // The browser's decoder has no way to take FEC or to conceal: a lost frame is
            // 20 ms of silence here.
            Packet::Fec(_) | Packet::Lost => {
                self.out
                    .borrow_mut()
                    .extend(std::iter::repeat_n(0.0, FRAME));
                return;
            }
        };
        let data = js_sys::Uint8Array::from(bytes);
        let init = EncodedAudioChunkInit::new(&data, 0, EncodedAudioChunkType::Key);
        init.set_timestamp_f64(self.fed as f64 * 1e6 / RATE as f64);
        self.fed += FRAME as u64;
        if let Ok(chunk) = EncodedAudioChunk::new(&init)
            && self.inner.decode(&chunk).is_ok()
        {
            *self.pending.borrow_mut() += 1;
        }
    }

    fn drain(&mut self, pcm: &mut VecDeque<f32>) -> usize {
        let mut out = self.out.borrow_mut();
        let n = out.len();
        pcm.extend(out.drain(..));
        n
    }

    fn in_flight(&self) -> usize {
        *self.pending.borrow() * FRAME
    }
}

impl Drop for WebDecoder {
    fn drop(&mut self) {
        let _ = self.inner.close();
    }
}

pub(crate) fn new_decoder() -> Option<Box<dyn VoiceDecoder>> {
    match WebDecoder::new() {
        Ok(d) => Some(Box::new(d)),
        Err(e) => {
            bevy::log::error!("bevy_iroh: opus: {e}");
            None
        }
    }
}

// -- sweep ---------------------------------------------------------------------------------

pub(crate) fn sweep() {
    SESSIONS.with_borrow_mut(|sessions| {
        sessions.retain_mut(|session| {
            if session.shared.stop.load(Ordering::Relaxed) {
                session.close();
                return false;
            }
            if session.context.state() == AudioContextState::Suspended {
                let _ = session.context.resume();
            }
            true
        });
    });
    PLAYING.with_borrow_mut(|playing| {
        playing.retain_mut(|p| {
            if !p.stop.load(Ordering::Relaxed) {
                return true;
            }
            p.processor.set_onaudioprocess(None);
            let _ = p.processor.disconnect();
            let _ = p.gain.disconnect();
            p.callback = None;
            false
        });
    });
    OUTPUT.with_borrow(|context| {
        if let Some(context) = context
            && context.state() == AudioContextState::Suspended
        {
            let _ = context.resume();
        }
    });
}
