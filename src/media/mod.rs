//! Voice over the same endpoint. Behind the `media` feature.
//!
//! Put [`Voice`] on a [`Shared`](crate::replicate::Shared) entity and your microphone goes to
//! everyone in the room, announced by replication like any other component. A remote entity
//! that arrives with `Voice` is subscribed to and played back, with gain and pan from where it
//! is relative to the [`AudioListener`]. Nothing to write on the receiving side.

pub mod audio;
pub mod transport;
#[cfg(all(feature = "v4l2", target_os = "linux"))]
pub mod v4l2;
pub mod video;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

pub use audio::{AudioOutput, AudioSource, Microphone, Mixer, RemoteTrack, Speaker};
pub use transport::{ALPN, MediaHub, TrackKind};
pub use video::{
    Pixels, RemoteVideo, RgbaFrame, TestPattern, VideoConfig, VideoFrame, VideoSource,
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

/// This entity shows a picture: publish frames under its id. Replicated, so peers subscribe.
/// Pair it locally with a [`VideoInput`] naming where the frames come from.
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
    source: Mutex<Option<Box<dyn VideoSource>>>,
    pub config: VideoConfig,
}

impl VideoInput {
    pub fn new(source: impl VideoSource) -> Self {
        Self {
            source: Mutex::new(Some(Box::new(source))),
            config: VideoConfig::default(),
        }
    }

    pub fn with_config(mut self, config: VideoConfig) -> Self {
        self.config = config;
        self
    }
}

/// On a remote [`VideoFeed`] entity once its first frame has decoded: the picture, kept
/// current. Put it on any material.
#[derive(Component, Debug, Clone, Deref)]
pub struct VideoImage(pub Handle<Image>);

/// Where the ears are. Put it on the camera. Without one, the listener is at the origin.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct AudioListener;

/// Which devices voice uses. Insert before `IrohPlugin` to choose; the default is the default
/// microphone and speakers, opened the first time they are needed.
#[derive(Resource)]
pub struct MediaSettings {
    pub microphone: MicrophoneChoice,
    pub speaker: SpeakerChoice,
    /// Metres at which a voice fades to silence.
    pub voice_range: f32,
}

impl Default for MediaSettings {
    fn default() -> Self {
        Self {
            microphone: MicrophoneChoice::Default,
            speaker: SpeakerChoice::Default,
            voice_range: 10.0,
        }
    }
}

pub enum MicrophoneChoice {
    Default,
    None,
    /// Your own source: a test tone, a file, a different capture library.
    Custom(Mutex<Option<Box<dyn AudioSource>>>),
}

pub enum SpeakerChoice {
    Default,
    None,
    Custom(Mutex<Option<Box<dyn AudioOutput>>>),
}

impl MicrophoneChoice {
    pub fn custom(source: impl AudioSource) -> Self {
        MicrophoneChoice::Custom(Mutex::new(Some(Box::new(source))))
    }
}

impl SpeakerChoice {
    pub fn custom(output: impl AudioOutput) -> Self {
        SpeakerChoice::Custom(Mutex::new(Some(Box::new(output))))
    }
}

/// The hub as a resource, plus the threads it has started.
#[derive(Resource)]
pub struct Media {
    pub hub: Arc<MediaHub>,
    /// Owners whose session dropped this frame, for every subscribe system to see.
    dead: Vec<iroh::EndpointId>,
    encoder: Option<Encoder>,
    speaker: Option<Arc<AtomicBool>>,
    speaker_failed: bool,
    mic_failed: bool,
}

struct Encoder {
    stop: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
}

/// Per local video feed: the encoder thread's stop flag.
#[derive(Component)]
struct Encoding(Arc<AtomicBool>);

impl Drop for Encoding {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        if let Some(e) = &self.encoder {
            e.stop.store(true, Ordering::Relaxed);
        }
        if let Some(s) = &self.speaker {
            s.store(true, Ordering::Relaxed);
        }
    }
}

impl Media {
    pub(crate) fn new(hub: Arc<MediaHub>) -> Self {
        Self {
            hub,
            dead: Vec::new(),
            encoder: None,
            speaker: None,
            speaker_failed: false,
            mic_failed: false,
        }
    }

    fn ensure_encoder(&mut self, settings: &MediaSettings) {
        if self.encoder.is_some() || self.mic_failed {
            return;
        }
        let source: Option<Box<dyn AudioSource>> = match &settings.microphone {
            MicrophoneChoice::None => {
                self.mic_failed = true;
                None
            }
            // An empty slot is a source that is not ready yet, not one that failed: the app
            // fills it when its device opens, and this is asked again every frame until then.
            MicrophoneChoice::Custom(slot) => slot.lock().unwrap_or_else(|e| e.into_inner()).take(),
            MicrophoneChoice::Default => match Microphone::default_device() {
                Ok(m) => Some(Box::new(m)),
                Err(e) => {
                    error!("bevy_iroh: microphone: {e}");
                    self.mic_failed = true;
                    None
                }
            },
        };
        let Some(source) = source else {
            return;
        };
        info!("bevy_iroh: microphone at {} Hz", source.sample_rate());
        let stop = Arc::new(AtomicBool::new(false));
        let level = Arc::new(AtomicU32::new(0));
        let (hub, s, l) = (self.hub.clone(), stop.clone(), level.clone());
        if let Err(e) = std::thread::Builder::new()
            .name("bevy_iroh-voice".into())
            .spawn(move || audio::run_encoder(source, hub, s, l))
        {
            error!("bevy_iroh: voice encoder: {e}");
            self.mic_failed = true;
            return;
        }
        self.encoder = Some(Encoder { stop, level });
    }

    /// Stop the encoder so the next local `Voice` opens the microphone again: for a device
    /// change, or a new source in `MediaSettings::microphone`.
    pub fn restart_microphone(&mut self) {
        if let Some(e) = self.encoder.take() {
            e.stop.store(true, Ordering::Relaxed);
        }
        self.mic_failed = false;
    }

    fn ensure_speaker(&mut self, settings: &MediaSettings) {
        if self.speaker.is_some() || self.speaker_failed {
            return;
        }
        let output: Option<Box<dyn AudioOutput>> = match &settings.speaker {
            SpeakerChoice::None => None,
            SpeakerChoice::Custom(slot) => slot.lock().unwrap_or_else(|e| e.into_inner()).take(),
            SpeakerChoice::Default => Some(Box::new(Speaker)),
        };
        let Some(output) = output else {
            self.speaker_failed = true;
            return;
        };
        let stop = Arc::new(AtomicBool::new(false));
        let mixer = Arc::new(Mutex::new(Mixer::new(self.hub.clone())));
        match output.start(mixer, stop.clone()) {
            Ok(()) => self.speaker = Some(stop),
            Err(e) => {
                error!("bevy_iroh: speaker: {e}");
                self.speaker_failed = true;
            }
        }
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
            .insert_resource(Media::new(self.hub.clone()))
            .replicate::<Voice>()
            .replicate::<VideoFeed>()
            .add_systems(
                Update,
                (
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
    }
}

fn publish(
    mut commands: Commands,
    mut media: ResMut<Media>,
    settings: Res<MediaSettings>,
    voices: Query<(Entity, &NetId, &Voice, Option<&Published>), (With<Shared>, Without<Remote>)>,
) {
    for (entity, id, voice, published) in &voices {
        match published {
            Some(p) => p.0.store(voice.muted, Ordering::Relaxed),
            None => {
                media.ensure_encoder(&settings);
                let muted = media.hub.publish(id.0);
                muted.store(voice.muted, Ordering::Relaxed);
                commands
                    .entity(entity)
                    .insert((Published(muted), VoiceLevel::default()));
            }
        }
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
        media.ensure_speaker(&settings);
        let (hub, endpoint, owner_id, track) = (media.hub.clone(), iroh.endpoint(), owner.0, id.0);
        iroh.spawn(async move {
            if let Err(e) = hub
                .subscribe(endpoint, owner_id, track, TrackKind::Audio)
                .await
            {
                debug!("bevy_iroh: subscribe to {:016x}: {e:#}", track);
                lock_dead(&hub, owner_id);
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

fn lock_dead(hub: &MediaHub, owner: iroh::EndpointId) {
    // A failed dial is a dead session for the purpose of trying again.
    hub.mark_dead(owner);
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
    let dt = time.delta_secs();
    for (entity, id, mut level, remote, stats) in &mut voices {
        let rms = if remote {
            media.hub.remote(id.0).map(|t| t.level()).unwrap_or(0.0)
        } else {
            media
                .encoder
                .as_ref()
                .map(|e| f32::from_bits(e.level.load(Ordering::Relaxed)))
                .unwrap_or(0.0)
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
    feeds: Query<
        (Entity, &NetId, &VideoInput),
        (
            With<Shared>,
            With<VideoFeed>,
            Without<Remote>,
            Without<Encoding>,
        ),
    >,
) {
    let Some(iroh) = iroh else { return };
    for (entity, id, input) in &feeds {
        let Some(source) = input
            .source
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            continue;
        };
        let track = id.0;
        media.hub.publish(track);
        let keyframe = media.hub.keyframe_flag(track);
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (config, s) = (input.config.clone(), stop.clone());
        if let Err(e) = std::thread::Builder::new()
            .name("bevy_iroh-encode".into())
            .spawn(move || video::run_encoder(source, config, keyframe, s, tx))
        {
            error!("bevy_iroh: video encoder: {e}");
            continue;
        }
        let hub = media.hub.clone();
        iroh.spawn(async move { video::run_publisher(hub, track, rx).await });
        commands.entity(entity).insert(Encoding(stop));
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
        let (hub, endpoint, owner_id, track) = (media.hub.clone(), iroh.endpoint(), owner.0, id.0);
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

/// Newest decoded frames into images.
fn video_frames(
    mut commands: Commands,
    media: Res<Media>,
    images: Option<ResMut<Assets<Image>>>,
    feeds: Query<(Entity, &NetId, Option<&VideoImage>), (With<VideoFeed>, With<Remote>)>,
) {
    let Some(mut images) = images else { return };
    for (entity, id, image) in &feeds {
        let Some(remote) = media.hub.remote_video(id.0) else {
            continue;
        };
        let Some(frame) = remote.take_frame() else {
            continue;
        };
        let size = wgpu_types::Extent3d {
            width: frame.width,
            height: frame.height,
            depth_or_array_layers: 1,
        };
        let reusable = image
            .filter(|i| {
                images
                    .get(&i.0)
                    .is_some_and(|img| img.texture_descriptor.size == size)
            })
            .map(|i| i.0.clone());
        if let Some(handle) = reusable {
            if let Some(mut img) = images.get_mut(&handle) {
                img.data = Some(frame.data);
            }
            continue;
        }
        let img = Image::new(
            size,
            wgpu_types::TextureDimension::D2,
            frame.data,
            wgpu_types::TextureFormat::Rgba8UnormSrgb,
            bevy::asset::RenderAssetUsages::MAIN_WORLD
                | bevy::asset::RenderAssetUsages::RENDER_WORLD,
        );
        let handle = images.add(img);
        commands.entity(entity).insert(VideoImage(handle));
    }
}

fn on_video_removed(
    remove: On<Remove, VideoFeed>,
    iroh: Option<Res<Iroh>>,
    media: Res<Media>,
    q: Query<(&NetId, Has<Remote>, Option<&VideoSubscribed>)>,
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
