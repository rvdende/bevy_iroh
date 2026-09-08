//! Rooms and who is in them.
//!
//! A room is an entity with no spatial meaning: a gossip topic, the peers heard on it, and the
//! entities shared into it. Membership is inferred from signed traffic, not from gossip
//! neighbour events, because a gossip mesh does not make every member your neighbour: every
//! member says `Hello` on join and every few seconds after, and a peer that has been silent for
//! three heartbeats is gone.

use bevy::{platform::collections::HashMap, prelude::*, time::Real};
use iroh::EndpointId;
use iroh_gossip::proto::TopicId;

use crate::{
    IrohSet,
    net::{
        self, Kind, Payload, RoomTicket,
        proto::{Goodbye, Hello},
    },
    node::{FromNet, Inbox, Iroh, Receive, Timing, ToNet},
};

/// A room: a topic and its members. Spawn one with [`Room::host`] or [`Room::join`].
#[derive(Component, Debug, Clone)]
pub struct Room {
    pub name: String,
}

impl Room {
    /// Open a fresh room. A [`Ticket`] appears on the entity once there is one to share.
    pub fn host(name: impl Into<String>) -> (Room, RoomRequest, RoomStatus) {
        (
            Room { name: name.into() },
            RoomRequest::Host,
            RoomStatus::Connecting,
        )
    }

    /// Join the room a ticket names.
    pub fn join(ticket: RoomTicket) -> (Room, RoomRequest, RoomStatus) {
        (
            Room {
                name: ticket.name.clone(),
            },
            RoomRequest::Join(ticket),
            RoomStatus::Connecting,
        )
    }
}

/// What the room entity asked for. Consumed on the first frame the node sees it.
#[derive(Component, Debug, Clone)]
pub enum RoomRequest {
    Host,
    Join(RoomTicket),
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub enum RoomStatus {
    Connecting,
    Joined,
    Failed(String),
}

/// The topic, once the node has subscribed. Its presence means "in the room".
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Deref)]
pub struct RoomTopic(pub TopicId);

/// What to hand someone else so they can join. Inserted once mintable: for a host, after a
/// relay is reached or five seconds pass; for a joiner, immediately.
#[derive(Component, Debug, Clone, Deref)]
pub struct Ticket(pub RoomTicket);

/// A remote member of a room. One entity per peer per room, related to it by [`MemberOf`].
#[derive(Component, Debug, Clone)]
pub struct Peer {
    pub id: EndpointId,
    /// `None` until their first `Hello` carries one.
    pub name: Option<String>,
    /// Seconds on `Time<Real>` when they were last heard.
    pub last_heard: f64,
}

impl Peer {
    /// The name, or a short key: unambiguous even when not friendly.
    pub fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| self.id.fmt_short().to_string())
    }
}

/// Which room a [`Peer`] entity belongs to.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
#[relationship(relationship_target = Members)]
pub struct MemberOf(pub Entity);

/// The [`Peer`] entities of a room. Despawning the room despawns them.
#[derive(Component, Debug, Default)]
#[relationship_target(relationship = MemberOf, linked_spawn)]
pub struct Members(Vec<Entity>);

impl Members {
    pub fn iter(&self) -> impl Iterator<Item = Entity> + '_ {
        self.0.iter().copied()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A peer was heard for the first time in a room.
#[derive(Message, Debug, Clone)]
pub struct PeerJoined {
    pub room: Entity,
    pub peer: Entity,
    pub id: EndpointId,
}

/// A peer said goodbye or fell silent. The entity is despawned after this is written.
#[derive(Message, Debug, Clone)]
pub struct PeerLeft {
    pub room: Entity,
    pub peer: Entity,
    pub id: EndpointId,
}

/// Topic to room entity, and back.
#[derive(Resource, Default)]
pub struct Rooms {
    by_topic: HashMap<TopicId, Entity>,
    peers: HashMap<(Entity, EndpointId), Entity>,
}

impl Rooms {
    pub fn by_topic(&self, topic: &TopicId) -> Option<Entity> {
        self.by_topic.get(topic).copied()
    }

    /// The [`Peer`] entity for `id` in `room`.
    pub fn peer(&self, room: Entity, id: EndpointId) -> Option<Entity> {
        self.peers.get(&(room, id)).copied()
    }

    /// Every room this node has joined.
    pub fn joined(&self) -> impl Iterator<Item = (TopicId, Entity)> + '_ {
        self.by_topic.iter().map(|(t, e)| (*t, *e))
    }
}

/// Per-room presence clock.
#[derive(Component)]
struct Presence {
    last_hello: f64,
    /// Say hello on the next send, ahead of the clock: a neighbour just appeared.
    nudge: bool,
}

impl Default for Presence {
    fn default() -> Self {
        // The first hello goes out on the first frame in the room, not after one interval.
        Self {
            last_hello: f64::NEG_INFINITY,
            nudge: false,
        }
    }
}

pub(crate) struct RoomPlugin;

impl Plugin for RoomPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Rooms>()
            .add_message::<PeerJoined>()
            .add_message::<PeerLeft>()
            .add_systems(
                PreUpdate,
                (
                    drain.in_set(Receive::Drain),
                    (presence_in, reap).chain().in_set(Receive::Rooms),
                ),
            )
            .add_systems(PostUpdate, (request, presence_out).in_set(IrohSet::Send))
            .add_observer(leave_on_despawn);
    }
}

/// Send the node what new room entities asked for.
fn request(
    mut commands: Commands,
    iroh: Option<Res<Iroh>>,
    rooms: Query<(Entity, &Room, &RoomRequest)>,
) {
    for (entity, room, req) in &rooms {
        commands.entity(entity).remove::<RoomRequest>();
        let Some(iroh) = iroh.as_deref() else {
            commands
                .entity(entity)
                .insert(RoomStatus::Failed("networking is off".into()));
            continue;
        };
        let room_id: u64 = entity.to_bits();
        match req {
            RoomRequest::Host => iroh.send(ToNet::Host {
                room: room_id,
                name: room.name.clone(),
            }),
            RoomRequest::Join(ticket) => {
                commands.entity(entity).insert(Ticket(ticket.clone()));
                iroh.send(ToNet::Join {
                    room: room_id,
                    ticket: ticket.clone(),
                });
            }
        }
    }
}

/// Everything the node said since last frame, sorted into room state and the [`Inbox`].
fn drain(
    mut commands: Commands,
    iroh: Option<ResMut<Iroh>>,
    mut rooms: ResMut<Rooms>,
    mut inbox: ResMut<Inbox>,
    mut presence: Query<&mut Presence>,
) {
    let Some(mut iroh) = iroh else { return };
    for message in iroh.drain() {
        match message {
            FromNet::Joined {
                room,
                topic,
                ticket,
                relayed,
            } => {
                let Some(entity) = Entity::try_from_bits(room) else {
                    continue;
                };
                let Ok(mut e) = commands.get_entity(entity) else {
                    // The room was despawned while joining: leave again.
                    iroh.send(ToNet::Leave { topic });
                    continue;
                };
                if !relayed {
                    warn!(
                        "bevy_iroh: no relay reached; this ticket only works on the local network"
                    );
                }
                e.insert((
                    RoomTopic(topic),
                    RoomStatus::Joined,
                    Ticket(ticket),
                    Presence::default(),
                ));
                rooms.by_topic.insert(topic, entity);
            }
            FromNet::Failed {
                room: Some(room),
                why,
            } => {
                if let Some(entity) = Entity::try_from_bits(room)
                    && let Ok(mut e) = commands.get_entity(entity)
                {
                    warn!("bevy_iroh: room failed: {why}");
                    e.insert(RoomStatus::Failed(why));
                }
            }
            FromNet::Failed { room: None, why } => error!("bevy_iroh: {why}"),
            FromNet::Event(net::Event::Frame(frame)) => inbox.frames.push(frame),
            FromNet::Event(net::Event::Neighbor {
                topic, up: true, ..
            }) => {
                // They joined after our last hello and would otherwise wait for the clock.
                if let Some(room) = rooms.by_topic(&topic)
                    && let Ok(mut p) = presence.get_mut(room)
                {
                    p.nudge = true;
                }
            }
            FromNet::Event(net::Event::Neighbor { .. }) => {}
            FromNet::Event(net::Event::Notice { text, .. }) => debug!("bevy_iroh: {text}"),
        }
    }
}

/// Hello and goodbye frames become [`Peer`] entities and their absence.
fn presence_in(
    mut commands: Commands,
    mut inbox: ResMut<Inbox>,
    mut rooms: ResMut<Rooms>,
    time: Res<Time<Real>>,
    mut peers: Query<&mut Peer>,
    mut joined: MessageWriter<PeerJoined>,
    mut left: MessageWriter<PeerLeft>,
) {
    let now = time.elapsed_secs_f64();
    for frame in inbox.take(Kind::HELLO) {
        let Some(room) = rooms.by_topic(&frame.topic) else {
            continue;
        };
        let name = Hello::from_body(&frame.body)
            .ok()
            .map(|h| h.name)
            .filter(|n| !n.is_empty());
        match rooms.peer(room, frame.from) {
            Some(peer) => {
                if let Ok(mut p) = peers.get_mut(peer) {
                    p.last_heard = now;
                    if name.is_some() {
                        p.name = name;
                    }
                }
            }
            None => {
                let peer = commands
                    .spawn((
                        Peer {
                            id: frame.from,
                            name,
                            last_heard: now,
                        },
                        MemberOf(room),
                    ))
                    .id();
                rooms.peers.insert((room, frame.from), peer);
                joined.write(PeerJoined {
                    room,
                    peer,
                    id: frame.from,
                });
            }
        }
    }
    for frame in inbox.take(Kind::GOODBYE) {
        let Some(room) = rooms.by_topic(&frame.topic) else {
            continue;
        };
        if let Some(peer) = rooms.peers.remove(&(room, frame.from)) {
            left.write(PeerLeft {
                room,
                peer,
                id: frame.from,
            });
            commands.entity(peer).despawn();
        }
    }
    // Any signed frame is proof of life, whatever it says.
    for frame in &inbox.frames {
        if let Some(room) = rooms.by_topic(&frame.topic)
            && let Some(peer) = rooms.peer(room, frame.from)
            && let Ok(mut p) = peers.get_mut(peer)
        {
            p.last_heard = now;
        }
    }
}

/// Three missed heartbeats and a peer is gone. Not two: one missed message is ordinary on a
/// gossip mesh.
fn reap(
    mut commands: Commands,
    mut rooms: ResMut<Rooms>,
    timing: Res<Timing>,
    time: Res<Time<Real>>,
    peers: Query<(Entity, &Peer, &MemberOf)>,
    mut left: MessageWriter<PeerLeft>,
) {
    let silence = timing.heartbeat.as_secs_f64() * 3.0;
    let now = time.elapsed_secs_f64();
    for (entity, peer, member_of) in &peers {
        if now - peer.last_heard > silence {
            rooms.peers.remove(&(member_of.0, peer.id));
            left.write(PeerLeft {
                room: member_of.0,
                peer: entity,
                id: peer.id,
            });
            commands.entity(entity).despawn();
        }
    }
}

/// Say hello on the clock, and sooner when a neighbour appears.
fn presence_out(
    iroh: Option<Res<Iroh>>,
    timing: Res<Timing>,
    time: Res<Time<Real>>,
    mut rooms: Query<(&RoomTopic, &mut Presence)>,
) {
    let Some(iroh) = iroh else { return };
    let now = time.elapsed_secs_f64();
    let every = timing.heartbeat.as_secs_f64();
    for (topic, mut presence) in &mut rooms {
        if presence.nudge || now - presence.last_hello >= every {
            presence.nudge = false;
            presence.last_hello = now;
            let hello = Hello {
                name: iroh.display_name().to_string(),
            };
            if let Ok(body) = hello.to_body() {
                iroh.send(ToNet::Broadcast {
                    topic: topic.0,
                    kind: Kind::HELLO,
                    body,
                });
            }
        }
    }
}

/// Despawning a room leaves its topic, so nothing in it ghosts on peers.
fn leave_on_despawn(
    remove: On<Remove, RoomTopic>,
    iroh: Option<Res<Iroh>>,
    mut rooms: ResMut<Rooms>,
    topics: Query<&RoomTopic>,
) {
    let Ok(topic) = topics.get(remove.entity) else {
        return;
    };
    rooms.by_topic.remove(&topic.0);
    rooms.peers.retain(|(room, _), _| *room != remove.entity);
    if let Some(iroh) = iroh {
        if let Ok(body) = Goodbye.to_body() {
            iroh.send(ToNet::Broadcast {
                topic: topic.0,
                kind: Kind::GOODBYE,
                body,
            });
        }
        iroh.send(ToNet::Leave { topic: topic.0 });
    }
}
