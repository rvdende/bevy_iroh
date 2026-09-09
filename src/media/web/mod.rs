//! Media in a browser: Web Audio for the microphone and the speakers, WebCodecs for opus and
//! H.264, `getUserMedia` for the camera. On `wasm32`, with the `media` feature.
//!
//! What is different from a desktop is only where samples and pictures come from and go to.
//! The framing, the jitter buffer, the mixer, the transport and every Bevy-facing type are
//! the shared ones. The JS handles are `!Send`, so they live in thread-locals and the shared
//! types hold only flags; [`sweep`] runs once a frame to take apart what has been stopped
//! and to nudge suspended audio contexts awake, which a browser will not do until the page
//! has been clicked on.

pub mod audio;
pub mod devices;
pub mod video;

use wasm_bindgen::{JsCast, JsValue};

/// Takes apart what has been stopped, and resumes audio contexts once the page has been
/// interacted with. A system, so nothing is freed from inside its own callback.
pub(crate) fn sweep() {
    audio::sweep();
    video::sweep();
}

/// A `JsValue` error as something worth putting in a log line.
pub(crate) fn describe(error: JsValue) -> String {
    error
        .dyn_ref::<js_sys::Error>()
        .map(|error| String::from(error.message()))
        .or_else(|| error.as_string())
        .unwrap_or_else(|| format!("{error:?}"))
}

/// `navigator.mediaDevices`, or why there isn't one: it exists only in a secure context, so a
/// page served over plain http from anything but localhost has no camera or microphone.
pub(crate) fn media_devices() -> Result<web_sys::MediaDevices, String> {
    let window = web_sys::window().ok_or("no window")?;
    window.navigator().media_devices().map_err(|e| {
        format!(
            "navigator.mediaDevices is unavailable ({}): this needs https, or localhost",
            describe(e)
        )
    })
}

/// A constraint naming one exact device. `{ deviceId: { exact } }` rather than a bare id,
/// which is a preference a browser may quietly ignore in favour of a different device.
pub(crate) fn exactly(id: Option<&str>) -> Result<JsValue, String> {
    let Some(id) = id else {
        return Ok(JsValue::TRUE);
    };
    let exact = js_sys::Object::new();
    js_sys::Reflect::set(&exact, &"exact".into(), &id.into()).map_err(describe)?;
    let constraint = js_sys::Object::new();
    js_sys::Reflect::set(&constraint, &"deviceId".into(), &exact).map_err(describe)?;
    Ok(constraint.into())
}

/// Ends every track of a stream, which is what actually turns the camera light off.
pub(crate) fn stop_stream(stream: &web_sys::MediaStream) {
    for track in stream.get_tracks().iter() {
        if let Ok(track) = track.dyn_into::<web_sys::MediaStreamTrack>() {
            track.stop();
        }
    }
}

/// Milliseconds since the page's first look at the clock.
pub(crate) fn now_ms() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<web_time::Instant> = OnceLock::new();
    START
        .get_or_init(web_time::Instant::now)
        .elapsed()
        .as_millis() as u64
}
