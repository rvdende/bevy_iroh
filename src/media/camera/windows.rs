//! Media Foundation: enumerate the cameras, say what each delivers, and read one.
//!
//! `IMFSourceReader` owns its buffer pool and hands out a locked buffer per sample, so the
//! read loop is one blocking call, like V4L2's `DQBUF` without the queue bookkeeping. The
//! reader is opened with `MF_READWRITE_DISABLE_CONVERTERS`, which is load-bearing: left off,
//! Media Foundation inserts a decoder to produce whatever is asked for, so a camera that only
//! offers MJPEG would be silently transcoded on the CPU. Off, only the layouts the camera
//! produces itself are listed, and they are `YUY2` and `NV12`.
//!
//! Cameras are named by their symbolic link, an opaque string that goes straight back in.

use std::sync::Once;

use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaSource, IMFMediaType, IMFSourceReader,
    MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_SUBTYPE, MF_READWRITE_DISABLE_CONVERTERS, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_VERSION, MFCreateAttributes, MFCreateDeviceSource, MFCreateSourceReaderFromMediaSource,
    MFEnumDeviceSources, MFSTARTUP_NOSOCKET, MFStartup,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{GUID, PCWSTR, PWSTR};

use super::{Device, Format, Layout, Wait};

/// Media Foundation has to be started once per process. Never shut down: `MFShutdown` would
/// have to come after every stream and enumeration, and getting that wrong deadlocks on exit.
fn ensure_started() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        if let Err(why) = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) {
            tracing::error!("bevy_iroh: Media Foundation would not start: {why}");
        }
    });
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Reads one of the string attributes off a device.
unsafe fn allocated_string(activate: &IMFActivate, key: &GUID) -> Option<String> {
    let mut buffer = PWSTR::null();
    let mut len: u32 = 0;
    unsafe { activate.GetAllocatedString(key, &mut buffer, &mut len) }.ok()?;
    if buffer.is_null() {
        return None;
    }
    let text = unsafe { std::slice::from_raw_parts(buffer.0, len as usize) };
    let text = String::from_utf16_lossy(text);
    unsafe { CoTaskMemFree(Some(buffer.0 as *const _)) };
    Some(text)
}

/// Every camera the system currently has.
pub fn devices() -> Vec<Device> {
    ensure_started();
    let mut found = Vec::new();
    unsafe {
        let mut attributes: Option<IMFAttributes> = None;
        if MFCreateAttributes(&mut attributes, 1).is_err() {
            return found;
        }
        let Some(attributes) = attributes else {
            return found;
        };
        if attributes
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .is_err()
        {
            return found;
        }
        let mut sources: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        if MFEnumDeviceSources(&attributes, &mut sources, &mut count).is_err() || sources.is_null()
        {
            return found;
        }
        // `MFEnumDeviceSources` hands over an array it allocated with `CoTaskMemAlloc`, and
        // one reference on each activation object in it. Each is read out (and so released
        // when it drops), then the array itself is freed.
        for i in 0..count as usize {
            let Some(activate) = std::ptr::read(sources.add(i)) else {
                continue;
            };
            // Without the symbolic link there is no way to reopen it.
            let Some(id) = allocated_string(
                &activate,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            ) else {
                continue;
            };
            let name = allocated_string(&activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
                .unwrap_or_else(|| "Camera".to_string());
            found.push(Device { id, name });
        }
        CoTaskMemFree(Some(sources as *const _));
    }
    found
}

/// Opens the media source named by a symbolic link, wrapped in a source reader.
unsafe fn open_reader(id: &str) -> Result<IMFSourceReader, String> {
    ensure_started();
    let link = wide(id);
    unsafe {
        let mut attributes: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attributes, 2).map_err(|e| e.to_string())?;
        let attributes = attributes.ok_or("no attributes")?;
        attributes
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .map_err(|e| e.to_string())?;
        attributes
            .SetString(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                PCWSTR(link.as_ptr()),
            )
            .map_err(|e| e.to_string())?;
        let source: IMFMediaSource =
            MFCreateDeviceSource(&attributes).map_err(|e| format!("no camera at {id}: {e}"))?;
        let mut reader_attributes: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut reader_attributes, 1).map_err(|e| e.to_string())?;
        let reader_attributes = reader_attributes.ok_or("no attributes")?;
        reader_attributes
            .SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1)
            .map_err(|e| e.to_string())?;
        MFCreateSourceReaderFromMediaSource(&source, &reader_attributes).map_err(|e| e.to_string())
    }
}

const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// The modes a camera offers, deduplicated.
pub fn formats(id: &str) -> Result<Vec<Format>, String> {
    let reader = unsafe { open_reader(id)? };
    let mut found: Vec<Format> = Vec::new();
    // Indexed rather than enumerated: the reader answers `MF_E_NO_MORE_TYPES` at the end.
    for index in 0.. {
        let Ok(media_type) = (unsafe { reader.GetNativeMediaType(STREAM, index) }) else {
            break;
        };
        if let Some(format) = describe(&media_type)
            && !found.contains(&format)
        {
            found.push(format);
        }
    }
    Ok(found)
}

/// One media type as a [`Format`], or `None` if it is not a layout read here.
fn describe(media_type: &IMFMediaType) -> Option<Format> {
    unsafe {
        let subtype = media_type.GetGUID(&MF_MT_SUBTYPE).ok()?;
        // A video subtype GUID is `{fourcc-0000-0010-8000-00AA00389B71}`: its first field is
        // the fourcc.
        let layout = match &subtype.data1.to_le_bytes() {
            b"YUY2" => Layout::Yuyv,
            b"NV12" => Layout::Nv12,
            _ => return None,
        };
        // Both are two `u32`s packed into a `u64`, high word first.
        let size = media_type.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
        let width = (size >> 32) as u32;
        let height = (size & 0xffff_ffff) as u32;
        let fps = match media_type.GetUINT64(&MF_MT_FRAME_RATE) {
            Ok(rate) => {
                let numerator = (rate >> 32) as u32;
                let denominator = (rate & 0xffff_ffff) as u32;
                if denominator == 0 {
                    0
                } else {
                    (numerator as f64 / denominator as f64).round() as u32
                }
            }
            Err(_) => 0,
        };
        (width > 0 && height > 0).then_some(Format {
            width,
            height,
            fps,
            layout,
        })
    }
}

/// A running capture.
pub struct Stream {
    reader: IMFSourceReader,
    format: Format,
    latest: Vec<u8>,
}

// SAFETY: the reader is used only from the thread that owns the `Stream`, which is moved to
// the encoder thread once and never shared.
unsafe impl Send for Stream {}

impl Stream {
    /// Opens a camera at one of the modes it offered.
    pub fn open(id: &str, wanted: Format) -> Result<Self, String> {
        let reader = unsafe { open_reader(id)? };
        // The camera's own media type, selected rather than built by hand, so the camera is
        // asked for something it said it can do.
        let mut chosen = None;
        for index in 0.. {
            let Ok(media_type) = (unsafe { reader.GetNativeMediaType(STREAM, index) }) else {
                break;
            };
            if describe(&media_type) == Some(wanted) {
                chosen = Some(media_type);
                break;
            }
        }
        let chosen = chosen.ok_or_else(|| {
            format!(
                "{id} does not offer {}x{} {:?} at {} fps",
                wanted.width, wanted.height, wanted.layout, wanted.fps
            )
        })?;
        unsafe { reader.SetCurrentMediaType(STREAM, None, &chosen) }
            .map_err(|e| format!("set format: {e}"))?;
        Ok(Self {
            reader,
            format: wanted,
            latest: Vec::new(),
        })
    }

    fn frame_len(&self) -> usize {
        let (w, h) = (self.format.width as usize, self.format.height as usize);
        match self.format.layout {
            Layout::Yuyv => w * h * 2,
            Layout::Nv12 => w * h * 3 / 2,
        }
    }

    /// Blocks for the next frame and hands its bytes to `consume`.
    pub fn next_frame(&mut self, consume: impl FnOnce(&[u8])) -> Result<(), Wait> {
        // A read can legitimately return no sample (a stream tick, a gap); a few are retried
        // before this counts as a wait.
        for _ in 0..8 {
            let mut flags = 0u32;
            let mut timestamp = 0i64;
            let mut sample = None;
            unsafe {
                self.reader.ReadSample(
                    STREAM,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut timestamp),
                    Some(&mut sample),
                )
            }
            .map_err(|e| Wait::Ended(format!("read: {e}")))?;
            const MF_SOURCE_READERF_ENDOFSTREAM: u32 = 0x0000_0002;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM != 0 {
                return Err(Wait::Ended("the camera ended its stream".into()));
            }
            let Some(sample) = sample else {
                continue;
            };
            // One flat run of bytes, whatever the sample was made of.
            let buffer = unsafe { sample.ConvertToContiguousBuffer() }
                .map_err(|e| Wait::Ended(format!("buffer: {e}")))?;
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut length: u32 = 0;
            unsafe { buffer.Lock(&mut data, None, Some(&mut length)) }
                .map_err(|e| Wait::Ended(format!("lock: {e}")))?;
            self.latest.clear();
            // SAFETY: `Lock` succeeded, so `data` points at `length` readable bytes until
            // `Unlock`.
            self.latest
                .extend_from_slice(unsafe { std::slice::from_raw_parts(data, length as usize) });
            // Unlocked before `consume` runs: the buffer belongs to the reader's pool.
            let _ = unsafe { buffer.Unlock() };
            if self.latest.len() < self.frame_len() {
                continue;
            }
            consume(&self.latest);
            return Ok(());
        }
        Err(Wait::Timeout)
    }
}
