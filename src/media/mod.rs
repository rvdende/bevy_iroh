//! Voice over the same endpoint. Behind the `media` feature.
//!
//! Put [`Voice`] on a [`Shared`](crate::replicate::Shared) entity and your microphone goes to
//! everyone in the room, announced by replication like any other component. A remote entity
//! that arrives with `Voice` is subscribed to and played back, with gain and pan from where it
//! is relative to the [`AudioListener`]. Nothing to write on the receiving side.

pub mod audio;
pub mod transport;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

pub use audio::{AudioOutput, AudioSource, Microphone, Mixer, RemoteTrack, Speaker};
pub use transport::{ALPN, MediaHub};

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

/// Loudness of the last 20 ms, 0 to about 1. On local `Voice` entities from the microphone, on
/// remote ones from what was decoded. For meters.
#[derive(Component, Debug, Clone, Copy, Default, Deref)]
pub struct VoiceLevel(pub f32);

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
    encoder: Option<Encoder>,
    speaker: Option<Arc<AtomicBool>>,
    speaker_failed: bool,
    mic_failed: bool,
}

struct Encoder {
    stop: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
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
            MicrophoneChoice::None => None,
            MicrophoneChoice::Custom(slot) => slot.lock().unwrap_or_else(|e| e.into_inner()).take(),
            MicrophoneChoice::Default => match Microphone::default_device() {
                Ok(m) => Some(Box::new(m)),
                Err(e) => {
                    error!("bevy_iroh: microphone: {e}");
                    None
                }
            },
        };
        let Some(source) = source else {
            self.mic_failed = true;
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
            .add_systems(Update, (publish, subscribe, spatialize, levels))
            .add_observer(on_voice_removed);
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

const RESUBSCRIBE_AFTER: f64 = 2.0;

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
    let dead = media.hub.take_dead();
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
            if let Err(e) = hub.subscribe(endpoint, owner_id, track).await {
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
    let listener = listener.iter().next().copied().unwrap_or_default();
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

fn levels(media: Res<Media>, mut voices: Query<(&NetId, &mut VoiceLevel, Has<Remote>)>) {
    for (id, mut level, remote) in &mut voices {
        let value = if remote {
            media.hub.remote(id.0).map(|t| t.level()).unwrap_or(0.0)
        } else {
            media
                .encoder
                .as_ref()
                .map(|e| f32::from_bits(e.level.load(Ordering::Relaxed)))
                .unwrap_or(0.0)
        };
        if (level.0 - value).abs() > 1e-4 {
            level.0 = value;
        }
    }
}
