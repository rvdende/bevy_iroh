# bevy_iroh

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
        .add_observer(dress_cube)
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
fn dress_cube(add: On<Add, Cube>, mut commands: Commands /* , meshes, materials */) {
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
bevy_iroh = { version = "0.2", features = ["media"] }
```

Put `Voice` on a shared entity and your microphone goes to everyone in the room. A remote
entity that arrives with `Voice` is played back from where it is, relative to the camera that
carries `AudioListener`. `VoiceLevel` on both is a meter; `Voice::muted` is replicated so peers
can show it. Frames are 20 ms opus over QUIC datagrams on the same endpoint, with a jitter
buffer per track and concealment for what gossip would have retransmitted too late. Devices
are the defaults unless you insert a `MediaSettings` first; its `AudioSource` and `AudioOutput`
traits are how a test feeds a tone in and reads the mix out (`tests/voice.rs`). Pulls in cpal
and libopus, built with cmake. Headphones: there is no echo cancellation.

Video is the same shape. `VideoFeed` on a shared entity, paired locally with a `VideoInput`
holding any `VideoSource` (the crate ships a `TestPattern`; a camera is a few lines against
your capture library), publishes H.264 through openh264, one QUIC stream per group of
pictures so a stalled group never holds up the next. A remote `VideoFeed` gets a `VideoImage`
handle once its first frame decodes, kept current; put it on a material.

`cargo run --example conference --features media` is a room of spheres that swell when their
owners speak, each with a screen above it.

The buffering is the part that decides whether a call is usable, and it follows what
substrate learned on real calls: a 60 ms jitter target that doubles on an underrun (after a
one-second grace) and halves again after ten clean seconds, capped at 240 ms; an underrun is a
whole buffer of silence rather than a splice; backlog is spent by playing up to 2% fast rather
than kept; a lost frame is rebuilt from the next frame's in-band FEC; muted sends silence so a
muted peer is quiet, not gone. `VoiceStats` on each remote voice carries the counters.

## A camera: the `v4l2` feature

With `features = ["v4l2"]` (Linux), a `bevy_v4l2` webcam is a video source in one line:
`VideoInput::new(feed.capture.color_tap())`.

## Browsers: the `wasm` feature

The core transport runs in a page as it is: iroh reaches the relays over WebSockets. The
`wasm` feature adds an identity kept in `localStorage` (`web::stored_identity`) and the ticket
out of the address bar (`web::ticket_from_url("join")`). `scripts/web.sh` builds the cube
example for the browser and serves it; open `http://localhost:8000/?join=<ticket>` from a
`cargo run --example host`, a headless peer that keeps a room alive with an orbiting cube.
`tests/wasm.rs` runs two apps in one page against the real relay under `wasm-bindgen-test`
(`cargo test --profile wasm-test --target wasm32-unknown-unknown --features wasm --test wasm`
with geckodriver or chromedriver on `PATH`). Voice and video in the browser are not there yet:
the codecs are C builds.

## Status

Rooms, presence, replication, messages, voice and video work, each with a two-app integration
test over real iroh (`cargo test --features media`), and a browser peer joins a native host.
Next: browser media through WebCodecs, and substrate moving onto this crate. See `PLAN.md`.

## License

MIT or Apache-2.0, at your option.
