//! Microphones and speakers through cpal, on a desktop.
//!
//! What was learned on ALSA and is kept here: a capture stream at the backend's default buffer
//! size hands sound over in tenth-of-a-second slabs, so a small fixed buffer is asked for
//! first; the device list mixes converter plugins in with the hardware and lists one card
//! once per route, so the list is filtered and folded; and a device thread that is not joined
//! is a device still busy when the next open comes a frame later.

use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use cpal::{
    Sample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

use super::{
    audio::{AudioOutput, AudioSource, Mixer, Running},
    devices::AudioDevice,
};

/// The capture and playback buffer to ask for, in frames: about 11 ms at 48 kHz. A device
/// that refuses gets its default, because sound that arrives late beats sound that does not.
const LOW_LATENCY_FRAMES: u32 = 512;

// -- enumeration ---------------------------------------------------------------------------

/// Whether an ALSA pcm id names something a person would call a device. The sound servers by
/// name; real hardware by the `CARD=` every `hw:`-family entry carries; `null` and the raw
/// `usbstream` endpoint are neither. Everything, off ALSA: other backends enumerate endpoints.
fn is_a_device(id: &str) -> bool {
    if !cfg!(target_os = "linux") {
        return true;
    }
    const SERVERS: [&str; 4] = ["default", "pipewire", "pulse", "sysdefault"];
    const NOT_DEVICES: [&str; 2] = ["usbstream", "null"];
    let head = id.split(':').next().unwrap_or(id);
    if NOT_DEVICES.contains(&head) {
        return false;
    }
    SERVERS.contains(&head) || id.contains("CARD=")
}

/// How much we would rather reach a device by this ALSA route. Lower is better: `sysdefault`
/// and `plughw` convert, `dsnoop`/`dmix` share, bare `hw` is exclusive and refuses rates.
fn route_rank(id: &str) -> usize {
    const ORDER: [&str; 6] = ["sysdefault", "plughw", "dsnoop", "dmix", "front", "hw"];
    let head = id.split(':').next().unwrap_or(id);
    ORDER.iter().position(|r| *r == head).unwrap_or(ORDER.len())
}

/// One row per piece of hardware, reached by the best route. ALSA lists a card once per route
/// and again under a numeric id; the description is the only thing they all agree on. Off
/// Linux the list is left alone: two identical headsets are two devices there.
fn one_row_per_device(found: Vec<AudioDevice>) -> Vec<AudioDevice> {
    if !cfg!(target_os = "linux") {
        return found;
    }
    let mut best: Vec<AudioDevice> = Vec::new();
    for device in found {
        match best.iter_mut().find(|kept| kept.name == device.name) {
            Some(kept) if route_rank(&device.id) < route_rank(&kept.id) => *kept = device,
            Some(_) => {}
            None => best.push(device),
        }
    }
    best
}

fn describe(device: &cpal::Device, id: &cpal::DeviceId, default: bool, input: bool) -> AudioDevice {
    let config = if input {
        device.default_input_config().ok()
    } else {
        device.default_output_config().ok()
    };
    AudioDevice {
        id: id.to_string(),
        name: device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| id.to_string()),
        is_default: default,
        sample_rate: config.as_ref().map(|c| c.sample_rate()),
        channels: config.as_ref().map(|c| c.channels()),
    }
}

/// Every microphone the host will name.
pub fn microphones() -> Vec<AudioDevice> {
    let host = cpal::default_host();
    let default = host.default_input_device().and_then(|d| d.id().ok());
    let Ok(found) = host.input_devices() else {
        return Vec::new();
    };
    let listed = found
        .filter_map(|device| {
            let id = device.id().ok()?;
            let raw = id.id().to_string();
            if !is_a_device(&raw) {
                return None;
            }
            Some(describe(&device, &id, Some(&id) == default.as_ref(), true))
        })
        .collect();
    one_row_per_device(listed)
}

/// Every speaker the host will name.
pub fn speakers() -> Vec<AudioDevice> {
    let host = cpal::default_host();
    let default = host.default_output_device().and_then(|d| d.id().ok());
    let Ok(found) = host.output_devices() else {
        return Vec::new();
    };
    let listed = found
        .filter_map(|device| {
            let id = device.id().ok()?;
            let raw = id.id().to_string();
            if !is_a_device(&raw) {
                return None;
            }
            Some(describe(&device, &id, Some(&id) == default.as_ref(), false))
        })
        .collect();
    one_row_per_device(listed)
}

/// The device with this id, or the default. Matched by hand rather than through
/// `Host::device_by_id`, which canonicalises the id it is given but not the ones it compares
/// against, and so never matches its own output.
fn find(id: Option<&str>, input: bool) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    let Some(id) = id else {
        return if input {
            host.default_input_device().ok_or("no input device".into())
        } else {
            host.default_output_device()
                .ok_or("no output device".into())
        };
    };
    let wanted: cpal::DeviceId = id.parse().map_err(|e| format!("device id {id}: {e}"))?;
    let mut devices = if input {
        host.input_devices().map_err(|e| e.to_string())?
    } else {
        host.output_devices().map_err(|e| e.to_string())?
    };
    devices
        .find(|d| d.id().is_ok_and(|found| found == wanted))
        .ok_or_else(|| format!("no device {id}"))
}

fn supported_format(format: cpal::SampleFormat) -> Result<(), String> {
    use cpal::SampleFormat::*;
    if matches!(format, F32 | I16 | U16 | I32) {
        Ok(())
    } else {
        Err(format!("unsupported sample format {format}"))
    }
}

// -- microphone ----------------------------------------------------------------------------

/// Samples from the capture callback, and a way to wait for them.
#[derive(Default)]
struct MicBuffer {
    samples: Mutex<std::collections::VecDeque<f32>>,
    arrived: Condvar,
}

/// A capture device, mono, at whatever rate it prefers.
pub struct Microphone {
    rate: u32,
    buffer: Arc<MicBuffer>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// How much capture may bank before the reader is behind: three encoder frames. More is
/// sender-side latency that never comes back.
const MIC_BACKLOG_FRAMES: usize = 3;

impl Microphone {
    /// Opens the default input device.
    pub fn default_device() -> Result<Self, String> {
        Self::open(None)
    }

    /// Opens the device with this id (from [`AudioDevice::id`]), or the default. The stream
    /// lives on its own thread until this is dropped; dropping waits for the device to close.
    pub fn open(id: Option<&str>) -> Result<Self, String> {
        let buffer = Arc::new(MicBuffer::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (ready, started) = std::sync::mpsc::channel();
        let (sink, flag, id) = (buffer.clone(), stop.clone(), id.map(str::to_string));
        let thread = std::thread::Builder::new()
            .name("bevy_iroh-mic".into())
            .spawn(move || {
                let stream = match start_capture(id.as_deref(), sink) {
                    Ok((stream, rate)) => {
                        let _ = ready.send(Ok(rate));
                        stream
                    }
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                // Parked, not polled: unparked by `drop`.
                while !flag.load(Ordering::Relaxed) {
                    std::thread::park();
                }
                drop(stream);
            })
            .map_err(|e| e.to_string())?;
        let rate = started
            .recv()
            .map_err(|_| "microphone thread died".to_string())??;
        Ok(Self {
            rate,
            buffer,
            stop,
            thread: Some(thread),
        })
    }
}

fn start_capture(id: Option<&str>, sink: Arc<MicBuffer>) -> Result<(cpal::Stream, u32), String> {
    let device = find(id, true)?;
    let supported = device.default_input_config().map_err(|e| e.to_string())?;
    supported_format(supported.sample_format())?;
    let rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    let cap = (rate as usize / 50) * MIC_BACKLOG_FRAMES;
    let build_with = |buffer_size: cpal::BufferSize| {
        let mut config = supported.config();
        config.buffer_size = buffer_size;
        let sink = sink.clone();
        match supported.sample_format() {
            cpal::SampleFormat::F32 => capture::<f32>(&device, config, sink, channels, cap),
            cpal::SampleFormat::I16 => capture::<i16>(&device, config, sink, channels, cap),
            cpal::SampleFormat::U16 => capture::<u16>(&device, config, sink, channels, cap),
            cpal::SampleFormat::I32 => capture::<i32>(&device, config, sink, channels, cap),
            _ => unreachable!("checked above"),
        }
    };
    let stream = build_with(cpal::BufferSize::Fixed(LOW_LATENCY_FRAMES))
        .or_else(|_| build_with(cpal::BufferSize::Default))
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((stream, rate))
}

fn capture<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    sink: Arc<MicBuffer>,
    channels: usize,
    cap: usize,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    device.build_input_stream(
        config,
        move |data: &[T], _| {
            {
                let mut b = sink.samples.lock().unwrap_or_else(|e| e.into_inner());
                for frame in data.chunks(channels.max(1)) {
                    let sum: f32 = frame.iter().map(|s| f32::from_sample(*s)).sum();
                    b.push_back(sum / frame.len() as f32);
                }
                while b.len() > cap {
                    b.pop_front();
                }
            }
            sink.arrived.notify_one();
        },
        |e| tracing::warn!("bevy_iroh: microphone: {e}"),
        None,
    )
}

impl Drop for Microphone {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

impl AudioSource for Microphone {
    fn sample_rate(&self) -> u32 {
        self.rate
    }

    /// Waits up to 20 ms for the device rather than polling.
    fn read(&mut self, out: &mut [f32]) -> usize {
        let mut b = self
            .buffer
            .samples
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if b.is_empty() {
            let (guard, _) = self
                .buffer
                .arrived
                .wait_timeout(b, Duration::from_millis(20))
                .unwrap_or_else(|e| e.into_inner());
            b = guard;
        }
        let n = out.len().min(b.len());
        for s in out.iter_mut().take(n) {
            *s = b.pop_front().unwrap_or(0.0);
        }
        n
    }
}

// -- speaker -------------------------------------------------------------------------------

/// An output device: the default, or one by id.
#[derive(Default)]
pub struct Speaker {
    pub id: Option<String>,
}

impl Speaker {
    pub fn open(id: Option<&str>) -> Self {
        Self {
            id: id.map(str::to_string),
        }
    }
}

impl AudioOutput for Speaker {
    fn start(self: Box<Self>, mixer: Arc<Mutex<Mixer>>) -> Result<Running, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready, started) = std::sync::mpsc::channel();
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("bevy_iroh-speaker".into())
            .spawn(move || {
                let stream = match start_playback(self.id.as_deref(), mixer) {
                    Ok(stream) => {
                        let _ = ready.send(Ok(()));
                        stream
                    }
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                while !flag.load(Ordering::Relaxed) {
                    std::thread::park();
                }
                drop(stream);
            })
            .map_err(|e| e.to_string())?;
        started
            .recv()
            .map_err(|_| "speaker thread died".to_string())??;
        let handle = thread.thread().clone();
        Ok(Running::new(stop, move || {
            handle.unpark();
            let _ = thread.join();
        }))
    }
}

fn start_playback(id: Option<&str>, mixer: Arc<Mutex<Mixer>>) -> Result<cpal::Stream, String> {
    let device = find(id, false)?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    supported_format(supported.sample_format())?;
    let rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    tracing::info!("bevy_iroh: speaker at {rate} Hz, {channels} channels");
    let build_with = |buffer_size: cpal::BufferSize| {
        let mut config = supported.config();
        config.buffer_size = buffer_size;
        let mixer = mixer.clone();
        match supported.sample_format() {
            cpal::SampleFormat::F32 => playback::<f32>(&device, config, mixer, channels, rate),
            cpal::SampleFormat::I16 => playback::<i16>(&device, config, mixer, channels, rate),
            cpal::SampleFormat::U16 => playback::<u16>(&device, config, mixer, channels, rate),
            cpal::SampleFormat::I32 => playback::<i32>(&device, config, mixer, channels, rate),
            _ => unreachable!("checked above"),
        }
    };
    let stream = build_with(cpal::BufferSize::Fixed(LOW_LATENCY_FRAMES))
        .or_else(|_| build_with(cpal::BufferSize::Default))
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

fn playback<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mixer: Arc<Mutex<Mixer>>,
    channels: usize,
    rate: u32,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    // Owned by the callback and reused: no allocation under the device's deadline.
    let mut mix: Vec<f32> = Vec::new();
    device.build_output_stream(
        config,
        move |data: &mut [T], _| {
            mix.clear();
            mix.resize(data.len(), 0.0);
            mixer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .render(&mut mix, channels, rate);
            for (out, s) in data.iter_mut().zip(&mix) {
                *out = T::from_sample(*s);
            }
        },
        |e| tracing::warn!("bevy_iroh: speaker: {e}"),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alsa_plugins_are_not_devices() {
        if !cfg!(target_os = "linux") {
            return;
        }
        assert!(is_a_device("default"));
        assert!(is_a_device("sysdefault:CARD=USB"));
        assert!(is_a_device("hw:CARD=0,DEV=0"));
        assert!(!is_a_device("null"));
        assert!(!is_a_device("usbstream:CARD=USB"));
        assert!(!is_a_device("lavrate"));
    }

    #[test]
    fn one_card_one_row_by_best_route() {
        if !cfg!(target_os = "linux") {
            return;
        }
        let row = |id: &str| AudioDevice {
            id: id.into(),
            name: "USB Audio".into(),
            is_default: false,
            sample_rate: None,
            channels: None,
        };
        let rows = one_row_per_device(vec![
            row("hw:CARD=USB,DEV=0"),
            row("plughw:CARD=USB,DEV=0"),
            row("sysdefault:CARD=USB"),
            row("dsnoop:CARD=USB,DEV=0"),
        ]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "sysdefault:CARD=USB");
    }
}
