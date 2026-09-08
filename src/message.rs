//! Typed messages between peers: anything that is not an entity.
//!
//! ```ignore
//! app.add_net_message::<Chat>();
//! fn say(mut net: NetSender, rooms: Query<Entity, With<RoomTopic>>) { net.broadcast(room, &Chat { .. }); }
//! fn hear(mut rx: MessageReader<Received<Chat>>) { for m in rx.read() { .. } }
//! ```

use bevy::{ecs::system::SystemParam, prelude::*};
use iroh::EndpointId;
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    net::{Kind, Via},
    node::{Inbox, Iroh, Receive, ToNet},
    room::{RoomTopic, Rooms},
};

/// A message that can travel between peers. Blanket: derive serde and it is one.
pub trait NetMessage: Serialize + DeserializeOwned + Send + Sync + 'static {}
impl<T: Serialize + DeserializeOwned + Send + Sync + 'static> NetMessage for T {}

/// A message from a peer, read with `MessageReader<Received<T>>`.
#[derive(Debug, Clone)]
pub struct Received<T> {
    pub from: EndpointId,
    /// The sender's [`Peer`](crate::room::Peer) entity, if they have said hello.
    pub peer: Option<Entity>,
    pub room: Entity,
    pub via: Via,
    pub msg: T,
}

impl<T: Send + Sync + 'static> Message for Received<T> {}

/// Send messages. `broadcast` is gossip and reaches the whole room; `send_to` dials one peer
/// and reaches only them.
#[derive(SystemParam)]
pub struct NetSender<'w, 's> {
    iroh: Option<Res<'w, Iroh>>,
    rooms: Query<'w, 's, &'static RoomTopic>,
}

impl NetSender<'_, '_> {
    /// `false` if the room is not joined or networking is off.
    pub fn broadcast<T: NetMessage>(&self, room: Entity, msg: &T) -> bool {
        let (Some(iroh), Ok(topic)) = (self.iroh.as_deref(), self.rooms.get(room)) else {
            return false;
        };
        let Ok(body) = postcard::to_stdvec(msg) else {
            return false;
        };
        iroh.send(ToNet::Broadcast {
            topic: topic.0,
            kind: Kind::of::<T>(),
            body,
        });
        true
    }

    /// To every room this node is in.
    pub fn broadcast_all<T: NetMessage>(&self, msg: &T) {
        let Some(iroh) = self.iroh.as_deref() else {
            return;
        };
        let Ok(body) = postcard::to_stdvec(msg) else {
            return;
        };
        for topic in &self.rooms {
            iroh.send(ToNet::Broadcast {
                topic: topic.0,
                kind: Kind::of::<T>(),
                body: body.clone(),
            });
        }
    }

    /// Direct to one peer, which nobody else is party to.
    pub fn send_to<T: NetMessage>(&self, room: Entity, to: EndpointId, msg: &T) -> bool {
        let (Some(iroh), Ok(topic)) = (self.iroh.as_deref(), self.rooms.get(room)) else {
            return false;
        };
        let Ok(body) = postcard::to_stdvec(msg) else {
            return false;
        };
        iroh.send(ToNet::SendTo {
            topic: topic.0,
            to,
            kind: Kind::of::<T>(),
            body,
        });
        true
    }
}

pub trait MessageAppExt {
    /// Register `T` so it can be sent with [`NetSender`] and read as `Received<T>`.
    fn add_net_message<T: NetMessage>(&mut self) -> &mut Self;
}

impl MessageAppExt for App {
    fn add_net_message<T: NetMessage>(&mut self) -> &mut Self {
        self.add_message::<Received<T>>()
            .add_systems(PreUpdate, receive::<T>.in_set(Receive::Messages))
    }
}

fn receive<T: NetMessage>(
    mut inbox: ResMut<Inbox>,
    rooms: Res<Rooms>,
    mut out: MessageWriter<Received<T>>,
) {
    for frame in inbox.take(Kind::of::<T>()) {
        let Some(room) = rooms.by_topic(&frame.topic) else {
            continue;
        };
        match postcard::from_bytes::<T>(&frame.body) {
            Ok(msg) => {
                out.write(Received {
                    from: frame.from,
                    peer: rooms.peer(room, frame.from),
                    room,
                    via: frame.via,
                    msg,
                });
            }
            Err(e) => debug!(
                "bevy_iroh: {} from {}: {e}",
                std::any::type_name::<T>(),
                frame.from.fmt_short()
            ),
        }
    }
}
