//! Video: a source of frames, H.264 through openh264, one QUIC stream per group of pictures.
//!
//! Streams are independent, so a stalled group cannot hold up the next one, and a subscriber
//! that arrives mid-group is served from the next keyframe, which the encoder is asked for.
//! Decoded frames land in a newest-wins slot that the Bevy side copies into an `Image`.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use openh264::{
    decoder::Decoder,
    encoder::{BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, UsageType},
    formats::{RgbaSliceU8, YUVBuffer, YUVSlices, YUVSource},
};

use super::transport::MediaHub;

/// One frame from a [`VideoSource`].
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Pixels,
    /// Milliseconds on any monotonic clock of the source's choosing.
    pub timestamp_ms: u64,
}

pub enum Pixels {
    /// `width * height * 4` bytes.
    Rgba(Vec<u8>),
    /// Planar 4:2:0: a full-size luma plane and two quarter-size chroma planes.
    I420 { y: Vec<u8>, u: Vec<u8>, v: Vec<u8> },
}

/// Where frames come from. Polled by the encoder thread; hand back the newest frame since the
/// last call, or `None`.
pub trait VideoSource: Send + 'static {
    fn next_frame(&mut self) -> Option<VideoFrame>;
}

/// Encoder settings for one published feed.
#[derive(Debug, Clone)]
pub struct VideoConfig {
    pub bitrate_bps: u32,
    pub max_fps: f32,
    /// Frames between keyframes when nobody asked for one sooner.
    pub keyframe_interval: u32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            bitrate_bps: 1_500_000,
            max_fps: 30.0,
            keyframe_interval: 60,
        }
    }
}

/// An encoded frame with what a decoder needs to know about it.
#[derive(Debug, Clone)]
pub struct VideoPacket {
    pub group: u32,
    pub keyframe: bool,
    pub pts_ms: u64,
    pub data: Vec<u8>,
}

/// Runs on its own thread: polls the source, encodes, hands packets to the publish task.
pub(crate) fn run_encoder(
    mut source: Box<dyn VideoSource>,
    config: VideoConfig,
    keyframe_wanted: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    out: tokio::sync::mpsc::UnboundedSender<VideoPacket>,
) {
    let encoder_config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(config.bitrate_bps))
        .max_frame_rate(FrameRate::from_hz(config.max_fps))
        .usage_type(UsageType::CameraVideoRealTime)
        .intra_frame_period(IntraFramePeriod::from_num_frames(config.keyframe_interval))
        .skip_frames(true);
    let mut encoder =
        match Encoder::with_api_config(openh264::OpenH264API::from_source(), encoder_config) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("bevy_iroh: h264 encoder: {e}");
                return;
            }
        };
    let min_gap = Duration::from_secs_f32(1.0 / config.max_fps.max(1.0));
    let mut last = Instant::now() - min_gap;
    let mut group: u32 = 0;
    let mut yuv: Option<YUVBuffer> = None;
    while !stop.load(Ordering::Relaxed) {
        let since = last.elapsed();
        if since < min_gap {
            std::thread::sleep(min_gap - since);
        }
        let Some(frame) = source.next_frame() else {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        };
        last = Instant::now();
        let (w, h) = (frame.width as usize, frame.height as usize);
        if w < 16 || h < 16 || w % 2 != 0 || h % 2 != 0 {
            tracing::warn!("bevy_iroh: video frames must be even-sized and at least 16x16");
            continue;
        }
        if keyframe_wanted.swap(false, Ordering::Relaxed) {
            encoder.force_intra_frame();
        }
        let pts = openh264::Timestamp::from_millis(frame.timestamp_ms);
        let encoded = match &frame.pixels {
            Pixels::Rgba(data) => {
                if data.len() < w * h * 4 {
                    continue;
                }
                let buffer = yuv.get_or_insert_with(|| YUVBuffer::new(w, h));
                if buffer.dimensions() != (w, h) {
                    *buffer = YUVBuffer::new(w, h);
                }
                buffer.read_rgba8(RgbaSliceU8::new(&data[..w * h * 4], (w, h)));
                encoder.encode_at(buffer, pts)
            }
            Pixels::I420 { y, u, v } => {
                let (cw, ch) = (w / 2, h / 2);
                if y.len() < w * h || u.len() < cw * ch || v.len() < cw * ch {
                    continue;
                }
                let slices = YUVSlices::new(
                    (&y[..w * h], &u[..cw * ch], &v[..cw * ch]),
                    (w, h),
                    (w, cw, cw),
                );
                encoder.encode_at(&slices, pts)
            }
        };
        let bitstream = match encoded {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!("bevy_iroh: h264 encode: {e}");
                continue;
            }
        };
        let keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        let data = bitstream.to_vec();
        if data.is_empty() {
            continue;
        }
        if keyframe {
            group = group.wrapping_add(1);
            tracing::debug!(
                "bevy_iroh: video keyframe, group {group}, {} bytes",
                data.len()
            );
        }
        if out
            .send(VideoPacket {
                group,
                keyframe,
                pts_ms: frame.timestamp_ms,
                data,
            })
            .is_err()
        {
            return;
        }
    }
}

/// Runs on the network runtime: takes encoded packets and writes each group to every
/// subscriber on a stream of its own.
pub(crate) async fn run_publisher(
    hub: Arc<MediaHub>,
    track: u64,
    mut packets: tokio::sync::mpsc::UnboundedReceiver<VideoPacket>,
) {
    use std::collections::HashMap;
    let mut streams: HashMap<usize, iroh::endpoint::SendStream> = HashMap::new();
    while let Some(packet) = packets.recv().await {
        if packet.keyframe {
            // A new group: finish the old streams and open one per subscriber.
            for (_, mut stream) in streams.drain() {
                let _ = stream.finish();
            }
            let subscribers = hub.subscribers_of(track);
            tracing::debug!(
                "bevy_iroh: video group {} to {} subscriber(s)",
                packet.group,
                subscribers.len()
            );
            for conn in subscribers {
                match MediaHub::open_group(&conn, track, packet.group).await {
                    Ok(stream) => {
                        streams.insert(conn.stable_id(), stream);
                    }
                    Err(e) => tracing::debug!("bevy_iroh: open video group: {e:#}"),
                }
            }
        }
        let mut gone = Vec::new();
        for (id, stream) in streams.iter_mut() {
            if MediaHub::write_video_frame(stream, &packet).await.is_err() {
                gone.push(*id);
            }
        }
        for id in gone {
            streams.remove(&id);
        }
    }
    for (_, mut stream) in streams.drain() {
        let _ = stream.finish();
    }
}

/// A decoded frame, RGBA.
#[derive(Clone)]
pub struct RgbaFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub pts_ms: u64,
}

/// A remote video track: packets go to a decoder thread, the newest decoded frame waits here.
pub struct RemoteVideo {
    packets: Mutex<Option<std::sync::mpsc::Sender<VideoPacket>>>,
    latest: Mutex<Option<RgbaFrame>>,
    stop: Arc<AtomicBool>,
    frames: std::sync::atomic::AtomicU64,
}

impl RemoteVideo {
    pub(crate) fn new() -> Self {
        Self {
            packets: Mutex::new(None),
            latest: Mutex::new(None),
            stop: Arc::new(AtomicBool::new(false)),
            frames: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub(crate) fn push(self: &Arc<Self>, packet: VideoPacket) {
        let mut slot = self.packets.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            let me = self.clone();
            if std::thread::Builder::new()
                .name("bevy_iroh-decode".into())
                .spawn(move || me.run_decoder(rx))
                .is_ok()
            {
                *slot = Some(tx);
            }
        }
        if let Some(tx) = slot.as_ref()
            && tx.send(packet).is_err()
        {
            *slot = None;
        }
    }

    /// The newest decoded frame, taken. `None` until the first keyframe decodes, and between
    /// frames.
    pub fn take_frame(&self) -> Option<RgbaFrame> {
        self.latest.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Frames decoded so far.
    pub fn decoded(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    fn run_decoder(self: Arc<Self>, rx: std::sync::mpsc::Receiver<VideoPacket>) {
        let mut decoder = match Decoder::new() {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("bevy_iroh: h264 decoder: {e}");
                return;
            }
        };
        let mut current_group: Option<u32> = None;
        let mut rgba = Vec::new();
        while !self.stop.load(Ordering::Relaxed) {
            let Ok(packet) = rx.recv_timeout(Duration::from_millis(200)) else {
                if Arc::strong_count(&self) == 1 {
                    return;
                }
                continue;
            };
            // Groups can interleave at a boundary; once a newer keyframe has been seen, the
            // tail of the old group is stale.
            match current_group {
                Some(g) if packet.group.wrapping_sub(g) > u32::MAX / 2 => continue,
                Some(g) if packet.group != g && !packet.keyframe => continue,
                _ => {}
            }
            if packet.keyframe {
                current_group = Some(packet.group);
            }
            let decoded = match decoder.decode(&packet.data) {
                Ok(Some(frame)) => frame,
                Ok(None) => continue,
                Err(e) => {
                    tracing::debug!("bevy_iroh: h264 decode: {e}");
                    continue;
                }
            };
            let (w, h) = decoded.dimensions();
            rgba.resize(w * h * 4, 0);
            decoded.write_rgba8(&mut rgba);
            if self.frames.fetch_add(1, Ordering::Relaxed) == 0 {
                tracing::info!("bevy_iroh: seeing a {w}x{h} picture");
            }
            *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(RgbaFrame {
                width: w as u32,
                height: h as u32,
                data: rgba.clone(),
                pts_ms: packet.pts_ms,
            });
        }
    }
}

impl Drop for RemoteVideo {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A moving test pattern: colour bars sliding at `fps`. For examples and tests, and for a
/// conference client with no camera.
pub struct TestPattern {
    width: u32,
    height: u32,
    fps: f32,
    started: Instant,
    last_frame: Option<u64>,
}

impl TestPattern {
    pub fn new(width: u32, height: u32, fps: f32) -> Self {
        Self {
            width,
            height,
            fps,
            started: Instant::now(),
            last_frame: None,
        }
    }
}

impl VideoSource for TestPattern {
    fn next_frame(&mut self) -> Option<VideoFrame> {
        let elapsed = self.started.elapsed();
        let index = (elapsed.as_secs_f32() * self.fps) as u64;
        if self.last_frame == Some(index) {
            return None;
        }
        self.last_frame = Some(index);
        let (w, h) = (self.width as usize, self.height as usize);
        let shift = (index as usize * 4) % w;
        let mut data = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let bar = ((x + shift) * 8 / w) % 8;
                let (r, g, b) = match bar {
                    0 => (235, 235, 235),
                    1 => (235, 235, 16),
                    2 => (16, 235, 235),
                    3 => (16, 235, 16),
                    4 => (235, 16, 235),
                    5 => (235, 16, 16),
                    6 => (16, 16, 235),
                    _ => (16, 16, 16),
                };
                let shade = 1.0 - (y as f32 / h as f32) * 0.4;
                let i = (y * w + x) * 4;
                data[i] = (r as f32 * shade) as u8;
                data[i + 1] = (g as f32 * shade) as u8;
                data[i + 2] = (b as f32 * shade) as u8;
                data[i + 3] = 255;
            }
        }
        Some(VideoFrame {
            width: self.width,
            height: self.height,
            pixels: Pixels::Rgba(data),
            timestamp_ms: elapsed.as_millis() as u64,
        })
    }
}
