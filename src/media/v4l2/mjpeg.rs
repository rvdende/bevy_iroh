//! MJPEG decode thread: dequeues JPEG frames, decodes the newest into RGBA, recycles buffers.
//!
//! Only used when the camera's chosen mode is compressed. The V4L2 buffer is returned to the
//! driver the moment decoding finishes, so buffer depth is independent of GPU pacing.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use super::capture::Capture;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use zune_jpeg::{
    JpegDecoder,
    zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions},
};

/// A decoded frame, tightly packed RGBA.
pub struct DecodedFrame {
    pub rgba: Vec<u8>,
    #[allow(dead_code)]
    pub sequence: u32,
    pub timestamp: Duration,
    pub decode_time: Duration,
}

pub struct Decoder {
    frames: Receiver<DecodedFrame>,
    pool: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    skipped: Arc<AtomicU64>,
    failed: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl Decoder {
    pub fn start(capture: Arc<Capture>, width: u32, height: u32) -> Self {
        let (tx, rx) = crossbeam_channel::bounded(2);
        let pool: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let skipped = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicU64::new(0));
        let thread = {
            let (pool, stop, skipped, failed) =
                (pool.clone(), stop.clone(), skipped.clone(), failed.clone());
            std::thread::Builder::new()
                .name("mjpeg-decode".into())
                .spawn(move || decode_loop(capture, width, height, tx, pool, stop, skipped, failed))
                .expect("spawn mjpeg decoder")
        };
        Self {
            frames: rx,
            pool,
            stop,
            skipped,
            failed,
            thread: Some(thread),
        }
    }

    /// Newest decoded frame, recycling any older ones still queued.
    pub fn try_recv_latest(&self) -> Option<DecodedFrame> {
        let mut latest = None;
        while let Ok(f) = self.frames.try_recv() {
            if let Some(prev) = latest.replace(f) {
                self.recycle(prev.rgba);
                self.skipped.fetch_add(1, Ordering::Relaxed);
            }
        }
        latest
    }

    /// Return a buffer for reuse.
    pub fn recycle(&self, buf: Vec<u8>) {
        let mut pool = self.pool.lock().unwrap();
        if pool.len() < 4 {
            pool.push(buf);
        }
    }

    pub fn skipped(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
    }

    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_loop(
    capture: Arc<Capture>,
    width: u32,
    height: u32,
    tx: Sender<DecodedFrame>,
    pool: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    skipped: Arc<AtomicU64>,
    failed: Arc<AtomicU64>,
) {
    let options = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGBA)
        .set_strict_mode(false);
    let expected = (width * height * 4) as usize;
    while !stop.load(Ordering::Relaxed) {
        let mut frame = match capture.frames().recv_timeout(Duration::from_millis(100)) {
            Ok(f) => f,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        // Newest wins: anything else already waiting goes straight back to the driver.
        while let Ok(newer) = capture.frames().try_recv() {
            capture.requeue(frame.index);
            skipped.fetch_add(1, Ordering::Relaxed);
            frame = newer;
        }
        let buffer = &capture.buffers()[frame.index as usize];
        let len = (frame.bytes_used as usize).min(buffer.length);
        let bytes = &buffer.as_slice()[..len];

        let mut out = pool.lock().unwrap().pop().unwrap_or_default();
        out.resize(expected, 0);
        let t0 = Instant::now();
        let mut decoder = JpegDecoder::new_with_options(ZCursor::new(bytes), options);
        let result = decoder.decode_headers().and_then(|_| {
            let info = decoder.info().expect("headers decoded");
            if (info.width as u32, info.height as u32) != (width, height) {
                return Err(zune_jpeg::errors::DecodeErrors::Format(format!(
                    "frame is {}x{}, stream is {width}x{height}",
                    info.width, info.height
                )));
            }
            decoder.decode_into(&mut out)
        });
        let decode_time = t0.elapsed();
        // The JPEG bytes are no longer needed; give the kernel its buffer back now.
        capture.requeue(frame.index);

        match result {
            Ok(()) => {
                capture.publish_gray_from_rgba(
                    &out,
                    width,
                    height,
                    frame.sequence,
                    frame.timestamp,
                );
                capture.publish_color_from_rgba(
                    &out,
                    width,
                    height,
                    frame.sequence,
                    frame.timestamp,
                );
                let decoded = DecodedFrame {
                    rgba: out,
                    sequence: frame.sequence,
                    timestamp: frame.timestamp,
                    decode_time,
                };
                match tx.try_send(decoded) {
                    Ok(()) => {}
                    Err(TrySendError::Full(d)) => {
                        // Consumer is behind; it drains to the newest anyway, so drop this one.
                        pool.lock().unwrap().push(d.rgba);
                        skipped.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
            Err(e) => {
                failed.fetch_add(1, Ordering::Relaxed);
                if failed.load(Ordering::Relaxed) <= 3 {
                    tracing::warn!("mjpeg decode failed: {e:?}");
                }
                pool.lock().unwrap().push(out);
            }
        }
    }
}
