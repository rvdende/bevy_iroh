//! The seam between tokio and Bevy.
//!
//! One named OS thread, `"iroh"`, owns a small tokio runtime, the endpoint, the router and
//! every subscription, and never touches the `World`. Two unbounded tokio channels of plain
//! owned data cross the seam: an unbounded `send` is synchronous, so a system posts directly,
//! while the tokio side still awaits `recv`. One system drains the other direction each frame.
//! On wasm there is no thread: the page's event loop is the runtime.

use std::{
    future::Future,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use bevy::prelude::*;
use iroh::{Endpoint, EndpointId, SecretKey, protocol::DynProtocolHandler};
use iroh_gossip::proto::TopicId;
use tokio::sync::{mpsc, oneshot};

use crate::net::{
    self, Config, Kind, Node, Payload, Relays, RoomTicket, Topic,
    stats::{SAMPLE_EVERY, Sampler, Stats},
};

/// How long a host waits for a relay before minting a ticket anyway. Without a relay the
/// ticket carries direct addresses only and works on the local network.
const RELAY_WAIT: Duration = Duration::from_secs(5);

/// A room entity's bits, carried out and back so an answer is a lookup rather than a guess.
pub type RoomId = u64;

/// Where the secret key comes from.
#[derive(Debug, Clone, Default)]
pub enum Identity {
    /// A fresh key each run. Fine for a demo; peers will not recognise you next time.
    #[default]
    Ephemeral,
    /// Load from this file, creating it on first run (32 raw bytes, mode 0600).
    File(PathBuf),
    Key(SecretKey),
}

pub(crate) enum ToNet {
    /// How to introduce ourselves, and how often. Sent once at startup and again whenever
    /// the display name changes.
    Presence {
        name: String,
        every: Duration,
    },
    Host {
        room: RoomId,
        name: String,
    },
    Join {
        room: RoomId,
        ticket: RoomTicket,
    },
    Leave {
        topic: TopicId,
    },
    Broadcast {
        topic: TopicId,
        kind: Kind,
        body: Vec<u8>,
    },
    /// Who to measure round trips to.
    Peers(Vec<(EndpointId, Option<String>)>),
    SendTo {
        topic: TopicId,
        to: EndpointId,
        kind: Kind,
        body: Vec<u8>,
    },
}

pub(crate) enum FromNet {
    Joined {
        room: RoomId,
        topic: TopicId,
        ticket: RoomTicket,
        /// `false` when the host gave up waiting for a relay: the ticket is LAN-only.
        relayed: bool,
    },
    Failed {
        room: Option<RoomId>,
        why: String,
    },
    Event(net::Event),
    Stats(Stats),
}

/// The last once-a-second sample of the wire: bytes, rates, relay share, a round trip per
/// peer, and this node's current address. `None` until the first sample.
#[derive(Resource, Default, Debug)]
pub struct NetStats(pub Option<Stats>);

/// The running node, as a resource. Present once the endpoint is bound.
///
/// Also the escape hatch: [`Iroh::endpoint`] is the real `iroh::Endpoint`, and
/// [`Iroh::spawn`] runs any future on the network runtime.
#[derive(Resource)]
pub struct Iroh {
    id: EndpointId,
    #[cfg(not(target_arch = "wasm32"))]
    endpoint: Endpoint,
    /// Filled once the endpoint has bound, which a page cannot wait for synchronously.
    #[cfg(target_arch = "wasm32")]
    endpoint: Arc<Mutex<Option<Endpoint>>>,
    display_name: String,
    to_net: mpsc::UnboundedSender<ToNet>,
    from_net: mpsc::UnboundedReceiver<FromNet>,
    stop: Arc<AtomicBool>,
    #[cfg(not(target_arch = "wasm32"))]
    runtime: tokio::runtime::Handle,
    /// The network thread: it ends once the node is shut down, its endpoint closed and off the
    /// relay.
    #[cfg(not(target_arch = "wasm32"))]
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Iroh {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Iroh {
    /// This node's public key: what peers dial and what signs every message.
    pub fn id(&self) -> EndpointId {
        self.id
    }

    /// The endpoint every protocol shares. Dial peers on your own ALPN with it.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn endpoint(&self) -> Endpoint {
        self.endpoint.clone()
    }

    /// The endpoint every protocol shares. `None` until it has bound, which in a page happens
    /// a few frames after startup rather than before it.
    #[cfg(target_arch = "wasm32")]
    pub fn endpoint(&self) -> Option<Endpoint> {
        self.endpoint
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Add a relay to the running endpoint: for a relay list that arrives from a service after
    /// startup. The endpoint keeps the ones it was built with.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn add_relay(&self, config: iroh::RelayConfig) {
        let endpoint = self.endpoint();
        let _ = self.spawn(async move {
            endpoint
                .insert_relay(config.url.clone(), Arc::new(config))
                .await;
        });
    }

    /// Where this node can be reached right now, for anything that publishes it. In a page,
    /// `None` until the endpoint has bound.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn addr(&self) -> Option<iroh::EndpointAddr> {
        Some(self.endpoint.addr())
    }

    #[cfg(target_arch = "wasm32")]
    pub fn addr(&self) -> Option<iroh::EndpointAddr> {
        self.endpoint().map(|e| e.addr())
    }

    /// Run a future on the network runtime. Poll the returned task from a system.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn spawn<F>(&self, future: F) -> IrohTask<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.runtime.spawn(async move {
            let _ = tx.send(future.await);
        });
        IrohTask { rx }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn spawn<F>(&self, future: F) -> IrohTask<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let (tx, rx) = oneshot::channel();
        n0_future::task::spawn(async move {
            let _ = tx.send(future.await);
        });
        IrohTask { rx }
    }

    pub(crate) fn send(&self, message: ToNet) {
        let _ = self.to_net.send(message);
    }

    pub(crate) fn drain(&mut self) -> Vec<FromNet> {
        let mut out = Vec::new();
        while let Ok(message) = self.from_net.try_recv() {
            out.push(message);
        }
        out
    }
}

/// A future running on the network runtime. `try_take` from a system until it answers.
pub struct IrohTask<T> {
    rx: oneshot::Receiver<T>,
}

impl<T> IrohTask<T> {
    /// `Some` exactly once, when the future has finished. `None` while it runs, and after a
    /// runtime that shut down without answering.
    pub fn try_take(&mut self) -> Option<T> {
        self.rx.try_recv().ok()
    }
}

/// The plugin. `IrohPlugin::default()` is an ephemeral identity on n0's relays.
pub struct IrohPlugin {
    pub identity: Identity,
    pub relays: Relays,
    /// What peers see of you in `Peer::name`.
    pub display_name: String,
    /// How often presence is announced; silence for three of these reaps a peer.
    pub heartbeat: Duration,
    /// Replicate `Transform` at 20 Hz with smoothing. Off, register your own transform codec.
    pub replicate_transform: bool,
    /// Start the node as soon as the plugin is added. Off, the plugin only registers its
    /// systems and nothing touches the network until [`StartIroh`] is queued: an app that
    /// connects only when its user asks.
    pub autostart: bool,
    protocols: Mutex<Vec<(Vec<u8>, Protocol)>>,
}

impl Default for IrohPlugin {
    fn default() -> Self {
        Self {
            identity: Identity::Ephemeral,
            relays: Relays::N0,
            display_name: String::new(),
            heartbeat: Duration::from_secs(5),
            replicate_transform: true,
            autostart: true,
            protocols: Mutex::new(Vec::new()),
        }
    }
}

/// A handler for an extra ALPN: one to hand over once, or a way to make one for each start.
enum Protocol {
    Once(Option<Box<dyn DynProtocolHandler>>),
    Factory(Box<dyn Fn() -> Box<dyn DynProtocolHandler> + Send + Sync>),
}

impl IrohPlugin {
    pub fn with_identity(mut self, identity: Identity) -> Self {
        self.identity = identity;
        self
    }

    pub fn with_relays(mut self, relays: Relays) -> Self {
        self.relays = relays;
        self
    }

    pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = name.into();
        self
    }

    pub fn with_autostart(mut self, autostart: bool) -> Self {
        self.autostart = autostart;
        self
    }

    /// Accept connections on another ALPN, on the same endpoint. How media, or anything else
    /// that is a stream rather than a message, rides along.
    ///
    /// The handler serves the first node only: a node started again after [`StopIroh`] goes
    /// without it. [`IrohPlugin::with_protocol_factory`] serves every one.
    pub fn with_protocol(
        self,
        alpn: impl AsRef<[u8]>,
        handler: impl Into<Box<dyn DynProtocolHandler>>,
    ) -> Self {
        self.protocols
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((alpn.as_ref().to_vec(), Protocol::Once(Some(handler.into()))));
        self
    }

    /// [`IrohPlugin::with_protocol`] with a fresh handler for each node, so the protocol
    /// survives stopping and starting again.
    pub fn with_protocol_factory(
        self,
        alpn: impl AsRef<[u8]>,
        make: impl Fn() -> Box<dyn DynProtocolHandler> + Send + Sync + 'static,
    ) -> Self {
        self.protocols
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((alpn.as_ref().to_vec(), Protocol::Factory(Box::new(make))));
        self
    }
}

/// Starts the node, if it is not running: what [`IrohPlugin::autostart`] does when the plugin
/// is added, and how an app with it off connects later. The node keeps its identity across
/// starts (an ephemeral one too). A failure is logged and left in [`StartError`].
///
/// On native targets this waits for the endpoint to bind, which takes a moment, not the
/// relay: that is reached in the background.
#[derive(Debug, Clone, Default)]
pub struct StartIroh {
    /// Start as this identity instead of the plugin's (an app that switched accounts while
    /// off the network); later starts keep it.
    pub identity: Option<Identity>,
    /// And under this name.
    pub display_name: Option<String>,
}

impl Command for StartIroh {
    type Out = ();

    fn apply(self, world: &mut World) {
        if world.contains_resource::<Iroh>() {
            return;
        }
        if let Some(mut launcher) = world.get_resource_mut::<Launcher>() {
            if let Some(identity) = self.identity {
                launcher.identity = identity;
            }
            if let Some(name) = self.display_name {
                launcher.display_name = name;
            }
        }
        start(world);
    }
}

/// Stops the node: [`Iroh`] goes away and the network thread winds down. Every room says
/// goodbye and is left; its entity stays, [`RoomStatus::Failed`](crate::room::RoomStatus),
/// for the app to despawn or replace, and its [`Peer`](crate::room::Peer)s leave
/// ([`PeerLeft`](crate::room::PeerLeft)). Tasks still running on the network never answer.
pub struct StopIroh;

impl Command for StopIroh {
    type Out = ();

    fn apply(self, world: &mut World) {
        if !world.contains_resource::<Iroh>() {
            return;
        }
        crate::room::leave_all(world, "networking is off");
        #[allow(unused_mut)]
        let mut iroh = world.remove_resource::<Iroh>();
        #[cfg(not(target_arch = "wasm32"))]
        if let (Some(thread), Some(mut launcher)) = (
            iroh.as_mut().and_then(|i| i.thread.take()),
            world.get_resource_mut::<Launcher>(),
        ) {
            launcher.stopping = Some(thread);
        }
        drop(iroh);
        world.resource_mut::<NetStats>().0 = None;
    }
}

/// Why the last [`StartIroh`] (or the plugin's own start) failed. Removed when a node starts.
#[derive(Resource, Debug, Clone)]
pub struct StartError(pub String);

/// What a start needs, kept from the plugin so the node can start at any time, and again.
#[derive(Resource)]
struct Launcher {
    identity: Identity,
    relays: Relays,
    display_name: String,
    heartbeat: Duration,
    protocols: Vec<(Vec<u8>, Protocol)>,
    fast_paths: Arc<net::FastPaths>,
    /// The node stopped last, still shutting down: the next one waits for it, so the relay
    /// never sees two endpoints with one key (it keeps one and drops the other).
    #[cfg(not(target_arch = "wasm32"))]
    stopping: Option<std::thread::JoinHandle<()>>,
}

/// How long a start waits for the node before it to shut down.
#[cfg(not(target_arch = "wasm32"))]
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

impl Launcher {
    fn config(&mut self) -> Config {
        let secret = match &self.identity {
            Identity::Ephemeral => SecretKey::generate(),
            Identity::Key(key) => key.clone(),
            Identity::File(path) => match net::identity::load_or_create(path) {
                Ok(key) => key,
                Err(why) => {
                    error!("bevy_iroh: identity: {why:#}; using an ephemeral key");
                    SecretKey::generate()
                }
            },
        };
        // The same key next time: peers recognise a node that comes back.
        if matches!(self.identity, Identity::Ephemeral) {
            self.identity = Identity::Key(secret.clone());
        }
        let protocols = self
            .protocols
            .iter_mut()
            .filter_map(|(alpn, protocol)| {
                let handler = match protocol {
                    Protocol::Once(handler) => handler.take(),
                    Protocol::Factory(make) => Some(make()),
                };
                if handler.is_none() {
                    warn!(
                        "bevy_iroh: no handler for ALPN {} on this start (with_protocol serves one node)",
                        String::from_utf8_lossy(alpn)
                    );
                }
                handler.map(|h| (alpn.clone(), h))
            })
            .collect();
        Config {
            secret_key: secret,
            relays: self.relays.clone(),
            protocols,
            fast_paths: self.fast_paths.clone(),
        }
    }
}

fn start(world: &mut World) {
    if world.contains_resource::<Iroh>() {
        return;
    }
    let Some(mut launcher) = world.get_resource_mut::<Launcher>() else {
        return;
    };
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(stopping) = launcher.stopping.take() {
        let since = std::time::Instant::now();
        while !stopping.is_finished() && since.elapsed() < SHUTDOWN_WAIT {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !stopping.is_finished() {
            warn!("bevy_iroh: the last node is still shutting down; starting anyway");
        }
    }
    let config = launcher.config();
    let (name, every) = (launcher.display_name.clone(), launcher.heartbeat);
    match spawn(config, name.clone()) {
        Ok(iroh) => {
            info!("bevy_iroh: node {}", iroh.id().fmt_short());
            iroh.send(ToNet::Presence { name, every });
            world.remove_resource::<StartError>();
            world.insert_resource(iroh);
        }
        Err(why) => {
            error!("bevy_iroh: {why}; networking is off");
            world.insert_resource(StartError(why));
        }
    }
}

/// Where this crate's systems run. `Receive` is in `PreUpdate`, `Send` in `PostUpdate`.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IrohSet {
    Receive,
    Send,
}

/// The order inside `IrohSet::Receive`.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Receive {
    Drain,
    Rooms,
    Replicate,
    Messages,
    Sweep,
}

/// Timing knobs shared by the presence systems.
#[derive(Resource, Clone, Copy)]
pub(crate) struct Timing {
    pub heartbeat: Duration,
}

impl Plugin for IrohPlugin {
    fn build(&self, app: &mut App) {
        #[allow(unused_mut)]
        let mut protocols =
            std::mem::take(&mut *self.protocols.lock().unwrap_or_else(|e| e.into_inner()));
        let fast_paths = Arc::new(net::FastPaths::default());
        #[cfg(feature = "media")]
        let hub = {
            let hub = Arc::new(crate::media::MediaHub::new(fast_paths.clone()));
            let for_handler = hub.clone();
            protocols.push((
                crate::media::ALPN.to_vec(),
                Protocol::Factory(Box::new(move || {
                    Box::new(crate::media::transport::MediaHandler {
                        hub: for_handler.clone(),
                    }) as Box<dyn DynProtocolHandler>
                })),
            ));
            hub
        };
        app.insert_resource(Launcher {
            identity: self.identity.clone(),
            relays: self.relays.clone(),
            display_name: self.display_name.clone(),
            heartbeat: self.heartbeat,
            protocols,
            fast_paths,
            #[cfg(not(target_arch = "wasm32"))]
            stopping: None,
        });
        if self.autostart {
            start(app.world_mut());
        }
        app.insert_resource(Timing {
            heartbeat: self.heartbeat,
        });
        app.init_resource::<Inbox>();
        app.init_resource::<NetStats>();
        app.configure_sets(PreUpdate, IrohSet::Receive);
        app.configure_sets(
            PreUpdate,
            (
                Receive::Drain,
                Receive::Rooms,
                Receive::Replicate,
                Receive::Messages,
                Receive::Sweep,
            )
                .chain()
                .in_set(IrohSet::Receive),
        );
        app.configure_sets(PostUpdate, IrohSet::Send);
        app.add_systems(PreUpdate, sweep.in_set(Receive::Sweep));
        app.add_plugins((
            crate::room::RoomPlugin,
            crate::replicate::ReplicatePlugin {
                transform: self.replicate_transform,
            },
        ));
        #[cfg(feature = "media")]
        app.add_plugins(crate::media::MediaPlugin { hub });
    }
}

/// What arrived this frame and has not been claimed yet. Consumers take their kinds out in
/// `Receive` order; whatever is left at `Sweep` is logged as unknown and dropped.
#[derive(Resource, Default)]
pub struct Inbox {
    pub frames: Vec<net::Frame>,
}

impl Inbox {
    /// Remove and return every frame of `kind`, in arrival order.
    pub fn take(&mut self, kind: Kind) -> Vec<net::Frame> {
        self.take_where(|k| k == kind)
    }

    /// Remove and return every frame whose kind matches, in arrival order.
    pub fn take_where(&mut self, mut pred: impl FnMut(Kind) -> bool) -> Vec<net::Frame> {
        self.frames.extract_if(.., |f| pred(f.kind)).collect()
    }
}

fn sweep(mut inbox: ResMut<Inbox>) {
    for frame in inbox.frames.drain(..) {
        // Not an error: this is what a peer running a newer build looks like.
        debug!(
            "bevy_iroh: unknown {} of {} bytes from {}",
            frame.kind,
            frame.body.len(),
            frame.from.fmt_short()
        );
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn(config: Config, display_name: String) -> Result<Iroh, String> {
    let (to_net, to_net_rx) = mpsc::unbounded_channel();
    let (from_net_tx, from_net) = mpsc::unbounded_channel();
    let (ready, started) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread = std::thread::Builder::new()
        .name("iroh".into())
        .spawn(move || {
            // Two workers: iroh's work is I/O, and this runtime shares the machine with the
            // render threads.
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(why) => {
                    let _ = ready.send(Err(format!("tokio runtime: {why}")));
                    return;
                }
            };
            let handle = runtime.handle().clone();
            runtime.block_on(run(
                config,
                to_net_rx,
                from_net_tx,
                move |r| {
                    let _ = ready.send(r.map(|(id, ep)| (id, ep, handle)));
                },
                thread_stop,
            ));
        })
        .map_err(|why| format!("spawn iroh thread: {why}"))?;
    let (id, endpoint, runtime) = started
        .recv()
        .map_err(|_| "the iroh thread died while starting".to_string())??;
    Ok(Iroh {
        id,
        endpoint,
        display_name,
        to_net,
        from_net,
        stop,
        runtime,
        thread: Some(thread),
    })
}

#[cfg(target_arch = "wasm32")]
fn spawn(config: Config, display_name: String) -> Result<Iroh, String> {
    // Blocking the main thread in a page blocks the event loop the endpoint needs in order to
    // bind, so nothing here waits: the id is derivable from the key, the endpoint lands in a
    // slot when bound, and a bind failure arrives as `FromNet::Failed` on a later drain.
    let (to_net, to_net_rx) = mpsc::unbounded_channel();
    let (from_net_tx, from_net) = mpsc::unbounded_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let id = config.secret_key.public();
    let endpoint: Arc<Mutex<Option<Endpoint>>> = Arc::new(Mutex::new(None));
    let slot = endpoint.clone();
    let failed = from_net_tx.clone();
    n0_future::task::spawn(run(
        config,
        to_net_rx,
        from_net_tx,
        move |r| match r {
            Ok((_, ep)) => *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(ep),
            Err(why) => {
                let _ = failed.send(FromNet::Failed { room: None, why });
            }
        },
        stop.clone(),
    ));
    Ok(Iroh {
        id,
        endpoint,
        display_name,
        to_net,
        from_net,
        stop,
    })
}

async fn run(
    config: Config,
    mut commands: mpsc::UnboundedReceiver<ToNet>,
    out: mpsc::UnboundedSender<FromNet>,
    ready: impl FnOnce(Result<(EndpointId, Endpoint), String>),
    stop: Arc<AtomicBool>,
) {
    let node = match Node::spawn(config).await {
        Ok(node) => node,
        Err(why) => {
            ready(Err(format!("start iroh: {why:#}")));
            return;
        }
    };
    let Some(mut events) = node.take_events() else {
        ready(Err("the event stream was already taken".into()));
        return;
    };
    ready(Ok((node.id(), node.endpoint().clone())));

    let mut topics: std::collections::HashMap<TopicId, Topic> = Default::default();
    let mut peers: Vec<(EndpointId, Option<String>)> = Vec::new();
    let mut sampler = Sampler::new();
    let mut sampled = n0_future::time::Instant::now();
    // Heartbeats go out from here rather than from a Bevy system: a page whose tab is hidden
    // stops running frames, and a peer that stops saying hello is reaped by everyone else.
    // This loop keeps running on a throttled timer, which is enough.
    let mut presence: Option<(String, Duration)> = None;
    let mut last_hello = n0_future::time::Instant::now();
    let ctx = node.ctx();
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // On elapsed time rather than on a timer arm: the sleep arm only fires on an idle loop.
        if sampled.elapsed() >= SAMPLE_EVERY {
            sampled = n0_future::time::Instant::now();
            let _ = out.send(FromNet::Stats(sampler.sample(&ctx, &peers)));
        }
        if let Some((name, every)) = &presence
            && last_hello.elapsed() >= *every
        {
            last_hello = n0_future::time::Instant::now();
            if let Ok(body) = (net::proto::Hello { name: name.clone() }).to_body() {
                for joined in topics.values() {
                    if let Err(e) = joined.broadcast(Kind::HELLO, body.clone()).await {
                        debug!("bevy_iroh: hello: {e:#}");
                    }
                }
            }
        }
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break };
                if let ToNet::Peers(list) = command {
                    peers = list;
                    continue;
                }
                if let ToNet::Presence { name, every } = command {
                    presence = Some((name, every));
                    continue;
                }
                let room = match &command {
                    ToNet::Host { room, .. } | ToNet::Join { room, .. } => Some(*room),
                    _ => None,
                };
                if let Err(why) = apply(&node, &mut topics, command, &out).await {
                    let _ = out.send(FromNet::Failed { room, why });
                }
            }
            event = events.recv() => {
                let Some(event) = event else { break };
                let _ = out.send(FromNet::Event(event));
            }
            // So a dropped resource, and a heartbeat due, are noticed by an otherwise idle loop.
            _ = n0_future::time::sleep(Duration::from_millis(200)) => {}
        }
    }
    let _ = node.shutdown().await;
}

async fn apply(
    node: &Node,
    topics: &mut std::collections::HashMap<TopicId, Topic>,
    command: ToNet,
    out: &mpsc::UnboundedSender<FromNet>,
) -> Result<(), String> {
    match command {
        ToNet::Host { room, name } => {
            let relayed = n0_future::time::timeout(RELAY_WAIT, node.online())
                .await
                .is_ok();
            let topic = TopicId::from_bytes(rand::random());
            let joined = node
                .join(topic, Vec::new())
                .await
                .map_err(|e| format!("{e:#}"))?;
            let ticket = joined.ticket(name);
            topics.insert(topic, joined);
            let _ = out.send(FromNet::Joined {
                room,
                topic,
                ticket,
                relayed,
            });
        }
        ToNet::Join { room, ticket } => {
            let topic = ticket.topic;
            let joined = node
                .join(topic, ticket.peers.clone())
                .await
                .map_err(|e| format!("{e:#}"))?;
            // Our ticket for re-sharing: theirs, plus us.
            let mut peers = ticket.peers;
            peers.push(node.addr());
            let ticket = RoomTicket::new(topic, ticket.name, peers);
            topics.insert(topic, joined);
            let _ = out.send(FromNet::Joined {
                room,
                topic,
                ticket,
                relayed: true,
            });
        }
        ToNet::Leave { topic } => {
            if let Some(joined) = topics.remove(&topic) {
                joined.leave().await.map_err(|e| format!("{e:#}"))?;
            }
        }
        ToNet::Broadcast { topic, kind, body } => {
            let joined = topics.get(&topic).ok_or("not in that room")?;
            joined
                .broadcast(kind, body)
                .await
                .map_err(|e| format!("{e:#}"))?;
        }
        ToNet::Peers(_) | ToNet::Presence { .. } => {}
        ToNet::SendTo {
            topic,
            to,
            kind,
            body,
        } => {
            let joined = topics.get(&topic).ok_or("not in that room")?.clone();
            // Off the loop: a dial can take seconds, and nothing else should wait on it.
            n0_future::task::spawn(async move {
                if let Err(e) = joined.send_to(to, kind, body).await {
                    debug!("bevy_iroh: send to {}: {e:#}", to.fmt_short());
                }
            });
        }
    }
    Ok(())
}
