//! Share Bevy entities between peers over [iroh](https://iroh.computer).
//!
//! ```ignore
//! App::new()
//!     .add_plugins((DefaultPlugins, IrohPlugin::default()))
//!     .replicate::<Cube>()
//!     .run();
//!
//! commands.spawn(Room::host("hello"));
//! commands.spawn((Cube, Shared::default(), Transform::default()));
//! ```
#![allow(clippy::type_complexity)]

#[cfg(all(feature = "media", not(target_arch = "wasm32")))]
pub mod media;
pub mod message;
pub mod net;
pub mod node;
pub mod replicate;
pub mod room;

pub use iroh;
pub use iroh_gossip::proto::TopicId;
pub use net::{Relays, RoomTicket, Via};
pub use node::{Identity, Iroh, IrohPlugin, IrohSet, IrohTask};

pub mod prelude {
    #[cfg(all(feature = "media", not(target_arch = "wasm32")))]
    pub use crate::media::{AudioListener, MediaSettings, Voice, VoiceLevel};
    pub use crate::{
        Identity, Iroh, IrohPlugin, IrohSet, Relays, RoomTicket, Via,
        message::{MessageAppExt, NetSender, Received},
        replicate::{
            Authority, Codec, DecodeCx, EncodeCx, Glide, InRoom, NetId, Owner, Rate, Rejected,
            Remote, ReplicateAppExt, Shared,
        },
        room::{
            MemberOf, Members, Peer, PeerJoined, PeerLeft, Room, RoomStatus, RoomTopic, Rooms,
            Ticket,
        },
    };
    pub use iroh::EndpointId;
}
