//! Entity replication: mark an entity [`Shared`] and peers get it.
//!
//! Every registered codec (see [`codec`]) is checked for changes on the owner's side each
//! frame, batched per room, and applied on the receiver's side. An entity has one owner, the
//! node that spawned it, and a per-entity version for last-writer-wins.

pub mod codec;
pub mod transform;
pub mod wire;

mod apply;
mod send;

use std::collections::HashSet;

use bevy::{ecs::change_detection::Tick, platform::collections::HashMap, prelude::*};
use iroh::EndpointId;
use iroh_gossip::proto::TopicId;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub use codec::{Codec, DecodeCx, EncodeCx, Rate, Rejected, Serde};
pub use transform::{Glide, TransformCodec};

use crate::{IrohSet, net::Kind, node::Receive};

use codec::{Erased, ErasedCodec};

/// Replicate this entity to peers. Add it, and every registered component on the entity goes
/// out; remove it, or despawn the entity, and peers drop it.
#[derive(Component, Debug, Clone, Default)]
#[require(NetId)]
pub struct Shared {
    pub authority: Authority,
}

impl Shared {
    pub fn anyone() -> Self {
        Shared {
            authority: Authority::Anyone,
        }
    }
}

/// Who may change a shared entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Authority {
    /// Only the node that spawned it. Replicas never send.
    #[default]
    Owner,
    /// Any peer. Last writer wins, ordered by `(lamport, node id)`.
    Anyone,
}

/// A shared entity's identity across peers. Random by default; [`NetId::keyed`] derives one
/// from a name so peers that each spawn "the floor" get one floor, not two.
#[derive(
    Component, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct NetId(pub u64);

impl NetId {
    pub fn random() -> Self {
        NetId(rand::random())
    }

    pub fn keyed(key: &str) -> Self {
        NetId(crate::net::proto::fnv1a(key.as_bytes()))
    }
}

impl Default for NetId {
    fn default() -> Self {
        Self::random()
    }
}

impl std::fmt::Display for NetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// Who spawned a shared entity. On your own entities, you.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Deref)]
pub struct Owner(pub EndpointId);

/// On every replica of someone else's entity, for `Without<Remote>` = "mine".
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct Remote;

/// Scope a shared entity to one room. Absent, an entity is shared into every room this node
/// is in.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
#[relationship(relationship_target = SharedEntities)]
pub struct InRoom(pub Entity);

/// The entities scoped to a room by [`InRoom`].
#[derive(Component, Debug, Default)]
#[relationship_target(relationship = InRoom)]
pub struct SharedEntities(Vec<Entity>);

impl SharedEntities {
    pub fn iter(&self) -> impl Iterator<Item = Entity> + '_ {
        self.0.iter().copied()
    }
}

/// This crate's bookkeeping on every shared entity.
#[derive(Component, Debug)]
pub struct Replica {
    /// Version for last-writer-wins. The writer increments it on every send.
    pub lamport: u64,
    /// Topics this entity has been spawned into.
    pub(crate) announced: HashSet<TopicId>,
    /// The tick a remote value was written at, so it is not echoed back as our change.
    pub(crate) applied_tick: Option<Tick>,
    /// For remotes: the topic it arrived on, so leaving the room removes it.
    pub(crate) from_topic: Option<TopicId>,
    /// Spawned by a reference before its own spawn arrived.
    pub(crate) placeholder: bool,
}

impl Replica {
    fn local() -> Self {
        Replica {
            lamport: 0,
            announced: HashSet::new(),
            applied_tick: None,
            from_topic: None,
            placeholder: false,
        }
    }

    fn remote(lamport: u64, topic: TopicId) -> Self {
        Replica {
            lamport,
            announced: HashSet::new(),
            applied_tick: None,
            from_topic: Some(topic),
            placeholder: false,
        }
    }

    pub(crate) fn placeholder() -> Self {
        Replica {
            lamport: 0,
            announced: HashSet::new(),
            applied_tick: None,
            from_topic: None,
            placeholder: true,
        }
    }
}

/// [`NetId`] to entity.
#[derive(Resource, Default)]
pub struct NetIds {
    map: HashMap<NetId, Entity>,
}

impl NetIds {
    pub fn get(&self, id: NetId) -> Option<Entity> {
        self.map.get(&id).copied()
    }

    pub(crate) fn insert(&mut self, id: NetId, entity: Entity) {
        self.map.insert(id, entity);
    }

    pub(crate) fn remove(&mut self, id: NetId) {
        self.map.remove(&id);
    }
}

/// Every registered codec.
#[derive(Resource, Default)]
pub(crate) struct Registry {
    entries: Vec<Entry>,
    by_key: HashMap<u64, usize>,
    by_source: HashMap<std::any::TypeId, u64>,
}

pub(crate) struct Entry {
    pub key: u64,
    pub name: &'static str,
    pub rate: Rate,
    pub last_sent: f64,
    pub codec: Box<dyn ErasedCodec>,
}

impl Registry {
    fn add<C: Codec>(&mut self, codec: C) {
        let name = codec.name();
        let key = Kind::named(name).0;
        self.by_source
            .insert(std::any::TypeId::of::<C::Source>(), key);
        if let Some(&i) = self.by_key.get(&key) {
            warn!(
                "bevy_iroh: {} is registered twice; keeping the second",
                self.entries[i].name
            );
            self.entries.remove(i);
            self.by_key.clear();
            for (i, e) in self.entries.iter().enumerate() {
                self.by_key.insert(e.key, i);
            }
        }
        self.by_key.insert(key, self.entries.len());
        self.entries.push(Entry {
            key,
            name,
            rate: codec.rate(),
            last_sent: f64::NEG_INFINITY,
            codec: Box::new(Erased::new(codec)),
        });
    }

    /// The key of the codec whose source is `T`.
    pub fn key_of<T: 'static>(&self) -> Option<u64> {
        self.by_source.get(&std::any::TypeId::of::<T>()).copied()
    }

    pub fn by_key(&self, key: u64) -> Option<&Entry> {
        self.by_key.get(&key).map(|&i| &self.entries[i])
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn entries_mut(&mut self) -> &mut [Entry] {
        &mut self.entries
    }
}

/// How a remote entity comes to exist. The default is a bare `spawn`; an app whose entities
/// are kinds with their own spawn path sets one with
/// [`ReplicateAppExt::spawn_remote_with`], gets the wire components to look at, and returns
/// the entity the replicated components should land on.
#[derive(Resource)]
pub struct RemoteSpawner(
    pub Box<dyn Fn(&mut World, &[wire::Component]) -> Option<Entity> + Send + Sync>,
);

impl Default for RemoteSpawner {
    fn default() -> Self {
        RemoteSpawner(Box::new(|world, _| Some(world.spawn_empty().id())))
    }
}

/// Component removals and despawns noticed by observers, for the send system to announce.
#[derive(Resource, Default)]
pub(crate) struct Outbox {
    pub despawned: Vec<(NetId, u64, Vec<TopicId>)>,
    pub removed: Vec<(NetId, u64, u64, Vec<TopicId>)>,
}

/// Peers that need a snapshot: everything I own in the room, or the ids they asked for.
#[derive(Resource, Default)]
pub(crate) struct Snapshots {
    pub pending: Vec<(TopicId, EndpointId, Option<Vec<NetId>>)>,
    /// Ids we have asked for, and when, so a manifest does not ask every second.
    pub asked: HashMap<NetId, f64>,
}

pub trait ReplicateAppExt {
    /// Replicate `T` as is: derive serde and it goes.
    fn replicate<T>(&mut self) -> &mut Self
    where
        T: Component + Clone + Serialize + DeserializeOwned;

    /// Replicate through a codec of your own.
    fn replicate_with<C: Codec>(&mut self, codec: C) -> &mut Self;

    /// Decide how a remote entity is created, from its wire components, before they are
    /// applied to it. For apps whose entities are spawned by kind rather than assembled from
    /// components.
    fn spawn_remote_with(
        &mut self,
        spawner: impl Fn(&mut World, &[wire::Component]) -> Option<Entity> + Send + Sync + 'static,
    ) -> &mut Self;
}

impl ReplicateAppExt for App {
    fn replicate<T>(&mut self) -> &mut Self
    where
        T: Component + Clone + Serialize + DeserializeOwned,
    {
        self.replicate_with(Serde::<T>::new())
    }

    fn replicate_with<C: Codec>(&mut self, codec: C) -> &mut Self {
        self.init_resource::<Registry>();
        self.world_mut().resource_mut::<Registry>().add(codec);
        self.add_observer(send::on_remove_source::<C>);
        self
    }

    fn spawn_remote_with(
        &mut self,
        spawner: impl Fn(&mut World, &[wire::Component]) -> Option<Entity> + Send + Sync + 'static,
    ) -> &mut Self {
        self.insert_resource(RemoteSpawner(Box::new(spawner)));
        self
    }
}

pub(crate) struct ReplicatePlugin {
    pub transform: bool,
}

impl Plugin for ReplicatePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Registry>()
            .init_resource::<NetIds>()
            .init_resource::<Outbox>()
            .init_resource::<Snapshots>()
            .init_resource::<RemoteSpawner>()
            .add_systems(PreUpdate, apply::apply.in_set(Receive::Replicate))
            .add_systems(Update, transform::glide)
            .add_systems(
                PostUpdate,
                (send::queue_snapshots, send::send)
                    .chain()
                    .in_set(IrohSet::Send),
            )
            .add_observer(send::on_remove_shared)
            .add_observer(apply::on_room_left)
            .add_systems(PreUpdate, apply::on_peer_left.in_set(Receive::Replicate));
        if self.transform {
            app.replicate_with(TransformCodec::default());
        }
    }
}
