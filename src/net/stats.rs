//! What the wire is doing, once a second: bytes, rates, how much of it goes through a relay,
//! and a round trip per peer.
//!
//! The round trip comes from one held, idle QUIC connection per peer on the direct ALPN:
//! nothing is sent on it, and iroh's own keepalives refresh the path's RTT estimate. A dial
//! that fails is parked for a while rather than retried every second.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use iroh::{EndpointAddr, EndpointId, endpoint::Connection};
use n0_future::time::{Duration, Instant, timeout};
use tokio::sync::mpsc;

use super::{Ctx, direct};

pub const SAMPLE_EVERY: Duration = Duration::from_secs(1);
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);
const REDIAL_AFTER: Duration = Duration::from_secs(30);

/// A sample of the endpoint.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    /// Bytes sent, ever.
    pub sent: u64,
    pub received: u64,
    /// Bytes per second since the previous sample.
    pub tx_rate: f64,
    pub rx_rate: f64,
    /// The part of `sent` / `received` that went through a relay.
    pub relay_sent: u64,
    pub relay_received: u64,
    /// One per known peer.
    pub links: Vec<Link>,
    /// Where this node can be reached right now: what a registry heartbeat should carry.
    pub addr: Option<EndpointAddr>,
}

impl Stats {
    /// The best round trip to any peer.
    pub fn best_rtt(&self) -> Option<Duration> {
        self.links.iter().filter_map(|link| link.rtt).min()
    }

    /// Every measured link goes through a relay: no hole punch succeeded.
    pub fn all_relayed(&self) -> bool {
        let measured: Vec<&Link> = self.links.iter().filter(|l| l.rtt.is_some()).collect();
        !measured.is_empty() && measured.iter().all(|link| link.relayed)
    }
}

#[derive(Clone, Debug)]
pub struct Link {
    pub peer: EndpointId,
    pub name: Option<String>,
    /// `None` until a probe connection is up.
    pub rtt: Option<Duration>,
    pub relayed: bool,
}

impl Link {
    pub fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| self.peer.fmt_short().to_string())
    }
}

#[derive(Clone, Copy, Default)]
struct Counters {
    sent: u64,
    received: u64,
    relay_sent: u64,
    relay_received: u64,
}

pub struct Sampler {
    last: Option<(Counters, Instant)>,
    probes: HashMap<EndpointId, Connection>,
    dialling: HashSet<EndpointId>,
    failed: HashMap<EndpointId, Instant>,
    dialled: (
        mpsc::UnboundedSender<(EndpointId, Option<Connection>)>,
        mpsc::UnboundedReceiver<(EndpointId, Option<Connection>)>,
    ),
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        Self {
            last: None,
            probes: HashMap::new(),
            dialling: HashSet::new(),
            failed: HashMap::new(),
            dialled: mpsc::unbounded_channel(),
        }
    }

    /// One sample. `peers` is who to measure: the members the application knows about.
    pub fn sample(&mut self, ctx: &Arc<Ctx>, peers: &[(EndpointId, Option<String>)]) -> Stats {
        let now = Instant::now();
        let counters = read_counters(ctx);
        let rates = match self.last.replace((counters, now)) {
            Some((before, when)) => {
                let elapsed = now.saturating_duration_since(when).as_secs_f64();
                if elapsed > 0.0 {
                    (
                        counters.sent.saturating_sub(before.sent) as f64 / elapsed,
                        counters.received.saturating_sub(before.received) as f64 / elapsed,
                    )
                } else {
                    (0.0, 0.0)
                }
            }
            None => (0.0, 0.0),
        };
        Stats {
            sent: counters.sent,
            received: counters.received,
            tx_rate: rates.0,
            rx_rate: rates.1,
            relay_sent: counters.relay_sent,
            relay_received: counters.relay_received,
            links: self.links(ctx, peers, now),
            addr: Some(ctx.endpoint().addr()),
        }
    }

    fn links(
        &mut self,
        ctx: &Arc<Ctx>,
        peers: &[(EndpointId, Option<String>)],
        now: Instant,
    ) -> Vec<Link> {
        while let Ok((peer, connection)) = self.dialled.1.try_recv() {
            self.dialling.remove(&peer);
            match connection {
                Some(connection) => {
                    self.failed.remove(&peer);
                    self.probes.insert(peer, connection);
                }
                None => {
                    self.failed.insert(peer, now);
                }
            }
        }
        let known: HashSet<EndpointId> = peers.iter().map(|(id, _)| *id).collect();
        self.probes
            .retain(|peer, conn| known.contains(peer) && conn.close_reason().is_none());
        self.failed.retain(|peer, _| known.contains(peer));
        let mut links: Vec<Link> = peers
            .iter()
            .map(|(peer, name)| {
                let path = self.probes.get(peer).and_then(selected_path);
                Link {
                    peer: *peer,
                    name: name.clone(),
                    rtt: path.map(|(rtt, _)| rtt),
                    relayed: path.is_some_and(|(_, relayed)| relayed),
                }
            })
            .collect();
        links.sort_by_key(Link::label);
        for peer in known {
            let waiting = self
                .failed
                .get(&peer)
                .is_some_and(|at| now.saturating_duration_since(*at) < REDIAL_AFTER);
            if self.probes.contains_key(&peer) || waiting || !self.dialling.insert(peer) {
                continue;
            }
            self.dial(ctx, peer);
        }
        links
    }

    fn dial(&self, ctx: &Arc<Ctx>, peer: EndpointId) {
        let addr = ctx.addr_for(peer);
        let endpoint = ctx.endpoint().clone();
        let report = self.dialled.0.clone();
        n0_future::task::spawn(async move {
            let dial = endpoint.connect(addr, direct::ALPN);
            let connection = match timeout(DIAL_TIMEOUT, dial).await {
                Ok(Ok(connection)) => Some(connection),
                Ok(Err(_)) | Err(_) => None,
            };
            let _ = report.send((peer, connection));
        });
    }
}

fn selected_path(connection: &Connection) -> Option<(Duration, bool)> {
    connection
        .paths()
        .iter()
        .find(|path| path.is_selected())
        .map(|path| (path.rtt(), path.is_relay()))
}

fn read_counters(ctx: &Arc<Ctx>) -> Counters {
    let socket = &ctx.endpoint().metrics().socket;
    Counters {
        sent: socket.send_ipv4.get() + socket.send_ipv6.get() + socket.send_relay.get(),
        received: socket.recv_data_ipv4.get()
            + socket.recv_data_ipv6.get()
            + socket.recv_data_relay.get()
            + socket.recv_data_custom.get(),
        relay_sent: socket.send_relay.get(),
        relay_received: socket.recv_data_relay.get(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(rtt: Option<u64>, relayed: bool) -> Link {
        Link {
            peer: iroh::SecretKey::generate().public(),
            name: None,
            rtt: rtt.map(Duration::from_millis),
            relayed,
        }
    }

    #[test]
    fn the_headline_round_trip_is_the_best_one() {
        let stats = Stats {
            links: vec![
                link(Some(120), true),
                link(Some(14), false),
                link(None, false),
            ],
            ..Stats::default()
        };
        assert_eq!(stats.best_rtt(), Some(Duration::from_millis(14)));
    }

    #[test]
    fn the_relay_warning_needs_every_measured_link_to_be_relayed() {
        let all = Stats {
            links: vec![link(Some(80), true), link(Some(95), true)],
            ..Stats::default()
        };
        assert!(all.all_relayed());
        let some = Stats {
            links: vec![link(Some(80), true), link(Some(12), false)],
            ..Stats::default()
        };
        assert!(!some.all_relayed());
        assert!(!Stats::default().all_relayed());
    }
}
