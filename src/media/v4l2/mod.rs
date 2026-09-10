//! Cameras through V4L2. Behind the `v4l2` feature, Linux only.
//!
//! * [`capture`]: V4L2 streaming with kernel `MMAP` buffers exported as DMA-BUF file
//!   descriptors, newest-frame-wins delivery, and the CPU cache write-back that keeps a
//!   non-snooping GPU coherent with the driver's writes.
//! * [`dmabuf`] (feature `dmabuf`): enables the Vulkan extensions during Bevy's device creation
//!   and wraps a DMA-BUF as a `wgpu::Texture` with no copy.
//! * [`plugin`]: [`WebcamPlugin`] and the [`Webcam`] component, which put the feed on a plane
//!   with a material whose shader converts packed YUV to RGB on the GPU; MJPEG streams are
//!   decoded on a thread ([`mjpeg`]).
//! * [`select`]: picks the best mode for a wanted size and rate, preferring raw (zero-copy).
//! * [`controls`]: device listing and V4L2 controls (brightness, focus, zoom, ...).
//!
//! [`Camera`] opens a device headless: no plane, no material, no render world, just the
//! capture thread and its colour tap feeding the encoder in planar 4:2:0 with the driver's
//! timestamps. [`VideoInput::camera`](super::VideoInput::camera) builds one from
//! [`MediaSettings::camera`](super::MediaSettings). An app that already runs a [`Webcam`]
//! entity can share its capture instead: `VideoInput::new(feed.capture.color_tap())`.
//!
//! ```no_run
//! use bevy::prelude::*;
//! use bevy_iroh::media::v4l2::{Webcam, WebcamPlugin};
//!
//! App::new()
//!     .add_plugins(DefaultPlugins)
//!     .add_plugins(WebcamPlugin)
//!     .add_systems(Startup, |mut commands: Commands| {
//!         commands.spawn((Webcam::want(1280, 720, 60), Transform::from_xyz(0.0, 0.45, 0.0)));
//!     })
//!     .run();
//! ```
//!
//! With the `dmabuf` feature, add `DmabufTexturePlugin` before `DefaultPlugins` so the Vulkan
//! device gets the DMA-BUF extensions and the plane samples the driver's buffers directly.

pub mod capture;
pub mod controls;
#[cfg(feature = "dmabuf")]
pub mod dmabuf;
pub mod mjpeg;
pub mod plugin;
pub mod select;

pub use capture::*;
pub use controls::*;
#[cfg(feature = "dmabuf")]
pub use dmabuf::{DmabufImportEnabled, DmabufTexturePlugin};
pub use plugin::*;
pub use select::*;
pub use v4l::FourCC;

use std::{sync::Arc, time::Duration};

use super::{
    devices::CameraDevice,
    video::{Pixels, VideoFrame, VideoSource},
};
/// Every capture device under `/dev`, with the mode it would open at.
pub fn cameras() -> Vec<CameraDevice> {
    controls::devices()
        .into_iter()
        .enumerate()
        .map(|(i, info)| {
            let detail = capture::list_modes(&info.path)
                .ok()
                .and_then(|modes| choose(&modes, RequestedFormat::HighestResolution))
                .map(|c| format!("{}x{} {} @ {:.0} fps", c.width, c.height, c.fourcc, c.fps))
                .unwrap_or_else(|| "no usable mode".into());
            CameraDevice {
                id: info.path.to_string_lossy().into_owned(),
                name: info.name,
                is_default: i == 0,
                detail,
            }
        })
        .collect()
}

/// A running camera, as a video source.
pub struct Camera {
    capture: Arc<Capture>,
    tap: ColorTap,
    /// Present for compressed modes: it returns buffers to the driver and feeds the tap.
    _decoder: Option<mjpeg::Decoder>,
    /// Raw modes hand every frame to us to return.
    raw: bool,
}

impl Camera {
    /// Open `id` (a `/dev/videoN` path; `None` is the first camera) at the mode nearest to
    /// `width`x`height` at `fps`, raw if a raw mode reaches the rate, MJPEG otherwise.
    pub fn open(id: Option<&str>, width: u32, height: u32, fps: f32) -> Result<Self, String> {
        let path = match id {
            Some(id) => std::path::PathBuf::from(id),
            None => controls::devices()
                .into_iter()
                .next()
                .map(|d| d.path)
                .ok_or("no camera")?,
        };
        let modes = capture::list_modes(&path)
            .map_err(|e| format!("cannot enumerate {}: {e}", path.display()))?;
        let want = CameraFormat::new(width, height, FrameFormat::Any, fps.round() as u32);
        let choice = choose(&modes, RequestedFormat::Closest(want))
            .ok_or_else(|| format!("{} offers nothing usable", path.display()))?;
        let config = CaptureConfig {
            device: path.clone(),
            width: choice.width,
            height: choice.height,
            fourcc: choice.fourcc,
            fps: Some(choice.fps.round() as u32),
            ..Default::default()
        };
        let capture = Arc::new(Capture::open(&config).map_err(|e| format!("camera: {e}"))?);
        let layout = capture.layout();
        let raw = layout.fourcc != FOURCC_MJPG;
        tracing::info!(
            "bevy_iroh: camera {} at {}x{} {} @ {:.0} fps ({})",
            path.display(),
            layout.width,
            layout.height,
            layout.fourcc,
            choice.fps,
            if raw { "raw" } else { "MJPEG" }
        );
        let tap = capture.color_tap();
        let decoder =
            (!raw).then(|| mjpeg::Decoder::start(capture.clone(), layout.width, layout.height));
        Ok(Self {
            capture,
            tap,
            _decoder: decoder,
            raw,
        })
    }
}

impl VideoSource for Camera {
    fn next_frame(&mut self) -> Option<VideoFrame> {
        if self.raw {
            // The colour copy was taken on the capture thread; the buffer itself goes
            // straight back to the driver.
            while let Some(frame) = self.capture.try_recv_latest() {
                self.capture.requeue(frame.index);
            }
        }
        let frame = self.tap.recv_timeout(Duration::from_millis(5))?;
        // Newest wins, if more than one arrived.
        let frame = std::iter::from_fn(|| self.tap.try_recv_latest())
            .last()
            .unwrap_or(frame);
        Some(yuv_to_frame(frame))
    }
}

fn yuv_to_frame(frame: capture::YuvFrame) -> VideoFrame {
    VideoFrame {
        width: frame.width,
        height: frame.height,
        pixels: Pixels::I420 {
            y: frame.y,
            u: frame.u,
            v: frame.v,
        },
        timestamp_ms: frame.timestamp.as_millis() as u64,
    }
}

/// A [`ColorTap`] as a source, for sharing a `Webcam` entity's capture.
impl VideoSource for ColorTap {
    fn next_frame(&mut self) -> Option<VideoFrame> {
        self.try_recv_latest().map(yuv_to_frame)
    }
}
