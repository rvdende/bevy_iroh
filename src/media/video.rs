//! Video: a source of frames, H.264 through openh264, one QUIC stream per group of pictures.
//!
//! Streams are independent, so a stalled group cannot hold up the next one, and a subscriber
//! that arrives mid-group is served from the next keyframe, which the encoder is asked for.
//! Each subscriber is fed by a task of its own from a short queue: a peer on a slow link
//! falls behind on its own stream and skips to the next keyframe, and nobody else notices.
//! Decoded frames land in a newest-wins slot that the Bevy side copies into an `Image`.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use bevy::platform::time::Instant;

use super::transport::{Link, MediaHub, Outbound};

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
    /// `width * height * 4` bytes, blue first: what most capture APIs and swapchains hand out.
    Bgra(Vec<u8>),
    /// Planar 4:2:0: a full-size luma plane and two quarter-size chroma planes.
    I420 { y: Vec<u8>, u: Vec<u8>, v: Vec<u8> },
}

/// Where frames come from. Polled by the encoder; hand back the newest frame since the last
/// call, or `None`.
pub trait VideoSource: Send + 'static {
    fn next_frame(&mut self) -> Option<VideoFrame>;
}

/// Encoder settings for one published feed.
#[derive(Debug, Clone)]
pub struct VideoConfig {
    /// `None` picks a rate for the picture's size: about 0.07 bits per pixel per frame, which
    /// is 650 kbps for 640x480 at 30 fps and 2 Mbps for 720p.
    pub bitrate_bps: Option<u32>,
    pub max_fps: f32,
    /// Frames between keyframes when nobody asked for one sooner.
    pub keyframe_interval: u32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            bitrate_bps: None,
            max_fps: 30.0,
            keyframe_interval: 60,
        }
    }
}

impl VideoConfig {
    pub fn bitrate_for(&self, width: u32, height: u32) -> u32 {
        self.bitrate_bps
            .unwrap_or_else(|| {
                ((width * height) as f32 * 0.07 * self.max_fps.clamp(1.0, 60.0)) as u32
            })
            .max(100_000)
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

/// What a video track has been through.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoTrackStats {
    /// Frames that arrived.
    pub received: u64,
    /// Frames decoded into a picture.
    pub decoded: u64,
    /// Frames skipped to catch up with a newer group.
    pub skipped: u64,
    /// Frames thrown away because the decoder was behind.
    pub dropped: u64,
}

/// What a published feed has been through.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FeedStats {
    /// Frames encoded.
    pub encoded: u64,
    /// Frames the network side could not take in time. Each costs a keyframe.
    pub dropped: u64,
}

/// What an encoder shares with the Bevy side.
pub(crate) struct EncoderShared {
    pub stop: AtomicBool,
    /// The picture's size as `width << 32 | height`, once known.
    pub size: AtomicU64,
    pub encoded: AtomicU64,
    pub dropped: AtomicU64,
    /// The newest frame handed to the encoder, as a picture, when a preview is wanted.
    pub preview: Option<Mutex<Option<RgbaFrame>>>,
}

impl EncoderShared {
    pub fn new(preview: bool) -> Self {
        Self {
            stop: AtomicBool::new(false),
            size: AtomicU64::new(0),
            encoded: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            preview: preview.then(|| Mutex::new(None)),
        }
    }

    pub fn size(&self) -> Option<(u32, u32)> {
        let packed = self.size.load(Ordering::Relaxed);
        (packed != 0).then_some(((packed >> 32) as u32, packed as u32))
    }

    pub fn stats(&self) -> FeedStats {
        FeedStats {
            encoded: self.encoded.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn saw(&self, frame: &VideoFrame) {
        self.size.store(
            ((frame.width as u64) << 32) | frame.height as u64,
            Ordering::Relaxed,
        );
        if let Some(slot) = &self.preview {
            let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
            let spare = slot.take().map(|f| f.data).unwrap_or_default();
            *slot = Some(frame.to_rgba(spare));
        }
    }

    pub fn take_preview(&self) -> Option<RgbaFrame> {
        self.preview
            .as_ref()?
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

impl VideoFrame {
    /// The frame as RGBA, into `into` if it is the right size.
    pub fn to_rgba(&self, mut into: Vec<u8>) -> RgbaFrame {
        let (w, h) = (self.width as usize, self.height as usize);
        into.resize(w * h * 4, 0);
        match &self.pixels {
            Pixels::Rgba(data) => into.copy_from_slice(&data[..(w * h * 4).min(data.len())]),
            Pixels::Bgra(data) => {
                for (out, px) in into
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(data.as_chunks::<4>().0.iter())
                {
                    out[0] = px[2];
                    out[1] = px[1];
                    out[2] = px[0];
                    out[3] = 255;
                }
            }
            Pixels::I420 { y, u, v } => i420_to_rgba(y, u, v, w, h, &mut into),
        }
        RgbaFrame {
            width: self.width,
            height: self.height,
            data: into,
            pts_ms: self.timestamp_ms,
        }
    }
}

/// BT.601 limited range, which is what cameras hand out.
fn i420_to_rgba(y: &[u8], u: &[u8], v: &[u8], w: usize, h: usize, out: &mut [u8]) {
    let cw = w / 2;
    for row in 0..h {
        let crow = row / 2;
        for col in 0..w {
            let i = row * w + col;
            let c = crow * cw + col / 2;
            let (Some(&yy), Some(&uu), Some(&vv)) = (y.get(i), u.get(c), v.get(c)) else {
                return;
            };
            let yy = (yy as f32 - 16.0) * 1.164;
            let uu = uu as f32 - 128.0;
            let vv = vv as f32 - 128.0;
            let o = i * 4;
            out[o] = (yy + 1.596 * vv).clamp(0.0, 255.0) as u8;
            out[o + 1] = (yy - 0.392 * uu - 0.813 * vv).clamp(0.0, 255.0) as u8;
            out[o + 2] = (yy + 2.017 * uu).clamp(0.0, 255.0) as u8;
            out[o + 3] = 255;
        }
    }
}

// -- encoding (desktop) --------------------------------------------------------------------

/// Frames the network side may hold before the encoder drops one. Two is a frame in flight
/// and a frame waiting; more is latency standing in a queue.
pub(crate) const ENCODER_QUEUE: usize = 2;
/// Frames one subscriber may have queued before it is skipped to the next keyframe.
const SUBSCRIBER_QUEUE: usize = 2;

/// Runs on its own thread: polls the source, encodes, hands packets to the publish task.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn run_encoder(
    mut source: Box<dyn VideoSource>,
    config: VideoConfig,
    keyframe_wanted: Arc<AtomicBool>,
    shared: Arc<EncoderShared>,
    out: tokio::sync::mpsc::Sender<Arc<VideoPacket>>,
) {
    use openh264::{
        encoder::{
            BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
            RateControlMode, UsageType,
        },
        formats::{BgraSliceU8, RgbaSliceU8, YUVBuffer, YUVSlices, YUVSource},
    };
    let min_gap = std::time::Duration::from_secs_f32(1.0 / config.max_fps.max(1.0));
    let mut encoder: Option<Encoder> = None;
    let mut group: u32 = 0;
    let mut yuv: Option<YUVBuffer> = None;
    let mut reported = false;
    while !shared.stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        let Some(frame) = source.next_frame() else {
            std::thread::sleep(std::time::Duration::from_millis(2));
            continue;
        };
        let (w, h) = (frame.width as usize, frame.height as usize);
        if w < 16 || h < 16 || w % 2 != 0 || h % 2 != 0 {
            tracing::warn!("bevy_iroh: video frames must be even-sized and at least 16x16");
            continue;
        }
        shared.saw(&frame);
        if !reported {
            reported = true;
            // Zero means the source is handing over blank frames, which is indistinguishable
            // downstream from a working pipeline carrying a black picture.
            let brightest = match &frame.pixels {
                Pixels::Rgba(data) | Pixels::Bgra(data) => data.iter().copied().max().unwrap_or(0),
                Pixels::I420 { y, .. } => y.iter().copied().max().unwrap_or(0),
            };
            tracing::info!(
                "bevy_iroh: encoding {w}x{h} video at {} kbps, brightest byte {brightest}",
                config.bitrate_for(frame.width, frame.height) / 1000
            );
        }
        let enc = match &mut encoder {
            Some(e) => e,
            None => {
                let encoder_config = EncoderConfig::new()
                    .bitrate(BitRate::from_bps(
                        config.bitrate_for(frame.width, frame.height),
                    ))
                    .rate_control_mode(RateControlMode::Bitrate)
                    .max_frame_rate(FrameRate::from_hz(config.max_fps))
                    .usage_type(UsageType::CameraVideoRealTime)
                    .intra_frame_period(IntraFramePeriod::from_num_frames(config.keyframe_interval))
                    .skip_frames(true);
                match Encoder::with_api_config(openh264::OpenH264API::from_source(), encoder_config)
                {
                    Ok(e) => encoder.insert(e),
                    Err(e) => {
                        tracing::error!("bevy_iroh: h264 encoder: {e}");
                        return;
                    }
                }
            }
        };
        if keyframe_wanted.swap(false, Ordering::Relaxed) {
            enc.force_intra_frame();
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
                enc.encode_at(buffer, pts)
            }
            Pixels::Bgra(data) => {
                if data.len() < w * h * 4 {
                    continue;
                }
                let buffer = yuv.get_or_insert_with(|| YUVBuffer::new(w, h));
                if buffer.dimensions() != (w, h) {
                    *buffer = YUVBuffer::new(w, h);
                }
                buffer.read_bgra8(BgraSliceU8::new(&data[..w * h * 4], (w, h)));
                enc.encode_at(buffer, pts)
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
                enc.encode_at(&slices, pts)
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
        shared.encoded.fetch_add(1, Ordering::Relaxed);
        let packet = Arc::new(VideoPacket {
            group,
            keyframe,
            pts_ms: frame.timestamp_ms,
            data,
        });
        match out.try_send(packet) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                // The network side is behind; a queued frame is latency. Drop it, and start
                // a fresh group next so no subscriber decodes past a hole.
                shared.dropped.fetch_add(1, Ordering::Relaxed);
                keyframe_wanted.store(true, Ordering::Relaxed);
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
        }
        // Pacing counts the encode against the interval, so a slow encode does not add to it.
        let took = started.elapsed();
        if took < min_gap {
            std::thread::sleep(min_gap - took);
        }
    }
}

/// Runs on the network runtime: fans packets out to one task per subscriber, each with its
/// own short queue. Subscribers are re-read at every keyframe, which is also when a new one
/// can start, and a subscriber that cannot keep up is skipped to the next keyframe.
pub(crate) async fn run_publisher(
    hub: Arc<MediaHub>,
    track: u64,
    mut packets: tokio::sync::mpsc::Receiver<Arc<VideoPacket>>,
) {
    use std::collections::HashMap;
    struct Feed {
        tx: tokio::sync::mpsc::Sender<Arc<VideoPacket>>,
        /// Set when a packet was dropped for this subscriber: its stream is broken until the
        /// next keyframe.
        broken: Arc<AtomicBool>,
    }
    let mut feeds: HashMap<u64, Feed> = HashMap::new();
    let keyframe_wanted = hub.keyframe_flag(track);
    while let Some(packet) = packets.recv().await {
        if packet.keyframe {
            let subscribers = hub.subscribers_of(track);
            feeds.retain(|id, _| subscribers.iter().any(|l| l.id() == *id));
            for link in subscribers {
                let id = link.id();
                if feeds.contains_key(&id) {
                    continue;
                }
                let (tx, rx) = tokio::sync::mpsc::channel(SUBSCRIBER_QUEUE);
                let broken = Arc::new(AtomicBool::new(false));
                n0_future::task::spawn(serve_subscriber(link, track, rx, broken.clone()));
                feeds.insert(id, Feed { tx, broken });
            }
            tracing::debug!(
                "bevy_iroh: video group {} to {} subscriber(s)",
                packet.group,
                feeds.len()
            );
        }
        let mut gone = Vec::new();
        for (id, feed) in feeds.iter() {
            match feed.tx.try_send(packet.clone()) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    if !feed.broken.swap(true, Ordering::Relaxed) {
                        // The sooner the next keyframe, the sooner this subscriber is back.
                        keyframe_wanted.store(true, Ordering::Relaxed);
                    }
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => gone.push(*id),
            }
        }
        for id in gone {
            feeds.remove(&id);
        }
    }
}

async fn serve_subscriber(
    link: Link,
    track: u64,
    mut packets: tokio::sync::mpsc::Receiver<Arc<VideoPacket>>,
    broken: Arc<AtomicBool>,
) {
    match link {
        Link::Quic(conn) => serve_quic(conn, track, packets, broken).await,
        Link::Rtc(link) => {
            // One reliable channel: a frame is a message, and a hole in a group means
            // waiting for the next keyframe. A link that will not take a frame is behind
            // by more than a group's worth, and a fresh task starts it at the next keyframe.
            let mut skipping = false;
            while let Some(packet) = packets.recv().await {
                if packet.keyframe {
                    skipping = false;
                    broken.store(false, Ordering::Relaxed);
                } else if skipping || broken.load(Ordering::Relaxed) {
                    skipping = true;
                    continue;
                }
                if !link.send(Outbound::Video(MediaHub::video_message(track, &packet))) {
                    return;
                }
            }
        }
    }
}

async fn serve_quic(
    conn: iroh::endpoint::Connection,
    track: u64,
    mut packets: tokio::sync::mpsc::Receiver<Arc<VideoPacket>>,
    broken: Arc<AtomicBool>,
) {
    let mut stream: Option<iroh::endpoint::SendStream> = None;
    while let Some(packet) = packets.recv().await {
        if packet.keyframe {
            if let Some(mut old) = stream.take() {
                let _ = old.finish();
            }
            broken.store(false, Ordering::Relaxed);
            match MediaHub::open_group(&conn, track, packet.group).await {
                Ok(s) => stream = Some(s),
                Err(e) => {
                    tracing::debug!("bevy_iroh: open video group: {e:#}");
                    return;
                }
            }
        } else if broken.load(Ordering::Relaxed) {
            // A hole in this group: the rest of it would decode as garbage.
            if let Some(mut old) = stream.take() {
                let _ = old.finish();
            }
            continue;
        }
        let Some(s) = &mut stream else { continue };
        if MediaHub::write_video_frame(s, &packet).await.is_err() {
            return;
        }
    }
    if let Some(mut s) = stream.take() {
        let _ = s.finish();
    }
}

// -- decoding ------------------------------------------------------------------------------

/// A decoded frame, RGBA.
#[derive(Clone)]
pub struct RgbaFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub pts_ms: u64,
}

/// Packets more than this far behind the newest keyframe are skipped: a picture that is
/// 150 ms old is not live, and the keyframe is a place to start fresh.
pub(crate) const MAX_BEHIND_MS: u64 = 150;
/// Packets waiting for the decoder before the oldest is dropped.
#[cfg(not(target_arch = "wasm32"))]
const DECODE_QUEUE: usize = 16;

/// A remote video track: packets go to a decoder, the newest decoded frame waits here.
pub struct RemoteVideo {
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) track: u64,
    #[cfg(not(target_arch = "wasm32"))]
    packets: Mutex<Option<std::sync::mpsc::SyncSender<VideoPacket>>>,
    latest: Mutex<Option<RgbaFrame>>,
    received: AtomicU64,
    decoded: AtomicU64,
    skipped: AtomicU64,
    dropped: AtomicU64,
}

impl RemoteVideo {
    pub(crate) fn new(track: u64) -> Self {
        Self {
            track,
            #[cfg(not(target_arch = "wasm32"))]
            packets: Mutex::new(None),
            latest: Mutex::new(None),
            received: AtomicU64::new(0),
            decoded: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn push(self: &Arc<Self>, packet: VideoPacket) {
        self.received.fetch_add(1, Ordering::Relaxed);
        let mut slot = self.packets.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            let (tx, rx) = std::sync::mpsc::sync_channel(DECODE_QUEUE);
            let me = self.clone();
            if std::thread::Builder::new()
                .name("bevy_iroh-decode".into())
                .spawn(move || me.run_decoder(rx))
                .is_ok()
            {
                *slot = Some(tx);
            }
        }
        if let Some(tx) = slot.as_ref() {
            match tx.try_send(packet) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => *slot = None,
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn push(self: &Arc<Self>, packet: VideoPacket) {
        self.received.fetch_add(1, Ordering::Relaxed);
        super::web::video::decode(self, packet);
    }

    /// The newest decoded frame, taken. `None` until the first keyframe decodes, and between
    /// frames.
    pub fn take_frame(&self) -> Option<RgbaFrame> {
        self.latest.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// A decoded picture, from whichever decoder produced it.
    pub(crate) fn deliver(&self, frame: RgbaFrame) -> Option<Vec<u8>> {
        if self.decoded.fetch_add(1, Ordering::Relaxed) == 0 {
            let brightest = frame.data.iter().copied().max().unwrap_or(0);
            tracing::info!(
                "bevy_iroh: seeing a {}x{} picture, brightest byte {brightest}",
                frame.width,
                frame.height
            );
        }
        self.latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(frame)
            .map(|unshown| unshown.data)
    }

    pub(crate) fn skipped(&self, n: u64) {
        self.skipped.fetch_add(n, Ordering::Relaxed);
    }

    /// Frames decoded so far.
    pub fn decoded(&self) -> u64 {
        self.decoded.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> VideoTrackStats {
        VideoTrackStats {
            received: self.received.load(Ordering::Relaxed),
            decoded: self.decoded(),
            skipped: self.skipped.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn run_decoder(self: Arc<Self>, rx: std::sync::mpsc::Receiver<VideoPacket>) {
        use std::collections::VecDeque;

        use openh264::formats::YUVSource;
        let mut decoder = match openh264::decoder::Decoder::new() {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("bevy_iroh: h264 decoder: {e}");
                return;
            }
        };
        let mut current_group: Option<u32> = None;
        let mut spare: Vec<u8> = Vec::new();
        let mut queue: VecDeque<VideoPacket> = VecDeque::new();
        loop {
            if queue.is_empty() {
                match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                    Ok(packet) => queue.push_back(packet),
                    Err(_) => {
                        // The subscription is gone when this thread holds the last reference.
                        if Arc::strong_count(&self) == 1 {
                            return;
                        }
                        continue;
                    }
                }
            }
            while let Ok(packet) = rx.try_recv() {
                queue.push_back(packet);
            }
            // Behind by more than a beat, with a fresh start waiting: take it.
            if let Some(newest_key) = queue.iter().rposition(|p| p.keyframe)
                && newest_key > 0
            {
                let span = queue[newest_key].pts_ms.saturating_sub(queue[0].pts_ms);
                if span > MAX_BEHIND_MS {
                    queue.drain(..newest_key);
                    self.skipped(newest_key as u64);
                }
            }
            let packet = queue.pop_front().expect("non-empty");
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
            // Reuse the buffer the Bevy side handed back, rather than allocating a full
            // picture per frame.
            let mut rgba = std::mem::take(&mut spare);
            rgba.resize(w * h * 4, 0);
            decoded.write_rgba8(&mut rgba);
            if let Some(unshown) = self.deliver(RgbaFrame {
                width: w as u32,
                height: h as u32,
                data: rgba,
                pts_ms: packet.pts_ms,
            }) {
                spare = unshown;
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_scales_with_the_picture() {
        let config = VideoConfig::default();
        assert_eq!(config.bitrate_for(640, 480), 645_120);
        assert_eq!(config.bitrate_for(1280, 720), 1_935_360);
        assert_eq!(config.bitrate_for(16, 16), 100_000);
        let fixed = VideoConfig {
            bitrate_bps: Some(300_000),
            ..default()
        };
        assert_eq!(fixed.bitrate_for(1280, 720), 300_000);
    }

    #[test]
    fn i420_grey_is_grey() {
        let (w, h) = (4, 2);
        let y = vec![128u8; w * h];
        let u = vec![128u8; 2];
        let v = vec![128u8; 2];
        let frame = VideoFrame {
            width: 4,
            height: 2,
            pixels: Pixels::I420 { y, u, v },
            timestamp_ms: 0,
        };
        let rgba = frame.to_rgba(Vec::new());
        assert_eq!(rgba.data.len(), w * h * 4);
        let px = &rgba.data[..4];
        assert!(px[0].abs_diff(130) <= 2 && px[1].abs_diff(130) <= 2 && px[2].abs_diff(130) <= 2);
        assert_eq!(px[3], 255);
    }

    fn default<T: Default>() -> T {
        T::default()
    }
}
