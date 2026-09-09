//! The transport, with no Bevy in it.
//!
//! One [`Node`] per process holds an iroh endpoint, a gossip instance and a router; any number
//! of [`Topic`]s hang off it. Two transports, chosen by what a message is rather than who sent
//! it: **gossip** for anything the whole room should see, **direct** for anything between two
//! peers. Everything the caller learns arrives as an [`Event`] on one channel of plain owned
//! data. Nothing here knows a `World` exists; `crate::node` is the seam.

pub mod direct;
pub mod identity;
pub mod proto;
pub mod stats;
pub mod ticket;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
};

use anyhow::{Context, Result};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayConfig, RelayMap, RelayMode, SecretKey,
    address_lookup::memory::MemoryLookup,
    endpoint::presets,
    protocol::{DynProtocolHandler, Router},
};
use iroh_gossip::{
    api::{Event as GossipEvent, GossipReceiver, GossipSender},
    net::{GOSSIP_ALPN, Gossip},
    proto::TopicId,
};
use n0_future::StreamExt;
use tokio::sync::mpsc;

pub use proto::{Audience, Kind, Payload};
pub use ticket::RoomTicket;

/// How a message reached us. "Everyone saw that" and "only you saw that" are different enough
/// that the difference should never be guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Via {
    /// A WebRTC data channel to that peer (the `webrtc` feature): hole-punched, and shared
    /// with its voice and video.
    WebRtc,
    Gossip,
    Direct,
}

/// Which relays the endpoint uses.
#[derive(Debug, Clone, Default)]
pub enum Relays {
    /// n0's public relays.
    #[default]
    N0,
    /// n0's relays plus these: a nearer relay without making it a single point of failure.
    Extend(Vec<RelayConfig>),
    /// Only these.
    Only(Vec<RelayConfig>),
    /// No relay at all: direct addresses only. The local network, and tests.
    Disabled,
}

impl Relays {
    fn mode(&self) -> RelayMode {
        match self {
            Relays::N0 => RelayMode::Default,
            Relays::Disabled => RelayMode::Disabled,
            Relays::Only(relays) => RelayMode::Custom(RelayMap::from_iter(relays.iter().cloned())),
            Relays::Extend(relays) => {
                let n0: Vec<Arc<RelayConfig>> = RelayMode::Default.relay_map().relays();
                let all = n0
                    .iter()
                    .map(|c| RelayConfig::clone(c))
                    .chain(relays.iter().cloned());
                RelayMode::Custom(RelayMap::from_iter(all))
            }
        }
    }
}

/// A verified message, attributed to its author.
#[derive(Debug, Clone)]
pub struct Frame {
    pub topic: TopicId,
    pub from: EndpointId,
    pub kind: Kind,
    pub body: Vec<u8>,
    pub sent_at_ms: u64,
    pub via: Via,
}

/// Everything that happens to a node, in the order it happened.
#[derive(Debug, Clone)]
pub enum Event {
    Frame(Frame),
    /// A gossip neighbour came or went. Membership is *not* this: a gossip mesh does not make
    /// every member a neighbour. It is a hint to introduce yourself.
    Neighbor {
        topic: TopicId,
        peer: EndpointId,
        up: bool,
    },
    /// Something went wrong in a place that has no caller to return it to.
    Notice {
        topic: Option<TopicId>,
        text: String,
    },
}

pub struct Config {
    pub secret_key: SecretKey,
    pub relays: Relays,
    /// Extra ALPNs on the router. How media, or anything else that is a stream rather than a
    /// message, rides the same endpoint.
    pub protocols: Vec<(Vec<u8>, Box<dyn DynProtocolHandler>)>,
    /// Peers reachable some other way than iroh: see [`FastPaths`].
    pub fast_paths: Arc<FastPaths>,
}

/// Another way to reach a peer than gossip or a dial: a WebRTC data channel, today. A frame
/// broadcast to a room also goes to every fast path, and a frame for one peer goes only there
/// when there is one. Receivers drop the copy that arrives second, so gossip may keep
/// forwarding as it does and the fast path merely wins the race.
#[derive(Default)]
pub struct FastPaths {
    /// Per peer: hand a signed frame over; `false` when the path is gone.
    links: std::sync::Mutex<HashMap<EndpointId, Arc<dyn Fn(&[u8]) -> bool + Send + Sync>>>,
    /// Where frames that arrived on a fast path go: the node's verify-and-deliver.
    inbound: std::sync::Mutex<Option<Arc<dyn Fn(Vec<u8>) + Send + Sync>>>,
}

impl FastPaths {
    pub fn add(&self, peer: EndpointId, send: impl Fn(&[u8]) -> bool + Send + Sync + 'static) {
        self.links
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(peer, Arc::new(send));
    }

    pub fn remove(&self, peer: EndpointId) {
        self.links
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&peer);
    }

    pub fn has(&self, peer: EndpointId) -> bool {
        self.links
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&peer)
    }

    fn all(&self) -> Vec<(EndpointId, Arc<dyn Fn(&[u8]) -> bool + Send + Sync>)> {
        self.links
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    }

    fn one(&self, peer: EndpointId) -> Option<Arc<dyn Fn(&[u8]) -> bool + Send + Sync>> {
        self.links
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&peer)
            .cloned()
    }

    /// A signed frame that arrived on a fast path.
    pub fn deliver(&self, frame: Vec<u8>) {
        let sink = self
            .inbound
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(sink) = sink {
            sink(frame);
        }
    }

    fn set_inbound(&self, sink: impl Fn(Vec<u8>) + Send + Sync + 'static) {
        *self.inbound.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(sink));
    }
}

/// Frames remembered for duplicate detection. A frame can arrive on a fast path and again
/// through gossip; the second is dropped.
const SEEN: usize = 4096;

/// State shared by every task in the node.
pub struct Ctx {
    endpoint: Endpoint,
    secret: SecretKey,
    events: mpsc::UnboundedSender<Event>,
    topics: RwLock<HashMap<TopicId, Arc<TopicState>>>,
    /// Addresses we have been told about, so a direct dial can skip discovery.
    addrs: RwLock<HashMap<EndpointId, EndpointAddr>>,
    fast_paths: Arc<FastPaths>,
    seen: std::sync::Mutex<(
        std::collections::VecDeque<u64>,
        std::collections::HashSet<u64>,
    )>,
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub struct TopicState {
    topic: TopicId,
    sender: GossipSender,
    task: Mutex<Option<n0_future::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for Ctx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx").field("me", &self.me()).finish()
    }
}

impl Ctx {
    pub fn me(&self) -> EndpointId {
        self.secret.public()
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn emit(&self, event: Event) {
        // The receiver is the application. If it is gone the app is shutting down.
        let _ = self.events.send(event);
    }

    pub fn report(&self, topic: Option<TopicId>, text: impl Into<String>) {
        self.emit(Event::Notice {
            topic,
            text: text.into(),
        });
    }

    pub fn remember(&self, addr: EndpointAddr) {
        self.addrs
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(addr.id, addr);
    }

    /// Where to dial `id`: a known address, or the bare key for discovery to resolve.
    pub fn addr_for(&self, id: EndpointId) -> EndpointAddr {
        self.addrs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
            .unwrap_or_else(|| EndpointAddr::new(id))
    }

    fn topic(&self, topic: &TopicId) -> Option<Arc<TopicState>> {
        self.topics
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(topic)
            .cloned()
    }

    /// Whether this exact frame is new. The signature makes every frame unique.
    fn first_sight(&self, bytes: &[u8]) -> bool {
        let key = fnv1a(bytes);
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if !seen.1.insert(key) {
            return false;
        }
        seen.0.push_back(key);
        if seen.0.len() > SEEN
            && let Some(old) = seen.0.pop_front()
        {
            seen.1.remove(&old);
        }
        true
    }

    fn sign(
        &self,
        topic: TopicId,
        audience: Audience,
        kind: Kind,
        body: Vec<u8>,
    ) -> Result<Vec<u8>> {
        proto::encode_raw(&self.secret, topic, audience, kind, body)
    }

    /// Verify a frame, work out whether it is for us, and act on it. Returns a reply when the
    /// transport has somewhere to write it: a direct stream does, gossip does not.
    pub async fn handle_frame(&self, bytes: &[u8], via: Via) -> Result<Option<Vec<u8>>> {
        let (from, envelope) = proto::decode(bytes)?;
        // Our own broadcasts come back through the swarm.
        if from == self.me() {
            return Ok(None);
        }
        if !self.first_sight(bytes) {
            return Ok(None);
        }
        let Some(state) = self.topic(&envelope.topic) else {
            // A topic we are not in: leaving while a message is in flight is ordinary.
            return Ok(None);
        };
        if !envelope.audience.includes(&self.me()) {
            return Ok(None);
        }
        if envelope.kind == Kind::PING {
            let ping = proto::Ping::from_body(&envelope.body)?;
            let pong = proto::Pong {
                nonce: ping.nonce,
                ping_sent_at_ms: envelope.sent_at_ms,
            };
            let frame = self.sign(
                state.topic,
                Audience::Only(vec![from]),
                Kind::PONG,
                pong.to_body()?,
            )?;
            return match via {
                Via::Direct => Ok(Some(frame)),
                Via::WebRtc => {
                    if let Some(send) = self.fast_paths.one(from) {
                        send(&frame);
                    }
                    Ok(None)
                }
                Via::Gossip => {
                    state.sender.broadcast(frame.into()).await?;
                    Ok(None)
                }
            };
        }
        self.emit(Event::Frame(Frame {
            topic: state.topic,
            from,
            kind: envelope.kind,
            body: envelope.body,
            sent_at_ms: envelope.sent_at_ms,
            via,
        }));
        Ok(None)
    }
}

pub struct Node {
    ctx: Arc<Ctx>,
    relays_disabled: bool,
    gossip: Gossip,
    lookup: MemoryLookup,
    router: Router,
    events: Mutex<Option<mpsc::UnboundedReceiver<Event>>>,
}

impl Node {
    pub async fn spawn(config: Config) -> Result<Self> {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        // The address book tickets are poured into. Without it an address from a ticket is a
        // key we have no route to.
        let lookup = MemoryLookup::new();
        // n0's preset, then our relay choice on top: builder calls after a preset override it.
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(config.secret_key.clone())
            .relay_mode(config.relays.mode())
            .address_lookup(lookup.clone())
            .bind()
            .await
            .context("bind endpoint")?;
        let relays_disabled = matches!(config.relays, Relays::Disabled);
        let gossip = Gossip::builder().spawn(endpoint.clone());
        let ctx = Arc::new(Ctx {
            endpoint: endpoint.clone(),
            secret: config.secret_key,
            events: events_tx,
            topics: RwLock::new(HashMap::new()),
            addrs: RwLock::new(HashMap::new()),
            fast_paths: config.fast_paths.clone(),
            seen: Default::default(),
        });
        // Frames off a fast path go through the same verification as everything else.
        {
            let ctx = ctx.clone();
            config.fast_paths.set_inbound(move |frame| {
                let ctx = ctx.clone();
                n0_future::task::spawn(async move {
                    if let Err(e) = ctx.handle_frame(&frame, Via::WebRtc).await {
                        tracing::debug!("bevy_iroh: fast path frame: {e:#}");
                    }
                });
            });
        }
        let mut router = Router::builder(endpoint)
            .accept(GOSSIP_ALPN, gossip.clone())
            .accept(direct::ALPN, direct::DirectHandler::new(ctx.clone()));
        for (alpn, handler) in config.protocols {
            router = router.accept(alpn, handler);
        }
        Ok(Node {
            ctx,
            relays_disabled,
            gossip,
            lookup,
            router: router.spawn(),
            events: Mutex::new(Some(events_rx)),
        })
    }

    pub fn ctx(&self) -> Arc<Ctx> {
        self.ctx.clone()
    }

    pub fn id(&self) -> EndpointId {
        self.ctx.me()
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.ctx.endpoint
    }

    pub fn addr(&self) -> EndpointAddr {
        self.ctx.endpoint.addr()
    }

    /// Waits until a ticket minted now would name somewhere to dial: a relay, or with relays
    /// off, a direct address. Never returns without one, so callers bound it.
    pub async fn online(&self) {
        if self.relays_disabled {
            use iroh::Watcher;
            let mut addr = self.ctx.endpoint.watch_addr();
            loop {
                if !addr.get().addrs.is_empty() {
                    return;
                }
                if addr.updated().await.is_err() {
                    return;
                }
            }
        } else {
            self.ctx.endpoint.online().await;
        }
    }

    pub fn take_events(&self) -> Option<mpsc::UnboundedReceiver<Event>> {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Subscribe to a topic, dialling `bootstrap` to find the swarm. Empty bootstrap opens a
    /// topic nobody else is in yet; gossip queues what we send until someone arrives.
    pub async fn join(&self, topic: TopicId, bootstrap: Vec<EndpointAddr>) -> Result<Topic> {
        let me = self.id();
        for addr in &bootstrap {
            if addr.id != me {
                self.lookup.add_endpoint_info(addr.clone());
                self.ctx.remember(addr.clone());
            }
        }
        let peers: Vec<EndpointId> = bootstrap
            .iter()
            .map(|a| a.id)
            .filter(|id| *id != me)
            .collect();
        if peers.is_empty() && !bootstrap.is_empty() {
            // A ticket naming nobody but us: two instances sharing one identity. Everything
            // downstream would succeed and nobody would ever arrive.
            anyhow::bail!(
                "this ticket names only {}, which is you; two instances need two identities",
                me.fmt_short()
            );
        }
        // `subscribe` rather than `subscribe_and_join`: the latter waits for a neighbour, which
        // for whoever opened the room means waiting for someone else to turn up.
        let handle = self
            .gossip
            .subscribe(topic, peers)
            .await
            .context("subscribe to topic")?;
        let (sender, receiver) = handle.split();
        let state = Arc::new(TopicState {
            topic,
            sender,
            task: Mutex::new(None),
        });
        self.ctx
            .topics
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(topic, state.clone());
        let task = n0_future::task::spawn(gossip_loop(self.ctx.clone(), state.clone(), receiver));
        *state.task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
        Ok(Topic {
            ctx: self.ctx.clone(),
            state,
        })
    }

    pub async fn shutdown(&self) -> Result<()> {
        let topics: Vec<_> = self
            .ctx
            .topics
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for state in topics {
            let _ = Topic {
                ctx: self.ctx.clone(),
                state,
            }
            .leave()
            .await;
        }
        self.router
            .shutdown()
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        self.ctx.endpoint.close().await;
        Ok(())
    }
}

/// One joined topic.
#[derive(Clone)]
pub struct Topic {
    ctx: Arc<Ctx>,
    state: Arc<TopicState>,
}

impl Topic {
    pub fn id(&self) -> TopicId {
        self.state.topic
    }

    /// A ticket naming us as the bootstrap peer.
    pub fn ticket(&self, name: impl Into<String>) -> RoomTicket {
        RoomTicket::new(self.state.topic, name, vec![self.ctx.endpoint.addr()])
    }

    pub async fn broadcast(&self, kind: Kind, body: Vec<u8>) -> Result<()> {
        let frame = self
            .ctx
            .sign(self.state.topic, Audience::Everyone, kind, body)?;
        // Fast paths first: whoever has one gets it now and drops gossip's copy later.
        for (_, send) in self.ctx.fast_paths.all() {
            send(&frame);
        }
        self.state
            .sender
            .broadcast(frame.into())
            .await
            .context("broadcast")?;
        Ok(())
    }

    pub async fn broadcast_payload<P: Payload>(&self, payload: &P) -> Result<()> {
        self.broadcast(P::KIND, payload.to_body()?).await
    }

    /// Deliver to one peer, over its fast path if it has one and a dial otherwise, returning
    /// a reply if the dial got one.
    pub async fn send_to(
        &self,
        to: EndpointId,
        kind: Kind,
        body: Vec<u8>,
    ) -> Result<Option<Vec<u8>>> {
        let frame = self
            .ctx
            .sign(self.state.topic, Audience::Only(vec![to]), kind, body)?;
        if let Some(send) = self.ctx.fast_paths.one(to)
            && send(&frame)
        {
            return Ok(None);
        }
        direct::request(&self.ctx.endpoint, self.ctx.addr_for(to), &frame).await
    }

    /// Dial one peer and deliver, returning its reply if it wrote one.
    async fn dial(&self, to: EndpointId, kind: Kind, body: Vec<u8>) -> Result<Option<Vec<u8>>> {
        let frame = self
            .ctx
            .sign(self.state.topic, Audience::Only(vec![to]), kind, body)?;
        direct::request(&self.ctx.endpoint, self.ctx.addr_for(to), &frame).await
    }

    /// Round trip to one peer, measured on our clock.
    pub async fn ping(&self, to: EndpointId) -> Result<u64> {
        let nonce: u64 = rand::random();
        let sent_at = proto::now_ms();
        let reply = self
            .dial(to, Kind::PING, proto::Ping { nonce }.to_body()?)
            .await?
            .context("peer accepted the ping but did not answer it")?;
        let (_, envelope) = proto::decode(&reply)?;
        let pong = proto::Pong::from_body(&envelope.body)?;
        anyhow::ensure!(pong.nonce == nonce, "pong answers a different ping");
        Ok(proto::now_ms().saturating_sub(sent_at))
    }

    pub async fn leave(&self) -> Result<()> {
        let goodbye = self.broadcast_payload(&proto::Goodbye).await;
        self.ctx
            .topics
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.state.topic);
        // Ends the gossip loop and, with the receiver dropped, the subscription.
        if let Some(task) = self
            .state
            .task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            task.abort();
        }
        goodbye
    }
}

async fn gossip_loop(ctx: Arc<Ctx>, state: Arc<TopicState>, mut receiver: GossipReceiver) {
    loop {
        match receiver.try_next().await {
            Ok(Some(event)) => match event {
                GossipEvent::NeighborUp(peer) => ctx.emit(Event::Neighbor {
                    topic: state.topic,
                    peer,
                    up: true,
                }),
                GossipEvent::NeighborDown(peer) => ctx.emit(Event::Neighbor {
                    topic: state.topic,
                    peer,
                    up: false,
                }),
                GossipEvent::Received(message) => {
                    // A bad frame is one peer's problem and must not end the loop.
                    if let Err(e) = ctx.handle_frame(&message.content, Via::Gossip).await {
                        ctx.report(
                            Some(state.topic),
                            format!("from {}: {e}", message.delivered_from.fmt_short()),
                        );
                    }
                }
                GossipEvent::Lagged => ctx.report(
                    Some(state.topic),
                    "fell behind on this topic; some messages were dropped",
                ),
            },
            Ok(None) => break,
            Err(e) => {
                ctx.report(Some(state.topic), format!("topic ended: {e}"));
                break;
            }
        }
    }
}
