//! The receiver's side: spawns, updates and despawns from peers, applied to the world.

use bevy::{prelude::*, time::Real};
use iroh::EndpointId;
use iroh_gossip::proto::TopicId;

use super::{
    Authority, InRoom, NetId, NetIds, Owner, Registry, Remote, RemoteSpawner, Replica, Shared,
    Snapshots, wire,
};
use crate::{
    net::{Frame, Kind, Payload, proto::MAX_ITEMS},
    node::{Inbox, Iroh, ToNet},
    room::{PeerLeft, RoomTopic, Rooms},
};

const KINDS: [Kind; 7] = [
    wire::Spawn::KIND,
    wire::Update::KIND,
    wire::Remove::KIND,
    wire::Despawn::KIND,
    wire::Manifest::KIND,
    wire::SnapshotRequest::KIND,
    wire::Snapshot::KIND,
];

/// How long before asking the owner for the same id again.
const ASK_AGAIN: f64 = 3.0;

pub(crate) fn apply(world: &mut World) {
    let Some(me) = world.get_resource::<Iroh>().map(|i| i.id()) else {
        return;
    };
    let frames: Vec<Frame> = world
        .resource_mut::<Inbox>()
        .take_where(|k| KINDS.contains(&k));
    if frames.is_empty() {
        return;
    }
    let now = world.resource::<Time<Real>>().elapsed_secs_f64();
    world.resource_scope(|world, registry: Mut<Registry>| {
        let mut cx = Cx {
            world,
            registry: &registry,
            me,
            now,
        };
        for frame in frames {
            let Some(room) = cx.world.resource::<Rooms>().by_topic(&frame.topic) else {
                continue;
            };
            if let Err(why) = cx.frame(frame, room) {
                debug!("bevy_iroh: replication: {why}");
            }
        }
    });
}

struct Cx<'w> {
    world: &'w mut World,
    registry: &'w Registry,
    me: EndpointId,
    now: f64,
}

impl Cx<'_> {
    fn frame(&mut self, frame: Frame, room: Entity) -> anyhow::Result<()> {
        let (topic, from) = (frame.topic, frame.from);
        match frame.kind {
            k if k == wire::Spawn::KIND => {
                let spawn = wire::Spawn::from_body(&frame.body)?;
                self.spawn(topic, room, from, spawn);
            }
            k if k == wire::Update::KIND => {
                let update = wire::Update::from_body(&frame.body)?;
                anyhow::ensure!(update.entities.len() <= MAX_ITEMS, "oversized update");
                for e in update.entities {
                    self.update(topic, from, e);
                }
            }
            k if k == wire::Remove::KIND => {
                let remove = wire::Remove::from_body(&frame.body)?;
                self.remove(from, remove);
            }
            k if k == wire::Despawn::KIND => {
                let despawn = wire::Despawn::from_body(&frame.body)?;
                self.despawn(from, despawn);
            }
            k if k == wire::Manifest::KIND => {
                let manifest = wire::Manifest::from_body(&frame.body)?;
                anyhow::ensure!(manifest.entries.len() <= MAX_ITEMS, "oversized manifest");
                self.manifest(topic, from, manifest);
            }
            k if k == wire::SnapshotRequest::KIND => {
                let request = wire::SnapshotRequest::from_body(&frame.body)?;
                anyhow::ensure!(request.ids.len() <= MAX_ITEMS, "oversized snapshot request");
                self.world.resource_mut::<Snapshots>().pending.push((
                    topic,
                    from,
                    Some(request.ids),
                ));
            }
            k if k == wire::Snapshot::KIND => {
                let snapshot = wire::Snapshot::from_body(&frame.body)?;
                anyhow::ensure!(snapshot.spawns.len() <= MAX_ITEMS, "oversized snapshot");
                for spawn in snapshot.spawns {
                    self.spawn(topic, room, from, spawn);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The live entity for an id, if any.
    fn entity(&self, id: NetId) -> Option<Entity> {
        let e = self.world.resource::<NetIds>().get(id)?;
        self.world.get_entity(e).is_ok().then_some(e)
    }

    fn is_remote(&self, entity: Entity) -> bool {
        self.world.get::<Remote>(entity).is_some()
    }

    fn authority(&self, entity: Entity) -> Authority {
        self.world
            .get::<Shared>(entity)
            .map(|s| s.authority)
            .unwrap_or_default()
    }

    fn apply_components(
        &mut self,
        entity: Entity,
        from: EndpointId,
        components: Vec<wire::Component>,
    ) {
        for (key, bytes) in components {
            match self.registry.by_key(key) {
                Some(entry) => {
                    if let Err(why) = entry.codec.apply(self.world, entity, from, &bytes) {
                        debug!("bevy_iroh: {} from {}: {why}", entry.name, from.fmt_short());
                    }
                }
                None => debug!(
                    "bevy_iroh: unknown component {key:016x} from {}",
                    from.fmt_short()
                ),
            }
        }
        let tick = self.world.change_tick();
        if let Some(mut replica) = self.world.get_mut::<Replica>(entity) {
            replica.applied_tick = Some(tick);
        }
    }

    fn spawn(&mut self, topic: TopicId, room: Entity, from: EndpointId, spawn: wire::Spawn) {
        let entity = match self.entity(spawn.id) {
            None => {
                // The app may want to build the entity its own way first.
                let e = self
                    .world
                    .resource_scope(|world, spawner: Mut<RemoteSpawner>| {
                        (spawner.0)(world, &spawn.components)
                    });
                let Some(e) = e else {
                    debug!("bevy_iroh: the app declined to spawn {}", spawn.id);
                    return;
                };
                self.world.entity_mut(e).insert((
                    spawn.id,
                    Shared {
                        authority: spawn.authority,
                    },
                    Owner(spawn.owner),
                    Remote,
                    InRoom(room),
                    Replica::remote(spawn.lamport, topic),
                ));
                self.world.resource_mut::<NetIds>().insert(spawn.id, e);
                e
            }
            Some(e) => {
                let placeholder = self.world.get::<Replica>(e).is_some_and(|r| r.placeholder);
                if placeholder {
                    self.world.entity_mut(e).insert((
                        Shared {
                            authority: spawn.authority,
                        },
                        Owner(spawn.owner),
                        InRoom(room),
                        Replica::remote(spawn.lamport, topic),
                    ));
                } else if self.is_remote(e) {
                    let ours = self.world.get::<Replica>(e).map(|r| r.lamport).unwrap_or(0);
                    if spawn.lamport < ours {
                        return;
                    }
                    let mut em = self.world.entity_mut(e);
                    em.insert(Owner(spawn.owner));
                    if let Some(mut r) = em.get_mut::<Replica>() {
                        r.lamport = spawn.lamport;
                        r.from_topic = Some(topic);
                    }
                } else {
                    // Both of us spawned this id: a keyed entity. The lower node id keeps it.
                    if spawn.owner >= self.me {
                        return;
                    }
                    let mut em = self.world.entity_mut(e);
                    em.insert((Remote, Owner(spawn.owner)));
                    if let Some(mut r) = em.get_mut::<Replica>() {
                        r.lamport = spawn.lamport;
                        r.from_topic = Some(topic);
                    }
                }
                e
            }
        };
        self.apply_components(entity, from, spawn.components);
    }

    fn update(&mut self, topic: TopicId, from: EndpointId, update: wire::EntityUpdate) {
        let Some(entity) = self.entity(update.id) else {
            self.ask(topic, from, vec![update.id]);
            return;
        };
        let Some(replica) = self.world.get::<Replica>(entity) else {
            return;
        };
        if replica.placeholder {
            self.ask(topic, from, vec![update.id]);
            return;
        }
        let ours = replica.lamport;
        let remote = self.is_remote(entity);
        if !remote && self.authority(entity) == Authority::Owner {
            return;
        }
        if update.lamport < ours || (update.lamport == ours && !remote && from > self.me) {
            return;
        }
        if let Some(mut r) = self.world.get_mut::<Replica>(entity) {
            r.lamport = update.lamport;
        }
        self.apply_components(entity, from, update.components);
    }

    fn remove(&mut self, from: EndpointId, remove: wire::Remove) {
        let Some(entity) = self.entity(remove.id) else {
            return;
        };
        let remote = self.is_remote(entity);
        if !remote && self.authority(entity) == Authority::Owner {
            return;
        }
        let ours = self
            .world
            .get::<Replica>(entity)
            .map(|r| r.lamport)
            .unwrap_or(0);
        if remove.lamport < ours {
            return;
        }
        if let Some(entry) = self.registry.by_key(remove.key) {
            entry.codec.remove(self.world, entity);
        }
        let tick = self.world.change_tick();
        if let Some(mut r) = self.world.get_mut::<Replica>(entity) {
            r.lamport = remove.lamport;
            r.applied_tick = Some(tick);
        }
        let _ = from;
    }

    fn despawn(&mut self, _from: EndpointId, despawn: wire::Despawn) {
        let Some(entity) = self.entity(despawn.id) else {
            return;
        };
        let remote = self.is_remote(entity);
        if !remote && self.authority(entity) == Authority::Owner {
            return;
        }
        self.world.resource_mut::<NetIds>().remove(despawn.id);
        self.world.despawn(entity);
    }

    fn manifest(&mut self, topic: TopicId, from: EndpointId, manifest: wire::Manifest) {
        let mut missing = Vec::new();
        for (id, lamport) in manifest.entries {
            match self.entity(id) {
                None => missing.push(id),
                Some(e) => {
                    let behind = self.world.get::<Replica>(e).is_some_and(|r| {
                        r.placeholder || (self.is_remote(e) && r.lamport < lamport)
                    });
                    if behind {
                        missing.push(id);
                    }
                }
            }
        }
        if !missing.is_empty() {
            self.ask(topic, from, missing);
        }
    }

    /// Ask the owner for these ids, whole, no more than once every few seconds per id.
    fn ask(&mut self, topic: TopicId, owner: EndpointId, ids: Vec<NetId>) {
        let now = self.now;
        let mut snapshots = self.world.resource_mut::<Snapshots>();
        let ids: Vec<NetId> = ids
            .into_iter()
            .filter(|id| snapshots.asked.get(id).is_none_or(|t| now - t >= ASK_AGAIN))
            .collect();
        if ids.is_empty() {
            return;
        }
        for id in &ids {
            snapshots.asked.insert(*id, now);
        }
        if let Ok(body) = (wire::SnapshotRequest { ids }).to_body() {
            self.world.resource::<Iroh>().send(ToNet::SendTo {
                topic,
                to: owner,
                kind: wire::SnapshotRequest::KIND,
                body,
            });
        }
    }
}

/// A peer that left, or fell silent, takes its entities with it. Their owner's `Despawn` may
/// never come: a closed laptop sends nothing.
pub(crate) fn on_peer_left(
    mut left: MessageReader<PeerLeft>,
    mut commands: Commands,
    topics: Query<&RoomTopic>,
    mut ids: ResMut<NetIds>,
    replicas: Query<(Entity, &NetId, &Owner, &Replica), With<Remote>>,
) {
    for gone in left.read() {
        let Ok(topic) = topics.get(gone.room) else {
            continue;
        };
        for (entity, id, owner, replica) in &replicas {
            if owner.0 == gone.id && replica.from_topic == Some(topic.0) {
                ids.remove(*id);
                commands.entity(entity).despawn();
            }
        }
    }
}

/// Leaving a room drops the replicas that came from it, and forgets that local entities were
/// announced there.
pub(crate) fn on_room_left(
    remove: On<Remove, RoomTopic>,
    mut commands: Commands,
    topics: Query<&RoomTopic>,
    mut ids: ResMut<NetIds>,
    mut replicas: Query<(Entity, &NetId, &mut Replica, Has<Remote>)>,
) {
    let Ok(topic) = topics.get(remove.entity) else {
        return;
    };
    for (entity, id, mut replica, remote) in &mut replicas {
        if remote {
            if replica.from_topic == Some(topic.0) {
                ids.remove(*id);
                commands.entity(entity).despawn();
            }
        } else {
            replica.announced.remove(&topic.0);
        }
    }
}
