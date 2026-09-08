//! What only a browser has. Behind the `wasm` feature, on `wasm32`.
//!
//! The core transport runs in a page without any of this: iroh reaches relays over
//! WebSockets and resolves peers over HTTPS. This module is the two things a page wants on
//! top: an identity that survives a reload, and the ticket out of the address bar.

use iroh::SecretKey;

use crate::{Identity, RoomTicket};

/// An identity kept in `localStorage` under `key`, generated on the first visit.
///
/// Ephemeral when storage is unavailable (a private window, a page that denies it), so the
/// app still runs; peers just will not recognise it next time.
pub fn stored_identity(key: &str) -> Identity {
    let Some(storage) = storage() else {
        return Identity::Ephemeral;
    };
    if let Ok(Some(hex)) = storage.get_item(key)
        && let Some(secret) = from_hex(&hex)
    {
        return Identity::Key(secret);
    }
    let secret = SecretKey::generate();
    let _ = storage.set_item(key, &to_hex(&secret.to_bytes()));
    Identity::Key(secret)
}

/// The ticket named by `?<param>=` in the page's URL, if any.
pub fn ticket_from_url(param: &str) -> Option<RoomTicket> {
    let search = web_sys::window()?.location().search().ok()?;
    let query = search.strip_prefix('?').unwrap_or(&search);
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == param).then(|| v.parse().ok()).flatten()
    })
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(hex: &str) -> Option<SecretKey> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk).ok()?;
        bytes[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(SecretKey::from_bytes(&bytes))
}
