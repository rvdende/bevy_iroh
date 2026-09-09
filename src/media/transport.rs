//! Media frames over the same endpoint, on their own ALPN, or over a WebRTC data channel.
//!
//! A subscriber dials the owner of a track and opens one control stream on which it names the
//! tracks it wants. Audio then flows back as QUIC datagrams: unreliable and unordered, which is
//! what a 20 ms voice frame wants, since a retransmitted frame arrives too late to be worth
//! hearing. Video is one QUIC stream per group of pictures, newer groups at higher priority.
//! Nothing here touches gossip; the announce that a track exists is the replicated component
//! on the entity, and the track id is that entity's `NetId`.
//!
//! A browser cannot be dialled over QUIC, so a peer may also be reached through a [`Link`]
//! made of two WebRTC data channels (the `webrtc` feature): the same bytes, audio on an
//! unreliable unordered channel, video and control on a reliable one. When such a link comes
//! up for a peer, subscriptions move onto it; when it goes, they move back.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

use bytes::Bytes;
use iroh::{
    Endpoint, EndpointAddr, EndpointId,
    endpoint::{Connection, SendStream},
    protocol::{AcceptError, ProtocolHandler},
};
use serde::{Deserialize, Serialize};

use super::{
    audio::RemoteTrack,
    video::{RemoteVideo, VideoPacket},
};

pub const ALPN: &[u8] = b"bevy_iroh/media/1";

/// The first byte of a datagram or a stream.
const TAG_AUDIO: u8 = 1;
const TAG_VIDEO: u8 = 2;
/// track (8) + seq (4) after the tag.
const HEADER: usize = 1 + 8 + 4;
/// The largest encoded video frame accepted: a hostile length prefix must not allocate more.
const MAX_VIDEO_FRAME: usize = 8 << 20;
/// tag (1) + track (8) + group (4) + keyframe (1) + pts (8): a video frame on a data channel.
const VIDEO_MESSAGE_HEADER: usize = 1 + 8 + 4 + 1 + 8;

/// What a track carries, so a subscription knows what to build for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Audio,
    Video,
}

#[derive(Debug, Serialize, Deserialize)]
enum Control {
    Subscribe {
        track: u64,
    },
    Unsubscribe {
        track: u64,
    },
    /// The subscriber's decoder lost its place; the next frame should be a keyframe.
    Keyframe {
        track: u64,
    },
}

/// The first byte of a WebRTC message carrying a control frame.
const TAG_CONTROL: u8 = 3;
/// The first byte of a WebRTC message carrying a signed room frame: replication, presence,
/// typed messages, the same bytes gossip would carry.
const TAG_FRAME: u8 = 4;

/// What goes out on a WebRTC link.
pub enum Outbound {
    /// On the unreliable channel.
    Audio(Bytes),
    /// On the reliable video channel.
    Video(Bytes),
    Control(Bytes),
    /// On the reliable frames channel, apart from video so a keyframe never delays a move.
    Frame(Bytes),
}

/// A WebRTC data-channel pair to one peer. `out` hands bytes to whatever drives the
/// connection: a `str0m` task on a desktop, `RTCDataChannel`s in a page. It answers `false`
/// when the link is gone or too far behind to take more.
pub struct RtcLink {
    pub id: u64,
    pub peer: EndpointId,
    out: Box<dyn Fn(Outbound) -> bool + Send + Sync>,
    alive: AtomicBool,
}

impl RtcLink {
    pub fn new(peer: EndpointId, out: impl Fn(Outbound) -> bool + Send + Sync + 'static) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(1);
        // High bit set: never collides with a QUIC connection's stable id.
        let id = (1u64 << 63) | NEXT.fetch_add(1, Ordering::Relaxed) as u64;
        Self {
            id,
            peer,
            out: Box::new(out),
            alive: AtomicBool::new(true),
        }
    }

    pub fn send(&self, message: Outbound) -> bool {
        self.alive.load(Ordering::Relaxed) && (self.out)(message)
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

impl std::fmt::Debug for RtcLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RtcLink({:x} to {})", self.id, self.peer.fmt_short())
    }
}

/// A way to reach one peer with media.
#[derive(Clone, Debug)]
pub enum Link {
    Quic(Connection),
    Rtc(Arc<RtcLink>),
}

impl Link {
    pub fn id(&self) -> u64 {
        match self {
            Link::Quic(c) => c.stable_id() as u64,
            Link::Rtc(l) => l.id,
        }
    }

    pub fn is_rtc(&self) -> bool {
        matches!(self, Link::Rtc(_))
    }
}

/// State shared by the Bevy side, the protocol handler, the audio threads and the sessions.
pub struct MediaHub {
    /// The node's fast paths: a WebRTC link is one, for everything the room says.
    fast_paths: Arc<crate::net::FastPaths>,
    /// Tracks this node publishes, and whether they are muted.
    published: Mutex<HashMap<u64, Arc<AtomicBool>>>,
    /// Who subscribed to each of my tracks.
    subscribers: Mutex<HashMap<u64, Vec<Link>>>,
    /// WebRTC links that are up, by peer.
    rtc_links: Mutex<HashMap<EndpointId, Arc<RtcLink>>>,
    /// Audio tracks I subscribe to, by id: where their frames land.
    remote: Mutex<HashMap<u64, Arc<RemoteTrack>>>,
    /// Video tracks I subscribe to.
    remote_video: Mutex<HashMap<u64, Arc<RemoteVideo>>>,
    /// Video tracks I publish: a keyframe is wanted when a subscriber arrives mid-group.
    keyframe_wanted: Mutex<HashMap<u64, Arc<AtomicBool>>>,
    /// One dialled connection per owner, and the control stream on it.
    sessions: tokio::sync::Mutex<HashMap<EndpointId, Session>>,
    /// Sessions that died, for the Bevy side to resubscribe through.
    dead: Mutex<Vec<EndpointId>>,
}

struct Session {
    link: Link,
    /// The control stream, on a QUIC session.
    control: Option<SendStream>,
    tracks: Vec<u64>,
}

impl Session {
    async fn control(&mut self, control: &Control) -> anyhow::Result<()> {
        match &self.link {
            Link::Quic(_) => {
                let stream = self
                    .control
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("no control stream"))?;
                write_control(stream, control).await
            }
            Link::Rtc(link) => {
                let mut body = vec![TAG_CONTROL];
                body.extend(postcard::to_stdvec(control)?);
                anyhow::ensure!(link.send(Outbound::Control(Bytes::from(body))), "link gone");
                Ok(())
            }
        }
    }

    fn close(self) {
        if let Link::Quic(conn) = self.link {
            conn.close(0u32.into(), b"done");
        }
    }
}

impl std::fmt::Debug for MediaHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MediaHub")
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Default for MediaHub {
    fn default() -> Self {
        Self::new(Arc::new(crate::net::FastPaths::default()))
    }
}

impl MediaHub {
    pub fn new(fast_paths: Arc<crate::net::FastPaths>) -> Self {
        Self {
            fast_paths,
            published: Default::default(),
            subscribers: Default::default(),
            rtc_links: Default::default(),
            remote: Default::default(),
            remote_video: Default::default(),
            keyframe_wanted: Default::default(),
            sessions: Default::default(),
            dead: Default::default(),
        }
    }

    // -- publishing -------------------------------------------------------------------------

    /// Start publishing `track`. Returns its mute flag, shared with the encoder.
    pub fn publish(&self, track: u64) -> Arc<AtomicBool> {
        lock(&self.published)
            .entry(track)
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    }

    pub fn unpublish(&self, track: u64) {
        lock(&self.published).remove(&track);
        lock(&self.subscribers).remove(&track);
        lock(&self.keyframe_wanted).remove(&track);
    }

    /// The flag a video encoder checks before each frame: set when a subscriber joined
    /// mid-group and cannot decode anything until the next keyframe.
    pub fn keyframe_flag(&self, track: u64) -> Arc<AtomicBool> {
        lock(&self.keyframe_wanted)
            .entry(track)
            .or_insert_with(|| Arc::new(AtomicBool::new(true)))
            .clone()
    }

    /// The links currently subscribed to `track`, for a video group to be opened on.
    pub(crate) fn subscribers_of(&self, track: u64) -> Vec<Link> {
        lock(&self.subscribers)
            .get(&track)
            .cloned()
            .unwrap_or_default()
    }

    pub fn published(&self) -> Vec<(u64, Arc<AtomicBool>)> {
        lock(&self.published)
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    }

    pub fn is_published(&self, track: u64) -> bool {
        lock(&self.published).contains_key(&track)
    }

    /// Send one audio frame to everyone subscribed to `track`. Never blocks, never waits: a
    /// frame that cannot go now is a frame nobody wants later.
    pub fn send_audio(&self, track: u64, seq: u32, frame: &[u8]) {
        let mut subs = lock(&self.subscribers);
        let Some(conns) = subs.get_mut(&track) else {
            return;
        };
        let mut buf = Vec::with_capacity(HEADER + frame.len());
        buf.push(TAG_AUDIO);
        buf.extend_from_slice(&track.to_le_bytes());
        buf.extend_from_slice(&seq.to_le_bytes());
        buf.extend_from_slice(frame);
        let bytes = Bytes::from(buf);
        conns.retain(|link| match link {
            Link::Quic(conn) => match conn.send_datagram(bytes.clone()) {
                Ok(()) => true,
                Err(iroh::endpoint::SendDatagramError::ConnectionLost(_)) => false,
                Err(_) => true,
            },
            Link::Rtc(link) => {
                link.send(Outbound::Audio(bytes.clone()));
                link.is_alive()
            }
        });
    }

    // -- WebRTC links -----------------------------------------------------------------------

    /// The WebRTC link to `peer`, if one is up.
    pub fn rtc_link(&self, peer: EndpointId) -> Option<Arc<RtcLink>> {
        lock(&self.rtc_links).get(&peer).cloned()
    }

    /// Whether `peer` is reached over WebRTC right now.
    pub fn is_rtc(&self, peer: EndpointId) -> bool {
        lock(&self.rtc_links).contains_key(&peer)
    }

    /// A WebRTC link came up. Anything subscribed from that peer over QUIC is dropped and
    /// marked for subscribing again, which lands on the link.
    pub async fn add_rtc_link(self: &Arc<Self>, link: Arc<RtcLink>) {
        let peer = link.peer;
        let frames = link.clone();
        self.fast_paths.add(peer, move |frame| {
            let mut buf = Vec::with_capacity(1 + frame.len());
            buf.push(TAG_FRAME);
            buf.extend_from_slice(frame);
            frames.send(Outbound::Frame(Bytes::from(buf)))
        });
        if let Some(old) = lock(&self.rtc_links).insert(peer, link) {
            old.alive.store(false, Ordering::Relaxed);
        }
        if let Some(session) = self.sessions.lock().await.remove(&peer) {
            session.close();
        }
        lock(&self.dead).push(peer);
        tracing::info!("bevy_iroh: webrtc to {} is up", peer.fmt_short());
    }

    /// A WebRTC link went. Subscriptions over it are marked for subscribing again, which
    /// dials QUIC.
    pub async fn remove_rtc_link(self: &Arc<Self>, link_id: u64) {
        let peer = {
            let mut links = lock(&self.rtc_links);
            let Some((peer, _)) = links
                .iter()
                .find(|(_, l)| l.id == link_id)
                .map(|(p, l)| (*p, l.clone()))
            else {
                return;
            };
            if let Some(l) = links.remove(&peer) {
                l.alive.store(false, Ordering::Relaxed);
            }
            peer
        };
        self.fast_paths.remove(peer);
        for links in lock(&self.subscribers).values_mut() {
            links.retain(|l| l.id() != link_id);
        }
        let mut sessions = self.sessions.lock().await;
        if sessions.get(&peer).is_some_and(|s| s.link.id() == link_id) {
            sessions.remove(&peer);
        }
        drop(sessions);
        lock(&self.dead).push(peer);
        tracing::info!("bevy_iroh: webrtc to {} is down", peer.fmt_short());
    }

    /// One message that arrived on a WebRTC link, on either channel.
    pub fn rtc_receive(&self, link: &Arc<RtcLink>, data: &[u8]) {
        match data.first() {
            Some(&TAG_AUDIO) if data.len() >= HEADER => {
                let track = u64::from_le_bytes(data[1..9].try_into().expect("8 bytes"));
                let seq = u32::from_le_bytes(data[9..13].try_into().expect("4 bytes"));
                if let Some(remote) = self.remote(track) {
                    remote.push(seq, Bytes::copy_from_slice(&data[HEADER..]));
                }
            }
            Some(&TAG_VIDEO) if data.len() >= VIDEO_MESSAGE_HEADER => {
                let track = u64::from_le_bytes(data[1..9].try_into().expect("8 bytes"));
                let group = u32::from_le_bytes(data[9..13].try_into().expect("4 bytes"));
                let keyframe = data[13] & 1 != 0;
                let pts_ms = u64::from_le_bytes(data[14..22].try_into().expect("8 bytes"));
                if let Some(remote) = self.remote_video(track) {
                    remote.push(VideoPacket {
                        group,
                        keyframe,
                        pts_ms,
                        data: data[VIDEO_MESSAGE_HEADER..].to_vec(),
                    });
                }
            }
            Some(&TAG_CONTROL) => {
                let Ok(control) = postcard::from_bytes::<Control>(&data[1..]) else {
                    return;
                };
                self.control(Link::Rtc(link.clone()), control);
            }
            // The node verifies the signature; a link is a route, not a trust.
            Some(&TAG_FRAME) => self.fast_paths.deliver(data[1..].to_vec()),
            _ => {}
        }
    }

    /// One video frame as a single reliable-channel message.
    pub(crate) fn video_message(track: u64, packet: &VideoPacket) -> Bytes {
        let mut buf = Vec::with_capacity(VIDEO_MESSAGE_HEADER + packet.data.len());
        buf.push(TAG_VIDEO);
        buf.extend_from_slice(&track.to_le_bytes());
        buf.extend_from_slice(&packet.group.to_le_bytes());
        buf.push(packet.keyframe as u8);
        buf.extend_from_slice(&packet.pts_ms.to_le_bytes());
        buf.extend_from_slice(&packet.data);
        Bytes::from(buf)
    }

    /// A control frame from a subscriber, over either transport.
    fn control(&self, link: Link, control: Control) {
        let id = link.id();
        match control {
            Control::Subscribe { track } => {
                let mut subs = lock(&self.subscribers);
                let links = subs.entry(track).or_default();
                if !links.iter().any(|l| l.id() == id) {
                    links.push(link);
                }
                drop(subs);
                // A video subscriber can decode nothing until a keyframe.
                self.want_keyframe(track);
            }
            Control::Unsubscribe { track } => {
                if let Some(links) = lock(&self.subscribers).get_mut(&track) {
                    links.retain(|l| l.id() != id);
                }
            }
            Control::Keyframe { track } => self.want_keyframe(track),
        }
    }

    // -- subscribing ------------------------------------------------------------------------

    pub fn remote(&self, track: u64) -> Option<Arc<RemoteTrack>> {
        lock(&self.remote).get(&track).cloned()
    }

    pub fn remotes(&self) -> Vec<(u64, Arc<RemoteTrack>)> {
        lock(&self.remote)
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    }

    pub fn remote_video(&self, track: u64) -> Option<Arc<RemoteVideo>> {
        lock(&self.remote_video).get(&track).cloned()
    }

    /// Owners whose session dropped since the last call. Their tracks need subscribing again.
    pub fn take_dead(&self) -> Vec<EndpointId> {
        std::mem::take(&mut *lock(&self.dead))
    }

    pub fn mark_dead(&self, owner: EndpointId) {
        lock(&self.dead).push(owner);
    }

    /// Stop playing a track without a session to tell.
    pub fn forget_remote(&self, track: u64) {
        lock(&self.remote).remove(&track);
        lock(&self.remote_video).remove(&track);
    }

    /// Subscribe to `track` from `owner`, dialling them if there is no session yet.
    #[allow(clippy::map_entry)]
    pub async fn subscribe(
        self: &Arc<Self>,
        endpoint: Endpoint,
        owner: EndpointId,
        track: u64,
        kind: TrackKind,
    ) -> anyhow::Result<()> {
        match kind {
            TrackKind::Audio => {
                lock(&self.remote)
                    .entry(track)
                    .or_insert_with(|| Arc::new(RemoteTrack::new()));
            }
            TrackKind::Video => {
                lock(&self.remote_video)
                    .entry(track)
                    .or_insert_with(|| Arc::new(RemoteVideo::new(track)));
            }
        }
        let mut sessions = self.sessions.lock().await;
        if !sessions.contains_key(&owner) {
            let session = match self.rtc_link(owner) {
                Some(link) => Session {
                    link: Link::Rtc(link),
                    control: None,
                    tracks: Vec::new(),
                },
                None => {
                    let conn = endpoint.connect(EndpointAddr::new(owner), ALPN).await?;
                    let (control, _recv) = conn.open_bi().await?;
                    let hub = self.clone();
                    let reader = conn.clone();
                    let id = conn.stable_id() as u64;
                    n0_future::task::spawn(async move {
                        hub.read_datagrams(reader).await;
                        let mut sessions = hub.sessions.lock().await;
                        if sessions.get(&owner).is_some_and(|s| s.link.id() == id) {
                            sessions.remove(&owner);
                        }
                        drop(sessions);
                        lock(&hub.dead).push(owner);
                    });
                    let hub = self.clone();
                    let streams = conn.clone();
                    n0_future::task::spawn(async move { hub.read_streams(streams).await });
                    Session {
                        link: Link::Quic(conn),
                        control: Some(control),
                        tracks: Vec::new(),
                    }
                }
            };
            sessions.insert(owner, session);
        }
        let session = sessions.get_mut(&owner).expect("just inserted");
        if !session.tracks.contains(&track) {
            session.tracks.push(track);
            session.control(&Control::Subscribe { track }).await?;
        }
        Ok(())
    }

    pub async fn unsubscribe(self: &Arc<Self>, owner: EndpointId, track: u64) {
        lock(&self.remote).remove(&track);
        lock(&self.remote_video).remove(&track);
        let mut sessions = self.sessions.lock().await;
        let Some(session) = sessions.get_mut(&owner) else {
            return;
        };
        session.tracks.retain(|t| *t != track);
        let _ = session.control(&Control::Unsubscribe { track }).await;
        if session.tracks.is_empty() {
            let session = sessions.remove(&owner).expect("present");
            session.close();
        }
    }

    /// Ask the owner of `track` for a keyframe: the decoder here lost its place.
    pub async fn request_keyframe(self: &Arc<Self>, owner: EndpointId, track: u64) {
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get_mut(&owner) {
            let _ = session.control(&Control::Keyframe { track }).await;
        }
    }

    async fn read_datagrams(&self, conn: Connection) {
        let mut heard = false;
        while let Ok(datagram) = conn.read_datagram().await {
            if !heard {
                heard = true;
                tracing::info!("bevy_iroh: hearing {}", conn.remote_id().fmt_short());
            }
            if datagram.len() < HEADER || datagram[0] != TAG_AUDIO {
                continue;
            }
            let track = u64::from_le_bytes(datagram[1..9].try_into().expect("8 bytes"));
            let seq = u32::from_le_bytes(datagram[9..13].try_into().expect("4 bytes"));
            if let Some(remote) = self.remote(track) {
                remote.push(seq, datagram.slice(HEADER..));
            }
        }
    }

    /// One stream per video group: a header naming the track and the group, then frames until
    /// the publisher finishes it at the next keyframe.
    async fn read_streams(self: Arc<Self>, conn: Connection) {
        // Groups a peer may have open at once. Two is the seam between one group and the
        // next; a peer opening dozens is not sending video.
        const MAX_OPEN_GROUPS: usize = 8;
        let open = Arc::new(AtomicU32::new(0));
        while let Ok(mut recv) = conn.accept_uni().await {
            if open.load(Ordering::Relaxed) as usize >= MAX_OPEN_GROUPS {
                continue;
            }
            open.fetch_add(1, Ordering::Relaxed);
            let open = open.clone();
            let hub = self.clone();
            let hub_remote = move |track: u64| hub.remote_video(track);
            n0_future::task::spawn(async move {
                let mut header = [0u8; 1 + 8 + 4];
                if recv.read_exact(&mut header).await.is_err() || header[0] != TAG_VIDEO {
                    return;
                }
                struct Open(Arc<AtomicU32>);
                impl Drop for Open {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::Relaxed);
                    }
                }
                let _open = Open(open);
                let track = u64::from_le_bytes(header[1..9].try_into().expect("8 bytes"));
                let group = u32::from_le_bytes(header[9..13].try_into().expect("4 bytes"));
                let Some(remote) = hub_remote(track) else {
                    return;
                };
                loop {
                    let mut head = [0u8; 4 + 1 + 8];
                    if recv.read_exact(&mut head).await.is_err() {
                        return;
                    }
                    let len = u32::from_le_bytes(head[0..4].try_into().expect("4 bytes")) as usize;
                    if len > MAX_VIDEO_FRAME {
                        return;
                    }
                    let keyframe = head[4] & 1 != 0;
                    let pts_ms = u64::from_le_bytes(head[5..13].try_into().expect("8 bytes"));
                    let mut data = vec![0u8; len];
                    if recv.read_exact(&mut data).await.is_err() {
                        return;
                    }
                    remote.push(VideoPacket {
                        group,
                        keyframe,
                        pts_ms,
                        data,
                    });
                }
            });
        }
    }

    /// Write one video frame to a group stream. An error is the stream having gone.
    pub(crate) async fn write_video_frame(
        stream: &mut SendStream,
        packet: &VideoPacket,
    ) -> anyhow::Result<()> {
        let mut head = Vec::with_capacity(13);
        head.extend_from_slice(&(packet.data.len() as u32).to_le_bytes());
        head.push(packet.keyframe as u8);
        head.extend_from_slice(&packet.pts_ms.to_le_bytes());
        stream.write_all(&head).await?;
        stream.write_all(&packet.data).await?;
        Ok(())
    }

    /// Open a group stream on `conn` for `track`. Newer groups get higher priority, so a group
    /// still draining when the next one starts cannot hold the picture back.
    pub(crate) async fn open_group(
        conn: &Connection,
        track: u64,
        group: u32,
    ) -> anyhow::Result<SendStream> {
        let mut stream = conn.open_uni().await?;
        let _ = stream.set_priority(group as i32);
        let mut header = Vec::with_capacity(13);
        header.push(TAG_VIDEO);
        header.extend_from_slice(&track.to_le_bytes());
        header.extend_from_slice(&group.to_le_bytes());
        stream.write_all(&header).await?;
        Ok(stream)
    }

    // -- accept side ------------------------------------------------------------------------

    async fn serve(&self, conn: Connection) {
        let Ok((_send, mut recv)) = conn.accept_bi().await else {
            return;
        };
        let id = conn.stable_id() as u64;
        loop {
            let Ok(control) = read_control(&mut recv).await else {
                break;
            };
            self.control(Link::Quic(conn.clone()), control);
        }
        for links in lock(&self.subscribers).values_mut() {
            links.retain(|l| l.id() != id);
        }
    }

    pub(crate) fn want_keyframe(&self, track: u64) {
        if let Some(flag) = lock(&self.keyframe_wanted).get(&track) {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

async fn write_control(stream: &mut SendStream, control: &Control) -> anyhow::Result<()> {
    let body = postcard::to_stdvec(control)?;
    stream.write_all(&(body.len() as u32).to_le_bytes()).await?;
    stream.write_all(&body).await?;
    Ok(())
}

async fn read_control(stream: &mut iroh::endpoint::RecvStream) -> anyhow::Result<Control> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    anyhow::ensure!(len <= 1024, "control message too large");
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    Ok(postcard::from_bytes(&body)?)
}

/// Accepts subscribers.
#[derive(Debug, Clone)]
pub struct MediaHandler {
    pub hub: Arc<MediaHub>,
}

impl ProtocolHandler for MediaHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        self.hub.serve(connection).await;
        Ok(())
    }
}
