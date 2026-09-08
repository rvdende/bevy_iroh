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

## Status

Rooms, presence, replication, messages and the example work, with a two-app integration test
over real iroh (`cargo test`). Planned next, as features of this crate: `media` (voice and
camera feeds over the same endpoint), `v4l2` (a `bevy_v4l2` camera as a source) and `wasm`
(browser peers). See `PLAN.md`.

## License

MIT or Apache-2.0, at your option.
