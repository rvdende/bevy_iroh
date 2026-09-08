//! The one durable thing about a user.
//!
//! A peer is an ed25519 keypair and iroh dials by its public half, so the secret key *is* the
//! identity. It lives in a file of its own and is generated exactly once. Where the file goes
//! is the application's business; this takes a path.

use std::{fs, path::Path};

use anyhow::{Context, Result};
use iroh::SecretKey;

/// Load the key at `path`, generating and saving one if it is not there yet.
///
/// The file holds the 32 raw bytes and nothing else. A file of the wrong length is an error,
/// not a fresh identity: silently replacing it would lose the real one after a bad read.
pub fn load_or_create(path: &Path) -> Result<SecretKey> {
    match fs::read(path) {
        Ok(bytes) => {
            let bytes: [u8; 32] = bytes.as_slice().try_into().with_context(|| {
                format!(
                    "{} holds {} bytes, not the 32 a secret key needs; move it aside to start over",
                    path.display(),
                    bytes.len()
                )
            })?;
            Ok(SecretKey::from_bytes(&bytes))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let secret = SecretKey::generate();
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            fs::write(path, secret.to_bytes())
                .with_context(|| format!("write {}", path.display()))?;
            restrict(path)?;
            Ok(secret)
        }
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Mode 0600. A private key readable by every process on the machine is not private.
#[cfg(unix)]
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restrict permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_once_then_loads_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        let first = load_or_create(&path).unwrap();
        let second = load_or_create(&path).unwrap();
        assert_eq!(first.public(), second.public());
    }

    #[cfg(unix)]
    #[test]
    fn is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        load_or_create(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
    }

    #[test]
    fn a_truncated_file_is_an_error_not_a_new_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        fs::write(&path, [1, 2, 3]).unwrap();
        assert!(load_or_create(&path).is_err());
    }
}
