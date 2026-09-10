//! Screen and window capture: the desktop as a stream of frames. Behind the `desktop`
//! feature, not in a page.
//!
//! [`Capture::open`] returns at once and frames arrive on a thread of their own;
//! [`Capture::try_recv_latest`] hands out the newest. Three backends, one shape:
//!
//! * Linux (`portal`): the ScreenCast portal puts up the compositor's own picker for a
//!   monitor or window, and the pixels come over PipeWire. That is GNOME, KDE, Hyprland, sway
//!   and anything else with an `xdg-desktop-portal` backend, under Wayland or X11. Building
//!   needs the PipeWire headers (`libpipewire-0.3-dev` on Debian and Ubuntu, `pipewire-devel`
//!   on Fedora) and `clang`.
//! * macOS (`macos`): ScreenCaptureKit, the main display. Screen Recording permission is
//!   granted for the *next* launch, so the first attempt fails and says so.
//! * Windows (`windows`): Desktop Duplication, the primary output. The pointer is not drawn.
//!
//! Anything else fails at once with a reason.
//!
//! Frames are tightly packed, upright, and stamped with the time since the capture opened.
//! A portal share can be ended from the compositor's side at any moment; [`Capture::state`]
//! then reads [`State::Ended`], and nothing more arrives.
//!
//! [`Screen`] is the capture as a [`VideoSource`] for [`VideoInput::desktop`](super::VideoInput::desktop):
//! headless, and fitted into the size the entity's `VideoFeed` asked for with a box filter
//! before it reaches the encoder. A 4K monitor becomes 1080p in a few milliseconds, and a
//! small window is left as it is, cropped to even edges.
#![cfg_attr(
    not(any(target_os = "linux", target_os = "macos", target_os = "windows")),
    allow(dead_code)
)]

use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use super::video::{Pixels, VideoFrame, VideoSource};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod portal;
#[cfg(target_os = "windows")]
mod windows;

/// Platforms without a backend yet: the capture fails at once, with a reason.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod unsupported {
    use std::sync::Arc;

    use super::{DesktopConfig, Shared, State};

    pub(crate) fn spawn(_config: DesktopConfig, shared: Arc<Shared>) -> Result<(), String> {
        let why = format!("no screen capture for {} yet", std::env::consts::OS);
        shared.finish(State::Failed(why.clone()));
        Err(why)
    }
}

/// What to ask for.
#[derive(Debug, Clone, Default)]
pub struct DesktopConfig {
    /// Draw the pointer into the frames. The portal decides how and most compositors do;
    /// ScreenCaptureKit composites it; Desktop Duplication cannot.
    pub cursor: bool,
    /// A token from an earlier share's [`Capture::restore_token`]: the portal skips the picker
    /// and gives the same source again, when it still can.
    pub restore_token: Option<String>,
}

impl DesktopConfig {
    /// Cursor drawn, no token: the picker every time.
    pub fn with_cursor() -> Self {
        Self {
            cursor: true,
            restore_token: None,
        }
    }
}

/// Byte order of a [`Frame`]. Both are four bytes per pixel, alpha last and usually opaque.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Bgra,
    Rgba,
}

/// One picture of the desktop.
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub format: Format,
    /// `width * height * 4` bytes, rows tightly packed, top row first.
    pub data: Vec<u8>,
    /// Since the capture opened, on a monotonic clock.
    pub timestamp: Duration,
}

/// Where a capture is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// The picker is up, or PipeWire is still negotiating.
    Starting,
    Streaming {
        width: u32,
        height: u32,
    },
    /// The user stopped sharing, or the source went away.
    Ended,
    Failed(String),
}

/// What the capture thread and the reader share.
pub(crate) struct Shared {
    stop: AtomicBool,
    state: Mutex<State>,
    /// Newest frame not yet taken. One slot: a reader that falls behind sees the latest.
    slot: Mutex<Option<Frame>>,
    arrived: Condvar,
    frames: AtomicU64,
    restore_token: Mutex<Option<String>>,
    /// Buffers handed back by readers, for the producer to fill again.
    pool: Mutex<Vec<Vec<u8>>>,
    started: Instant,
}

impl Shared {
    fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            state: Mutex::new(State::Starting),
            slot: Mutex::new(None),
            arrived: Condvar::new(),
            frames: AtomicU64::new(0),
            restore_token: Mutex::new(None),
            pool: Mutex::new(Vec::new()),
            started: Instant::now(),
        }
    }

    pub(crate) fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub(crate) fn state(&self) -> State {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// A state that only moves forward: an ended or failed capture stays that way.
    pub(crate) fn finish(&self, state: State) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(*s, State::Ended | State::Failed(_)) {
            *s = state;
        }
        drop(s);
        self.arrived.notify_all();
    }

    /// Only the portal hands one out.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn set_restore_token(&self, token: Option<String>) {
        *self.restore_token.lock().unwrap_or_else(|e| e.into_inner()) = token;
    }

    /// A buffer to fill, from the pool or fresh.
    pub(crate) fn buffer(&self, len: usize) -> Vec<u8> {
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        let mut buf = pool.pop().unwrap_or_default();
        buf.clear();
        buf.reserve(len);
        buf
    }

    pub(crate) fn publish(&self, width: u32, height: u32, format: Format, data: Vec<u8>) {
        let frame = Frame {
            width,
            height,
            format,
            data,
            timestamp: self.started.elapsed(),
        };
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match *state {
                State::Starting => *state = State::Streaming { width, height },
                State::Streaming {
                    width: w,
                    height: h,
                } if (w, h) != (width, height) => *state = State::Streaming { width, height },
                _ => {}
            }
        }
        let old = self
            .slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(frame);
        if let Some(old) = old {
            self.recycle(old.data);
        }
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.arrived.notify_all();
    }

    pub(crate) fn recycle(&self, buf: Vec<u8>) {
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        if pool.len() < 3 {
            pool.push(buf);
        }
    }
}

/// A running (or starting, or finished) capture. Dropping it stops the thread and, on Linux,
/// closes the portal session.
pub struct Capture {
    shared: Arc<Shared>,
}

impl Capture {
    /// Start asking. Returns before the user has chosen anything; watch [`Capture::state`],
    /// or just poll for frames.
    pub fn open(config: DesktopConfig) -> Result<Self, String> {
        let shared = Arc::new(Shared::new());
        #[cfg(target_os = "linux")]
        portal::spawn(config, shared.clone())?;
        #[cfg(target_os = "macos")]
        macos::spawn(config, shared.clone())?;
        #[cfg(target_os = "windows")]
        windows::spawn(config, shared.clone())?;
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        unsupported::spawn(config, shared.clone())?;
        Ok(Self { shared })
    }

    pub fn state(&self) -> State {
        self.shared.state()
    }

    /// Whether frames are done for good: the share ended or never started.
    pub fn ended(&self) -> bool {
        matches!(self.state(), State::Ended | State::Failed(_))
    }

    /// Size of the pictures, once one has arrived.
    pub fn size(&self) -> Option<(u32, u32)> {
        match self.state() {
            State::Streaming { width, height } => Some((width, height)),
            _ => None,
        }
    }

    /// Frames received so far, taken or not.
    pub fn frame_count(&self) -> u64 {
        self.shared.frames.load(Ordering::Relaxed)
    }

    /// The portal's token for asking for the same source again without a picker, once known.
    pub fn restore_token(&self) -> Option<String> {
        self.shared
            .restore_token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The newest frame since the last call, or `None`.
    pub fn try_recv_latest(&self) -> Option<Frame> {
        self.shared
            .slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// The newest frame, waiting up to `timeout` for one to arrive.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Frame> {
        let deadline = Instant::now() + timeout;
        let mut slot = self.shared.slot.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(frame) = slot.take() {
                return Some(frame);
            }
            if self.ended() {
                return None;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (guard, _) = self
                .shared
                .arrived
                .wait_timeout(slot, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            slot = guard;
        }
    }

    /// Hand a frame's buffer back so the next one is filled without an allocation.
    pub fn recycle(&self, data: Vec<u8>) {
        self.shared.recycle(data);
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.arrived.notify_all();
    }
}

/// Copy `height` rows of `width * 4` bytes out of a buffer whose rows are `stride` apart.
pub(crate) fn unpad_rows(src: &[u8], width: u32, height: u32, stride: usize, out: &mut Vec<u8>) {
    let row = width as usize * 4;
    out.clear();
    if stride == row && src.len() >= row * height as usize {
        out.extend_from_slice(&src[..row * height as usize]);
        return;
    }
    for y in 0..height as usize {
        let start = y * stride;
        match src.get(start..start + row) {
            Some(r) => out.extend_from_slice(r),
            None => out.resize(out.len() + row, 0),
        }
    }
}

// -- The capture as a video source ----------------------------------------------------

/// A running screen share, as a video source.
pub struct Screen {
    capture: Capture,
    /// The largest picture to send, from the feed.
    fit: (u32, u32),
    scaler: Scaler,
}

impl Screen {
    /// Ask for a screen; the picker appears at once. Frames arrive after the choice, at most
    /// `width`x`height`.
    pub fn open(width: u32, height: u32) -> Result<Self, String> {
        let capture = Capture::open(DesktopConfig::with_cursor())?;
        Ok(Self {
            capture,
            fit: (width.max(16), height.max(16)),
            scaler: Scaler::default(),
        })
    }
}

impl VideoSource for Screen {
    fn ended(&self) -> bool {
        self.capture.ended()
    }

    fn next_frame(&mut self) -> Option<VideoFrame> {
        let frame = self.capture.recv_timeout(Duration::from_millis(5))?;
        let (w, h) = fitted(frame.width, frame.height, self.fit);
        let data = if (w, h) == (frame.width, frame.height) {
            frame.data
        } else {
            let out = self
                .scaler
                .scale(&frame.data, frame.width, frame.height, w, h);
            self.capture.recycle(frame.data);
            out
        };
        let pixels = match frame.format {
            Format::Bgra => Pixels::Bgra(data),
            Format::Rgba => Pixels::Rgba(data),
        };
        Some(VideoFrame {
            width: w,
            height: h,
            pixels,
            timestamp_ms: frame.timestamp.as_millis() as u64,
        })
    }
}

/// The even size a `sw`x`sh` picture takes inside `fit`, keeping its shape and never growing.
fn fitted(sw: u32, sh: u32, fit: (u32, u32)) -> (u32, u32) {
    let scale = (fit.0 as f32 / sw as f32)
        .min(fit.1 as f32 / sh as f32)
        .min(1.0);
    let w = ((sw as f32 * scale) as u32 & !1).max(16);
    let h = ((sh as f32 * scale) as u32 & !1).max(16);
    (w.min(sw & !1).max(16), h.min(sh & !1).max(16))
}

/// A box-filter downscale of packed 4-byte pixels, with its tables and output buffer kept
/// between frames. Rows are split into bands across the machine's cores: a 4K frame is 33 MB
/// to read, and one thread would take longer over it than the encoder takes over the result.
#[derive(Default)]
struct Scaler {
    size: (u32, u32, u32, u32),
    /// For each output column, the input columns it averages.
    cols: Vec<(usize, usize)>,
    rows: Vec<(usize, usize)>,
    out: Vec<u8>,
}

impl Scaler {
    fn scale(&mut self, src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
        if self.size != (sw, sh, dw, dh) {
            self.size = (sw, sh, dw, dh);
            self.cols = spans(sw, dw);
            self.rows = spans(sh, dh);
        }
        let stride = sw as usize * 4;
        let out_row = dw as usize * 4;
        let mut out = std::mem::take(&mut self.out);
        out.clear();
        out.resize(out_row * dh as usize, 0);
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, 8);
        let band = (dh as usize).div_ceil(threads).max(1);
        let (cols, rows) = (&self.cols, &self.rows);
        let scale_band = |first_row: usize, dst: &mut [u8]| {
            let mut acc = vec![0u32; dw as usize * 4];
            for (i, dst_row) in dst.chunks_exact_mut(out_row).enumerate() {
                let (y0, y1) = rows[first_row + i];
                acc.fill(0);
                for y in y0..y1 {
                    let Some(row) = src.get(y * stride..(y + 1) * stride) else {
                        break;
                    };
                    for (o, &(x0, x1)) in cols.iter().enumerate() {
                        let a = &mut acc[o * 4..o * 4 + 4];
                        for px in row[x0 * 4..x1 * 4].as_chunks::<4>().0 {
                            a[0] += px[0] as u32;
                            a[1] += px[1] as u32;
                            a[2] += px[2] as u32;
                            a[3] += px[3] as u32;
                        }
                    }
                }
                let ny = (y1 - y0).max(1) as u32;
                for (o, &(x0, x1)) in cols.iter().enumerate() {
                    let n = ny * (x1 - x0).max(1) as u32;
                    let a = &acc[o * 4..o * 4 + 4];
                    dst_row[o * 4..o * 4 + 4].copy_from_slice(&[
                        (a[0] / n) as u8,
                        (a[1] / n) as u8,
                        (a[2] / n) as u8,
                        (a[3] / n) as u8,
                    ]);
                }
            }
        };
        if threads == 1 || dh as usize <= band {
            scale_band(0, &mut out);
        } else {
            std::thread::scope(|scope| {
                for (i, dst) in out.chunks_mut(band * out_row).enumerate() {
                    let scale_band = &scale_band;
                    scope.spawn(move || scale_band(i * band, dst));
                }
            });
        }
        // Keep a spare of the right size for next time.
        self.out = Vec::with_capacity(out.len());
        out
    }
}

/// Which input pixels each output pixel covers, at least one each.
fn spans(src: u32, dst: u32) -> Vec<(usize, usize)> {
    (0..dst as usize)
        .map(|o| {
            let a = o * src as usize / dst as usize;
            let b = ((o + 1) * src as usize / dst as usize)
                .max(a + 1)
                .min(src as usize);
            (a, b)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_keep_shape_and_never_grow() {
        assert_eq!(fitted(3840, 2160, (1920, 1080)), (1920, 1080));
        assert_eq!(fitted(800, 600, (1920, 1080)), (800, 600));
        assert_eq!(fitted(801, 601, (1920, 1080)), (800, 600));
        assert_eq!(fitted(2560, 1440, (1280, 720)), (1280, 720));
        assert_eq!(fitted(1000, 3000, (1920, 1080)), (360, 1080));
    }

    #[test]
    fn halving_averages_each_block() {
        let mut src = Vec::new();
        for y in 0..4u8 {
            for x in 0..4u8 {
                src.extend_from_slice(&[x * 10, y * 10, 100, 255]);
            }
        }
        let out = Scaler::default().scale(&src, 4, 4, 2, 2);
        assert_eq!(out.len(), 16);
        assert_eq!(&out[0..4], &[5, 5, 100, 255]);
        assert_eq!(&out[4..8], &[25, 5, 100, 255]);
        assert_eq!(&out[12..16], &[25, 25, 100, 255]);
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    #[test]
    #[ignore]
    fn four_k_to_1080p() {
        let src = vec![128u8; 3840 * 2160 * 4];
        let mut scaler = Scaler::default();
        let _ = scaler.scale(&src, 3840, 2160, 1920, 1080);
        let t = Instant::now();
        for _ in 0..10 {
            let out = scaler.scale(&src, 3840, 2160, 1920, 1080);
            std::hint::black_box(&out);
        }
        eprintln!(
            "scale 4K->1080p: {:.1} ms",
            t.elapsed().as_secs_f64() * 100.0
        );
        let t = Instant::now();
        let mut out = Vec::new();
        for _ in 0..10 {
            unpad_like(&src, 3840, 2160, &mut out);
        }
        eprintln!("copy 4K: {:.1} ms", t.elapsed().as_secs_f64() * 100.0);
    }

    fn unpad_like(src: &[u8], w: u32, h: u32, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(&src[..w as usize * h as usize * 4]);
    }
}
