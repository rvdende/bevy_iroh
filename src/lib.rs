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

#[cfg(feature = "media")]
pub mod media;
pub mod message;
pub mod net;
pub mod node;
pub mod replicate;
pub mod room;
#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
pub mod web;

pub use iroh;
pub use iroh_gossip::proto::TopicId;
pub use net::{
    FastPaths, Relays, RoomTicket, Via,
    stats::{Link, Stats},
};
pub use node::{Identity, Iroh, IrohPlugin, IrohSet, IrohTask, NetStats};

pub mod prelude {
    #[cfg(feature = "ui")]
    pub use crate::media::ui::{
        DevicePicker, MediaPanel, MediaUiPlugin, MicMeter, MuteButton, VoiceIndicator,
    };
    #[cfg(feature = "media")]
    pub use crate::media::{
        AudioDevice, AudioDevices, AudioListener, CameraChoice, CameraDevice, CameraDevices,
        MediaSettings, MicLevel, MicrophoneChoice, SpeakerChoice, TestPattern, VideoFeed,
        VideoFeedStats, VideoImage, VideoInput, VideoStats, Voice, VoiceLevel, VoiceStats,
    };
    #[cfg(feature = "webrtc")]
    pub use crate::media::{MediaPath, RtcSettings};
    pub use crate::{
        Identity, Iroh, IrohPlugin, IrohSet, Link, NetStats, Relays, RoomTicket, Stats, Via,
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
