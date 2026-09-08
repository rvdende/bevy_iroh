//! What replication puts on the wire.

use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use super::{Authority, NetId};
use crate::net::{Kind, Payload};

/// One component as it travels: the codec's key and its encoded wire form.
pub type Component = (u64, Vec<u8>);

/// An entity, whole. Sent when it is first shared into a room and in snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spawn {
    pub id: NetId,
    pub owner: EndpointId,
    pub authority: Authority,
    pub lamport: u64,
    pub components: Vec<Component>,
}
impl Payload for Spawn {
    const KIND: Kind = Kind::named("bevy_iroh/replicate/spawn/1");
}

/// Changed components, batched per room per frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Update {
    pub entities: Vec<EntityUpdate>,
}
impl Payload for Update {
    const KIND: Kind = Kind::named("bevy_iroh/replicate/update/1");
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityUpdate {
    pub id: NetId,
    pub lamport: u64,
    pub components: Vec<Component>,
}

/// A component left an entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Remove {
    pub id: NetId,
    pub lamport: u64,
    pub key: u64,
}
impl Payload for Remove {
    const KIND: Kind = Kind::named("bevy_iroh/replicate/remove/1");
}

/// An entity is no longer shared, or no longer exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Despawn {
    pub id: NetId,
    pub lamport: u64,
}
impl Payload for Despawn {
    const KIND: Kind = Kind::named("bevy_iroh/replicate/despawn/1");
}

/// What the sender owns in this room, once a second. A receiver that lacks an id, or has an
/// older version, asks for it: the answer to gossip having dropped a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub entries: Vec<(NetId, u64)>,
}
impl Payload for Manifest {
    const KIND: Kind = Kind::named("bevy_iroh/replicate/manifest/1");
}

/// Direct, to the owner: send me these whole.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotRequest {
    pub ids: Vec<NetId>,
}
impl Payload for SnapshotRequest {
    const KIND: Kind = Kind::named("bevy_iroh/replicate/snapshot-request/1");
}

/// Direct, to a peer: everything I own here, or what they asked for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub spawns: Vec<Spawn>,
}
impl Payload for Snapshot {
    const KIND: Kind = Kind::named("bevy_iroh/replicate/snapshot/1");
}
