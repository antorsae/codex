//! Preserve a legacy login after import without creating two refresh-token authorities.

use super::AuthDotJson;
use super::AuthKeyringBackendKind;
use super::load_auth_dot_json;
use codex_config::types::AuthCredentialsStoreMode;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
struct ManagedLink {
    credential_key: String,
    original_login_digest: String,
}

fn digest(auth: &AuthDotJson) -> io::Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(auth)?)))
}

/// Link only the exact imported login. A later login or logout naturally severs this link.
pub fn link_managed_chatgpt_login(
    source: &Path,
    canonical: &Path,
    original: &AuthDotJson,
) -> io::Result<()> {
    let key = canonical
        .strip_prefix(source.join("accounts").join("credentials"))
        .ok()
        .and_then(|path| path.to_str())
        .filter(|key| valid_key(key))
        .ok_or_else(|| {
            io::Error::other("Managed credentials must be inside the user account store")
        })?;
    let link = ManagedLink {
        credential_key: key.to_owned(),
        original_login_digest: digest(original)?,
    };
    let mut temp = tempfile::NamedTempFile::new_in(source)?;
    temp.write_all(&serde_json::to_vec(&link)?)?;
    temp.as_file().sync_all()?;
    temp.persist(source.join("auth.managed.json"))?;
    Ok(())
}

fn valid_key(key: &str) -> bool {
    key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Preserve the legacy login when removing its managed alias. Call with both stores locked.
pub fn detach_managed_chatgpt_login(
    source: &Path,
    canonical: &Path,
    mode: AuthCredentialsStoreMode,
    keyring: AuthKeyringBackendKind,
) -> io::Result<()> {
    if resolve(source, mode, keyring)? == canonical {
        let auth = load_auth_dot_json(canonical, mode, keyring)?
            .ok_or_else(|| io::Error::other("Managed login is missing"))?;
        super::save_auth(source, &auth, mode, keyring)?;
        std::fs::remove_file(source.join("auth.managed.json"))?;
    }
    Ok(())
}

pub(super) fn resolve(
    source: &Path,
    mode: AuthCredentialsStoreMode,
    keyring: AuthKeyringBackendKind,
) -> io::Result<PathBuf> {
    let bytes = match std::fs::read(source.join("auth.managed.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(source.to_owned()),
        Err(error) => return Err(error),
    };
    let link: ManagedLink = serde_json::from_slice(&bytes)?;
    if !valid_key(&link.credential_key) {
        return Err(io::Error::other("Invalid managed credential reference"));
    }
    let Some(original) = load_auth_dot_json(source, mode, keyring)? else {
        return Ok(source.to_owned());
    };
    if digest(&original)? != link.original_login_digest {
        return Ok(source.to_owned());
    }
    Ok(source
        .join("accounts")
        .join("credentials")
        .join(link.credential_key))
}
