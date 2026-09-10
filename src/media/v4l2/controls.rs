//! Camera enumeration and V4L2 controls (brightness, focus, zoom, ...).
//!
//! Controls are walked with `V4L2_CTRL_FLAG_NEXT_CTRL`, so a camera shows exactly what it has:
//! a BRIO exposes about seventeen, a cheap webcam three.

use std::{
    ffi::c_void,
    io,
    path::{Path, PathBuf},
};

use v4l::{
    Device,
    control::{Control, Type, Value},
    v4l_sys::v4l2_capability,
    v4l2,
};

/// Common control ids, as V4L2 numbers them.
pub mod cid {
    pub const BRIGHTNESS: u32 = 0x0098_0900;
    pub const CONTRAST: u32 = 0x0098_0901;
    pub const SATURATION: u32 = 0x0098_0902;
    pub const WHITE_BALANCE_AUTO: u32 = 0x0098_090c;
    pub const GAIN: u32 = 0x0098_0913;
    pub const POWER_LINE_FREQUENCY: u32 = 0x0098_0918;
    pub const WHITE_BALANCE_TEMPERATURE: u32 = 0x0098_091a;
    pub const SHARPNESS: u32 = 0x0098_091b;
    pub const BACKLIGHT_COMPENSATION: u32 = 0x0098_091c;
    pub const EXPOSURE_AUTO: u32 = 0x009a_0901;
    pub const EXPOSURE_ABSOLUTE: u32 = 0x009a_0902;
    pub const PAN_ABSOLUTE: u32 = 0x009a_0908;
    pub const TILT_ABSOLUTE: u32 = 0x009a_0909;
    /// 0 is infinity, the maximum is closest. Ignored by the driver while [`FOCUS_AUTO`] is on.
    pub const FOCUS_ABSOLUTE: u32 = 0x009a_090a;
    /// Continuous autofocus.
    pub const FOCUS_AUTO: u32 = 0x009a_090c;
    pub const ZOOM_ABSOLUTE: u32 = 0x009a_090d;
}

/// A capture device, as `/dev/video*` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub path: PathBuf,
    /// The driver's card name, e.g. "BRIO 4K Stream Edition".
    pub name: String,
    pub driver: String,
    pub bus: String,
}

/// Every node that can capture video. UVC cameras also publish a metadata node beside the
/// capture node; that one is excluded by checking the node's own `device_caps`.
pub fn devices() -> Vec<DeviceInfo> {
    let Ok(entries) = std::fs::read_dir("/dev") else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("video"))
        })
        .collect();
    paths.sort_by_key(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.trim_start_matches("video").parse::<u32>().ok())
            .unwrap_or(u32::MAX)
    });
    paths.iter().filter_map(|p| probe(p).ok()).collect()
}

fn probe(path: &Path) -> io::Result<DeviceInfo> {
    const V4L2_CAP_VIDEO_CAPTURE: u32 = 0x1;
    let device = Device::with_path(path)?;
    let mut caps: v4l2_capability = unsafe { std::mem::zeroed() };
    // SAFETY: valid v4l2_capability for QUERYCAP.
    unsafe {
        v4l2::ioctl(
            device.handle().fd(),
            v4l2::vidioc::VIDIOC_QUERYCAP,
            &mut caps as *mut _ as *mut c_void,
        )?
    };
    if caps.device_caps & V4L2_CAP_VIDEO_CAPTURE == 0 {
        return Err(io::Error::other("not a capture node"));
    }
    Ok(DeviceInfo {
        path: path.to_path_buf(),
        name: cstr(&caps.card),
        driver: cstr(&caps.driver),
        bus: cstr(&caps.bus_info),
    })
}

fn cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// One camera control, as the driver describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub id: u32,
    pub name: String,
    pub min: i64,
    pub max: i64,
    pub step: i64,
    pub default: i64,
    pub boolean: bool,
    /// A menu of named choices; `value` is the index.
    pub menu: Vec<(u32, String)>,
    /// Currently ignored by the driver (focus while autofocus is on) or read-only.
    pub inactive: bool,
}

/// Every integer, boolean and menu control the camera exposes, in driver order.
pub fn settings(path: impl AsRef<Path>) -> io::Result<Vec<Setting>> {
    let device = Device::with_path(path)?;
    Ok(device
        .query_controls()?
        .into_iter()
        .filter(|d| {
            matches!(
                d.typ,
                Type::Integer | Type::Boolean | Type::Menu | Type::IntegerMenu
            )
        })
        .map(|d| Setting {
            id: d.id,
            name: d.name.clone(),
            min: d.minimum,
            max: d.maximum,
            step: (d.step as i64).max(1),
            default: d.default,
            boolean: d.typ == Type::Boolean,
            menu: d
                .items
                .as_ref()
                .map(|items| items.iter().map(|(i, m)| (*i, m.to_string())).collect())
                .unwrap_or_default(),
            inactive: d
                .flags
                .intersects(v4l::control::Flags::INACTIVE | v4l::control::Flags::READ_ONLY),
        })
        .collect())
}

/// Reads one control.
pub fn get_setting(path: impl AsRef<Path>, id: u32) -> io::Result<i64> {
    let device = Device::with_path(path)?;
    match device.control(id)?.value {
        Value::Integer(v) => Ok(v),
        Value::Boolean(b) => Ok(b as i64),
        other => Err(io::Error::other(format!(
            "control {id:#x} has a non-scalar value {other:?}"
        ))),
    }
}

/// Writes one control, clamped to its range and snapped to its step. Returns what was written.
pub fn set_setting(path: impl AsRef<Path>, setting: &Setting, value: i64) -> io::Result<i64> {
    let clamped = value.clamp(setting.min, setting.max);
    let stepped = if setting.step > 1 {
        setting.min + ((clamped - setting.min) / setting.step) * setting.step
    } else {
        clamped
    };
    let device = Device::with_path(path)?;
    let value = if setting.boolean {
        Value::Boolean(stepped != 0)
    } else {
        Value::Integer(stepped)
    };
    device.set_control(Control {
        id: setting.id,
        value,
    })?;
    Ok(stepped)
}

/// Sets manual focus. Continuous autofocus is switched off first, because the driver ignores
/// `focus_absolute` while it is on.
pub fn set_focus(path: impl AsRef<Path>, value: i64) -> io::Result<i64> {
    let path = path.as_ref();
    let all = settings(path)?;
    if let Some(auto) = all.iter().find(|s| s.id == cid::FOCUS_AUTO) {
        set_setting(path, auto, 0)?;
    }
    let focus = all
        .iter()
        .find(|s| s.id == cid::FOCUS_ABSOLUTE)
        .ok_or_else(|| io::Error::other("camera has no focus control"))?;
    set_setting(path, focus, value)
}

/// Finds a control by hex id (`0x009a090a`), exact name (case-insensitive) or name prefix.
pub fn find_setting<'a>(settings: &'a [Setting], query: &str) -> Option<&'a Setting> {
    let by_id = query
        .strip_prefix("0x")
        .and_then(|h| u32::from_str_radix(h, 16).ok());
    let lower = query.to_ascii_lowercase();
    settings
        .iter()
        .find(|s| by_id == Some(s.id))
        .or_else(|| settings.iter().find(|s| s.name.eq_ignore_ascii_case(query)))
        .or_else(|| {
            settings
                .iter()
                .find(|s| s.name.to_ascii_lowercase().starts_with(&lower))
        })
}
