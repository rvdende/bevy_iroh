//! AVFoundation: enumerate the cameras, say what each delivers, and stream one.
//!
//! AVFoundation *pushes*: a serial queue the framework calls a delegate on. The encoder pulls,
//! so [`Slot`] is the bridge: the delegate drops each frame into a mutex and signals a condvar,
//! and [`Stream::next_frame`] waits on it. One frame deep on purpose; the newest wins, and
//! `alwaysDiscardsLateVideoFrames` makes the framework drop on its side too.
//!
//! The pipeline is asked for `yuvs`, packed 4:2:2 in `Y0 U Y1 V` order, whatever the camera's
//! own encoding: AVFoundation converts as a supported part of its graph rather than a CPU
//! transcode. `2vuy` (the same samples as `U Y0 V Y1`) is accepted as a fallback and turned
//! round on the way out. Every listed mode therefore says [`Layout::Yuyv`].
//!
//! The camera is behind a TCC permission. The first `startRunning` raises the prompt, and
//! until it is answered no frame arrives, which shows here as a wait rather than an error.

use std::{
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::{
    AllocAnyThread, DefinedClass, define_class,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
};
use objc2_av_foundation::{
    AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceFormat, AVCaptureDeviceInput,
    AVCaptureOutput, AVCaptureSession, AVCaptureSessionPresetInputPriority,
    AVCaptureVideoDataOutput, AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeVideo,
};
use objc2_core_media::{CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetDimensions};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_422YpCbCr8,
    kCVPixelFormatType_422YpCbCr8_yuvs,
};
use objc2_foundation::{NSDictionary, NSNumber, NSObject, NSObjectProtocol, NSString};

use super::{Device, Format, Layout, Wait};

/// How long [`Stream::next_frame`] waits before reporting nothing came. Short, so a dropped
/// capture stops promptly; the encoder simply asks again.
const FRAME_TIMEOUT: Duration = Duration::from_millis(500);

/// `Y0 Cb Y1 Cr`, byte for byte what V4L2 calls `YUYV`.
const WANTED_PIXEL_FORMAT: u32 = kCVPixelFormatType_422YpCbCr8_yuvs;
/// `Cb Y0 Cr Y1`: the same samples the other way round, swapped during the copy out.
const FALLBACK_PIXEL_FORMAT: u32 = kCVPixelFormatType_422YpCbCr8;

fn device_at(id: &str) -> Result<Retained<AVCaptureDevice>, String> {
    let unique = NSString::from_str(id);
    unsafe { AVCaptureDevice::deviceWithUniqueID(&unique) }.ok_or_else(|| format!("no camera {id}"))
}

/// Every camera the system currently has, external UVC cameras and Continuity included.
///
/// `devicesWithMediaType:` rather than a discovery session, deprecation accepted: the
/// session takes explicit `AVCaptureDeviceType`s, which are `extern` symbols with their own OS
/// availability, and naming them is how a build stops launching on the OS it was not compiled
/// against.
pub fn devices() -> Vec<Device> {
    #[expect(
        deprecated,
        reason = "the replacement needs version-gated AVCaptureDeviceType symbols"
    )]
    // SAFETY: `AVMediaTypeVideo` is an AVFoundation constant present since 10.7; the `Option`
    // is how the bindings model a nullable `extern` static.
    let found = unsafe {
        let video = AVMediaTypeVideo.expect("AVMediaTypeVideo is an AVFoundation constant");
        AVCaptureDevice::devicesWithMediaType(video)
    };
    found
        .iter()
        .map(|device| Device {
            id: unsafe { device.uniqueID() }.to_string(),
            name: unsafe { device.localizedName() }.to_string(),
        })
        .collect()
}

/// The modes a camera offers, one per size at its fastest rate, deduplicated: a camera lists
/// 1920x1080 once per native encoding, and the encoding is not varied here.
pub fn formats(id: &str) -> Result<Vec<Format>, String> {
    let device = device_at(id)?;
    let mut found: Vec<Format> = Vec::new();
    for format in unsafe { device.formats() }.iter() {
        if let Some(described) = describe(&format)
            && !found.contains(&described)
        {
            found.push(described);
        }
    }
    Ok(found)
}

fn describe(format: &AVCaptureDeviceFormat) -> Option<Format> {
    let description = unsafe { format.formatDescription() };
    let dimensions = unsafe { CMVideoFormatDescriptionGetDimensions(&description) };
    if dimensions.width <= 0 || dimensions.height <= 0 {
        return None;
    }
    let fps = unsafe { format.videoSupportedFrameRateRanges() }
        .iter()
        .map(|range| unsafe { range.maxFrameRate() })
        .fold(0.0f64, f64::max);
    Some(Format {
        width: dimensions.width as u32,
        height: dimensions.height as u32,
        fps: fps.round() as u32,
        layout: Layout::Yuyv,
    })
}

/// The frame the delegate most recently handed over, and the wait for the next one.
struct Slot {
    held: Mutex<Held>,
    ready: Condvar,
}

#[derive(Default)]
struct Held {
    /// The frame, unpadded and in `Y0 U Y1 V` order, with the pixel buffer's own size.
    latest: Option<(Vec<u8>, u32, u32)>,
    /// A buffer that has been through once, handed back to be refilled.
    spare: Vec<u8>,
}

impl Slot {
    fn new() -> Self {
        Self {
            held: Mutex::new(Held::default()),
            ready: Condvar::new(),
        }
    }

    fn buffer(&self) -> Vec<u8> {
        self.held
            .lock()
            .map(|mut held| std::mem::take(&mut held.spare))
            .unwrap_or_default()
    }

    /// The newest frame, waiting up to [`FRAME_TIMEOUT`] for one. A loop against a deadline
    /// rather than one `wait_timeout`, because a condvar may wake spuriously.
    fn take(&self) -> Option<(Vec<u8>, u32, u32)> {
        let deadline = Instant::now() + FRAME_TIMEOUT;
        let mut held = self.held.lock().ok()?;
        loop {
            if let Some(frame) = held.latest.take() {
                return Some(frame);
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            let (next, timed_out) = self.ready.wait_timeout(held, left).ok()?;
            held = next;
            if timed_out.timed_out() && held.latest.is_none() {
                return None;
            }
        }
    }

    /// Replaces whatever is held; the displaced frame's buffer becomes the next spare.
    fn put(&self, frame: (Vec<u8>, u32, u32)) {
        if let Ok(mut held) = self.held.lock() {
            if let Some((stale, _, _)) = held.latest.replace(frame) {
                held.spare = stale;
            }
            self.ready.notify_one();
        }
    }

    fn recycle(&self, buffer: Vec<u8>) {
        if let Ok(mut held) = self.held.lock()
            && held.spare.capacity() < buffer.capacity()
        {
            held.spare = buffer;
        }
    }
}

define_class!(
    // SAFETY:
    // - `NSObject` has no subclassing requirements.
    // - `SampleDelegate` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    // Built on whichever thread opened the camera and called on the dispatch queue below;
    // the only state it touches is a mutex.
    #[thread_kind = AllocAnyThread]
    #[name = "BevyIrohCameraDelegate"]
    #[ivars = Arc<Slot>]
    struct SampleDelegate;

    unsafe impl NSObjectProtocol for SampleDelegate {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for SampleDelegate {
        /// The `CVPixelBuffer` belongs to a pool the capture graph refills, and holding one
        /// past the end of this method starves the camera. The bytes are copied out here.
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(
            &self,
            _output: &AVCaptureOutput,
            sample: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            let Some(pixels) = (unsafe { sample.image_buffer() }) else {
                return;
            };
            let slot = self.ivars();
            if let Some(frame) = read_pixels(&pixels, slot.buffer()) {
                slot.put(frame);
            }
        }
    }
);

impl SampleDelegate {
    fn new(slot: Arc<Slot>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(slot);
        unsafe { objc2::msg_send![super(this), init] }
    }
}

/// Locks a pixel buffer, copies its rows out unpadded and in `Y0 U Y1 V` order, unlocks it.
/// `None` for a layout this backend did not ask for.
fn read_pixels(pixels: &CVPixelBuffer, mut data: Vec<u8>) -> Option<(Vec<u8>, u32, u32)> {
    let swapped = match CVPixelBufferGetPixelFormatType(pixels) {
        WANTED_PIXEL_FORMAT => false,
        FALLBACK_PIXEL_FORMAT => true,
        _ => return None,
    };
    // SAFETY: `pixels` is the sample's own image buffer, alive for the length of the delegate
    // call, and every path out of this function unlocks it exactly once.
    if unsafe { CVPixelBufferLockBaseAddress(pixels, CVPixelBufferLockFlags::ReadOnly) } != 0 {
        return None;
    }
    let width = CVPixelBufferGetWidth(pixels);
    let height = CVPixelBufferGetHeight(pixels);
    let stride = CVPixelBufferGetBytesPerRow(pixels);
    let base = CVPixelBufferGetBaseAddress(pixels);
    let frame = if base.is_null() || width == 0 || height == 0 || stride < width * 2 {
        None
    } else {
        // SAFETY: the lock succeeded, so `base` points at `stride * height` readable bytes
        // until the unlock below.
        let source = unsafe { std::slice::from_raw_parts(base as *const u8, stride * height) };
        copy_rows(source, width, height, stride, swapped, &mut data);
        Some((data, width as u32, height as u32))
    };
    // SAFETY: paired with the successful lock above, with the same flags.
    unsafe { CVPixelBufferUnlockBaseAddress(pixels, CVPixelBufferLockFlags::ReadOnly) };
    frame
}

/// `height` rows of `width * 2` bytes out of rows `stride` apart, quads put in `Y0 U Y1 V`
/// order. `dst` is cleared first, so a reused buffer can be handed straight in.
fn copy_rows(
    src: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    swapped: bool,
    dst: &mut Vec<u8>,
) {
    let row_bytes = width * 2;
    dst.clear();
    dst.reserve(row_bytes * height);
    for row in 0..height {
        let start = row * stride;
        if start + row_bytes > src.len() {
            break;
        }
        let line = &src[start..start + row_bytes];
        if !swapped {
            dst.extend_from_slice(line);
            continue;
        }
        for quad in line.as_chunks::<4>().0 {
            dst.extend_from_slice(&[quad[1], quad[0], quad[3], quad[2]]);
        }
    }
}

/// A running capture. The delegate and the queue are held for their lifetimes: AVFoundation
/// keeps the delegate only weakly, and releasing the queue under a running session pulls the
/// callback queue out from under the framework.
pub struct Stream {
    session: Retained<AVCaptureSession>,
    _delegate: Retained<SampleDelegate>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    slot: Arc<Slot>,
    latest: Vec<u8>,
    format: Format,
}

// SAFETY: every Objective-C object here is used only from the thread that owns the `Stream`,
// which is moved to the encoder thread once and never shared. `slot` is `Sync` on its own
// terms, and `AVCaptureSession` is documented as usable from any single thread.
unsafe impl Send for Stream {}

impl Drop for Stream {
    /// `stopRunning` is what closes the device and drops the camera indicator.
    fn drop(&mut self) {
        unsafe { self.session.stopRunning() };
    }
}

impl Stream {
    /// Opens a camera at one of the modes it offered.
    pub fn open(id: &str, wanted: Format) -> Result<Self, String> {
        let device = device_at(id)?;
        let chosen = unsafe { device.formats() }
            .iter()
            .find(|format| describe(format) == Some(wanted))
            .ok_or_else(|| {
                format!(
                    "{id} does not offer {}x{} at {} fps",
                    wanted.width, wanted.height, wanted.fps
                )
            })?;

        let session = unsafe { AVCaptureSession::new() };
        let input = unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&device) }
            .map_err(|why| why.localizedDescription().to_string())?;
        let output = unsafe { AVCaptureVideoDataOutput::new() };
        // Dropped rather than queued when the consumer falls behind.
        unsafe { output.setAlwaysDiscardsLateVideoFrames(true) };
        set_pixel_format(&output)?;

        let slot = Arc::new(Slot::new());
        let delegate = SampleDelegate::new(slot.clone());
        // Serial, so frames arrive in order into a single-cell slot.
        let queue = DispatchQueue::new("bevy_iroh.camera", DispatchQueueAttr::SERIAL);
        unsafe {
            output.setSampleBufferDelegate_queue(
                Some(ProtocolObject::from_ref(&*delegate)),
                Some(&queue),
            );
        }

        // Every `can*` check first: `addInput`/`addOutput` on a graph that will not take them
        // raises an Objective-C exception, which is an abort rather than an error.
        unsafe { session.beginConfiguration() };
        let added = unsafe {
            // A session's default preset is a promise about the output that it keeps by
            // reconfiguring its inputs, so attaching a device would overwrite the chosen
            // format. `InputPriority` is how AVFoundation spells "the format I chose wins".
            let preset = AVCaptureSessionPresetInputPriority;
            if session.canSetSessionPreset(preset) {
                session.setSessionPreset(preset);
            }
            let input_ok = session.canAddInput(&input);
            if input_ok {
                session.addInput(&input);
            }
            let output_ok = session.canAddOutput(&output);
            if output_ok {
                session.addOutput(&output);
            }
            input_ok && output_ok
        };
        unsafe { session.commitConfiguration() };
        if !added {
            return Err(format!(
                "{id} could not be added to a capture session; it may be in use by another app"
            ));
        }
        // After the graph is committed and priority is the input's: this is the format the
        // camera keeps.
        configure(&device, &chosen, wanted.fps)?;
        unsafe { session.startRunning() };

        Ok(Self {
            session,
            _delegate: delegate,
            _queue: queue,
            slot,
            latest: Vec::new(),
            format: wanted,
        })
    }

    /// Waits for the next frame and hands its bytes to `consume`.
    pub fn next_frame(&mut self, consume: impl FnOnce(&[u8])) -> Result<(), Wait> {
        let Some((data, width, height)) = self.slot.take() else {
            return Err(Wait::Timeout);
        };
        if (width, height) != (self.format.width, self.format.height) {
            return Err(Wait::Ended(format!(
                "the camera changed to {width}x{height}, but the capture was opened at {}x{}",
                self.format.width, self.format.height
            )));
        }
        let previous = std::mem::replace(&mut self.latest, data);
        self.slot.recycle(previous);
        consume(&self.latest);
        Ok(())
    }
}

/// Pins the device to one format and one rate, on both ends: setting only the minimum leaves a
/// UVC camera free to halve its rate in poor light.
fn configure(
    device: &AVCaptureDevice,
    format: &AVCaptureDeviceFormat,
    fps: u32,
) -> Result<(), String> {
    unsafe { device.lockForConfiguration() }
        .map_err(|why| why.localizedDescription().to_string())?;
    unsafe { device.setActiveFormat(format) };
    if fps > 0 {
        let duration = CMTime {
            value: 1,
            timescale: fps as i32,
            flags: objc2_core_media::CMTimeFlags::Valid,
            epoch: 0,
        };
        unsafe { device.setActiveVideoMinFrameDuration(duration) };
        unsafe { device.setActiveVideoMaxFrameDuration(duration) };
    }
    unsafe { device.unlockForConfiguration() };
    Ok(())
}

/// Asks the output for [`WANTED_PIXEL_FORMAT`], or [`FALLBACK_PIXEL_FORMAT`]. Checked against
/// what the graph offers first: setting a format it cannot produce raises an exception.
fn set_pixel_format(output: &AVCaptureVideoDataOutput) -> Result<(), String> {
    let available: Vec<u32> = unsafe { output.availableVideoCVPixelFormatTypes() }
        .iter()
        .map(|number| number.as_u32())
        .collect();
    let chosen = [WANTED_PIXEL_FORMAT, FALLBACK_PIXEL_FORMAT]
        .into_iter()
        .find(|format| available.contains(format))
        .ok_or_else(|| {
            format!(
                "this camera's pipeline offers no packed 4:2:2 output ({})",
                available
                    .iter()
                    .map(|f| fourcc_name(*f))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    // The key is a `CFString` and the dictionary is typed in `NSString`; they are the same
    // object at runtime (toll-free bridging), so this is a change of Rust type over one
    // pointer.
    //
    // SAFETY: `kCVPixelBufferPixelFormatTypeKey` is a CoreVideo `CFString` constant, and every
    // `CFString` is an `NSString`; the reference it is read from is `'static`.
    let key: &NSString = unsafe { &*std::ptr::from_ref(kCVPixelBufferPixelFormatTypeKey).cast() };
    let value = NSNumber::new_u32(chosen);
    let objects: [&AnyObject; 1] = [&value];
    let settings = NSDictionary::from_slices(&[key], &objects);
    unsafe { output.setVideoSettings(Some(&settings)) };
    Ok(())
}

/// A CoreVideo pixel format as its four letters, for a message. These are big-endian
/// `OSType`s; the RGB ones are small integers and are printed as numbers.
fn fourcc_name(value: u32) -> String {
    let letters: String = value.to_be_bytes().iter().map(|b| *b as char).collect();
    if letters.chars().all(|c| c.is_ascii_graphic()) {
        letters
    } else {
        format!("0x{value:08x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_is_taken_off_every_row() {
        let src = vec![0, 1, 2, 3, 9, 9, 9, 9, 4, 5, 6, 7, 9, 9, 9, 9];
        let mut out = Vec::new();
        copy_rows(&src, 2, 2, 8, false, &mut out);
        assert_eq!(out, vec![0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn the_swapped_layout_comes_out_in_yuyv_order() {
        let mut out = Vec::new();
        copy_rows(&[10, 20, 30, 40], 2, 1, 4, true, &mut out);
        assert_eq!(out, vec![20, 10, 40, 30]);
    }

    #[test]
    fn a_short_buffer_stops_instead_of_reading_past_it() {
        let mut out = Vec::new();
        copy_rows(&[0, 1, 2, 3, 4, 5], 2, 2, 4, false, &mut out);
        assert_eq!(out, vec![0, 1, 2, 3]);
    }

    #[test]
    fn the_two_accepted_formats_are_the_four_letter_codes_they_claim() {
        assert_eq!(fourcc_name(WANTED_PIXEL_FORMAT), "yuvs");
        assert_eq!(fourcc_name(FALLBACK_PIXEL_FORMAT), "2vuy");
        assert_eq!(fourcc_name(32), "0x00000020");
    }
}
