//! Windows: Desktop Duplication delivers the primary output.
//!
//! One blocking call per frame, straight from the display driver, and the API remote-desktop
//! software is built on. `AcquireNextFrame` blocks until the desktop changes and hands back a
//! GPU texture that must be *released* promptly: the duplication holds at most one acquired
//! frame, and the desktop stops updating for everyone until it is given back. So each frame is
//! copied to a staging texture, released, and only then mapped and published. When nothing on
//! screen changes the call times out, which is the idle case and costs nothing.
//!
//! Desktop Duplication hands back the *panel's* image: a monitor turned on its side arrives in
//! the panel's landscape orientation, so the pixels are turned here before they are published
//! and a peer sees the desktop the way the person does. It does not draw the pointer, so
//! [`DesktopConfig::cursor`] has no effect on this platform. There is no picker; the first
//! output of the first adapter is what is shared. A lost session (a mode change, a GPU reset,
//! the secure desktop for a UAC prompt) is re-acquired rather than ended.

use std::sync::Arc;

use windows::Win32::Foundation::{E_ACCESSDENIED, HMODULE};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE90,
    DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTPUT_DESC, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use super::{DesktopConfig, Format, Shared, State, unpad_rows};

/// How long `AcquireNextFrame` waits for the desktop to change before the loop goes round
/// again. Not a latency knob: a frame is delivered the moment it arrives. It only decides how
/// often an idle desktop wakes the thread to notice it has been asked to stop.
const ACQUIRE_TIMEOUT_MS: u32 = 100;

pub(crate) fn spawn(config: DesktopConfig, shared: Arc<Shared>) -> Result<(), String> {
    if config.cursor {
        tracing::debug!("bevy_iroh: screen: Desktop Duplication does not draw the pointer");
    }
    std::thread::Builder::new()
        .name("desktop-dxgi".into())
        .spawn(move || {
            let mut duplication = match open(None) {
                Ok(duplication) => duplication,
                Err(why) => {
                    tracing::warn!("bevy_iroh: screen: {why}");
                    shared.finish(State::Failed(why));
                    return;
                }
            };
            let name = duplication.output_name.clone();
            tracing::info!(
                "bevy_iroh: screen: sharing {name} ({}) at {}x{}",
                duplication.adapter_name,
                duplication.width,
                duplication.height
            );
            while !shared.stopped() {
                match duplication.pump(&shared) {
                    Ok(()) => {}
                    Err(Pump::AccessLost) => match open(Some(&name)) {
                        Ok(fresh) => {
                            tracing::info!("bevy_iroh: screen: re-acquired {name}");
                            duplication = fresh;
                        }
                        Err(why) => {
                            let why = format!("could not re-acquire {name}: {why}");
                            tracing::warn!("bevy_iroh: screen: {why}");
                            shared.finish(State::Failed(why));
                            return;
                        }
                    },
                    Err(Pump::Fatal(why)) => {
                        tracing::warn!("bevy_iroh: screen: {why}");
                        shared.finish(State::Failed(why));
                        return;
                    }
                }
            }
            shared.finish(State::Ended);
        })
        .map_err(|e| format!("could not start the capture thread: {e}"))?;
    Ok(())
}

/// How far the duplicated surface has to be turned clockwise to be the desktop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Turn {
    None,
    Cw90,
    Cw180,
    Cw270,
}

/// Compared rather than matched: the DXGI values are associated constants of a newtype.
fn turn_of(rotation: DXGI_MODE_ROTATION) -> Turn {
    if rotation == DXGI_MODE_ROTATION_ROTATE90 {
        Turn::Cw90
    } else if rotation == DXGI_MODE_ROTATION_ROTATE180 {
        Turn::Cw180
    } else if rotation == DXGI_MODE_ROTATION_ROTATE270 {
        Turn::Cw270
    } else {
        Turn::None
    }
}

/// Why one turn of the frame loop stopped short.
enum Pump {
    /// The session died: recoverable by duplicating again.
    AccessLost,
    Fatal(String),
}

/// One output being duplicated, and the D3D11 device doing it.
struct Duplication {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    /// The CPU-readable texture frames are copied into, sized from the first acquired frame
    /// and rebuilt if that ever changes: the acquired texture is the only thing that reliably
    /// says how big the surface is. `CopyResource` between mismatched textures is a silent
    /// no-op, which shows as a black picture at exactly the right resolution.
    staging: Option<ID3D11Texture2D>,
    surface: (u32, u32),
    turn: Turn,
    /// The desktop's size, for the log.
    width: u32,
    height: u32,
    adapter_name: String,
    output_name: String,
}

// SAFETY: every COM interface held here is used only from the capture thread that built the
// `Duplication`; it is moved there once and never shared. The device is free-threaded, the
// immediate context is not, so one owner is the rule.
unsafe impl Send for Duplication {}

fn wide_name(name: &[u16]) -> String {
    String::from_utf16_lossy(name)
        .trim_end_matches('\0')
        .to_string()
}

/// Opens the output named by `wanted` (a DXGI device name such as `\\.\DISPLAY1`), or the
/// first one if it is `None`.
fn open(wanted: Option<&str>) -> Result<Duplication, String> {
    unsafe {
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().map_err(|e| format!("DXGI factory: {e}"))?;
        let mut adapter_index = 0;
        while let Ok(adapter) = factory.EnumAdapters1(adapter_index) {
            adapter_index += 1;
            let adapter_name = adapter
                .GetDesc1()
                .map(|d| wide_name(&d.Description))
                .unwrap_or_default();
            let mut output_index = 0;
            while let Ok(output) = adapter.EnumOutputs(output_index) {
                output_index += 1;
                let Ok(desc) = output.GetDesc() else { continue };
                let name = wide_name(&desc.DeviceName);
                if wanted.is_some_and(|w| w != name) {
                    continue;
                }
                let output1: IDXGIOutput1 = output
                    .cast()
                    .map_err(|e| format!("{name} has no IDXGIOutput1: {e}"))?;
                return start(&adapter, output1, adapter_name, name, desc);
            }
        }
    }
    Err(match wanted {
        Some(name) => format!("no output named {name}"),
        None => "this machine reports no displays to capture".into(),
    })
}

/// Builds the D3D11 device on `adapter` and begins duplicating `output`.
unsafe fn start(
    adapter: &IDXGIAdapter1,
    output: IDXGIOutput1,
    adapter_name: String,
    output_name: String,
    desc: DXGI_OUTPUT_DESC,
) -> Result<Duplication, String> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    // The device must be created on the adapter the output hangs off: duplication fails
    // outright across adapters, which is what a laptop with switchable graphics hits with
    // the default device. `D3D_DRIVER_TYPE_UNKNOWN` is required when an adapter is named.
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|e| format!("D3D11 device: {e}"))?;
    let device = device.ok_or("D3D11CreateDevice returned no device")?;
    let context = context.ok_or("D3D11CreateDevice returned no context")?;

    let dxgi_device: IDXGIDevice = device.cast().map_err(|e| format!("DXGI device: {e}"))?;
    let duplication = unsafe { output.DuplicateOutput(&dxgi_device) }.map_err(|why| {
        if why.code() == E_ACCESSDENIED {
            format!(
                "the display server refused to duplicate {output_name}: something holds it \
                 exclusively (a full-screen game, or a secure desktop such as the UAC prompt)"
            )
        } else {
            format!("could not duplicate {output_name}: {why}")
        }
    })?;

    let bounds = desc.DesktopCoordinates;
    let width = (bounds.right - bounds.left).max(0) as u32;
    let height = (bounds.bottom - bounds.top).max(0) as u32;
    let turn = turn_of(desc.Rotation);
    if turn != Turn::None {
        tracing::info!(
            "bevy_iroh: screen: {output_name} is rotated ({turn:?}); frames are turned to match"
        );
    }
    Ok(Duplication {
        device,
        context,
        duplication,
        staging: None,
        surface: (0, 0),
        turn,
        width,
        height,
        adapter_name,
        output_name,
    })
}

/// A CPU-readable texture the acquired frame is copied into.
unsafe fn staging_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        // What the desktop is already in, so the copy is a straight blit.
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|e| format!("staging texture: {e}"))?;
    texture.ok_or_else(|| "CreateTexture2D returned no staging texture".into())
}

impl Duplication {
    /// Waits for the next frame and publishes it; returns without one when the desktop did
    /// not change within the timeout. The acquired frame is released before the pixels are
    /// read, because holding it blocks the desktop for every process.
    fn pump(&mut self, shared: &Shared) -> Result<(), Pump> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe {
            self.duplication
                .AcquireNextFrame(ACQUIRE_TIMEOUT_MS, &mut info, &mut resource)
        } {
            Ok(()) => {}
            Err(why) if why.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(()),
            Err(why) if why.code() == DXGI_ERROR_ACCESS_LOST => return Err(Pump::AccessLost),
            Err(why) => return Err(Pump::Fatal(format!("AcquireNextFrame: {why}"))),
        }

        let copied = (|| -> Result<bool, String> {
            // `LastPresentTime` of zero means only the mouse moved. `AccumulatedFrames` is
            // checked alongside it: the first frame after `DuplicateOutput` is the whole
            // desktop and can arrive with a zero present time.
            if info.LastPresentTime == 0 && info.AccumulatedFrames == 0 {
                return Ok(false);
            }
            let resource = resource.ok_or("frame acquired with no resource")?;
            let frame: ID3D11Texture2D = resource
                .cast()
                .map_err(|e| format!("frame is not a texture: {e}"))?;
            let mut frame_desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { frame.GetDesc(&mut frame_desc) };
            let size = (frame_desc.Width, frame_desc.Height);
            if self.staging.is_none() || self.surface != size {
                self.staging = Some(unsafe { staging_texture(&self.device, size.0, size.1) }?);
                self.surface = size;
            }
            let staging = self.staging.as_ref().expect("just ensured");
            unsafe { self.context.CopyResource(staging, &frame) };
            Ok(true)
        })();

        // Released whatever happened above: a frame left acquired freezes the desktop for the
        // whole machine until this process exits.
        let _ = unsafe { self.duplication.ReleaseFrame() };

        match copied {
            Ok(true) => self.publish(shared).map_err(Pump::Fatal),
            Ok(false) => Ok(()),
            Err(why) => Err(Pump::Fatal(why)),
        }
    }

    /// Maps the staging texture and hands its rows to `shared`, unpadded and turned upright.
    fn publish(&mut self, shared: &Shared) -> Result<(), String> {
        let staging = self
            .staging
            .as_ref()
            .ok_or("published before a frame was copied")?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .map_err(|e| format!("map: {e}"))?;
        let (width, height) = self.surface;
        let pitch = mapped.RowPitch as usize;
        let mut data = shared.buffer(width as usize * height as usize * 4);
        // SAFETY: `Map` succeeded, so `pData` points at `RowPitch * height` readable bytes
        // that stay mapped until `Unmap` below.
        let src = unsafe {
            std::slice::from_raw_parts(mapped.pData as *const u8, pitch * height as usize)
        };
        unpad_rows(src, width, height, pitch, &mut data);
        unsafe { self.context.Unmap(staging, 0) };

        let (data, width, height) = match self.turn {
            Turn::None => (data, width, height),
            turn => {
                let mut turned = shared.buffer(data.len());
                let (w, h) = rotate_bgra(&data, width, height, turn, &mut turned);
                shared.recycle(data);
                (turned, w, h)
            }
        };
        shared.publish(width, height, Format::Bgra, data);
        Ok(())
    }
}

/// Turns a packed 4-byte-pixel picture clockwise by `turn`, into `dst`; returns its size.
fn rotate_bgra(src: &[u8], width: u32, height: u32, turn: Turn, dst: &mut Vec<u8>) -> (u32, u32) {
    let (w, h) = (width as usize, height as usize);
    let px = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * w + x) * 4;
        [src[i], src[i + 1], src[i + 2], src[i + 3]]
    };
    dst.clear();
    dst.reserve(w * h * 4);
    match turn {
        Turn::None => {
            dst.extend_from_slice(&src[..w * h * 4]);
            (width, height)
        }
        Turn::Cw180 => {
            for y in (0..h).rev() {
                for x in (0..w).rev() {
                    dst.extend_from_slice(&px(x, y));
                }
            }
            (width, height)
        }
        // A pixel at (x, y) lands at (h - 1 - y, x) in an `h` wide, `w` tall picture.
        Turn::Cw90 => {
            for y2 in 0..w {
                for x2 in 0..h {
                    dst.extend_from_slice(&px(y2, h - 1 - x2));
                }
            }
            (height, width)
        }
        // A pixel at (x, y) lands at (y, w - 1 - x).
        Turn::Cw270 => {
            for y2 in 0..w {
                for x2 in 0..h {
                    dst.extend_from_slice(&px(w - 1 - y2, x2));
                }
            }
            (height, width)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 3x2 picture whose pixels are numbered 0..6 left to right, top to bottom.
    fn picture() -> Vec<u8> {
        (0..6u8).flat_map(|i| [i, i, i, 255]).collect()
    }

    fn pixels(data: &[u8]) -> Vec<u8> {
        data.chunks_exact(4).map(|p| p[0]).collect()
    }

    #[test]
    fn a_quarter_turn_clockwise_swaps_the_axes() {
        let mut out = Vec::new();
        let size = rotate_bgra(&picture(), 3, 2, Turn::Cw90, &mut out);
        assert_eq!(size, (2, 3));
        // 0 1 2      3 0
        // 3 4 5  ->  4 1
        //            5 2
        assert_eq!(pixels(&out), [3, 0, 4, 1, 5, 2]);
    }

    #[test]
    fn a_quarter_turn_anticlockwise_is_the_other_way() {
        let mut out = Vec::new();
        let size = rotate_bgra(&picture(), 3, 2, Turn::Cw270, &mut out);
        assert_eq!(size, (2, 3));
        //            2 5
        //            1 4
        //            0 3
        assert_eq!(pixels(&out), [2, 5, 1, 4, 0, 3]);
    }

    #[test]
    fn a_half_turn_reverses_everything() {
        let mut out = Vec::new();
        assert_eq!(rotate_bgra(&picture(), 3, 2, Turn::Cw180, &mut out), (3, 2));
        assert_eq!(pixels(&out), [5, 4, 3, 2, 1, 0]);
    }

    #[test]
    fn the_dxgi_rotation_values_map_to_turns() {
        assert_eq!(turn_of(DXGI_MODE_ROTATION_ROTATE90), Turn::Cw90);
        assert_eq!(turn_of(DXGI_MODE_ROTATION_ROTATE180), Turn::Cw180);
        assert_eq!(turn_of(DXGI_MODE_ROTATION_ROTATE270), Turn::Cw270);
        assert_eq!(turn_of(DXGI_MODE_ROTATION(0)), Turn::None);
        assert_eq!(turn_of(DXGI_MODE_ROTATION(1)), Turn::None);
    }
}
