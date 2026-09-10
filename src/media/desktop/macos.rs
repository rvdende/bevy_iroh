//! macOS: ScreenCaptureKit delivers the main display.
//!
//! The one structural difference from the portal: ScreenCaptureKit *pushes*. An `SCStream`
//! hands `CMSampleBuffer`s to a delegate on a dispatch queue it owns, at whatever rate the
//! desktop changes, so the thread spawned here exists only to hold the stream open and to
//! notice when the capture is dropped. The delegate publishes straight into [`Shared`].
//!
//! Frames are `CVPixelBuffer`s from a pool the window server refills, and `queueDepth` is all
//! the slack there is: the delegate locks each one, copies its rows out unpadded, and unlocks
//! it before returning. There is no picker; the main display (the one with the menu bar) is
//! what is shared, and the cursor is composited in when [`DesktopConfig::cursor`] asks.
//!
//! Screen Recording is a TCC permission with no blocking prompt: the first attempt raises the
//! system dialog *and fails*, and the app has to be relaunched after it is granted. A refusal
//! arrives as an error out of `SCShareableContent` or `startCapture`, and both say so.

use std::{
    sync::{Arc, mpsc},
    time::Duration,
};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::{AllocAnyThread, DefinedClass, define_class, rc::Retained, runtime::ProtocolObject};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayCopyDisplayMode, CGDisplayMode, CGDisplayPixelsHigh,
    CGDisplayPixelsWide, CGMainDisplayID,
};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput,
    SCStreamOutputType,
};

use super::{DesktopConfig, Format, Shared, State, unpad_rows};

/// How long to wait for ScreenCaptureKit's asynchronous calls. Listing what is shareable and
/// starting the stream both go out to the window server, and on a machine where Screen
/// Recording has never been granted they do not come back until the dialog is dismissed.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// How often the holding thread wakes to notice it has been asked to stop.
const STOP_POLL: Duration = Duration::from_millis(100);

pub(crate) fn spawn(config: DesktopConfig, shared: Arc<Shared>) -> Result<(), String> {
    std::thread::Builder::new()
        .name("desktop-sck".into())
        .spawn(move || {
            let session = match start(&config, shared.clone()) {
                Ok(session) => session,
                Err(why) => {
                    tracing::warn!("bevy_iroh: screen: {why}");
                    shared.finish(State::Failed(why));
                    return;
                }
            };
            // Nothing to do but wait: frames go from ScreenCaptureKit's queue into `shared`.
            // This loop gives `session` an owner, and its `Drop` stops the stream.
            while !shared.stopped() {
                std::thread::sleep(STOP_POLL);
            }
            drop(session);
            shared.finish(State::Ended);
        })
        .map_err(|e| format!("could not start the capture thread: {e}"))?;
    Ok(())
}

/// The desktop's size in backing pixels.
///
/// `CGDisplayPixelsWide`/`High` report the desktop in *points* and follow rotation, which is
/// the geometry wanted; on a Retina display a point is two pixels, and asking for the point
/// size would get a half-resolution capture. The display mode's pixel and point widths give
/// the scale between them, and that is all that is taken from it.
fn backing_size(display_id: CGDirectDisplayID) -> (u32, u32) {
    let points_wide = CGDisplayPixelsWide(display_id);
    let points_high = CGDisplayPixelsHigh(display_id);
    let mode = CGDisplayCopyDisplayMode(display_id);
    let scale = mode
        .as_deref()
        .map(|mode| {
            let mode_points = CGDisplayMode::width(Some(mode));
            let mode_pixels = CGDisplayMode::pixel_width(Some(mode));
            if mode_points == 0 || mode_pixels == 0 {
                1
            } else {
                (mode_pixels / mode_points).max(1)
            }
        })
        .unwrap_or(1);
    ((points_wide * scale) as u32, (points_high * scale) as u32)
}

/// The `SCDisplay` list, which only a completion handler can produce. Blocks on the capture
/// thread; ScreenCaptureKit answers on its own queue rather than the main one.
fn shareable_displays() -> Result<Retained<SCShareableContent>, String> {
    /// One `+1` reference in flight between the handler's thread and this one. Objective-C
    /// reference counts are atomic, so moving ownership of a retain between threads is sound.
    struct Owned(*mut SCShareableContent);
    // SAFETY: `Owned` carries sole ownership of a retain that the sending thread gives up.
    unsafe impl Send for Owned {}

    let (sender, receiver) = mpsc::channel();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: ScreenCaptureKit passes exactly one of these as non-null, and each is a
            // valid object of its type for the length of this call.
            let message = if content.is_null() {
                let why = unsafe { error.as_ref() }
                    .map(|error| error.localizedDescription().to_string())
                    .unwrap_or_else(|| {
                        "ScreenCaptureKit returned neither content nor an error".into()
                    });
                Err(why)
            } else {
                let retained = unsafe { Retained::retain(content) };
                Ok(Owned(
                    retained.map_or(std::ptr::null_mut(), Retained::into_raw),
                ))
            };
            let _ = sender.send(message);
        },
    );
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };

    match receiver.recv_timeout(HANDSHAKE_TIMEOUT) {
        // SAFETY: `Owned` holds the `+1` the handler took, consumed exactly here.
        Ok(Ok(Owned(content))) => unsafe { Retained::from_raw(content) }
            .ok_or_else(|| "ScreenCaptureKit handed back a null content list".to_string()),
        Ok(Err(why)) => Err(format!(
            "ScreenCaptureKit would not list what is shareable: {why}. This is what a machine \
             without Screen Recording permission reports: allow the app in System Settings > \
             Privacy & Security > Screen & System Audio Recording, then relaunch it"
        )),
        Err(_) => Err(format!(
            "ScreenCaptureKit did not answer within {}s; if a permission dialog is up, it is \
             waiting for it",
            HANDSHAKE_TIMEOUT.as_secs()
        )),
    }
}

/// The frames' destination, held by the delegate.
struct Sink {
    shared: Arc<Shared>,
}

define_class!(
    // SAFETY:
    // - `NSObject` has no subclassing requirements.
    // - `StreamSink` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    // Built on the capture thread and called back on a dispatch queue; the only state it
    // touches is the `Shared`, which is built for exactly that.
    #[thread_kind = AllocAnyThread]
    #[name = "BevyIrohScreenSink"]
    #[ivars = Sink]
    struct StreamSink;

    unsafe impl NSObjectProtocol for StreamSink {}

    unsafe impl SCStreamOutput for StreamSink {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            // A sample with no image buffer is how ScreenCaptureKit says "nothing changed":
            // it keeps ticking at the frame interval, and idle ticks carry no pixels.
            let Some(pixels) = (unsafe { sample.image_buffer() }) else {
                return;
            };
            self.ivars().publish(&pixels);
        }
    }
);

impl StreamSink {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Sink { shared });
        unsafe { objc2::msg_send![super(this), init] }
    }
}

impl Sink {
    /// Locks the frame, copies its rows out unpadded, and publishes it.
    fn publish(&self, pixels: &CVPixelBuffer) {
        // SAFETY: `pixels` is the sample's own image buffer, alive for the length of the
        // delegate call, and every path out of this function unlocks it exactly once.
        if unsafe { CVPixelBufferLockBaseAddress(pixels, CVPixelBufferLockFlags::ReadOnly) } != 0 {
            return;
        }
        let width = CVPixelBufferGetWidth(pixels);
        let height = CVPixelBufferGetHeight(pixels);
        let pitch = CVPixelBufferGetBytesPerRow(pixels);
        let base = CVPixelBufferGetBaseAddress(pixels);
        if !base.is_null() && width > 0 && height > 0 && pitch >= width * 4 {
            let mut out = self.shared.buffer(width * height * 4);
            // SAFETY: the lock succeeded, so `base` points at `pitch * height` readable bytes
            // that stay mapped until the unlock below.
            let src = unsafe { std::slice::from_raw_parts(base as *const u8, pitch * height) };
            unpad_rows(src, width as u32, height as u32, pitch, &mut out);
            self.shared
                .publish(width as u32, height as u32, Format::Bgra, out);
        }
        // SAFETY: paired with the successful lock above, with the same flags.
        unsafe { CVPixelBufferUnlockBaseAddress(pixels, CVPixelBufferLockFlags::ReadOnly) };
    }
}

/// A running capture. Every field is held for its lifetime: releasing the queue or the
/// delegate while the stream runs would pull them out from under ScreenCaptureKit.
struct Session {
    stream: Retained<SCStream>,
    _sink: Retained<StreamSink>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
}

// SAFETY: the `Session` is built on the capture thread and never touched from another. The
// one thing shared is the `Shared` the delegate holds, which is `Sync` on its own terms.
unsafe impl Send for Session {}

impl Drop for Session {
    /// Stops the stream and waits for the framework to confirm, so the delegate is not
    /// released with a frame still in flight on the delivery queue.
    fn drop(&mut self) {
        let (sender, receiver) = mpsc::channel();
        let handler = RcBlock::new(move |_error: *mut NSError| {
            let _ = sender.send(());
        });
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&handler)) };
        let _ = receiver.recv_timeout(HANDSHAKE_TIMEOUT);
    }
}

/// Builds the stream for the main display and starts it delivering into `shared`.
fn start(config: &DesktopConfig, shared: Arc<Shared>) -> Result<Session, String> {
    let display_id = CGMainDisplayID();
    let (width, height) = backing_size(display_id);
    if width == 0 || height == 0 {
        return Err("the main display reports a zero-sized desktop".into());
    }

    let content = shareable_displays()?;
    let displays = unsafe { content.displays() };
    let display = displays
        .iter()
        .find(|display| unsafe { display.displayID() } == display_id)
        .ok_or_else(|| "ScreenCaptureKit does not offer the main display".to_string())?;

    // Nothing excluded: the whole monitor.
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &display,
            &NSArray::new(),
        )
    };
    let configuration = unsafe { SCStreamConfiguration::new() };
    unsafe {
        configuration.setWidth(width as usize);
        configuration.setHeight(height as usize);
        configuration.setPixelFormat(kCVPixelFormatType_32BGRA);
        configuration.setShowsCursor(config.cursor);
        // Two frames of slack: how many pixel buffers the window server may have outstanding.
        // Deeper buys tolerance for a slow delegate at the cost of latency, and the delegate
        // here does one memcpy.
        configuration.setQueueDepth(2);
    }

    let sink = StreamSink::new(shared);
    // Serial: frames must be published in order, since `Shared` keeps the newest.
    let queue = DispatchQueue::new("bevy_iroh.screen", DispatchQueueAttr::SERIAL);
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &configuration,
            None,
        )
    };
    unsafe {
        stream.addStreamOutput_type_sampleHandlerQueue_error(
            ProtocolObject::from_ref(&*sink),
            SCStreamOutputType::Screen,
            Some(&queue),
        )
    }
    .map_err(|why| {
        format!(
            "could not attach to the stream: {}",
            why.localizedDescription()
        )
    })?;

    let (sender, receiver) = mpsc::channel();
    let handler = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: null means success; otherwise it is a valid `NSError` for this call.
        let message =
            unsafe { error.as_ref() }.map(|error| error.localizedDescription().to_string());
        let _ = sender.send(message);
    });
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };
    match receiver.recv_timeout(HANDSHAKE_TIMEOUT) {
        Ok(None) => {}
        Ok(Some(why)) => {
            return Err(format!(
                "ScreenCaptureKit would not start capturing: {why}. If this is a permission \
                 refusal, allow the app in System Settings > Privacy & Security > Screen & \
                 System Audio Recording and relaunch it"
            ));
        }
        Err(_) => {
            return Err(format!(
                "ScreenCaptureKit did not start within {}s",
                HANDSHAKE_TIMEOUT.as_secs()
            ));
        }
    }
    tracing::info!("bevy_iroh: screen: sharing the main display at {width}x{height}");
    Ok(Session {
        stream,
        _sink: sink,
        _queue: queue,
    })
}
