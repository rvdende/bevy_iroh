//! WebRTC on a desktop: `str0m` driving one UDP socket per peer on the iroh runtime.
//!
//! `str0m` is sans-IO: it is handed every packet and every tick and answers with packets to
//! send and events, so it lives inside an ordinary task. Candidates are gathered before the
//! SDP is written, one host candidate on the interface the default route uses and one
//! server-reflexive candidate from a STUN binding on the same socket, so nothing trickles
//! from this side; what the browser trickles is fed in as it arrives.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use bevy::prelude::Entity;
use bytes::Bytes;
use iroh::EndpointId;
use str0m::{
    Candidate, Event, IceConnectionState, Input, Output, Rtc,
    change::{SdpAnswer, SdpOffer, SdpPendingOffer},
    channel::{ChannelConfig, ChannelId, Reliability},
    net::{Protocol, Receive},
};
use tokio::{net::UdpSocket, sync::mpsc};

use super::{
    AUDIO_CHANNEL, FRAMES_CHANNEL, Outbox, RtcSettings, RtcSignal, SignalKind, VIDEO_BACKLOG,
    VIDEO_CHANNEL,
};
use crate::{
    media::transport::{MediaHub, Outbound, RtcLink},
    node::Iroh,
};

enum Cmd {
    Answer(String),
    Candidate(String),
    Close,
}

/// The task for each peer, by the commands it takes.
static PEERS: Mutex<Option<HashMap<EndpointId, mpsc::UnboundedSender<Cmd>>>> = Mutex::new(None);

fn peers() -> std::sync::MutexGuard<'static, Option<HashMap<EndpointId, mpsc::UnboundedSender<Cmd>>>>
{
    PEERS.lock().unwrap_or_else(|e| e.into_inner())
}

fn register(peer: EndpointId) -> mpsc::UnboundedReceiver<Cmd> {
    let (tx, rx) = mpsc::unbounded_channel();
    if let Some(old) = peers().get_or_insert_with(HashMap::new).insert(peer, tx) {
        let _ = old.send(Cmd::Close);
    }
    rx
}

fn command(peer: EndpointId, cmd: Cmd) {
    if let Some(tx) = peers().as_ref().and_then(|p| p.get(&peer)) {
        let _ = tx.send(cmd);
    }
}

pub(crate) fn offer(
    peer: EndpointId,
    room: Entity,
    settings: &RtcSettings,
    hub: Arc<MediaHub>,
    outbox: Outbox,
    iroh: &Iroh,
) {
    let stun = settings.stun.clone();
    let cmds = register(peer);
    let _ = iroh.spawn(async move {
        let (socket, candidates) = match gather(&stun).await {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("bevy_iroh: webrtc: {e}");
                return;
            }
        };
        let mut rtc = Rtc::builder().build(Instant::now());
        for c in candidates {
            rtc.add_local_candidate(c);
        }
        let mut api = rtc.sdp_api();
        api.add_channel_with_config(ChannelConfig {
            label: AUDIO_CHANNEL.into(),
            ordered: false,
            reliability: Reliability::MaxRetransmits { retransmits: 0 },
            negotiated: None,
            protocol: String::new(),
        });
        api.add_channel(VIDEO_CHANNEL.into());
        api.add_channel(FRAMES_CHANNEL.into());
        let Some((offer, pending)) = api.apply() else {
            return;
        };
        outbox.push(
            room,
            peer,
            RtcSignal {
                kind: SignalKind::Offer,
                body: offer.to_sdp_string(),
            },
        );
        drive(rtc, socket, peer, hub, cmds, Some(pending)).await;
    });
}

pub(crate) fn answer(
    peer: EndpointId,
    room: Entity,
    sdp: String,
    settings: &RtcSettings,
    hub: Arc<MediaHub>,
    outbox: Outbox,
    iroh: &Iroh,
) {
    let stun = settings.stun.clone();
    let cmds = register(peer);
    let _ = iroh.spawn(async move {
        let (socket, candidates) = match gather(&stun).await {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("bevy_iroh: webrtc: {e}");
                return;
            }
        };
        let mut rtc = Rtc::builder().build(Instant::now());
        for c in candidates {
            rtc.add_local_candidate(c);
        }
        let offer = match SdpOffer::from_sdp_string(&sdp) {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("bevy_iroh: webrtc offer: {e}");
                return;
            }
        };
        let answer = match rtc.sdp_api().accept_offer(offer) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("bevy_iroh: webrtc offer: {e}");
                return;
            }
        };
        outbox.push(
            room,
            peer,
            RtcSignal {
                kind: SignalKind::Answer,
                body: answer.to_sdp_string(),
            },
        );
        drive(rtc, socket, peer, hub, cmds, None).await;
    });
}

pub(crate) fn accept_answer(peer: EndpointId, sdp: String) {
    command(peer, Cmd::Answer(sdp));
}

pub(crate) fn add_candidate(peer: EndpointId, candidate: String) {
    command(peer, Cmd::Candidate(candidate));
}

pub(crate) fn close(peer: EndpointId, _hub: &Arc<MediaHub>) {
    command(peer, Cmd::Close);
}

/// The socket and the candidates that name it.
async fn gather(stun: &[String]) -> Result<(UdpSocket, Vec<Candidate>), String> {
    let ip = default_route_ip();
    let socket = UdpSocket::bind(SocketAddr::new(ip, 0))
        .await
        .map_err(|e| format!("bind: {e}"))?;
    let base = socket.local_addr().map_err(|e| e.to_string())?;
    let mut candidates = vec![Candidate::host(base, "udp").map_err(|e| e.to_string())?];
    for server in stun {
        match stun_binding(&socket, server).await {
            Ok(mapped) if mapped != base => {
                if let Ok(c) = Candidate::server_reflexive(mapped, base, "udp") {
                    candidates.push(c);
                }
                break;
            }
            Ok(_) => break,
            Err(e) => tracing::debug!("bevy_iroh: stun {server}: {e}"),
        }
    }
    Ok((socket, candidates))
}

/// The address the default route leaves from: a connected UDP socket sends nothing and
/// still learns it.
fn default_route_ip() -> IpAddr {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("8.8.8.8:53").and_then(|_| s.local_addr()))
        .map(|a| a.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

/// One STUN binding request: what the world sees this socket as.
async fn stun_binding(socket: &UdpSocket, server: &str) -> Result<SocketAddr, String> {
    const MAGIC: u32 = 0x2112_A442;
    let server = tokio::net::lookup_host(server)
        .await
        .map_err(|e| e.to_string())?
        .find(|a| a.is_ipv4())
        .ok_or("no address")?;
    let id: [u8; 12] = rand::random();
    let mut request = Vec::with_capacity(20);
    request.extend_from_slice(&0x0001u16.to_be_bytes());
    request.extend_from_slice(&0u16.to_be_bytes());
    request.extend_from_slice(&MAGIC.to_be_bytes());
    request.extend_from_slice(&id);
    socket
        .send_to(&request, server)
        .await
        .map_err(|e| e.to_string())?;
    let mut buf = [0u8; 256];
    let deadline = Duration::from_millis(700);
    let (n, _) = tokio::time::timeout(deadline, socket.recv_from(&mut buf))
        .await
        .map_err(|_| "timed out".to_string())?
        .map_err(|e| e.to_string())?;
    let response = &buf[..n];
    if n < 20 || response[0..2] != 0x0101u16.to_be_bytes() || response[8..20] != id {
        return Err("not a binding response".into());
    }
    let mut at = 20;
    while at + 4 <= n {
        let kind = u16::from_be_bytes([response[at], response[at + 1]]);
        let len = u16::from_be_bytes([response[at + 2], response[at + 3]]) as usize;
        let value = &response[at + 4..(at + 4 + len).min(n)];
        // XOR-MAPPED-ADDRESS, or the plain one from an old server.
        if (kind == 0x0020 || kind == 0x0001) && value.len() >= 8 && value[1] == 0x01 {
            let (xp, xa) = if kind == 0x0020 {
                ((MAGIC >> 16) as u16, MAGIC)
            } else {
                (0, 0)
            };
            let port = u16::from_be_bytes([value[2], value[3]]) ^ xp;
            let ip = u32::from_be_bytes([value[4], value[5], value[6], value[7]]) ^ xa;
            return Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port));
        }
        at += 4 + len.div_ceil(4) * 4;
    }
    Err("no mapped address".into())
}

/// The loop that is one peer's connection, until it dies.
async fn drive(
    mut rtc: Rtc,
    socket: UdpSocket,
    peer: EndpointId,
    hub: Arc<MediaHub>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    mut pending: Option<SdpPendingOffer>,
) {
    let Ok(local) = socket.local_addr() else {
        return;
    };
    let (out_tx, mut out_rx) = mpsc::channel::<Outbound>(64);
    let link = Arc::new(RtcLink::new(peer, move |message| match message {
        // Video refused when the queue is full: the subscriber task starts over at a keyframe.
        Outbound::Video(_) => out_tx.try_send(message).is_ok(),
        _ => {
            let _ = out_tx.try_send(message);
            true
        }
    }));
    let mut audio: Option<ChannelId> = None;
    let mut video: Option<ChannelId> = None;
    let mut frames: Option<ChannelId> = None;
    let mut announced = false;
    let mut buf = vec![0u8; 2000];
    'run: loop {
        let deadline = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
                    let _ = socket.send_to(&t.contents, t.destination).await;
                }
                Ok(Output::Event(event)) => match event {
                    Event::ChannelOpen(id, label) => {
                        match label.as_str() {
                            AUDIO_CHANNEL => audio = Some(id),
                            VIDEO_CHANNEL => video = Some(id),
                            FRAMES_CHANNEL => frames = Some(id),
                            _ => {}
                        }
                        if audio.is_some() && video.is_some() && frames.is_some() && !announced {
                            announced = true;
                            hub.add_rtc_link(link.clone()).await;
                        }
                    }
                    Event::ChannelData(data) => hub.rtc_receive(&link, &data.data),
                    Event::ChannelClose(_) => rtc.disconnect(),
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                        rtc.disconnect();
                    }
                    Event::Closed => break 'run,
                    _ => {}
                },
                Err(e) => {
                    tracing::debug!("bevy_iroh: webrtc {}: {e}", peer.fmt_short());
                    break 'run;
                }
            }
        };
        if !rtc.is_alive() {
            break;
        }
        let wait = deadline.saturating_duration_since(Instant::now());
        tokio::select! {
            received = socket.recv_from(&mut buf) => {
                if let Ok((n, source)) = received
                    && let Ok(contents) = buf[..n].try_into()
                {
                    let _ = rtc.handle_input(Input::Receive(
                        Instant::now(),
                        Receive { proto: Protocol::Udp, source, destination: local, contents },
                    ));
                }
            }
            Some(message) = out_rx.recv() => {
                let (channel, bytes): (Option<ChannelId>, Bytes) = match message {
                    Outbound::Audio(b) => (audio, b),
                    Outbound::Video(b) | Outbound::Control(b) => (video, b),
                    Outbound::Frame(b) => (frames, b),
                };
                if let Some(id) = channel && let Some(mut ch) = rtc.channel(id) {
                    if ch.buffered_amount() > VIDEO_BACKLOG && matches!(id, _ if Some(id) == video) {
                        continue;
                    }
                    let _ = ch.write(true, &bytes);
                }
            }
            Some(cmd) = cmds.recv() => match cmd {
                Cmd::Answer(sdp) => {
                    if let (Some(p), Ok(a)) = (pending.take(), SdpAnswer::from_sdp_string(&sdp))
                        && let Err(e) = rtc.sdp_api().accept_answer(p, a)
                    {
                        tracing::warn!("bevy_iroh: webrtc answer: {e}");
                    }
                }
                Cmd::Candidate(c) => match Candidate::from_sdp_string(&c) {
                    Ok(c) => rtc.add_remote_candidate(c),
                    Err(e) => tracing::debug!("bevy_iroh: webrtc candidate: {e}"),
                },
                Cmd::Close => rtc.disconnect(),
            },
            _ = tokio::time::sleep(wait) => {
                let _ = rtc.handle_input(Input::Timeout(Instant::now()));
            }
        }
    }
    if announced {
        hub.remove_rtc_link(link.id).await;
    }
    if let Some(p) = peers().as_mut()
        && p.get(&peer).is_some_and(|tx| tx.is_closed())
    {
        p.remove(&peer);
    }
}
