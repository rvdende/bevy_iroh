//! Voice and video over the same endpoint. Behind the `media` feature.
//!
//! Put [`Voice`] on a [`Shared`](crate::replicate::Shared) entity and your microphone goes to
//! everyone in the room, announced by replication like any other component. A remote entity
//! that arrives with `Voice` is subscribed to and played back, with gain and pan from where it
//! is relative to the [`AudioListener`]. Nothing to write on the receiving side.
//!
//! [`VideoFeed`] is the same shape for a picture: pair it locally with a [`VideoInput`] naming
//! where frames come from, and a remote one grows a [`VideoImage`] to put on any material.
//!
//! Which microphone, speaker and camera to use is [`MediaSettings`]; the lists to choose from
//! are [`AudioDevices`] and [`CameraDevices`]. Change a setting and the device is reopened
//! while everything published carries on.

pub mod audio;
pub mod devices;
#[cfg(not(target_arch = "wasm32"))]
pub mod native;
pub mod transport;
#[cfg(feature = "ui")]
pub mod ui;
#[cfg(all(feature = "v4l2", target_os = "linux"))]
pub mod v4l2;
pub mod video;
#[cfg(target_arch = "wasm32")]
pub mod web;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

pub use audio::{AudioOutput, AudioSource, Mixer, RemoteTrack, Running, VoiceDecoder};
pub use devices::{AudioDevice, AudioDevices, CameraDevice, CameraDevices, Permission};
#[cfg(not(target_arch = "wasm32"))]
pub use native::{Microphone, Speaker};
pub use transport::{ALPN, MediaHub, TrackKind};
pub use video::{
    FeedStats, Pixels, RemoteVideo, RgbaFrame, TestPattern, VideoConfig, VideoFrame, VideoSource,
    VideoTrackStats,
};

use crate::{
    node::Iroh,
    replicate::{NetId, Owner, Remote, ReplicateAppExt, Shared},
};

/// This entity speaks: publish the microphone under its id. Replicated, so peers subscribe.
#[derive(Component, Debug, Clone, Default, Serialize, Deserialize)]
pub struct Voice {
    /// Nothing is sent while muted. Replicated, so peers can show it.
    pub muted: bool,
}

/// Loudness for a meter, 0 to 1 on a decibel scale (-60 dB to 0 dB), with a 12 ms attack and
/// a 300 ms release. On local `Voice` entities from the microphone, on remote ones from what
/// was decoded. Loudness is not linear: a meter on raw amplitude sits at the bottom for
/// everything anybody actually says.
#[derive(Component, Debug, Clone, Copy, Default, Deref)]
pub struct VoiceLevel(pub f32);

/// What a remote voice has been through, updated every frame. `received` advancing between
/// two looks is the sign of life; `starved` advancing is the network or the sender.
#[derive(Component, Debug, Clone, Copy, Default, Deref)]
pub struct VoiceStats(pub audio::TrackStats);

/// The microphone's level for a meter in a corner of the screen, 0 to 1: the square root of
/// the loudest sample since last frame, which spreads the quiet end, falling at 2.5 per
/// second so a syllable stays on screen long enough to be seen. Zero while every local
/// [`Voice`] is muted, and whenever the microphone is not open.
#[derive(Resource, Debug, Clone, Copy, Default, Deref)]
pub struct MicLevel(pub f32);

/// This entity shows a picture: publish frames under its id. Replicated, so peers subscribe.
/// Pair it locally with a [`VideoInput`] naming where the frames come from. The size is what
/// the source produces; it is corrected once the first frame is seen.
#[derive(Component, Debug, Clone, Serialize, Deserialize)]
pub struct VideoFeed {
    pub width: u32,
    pub height: u32,
}

impl VideoFeed {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

/// The local half of a [`VideoFeed`]: the source and how to encode it. Not replicated.
#[derive(Component)]
pub struct VideoInput {
    kind: InputKind,
    pub config: VideoConfig,
    /// Also keep a [`VideoImage`] of what is being sent on this entity, so the app can show
    /// the person their own picture the same way it shows everyone else's.
    pub preview: bool,
}

enum InputKind {
    /// The camera named in [`MediaSettings::camera`], reopened when that changes.
    Camera,
    Source(Mutex<Option<Box<dyn VideoSource>>>),
}

impl VideoInput {
    /// Frames from your own source.
    pub fn new(source: impl VideoSource) -> Self {
        Self {
            kind: InputKind::Source(Mutex::new(Some(Box::new(source)))),
            config: VideoConfig::default(),
            preview: false,
        }
    }

    /// Frames from the camera chosen in [`MediaSettings`]: a `bevy_v4l2` device on Linux
    /// with the `v4l2` feature, `getUserMedia` in a page. Opened at about the size the
    /// entity's [`VideoFeed`] asks for.
    pub fn camera() -> Self {
        Self {
            kind: InputKind::Camera,
            config: VideoConfig::default(),
            preview: true,
        }
    }

    pub fn with_config(mut self, config: VideoConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_preview(mut self, preview: bool) -> Self {
        self.preview = preview;
        self
    }
}

/// On a remote [`VideoFeed`] entity once its first frame has decoded, and on a local one
/// with a preview: the picture, kept current. Put it on any material.
#[derive(Component, Debug, Clone, Deref)]
pub struct VideoImage(pub Handle<Image>);

/// What a remote video feed has been through, updated every frame.
#[derive(Component, Debug, Clone, Copy, Default, Deref)]
pub struct VideoStats(pub VideoTrackStats);

/// What a local video feed has been through.
#[derive(Component, Debug, Clone, Copy, Default, Deref)]
pub struct VideoFeedStats(pub FeedStats);

/// Where the ears are. Put it on the camera. Without one, the listener is at the origin.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct AudioListener;

/// Which devices media uses. Change a field and the device is reopened; what is published
/// keeps its track ids, so peers hear the switch as a moment's silence and nothing else.
#[derive(Resource)]
pub struct MediaSettings {
    pub microphone: MicrophoneChoice,
    pub speaker: SpeakerChoice,
    pub camera: CameraChoice,
    /// Metres at which a voice fades to silence.
    pub voice_range: f32,
    /// Master gain on everything heard, 0 to 1 (and above, if you must).
    pub output_volume: f32,
}

impl Default for MediaSettings {
    fn default() -> Self {
        Self {
            microphone: MicrophoneChoice::Default,
            speaker: SpeakerChoice::Default,
            camera: CameraChoice::Default,
            voice_range: 10.0,
            output_volume: 1.0,
        }
    }
}

pub enum MicrophoneChoice {
    /// The host's default input.
    Default,
    None,
    /// One from [`AudioDevices::microphones`], by id.
    Device(String),
    /// Your own source: a test tone, a file, a different capture library.
    Custom(Mutex<Option<Box<dyn AudioSource>>>),
}

pub enum SpeakerChoice {
    Default,
    None,
    /// One from [`AudioDevices::speakers`], by id. Ignored in a page, which plays through
    /// whatever the browser plays through.
    Device(String),
    Custom(Mutex<Option<Box<dyn AudioOutput>>>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CameraChoice {
    /// The first camera the machine names.
    Default,
    None,
    /// One from [`CameraDevices::cameras`], by id.
    Device(String),
}

impl MicrophoneChoice {
    pub fn custom(source: impl AudioSource) -> Self {
        MicrophoneChoice::Custom(Mutex::new(Some(Box::new(source))))
    }

    /// What this choice is, for telling two apart. A custom source is a fresh key every time
    /// the settings change, since a new source may have been put in the slot.
    fn key(&self, generation: u64) -> String {
        match self {
            MicrophoneChoice::Default => "default".into(),
            MicrophoneChoice::None => "none".into(),
            MicrophoneChoice::Device(id) => format!("device:{id}"),
            MicrophoneChoice::Custom(_) => format!("custom:{generation}"),
        }
    }
}

impl SpeakerChoice {
    pub fn custom(output: impl AudioOutput) -> Self {
        SpeakerChoice::Custom(Mutex::new(Some(Box::new(output))))
    }

    fn key(&self, generation: u64) -> String {
        match self {
            SpeakerChoice::Default => "default".into(),
            SpeakerChoice::None => "none".into(),
            SpeakerChoice::Device(id) => format!("device:{id}"),
            SpeakerChoice::Custom(_) => format!("custom:{generation}"),
        }
    }
}

impl CameraChoice {
    fn key(&self) -> String {
        match self {
            CameraChoice::Default => "default".into(),
            CameraChoice::None => "none".into(),
            CameraChoice::Device(id) => format!("device:{id}"),
        }
    }

    /// The id to open, or `None` for the default. `Err` when no camera is wanted.
    pub(crate) fn id(&self) -> Result<Option<&str>, ()> {
        match self {
            CameraChoice::Default => Ok(None),
            CameraChoice::None => Err(()),
            CameraChoice::Device(id) => Ok(Some(id)),
        }
    }
}

/// How long after a device fails to open before it is tried again.
const DEVICE_RETRY: f64 = 5.0;

/// The hub as a resource, plus the threads it has started.
#[derive(Resource)]
pub struct Media {
    pub hub: Arc<MediaHub>,
    /// Owners whose session dropped this frame, for every subscribe system to see.
    dead: Vec<iroh::EndpointId>,
    encoder: Option<Encoder>,
    mic_key: String,
    mic_retry_at: f64,
    speaker: Option<Running>,
    speaker_key: String,
    speaker_retry_at: f64,
    camera_key: String,
    /// Bumped every time the settings change, to tell one custom source from the next.
    generation: u64,
    /// The master gain, shared with the mixer, as f32 bits.
    volume: Arc<AtomicU32>,
}

struct Encoder {
    shared: Arc<audio::EncoderShared>,
    #[cfg(not(target_arch = "wasm32"))]
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Per local video feed: the encoder's shared state; dropping it stops the encoder.
#[derive(Component)]
struct Encoding(Arc<video::EncoderShared>);

impl Drop for Encoding {
    fn drop(&mut self) {
        self.0.stop.store(true, Ordering::Relaxed);
    }
}

/// Per local video feed whose camera would not open: when to try again.
#[derive(Component)]
struct CameraRetry(f64);

impl Media {
    pub(crate) fn new(hub: Arc<MediaHub>) -> Self {
        Self {
            hub,
            dead: Vec::new(),
            encoder: None,
            mic_key: String::new(),
            mic_retry_at: 0.0,
            speaker: None,
            speaker_key: String::new(),
            speaker_retry_at: 0.0,
            camera_key: String::new(),
            generation: 0,
            volume: Arc::new(AtomicU32::new(1.0f32.to_bits())),
        }
    }

    fn ensure_encoder(&mut self, settings: &MediaSettings, now: f64) {
        if self.encoder.is_some() || now < self.mic_retry_at {
            return;
        }
        self.mic_key = settings.microphone.key(self.generation);
        let shared = Arc::new(audio::EncoderShared::default());
        #[cfg(not(target_arch = "wasm32"))]
        {
            let source: Option<Box<dyn AudioSource>> = match &settings.microphone {
                MicrophoneChoice::None => None,
                // An empty slot is a source that is not ready yet, not one that failed: the
                // app fills it when its device opens, and this is asked again until then.
                MicrophoneChoice::Custom(slot) => {
                    slot.lock().unwrap_or_else(|e| e.into_inner()).take()
                }
                MicrophoneChoice::Default | MicrophoneChoice::Device(_) => {
                    let id = match &settings.microphone {
                        MicrophoneChoice::Device(id) => Some(id.as_str()),
                        _ => None,
                    };
                    match Microphone::open(id) {
                        Ok(m) => Some(Box::new(m)),
                        Err(e) => {
                            error!("bevy_iroh: microphone: {e}");
                            self.mic_retry_at = now + DEVICE_RETRY;
                            None
                        }
                    }
                }
            };
            let Some(source) = source else {
                return;
            };
            info!("bevy_iroh: microphone at {} Hz", source.sample_rate());
            let (hub, s) = (self.hub.clone(), shared.clone());
            match std::thread::Builder::new()
                .name("bevy_iroh-voice".into())
                .spawn(move || audio::run_encoder(source, hub, s))
            {
                Ok(thread) => {
                    self.encoder = Some(Encoder {
                        shared,
                        thread: Some(thread),
                    })
                }
                Err(e) => {
                    error!("bevy_iroh: voice encoder: {e}");
                    self.mic_retry_at = now + DEVICE_RETRY;
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            match web::audio::start_encoder(&settings.microphone, self.hub.clone(), shared.clone())
            {
                Ok(true) => self.encoder = Some(Encoder { shared }),
                Ok(false) => {}
                Err(e) => {
                    error!("bevy_iroh: microphone: {e}");
                    self.mic_retry_at = now + DEVICE_RETRY;
                }
            }
        }
    }

    /// Close the microphone so the next local `Voice` opens it again, from whatever
    /// `MediaSettings::microphone` says now. Done for you when the settings change.
    pub fn restart_microphone(&mut self) {
        self.encoder = None;
        self.mic_retry_at = 0.0;
    }

    /// The same for the speaker.
    pub fn restart_speaker(&mut self) {
        self.speaker = None;
        self.speaker_retry_at = 0.0;
    }

    fn ensure_speaker(&mut self, settings: &MediaSettings, now: f64) {
        if self.speaker.is_some() || now < self.speaker_retry_at {
            return;
        }
        self.speaker_key = settings.speaker.key(self.generation);
        let output: Option<Box<dyn AudioOutput>> = match &settings.speaker {
            SpeakerChoice::None => None,
            SpeakerChoice::Custom(slot) => slot.lock().unwrap_or_else(|e| e.into_inner()).take(),
            #[cfg(not(target_arch = "wasm32"))]
            SpeakerChoice::Default => Some(Box::new(Speaker::default())),
            #[cfg(not(target_arch = "wasm32"))]
            SpeakerChoice::Device(id) => Some(Box::new(Speaker::open(Some(id)))),
            #[cfg(target_arch = "wasm32")]
            SpeakerChoice::Default | SpeakerChoice::Device(_) => {
                web::audio::set_sink(match &settings.speaker {
                    SpeakerChoice::Device(id) => Some(id.as_str()),
                    _ => None,
                });
                Some(Box::new(web::audio::Speaker))
            }
        };
        let Some(output) = output else {
            return;
        };
        let mixer = Arc::new(Mutex::new(Mixer::new(
            self.hub.clone(),
            self.volume.clone(),
        )));
        match output.start(mixer) {
            Ok(running) => self.speaker = Some(running),
            Err(e) => {
                error!("bevy_iroh: speaker: {e}");
                self.speaker_retry_at = now + DEVICE_RETRY;
            }
        }
    }

    /// The microphone's meters, if it is open.
    pub(crate) fn mic_meters(&self) -> Option<&audio::EncoderShared> {
        self.encoder.as_ref().map(|e| &*e.shared)
    }
}

/// Per remote voice entity: the subscription's owner, and when to try again if it dropped.
#[derive(Component)]
struct Subscribed {
    owner: iroh::EndpointId,
    retry_at: f64,
}

/// The same for a remote video feed. Separate, since one entity can carry both.
#[derive(Component)]
struct VideoSubscribed {
    owner: iroh::EndpointId,
    retry_at: f64,
}

/// Per local voice entity: the mute flag shared with the encoder.
#[derive(Component)]
struct Published(Arc<AtomicBool>);

pub(crate) struct MediaPlugin {
    pub hub: Arc<MediaHub>,
}

impl Plugin for MediaPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MediaSettings>()
            .init_resource::<MicLevel>()
            .insert_resource(AudioDevices {
                // A desktop lists its devices at startup; a page waits to be asked.
                scan_wanted: cfg!(not(target_arch = "wasm32")),
                ..default()
            })
            .insert_resource(CameraDevices {
                scan_wanted: cfg!(not(target_arch = "wasm32")),
                ..default()
            })
            .insert_resource(Media::new(self.hub.clone()))
            .replicate::<Voice>()
            .replicate::<VideoFeed>()
            .add_systems(
                Update,
                (
                    devices::scan_audio,
                    devices::scan_cameras,
                    apply_settings,
                    collect_dead,
                    publish,
                    subscribe,
                    spatialize,
                    levels,
                    publish_video,
                    subscribe_video,
                    video_frames,
                )
                    .chain(),
            )
            .add_observer(on_voice_removed)
            .add_observer(on_video_removed);
        #[cfg(target_arch = "wasm32")]
        app.add_systems(Update, (devices::poll_web_devices, web::sweep));
    }
}

/// A changed setting reopens the device it names. The rest of the pipeline is untouched: the
/// remote tracks live in the hub, the published tracks keep their ids.
fn apply_settings(
    mut media: ResMut<Media>,
    settings: Res<MediaSettings>,
    mut feeds: Query<Entity, (With<VideoInput>, With<Encoding>)>,
    inputs: Query<&VideoInput>,
    mut commands: Commands,
) {
    if !settings.is_changed() {
        return;
    }
    media.generation += 1;
    let generation = media.generation;
    media
        .volume
        .store(settings.output_volume.max(0.0).to_bits(), Ordering::Relaxed);
    if settings.microphone.key(generation) != media.mic_key {
        media.restart_microphone();
    }
    if settings.speaker.key(generation) != media.speaker_key {
        media.restart_speaker();
    }
    if settings.camera.key() != media.camera_key {
        media.camera_key = settings.camera.key();
        for entity in &mut feeds {
            if matches!(inputs.get(entity).map(|i| &i.kind), Ok(InputKind::Camera)) {
                commands
                    .entity(entity)
                    .remove::<(Encoding, CameraRetry, VideoFeedStats)>();
            }
        }
    }
}

fn publish(
    mut commands: Commands,
    mut media: ResMut<Media>,
    settings: Res<MediaSettings>,
    time: Res<Time<bevy::time::Real>>,
    voices: Query<(Entity, &NetId, &Voice, Option<&Published>), (With<Shared>, Without<Remote>)>,
) {
    let now = time.elapsed_secs_f64();
    for (entity, id, voice, published) in &voices {
        match published {
            Some(p) => p.0.store(voice.muted, Ordering::Relaxed),
            None => {
                let muted = media.hub.publish(id.0);
                muted.store(voice.muted, Ordering::Relaxed);
                commands
                    .entity(entity)
                    .insert((Published(muted), VoiceLevel::default()));
            }
        }
        media.ensure_encoder(&settings, now);
    }
}

const RESUBSCRIBE_AFTER: f64 = 0.5;

fn subscribe(
    mut commands: Commands,
    iroh: Option<Res<Iroh>>,
    mut media: ResMut<Media>,
    settings: Res<MediaSettings>,
    time: Res<Time<bevy::time::Real>>,
    mut voices: Query<
        (Entity, &NetId, &Owner, Option<&mut Subscribed>),
        (With<Voice>, With<Remote>),
    >,
) {
    let Some(iroh) = iroh else { return };
    let now = time.elapsed_secs_f64();
    let dead = media.dead.clone();
    for (entity, id, owner, subscribed) in &mut voices {
        media.ensure_speaker(&settings, now);
        let due = match subscribed {
            None => true,
            Some(mut s) => {
                if dead.contains(&s.owner) {
                    s.retry_at = now + RESUBSCRIBE_AFTER;
                }
                let due = s.retry_at > 0.0 && now >= s.retry_at;
                if due {
                    s.retry_at = 0.0;
                }
                due
            }
        };
        if !due {
            continue;
        }
        let Some(endpoint) = endpoint(&iroh) else {
            continue;
        };
        let (hub, owner_id, track) = (media.hub.clone(), owner.0, id.0);
        iroh.spawn(async move {
            if let Err(e) = hub
                .subscribe(endpoint, owner_id, track, TrackKind::Audio)
                .await
            {
                debug!("bevy_iroh: subscribe to {:016x}: {e:#}", track);
                hub.mark_dead(owner_id);
            }
        });
        commands.entity(entity).insert((
            Subscribed {
                owner: owner.0,
                retry_at: 0.0,
            },
            VoiceLevel::default(),
        ));
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn endpoint(iroh: &Iroh) -> Option<iroh::Endpoint> {
    Some(iroh.endpoint())
}

#[cfg(target_arch = "wasm32")]
fn endpoint(iroh: &Iroh) -> Option<iroh::Endpoint> {
    iroh.endpoint()
}

fn on_voice_removed(
    remove: On<Remove, Voice>,
    iroh: Option<Res<Iroh>>,
    media: Res<Media>,
    q: Query<(&NetId, Has<Remote>, Option<&Subscribed>)>,
) {
    let Ok((id, remote, subscribed)) = q.get(remove.entity) else {
        return;
    };
    if remote {
        if let (Some(iroh), Some(s)) = (iroh, subscribed) {
            let (hub, owner, track) = (media.hub.clone(), s.owner, id.0);
            iroh.spawn(async move { hub.unsubscribe(owner, track).await });
        } else {
            media.hub.forget_remote(id.0);
        }
    } else {
        media.hub.unpublish(id.0);
    }
}

fn spatialize(
    media: Res<Media>,
    settings: Res<MediaSettings>,
    listener: Query<&GlobalTransform, With<AudioListener>>,
    voices: Query<(&NetId, Option<&GlobalTransform>), (With<Voice>, With<Remote>)>,
) {
    // No ears is not the same as ears at the origin: leave every gain as it is.
    let Some(listener) = listener.iter().next() else {
        return;
    };
    let (_, rotation, position) = listener.to_scale_rotation_translation();
    let right = rotation * Vec3::X;
    for (id, transform) in &voices {
        let Some(track) = media.hub.remote(id.0) else {
            continue;
        };
        let Some(transform) = transform else {
            track.set_gain(1.0);
            track.set_pan(0.0);
            continue;
        };
        let offset = transform.translation() - position;
        let distance = offset.length();
        let gain = (1.0 - distance / settings.voice_range.max(0.01)).clamp(0.0, 1.0);
        let pan = if distance > 0.05 {
            right.dot(offset / distance)
        } else {
            0.0
        };
        track.set_gain(gain);
        track.set_pan(pan);
    }
}

fn levels(
    media: Res<Media>,
    time: Res<Time<bevy::time::Real>>,
    mut mic: ResMut<MicLevel>,
    mut commands: Commands,
    mut voices: Query<(
        Entity,
        &NetId,
        &mut VoiceLevel,
        Has<Remote>,
        Option<&mut VoiceStats>,
    )>,
) {
    const FLOOR_DB: f32 = -60.0;
    const ATTACK: f32 = 0.012;
    const RELEASE: f32 = 0.30;
    /// How fast the corner meter falls, per second.
    const MIC_DECAY: f32 = 2.5;
    let dt = time.delta_secs();
    // The corner meter: instant attack, linear release, off the peak.
    let peak = media.mic_meters().map(|m| m.take_peak()).unwrap_or(0.0);
    let shown = peak.min(1.0).sqrt();
    let level = shown.max(mic.0 - MIC_DECAY * dt);
    if (mic.0 - level).abs() > 1e-4 {
        mic.0 = level;
    }
    for (entity, id, mut level, remote, stats) in &mut voices {
        let rms = if remote {
            media.hub.remote(id.0).map(|t| t.level()).unwrap_or(0.0)
        } else {
            media.mic_meters().map(|m| m.level()).unwrap_or(0.0)
        };
        let db = 20.0 * rms.max(1e-9).log10();
        let target = ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0);
        let tau = if target > level.0 { ATTACK } else { RELEASE };
        let alpha = 1.0 - (-dt / tau.max(1e-3)).exp();
        let value = level.0 + (target - level.0) * alpha;
        if (level.0 - value).abs() > 1e-4 {
            level.0 = value;
        }
        if remote && let Some(track) = media.hub.remote(id.0) {
            let now = track.stats();
            match stats {
                Some(mut s) => {
                    if s.0 != now {
                        s.0 = now;
                    }
                }
                None => {
                    commands.entity(entity).insert(VoiceStats(now));
                }
            }
        }
    }
}

fn collect_dead(mut media: ResMut<Media>) {
    media.dead = media.hub.take_dead();
}

fn publish_video(
    mut commands: Commands,
    iroh: Option<Res<Iroh>>,
    media: Res<Media>,
    settings: Res<MediaSettings>,
    time: Res<Time<bevy::time::Real>>,
    mut feeds: Query<
        (
            Entity,
            &NetId,
            &VideoInput,
            &mut VideoFeed,
            Option<&CameraRetry>,
            Option<&Encoding>,
            Option<&mut VideoFeedStats>,
        ),
        (With<Shared>, Without<Remote>),
    >,
) {
    let Some(iroh) = iroh else { return };
    let now = time.elapsed_secs_f64();
    for (entity, id, input, mut feed, retry, encoding, stats) in &mut feeds {
        if let Some(encoding) = encoding {
            // Running: keep the announced size honest and the counters current.
            if let Some((w, h)) = encoding.0.size()
                && (feed.width, feed.height) != (w, h)
            {
                feed.width = w;
                feed.height = h;
            }
            let fresh = encoding.0.stats();
            match stats {
                Some(mut s) => {
                    if s.0 != fresh {
                        s.0 = fresh;
                    }
                }
                None => {
                    commands.entity(entity).insert(VideoFeedStats(fresh));
                }
            }
            continue;
        }
        if retry.is_some_and(|r| now < r.0) {
            continue;
        }
        let source: Option<Box<dyn VideoSource>> = match &input.kind {
            InputKind::Source(slot) => slot.lock().unwrap_or_else(|e| e.into_inner()).take(),
            InputKind::Camera => {
                let Ok(camera) = settings.camera.id() else {
                    continue;
                };
                match open_camera(camera, feed.width, feed.height, input.config.max_fps) {
                    Ok(source) => Some(source),
                    Err(e) => {
                        error!("bevy_iroh: camera: {e}");
                        commands
                            .entity(entity)
                            .insert(CameraRetry(now + DEVICE_RETRY));
                        None
                    }
                }
            }
        };
        let Some(source) = source else {
            continue;
        };
        let track = id.0;
        media.hub.publish(track);
        let keyframe = media.hub.keyframe_flag(track);
        let shared = Arc::new(video::EncoderShared::new(input.preview));
        let (tx, rx) = tokio::sync::mpsc::channel(video::ENCODER_QUEUE);
        let hub = media.hub.clone();
        iroh.spawn(async move { video::run_publisher(hub, track, rx).await });
        #[cfg(not(target_arch = "wasm32"))]
        {
            let (config, s) = (input.config.clone(), shared.clone());
            if let Err(e) = std::thread::Builder::new()
                .name("bevy_iroh-encode".into())
                .spawn(move || video::run_encoder(source, config, keyframe, s, tx))
            {
                error!("bevy_iroh: video encoder: {e}");
                continue;
            }
        }
        #[cfg(target_arch = "wasm32")]
        web::video::start_encoder(source, input.config.clone(), keyframe, shared.clone(), tx);
        commands
            .entity(entity)
            .insert(Encoding(shared))
            .remove::<CameraRetry>();
    }
}

/// The platform's camera, as a source.
fn open_camera(
    id: Option<&str>,
    width: u32,
    height: u32,
    fps: f32,
) -> Result<Box<dyn VideoSource>, String> {
    #[cfg(all(feature = "v4l2", target_os = "linux"))]
    {
        v4l2::Camera::open(id, width, height, fps).map(|c| Box::new(c) as Box<dyn VideoSource>)
    }
    #[cfg(target_arch = "wasm32")]
    {
        web::video::Camera::open(id, width, height, fps)
            .map(|c| Box::new(c) as Box<dyn VideoSource>)
    }
    #[cfg(not(any(all(feature = "v4l2", target_os = "linux"), target_arch = "wasm32")))]
    {
        let _ = (id, width, height, fps);
        Err("no camera backend on this platform: enable the `v4l2` feature on Linux, or give the entity a VideoInput of your own".into())
    }
}

fn subscribe_video(
    mut commands: Commands,
    iroh: Option<Res<Iroh>>,
    media: Res<Media>,
    time: Res<Time<bevy::time::Real>>,
    mut feeds: Query<
        (Entity, &NetId, &Owner, Option<&mut VideoSubscribed>),
        (With<VideoFeed>, With<Remote>),
    >,
) {
    let Some(iroh) = iroh else { return };
    let now = time.elapsed_secs_f64();
    let dead = media.dead.clone();
    for (entity, id, owner, subscribed) in &mut feeds {
        let due = match subscribed {
            None => true,
            Some(mut s) => {
                if dead.contains(&s.owner) {
                    s.retry_at = now + RESUBSCRIBE_AFTER;
                }
                let due = s.retry_at > 0.0 && now >= s.retry_at;
                if due {
                    s.retry_at = 0.0;
                }
                due
            }
        };
        if !due {
            continue;
        }
        let Some(endpoint) = endpoint(&iroh) else {
            continue;
        };
        let (hub, owner_id, track) = (media.hub.clone(), owner.0, id.0);
        iroh.spawn(async move {
            if let Err(e) = hub
                .subscribe(endpoint, owner_id, track, TrackKind::Video)
                .await
            {
                debug!("bevy_iroh: subscribe to video {:016x}: {e:#}", track);
                hub.mark_dead(owner_id);
            }
        });
        commands.entity(entity).insert(VideoSubscribed {
            owner: owner.0,
            retry_at: 0.0,
        });
    }
}

/// Newest decoded frames into images: remote feeds from their decoder, local ones from the
/// preview slot.
fn video_frames(
    mut commands: Commands,
    media: Res<Media>,
    images: Option<ResMut<Assets<Image>>>,
    mut feeds: Query<
        (
            Entity,
            &NetId,
            Option<&VideoImage>,
            Has<Remote>,
            Option<&Encoding>,
            Option<&mut VideoStats>,
        ),
        With<VideoFeed>,
    >,
) {
    let Some(mut images) = images else { return };
    for (entity, id, image, remote, encoding, stats) in &mut feeds {
        let frame = if remote {
            let Some(track) = media.hub.remote_video(id.0) else {
                continue;
            };
            let fresh = track.stats();
            match stats {
                Some(mut s) => {
                    if s.0 != fresh {
                        s.0 = fresh;
                    }
                }
                None => {
                    commands.entity(entity).insert(VideoStats(fresh));
                }
            }
            track.take_frame()
        } else {
            encoding.and_then(|e| e.0.take_preview())
        };
        let Some(frame) = frame else {
            continue;
        };
        // Built from the default image rather than by naming texture types, which come from
        // whichever wgpu this bevy is built against.
        let reusable = image
            .filter(|i| {
                images.get(&i.0).is_some_and(|img| {
                    img.texture_descriptor.size.width == frame.width
                        && img.texture_descriptor.size.height == frame.height
                })
            })
            .map(|i| i.0.clone());
        if let Some(handle) = reusable {
            if let Some(mut img) = images.get_mut(&handle) {
                img.data = Some(frame.data);
            }
            continue;
        }
        let mut img = Image::default();
        img.texture_descriptor.size.width = frame.width;
        img.texture_descriptor.size.height = frame.height;
        img.texture_descriptor.size.depth_or_array_layers = 1;
        img.asset_usage = bevy::asset::RenderAssetUsages::MAIN_WORLD
            | bevy::asset::RenderAssetUsages::RENDER_WORLD;
        img.data = Some(frame.data);
        let handle = images.add(img);
        commands.entity(entity).insert(VideoImage(handle));
    }
}

fn on_video_removed(
    remove: On<Remove, VideoFeed>,
    iroh: Option<Res<Iroh>>,
    media: Res<Media>,
    q: Query<(&NetId, Has<Remote>, Option<&VideoSubscribed>)>,
    mut commands: Commands,
) {
    let Ok((id, remote, subscribed)) = q.get(remove.entity) else {
        return;
    };
    if remote {
        if let (Some(iroh), Some(s)) = (iroh, subscribed) {
            let (hub, owner, track) = (media.hub.clone(), s.owner, id.0);
            iroh.spawn(async move { hub.unsubscribe(owner, track).await });
        } else {
            media.hub.forget_remote(id.0);
        }
    } else {
        media.hub.unpublish(id.0);
        commands
            .entity(remove.entity)
            .remove::<(Encoding, VideoImage, VideoFeedStats)>();
    }
}
