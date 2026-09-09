//! The machine's microphones, speakers and cameras, as lists an app can put in front of a
//! person.
//!
//! Devices are named by an `id` string that is stable enough to persist: cpal's device id on
//! a desktop, `deviceId` in a browser, `/dev/videoN` for a camera on Linux. Choose one by
//! putting its id in [`MediaSettings`](super::MediaSettings); the crate reopens what needs
//! reopening and the entities publishing carry on with the same track ids.
//!
//! A browser cannot list devices until the person has said yes to a permission prompt, so
//! there the lists start empty and [`AudioDevices::request`] / [`CameraDevices::request`]
//! raise the prompt; on a desktop those are no-ops and the lists are filled at startup.

use bevy::prelude::*;

/// One capture or playback device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    /// What to put in `MediaSettings` to use it. Persistable.
    pub id: String,
    /// What the host calls it.
    pub name: String,
    /// The one the host would pick if not asked.
    pub is_default: bool,
    /// The rate it would open at, or `None` if it will not say, which in practice means it
    /// cannot be opened. Listed anyway: an empty list reads as a hardware fault.
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
}

impl AudioDevice {
    pub fn usable(&self) -> bool {
        self.sample_rate.is_some()
    }

    /// A second line for a row: `48000 Hz · stereo`.
    pub fn detail(&self) -> String {
        match (self.sample_rate, self.channels) {
            (Some(rate), Some(channels)) => format!(
                "{rate} Hz · {}",
                match channels {
                    1 => "mono".to_string(),
                    2 => "stereo".to_string(),
                    n => format!("{n} channels"),
                }
            ),
            (Some(rate), None) => format!("{rate} Hz"),
            _ => "cannot be opened".to_string(),
        }
    }
}

/// One camera.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraDevice {
    /// What to put in `MediaSettings::camera`. A device path on Linux, `deviceId` in a page.
    pub id: String,
    pub name: String,
    pub is_default: bool,
    /// The mode it would open at, as text, when known.
    pub detail: String,
}

/// How far a browser's permission prompt has got. Always `Ready` on a desktop.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Permission {
    /// Nobody has asked. Lists are empty in a page, filled on a desktop.
    #[default]
    Idle,
    /// The prompt may be on screen.
    Asking,
    Ready,
    /// Refused, or no such hardware. Not retried: a browser told "block" answers instantly
    /// and forever.
    Denied(String),
}

/// Every microphone and speaker the machine will name. Filled at startup on a desktop; in a
/// page, after [`AudioDevices::request`] and the prompt it raises.
#[derive(Resource, Debug, Default)]
pub struct AudioDevices {
    pub microphones: Vec<AudioDevice>,
    pub speakers: Vec<AudioDevice>,
    pub permission: Permission,
    pub(crate) scan_wanted: bool,
}

impl AudioDevices {
    /// Scan again: a headset was plugged in. Also what a page calls to raise the prompt.
    pub fn rescan(&mut self) {
        self.scan_wanted = true;
    }

    /// Ask the browser for microphone access, which is what fills the list there. A no-op
    /// on a desktop. Never called by the crate on its own: a prompt that appears because an
    /// app loaded, rather than because someone reached for a microphone, gets denied for good.
    pub fn request(&mut self) {
        if self.permission == Permission::Idle {
            self.scan_wanted = true;
        }
    }

    pub fn microphone(&self, id: &str) -> Option<&AudioDevice> {
        self.microphones.iter().find(|d| d.id == id)
    }

    pub fn speaker(&self, id: &str) -> Option<&AudioDevice> {
        self.speakers.iter().find(|d| d.id == id)
    }
}

/// Every camera the machine will name. Linux (`v4l2` feature) and browsers.
#[derive(Resource, Debug, Default)]
pub struct CameraDevices {
    pub cameras: Vec<CameraDevice>,
    pub permission: Permission,
    pub(crate) scan_wanted: bool,
}

impl CameraDevices {
    pub fn rescan(&mut self) {
        self.scan_wanted = true;
    }

    /// Ask the browser for camera access. A no-op on a desktop.
    pub fn request(&mut self) {
        if self.permission == Permission::Idle {
            self.scan_wanted = true;
        }
    }

    pub fn camera(&self, id: &str) -> Option<&CameraDevice> {
        self.cameras.iter().find(|d| d.id == id)
    }
}

/// Fill the audio lists when asked to.
pub(crate) fn scan_audio(mut devices: ResMut<AudioDevices>) {
    if !devices.scan_wanted {
        return;
    }
    devices.scan_wanted = false;
    #[cfg(not(target_arch = "wasm32"))]
    {
        devices.microphones = super::native::microphones();
        devices.speakers = super::native::speakers();
        devices.permission = Permission::Ready;
        info!(
            "bevy_iroh: {} microphone(s), {} speaker(s)",
            devices.microphones.len(),
            devices.speakers.len()
        );
    }
    #[cfg(target_arch = "wasm32")]
    super::web::devices::request_audio(&mut devices);
}

/// Fill the camera list when asked to.
pub(crate) fn scan_cameras(mut devices: ResMut<CameraDevices>) {
    if !devices.scan_wanted {
        return;
    }
    devices.scan_wanted = false;
    #[cfg(all(feature = "v4l2", target_os = "linux"))]
    {
        devices.cameras = super::v4l2::cameras();
        devices.permission = Permission::Ready;
        info!("bevy_iroh: {} camera(s)", devices.cameras.len());
    }
    #[cfg(target_arch = "wasm32")]
    super::web::devices::request_cameras(&mut devices);
    #[cfg(not(any(all(feature = "v4l2", target_os = "linux"), target_arch = "wasm32")))]
    {
        devices.permission = Permission::Ready;
    }
}

/// On a page the lists fill asynchronously; this copies what has arrived. The resources are
/// only touched when there is something to copy, so change detection stays honest.
#[cfg(target_arch = "wasm32")]
pub(crate) fn poll_web_devices(
    mut audio: ResMut<AudioDevices>,
    mut cameras: ResMut<CameraDevices>,
) {
    super::web::devices::poll(
        audio.bypass_change_detection(),
        cameras.bypass_change_detection(),
    );
    if super::web::devices::audio_arrived() {
        audio.set_changed();
    }
    if super::web::devices::cameras_arrived() {
        cameras.set_changed();
    }
}
