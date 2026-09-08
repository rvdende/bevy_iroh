//! The owner's side: notice what changed, batch it per room, and hand it to the node.

use bevy::{platform::collections::HashMap, prelude::*, time::Real};
use iroh::EndpointId;
use iroh_gossip::proto::TopicId;

use super::{
    Authority, Codec, InRoom, NetId, NetIds, Outbox, Registry, Remote, Replica, Shared, Snapshots,
    wire,
};
use crate::{
    net::{Payload, proto::MAX_ITEMS},
    node::{Iroh, ToNet},
    room::{PeerJoined, RoomTopic, Rooms},
};

const MANIFEST_EVERY: f64 = 1.0;

/// A local shared entity is going away, or is no longer shared. Runs before the components are
/// gone, so the id and the rooms are still readable.
pub(crate) fn on_remove_shared(
    remove: On<Remove, Shared>,
    q: Query<(&NetId, &Replica), Without<Remote>>,
    mut outbox: ResMut<Outbox>,
) {
    if let Ok((id, replica)) = q.get(remove.entity) {
        outbox.despawned.push((
            *id,
            replica.lamport + 1,
            replica.announced.iter().copied().collect(),
        ));
    }
}

/// A replicated component left a local shared entity.
pub(crate) fn on_remove_source<C: Codec>(
    remove: On<Remove, C::Source>,
    q: Query<(&NetId, &Replica), (With<Shared>, Without<Remote>)>,
    // Absent while the apply system holds the registry, which is exactly when a removal is a
    // peer's and not ours to announce.
    registry: Option<Res<Registry>>,
    mut outbox: ResMut<Outbox>,
) {
    if let Ok((id, replica)) = q.get(remove.entity)
        && let Some(key) = registry.and_then(|r| r.key_of::<C::Source>())
    {
        outbox.removed.push((
            *id,
            replica.lamport + 1,
            key,
            replica.announced.iter().copied().collect(),
        ));
    }
}

/// A newcomer gets everything I own in the room.
pub(crate) fn queue_snapshots(
    mut joined: MessageReader<PeerJoined>,
    rooms: Query<&RoomTopic>,
    mut snapshots: ResMut<Snapshots>,
) {
    for PeerJoined { room, id, .. } in joined.read() {
        if let Ok(topic) = rooms.get(*room) {
            snapshots.pending.push((topic.0, *id, None));
        }
    }
}

struct Local {
    entity: Entity,
    id: NetId,
    authority: Authority,
    topics: Vec<TopicId>,
}

/// Which topics an entity is shared into: its room's, or every joined room.
fn topics_for(world: &World, entity: Entity, joined: &[(TopicId, Entity)]) -> Vec<TopicId> {
    match world.get::<InRoom>(entity) {
        Some(InRoom(room)) => joined
            .iter()
            .filter(|(_, e)| e == room)
            .map(|(t, _)| *t)
            .collect(),
        None => joined.iter().map(|(t, _)| *t).collect(),
    }
}

fn broadcast<P: Payload>(iroh: &Iroh, topic: TopicId, payload: &P) {
    if let Ok(body) = payload.to_body() {
        iroh.send(ToNet::Broadcast {
            topic,
            kind: P::KIND,
            body,
        });
    }
}

fn send_to<P: Payload>(iroh: &Iroh, topic: TopicId, to: EndpointId, payload: &P) {
    if let Ok(body) = payload.to_body() {
        iroh.send(ToNet::SendTo {
            topic,
            to,
            kind: P::KIND,
            body,
        });
    }
}

fn encode_all(world: &World, registry: &Registry, entity: Entity) -> Vec<wire::Component> {
    registry
        .entries()
        .iter()
        .filter_map(|e| e.codec.encode(world, entity).map(|bytes| (e.key, bytes)))
        .collect()
}

pub(crate) fn send(
    world: &mut World,
    mut manifests: bevy::ecs::system::Local<HashMap<TopicId, f64>>,
) {
    let Some(me) = world.get_resource::<Iroh>().map(|i| i.id()) else {
        return;
    };
    let joined: Vec<(TopicId, Entity)> = world.resource::<Rooms>().joined().collect();
    let now = world.resource::<Time<Real>>().elapsed_secs_f64();

    // Adopt: a `Shared` entity nobody has bookkeeping for yet is a new local one.
    let fresh: Vec<Entity> = world
        .query_filtered::<Entity, (With<Shared>, Without<Replica>)>()
        .iter(world)
        .collect();
    for entity in fresh {
        let id = *world.get::<NetId>(entity).expect("Shared requires NetId");
        let mut e = world.entity_mut(entity);
        e.insert(Replica::local());
        if !e.contains::<super::Owner>() {
            e.insert(super::Owner(me));
        }
        world.resource_mut::<NetIds>().insert(id, entity);
    }

    // Everything local, and where it is shared.
    let locals: Vec<Local> = world
        .query_filtered::<(Entity, &NetId, &Shared), (With<Replica>, Without<Remote>)>()
        .iter(world)
        .map(|(entity, id, shared)| Local {
            entity,
            id: *id,
            authority: shared.authority,
            topics: Vec::new(),
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|mut l| {
            l.topics = topics_for(world, l.entity, &joined);
            l
        })
        .collect();

    // Spawns: every (entity, topic) pair not yet announced.
    world.resource_scope(|world, registry: Mut<Registry>| {
        let mut spawns: Vec<(TopicId, wire::Spawn, Entity)> = Vec::new();
        for l in &locals {
            let replica = world.get::<Replica>(l.entity).unwrap();
            let missing: Vec<TopicId> = l
                .topics
                .iter()
                .copied()
                .filter(|t| !replica.announced.contains(t))
                .collect();
            if missing.is_empty() {
                continue;
            }
            let spawn = wire::Spawn {
                id: l.id,
                owner: me,
                authority: l.authority,
                lamport: replica.lamport,
                components: encode_all(world, &registry, l.entity),
            };
            for topic in missing {
                spawns.push((topic, spawn.clone(), l.entity));
            }
        }
        let iroh = world.resource::<Iroh>();
        for (topic, spawn, _) in &spawns {
            broadcast(iroh, *topic, spawn);
        }
        for (topic, _, entity) in spawns {
            world
                .get_mut::<Replica>(entity)
                .unwrap()
                .announced
                .insert(topic);
        }
    });

    // Changes, per codec that is due, merged per entity.
    let mut changed: HashMap<Entity, Vec<wire::Component>> = HashMap::new();
    world.resource_scope(|world, mut registry: Mut<Registry>| {
        let mut out = Vec::new();
        for entry in registry.entries_mut() {
            if now - entry.last_sent < entry.rate.interval() {
                continue;
            }
            entry.last_sent = now;
            out.clear();
            entry.codec.collect(world, &mut out);
            for (entity, bytes) in out.drain(..) {
                changed.entry(entity).or_default().push((entry.key, bytes));
            }
        }
    });
    let mut updates: HashMap<TopicId, Vec<wire::EntityUpdate>> = HashMap::new();
    for l in &locals {
        let Some(components) = changed.remove(&l.entity) else {
            continue;
        };
        let mut replica = world.get_mut::<Replica>(l.entity).unwrap();
        replica.lamport += 1;
        let lamport = replica.lamport;
        let announced: Vec<TopicId> = l
            .topics
            .iter()
            .copied()
            .filter(|t| replica.announced.contains(t))
            .collect();
        for topic in announced {
            updates.entry(topic).or_default().push(wire::EntityUpdate {
                id: l.id,
                lamport,
                components: components.clone(),
            });
        }
    }

    // Removals and despawns the observers noticed.
    let mut outbox = std::mem::take(&mut *world.resource_mut::<Outbox>());
    for (id, ..) in &outbox.despawned {
        world.resource_mut::<NetIds>().remove(*id);
    }
    let despawned_ids: Vec<NetId> = outbox.despawned.iter().map(|(id, ..)| *id).collect();
    outbox
        .removed
        .retain(|(id, ..)| !despawned_ids.contains(id));

    // Manifests, once a second per topic.
    let mut manifest_out: Vec<(TopicId, wire::Manifest)> = Vec::new();
    for (topic, _) in &joined {
        let due = manifests
            .get(topic)
            .is_none_or(|last| now - last >= MANIFEST_EVERY);
        if !due {
            continue;
        }
        manifests.insert(*topic, now);
        let entries: Vec<(NetId, u64)> = locals
            .iter()
            .filter(|l| {
                world
                    .get::<Replica>(l.entity)
                    .is_some_and(|r| r.announced.contains(topic))
            })
            .map(|l| (l.id, world.get::<Replica>(l.entity).unwrap().lamport))
            .collect();
        if !entries.is_empty() {
            manifest_out.push((*topic, wire::Manifest { entries }));
        }
    }

    // Snapshots for newcomers and for peers that asked.
    let pending = std::mem::take(&mut world.resource_mut::<Snapshots>().pending);
    let mut snapshot_out: Vec<(TopicId, EndpointId, wire::Snapshot)> = Vec::new();
    world.resource_scope(|world, registry: Mut<Registry>| {
        for (topic, to, ids) in pending {
            let spawns: Vec<wire::Spawn> = locals
                .iter()
                .filter(|l| ids.as_ref().is_none_or(|ids| ids.contains(&l.id)))
                .filter(|l| {
                    world
                        .get::<Replica>(l.entity)
                        .is_some_and(|r| r.announced.contains(&topic))
                })
                .map(|l| wire::Spawn {
                    id: l.id,
                    owner: me,
                    authority: l.authority,
                    lamport: world.get::<Replica>(l.entity).unwrap().lamport,
                    components: encode_all(world, &registry, l.entity),
                })
                .collect();
            if !spawns.is_empty() {
                snapshot_out.push((topic, to, wire::Snapshot { spawns }));
            }
        }
    });

    let iroh = world.resource::<Iroh>();
    for (topic, entities) in updates {
        for chunk in entities.chunks(MAX_ITEMS) {
            broadcast(
                iroh,
                topic,
                &wire::Update {
                    entities: chunk.to_vec(),
                },
            );
        }
    }
    for (id, lamport, key, topics) in outbox.removed {
        for topic in topics {
            broadcast(iroh, topic, &wire::Remove { id, lamport, key });
        }
    }
    for (id, lamport, topics) in outbox.despawned {
        for topic in topics {
            broadcast(iroh, topic, &wire::Despawn { id, lamport });
        }
    }
    for (topic, manifest) in manifest_out {
        for chunk in manifest.entries.chunks(MAX_ITEMS) {
            broadcast(
                iroh,
                topic,
                &wire::Manifest {
                    entries: chunk.to_vec(),
                },
            );
        }
    }
    for (topic, to, snapshot) in snapshot_out {
        for chunk in snapshot.spawns.chunks(MAX_ITEMS) {
            send_to(
                iroh,
                topic,
                to,
                &wire::Snapshot {
                    spawns: chunk.to_vec(),
                },
            );
        }
    }
}
