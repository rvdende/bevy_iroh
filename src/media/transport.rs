//! Media frames over the same endpoint, on their own ALPN.
//!
//! A subscriber dials the owner of a track and opens one control stream on which it names the
//! tracks it wants. Audio then flows back as QUIC datagrams: unreliable and unordered, which is
//! what a 20 ms voice frame wants, since a retransmitted frame arrives too late to be worth
//! hearing. Video is one QUIC stream per group of pictures, newer groups at higher priority.
//! Nothing here touches gossip; the announce that a track exists is the replicated component
//! on the entity, and the track id is that entity's `NetId`.

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

/// State shared by the Bevy side, the protocol handler, the audio threads and the sessions.
#[derive(Default)]
pub struct MediaHub {
    /// Tracks this node publishes, and whether they are muted.
    published: Mutex<HashMap<u64, Arc<AtomicBool>>>,
    /// Who subscribed to each of my tracks.
    subscribers: Mutex<HashMap<u64, Vec<Connection>>>,
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
    conn: Connection,
    control: SendStream,
    tracks: Vec<u64>,
}

impl std::fmt::Debug for MediaHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MediaHub")
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl MediaHub {
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

    /// The connections currently subscribed to `track`, for a video group to be opened on.
    pub(crate) fn subscribers_of(&self, track: u64) -> Vec<Connection> {
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
        conns.retain(|conn| match conn.send_datagram(bytes.clone()) {
            Ok(()) => true,
            Err(iroh::endpoint::SendDatagramError::ConnectionLost(_)) => false,
            Err(_) => true,
        });
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
            let conn = endpoint.connect(EndpointAddr::new(owner), ALPN).await?;
            let (control, _recv) = conn.open_bi().await?;
            let hub = self.clone();
            let reader = conn.clone();
            n0_future::task::spawn(async move {
                hub.read_datagrams(reader).await;
                hub.sessions.lock().await.remove(&owner);
                lock(&hub.dead).push(owner);
            });
            let hub = self.clone();
            let streams = conn.clone();
            n0_future::task::spawn(async move { hub.read_streams(streams).await });
            sessions.insert(
                owner,
                Session {
                    conn,
                    control,
                    tracks: Vec::new(),
                },
            );
        }
        let session = sessions.get_mut(&owner).expect("just inserted");
        if !session.tracks.contains(&track) {
            session.tracks.push(track);
            write_control(&mut session.control, &Control::Subscribe { track }).await?;
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
        let _ = write_control(&mut session.control, &Control::Unsubscribe { track }).await;
        if session.tracks.is_empty() {
            let session = sessions.remove(&owner).expect("present");
            session.conn.close(0u32.into(), b"done");
        }
    }

    /// Ask the owner of `track` for a keyframe: the decoder here lost its place.
    pub async fn request_keyframe(self: &Arc<Self>, owner: EndpointId, track: u64) {
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get_mut(&owner) {
            let _ = write_control(&mut session.control, &Control::Keyframe { track }).await;
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
        let id = conn.stable_id();
        loop {
            let Ok(control) = read_control(&mut recv).await else {
                break;
            };
            match control {
                Control::Subscribe { track } => {
                    let mut subs = lock(&self.subscribers);
                    let conns = subs.entry(track).or_default();
                    if !conns.iter().any(|c| c.stable_id() == id) {
                        conns.push(conn.clone());
                    }
                    drop(subs);
                    // A video subscriber can decode nothing until a keyframe.
                    self.want_keyframe(track);
                }
                Control::Unsubscribe { track } => {
                    if let Some(conns) = lock(&self.subscribers).get_mut(&track) {
                        conns.retain(|c| c.stable_id() != id);
                    }
                }
                Control::Keyframe { track } => self.want_keyframe(track),
            }
        }
        for conns in lock(&self.subscribers).values_mut() {
            conns.retain(|c| c.stable_id() != id);
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
