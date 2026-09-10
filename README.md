# bevy_iroh

[![crates.io](https://img.shields.io/crates/v/bevy_iroh.svg)](https://crates.io/crates/bevy_iroh)
[![docs.rs](https://docs.rs/bevy_iroh/badge.svg)](https://docs.rs/bevy_iroh)
[![ci](https://github.com/rvdende/bevy_iroh/actions/workflows/ci.yml/badge.svg)](https://github.com/rvdende/bevy_iroh/actions/workflows/ci.yml)
[![pages](https://github.com/rvdende/bevy_iroh/actions/workflows/pages.yml/badge.svg)](https://github.com/rvdende/bevy_iroh/actions/workflows/pages.yml)

```sh
cargo add bevy_iroh                                   # rooms, presence, replication, messages, voice, video, screens, the panel
cargo add bevy_iroh --no-default-features             # the transport alone
cargo add bevy_iroh --no-default-features -F media    # plus voice and video without the Bevy UI
```

Live demos, in a browser tab: [rvdende.github.io/bevy_iroh](https://rvdende.github.io/bevy_iroh/).
Code and issues: [github.com/rvdende/bevy_iroh](https://github.com/rvdende/bevy_iroh).

Share Bevy entities between peers over [iroh](https://iroh.computer): QUIC dialled by public
key, hole punching with relay fallback, gossip for the room, direct streams for one peer. No
server. Tag an entity `Shared` and everyone in the room has it.

```rust
use bevy::prelude::*;
use bevy_iroh::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Component, Serialize, Deserialize, Clone)]
struct Cube;

fn main() {
    App::new()
        .add_plugins((DefaultPlugins, IrohPlugin::default()))
        .replicate::<Cube>()             // Transform is replicated by default
        .add_systems(Startup, setup)
        .add_observer(on_add_cube)
        .run();
}

fn setup(mut commands: Commands) {
    match std::env::args().nth(1) {
        Some(t) => commands.spawn(Room::join(t.parse().unwrap())),
        None => commands.spawn(Room::host("hello")),   // a `Ticket` appears on the entity
    };
    // Mine. Peers get `Cube`, `Transform` and `Owner`, and keep up as it moves.
    commands.spawn((Cube, Shared::default(), Transform::from_xyz(0.0, 0.5, 0.0)));
}

// Runs for my cube and for every cube a peer sends: one code path for visuals.
fn on_add_cube(add: On<Add, Cube>, mut commands: Commands /* , meshes, materials */) {
    commands.entity(add.entity).insert((/* Mesh3d, MeshMaterial3d */));
}
```

Run `cargo run --example cube`, copy the ticket it prints, and run it again with the ticket on
another machine. Each sees the other's cube glide as its owner drives it with the arrow keys.

## What is in it

| | |
|---|---|
| `IrohPlugin` | one tokio thread owning the endpoint; `Identity::{Ephemeral, File, Key}`; `Relays::{N0, Extend, Only, Disabled}`; `with_protocol(alpn, handler)` for your own ALPNs on the same endpoint |
| `Room` | an entity, not a place: `Room::host(name)` / `Room::join(ticket)`, then `RoomStatus`, `Ticket`, `RoomTopic` on it. Despawn it to leave |
| `Peer` | one entity per remote member per room, `MemberOf(room)`; `PeerJoined` / `PeerLeft` messages; heartbeats every 5 s, reaped after three silent ones |
| `Shared` | replicate this entity into every room I am in, or the one it is `InRoom`. `NetId` is its identity across peers (`NetId::keyed("floor")` for one that everyone spawns), `Owner` who made it, `Remote` on replicas |
| `replicate::<T>()` | derive serde and `T` crosses. `replicate_with(codec)` for anything else: a quantised form, validation, a component from another crate, a smoothing target on arrival (`Codec` has `Source`, `Wire`, `Target`) |
| `Transform` | 20 Hz, replicas ease toward each arrival over an interval learned from real gaps; never extrapolates |
| `add_net_message::<T>()` | typed messages: `NetSender::broadcast` is gossip to the room, `send_to` dials one peer; read `Received<T>` with `via` saying which |
| `Iroh` | the resource: `id()`, `endpoint()`, `spawn(future)` on the network runtime |

On the wire every message is signed by its author, and the body is opaque and length-prefixed
under a hashed kind, so a peer on an older build steps over what it does not know and keeps
working. Received data is treated as hostile: sizes are capped, unknown kinds are logged, and a
codec's `verify` runs before its `decode`.

## Voice and video: the `media` feature

```toml
bevy_iroh = "0.4"   # every feature but `wasm` is on by default
```

That includes `demo`, which gives `cargo run` in a checkout a window of its own (Bevy's
default plugins on Wayland and X11) so it can be the webcam example. An app that brings its
own Bevy can leave it off: `default-features = false, features = ["ui", "webrtc", "v4l2",
"desktop"]`.

Put `Voice` on a shared entity and your microphone goes to everyone in the room. A remote
entity that arrives with `Voice` is played back from where it is, relative to the camera that
carries `AudioListener`. `VoiceLevel` on both is a meter; `Voice::muted` is replicated so peers
can show it. Frames are 20 ms opus over QUIC datagrams on the same endpoint, with a jitter
buffer per track and concealment for what gossip would have retransmitted too late.

Video is the same shape: `VideoFeed` on a shared entity, paired locally with a `VideoInput`.
`VideoInput::camera()` is the camera named in the settings; `VideoInput::new(source)` is any
`VideoSource` of your own (the crate ships a `TestPattern`). H.264 goes out as one QUIC stream
per group of pictures, each subscriber fed by a task of its own, so a peer on a slow link
skips to the next keyframe and nobody else notices. A `VideoFeed` gets a `VideoImage` handle
once a frame is there, remote ones from the decoder and your own as a preview; put it on a
material.

Screen sharing is the `desktop` feature: `VideoInput::desktop()` on a shared entity is a
monitor, window or tab of the person's choosing, and `MediaSettings::share_screen` is the
switch. Turning it on opens the desktop's own picker, through the ScreenCast portal and
PipeWire on Linux or `getDisplayMedia` in a page; the frames are fitted into the entity's `VideoFeed` size, so a
4K monitor goes out as 1080p. When the share is stopped from the other side, the switch turns
itself off. `VideoFeed::live` tells every replica whether frames are coming, and a replica
drops its `VideoImage` when they stop, so a camera turned off or a share ended leaves no
frozen picture behind.

Which devices: `MediaSettings` names the microphone, speaker and camera by id, and
`AudioDevices` / `CameraDevices` are the lists to pick from. Change a setting and the device
is reopened while everything published keeps its track ids. `MicLevel` is the microphone's
level for a meter in a corner.

The `ui` feature is the corner and the meters, built: `commands.spawn(MediaPanel::voice())`
is a mute button, a level bar and a "Devices" button that opens the pickers;
`MediaPanel::video()` adds the camera toggle and, with `desktop`, "Share screen"; `VoiceIndicator`
on any entity with a `VoiceLevel` floats a small green bar over it. So a room with voice is
an entity with `Voice` and one line of UI:

```sh
cargo run --example voice                # prints a ticket
cargo run --example voice -- <ticket>
cargo run                                # the same, plus a camera and a screen (Linux); also `--example webcam`
./scripts/web.sh voice                   # the same, in a browser tab
```

Or with [just](https://just.systems): `just voice`, `just webcam <ticket>`, `just web webcam`,
and `just check` for everything a commit needs. `just` alone lists them.

The buffering is the part that decides whether a call is usable, and it follows what
substrate learned on real calls: capture and playback buffers of 512 frames (11 ms) where the
device allows; a 60 ms jitter target that doubles on an underrun (after a one-second grace)
and halves again after ten clean seconds, capped at 240 ms; an underrun is a whole buffer of
silence rather than a splice; backlog is spent by playing up to 2% fast rather than kept; a
lost frame is rebuilt from the next frame's in-band FEC; muted sends silence so a muted peer
is quiet, not gone. Video that falls more than 150 ms behind a waiting keyframe skips to it.
`VoiceStats` and `VideoStats` on each remote track carry the counters.

On a desktop this is cpal, libopus and openh264 (the last two built from source with cmake);
headphones, since there is no echo cancellation. In a browser it is Web Audio and WebCodecs,
which do have echo cancellation, and the two decode each other: opus is opus and the H.264 is
Annex B both ways. Cameras on a desktop come with `media`: AVFoundation on macOS and Media
Foundation on Windows, in `media::camera`. On Linux they are the `v4l2` feature: V4L2 capture
in `media::v4l2`, with a `Webcam` entity that puts a camera on a plane and converts YUV on the
GPU (zero-copy from the driver's DMA-BUF with the `dmabuf` feature), and an app that already
runs one can share its capture with `VideoInput::new(feed.capture.color_tap())`. Anywhere
else, give the entity a `VideoSource` of your own. Screens on a desktop are the `desktop`
feature, in `media::desktop`: the ScreenCast portal and PipeWire on Linux (a picker for a
monitor or window; building needs the PipeWire headers and clang), ScreenCaptureKit on macOS
(the main display; Screen Recording permission takes a relaunch to apply), Desktop
Duplication on Windows (the primary output, without the pointer); every browser has
`getDisplayMedia`. Microphones and speakers are cpal everywhere.

## Browsers: the `wasm` feature

The core transport runs in a page as it is: iroh reaches the relays over WebSockets. The
`wasm` feature adds an identity kept in `localStorage` (`web::stored_identity`) and the ticket
out of the address bar (`web::ticket_from_url("join")`). `scripts/web.sh [example]` builds an
example for the browser and serves it; open `http://localhost:8000/?join=<ticket>` from a
native `voice`, `webcam` or `host` run. A page needs a secure context, https or localhost,
for its microphone and camera and for iroh's relay probes; `HTTPS=1 ./scripts/web.sh voice`
serves a self-signed certificate for testing from another machine on the LAN. A browser lists
devices only after its permission prompt: "Devices" asks. WebCodecs is behind
web-sys's unstable cfg, which `.cargo/config.toml` sets for the wasm target; copy that into
your own project. `tests/wasm.rs` runs two apps in one page against the real relay under
`wasm-bindgen-test` (`cargo test --profile wasm-test --target wasm32-unknown-unknown
--features wasm --test wasm` with geckodriver or chromedriver on `PATH`).

## Browsers without the relay: the `webrtc` feature

A page cannot accept a QUIC connection, so on its own it reaches every peer through a relay,
and from far away that is most of the latency in a call. With `webrtc` (on by default) a page
offers a WebRTC connection to every peer it sees and a desktop answers: two data channels,
one unreliable and unordered for the voice frames and one reliable for video and control,
carrying the same bytes the QUIC path carries. The browser's own ICE does the hole punching;
`str0m` is the desktop side, on the iroh runtime, with a STUN binding for the public address.
Signalling is one typed message over the room, so there is no extra server. When the link is
up, everything to and from that peer moves onto it: voice and video, and also replication,
presence and typed messages, which go on a third reliable channel of their own so a video
keyframe never delays a move. Room broadcasts still go to gossip as well, and receivers drop
the copy that arrives second, so the link only ever wins the race. `Received::via` says
`Via::WebRtc` for what came that way. When the link drops, everything moves back to QUIC and
the page offers again. `MediaPath` on each `Peer` and remote media entity says which path is
in use, and `RtcSettings` holds the STUN servers and whether this node offers. Desktops do not
offer to desktops by default, since a QUIC dial already hole punches; the two-app test turns
that on to exercise the path locally.

## Status

Rooms, presence, replication, messages, voice, video and screen sharing work, each with a
two-app integration test over real iroh (`cargo test`); a browser
peer joins a native host and hears and sees it, over a relay or hole-punched through WebRTC. Next: substrate moving
onto this crate. See `PLAN.md`.

## License

MIT or Apache-2.0, at your option.
