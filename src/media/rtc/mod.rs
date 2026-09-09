//! Media over WebRTC data channels, for peers a QUIC dial cannot reach. Behind the `webrtc`
//! feature.
//!
//! A browser cannot accept a QUIC connection, so without this a page talks to everyone
//! through a relay. WebRTC brings the browser's own hole punching: two data channels, one
//! unreliable and unordered for the 20 ms voice frames and one reliable for video and
//! control, carrying exactly the bytes the QUIC path carries. When a link is up, media to
//! and from that peer moves onto it; when it drops, media moves back to the relay path.
//!
//! Signalling is one typed message over the room: the offer, the answer and each ICE
//! candidate as they arrive. By default a page offers to every peer it sees and a desktop
//! only answers, so a desktop never dials a desktop this way; [`RtcSettings::initiate`]
//! changes that, which is what the two-app test uses. Two pages offering to each other is
//! resolved by the higher id giving way.

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(target_arch = "wasm32")]
mod web;

use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use super::{Media, VideoFeed, Voice};
use crate::{
    message::{MessageAppExt, NetSender, Received},
    node::Iroh,
    replicate::{Owner, Remote},
    room::{MemberOf, Peer, PeerLeft},
};

#[cfg(not(target_arch = "wasm32"))]
use native as platform;
#[cfg(target_arch = "wasm32")]
use web as platform;

/// What one side tells the other while a connection is being set up.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtcSignal {
    pub kind: SignalKind,
    /// An SDP, or one ICE candidate line.
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignalKind {
    Offer,
    Answer,
    Candidate,
}

/// How WebRTC links are made. Insert before `IrohPlugin` to change the defaults.
#[derive(Resource, Debug, Clone)]
pub struct RtcSettings {
    /// STUN servers, as `host:port`, for learning this machine's public address. Empty is
    /// host candidates only: the local network.
    pub stun: Vec<String>,
    /// Offer to every peer that appears. On by default in a page, off on a desktop, which
    /// answers offers and otherwise reaches peers over QUIC.
    pub initiate: bool,
    /// Off, and no offer is made or answered.
    pub enabled: bool,
}

impl Default for RtcSettings {
    fn default() -> Self {
        Self {
            stun: vec!["stun.l.google.com:19302".into()],
            initiate: cfg!(target_arch = "wasm32"),
            enabled: true,
        }
    }
}

/// How media to and from a peer travels right now. On `Peer` entities and on every remote
/// [`Voice`] or [`VideoFeed`] entity.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaPath {
    /// Direct QUIC, or the relay when hole punching failed.
    Quic,
    /// A WebRTC data channel, hole-punched by the browser's ICE.
    WebRtc,
}

/// Signals the platform halves produce, sent from a system since only a system has the
/// `NetSender`.
#[derive(Resource, Clone, Default)]
pub(crate) struct Outbox(pub Arc<Mutex<Vec<(Entity, EndpointId, RtcSignal)>>>);

impl Outbox {
    pub fn push(&self, room: Entity, peer: EndpointId, signal: RtcSignal) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((room, peer, signal));
    }
}

/// Per peer: where the attempt stands.
#[derive(Component, Debug)]
struct RtcAttempt {
    /// Seconds on `Time<Real>` when the current attempt began.
    started: f64,
    phase: Phase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Offering,
    Answering,
    Up,
    Down,
}

/// How long an attempt may take before it is given up on.
const ATTEMPT_TIMEOUT: f64 = 20.0;
/// How long after a failed or dropped link before offering again.
const RETRY_AFTER: f64 = 5.0;

pub(crate) struct RtcPlugin;

impl Plugin for RtcPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RtcSettings>()
            .init_resource::<Outbox>()
            .add_net_message::<RtcSignal>()
            .add_systems(
                Update,
                (initiate, signals_in, signals_out, paths, peers_left).chain(),
            );
    }
}

/// Offer to peers, when this node is the kind that offers.
fn initiate(
    settings: Res<RtcSettings>,
    iroh: Option<Res<Iroh>>,
    media: Res<Media>,
    outbox: Res<Outbox>,
    time: Res<Time<bevy::time::Real>>,
    mut peers: Query<(Entity, &Peer, &MemberOf, Option<&mut RtcAttempt>)>,
    mut commands: Commands,
) {
    let Some(iroh) = iroh else { return };
    if !settings.enabled || !settings.initiate {
        return;
    }
    let now = time.elapsed_secs_f64();
    for (entity, peer, member_of, attempt) in &mut peers {
        let due = match attempt {
            None => true,
            Some(mut a) => match a.phase {
                Phase::Up | Phase::Answering => false,
                Phase::Down => now - a.started > RETRY_AFTER,
                Phase::Offering => {
                    if now - a.started > ATTEMPT_TIMEOUT && !media.hub.is_rtc(peer.id) {
                        a.phase = Phase::Down;
                        a.started = now;
                        platform::close(peer.id, &media.hub);
                    }
                    false
                }
            },
        };
        if !due {
            continue;
        }
        debug!("bevy_iroh: webrtc offer to {}", peer.id.fmt_short());
        platform::offer(
            peer.id,
            member_of.0,
            &settings,
            media.hub.clone(),
            outbox.clone(),
            &iroh,
        );
        commands.entity(entity).insert(RtcAttempt {
            started: now,
            phase: Phase::Offering,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn signals_in(
    mut signals: MessageReader<Received<RtcSignal>>,
    settings: Res<RtcSettings>,
    iroh: Option<Res<Iroh>>,
    media: Res<Media>,
    outbox: Res<Outbox>,
    time: Res<Time<bevy::time::Real>>,
    mut attempts: Query<&mut RtcAttempt>,
    mut commands: Commands,
) {
    let Some(iroh) = iroh else { return };
    let now = time.elapsed_secs_f64();
    for received in signals.read() {
        if !settings.enabled {
            continue;
        }
        let (from, room) = (received.from, received.room);
        match received.msg.kind {
            SignalKind::Offer => {
                // Both sides offered at once: the higher id gives way and answers.
                let mine = received
                    .peer
                    .and_then(|p| attempts.get(p).ok())
                    .map(|a| a.phase);
                if mine == Some(Phase::Offering) {
                    if iroh.id() < from {
                        debug!(
                            "bevy_iroh: webrtc glare with {}: mine stands",
                            from.fmt_short()
                        );
                        continue;
                    }
                    platform::close(from, &media.hub);
                }
                debug!("bevy_iroh: webrtc offer from {}", from.fmt_short());
                platform::answer(
                    from,
                    room,
                    received.msg.body.clone(),
                    &settings,
                    media.hub.clone(),
                    outbox.clone(),
                    &iroh,
                );
                if let Some(peer) = received.peer {
                    match attempts.get_mut(peer) {
                        Ok(mut a) => {
                            a.phase = Phase::Answering;
                            a.started = now;
                        }
                        Err(_) => {
                            commands.entity(peer).insert(RtcAttempt {
                                started: now,
                                phase: Phase::Answering,
                            });
                        }
                    }
                }
            }
            SignalKind::Answer => platform::accept_answer(from, received.msg.body.clone()),
            SignalKind::Candidate => platform::add_candidate(from, received.msg.body.clone()),
        }
    }
}

fn signals_out(outbox: Res<Outbox>, net: NetSender) {
    let pending: Vec<_> = std::mem::take(&mut *outbox.0.lock().unwrap_or_else(|e| e.into_inner()));
    for (room, peer, signal) in pending {
        net.send_to(room, peer, &signal);
    }
}

/// Keep `MediaPath` honest on peers and on their media entities, and attempts in step with
/// the links that actually exist.
fn paths(
    media: Res<Media>,
    time: Res<Time<bevy::time::Real>>,
    mut peers: Query<(Entity, &Peer, Option<&MediaPath>, Option<&mut RtcAttempt>)>,
    mut entities: Query<
        (Entity, &Owner, Option<&MediaPath>),
        (With<Remote>, Or<(With<Voice>, With<VideoFeed>)>),
    >,
    mut commands: Commands,
) {
    let now = time.elapsed_secs_f64();
    let path = |id: EndpointId| {
        if media.hub.is_rtc(id) {
            MediaPath::WebRtc
        } else {
            MediaPath::Quic
        }
    };
    for (entity, peer, current, attempt) in &mut peers {
        let wanted = path(peer.id);
        if current != Some(&wanted) {
            commands.entity(entity).insert(wanted);
        }
        if let Some(mut a) = attempt {
            match (wanted, a.phase) {
                (MediaPath::WebRtc, p) if p != Phase::Up => a.phase = Phase::Up,
                (MediaPath::Quic, Phase::Up) => {
                    a.phase = Phase::Down;
                    a.started = now;
                }
                _ => {}
            }
        }
    }
    for (entity, owner, current) in &mut entities {
        let wanted = path(owner.0);
        if current != Some(&wanted) {
            commands.entity(entity).insert(wanted);
        }
    }
}

fn peers_left(mut left: MessageReader<PeerLeft>, media: Res<Media>) {
    for l in left.read() {
        platform::close(l.id, &media.hub);
    }
}

/// The three channels every link is made of: voice frames, unreliable; video groups and
/// control, reliable; and the room's own frames, reliable and apart from video so a keyframe
/// in flight never holds a move back.
pub(crate) const AUDIO_CHANNEL: &str = "audio";
pub(crate) const VIDEO_CHANNEL: &str = "video";
pub(crate) const FRAMES_CHANNEL: &str = "frames";

/// Bytes a reliable channel may hold unsent before video frames are refused. Past this the
/// link is behind by more than a group, and the subscriber is better served by a fresh
/// keyframe than by everything in between.
pub(crate) const VIDEO_BACKLOG: usize = 512 * 1024;
