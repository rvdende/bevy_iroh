//! Cameras and microphones as a browser hands them over.
//!
//! `enumerateDevices()` is callable without permission and useless without it: before the
//! person has said yes, the browser returns one placeholder per kind with an empty id and an
//! empty label. So the order is fixed: `getUserMedia` first, which raises the prompt, then
//! `enumerateDevices` for a list with names in it. The probe stream that earned the
//! permission is stopped the moment the list is read.

use std::{cell::RefCell, rc::Rc};

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{MediaDeviceInfo, MediaDeviceKind, MediaStream};

use super::{describe, media_devices, stop_stream};
use crate::media::devices::{AudioDevice, AudioDevices, CameraDevice, CameraDevices, Permission};

#[derive(Default)]
struct Pending {
    audio: Option<Result<(Vec<AudioDevice>, Vec<AudioDevice>), String>>,
    cameras: Option<Result<Vec<CameraDevice>, String>>,
    /// Set by `poll` when it copied something, read once by the system that owns the resource.
    audio_arrived: bool,
    cameras_arrived: bool,
}

pub(crate) fn audio_arrived() -> bool {
    PENDING.with(|p| std::mem::take(&mut p.borrow_mut().audio_arrived))
}

pub(crate) fn cameras_arrived() -> bool {
    PENDING.with(|p| std::mem::take(&mut p.borrow_mut().cameras_arrived))
}

thread_local! {
    static PENDING: Rc<RefCell<Pending>> = Rc::new(RefCell::new(Pending::default()));
}

pub(crate) fn request_audio(devices: &mut AudioDevices) {
    if devices.permission == Permission::Asking {
        return;
    }
    devices.permission = Permission::Asking;
    let pending = PENDING.with(|p| p.clone());
    wasm_bindgen_futures::spawn_local(async move {
        let result = probe_audio().await;
        pending.borrow_mut().audio = Some(result);
    });
}

pub(crate) fn request_cameras(devices: &mut CameraDevices) {
    if devices.permission == Permission::Asking {
        return;
    }
    devices.permission = Permission::Asking;
    let pending = PENDING.with(|p| p.clone());
    wasm_bindgen_futures::spawn_local(async move {
        let result = probe_cameras().await;
        pending.borrow_mut().cameras = Some(result);
    });
}

/// Copies answers that have arrived into the resources.
pub(crate) fn poll(audio: &mut AudioDevices, cameras: &mut CameraDevices) {
    let (a, c) = PENDING.with(|p| {
        let mut p = p.borrow_mut();
        (p.audio.take(), p.cameras.take())
    });
    if let Some(result) = a {
        PENDING.with(|p| p.borrow_mut().audio_arrived = true);
        match result {
            Ok((microphones, speakers)) => {
                bevy::log::info!(
                    "bevy_iroh: {} microphone(s), {} speaker(s)",
                    microphones.len(),
                    speakers.len()
                );
                audio.microphones = microphones;
                audio.speakers = speakers;
                audio.permission = Permission::Ready;
            }
            Err(why) => {
                bevy::log::warn!("bevy_iroh: no microphone access: {why}");
                audio.permission = Permission::Denied(why);
            }
        }
    }
    if let Some(result) = c {
        PENDING.with(|p| p.borrow_mut().cameras_arrived = true);
        match result {
            Ok(found) => {
                bevy::log::info!("bevy_iroh: {} camera(s)", found.len());
                cameras.cameras = found;
                cameras.permission = Permission::Ready;
            }
            Err(why) => {
                bevy::log::warn!("bevy_iroh: no camera access: {why}");
                cameras.permission = Permission::Denied(why);
            }
        }
    }
}

async fn granted(audio: bool) -> Result<MediaStream, String> {
    let devices = media_devices()?;
    let constraints = web_sys::MediaStreamConstraints::new();
    if audio {
        constraints.set_audio(&JsValue::TRUE);
    } else {
        constraints.set_video(&JsValue::TRUE);
    }
    let promise = devices
        .get_user_media_with_constraints(&constraints)
        .map_err(describe)?;
    Ok(JsFuture::from(promise)
        .await
        .map_err(describe)?
        .unchecked_into())
}

async fn listed() -> Result<Vec<MediaDeviceInfo>, String> {
    let devices = media_devices()?;
    let list = JsFuture::from(devices.enumerate_devices().map_err(describe)?)
        .await
        .map_err(describe)?;
    Ok(js_sys::Array::from(&list)
        .iter()
        .filter_map(|entry| entry.dyn_into::<MediaDeviceInfo>().ok())
        // An entry with no id is the placeholder shown without permission.
        .filter(|info| !info.device_id().is_empty())
        .collect())
}

fn label(info: &MediaDeviceInfo, what: &str) -> String {
    if info.label().is_empty() {
        let id = info.device_id();
        format!("{what} {}", &id[..8.min(id.len())])
    } else {
        info.label()
    }
}

async fn probe_audio() -> Result<(Vec<AudioDevice>, Vec<AudioDevice>), String> {
    let probe = granted(true).await?;
    let listed = listed().await;
    // Released only after the list has been read: on some browsers the labels go blank
    // again the moment the last live track of that kind ends.
    stop_stream(&probe);
    let listed = listed?;
    let audio = |info: &MediaDeviceInfo, what: &str| AudioDevice {
        id: info.device_id(),
        name: label(info, what),
        is_default: info.device_id() == "default",
        // Web Audio runs at 48 kHz here whatever the device does.
        sample_rate: Some(48_000),
        channels: None,
    };
    let microphones = listed
        .iter()
        .filter(|i| i.kind() == MediaDeviceKind::Audioinput)
        .map(|i| audio(i, "microphone"))
        .collect();
    let speakers = listed
        .iter()
        .filter(|i| i.kind() == MediaDeviceKind::Audiooutput)
        .map(|i| audio(i, "speaker"))
        .collect();
    Ok((microphones, speakers))
}

async fn probe_cameras() -> Result<Vec<CameraDevice>, String> {
    let probe = granted(false).await?;
    let listed = listed().await;
    stop_stream(&probe);
    let listed = listed?;
    let mut cameras: Vec<CameraDevice> = listed
        .iter()
        .filter(|i| i.kind() == MediaDeviceKind::Videoinput)
        .map(|i| CameraDevice {
            id: i.device_id(),
            name: label(i, "camera"),
            is_default: false,
            detail: "getUserMedia".into(),
        })
        .collect();
    // Video has no synthetic `default` entry the way audio does: the first camera is the one
    // the browser would have picked.
    if let Some(first) = cameras.first_mut() {
        first.is_default = true;
    }
    Ok(cameras)
}
