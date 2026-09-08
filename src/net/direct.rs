//! Point-to-point messages: a QUIC connection dialled to one peer's public key, which iroh
//! authenticates end to end and which nobody else is party to.
//!
//! Shaped as **one request, one response, one bi stream**, which is what a private message
//! needs and what a request with an answer needs too. A connection per exchange: wasteful at
//! chat volumes and the right shape to keep until a workload asks for pooling.

use std::sync::Arc;

use anyhow::{Context, Result};
use iroh::{
    Endpoint, EndpointAddr,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};

use super::{Ctx, Via, proto::MAX_FRAME};

/// Versioned, so that when this framing changes old peers fail to connect with "no common
/// protocol" rather than connect and misread each other.
pub const ALPN: &[u8] = b"bevy_iroh/direct/1";

/// Dial `to` and deliver one frame, returning the reply if there is one.
pub async fn request(
    endpoint: &Endpoint,
    to: EndpointAddr,
    frame: &[u8],
) -> Result<Option<Vec<u8>>> {
    let conn = endpoint.connect(to, ALPN).await.context("dial peer")?;
    let (mut send, mut recv) = conn.open_bi().await.context("open stream")?;
    send.write_all(frame).await.context("write request")?;
    send.finish().context("finish request")?;
    let reply = recv.read_to_end(MAX_FRAME).await.context("read reply")?;
    conn.close(0u32.into(), b"done");
    Ok((!reply.is_empty()).then_some(reply))
}

/// The accept side of the same exchange.
#[derive(Debug, Clone)]
pub struct DirectHandler {
    ctx: Arc<Ctx>,
}

impl DirectHandler {
    pub fn new(ctx: Arc<Ctx>) -> Self {
        Self { ctx }
    }
}

impl ProtocolHandler for DirectHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id();
        // One connection may carry several exchanges; serve until the peer stops opening
        // streams, which `accept_bi` reports as a connection error.
        while let Ok((mut send, mut recv)) = connection.accept_bi().await {
            let ctx = self.ctx.clone();
            // Per stream, so a slow or hostile request cannot hold up the next one.
            n0_future::task::spawn(async move {
                // `MAX_FRAME` is the whole defence against a peer that promises more bytes
                // than we have memory for.
                let frame = match recv.read_to_end(MAX_FRAME).await {
                    Ok(frame) => frame,
                    Err(e) => {
                        ctx.report(
                            None,
                            format!("direct read from {}: {e}", remote.fmt_short()),
                        );
                        return;
                    }
                };
                match ctx.handle_frame(&frame, Via::Direct).await {
                    Ok(Some(reply)) => {
                        if let Err(e) = send.write_all(&reply).await {
                            ctx.report(
                                None,
                                format!("direct reply to {}: {e}", remote.fmt_short()),
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(e) => ctx.report(None, format!("from {}: {e}", remote.fmt_short())),
                }
                let _ = send.finish();
            });
        }
        connection.closed().await;
        Ok(())
    }
}
