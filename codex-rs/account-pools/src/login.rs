//! Reuse native browser/device OAuth, staging credentials in memory until an alias is committed.

use crate::AccountStore;
use anyhow::Result;
use codex_login::AuthCredentialsStoreMode;
use codex_login::ServerOptions;
use codex_protocol::account_pool::ManagedAccount;

pub struct AccountLogin {
    store: AccountStore,
    alias: String,
    home: tempfile::TempDir,
}

impl AccountLogin {
    pub fn new(store: AccountStore, alias: String) -> Result<Self> {
        crate::storage::validate_name(&alias)?;
        if store.auth_config.auth_credentials_store_mode == AuthCredentialsStoreMode::Ephemeral {
            anyhow::bail!("Named accounts require file, keyring or auto credential storage");
        }
        if !store
            .auth_config
            .is_login_method_allowed(codex_protocol::config_types::ForcedLoginMethod::Chatgpt)
        {
            anyhow::bail!("ChatGPT login is disabled by authentication policy");
        }
        Ok(Self {
            store,
            alias,
            home: tempfile::tempdir()?,
        })
    }

    pub fn options(&self) -> ServerOptions {
        ServerOptions::new(
            self.home.path().to_path_buf(),
            codex_login::oauth_client_id(),
            self.store.auth_config.effective_chatgpt_workspaces(),
            AuthCredentialsStoreMode::Ephemeral,
            self.store.auth_config.keyring_backend_kind,
            self.store.auth_config.auth_route_config.clone(),
        )
    }

    pub async fn finish(self) -> Result<ManagedAccount> {
        self.store
            .import_from_store(
                &self.alias,
                self.home.path(),
                AuthCredentialsStoreMode::Ephemeral,
            )
            .await
    }
}

impl Drop for AccountLogin {
    fn drop(&mut self) {
        let _ = codex_login::logout(
            self.home.path(),
            AuthCredentialsStoreMode::Ephemeral,
            self.store.auth_config.keyring_backend_kind,
        );
    }
}
