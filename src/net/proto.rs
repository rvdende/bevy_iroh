//! The wire format, and the one decision that makes it extensible.
//!
//! Everything on the wire is a [`Signed`] blob wrapping an [`Envelope`], and every envelope
//! carries an opaque `body: Vec<u8>` tagged with a [`Kind`]. The body is *not* a nested enum,
//! and that is deliberate.
//!
//! Postcard encodes an enum as a varint discriminant followed by the variant's fields, with no
//! length prefix. A decoder that meets a discriminant it does not know cannot skip the payload,
//! so it fails the whole message. With one big enum, the day one peer learns to send a new
//! variant is the day every older peer stops being able to read *any* message that sorts after
//! it. An opaque, length-prefixed body is what lets an old peer step over a new message and
//! keep reading. Adding a message is a new struct and a new [`Kind`]; nothing that exists
//! changes, and peers that predate it report it as unknown and carry on.

use anyhow::{Context, Result, bail, ensure};
use iroh::{EndpointId, SecretKey, Signature};
use iroh_gossip::proto::TopicId;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

#[cfg(not(target_arch = "wasm32"))]
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(target_arch = "wasm32")]
use web_time::{SystemTime, UNIX_EPOCH};

/// Bumped only for a change that is not backward compatible. Adding a [`Kind`] is not one.
pub const VERSION: u16 = 1;

/// The largest frame decoded on any transport. Received data is hostile: a length prefix is an
/// attacker's request that we allocate.
pub const MAX_FRAME: usize = 1 << 20;

/// The most items one batch may carry. More than this is a hostile length prefix, not a tick.
pub const MAX_ITEMS: usize = 4096;

/// What a message is: the only thing a receiver needs to route it.
///
/// A 64-bit FNV-1a hash of a stable name, so two features added in parallel cannot collide by
/// accident and nobody has to hand out ranges. Built-in kinds hash names under `bevy_iroh/`;
/// a user type's kind hashes its type path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Kind(pub u64);

impl Kind {
    /// The kind for a stable name. `const`, so built-in kinds are constants.
    pub const fn named(name: &str) -> Kind {
        Kind(fnv1a(name.as_bytes()))
    }

    /// The kind of a Rust type, from its type name. Stable between builds of the same source;
    /// a type that moves modules changes kind, which is the right outcome for a wire format.
    pub fn of<T: ?Sized>() -> Kind {
        Kind::named(std::any::type_name::<T>())
    }

    pub const HELLO: Kind = Kind::named("bevy_iroh/hello/1");
    pub const GOODBYE: Kind = Kind::named("bevy_iroh/goodbye/1");
    pub const PING: Kind = Kind::named("bevy_iroh/ping/1");
    pub const PONG: Kind = Kind::named("bevy_iroh/pong/1");
}

/// 64-bit FNV-1a. Not cryptographic and does not need to be: a kind is a label, not a proof.
pub const fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
        i += 1;
    }
    hash
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Kind::HELLO => f.write_str("hello"),
            Kind::GOODBYE => f.write_str("goodbye"),
            Kind::PING => f.write_str("ping"),
            Kind::PONG => f.write_str("pong"),
            _ => write!(f, "kind({:016x})", self.0),
        }
    }
}

/// A type that can travel as an envelope body.
pub trait Payload: Serialize + DeserializeOwned {
    const KIND: Kind;

    fn to_body(&self) -> Result<Vec<u8>> {
        postcard::to_stdvec(self).context("encode payload")
    }

    fn from_body(body: &[u8]) -> Result<Self> {
        postcard::from_bytes(body).context("decode payload")
    }
}

/// Who a message is meant for.
///
/// **`Only` is addressing, not confidentiality.** Over gossip every member still receives it and
/// is merely asked to ignore it. Anything private goes over a direct connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Audience {
    Everyone,
    Only(Vec<EndpointId>),
}

impl Audience {
    pub fn includes(&self, me: &EndpointId) -> bool {
        match self {
            Audience::Everyone => true,
            Audience::Only(peers) => peers.contains(me),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub version: u16,
    /// Redundant on gossip, where the subscription implies it; a direct connection carries
    /// messages for every room two peers share and has to tell them apart.
    pub topic: TopicId,
    pub kind: Kind,
    pub audience: Audience,
    /// Milliseconds since the epoch on the *sender's* clock. Not to be trusted for ordering.
    pub sent_at_ms: u64,
    pub body: Vec<u8>,
}

/// An envelope plus proof of who wrote it. Belt and braces on a direct connection; load
/// bearing on gossip, where a message arrives via forwarding peers we did not choose.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Signed {
    from: EndpointId,
    /// The encoded envelope, signed as bytes, so what is verified is exactly what is decoded.
    envelope: Vec<u8>,
    signature: Vec<u8>,
}

/// Sign and frame an already-encoded body.
pub fn encode_raw(
    secret: &SecretKey,
    topic: TopicId,
    audience: Audience,
    kind: Kind,
    body: Vec<u8>,
) -> Result<Vec<u8>> {
    let envelope = postcard::to_stdvec(&Envelope {
        version: VERSION,
        topic,
        kind,
        audience,
        sent_at_ms: now_ms(),
        body,
    })
    .context("encode envelope")?;
    let signature = secret.sign(&envelope);
    postcard::to_stdvec(&Signed {
        from: secret.public(),
        envelope,
        signature: signature.to_bytes().to_vec(),
    })
    .context("encode signed message")
}

/// Sign and frame a payload.
pub fn encode<P: Payload>(
    secret: &SecretKey,
    topic: TopicId,
    audience: Audience,
    payload: &P,
) -> Result<Vec<u8>> {
    encode_raw(secret, topic, audience, P::KIND, payload.to_body()?)
}

/// Verify and decode. Every failure here is reachable by a hostile peer, so all are errors and
/// none panic.
pub fn decode(bytes: &[u8]) -> Result<(EndpointId, Envelope)> {
    ensure!(
        bytes.len() <= MAX_FRAME,
        "frame of {} bytes exceeds the {MAX_FRAME} byte limit",
        bytes.len()
    );
    let signed: Signed = postcard::from_bytes(bytes).context("decode signed message")?;
    let signature: [u8; Signature::LENGTH] = signed
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("signature is {} bytes", signed.signature.len()))?;
    signed
        .from
        .verify(&signed.envelope, &Signature::from_bytes(&signature))
        .context("bad signature")?;
    let envelope: Envelope = postcard::from_bytes(&signed.envelope).context("decode envelope")?;
    if envelope.version != VERSION {
        bail!(
            "peer speaks protocol version {} and we speak {VERSION}",
            envelope.version
        );
    }
    Ok((signed.from, envelope))
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// -- built-in payloads ---------------------------------------------------------------------

/// Sent on joining and every few seconds after: presence, and the display name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub name: String,
}
impl Payload for Hello {
    const KIND: Kind = Kind::HELLO;
}

/// A courtesy on leaving. Peers that vanish without one are reaped by silence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goodbye;
impl Payload for Goodbye {
    const KIND: Kind = Kind::GOODBYE;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ping {
    pub nonce: u64,
}
impl Payload for Ping {
    const KIND: Kind = Kind::PING;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pong {
    pub nonce: u64,
    /// The ping's `sent_at_ms`, echoed, so the round trip is measured on one clock: ours.
    pub ping_sent_at_ms: u64,
}
impl Payload for Pong {
    const KIND: Kind = Kind::PONG;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topic() -> TopicId {
        TopicId::from_bytes([7; 32])
    }

    #[test]
    fn round_trips() {
        let secret = SecretKey::generate();
        let bytes = encode(
            &secret,
            topic(),
            Audience::Everyone,
            &Hello { name: "a".into() },
        )
        .unwrap();
        let (from, envelope) = decode(&bytes).unwrap();
        assert_eq!(from, secret.public());
        assert_eq!(envelope.kind, Kind::HELLO);
        assert_eq!(envelope.topic, topic());
        assert_eq!(Hello::from_body(&envelope.body).unwrap().name, "a");
    }

    #[test]
    fn rejects_a_forged_signature() {
        let secret = SecretKey::generate();
        let mut bytes = encode(&secret, topic(), Audience::Everyone, &Ping { nonce: 1 }).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn rejects_a_body_edited_in_flight() {
        let secret = SecretKey::generate();
        let bytes = encode(
            &secret,
            topic(),
            Audience::Everyone,
            &Hello {
                name: "pay alice".into(),
            },
        )
        .unwrap();
        let mut forged = bytes.clone();
        let at = forged.windows(5).position(|w| w == b"alice").unwrap();
        forged[at] = b'm';
        assert!(decode(&forged).is_err());
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(decode(&[]).is_err());
        assert!(decode(&[0xff; 64]).is_err());
        assert!(decode(&vec![0u8; MAX_FRAME + 1]).is_err());
    }

    /// The property the module exists for: a kind this build has never heard of still decodes
    /// as an envelope, attributed and sized, so the room keeps working.
    #[test]
    fn an_unknown_kind_still_decodes_as_an_envelope() {
        #[derive(Serialize, Deserialize)]
        struct FromTheFuture {
            hash: [u8; 32],
        }
        impl Payload for FromTheFuture {
            const KIND: Kind = Kind::named("someone-else/future/9");
        }
        let secret = SecretKey::generate();
        let bytes = encode(
            &secret,
            topic(),
            Audience::Everyone,
            &FromTheFuture { hash: [3; 32] },
        )
        .unwrap();
        let (from, envelope) = decode(&bytes).unwrap();
        assert_eq!(from, secret.public());
        assert_eq!(envelope.kind, Kind::named("someone-else/future/9"));
        assert!(!envelope.body.is_empty());
    }

    #[test]
    fn kinds_are_stable_and_distinct() {
        assert_eq!(Kind::named("bevy_iroh/hello/1"), Kind::HELLO);
        assert_ne!(Kind::HELLO, Kind::GOODBYE);
        assert_eq!(Kind::of::<Hello>(), Kind::of::<Hello>());
        assert_ne!(Kind::of::<Hello>(), Kind::of::<Goodbye>());
    }

    #[test]
    fn only_addresses_the_named_peers() {
        let alice = SecretKey::generate().public();
        let bob = SecretKey::generate().public();
        assert!(Audience::Everyone.includes(&alice));
        assert!(Audience::Only(vec![alice]).includes(&alice));
        assert!(!Audience::Only(vec![alice]).includes(&bob));
    }
}
