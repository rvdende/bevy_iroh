# bevy_iroh — plan

**Status (2026-09-08):** M1–M3 built and pushed to github.com/rvdende/bevy_iroh: node, rooms,
presence, replication with codecs, typed messages, the cube example, and the two-app test.
0.1.0 is on crates.io. M5 voice is built: the `media` feature, opus over QUIC datagrams on a
`bevy_iroh/media/1` ALPN of our own rather than the MoQ stack, so nothing depends on iroh-live
or on any git crate. M6 video and M7 browser are next.

A Bevy plugin that makes entities shareable between peers over [iroh](https://iroh.computer):
QUIC dialled by public key, hole punching with relay fallback, gossip for the group, direct
streams for one peer. Clean sheet, informed by what `robot2/crates/substrate-net` and
`substrate/src/sceneobjects/iroh` learned the hard way.

Targets: **Bevy 0.19.1**, **iroh 1.1**, **iroh-gossip 0.101**, edition 2024. Native first;
every seam that wasm needs (`n0-future` for tasks and timers, no `std::thread`, `web-time`) is
kept from the first commit, but the browser build is verified in a later milestone.

---

## 1. The hello world this must make true

```rust
use bevy::prelude::*;
use bevy_iroh::prelude::*;

#[derive(Component, Serialize, Deserialize, Clone)]
struct Cube;                                   // "what this is"; peers spawn visuals from it

fn main() {
    App::new()
        .add_plugins((DefaultPlugins, IrohPlugin::default()))
        .replicate::<Cube>()                   // Transform is replicated by default
        .add_systems(Startup, setup)
        .add_systems(Update, (drive, print_ticket))
        .add_observer(dress_cube)
        .run();
}

fn setup(mut commands: Commands) {
    match std::env::args().nth(1) {
        Some(t) => commands.spawn(Room::join(t.parse().expect("ticket"))),
        None    => commands.spawn(Room::host("hello")),
    };
    // Mine. Shared with every room I am in. Peers get `Cube` + `Transform` + `Owner`.
    commands.spawn((Cube, Shared::default(), Transform::from_xyz(0.0, 0.5, 0.0)));
}

// Runs for my cube and for every cube a peer sends me: one code path for visuals.
fn dress_cube(add: On<Add, Cube>, mut commands: Commands, /* meshes, materials */) {
    commands.entity(add.entity).insert((Mesh3d(..), MeshMaterial3d(..)));
}

fn drive(mut q: Query<&mut Transform, (With<Cube>, Without<Remote>)>, keys: Res<ButtonInput<KeyCode>>) { .. }

fn print_ticket(q: Query<&Ticket, Added<Ticket>>) {
    for t in &q { println!("join with: cargo run --example cube -- {t}"); }
}
```

Run one instance, copy the ticket, run a second with it: each sees the other's cube move.
`examples/cube.rs` is exactly this and is the acceptance test for the API.

---

## 2. Design decisions

### 2.1 Layers

```
 app       Cube, Health, Chat …            user components and messages
 ─────────────────────────────────────────────────────────────────────
 bevy_iroh::replicate   Shared, NetId, Owner, Remote, InRoom, replicate::<T>()
 bevy_iroh::message     add_net_message::<T>(), NetSender, Received<T>
 bevy_iroh::room        Room, Ticket, Members, Peer, presence
 bevy_iroh::node        IrohPlugin, Iroh resource, runtime thread, bridge
 bevy_iroh::net         (no Bevy in it) endpoint, gossip, direct, proto, ticket, identity
 ─────────────────────────────────────────────────────────────────────
 iroh / iroh-gossip     transport
```

`net` keeps substrate-net's rule — nothing in it knows a `World` exists — as a module rather
than a crate. The parts substrate-net carried that are not iroh at all (`api.rs`, `http.rs`,
the instance registry client) **stay in substrate**; they are product, not transport.

### 2.2 The runtime seam (kept from substrate, proven)

One named OS thread `"iroh"` owning a 2‑worker tokio runtime, the `Endpoint`, the `Router`,
gossip and every subscription. It never touches the `World`. Two `tokio::sync::mpsc::unbounded`
channels of plain owned data (`ToNet` in, `FromNet` out): the unbounded `send` is synchronous so
a system posts directly, the tokio side awaits `recv`. One system drains `FromNet` each frame.
Requests carry the requesting `Entity` as `u64` bits so answers are lookups, not guesses.
`Drop` of the resource stops the thread. On wasm the "thread" is `spawn_local` and startup
failure arrives on the first drain instead of blocking.

The `Iroh` resource is also the escape hatch for the "powerful" half: `Iroh::endpoint()` hands
out the real `iroh::Endpoint`, `Iroh::spawn(fut)` runs a future on the runtime, and
`IrohPlugin::with_protocol(alpn, handler)` registers extra ALPNs on the router before bind.
Substrate's MoQ media rides through that, unchanged in shape, without bevy_iroh knowing what
MoQ is.

### 2.3 Identity and endpoint

- `Identity::Ephemeral` (default), `Identity::File(path)` (32 raw bytes, 0600, wrong length is
  an error not a regeneration), `Identity::Key(SecretKey)`.
- `Endpoint::builder(presets::N0)` then `.relay_mode(..)` **on top** — builder calls after a
  preset override it, so substrate's hand‑copied `preset::Extended` and its "this can drift"
  warning are unnecessary. `Relays::N0` (default), `Relays::Extend(vec)` (n0's plus yours, what
  substrate wants: an African relay without a single point of failure), `Relays::Only(vec)`,
  `Relays::Disabled` (LAN and tests).
- `MemoryLookup` as a manual address book so a ticket's addresses are dialable before any
  discovery answers.
- `online()` is bounded (5 s) and only gates *minting a ticket*, never joining. It is per room
  request, not per process.

### 2.4 Rooms, tickets, presence

A **room is an entity**; the topic is 32 random bytes and is the room's identity and capability.
A room has **no spatial meaning**: no transform, no bounds, no geometry. It is a membership
handle — who is subscribed to this topic and which entities are shared into it. Substrate's
"drag it into the box and it is shared" is an app rule built on top: the app watches
containment and inserts or removes `Shared` (and `InRoom`) itself. Any other rule — a
button, a team, a distance — is equally valid, and the crate never requires one.

```rust
commands.spawn(Room::host("name"));          // Ticket inserted once mintable
commands.spawn(Room::join(ticket));          // Ticket inserted immediately (parsed locally)
```

Components on the room: `Room { name }`, `RoomStatus { Connecting | Joined | Failed(String) }`,
`Ticket(RoomTicket)`, `Members` (relationship target), `SharedEntities` (relationship target).
Despawning the room leaves the topic (substrate never did: "every object in that room ghosts
on peers"), despawns remote entities that were only in it, and strips sharing from local ones.

**Peers are entities**: one `Peer { id: EndpointId, name: Option<String>, last_heard }` per
remote member per room, related by `MemberOf(room)`. Membership is inferred from signed
traffic, not from gossip neighbour events — a gossip mesh does not make every member your
neighbour. Every member broadcasts `Hello { name }` on join and every 5 s; a peer is created on
first `Hello`, renamed when a later one carries a name, despawned on `Goodbye` or after 15 s of
silence (three heartbeats: "one missed message is ordinary on a gossip mesh"). Apps react with
`On<Add, Peer>` / `On<Remove, Peer>` or `Query<&Peer>`.

`RoomTicket { topic, name, peers: Vec<EndpointAddr> }` via the `iroh_tickets::Ticket` trait.
Wire layout kept byte‑compatible with `substrate-ticket` so the running registry keeps minting
valid tickets (open question §5).

### 2.5 Wire protocol (kept from substrate, with one simplification)

Every frame: `Signed { from, envelope: Vec<u8>, sig }` around
`Envelope { version, topic, kind, audience, sent_at_ms, body: Vec<u8> }`, postcard, signed
because gossip forwards through peers you did not choose. The body is **opaque and length
prefixed**, never a nested enum: an old peer steps over a kind it does not know and keeps
working. `two_apps` asserts this and it is the test to keep green.

Simplification: `kind` becomes a `u64` FNV‑1a hash of a stable name (`"bevy_iroh/hello/1"`,
or a user type's `type_path`), instead of hand‑allocated `u16` ranges. Nothing to coordinate,
nothing to collide by accident, and the same scheme keys replicated components.

Transports and the rule for choosing: **if the whole room should see it, gossip; if it
concerns two peers, dial** — one request, one response, one bi stream, connection per
exchange until a workload needs pooling. `Audience::Only` is addressing, not confidentiality.

Untrusted input is a contract: `MAX_FRAME = 1 MiB`, `MAX_ITEMS`, lengths checked before
allocation, unknown kinds reported and skipped, strict `VERSION` equality.

### 2.6 Replication: components, not kinds

Substrate replicates a per‑kind opaque body via `SceneObject::encode/apply`, because its
registry is a list of kinds. A general plugin has no kinds; it has components, and the people
using it must be able to say how a component crosses the wire **without forking and without a
trait on a type they do not own**. So the unit of extension is a **codec**, separate from the
component:

```rust
pub trait Codec: Send + Sync + 'static {
    type Source: Component;                   // what the owner reads and change‑detects
    type Wire: Serialize + DeserializeOwned;  // what crosses the network
    type Target: Bundle;                      // what the receiver inserts (usually = Source)
    const NAME: &'static str;                 // stable wire name, hashed into the key

    fn encode(&self, src: &Self::Source, cx: &EncodeCx) -> Option<Self::Wire>;
    fn verify(&self, wire: &Self::Wire) -> Result<(), Rejected> { Ok(()) }  // hostile‑input gate
    fn decode(&self, wire: Self::Wire, cx: &mut DecodeCx) -> Result<Self::Target, Rejected>;
}

app.replicate::<Health>()                     // sugar: Serde::<Health> — derive serde, done
app.replicate_with(QuantisedTransform { step: 0.001 })
app.replicate_with(VelocityCodec)             // avian's LinearVelocity, no newtype, no fork
```

`EncodeCx` carries `&World`, the entity and the id map; `DecodeCx` carries `&mut World`, the
target entity and the id map (`net_id(Entity)`, `entity(NetId)` — the latter spawns a
placeholder for an id that has not arrived yet, so out‑of‑order gossip cannot lose a parent).

How the cases land:

| want | do |
|---|---|
| plain data | `#[derive(Component, Serialize, Deserialize, Clone)]` + `replicate::<T>()` |
| custom wire form, validation, subset of fields | a codec struct; config lives on the instance |
| a third‑party component | a codec whose `Source` is that component |
| asset‑backed visuals | keep them local; replicate a `Prefab` description and dress it in `On<Add, Prefab>` (recommended). Or encode through `Assets` in `EncodeCx` and load in `DecodeCx` |
| entity references | map through the context; `ChildOf` ships as a built‑in codec doing exactly this |
| smoothing / reconcile on receive | `Target != Source`: the built‑in transform codec targets `Glide`, a system eases `Transform` toward it |
| substrate's kinds | one codec with `Source = Body { kind, bytes }`, kind dispatch inside it |

Codecs get world access, so encode and decode each run in one exclusive system per frame,
gated on having work; the registry caches a `QueryState<(Entity, &NetId, Ref<Source>), Changed<Source>>`
per codec so change detection is Bevy's own ticks — no per‑frame re‑encode‑and‑diff, which
substrate's comments flag as a cost that grows with object count. Changes are batched per room
per frame into one message. No proc‑macro crate: the macro people apply is serde's.

Components on a shared entity:

| component | who sets it | meaning |
|---|---|---|
| `Shared { authority }` | the app | replicate this entity. `Authority::Owner` (default) or `Anyone` |
| `NetId` | required by `Shared`; random `u64` unless given | identity across peers |
| `Owner(EndpointId)` | plugin | who spawned it; the tie‑break |
| `Remote` | plugin, on replicas | ergonomic `Without<Remote>` for "mine" |
| `InRoom(room)` | app, optional | scope. Absent = every room this node is in |
| `Glide` | plugin, on remote `Transform`s | interpolation target |

`Shared::keyed("floor")` derives `NetId` from `hash(topic, key)` — substrate's "identity before
transport" lesson: two peers spawning the same keyed thing get one object, no election.

Wire (replication namespace):

```rust
Spawn   { id, owner, lamport, components: Vec<(u64, Vec<u8>)> }
Update  { batch: Vec<(id, lamport, components)> }         // per frame, per room
Remove  { id, component: u64 }
Despawn { id, lamport }
Manifest{ ids: Vec<(id, lamport)> }                       // every 1 s; anti‑entropy for lost gossip
SnapshotRequest / Snapshot { spawns }                     // direct, to a peer who is missing ids
```

Authority: per entity. `Owner`: only the owner's changes go out; replicas never send. `Anyone`:
any peer's local edit goes out, last writer wins by `(lamport, node_id)` — substrate specified
the node‑id tie‑break and never built it. Echo suppression: the apply system records the change
tick it wrote at; the collect system skips components whose `last_changed()` equals it.

Late joiners: on a new `Peer`, each node sends a `Snapshot` of **its own** entities, direct
(substrate's actual behaviour, better than the doc's lowest‑node‑id election: no election,
nothing sent twice). `Manifest` covers gossip loss afterwards: a receiver that sees an id it
lacks asks the owner for it.

Despawn propagates. `On<Remove, Shared>` and entity despawn both send `Despawn`; the id map is
cleaned on the way past. Both were gaps in substrate.

`Transform` is registered by default at 20 Hz. Replicas carry `Glide` and interpolate over an
adaptively learned interval (EMA of arrival gaps × 1.75, clamped 30–250 ms) — substrate's
measured fix for a median 56 ms / p90 102 ms arrival spread — and never extrapolate.
`ChildOf` is replicated when the parent is also `Shared`, mapped through `NetId`. Other
`Entity`‑bearing components are the app's problem in v1.

### 2.7 Typed app messages

```rust
app.add_net_message::<Chat>();
fn send(mut net: NetSender)          { net.broadcast(room, Chat { .. }); net.send_to(peer, Chat { .. }); }
fn recv(mut rx: MessageReader<Received<Chat>>) { for Received { from, room, via, msg } in rx.read() { .. } }
```

This is what substrate's chat, ping, track announcements and voice descriptors become: a
struct and a registration, no `Kind` table edits. `Via::{Gossip, Direct}` is carried so
"everyone saw that" versus "only you saw that" is never inferred.

### 2.8 Schedule

```
PreUpdate   IrohSet::Receive   drain bridge → room/peer state → apply spawns/updates/despawns
Update      IrohSet::Glide     interpolate remote transforms
PostUpdate  IrohSet::Send      collect changed components → batch → post to bridge; heartbeats
```

Plain systems; observers only where an entity lifecycle *is* the event (room despawn, `Shared`
removed, `Peer` added). Run conditions gate the exclusive apply system on "inbox non‑empty".

### 2.9 Audio and video: the `media` feature

Voice chat and video feeds are part of `bevy_iroh`, behind a **`media`** feature, so a
conference or competition app is one dependency with one flag. Off by default: codecs are
C/C++ builds and cpal is per platform, and the hello‑world cube must not pay for them.

What `media` turns on:

```rust
// publish: my microphone and my camera, on entities peers already know about
commands.spawn((Shared::default(), Voice::default()));                  // opus, push‑to‑talk or open mic
commands.spawn((Shared::default(), VideoFeed::from(source), Transform::..)); // H.264, a plane peers see

// consume: nothing to write. A remote `Voice` plays back spatially from its entity's
// Transform; a remote `VideoFeed` gets an `Image` handle the app puts on any mesh.
```

- **Announce = replication.** `Voice { rate }` and `VideoFeed { w, h, turn, rendition }` are
  replicated components registered by the feature. The MoQ broadcast name derives from
  `NetId`; nothing about it travels. Subscribe when a `Remote` entity gains one, publish when a
  local one does, stop on removal. Bytes never touch gossip.
- **Sources are traits.** `VideoSource` (YUV frames with timestamps, newest wins) and
  `AudioSource` (PCM blocks). The feature ships a cpal microphone source and speaker sink.
  A **`v4l2`** feature (Linux) adds the `bevy_v4l2` adapter so a `Webcam` entity is a source
  with no glue; other cameras and screen capture implement the trait.
- **Playback is the missing half of NETWORKING.md §11 and is built here:** per‑track jitter
  buffer (adaptive 40–200 ms), mixing across peers, gain and pan from the relative `Transform`,
  never playing back your own microphone. Headphones first; echo cancellation is not promised.
- **Transport** is the MoQ stack substrate already runs (hang + moq‑net, published; the
  iroh↔moq session adapter from iroh‑live, git‑only) behind one module boundary, so per‑group
  QUIC streams on raw iroh can replace it. **Publishing constraint:** crates.io refuses git
  dependencies, so before `media` can ship on crates.io the adapter is either published under
  our name or replaced by our own framing. Decide at M5, not before.
- **Scale.** Meetings are flat, everyone direct. Simulcast renditions chosen by screen‑space
  size and the relay tree for a thousand viewers are NETWORKING.md §10 and stay future work;
  `VideoFeed::rendition` exists from the start so the wire does not change when they land.

`bevy_v4l2` needs two small additions for this: a raw full‑YUV tap for encoders (today's
`FrameTap` is luma only, for calibration; optionally the DMA‑BUF fd + layout for a future
VAAPI encoder) and an external feed so a decoder pushes I420/NV12 through the same
`WebcamMaterial` YUV shader. A remote camera is then a plane whose producer is a decoder.

### 2.9a The `wasm` feature

Core transport compiles for `wasm32` unconditionally: `n0-future` for tasks and timers,
`web-time`, no threads, startup reported asynchronously. That needs no flag. The **`wasm`**
feature turns on what only a browser has and only a browser app wants to compile: the
`wasm-bindgen`/`web-sys` surface, identity in `localStorage`, `?join=` from the page URL, and
— with `media` — the WebCodecs and Web Audio backends behind the same `VideoSource`/`AudioSource`
traits, plus the MoQ accept‑forwarding workaround for the router's `Send` disagreement.
`features = ["media", "wasm"]` is a browser conference client.

### 2.10 Not doing

- **bevy_replicon** as a base: it is server‑authoritative client/server; this is peer‑owned
  P2P with no host, which is the model substrate needs. Revisit only if a hosted‑game mode is
  wanted later.
- Reflection‑based serialisation: `Serialize` on the component is simpler, faster, and matches
  postcard. `bevy_world_serialization` could layer on top later.
- Blobs (`iroh-blobs` for meshes and textures by hash), relay trees for concert scale, and
  allowlists or publish tickets: not in M1–M7. Blobs would arrive as a `blobs` feature and a
  built‑in codec; the others are NETWORKING.md §10 and §13.

---

## 3. Crate layout

```
bevy_iroh/
  Cargo.toml            lib; bevy default-features=false (ecs, app, transform, time, log)
                        features: media, v4l2 (implies media, linux), wasm; none on by default
  src/lib.rs            IrohPlugin, IrohSet, prelude, AppExt (replicate, add_net_message)
  src/net/mod.rs        Node: endpoint, router, gossip, subscriptions, shutdown   (from substrate-net lib.rs)
  src/net/proto.rs      Envelope, Signed, Kind hash, encode/decode, limits         (from proto.rs)
  src/net/direct.rs     one request / one response over one bi stream            (from direct.rs)
  src/net/ticket.rs     RoomTicket via iroh_tickets::Ticket                       (from substrate-ticket)
  src/net/identity.rs   key file load_or_create                                   (from identity.rs)
  src/net/stats.rs      bytes, relayed share, per‑peer RTT probe                  (from stats.rs, later)
  src/node.rs           runtime thread, Bridge, ToNet/FromNet, Iroh resource     (from bridge.rs)
  src/room.rs           Room, Ticket, RoomStatus, Peer, MemberOf/Members, presence
  src/replicate/mod.rs  Shared, NetId, Owner, Remote, InRoom, registry, AppExt
  src/replicate/send.rs collect + batch + manifest + snapshot answer
  src/replicate/apply.rs inbox → world, lamport, echo suppression, despawn
  src/replicate/glide.rs Transform interpolation
  src/message.rs        typed messages, NetSender, Received<T>
  src/media/mod.rs      [media] Voice, VideoFeed, publish/subscribe, VideoSource/AudioSource
  src/media/transport.rs[media] MoQ session behind one boundary
  src/media/audio.rs    [media] cpal source/sink, opus, jitter buffer, mixer, spatial gain
  src/media/video.rs    [media] H.264 encode/decode, decoded frames → Image
  src/media/web.rs      [media+wasm] WebCodecs / Web Audio backends
  src/media/v4l2.rs     [media+v4l2] bevy_v4l2 adapter
  src/web.rs            [wasm] localStorage identity, page URL join
  examples/conference.rs [media] voice + camera planes per peer
  examples/cube.rs      the hello world
  tests/two_apps.rs     two Apps in one process, real iroh, no relay
  README.md
```

---

## 4. Milestones

Each one compiles, runs and is demoable on its own.

**M1 — node, rooms, presence.** `net/` lifted and trimmed, `IrohPlugin`, `Iroh` resource,
`Room::host/join`, `Ticket`, `Peer` entities with heartbeat and reaper, `Hello/Goodbye`,
unknown‑kind tolerance. Example prints its ticket and logs peers joining and leaving.
`tests/two_apps.rs`: host + join with `Relays::Disabled`, assert a `Peer` appears on both
within 10 s, assert an unknown kind does not break the room.

**M2 — replication.** `Shared`, `NetId`, registry, `replicate::<T>()`, `Spawn/Update/Despawn`,
`Transform` at 20 Hz with `Glide`, snapshot to new peers, despawn propagation. The cube example
works end to end. Test: spawn on A, assert `Cube + Transform + Remote` on B; move on A, assert
B follows; despawn on A, assert gone on B.

**M3 — messages, authority, resilience.** `add_net_message`, `NetSender`, direct `send_to`,
`Authority::Anyone` with `(lamport, node_id)` and echo suppression, `Manifest` anti‑entropy,
`ChildOf` mapping, `Shared::keyed`. Example gains a chat line and a "claim" key.

**M4 — polish.** `stats`, `Relays::Extend`, identity file, README with the hello world, docs on
every pub item, `cargo test` green, a `wasm32-unknown-unknown` `cargo check` with the
`n0-future` seams in place.

**M5 — voice.** `media` feature, audio only: `Voice`, cpal in/out, opus, jitter buffer,
mixing, spatial gain, own‑mic suppression, MoQ transport behind its boundary. The git‑dependency
decision for publishing. `examples/conference.rs` with voice and push‑to‑talk.

**M6 — video.** `VideoFeed`, H.264 via openh264, `VideoSource` trait, the `v4l2` adapter,
decoded frames onto a plane, the two `bevy_v4l2` additions. Conference example gains cameras.

**M7 — browser.** `wasm` feature: web identity and join URL, then WebCodecs/Web Audio backends
and the accept‑forwarding workaround. The conference example runs in a page against a native
peer.

**Then, in robot2 (separate work):** substrate depends on `bevy_iroh`; `substrate-net` and
`substrate-ticket` are deleted; containment reparents and inserts `Shared + InRoom` instead of
its own `Shared`; `SceneObject::encode/apply` collapse into one replicated
`Body { kind: String, bytes: Vec<u8> }` component; players and voice become ordinary shared
entities; media keeps its MoQ handler via `IrohPlugin::with_protocol`; the registry client
stays in substrate.

---

## 5. Open questions for you

1. **Name of the tag.** `Shared` (proposed: reads as "this entity is shared", matches
   substrate's containment vocabulary) versus `IrohShared` or `Replicated`.
2. **Ticket compatibility.** Keep `substrate-ticket`'s byte layout so the deployed registry
   keeps working unchanged (proposed), or define a fresh format and update the registry.
3. **`Shared` without `InRoom`** = every room this node is in (proposed), versus requiring an
   explicit room.
4. **Wasm.** Decided: a `wasm` feature (M7); core seams kept from M1 and `cargo check`ed in M4.
5. **Codec trait** separate from the component (proposed) versus a trait on the component with
   a derive macro. Decided: media and wasm are features of this crate, not sibling crates.
