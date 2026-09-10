//! Cameras and H.264 in a browser: `getUserMedia` in, WebCodecs both ways.
//!
//! The camera is a hidden `<video>` element playing the stream and a canvas it is drawn to
//! once per frame, so the encoder and the preview get the same RGBA bytes. The encoder is
//! asked for Annex B, which is what openh264 on a desktop sends and expects, so a browser
//! and a desktop decode each other without a catalog carrying an `avcC` blob.
//!
//! WebCodecs never answers from the call that asked: every encoded chunk and decoded frame
//! arrives on a later task, and the wrappers here turn that into the shapes the shared
//! pipeline expects.

use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    CanvasRenderingContext2d, EncodedVideoChunk, EncodedVideoChunkInit, EncodedVideoChunkType,
    HtmlCanvasElement, HtmlVideoElement, MediaStream, MediaStreamConstraints, VideoFrame,
    VideoFrameBufferInit, VideoPixelFormat,
};

use super::{describe, exactly, media_devices, now_ms, stop_stream};
use crate::media::video::{
    EncoderShared, MAX_BEHIND_MS, Pixels, RemoteVideo, RgbaFrame, VideoConfig, VideoFrame as Frame,
    VideoPacket, VideoSource,
};

/// The H.264 codec string for a picture of this size: Constrained Baseline, which every
/// hardware decoder implements and openh264 produces, at the lowest level that admits the
/// area. The level is not cosmetic: WebCodecs closes an encoder whose picture exceeds it.
fn h264_codec(width: u32, height: u32) -> String {
    // (level byte, MaxFS in macroblocks) from ITU-T H.264 Table A-1.
    const LEVELS: [(u8, u32); 8] = [
        (0x1f, 3600),
        (0x20, 5120),
        (0x28, 8192),
        (0x2a, 8704),
        (0x32, 22080),
        (0x33, 36864),
        (0x35, 139264),
        (0x3e, 139264),
    ];
    let macroblocks = width.div_ceil(16) * height.div_ceil(16);
    let level = LEVELS
        .iter()
        .find(|(_, max)| macroblocks <= *max)
        .map(|(level, _)| *level)
        .unwrap_or(0x3e);
    format!("avc1.42e0{level:02x}")
}

struct Callbacks {
    _output: Closure<dyn FnMut(JsValue, JsValue)>,
    _error: Closure<dyn FnMut(JsValue)>,
}

// -- camera --------------------------------------------------------------------------------

struct CameraSession {
    video: HtmlVideoElement,
    canvas: HtmlCanvasElement,
    context: CanvasRenderingContext2d,
    stream: Rc<RefCell<Option<MediaStream>>>,
    last_time: f64,
    /// The person said no, or the browser could not: nothing will ever arrive.
    failed: bool,
}

thread_local! {
    static CAMERAS: RefCell<HashMap<u64, CameraSession>> = RefCell::new(HashMap::new());
    static NEXT_CAMERA: RefCell<u64> = const { RefCell::new(1) };
    static DECODERS: RefCell<HashMap<u64, Decoder>> = RefCell::new(HashMap::new());
}

/// A camera from `getUserMedia`, or a screen from `getDisplayMedia`, as a video source.
/// Frames arrive once the person has said yes; until then `next_frame` is `None`. When the
/// track ends (the browser's own "Stop sharing", a camera unplugged) `ended` is true.
pub struct Camera {
    slot: u64,
}

/// What to ask the browser for.
enum Ask {
    Camera {
        id: Option<String>,
        width: u32,
        height: u32,
        fps: f32,
    },
    Screen,
}

impl Camera {
    pub fn open(id: Option<&str>, width: u32, height: u32, fps: f32) -> Result<Self, String> {
        Self::start(Ask::Camera {
            id: id.map(str::to_string),
            width,
            height,
            fps,
        })
    }

    /// The browser's own picker for a screen, window or tab. Must run soon after a click:
    /// the call needs the page's transient activation.
    pub fn open_screen() -> Result<Self, String> {
        Self::start(Ask::Screen)
    }

    fn start(ask: Ask) -> Result<Self, String> {
        let document = web_sys::window()
            .and_then(|w| w.document())
            .ok_or("no document")?;
        let video: HtmlVideoElement = document
            .create_element("video")
            .map_err(describe)?
            .dyn_into()
            .map_err(|_| "not a video element")?;
        video.set_autoplay(true);
        video.set_muted(true);
        let _ = video.set_attribute("playsinline", "");
        let canvas: HtmlCanvasElement = document
            .create_element("canvas")
            .map_err(describe)?
            .dyn_into()
            .map_err(|_| "not a canvas")?;
        let options = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&options, &"willReadFrequently".into(), &JsValue::TRUE);
        let context: CanvasRenderingContext2d = canvas
            .get_context_with_context_options("2d", &options)
            .map_err(describe)?
            .ok_or("no 2d context")?
            .dyn_into()
            .map_err(|_| "not a 2d context")?;
        let slot = NEXT_CAMERA.with_borrow_mut(|n| {
            let s = *n;
            *n += 1;
            s
        });
        let stream = Rc::new(RefCell::new(None));
        CAMERAS.with_borrow_mut(|cameras| {
            cameras.insert(
                slot,
                CameraSession {
                    video: video.clone(),
                    canvas,
                    context,
                    stream: stream.clone(),
                    last_time: -1.0,
                    failed: false,
                },
            )
        });
        wasm_bindgen_futures::spawn_local(async move {
            let (what, got) = match ask {
                Ask::Camera {
                    id,
                    width,
                    height,
                    fps,
                } => ("camera", acquire(id.as_deref(), width, height, fps).await),
                Ask::Screen => ("screen", acquire_screen().await),
            };
            match got {
                Ok(s) => {
                    video.set_src_object(Some(&s));
                    let _ = video.play();
                    *stream.borrow_mut() = Some(s);
                    bevy::log::info!("bevy_iroh: {what} open");
                }
                Err(e) => {
                    bevy::log::error!("bevy_iroh: {what}: {e}");
                    CAMERAS.with_borrow_mut(|cameras| {
                        if let Some(session) = cameras.get_mut(&slot) {
                            session.failed = true;
                        }
                    });
                }
            }
        });
        Ok(Self { slot })
    }
}

async fn acquire_screen() -> Result<MediaStream, String> {
    let devices = media_devices()?;
    let video = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&video, &"cursor".into(), &"always".into());
    let constraints = web_sys::DisplayMediaStreamConstraints::new();
    constraints.set_video(&video);
    constraints.set_audio(&JsValue::FALSE);
    let promise = devices
        .get_display_media_with_constraints(&constraints)
        .map_err(describe)?;
    Ok(JsFuture::from(promise)
        .await
        .map_err(describe)?
        .unchecked_into())
}

async fn acquire(
    id: Option<&str>,
    width: u32,
    height: u32,
    fps: f32,
) -> Result<MediaStream, String> {
    let devices = media_devices()?;
    let video = match id {
        Some(_) => exactly(id)?,
        None => js_sys::Object::new().into(),
    };
    let ideal = |v: f64| -> JsValue {
        let o = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&o, &"ideal".into(), &v.into());
        o.into()
    };
    let _ = js_sys::Reflect::set(&video, &"width".into(), &ideal(width as f64));
    let _ = js_sys::Reflect::set(&video, &"height".into(), &ideal(height as f64));
    let _ = js_sys::Reflect::set(&video, &"frameRate".into(), &ideal(fps as f64));
    let constraints = MediaStreamConstraints::new();
    constraints.set_video(&video);
    constraints.set_audio(&JsValue::FALSE);
    let promise = devices
        .get_user_media_with_constraints(&constraints)
        .map_err(describe)?;
    Ok(JsFuture::from(promise)
        .await
        .map_err(describe)?
        .unchecked_into())
}

impl VideoSource for Camera {
    fn ended(&self) -> bool {
        self.track_ended()
    }

    fn next_frame(&mut self) -> Option<Frame> {
        CAMERAS.with_borrow_mut(|cameras| {
            let session = cameras.get_mut(&self.slot)?;
            // HAVE_CURRENT_DATA or better, and a frame we have not drawn yet.
            if session.video.ready_state() < 2 {
                return None;
            }
            let time = session.video.current_time();
            if time == session.last_time {
                return None;
            }
            session.last_time = time;
            let (w, h) = (session.video.video_width(), session.video.video_height());
            if w < 16 || h < 16 {
                return None;
            }
            // Even sizes: what an H.264 encoder wants.
            let (w, h) = (w & !1, h & !1);
            if session.canvas.width() != w || session.canvas.height() != h {
                session.canvas.set_width(w);
                session.canvas.set_height(h);
            }
            session
                .context
                .draw_image_with_html_video_element_and_dw_and_dh(
                    &session.video,
                    0.0,
                    0.0,
                    w as f64,
                    h as f64,
                )
                .ok()?;
            let data = session
                .context
                .get_image_data(0, 0, w as i32, h as i32)
                .ok()?
                .data();
            Some(Frame {
                width: w,
                height: h,
                pixels: Pixels::Rgba(data.0),
                timestamp_ms: now_ms(),
            })
        })
    }
}

impl Camera {
    fn track_ended(&self) -> bool {
        CAMERAS.with_borrow(|cameras| {
            let Some(session) = cameras.get(&self.slot) else {
                return true;
            };
            if session.failed {
                return true;
            }
            let stream = session.stream.borrow();
            let Some(stream) = stream.as_ref() else {
                return false;
            };
            let tracks = stream.get_video_tracks();
            if tracks.length() == 0 {
                return true;
            }
            let track: web_sys::MediaStreamTrack = tracks.get(0).unchecked_into();
            track.ready_state() == web_sys::MediaStreamTrackState::Ended
        })
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        CAMERAS.with_borrow_mut(|cameras| {
            if let Some(session) = cameras.remove(&self.slot) {
                session.video.set_src_object(None);
                if let Some(stream) = session.stream.borrow().as_ref() {
                    stop_stream(stream);
                }
                bevy::log::info!("bevy_iroh: camera closed");
            }
        });
    }
}

// -- encode --------------------------------------------------------------------------------

struct WebEncoder {
    inner: web_sys::VideoEncoder,
    size: (u32, u32),
    _callbacks: Callbacks,
}

impl WebEncoder {
    fn new(
        width: u32,
        height: u32,
        config: &VideoConfig,
        out: tokio::sync::mpsc::Sender<Arc<VideoPacket>>,
        keyframe_wanted: Arc<AtomicBool>,
        shared: Arc<EncoderShared>,
    ) -> Result<Self, String> {
        let group = Rc::new(RefCell::new(0u32));
        let on_output = Closure::wrap(Box::new(move |chunk: JsValue, _meta: JsValue| {
            let chunk: EncodedVideoChunk = chunk.unchecked_into();
            let mut data = vec![0u8; chunk.byte_length() as usize];
            if chunk.copy_to_with_u8_slice(&mut data).is_err() {
                return;
            }
            let keyframe = chunk.type_() == EncodedVideoChunkType::Key;
            let mut g = group.borrow_mut();
            if keyframe {
                *g = g.wrapping_add(1);
            }
            shared.encoded.fetch_add(1, Ordering::Relaxed);
            let packet = Arc::new(VideoPacket {
                group: *g,
                keyframe,
                pts_ms: (chunk.timestamp() / 1000.0) as u64,
                data,
            });
            if let Err(tokio::sync::mpsc::error::TrySendError::Full(_)) = out.try_send(packet) {
                shared.dropped.fetch_add(1, Ordering::Relaxed);
                keyframe_wanted.store(true, Ordering::Relaxed);
            }
        }) as Box<dyn FnMut(JsValue, JsValue)>);
        let on_error = Closure::wrap(Box::new(|error: JsValue| {
            bevy::log::error!("bevy_iroh: video encoder: {}", describe(error));
        }) as Box<dyn FnMut(JsValue)>);
        let init = web_sys::VideoEncoderInit::new(
            on_error.as_ref().unchecked_ref(),
            on_output.as_ref().unchecked_ref(),
        );
        let inner = web_sys::VideoEncoder::new(&init).map_err(describe)?;
        let cfg = web_sys::VideoEncoderConfig::new(&h264_codec(width, height), height, width);
        cfg.set_bitrate(config.bitrate_for(width, height));
        cfg.set_framerate(config.max_fps as f64);
        cfg.set_latency_mode(web_sys::LatencyMode::Realtime);
        let avc = js_sys::Object::new();
        js_sys::Reflect::set(&avc, &"format".into(), &"annexb".into()).map_err(describe)?;
        js_sys::Reflect::set(&cfg, &"avc".into(), &avc).map_err(describe)?;
        inner.configure(&cfg).map_err(describe)?;
        bevy::log::info!(
            "bevy_iroh: encoding {width}x{height} video at {} kbps",
            config.bitrate_for(width, height) / 1000
        );
        Ok(Self {
            inner,
            size: (width, height),
            _callbacks: Callbacks {
                _output: on_output,
                _error: on_error,
            },
        })
    }

    fn encode(&self, frame: &Frame, keyframe: bool) -> Result<(), String> {
        let (w, h) = (frame.width, frame.height);
        let (format, bytes): (VideoPixelFormat, Vec<u8>) = match &frame.pixels {
            Pixels::Rgba(d) => (VideoPixelFormat::Rgba, d.clone()),
            Pixels::Bgra(d) => (VideoPixelFormat::Bgra, d.clone()),
            Pixels::I420 { y, u, v } => {
                let mut all = Vec::with_capacity(y.len() + u.len() + v.len());
                all.extend_from_slice(y);
                all.extend_from_slice(u);
                all.extend_from_slice(v);
                (VideoPixelFormat::I420, all)
            }
        };
        let mut bytes = bytes;
        let init =
            VideoFrameBufferInit::new_with_f64(h, w, format, frame.timestamp_ms as f64 * 1000.0);
        let vf = VideoFrame::new_with_u8_slice_and_video_frame_buffer_init(&mut bytes, &init)
            .map_err(describe)?;
        let options = web_sys::VideoEncoderEncodeOptions::new();
        options.set_key_frame(keyframe);
        let result = self.inner.encode_with_options(&vf, &options);
        // A `VideoFrame` holds a buffer the browser will not reclaim on GC alone.
        vf.close();
        result.map_err(describe)
    }
}

impl Drop for WebEncoder {
    fn drop(&mut self) {
        let _ = self.inner.close();
    }
}

/// Polls the source on a timer, encodes, hands packets to the publisher.
pub(crate) fn start_encoder(
    mut source: Box<dyn VideoSource>,
    config: VideoConfig,
    keyframe_wanted: Arc<AtomicBool>,
    shared: Arc<EncoderShared>,
    out: tokio::sync::mpsc::Sender<Arc<VideoPacket>>,
) {
    wasm_bindgen_futures::spawn_local(async move {
        let interval = std::time::Duration::from_secs_f32(1.0 / config.max_fps.max(1.0));
        let mut encoder: Option<WebEncoder> = None;
        while !shared.stop.load(Ordering::Relaxed) {
            let started = web_time::Instant::now();
            if source.ended() {
                shared.finished.store(true, Ordering::Relaxed);
                break;
            }
            if let Some(frame) = source.next_frame() {
                shared.saw(&frame);
                let size = (frame.width, frame.height);
                if encoder.as_ref().is_some_and(|e| e.size != size) {
                    encoder = None;
                }
                if encoder.is_none() {
                    match WebEncoder::new(
                        size.0,
                        size.1,
                        &config,
                        out.clone(),
                        keyframe_wanted.clone(),
                        shared.clone(),
                    ) {
                        Ok(e) => encoder = Some(e),
                        Err(e) => {
                            bevy::log::error!("bevy_iroh: h264: {e}");
                            return;
                        }
                    }
                }
                let key = keyframe_wanted.swap(false, Ordering::Relaxed);
                if let Some(enc) = &encoder
                    && let Err(e) = enc.encode(&frame, key)
                {
                    bevy::log::debug!("bevy_iroh: h264 encode: {e}");
                    // A closed codec stays closed: build a fresh one next frame.
                    encoder = None;
                    keyframe_wanted.store(true, Ordering::Relaxed);
                }
            }
            let took = started.elapsed();
            let wait = interval
                .saturating_sub(took)
                .max(std::time::Duration::from_millis(2));
            n0_future::time::sleep(wait).await;
        }
    });
}

// -- decode --------------------------------------------------------------------------------

struct Decoder {
    inner: web_sys::VideoDecoder,
    remote: Weak<RemoteVideo>,
    /// Waiting for a keyframe to start or restart on.
    waiting: bool,
    group: Option<u32>,
    /// Oldest pts still in the decoder's queue, for the latency bound.
    oldest_pts: Option<u64>,
    broken: Rc<RefCell<bool>>,
    _callbacks: Callbacks,
}

/// Frames the browser's decoder may hold before the rest of the group is skipped.
const DECODE_QUEUE: u32 = 8;

fn new_decoder(remote: &Arc<RemoteVideo>) -> Result<Decoder, String> {
    let weak = Arc::downgrade(remote);
    let out_remote = weak.clone();
    let on_output = Closure::wrap(Box::new(move |frame: JsValue, _extra: JsValue| {
        let frame: VideoFrame = frame.unchecked_into();
        // The display size, not the coded one: H.264 codes in 16x16 macroblocks and the
        // padding is not picture.
        let (width, height) = (frame.display_width(), frame.display_height());
        let options = web_sys::VideoFrameCopyToOptions::new();
        options.set_format(VideoPixelFormat::Rgba);
        if let Some(visible) = frame.visible_rect() {
            let rect = web_sys::DomRectInit::new();
            rect.set_x(visible.x());
            rect.set_y(visible.y());
            rect.set_width(visible.width());
            rect.set_height(visible.height());
            options.set_rect(&rect);
        }
        let size = frame
            .allocation_size_with_options(&options)
            .unwrap_or(width * height * 4);
        let pts_ms = (frame.timestamp() / 1000.0) as u64;
        let remote = out_remote.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let mut rgba = vec![0u8; size as usize];
            let copy = frame.copy_to_with_u8_slice_and_options(&mut rgba, &options);
            let done = JsFuture::from(copy).await.is_ok();
            frame.close();
            if done && let Some(remote) = remote.upgrade() {
                rgba.truncate((width * height * 4) as usize);
                remote.deliver(RgbaFrame {
                    width,
                    height,
                    data: rgba,
                    pts_ms,
                });
            }
        });
    }) as Box<dyn FnMut(JsValue, JsValue)>);
    let broken = Rc::new(RefCell::new(false));
    let flag = broken.clone();
    let on_error = Closure::wrap(Box::new(move |error: JsValue| {
        bevy::log::warn!("bevy_iroh: video decoder: {}", describe(error));
        *flag.borrow_mut() = true;
    }) as Box<dyn FnMut(JsValue)>);
    let init = web_sys::VideoDecoderInit::new(
        on_error.as_ref().unchecked_ref(),
        on_output.as_ref().unchecked_ref(),
    );
    let inner = web_sys::VideoDecoder::new(&init).map_err(describe)?;
    // Level 5.1 admits anything up to 4K; a decoder's string is an upper bound, not a
    // promise. No description: the stream is Annex B with SPS and PPS in band.
    let config = web_sys::VideoDecoderConfig::new("avc1.42e033");
    config.set_optimize_for_latency(true);
    inner.configure(&config).map_err(describe)?;
    Ok(Decoder {
        inner,
        remote: weak,
        waiting: true,
        group: None,
        oldest_pts: None,
        broken,
        _callbacks: Callbacks {
            _output: on_output,
            _error: on_error,
        },
    })
}

/// One packet for `remote`, from the network task.
pub(crate) fn decode(remote: &Arc<RemoteVideo>, packet: VideoPacket) {
    DECODERS.with_borrow_mut(|decoders| {
        let track = remote.track;
        if decoders
            .get(&track)
            .is_some_and(|d| *d.broken.borrow() || d.remote.upgrade().is_none())
        {
            decoders.remove(&track);
        }
        let decoder = match decoders.get_mut(&track) {
            Some(d) => d,
            None => match new_decoder(remote) {
                Ok(d) => {
                    decoders.insert(track, d);
                    decoders.get_mut(&track).expect("just inserted")
                }
                Err(e) => {
                    bevy::log::error!("bevy_iroh: h264 decoder: {e}");
                    return;
                }
            },
        };
        // Stale groups, and a group's tail once a newer one has started.
        match decoder.group {
            Some(g) if packet.group.wrapping_sub(g) > u32::MAX / 2 => return,
            Some(g) if packet.group != g && !packet.keyframe => return,
            _ => {}
        }
        if packet.keyframe {
            decoder.group = Some(packet.group);
            decoder.waiting = false;
            decoder.oldest_pts = None;
        } else if decoder.waiting {
            remote.skipped(1);
            return;
        }
        // Behind by more than a beat: give up on this group and wait for the next keyframe,
        // which the owner is asked for.
        let queued = decoder.inner.decode_queue_size();
        if queued == 0 {
            decoder.oldest_pts = Some(packet.pts_ms);
        }
        let behind = decoder
            .oldest_pts
            .map(|o| packet.pts_ms.saturating_sub(o))
            .unwrap_or(0);
        if !packet.keyframe && (queued > DECODE_QUEUE || behind > MAX_BEHIND_MS) {
            decoder.waiting = true;
            remote.skipped(1);
            return;
        }
        let kind = if packet.keyframe {
            EncodedVideoChunkType::Key
        } else {
            EncodedVideoChunkType::Delta
        };
        let data = js_sys::Uint8Array::from(packet.data.as_slice());
        let init = EncodedVideoChunkInit::new(&data, 0, kind);
        init.set_timestamp_f64(packet.pts_ms as f64 * 1000.0);
        if let Ok(chunk) = EncodedVideoChunk::new(&init)
            && let Err(e) = decoder.inner.decode(&chunk)
        {
            bevy::log::debug!("bevy_iroh: h264 decode: {}", describe(e));
            *decoder.broken.borrow_mut() = true;
        }
    });
}

pub(crate) fn sweep() {
    DECODERS.with_borrow_mut(|decoders| {
        decoders.retain(|_, d| d.remote.upgrade().is_some());
    });
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.inner.close();
    }
}
