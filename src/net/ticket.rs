//! What you hand someone else so they can join.
//!
//! A ticket is a **capability**: it names a topic and enough bootstrap addresses to reach
//! someone already in it, and anyone holding one can join. There is no revocation. That is
//! fine for "here is a link to my room" and wrong for anything private; an allowlist and
//! rotating the topic are the answers when that day comes.
//!
//! The encoding (postcard, base32 without padding, lowercase, a `chan` prefix) is byte for byte
//! what substrate's registry mints, so tickets from either side parse on the other.

use std::{fmt, str::FromStr};

use anyhow::{Context, Result, ensure};
use iroh::EndpointAddr;
use iroh_gossip::proto::TopicId;
use serde::{Deserialize, Serialize};

/// Prefixed so a mistyped or truncated paste fails here with a sentence, not inside postcard
/// with a byte offset.
const PREFIX: &str = "chan";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomTicket {
    pub topic: TopicId,
    /// So the joiner can show a name before anyone has said anything.
    pub name: String,
    /// Who to dial to find the swarm. Any one of them reaching us is enough.
    pub peers: Vec<EndpointAddr>,
}

impl RoomTicket {
    pub fn new(topic: TopicId, name: impl Into<String>, peers: Vec<EndpointAddr>) -> Self {
        Self {
            topic,
            name: name.into(),
            peers,
        }
    }
}

/// Lowercase because tickets get pasted into chat clients that like to capitalise things, and
/// `from_str` upcases before decoding.
impl fmt::Display for RoomTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = postcard::to_stdvec(self).expect("a ticket is always encodable");
        let mut text = data_encoding::BASE32_NOPAD.encode(&bytes);
        text.make_ascii_lowercase();
        write!(f, "{PREFIX}{text}")
    }
}

impl FromStr for RoomTicket {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        let body = s
            .get(..PREFIX.len())
            .filter(|head| head.eq_ignore_ascii_case(PREFIX))
            .map(|_| &s[PREFIX.len()..])
            .with_context(|| format!("a ticket starts with `{PREFIX}`"))?;
        ensure!(!body.is_empty(), "the ticket is empty");
        let bytes = data_encoding::BASE32_NOPAD
            .decode(body.to_ascii_uppercase().as_bytes())
            .context("the ticket is not valid base32")?;
        postcard::from_bytes(&bytes).context("the ticket is malformed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    fn addr() -> EndpointAddr {
        EndpointAddr::new(SecretKey::generate().public())
    }

    #[test]
    fn round_trips_through_a_string() {
        let ticket = RoomTicket::new(TopicId::from_bytes([9; 32]), "lab", vec![addr(), addr()]);
        let parsed: RoomTicket = ticket.to_string().parse().unwrap();
        assert_eq!(parsed, ticket);
    }

    #[test]
    fn survives_the_journey_through_a_chat_client() {
        let ticket = RoomTicket::new(TopicId::from_bytes([1; 32]), "lab", vec![addr()]);
        let mangled = format!("  {}  ", ticket.to_string().to_uppercase());
        assert_eq!(mangled.parse::<RoomTicket>().unwrap(), ticket);
    }

    #[test]
    fn bad_input_says_what_is_wrong() {
        assert!("".parse::<RoomTicket>().is_err());
        assert!("chan".parse::<RoomTicket>().is_err());
        assert!("nonsense".parse::<RoomTicket>().is_err());
        assert!("chan!!!!".parse::<RoomTicket>().is_err());
        let ticket = RoomTicket::new(TopicId::from_bytes([1; 32]), "lab", vec![addr()]);
        assert!(ticket.to_string()[..12].parse::<RoomTicket>().is_err());
    }
}
