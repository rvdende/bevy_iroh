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

use crate::net::{self, Config, Kind, Node, Relays, RoomTicket, Topic};

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
}

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
    protocols: Mutex<Vec<(Vec<u8>, Box<dyn DynProtocolHandler>)>>,
}

impl Default for IrohPlugin {
    fn default() -> Self {
        Self {
            identity: Identity::Ephemeral,
            relays: Relays::N0,
            display_name: String::new(),
            heartbeat: Duration::from_secs(5),
            replicate_transform: true,
            protocols: Mutex::new(Vec::new()),
        }
    }
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

    /// Accept connections on another ALPN, on the same endpoint. How media, or anything else
    /// that is a stream rather than a message, rides along.
    pub fn with_protocol(
        self,
        alpn: impl AsRef<[u8]>,
        handler: impl Into<Box<dyn DynProtocolHandler>>,
    ) -> Self {
        self.protocols
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((alpn.as_ref().to_vec(), handler.into()));
        self
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
        #[allow(unused_mut)]
        let mut protocols =
            std::mem::take(&mut *self.protocols.lock().unwrap_or_else(|e| e.into_inner()));
        #[cfg(all(feature = "media", not(target_arch = "wasm32")))]
        let hub = {
            let hub = Arc::new(crate::media::MediaHub::default());
            protocols.push((
                crate::media::ALPN.to_vec(),
                Box::new(crate::media::transport::MediaHandler { hub: hub.clone() })
                    as Box<dyn DynProtocolHandler>,
            ));
            hub
        };
        let config = Config {
            secret_key: secret,
            relays: self.relays.clone(),
            protocols,
        };
        match spawn(config, self.display_name.clone()) {
            Ok(iroh) => {
                info!("bevy_iroh: node {}", iroh.id().fmt_short());
                app.insert_resource(iroh);
            }
            Err(why) => error!("bevy_iroh: {why}; networking is off"),
        }
        app.insert_resource(Timing {
            heartbeat: self.heartbeat,
        });
        app.init_resource::<Inbox>();
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
        #[cfg(all(feature = "media", not(target_arch = "wasm32")))]
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
    std::thread::Builder::new()
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
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break };
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
            // So a dropped resource is noticed by an otherwise idle loop.
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
