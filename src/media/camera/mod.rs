//! Cameras on macOS and Windows, headless: no plane, no render world, just the capture and
//! its frames going to the encoder in planar 4:2:0. The Linux counterpart is
//! [`v4l2::Camera`](super::v4l2::Camera); a page uses `getUserMedia`.
//!
//! `macos` is AVFoundation, asked for packed 4:2:2 whatever the camera's own encoding, and
//! `windows` is Media Foundation reading `YUY2` or `NV12` with no converter in the way. Both
//! answer the same three questions: which cameras there are, what each offers, and a
//! `Stream` that blocks for the next frame. The device id is opaque and
//! goes back in as it came out: AVFoundation's `uniqueID`, Media Foundation's symbolic link.
//! Cameras that cannot be opened are still listed, so an empty list means none.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(target_os = "windows")]
use windows as platform;

use std::time::Instant;

use super::{
    devices::CameraDevice,
    video::{Pixels, VideoFrame, VideoSource, nv12_to_i420, yuyv_to_i420},
};

/// A camera, as the system lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
}

/// How a frame's bytes are laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// Packed 4:2:2 `Y0 U Y1 V`: `YUYV` on Linux, `YUY2` on Windows, `yuvs` on macOS.
    Yuyv,
    /// A luma plane, then an interleaved `UV` plane at half height.
    Nv12,
}

/// One mode a camera offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub width: u32,
    pub height: u32,
    /// The fastest rate the camera offers at this size and layout.
    pub fps: u32,
    pub layout: Layout,
}

/// Why a frame did not come.
pub enum Wait {
    /// Nothing yet; ask again. A camera still behind its permission prompt says this too.
    Timeout,
    /// No more frames will come: unplugged, or the stream ended.
    Ended(String),
}

/// Every camera the machine will name, with the largest mode each offers.
pub fn cameras() -> Vec<CameraDevice> {
    platform::devices()
        .into_iter()
        .enumerate()
        .map(|(i, device)| {
            let detail = platform::formats(&device.id)
                .ok()
                .and_then(|formats| {
                    formats
                        .into_iter()
                        .max_by_key(|f| (f.width * f.height, f.fps))
                })
                .map(|f| format!("{}x{} @ {} fps", f.width, f.height, f.fps))
                .unwrap_or_else(|| "no usable mode".into());
            CameraDevice {
                id: device.id,
                name: device.name,
                is_default: i == 0,
                detail,
            }
        })
        .collect()
}

/// The offered mode nearest to what was asked for: one that reaches the rate when any does,
/// then the closest size, then the fastest.
fn choose(formats: &[Format], width: u32, height: u32, fps: u32) -> Option<Format> {
    let want = width as i64 * height as i64;
    let distance = |f: &Format| (f.width as i64 * f.height as i64 - want).abs();
    let fast: Vec<&Format> = formats.iter().filter(|f| f.fps >= fps).collect();
    let pool = if fast.is_empty() {
        formats.iter().collect()
    } else {
        fast
    };
    pool.into_iter()
        .min_by_key(|f| (distance(f), std::cmp::Reverse(f.fps)))
        .copied()
}

/// A running camera, as a video source.
pub struct Camera {
    stream: platform::Stream,
    format: Format,
    opened: Instant,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    ended: bool,
}

impl Camera {
    /// Open `id` (`None` is the first camera) at the mode nearest to `width`x`height` at
    /// `fps`.
    pub fn open(id: Option<&str>, width: u32, height: u32, fps: f32) -> Result<Self, String> {
        let id = match id {
            Some(id) => id.to_string(),
            None => platform::devices()
                .into_iter()
                .next()
                .map(|d| d.id)
                .ok_or("no camera")?,
        };
        let formats = platform::formats(&id).map_err(|e| format!("cannot enumerate {id}: {e}"))?;
        let format = choose(&formats, width, height, fps.round() as u32)
            .ok_or_else(|| format!("{id} offers nothing usable"))?;
        let stream = platform::Stream::open(&id, format).map_err(|e| format!("camera: {e}"))?;
        tracing::info!(
            "bevy_iroh: camera {id} at {}x{} {:?} @ {} fps",
            format.width,
            format.height,
            format.layout,
            format.fps
        );
        Ok(Self {
            stream,
            format,
            opened: Instant::now(),
            y: Vec::new(),
            u: Vec::new(),
            v: Vec::new(),
            ended: false,
        })
    }
}

impl VideoSource for Camera {
    fn next_frame(&mut self) -> Option<VideoFrame> {
        if self.ended {
            return None;
        }
        let (w, h) = (self.format.width as usize, self.format.height as usize);
        let layout = self.format.layout;
        let Self {
            stream, y, u, v, ..
        } = self;
        let got = stream.next_frame(|bytes| match layout {
            Layout::Yuyv => yuyv_to_i420(bytes, w, h, w * 2, y, u, v),
            Layout::Nv12 => nv12_to_i420(bytes, w, h, w, y, u, v),
        });
        match got {
            Ok(()) => {}
            Err(Wait::Timeout) => return None,
            Err(Wait::Ended(why)) => {
                tracing::warn!("bevy_iroh: camera: {why}");
                self.ended = true;
                return None;
            }
        }
        Some(VideoFrame {
            width: self.format.width,
            height: self.format.height,
            pixels: Pixels::I420 {
                y: std::mem::take(&mut self.y),
                u: std::mem::take(&mut self.u),
                v: std::mem::take(&mut self.v),
            },
            timestamp_ms: self.opened.elapsed().as_millis() as u64,
        })
    }

    fn ended(&self) -> bool {
        self.ended
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(width: u32, height: u32, fps: u32) -> Format {
        Format {
            width,
            height,
            fps,
            layout: Layout::Yuyv,
        }
    }

    #[test]
    fn the_rate_is_reached_before_the_size_is_matched() {
        let offered = [f(1920, 1080, 5), f(1280, 720, 30), f(640, 480, 30)];
        assert_eq!(choose(&offered, 1920, 1080, 30), Some(f(1280, 720, 30)));
        assert_eq!(choose(&offered, 1920, 1080, 5), Some(f(1920, 1080, 5)));
        assert_eq!(choose(&offered, 640, 480, 60), Some(f(640, 480, 30)));
        assert_eq!(choose(&[], 640, 480, 30), None);
    }
}
