//! How a component crosses the wire.
//!
//! The unit of extension is a [`Codec`], separate from the component, so a component you do
//! not own can be replicated without a newtype or a fork. Plain data needs none of this:
//! derive serde and use [`Serde`], which is what `App::replicate::<T>()` does.

use std::marker::PhantomData;

use bevy::{ecs::change_detection::Tick, prelude::*};
use iroh::EndpointId;
use serde::{Serialize, de::DeserializeOwned};

use super::{NetId, NetIds, Remote, Replica};

/// How often a component's changes go out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Rate {
    /// Every frame it changed. Right for discrete state.
    EveryChange,
    /// At most this many times a second, sending the latest value. Right for a transform.
    Hz(f32),
}

impl Rate {
    pub fn hz(hz: f32) -> Rate {
        Rate::Hz(hz)
    }

    pub(crate) fn interval(self) -> f64 {
        match self {
            Rate::EveryChange => 0.0,
            Rate::Hz(hz) => 1.0 / hz.max(0.001) as f64,
        }
    }
}

/// Why a received value was refused. Received data is hostile; refusing is ordinary.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct Rejected(pub String);

impl From<String> for Rejected {
    fn from(s: String) -> Self {
        Rejected(s)
    }
}

impl From<&str> for Rejected {
    fn from(s: &str) -> Self {
        Rejected(s.to_string())
    }
}

/// What the sender sees while encoding.
pub struct EncodeCx<'w> {
    pub world: &'w World,
    pub entity: Entity,
}

impl EncodeCx<'_> {
    /// The [`NetId`] of a shared entity, for a component that refers to one.
    pub fn net_id(&self, entity: Entity) -> Option<NetId> {
        self.world.get::<NetId>(entity).copied()
    }
}

/// What the receiver sees while decoding: the world, the entity being written, and who sent it.
pub struct DecodeCx<'w> {
    pub world: &'w mut World,
    pub entity: Entity,
    pub from: EndpointId,
}

impl DecodeCx<'_> {
    /// The entity for a [`NetId`], spawning a placeholder if it has not arrived yet, so that
    /// out-of-order delivery cannot lose a reference. The placeholder is filled in when its
    /// own spawn arrives.
    pub fn entity(&mut self, id: NetId) -> Entity {
        if let Some(e) = self.world.resource::<NetIds>().get(id)
            && self.world.get_entity(e).is_ok()
        {
            return e;
        }
        let e = self.world.spawn((id, Remote, Replica::placeholder())).id();
        self.world.resource_mut::<NetIds>().insert(id, e);
        e
    }
}

/// A component's wire form.
pub trait Codec: Send + Sync + 'static {
    /// What the owner reads and change-detects.
    type Source: Component;
    /// What crosses the network.
    type Wire: Serialize + DeserializeOwned;
    /// What the receiver inserts. Usually `Source`; a smoothing target when arrival should not
    /// overwrite the local value directly.
    type Target: Bundle;

    /// The stable wire name, hashed into the key. Defaults to the source's type name, which is
    /// stable between builds of the same source; override it to survive a rename.
    fn name(&self) -> &'static str {
        std::any::type_name::<Self::Source>()
    }

    fn rate(&self) -> Rate {
        Rate::EveryChange
    }

    /// `None` sends nothing this time.
    fn encode(&self, src: &Self::Source, cx: &EncodeCx) -> Option<Self::Wire>;

    /// The hostile-input gate. Runs before `decode`, on every arrival.
    fn verify(&self, _wire: &Self::Wire) -> Result<(), Rejected> {
        Ok(())
    }

    fn decode(&self, wire: Self::Wire, cx: &mut DecodeCx) -> Result<Self::Target, Rejected>;
}

/// The codec for plain data: the component itself is the wire form.
pub struct Serde<T> {
    rate: Rate,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Serde<T> {
    pub fn new() -> Self {
        Self {
            rate: Rate::EveryChange,
            _marker: PhantomData,
        }
    }

    pub fn with_rate(mut self, rate: Rate) -> Self {
        self.rate = rate;
        self
    }
}

impl<T> Default for Serde<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Codec for Serde<T>
where
    T: Component + Clone + Serialize + DeserializeOwned,
{
    type Source = T;
    type Wire = T;
    type Target = T;

    fn rate(&self) -> Rate {
        self.rate
    }

    fn encode(&self, src: &T, _cx: &EncodeCx) -> Option<T> {
        Some(src.clone())
    }

    fn decode(&self, wire: T, _cx: &mut DecodeCx) -> Result<T, Rejected> {
        Ok(wire)
    }
}

/// A codec with its types erased, as the registry holds it.
pub(crate) trait ErasedCodec: Send + Sync {
    /// Every local, announced entity whose source changed since the last collect.
    fn collect(&mut self, world: &mut World, out: &mut Vec<(Entity, Vec<u8>)>);
    /// The source as it is now, for a spawn or a snapshot.
    fn encode(&self, world: &World, entity: Entity) -> Option<Vec<u8>>;
    fn apply(
        &self,
        world: &mut World,
        entity: Entity,
        from: EndpointId,
        bytes: &[u8],
    ) -> Result<(), Rejected>;
    fn remove(&self, world: &mut World, entity: Entity);
}

pub(crate) struct Erased<C: Codec> {
    codec: C,
    name: &'static str,
    query: Option<
        QueryState<
            (Entity, Ref<'static, C::Source>, &'static Replica),
            (With<super::Shared>, Without<Remote>),
        >,
    >,
    last_collect: Tick,
}

impl<C: Codec> Erased<C> {
    pub fn new(codec: C) -> Self {
        Self {
            name: codec.name(),
            codec,
            query: None,
            last_collect: Tick::new(0),
        }
    }
}

impl<C: Codec> ErasedCodec for Erased<C> {
    fn collect(&mut self, world: &mut World, out: &mut Vec<(Entity, Vec<u8>)>) {
        if self.query.is_none() {
            self.query = Some(world.query_filtered());
            self.last_collect = world.change_tick();
        }
        let query = self.query.as_mut().expect("just initialised");
        let last = self.last_collect;
        let now = world.change_tick();
        world.last_change_tick_scope(last, |world| {
            for (entity, source, replica) in query.iter(world) {
                if !source.is_changed() || replica.announced.is_empty() {
                    continue;
                }
                // What we just applied from a peer is not ours to echo.
                if replica.applied_tick == Some(source.last_changed()) {
                    continue;
                }
                let cx = EncodeCx { world, entity };
                if let Some(wire) = self.codec.encode(&source, &cx)
                    && let Ok(bytes) = postcard::to_stdvec(&wire)
                {
                    out.push((entity, bytes));
                }
            }
        });
        self.last_collect = now;
    }

    fn encode(&self, world: &World, entity: Entity) -> Option<Vec<u8>> {
        let source = world.get::<C::Source>(entity)?;
        let cx = EncodeCx { world, entity };
        let wire = self.codec.encode(source, &cx)?;
        postcard::to_stdvec(&wire).ok()
    }

    fn apply(
        &self,
        world: &mut World,
        entity: Entity,
        from: EndpointId,
        bytes: &[u8],
    ) -> Result<(), Rejected> {
        let wire: C::Wire =
            postcard::from_bytes(bytes).map_err(|e| Rejected(format!("{}: {e}", self.name)))?;
        self.codec.verify(&wire)?;
        let mut cx = DecodeCx {
            world,
            entity,
            from,
        };
        let target = self.codec.decode(wire, &mut cx)?;
        if let Ok(mut e) = world.get_entity_mut(entity) {
            e.insert(target);
        }
        Ok(())
    }

    fn remove(&self, world: &mut World, entity: Entity) {
        if let Ok(mut e) = world.get_entity_mut(entity) {
            e.remove::<C::Target>();
            e.remove::<C::Source>();
        }
    }
}
