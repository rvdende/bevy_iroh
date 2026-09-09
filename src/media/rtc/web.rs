//! WebRTC in a page: `RTCPeerConnection`, which brings ICE, DTLS and SCTP of its own.
//!
//! The JS handles cannot leave the thread, so they live in a thread-local keyed by peer and
//! the shared [`RtcLink`] only carries a closure that looks them up. Handlers never hold the
//! borrow while calling into the hub, since the hub may call back to send.

use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc};

use bevy::prelude::Entity;
use iroh::EndpointId;
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    MessageEvent, RtcConfiguration, RtcDataChannel, RtcDataChannelEvent, RtcDataChannelInit,
    RtcDataChannelState, RtcDataChannelType, RtcIceCandidateInit, RtcIceConnectionState,
    RtcIceServer, RtcPeerConnection, RtcPeerConnectionIceEvent, RtcSdpType,
    RtcSessionDescriptionInit,
};

use super::{
    AUDIO_CHANNEL, Outbox, RtcSettings, RtcSignal, SignalKind, VIDEO_BACKLOG, VIDEO_CHANNEL,
};
use crate::{
    media::{
        transport::{MediaHub, Outbound, RtcLink},
        web::describe,
    },
    node::Iroh,
};

struct WebPeer {
    pc: RtcPeerConnection,
    audio: Option<RtcDataChannel>,
    video: Option<RtcDataChannel>,
    link: Arc<RtcLink>,
    announced: bool,
    hub: Arc<MediaHub>,
    /// The Rust sides of the JS handlers, alive as long as the connection.
    _closures: Vec<Closure<dyn FnMut(JsValue)>>,
}

thread_local! {
    static PEERS: RefCell<HashMap<EndpointId, WebPeer>> = RefCell::new(HashMap::new());
}

fn configuration(settings: &RtcSettings) -> RtcConfiguration {
    let config = RtcConfiguration::new();
    let servers = js_sys::Array::new();
    for stun in &settings.stun {
        let server = RtcIceServer::new();
        server.set_urls(&JsValue::from_str(&format!("stun:{stun}")));
        servers.push(&server);
    }
    config.set_ice_servers(&servers);
    config
}

/// The closure a link sends through: a lookup, a readiness check, and `send`.
fn sender(peer: EndpointId) -> impl Fn(Outbound) -> bool + Send + Sync + 'static {
    move |message| {
        PEERS.with(|peers| {
            let peers = peers.borrow();
            let Some(p) = peers.get(&peer) else {
                return false;
            };
            let (channel, bytes, video) = match &message {
                Outbound::Audio(b) => (&p.audio, b, false),
                Outbound::Video(b) => (&p.video, b, true),
                Outbound::Control(b) => (&p.video, b, false),
            };
            let Some(channel) = channel else {
                return false;
            };
            if channel.ready_state() != RtcDataChannelState::Open {
                return false;
            }
            if video && channel.buffered_amount() as usize > VIDEO_BACKLOG {
                return false;
            }
            channel.send_with_u8_array(bytes).is_ok()
        })
    }
}

fn new_peer(
    peer: EndpointId,
    room: Entity,
    settings: &RtcSettings,
    hub: Arc<MediaHub>,
    outbox: Outbox,
) -> Result<RtcPeerConnection, String> {
    close(peer, &hub);
    let pc =
        RtcPeerConnection::new_with_configuration(&configuration(settings)).map_err(describe)?;
    let link = Arc::new(RtcLink::new(peer, sender(peer)));
    let mut closures = Vec::new();

    let on_candidate = Closure::wrap(Box::new(move |event: JsValue| {
        let event: RtcPeerConnectionIceEvent = event.unchecked_into();
        if let Some(candidate) = event.candidate() {
            outbox.push(
                room,
                peer,
                RtcSignal {
                    kind: SignalKind::Candidate,
                    body: candidate.candidate(),
                },
            );
        }
    }) as Box<dyn FnMut(JsValue)>);
    pc.set_onicecandidate(Some(on_candidate.as_ref().unchecked_ref()));
    closures.push(on_candidate);

    let (pc2, hub2) = (pc.clone(), hub.clone());
    let on_state = Closure::wrap(Box::new(move |_: JsValue| {
        let state = pc2.ice_connection_state();
        bevy::log::debug!("bevy_iroh: webrtc {} ice {:?}", peer.fmt_short(), state);
        if matches!(
            state,
            RtcIceConnectionState::Failed
                | RtcIceConnectionState::Disconnected
                | RtcIceConnectionState::Closed
        ) {
            close(peer, &hub2);
        }
    }) as Box<dyn FnMut(JsValue)>);
    pc.set_oniceconnectionstatechange(Some(on_state.as_ref().unchecked_ref()));
    closures.push(on_state);

    PEERS.with(|peers| {
        peers.borrow_mut().insert(
            peer,
            WebPeer {
                pc: pc.clone(),
                audio: None,
                video: None,
                link,
                announced: false,
                hub,
                _closures: closures,
            },
        )
    });
    Ok(pc)
}

/// Handlers on a channel, whichever side made it.
fn attach(peer: EndpointId, channel: RtcDataChannel) {
    channel.set_binary_type(RtcDataChannelType::Arraybuffer);
    let label = channel.label();
    let on_open = Closure::wrap(Box::new(move |_: JsValue| {
        let ready = PEERS.with(|peers| {
            let mut peers = peers.borrow_mut();
            let Some(p) = peers.get_mut(&peer) else {
                return None;
            };
            let open = |c: &Option<RtcDataChannel>| {
                c.as_ref()
                    .is_some_and(|c| c.ready_state() == RtcDataChannelState::Open)
            };
            if open(&p.audio) && open(&p.video) && !p.announced {
                p.announced = true;
                Some((p.link.clone(), p.hub.clone()))
            } else {
                None
            }
        });
        if let Some((link, hub)) = ready {
            wasm_bindgen_futures::spawn_local(async move { hub.add_rtc_link(link).await });
        }
    }) as Box<dyn FnMut(JsValue)>);
    channel.set_onopen(Some(on_open.as_ref().unchecked_ref()));

    let on_message = Closure::wrap(Box::new(move |event: JsValue| {
        let event: MessageEvent = event.unchecked_into();
        let Ok(buffer) = event.data().dyn_into::<js_sys::ArrayBuffer>() else {
            return;
        };
        let data = js_sys::Uint8Array::new(&buffer).to_vec();
        let found = PEERS.with(|peers| {
            peers
                .borrow()
                .get(&peer)
                .map(|p| (p.link.clone(), p.hub.clone()))
        });
        if let Some((link, hub)) = found {
            hub.rtc_receive(&link, &data);
        }
    }) as Box<dyn FnMut(JsValue)>);
    channel.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

    let hub = PEERS.with(|peers| peers.borrow().get(&peer).map(|p| p.hub.clone()));
    let on_close = Closure::wrap(Box::new(move |_: JsValue| {
        if let Some(hub) = &hub {
            close(peer, hub);
        }
    }) as Box<dyn FnMut(JsValue)>);
    channel.set_onclose(Some(on_close.as_ref().unchecked_ref()));

    PEERS.with(|peers| {
        let mut peers = peers.borrow_mut();
        let Some(p) = peers.get_mut(&peer) else {
            return;
        };
        match label.as_str() {
            AUDIO_CHANNEL => p.audio = Some(channel),
            VIDEO_CHANNEL => p.video = Some(channel),
            _ => return,
        }
        p._closures.extend([on_open, on_message, on_close]);
    });
}

fn description(kind: RtcSdpType, sdp: &str) -> RtcSessionDescriptionInit {
    let init = RtcSessionDescriptionInit::new(kind);
    init.set_sdp(sdp);
    init
}

fn sdp_of(value: &JsValue) -> Option<String> {
    js_sys::Reflect::get(value, &"sdp".into())
        .ok()
        .and_then(|s| s.as_string())
}

pub(crate) fn offer(
    peer: EndpointId,
    room: Entity,
    settings: &RtcSettings,
    hub: Arc<MediaHub>,
    outbox: Outbox,
    _iroh: &Iroh,
) {
    let pc = match new_peer(peer, room, settings, hub, outbox.clone()) {
        Ok(pc) => pc,
        Err(e) => {
            bevy::log::warn!("bevy_iroh: webrtc: {e}");
            return;
        }
    };
    let audio_init = RtcDataChannelInit::new();
    audio_init.set_ordered(false);
    audio_init.set_max_retransmits(0);
    attach(
        peer,
        pc.create_data_channel_with_data_channel_dict(AUDIO_CHANNEL, &audio_init),
    );
    attach(peer, pc.create_data_channel(VIDEO_CHANNEL));
    wasm_bindgen_futures::spawn_local(async move {
        let result: Result<(), String> = async {
            let offer = JsFuture::from(pc.create_offer()).await.map_err(describe)?;
            let sdp = sdp_of(&offer).ok_or("offer without sdp")?;
            JsFuture::from(pc.set_local_description(&description(RtcSdpType::Offer, &sdp)))
                .await
                .map_err(describe)?;
            outbox.push(
                room,
                peer,
                RtcSignal {
                    kind: SignalKind::Offer,
                    body: sdp,
                },
            );
            Ok(())
        }
        .await;
        if let Err(e) = result {
            bevy::log::warn!("bevy_iroh: webrtc offer: {e}");
        }
    });
}

pub(crate) fn answer(
    peer: EndpointId,
    room: Entity,
    sdp: String,
    settings: &RtcSettings,
    hub: Arc<MediaHub>,
    outbox: Outbox,
    _iroh: &Iroh,
) {
    let pc = match new_peer(peer, room, settings, hub, outbox.clone()) {
        Ok(pc) => pc,
        Err(e) => {
            bevy::log::warn!("bevy_iroh: webrtc: {e}");
            return;
        }
    };
    let on_channel = Closure::wrap(Box::new(move |event: JsValue| {
        let event: RtcDataChannelEvent = event.unchecked_into();
        attach(peer, event.channel());
    }) as Box<dyn FnMut(JsValue)>);
    pc.set_ondatachannel(Some(on_channel.as_ref().unchecked_ref()));
    PEERS.with(|peers| {
        if let Some(p) = peers.borrow_mut().get_mut(&peer) {
            p._closures.push(on_channel);
        }
    });
    wasm_bindgen_futures::spawn_local(async move {
        let result: Result<(), String> = async {
            JsFuture::from(pc.set_remote_description(&description(RtcSdpType::Offer, &sdp)))
                .await
                .map_err(describe)?;
            let answer = JsFuture::from(pc.create_answer()).await.map_err(describe)?;
            let sdp = sdp_of(&answer).ok_or("answer without sdp")?;
            JsFuture::from(pc.set_local_description(&description(RtcSdpType::Answer, &sdp)))
                .await
                .map_err(describe)?;
            outbox.push(
                room,
                peer,
                RtcSignal {
                    kind: SignalKind::Answer,
                    body: sdp,
                },
            );
            Ok(())
        }
        .await;
        if let Err(e) = result {
            bevy::log::warn!("bevy_iroh: webrtc answer: {e}");
        }
    });
}

pub(crate) fn accept_answer(peer: EndpointId, sdp: String) {
    let Some(pc) = PEERS.with(|peers| peers.borrow().get(&peer).map(|p| p.pc.clone())) else {
        return;
    };
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) =
            JsFuture::from(pc.set_remote_description(&description(RtcSdpType::Answer, &sdp))).await
        {
            bevy::log::warn!("bevy_iroh: webrtc answer: {}", describe(e));
        }
    });
}

pub(crate) fn add_candidate(peer: EndpointId, candidate: String) {
    let Some(pc) = PEERS.with(|peers| peers.borrow().get(&peer).map(|p| p.pc.clone())) else {
        return;
    };
    let init = RtcIceCandidateInit::new(&candidate);
    init.set_sdp_m_line_index(Some(0));
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) =
            JsFuture::from(pc.add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&init))).await
        {
            bevy::log::debug!("bevy_iroh: webrtc candidate: {}", describe(e));
        }
    });
}

pub(crate) fn close(peer: EndpointId, hub: &Arc<MediaHub>) {
    let Some(gone) = PEERS.with(|peers| peers.borrow_mut().remove(&peer)) else {
        return;
    };
    gone.pc.set_onicecandidate(None);
    gone.pc.set_oniceconnectionstatechange(None);
    gone.pc.set_ondatachannel(None);
    for channel in [&gone.audio, &gone.video].into_iter().flatten() {
        channel.set_onopen(None);
        channel.set_onmessage(None);
        channel.set_onclose(None);
    }
    gone.pc.close();
    if gone.announced {
        let (hub, id) = (hub.clone(), gone.link.id);
        wasm_bindgen_futures::spawn_local(async move { hub.remove_rtc_link(id).await });
    }
    let _ = Rc::new(());
}
