//! Serialize refresh-token rotation across processes sharing a credential store.

use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::time::Duration;

pub(super) async fn acquire(
    codex_home: &Path,
    mode: codex_config::types::AuthCredentialsStoreMode,
    keyring: super::AuthKeyringBackendKind,
) -> io::Result<File> {
    loop {
        let resolved = super::managed_link::resolve(codex_home, mode, keyring)?;
        std::fs::create_dir_all(&resolved)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(resolved.join("auth.refresh.lock"))?;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error),
            }
        }
        if super::managed_link::resolve(codex_home, mode, keyring)? == resolved {
            return Ok(file);
        }
    }
}
