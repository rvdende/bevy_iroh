//! No window: open a screen capture, count what arrives for ten seconds, and print the rate.
//! The number to look at when a share feels slow: it is what the compositor delivers before
//! bevy_iroh scales, encodes or draws anything.
//!
//! ```sh
//! cargo run --example screen_stats
//! ```

#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    use std::time::{Duration, Instant};

    use bevy_iroh::media::desktop::{Capture, DesktopConfig};

    let capture = Capture::open(DesktopConfig::with_cursor()).expect("open");
    let opened = Instant::now();
    let mut first = None;
    let mut frames = 0u64;
    let mut bytes = 0usize;
    while opened.elapsed() < Duration::from_secs(120) {
        if let Some(frame) = capture.recv_timeout(Duration::from_millis(500)) {
            let t = *first.get_or_insert_with(|| {
                println!(
                    "first frame after {:.0} ms: {}x{} {:?}",
                    opened.elapsed().as_millis(),
                    frame.width,
                    frame.height,
                    frame.format
                );
                Instant::now()
            });
            frames += 1;
            bytes += frame.data.len();
            capture.recycle(frame.data);
            if t.elapsed() > Duration::from_secs(10) {
                break;
            }
        }
        if capture.ended() {
            println!("ended: {:?}", capture.state());
            break;
        }
    }
    match first {
        Some(t) => {
            let secs = t.elapsed().as_secs_f64();
            println!(
                "{frames} frames in {secs:.1} s = {:.1} fps, {:.0} MB/s; state {:?}",
                frames as f64 / secs,
                bytes as f64 / secs / 1e6,
                capture.state(),
            );
        }
        None => println!("no frames; state {:?}", capture.state()),
    }
}
