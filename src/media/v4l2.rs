//! A `bevy_v4l2` camera as a video source. Behind the `v4l2` feature, Linux only.
//!
//! ```ignore
//! fn share_camera(mut commands: Commands, feeds: Query<(Entity, &WebcamFeed), Added<WebcamFeed>>) {
//!     for (entity, feed) in &feeds {
//!         let (w, h) = (feed.layout.width, feed.layout.height);
//!         commands.entity(entity).insert((
//!             Shared::default(),
//!             VideoFeed::new(w, h),
//!             VideoInput::new(feed.capture.color_tap()),
//!         ));
//!     }
//! }
//! ```

use bevy_v4l2::ColorTap;

use super::video::{Pixels, VideoFrame, VideoSource};

impl VideoSource for ColorTap {
    fn next_frame(&mut self) -> Option<VideoFrame> {
        let frame = self.try_recv_latest()?;
        Some(VideoFrame {
            width: frame.width,
            height: frame.height,
            pixels: Pixels::I420 {
                y: frame.y,
                u: frame.u,
                v: frame.v,
            },
            timestamp_ms: frame.timestamp.as_millis() as u64,
        })
    }
}
