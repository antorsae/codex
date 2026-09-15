use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_login::AuthConfig;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_protocol::account_pool::AccountPool;
use codex_protocol::account_pool::AccountPoolConfig;
use codex_protocol::account_pool::AccountSelection;
use codex_protocol::account_pool::ManagedAccount;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::Digest;
use sha2::Sha256;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct AccountStore {
    pub(crate) auth_config: AuthConfig,
}

impl AccountStore {
    pub fn from_config(config: &impl codex_login::AuthManagerConfig) -> Self {
        Self::new(AuthConfig {
            codex_home: config.codex_home(),
            auth_credentials_store_mode: config.cli_auth_credentials_store_mode(),
            keyring_backend_kind: config.auth_keyring_backend_kind(),
            forced_login_method: config.forced_login_method(),
            forced_chatgpt_workspace_id: config.forced_chatgpt_workspace_id(),
            managed_auth_policy: config.managed_auth_policy(),
            chatgpt_base_url: Some(config.chatgpt_base_url()),
            auth_route_config: config.auth_route_config(),
        })
    }

    pub fn new(auth_config: AuthConfig) -> Self {
        Self { auth_config }
    }

    pub fn read(&self) -> Result<AccountPoolConfig> {
        let config =
            read_json::<AccountPoolConfig>(&self.root().join("accounts.json"))?.unwrap_or_default();
        validate_config(&config)?;
        Ok(config)
    }

    pub fn root(&self) -> PathBuf {
        self.auth_config.codex_home.join("accounts")
    }

    #[expect(
        clippy::expect_used,
        reason = "Serializing a pair of strings to a JSON byte vector is infallible; the encoding is part of the persisted credential identity"
    )]
    pub fn credential_home(&self, account: &ManagedAccount) -> PathBuf {
        let identity = serde_json::to_vec(&(&account.user_id, &account.workspace_id))
            .expect("identity strings serialize");
        self.root()
            .join("credentials")
            .join(format!("{:x}", Sha256::digest(identity)))
    }

    /// Import a managed ChatGPT login without replacing the legacy login.
    /// Duplicate user/workspace identities resolve to their existing alias.
    pub async fn import(&self, alias: &str, source_home: &Path) -> Result<ManagedAccount> {
        self.import_from_store(
            alias,
            source_home,
            self.auth_config.auth_credentials_store_mode,
        )
        .await
    }

    pub(crate) async fn import_from_store(
        &self,
        alias: &str,
        source_home: &Path,
        source_mode: codex_login::AuthCredentialsStoreMode,
    ) -> Result<ManagedAccount> {
        validate_name(alias)?;
        if self.auth_config.auth_credentials_store_mode
            == codex_login::AuthCredentialsStoreMode::Ephemeral
        {
            bail!("Named accounts require file, keyring or auto credential storage");
        }
        let _guard = lock(&self.root().join("config.lock")).await?;
        let _source_guard = lock(&source_home.join("auth.refresh.lock")).await?;
        let mut source_config = self.auth_config.clone();
        source_config.codex_home = source_home.to_owned();
        source_config.auth_credentials_store_mode = source_mode;
        let auth = AuthManager::managed_from_auth_config(source_config.clone())
            .await
            .auth_cached()
            .context("No stored ChatGPT login to import")?;
        if !matches!(auth, CodexAuth::Chatgpt(_)) || !self.auth_config.allows_auth(&auth) {
            bail!(
                "A locally managed ChatGPT login allowed by the authentication policy is required"
            );
        }
        let mut account = ManagedAccount {
            alias: alias.to_owned(),
            user_id: auth
                .get_chatgpt_user_id()
                .context("Login has no authenticated user identity")?,
            workspace_id: auth
                .get_account_id()
                .context("Login has no workspace identity")?,
            email: auth.get_account_email(),
            plan: auth.get_token_data()?.id_token.get_chatgpt_plan_type_raw(),
        };
        let mut config = self.read()?;
        if let Some(existing) = config.accounts.iter().find(|entry| {
            entry.user_id == account.user_id && entry.workspace_id == account.workspace_id
        }) {
            account.alias.clone_from(&existing.alias);
        }
        if config.accounts.iter().any(|entry| {
            entry.alias == alias
                && (entry.user_id != account.user_id || entry.workspace_id != account.workspace_id)
        }) {
            bail!("Account alias already exists: {alias}");
        }
        let home = self.credential_home(&account);
        let _auth_guard = if source_home == home {
            None
        } else {
            Some(lock(&home.join("auth.refresh.lock")).await?)
        };
        let auth = AuthManager::managed_from_auth_config(source_config)
            .await
            .auth_cached()
            .context("Login was removed while importing")?;
        if auth.get_chatgpt_user_id().as_deref() != Some(&account.user_id)
            || auth.get_account_id().as_deref() != Some(&account.workspace_id)
        {
            bail!("Login identity changed during import");
        }
        let payload = codex_login::load_auth_dot_json(
            source_home,
            source_mode,
            self.auth_config.keyring_backend_kind,
        )?
        .context("Login was removed while importing")?;
        // Only subscription tokens belong to this store; omit unrelated auth material.
        let payload = codex_login::AuthDotJson {
            auth_mode: Some(codex_protocol::auth::AuthMode::Chatgpt),
            tokens: Some(auth.get_token_data()?),
            last_refresh: payload.last_refresh,
            openai_api_key: None,
            agent_identity: None,
            personal_access_token: None,
            bedrock_api_key: None,
            bedrock_access_keys: None,
        };
        codex_login::save_auth(
            &home,
            &payload,
            self.auth_config.auth_credentials_store_mode,
            self.auth_config.keyring_backend_kind,
        )?;
        if let Some(existing) = config
            .accounts
            .iter_mut()
            .find(|entry| entry.alias == account.alias)
        {
            *existing = account.clone();
        } else {
            config.accounts.push(account.clone());
        }
        if source_home == self.auth_config.codex_home
            && source_mode != codex_login::AuthCredentialsStoreMode::Ephemeral
        {
            let original = codex_login::load_auth_dot_json(
                source_home,
                source_mode,
                self.auth_config.keyring_backend_kind,
            )?
            .context("Login changed during import")?;
            codex_login::link_managed_chatgpt_login(source_home, &home, &original)?;
        }
        write_json(&self.root().join("accounts.json"), &config)?;
        Ok(account)
    }

    pub async fn remove_account(&self, alias: &str) -> Result<()> {
        let _guard = lock(&self.root().join("config.lock")).await?;
        let mut config = self.read()?;
        let account = config
            .accounts
            .iter()
            .find(|account| account.alias == alias)
            .context("Unknown account")?
            .clone();
        if config
            .pools
            .iter()
            .any(|pool| pool.accounts.iter().any(|name| name == alias))
        {
            bail!("Remove the account from its pools first");
        }
        config.accounts.retain(|account| account.alias != alias);
        if config.default_selection == Some(AccountSelection::Account(alias.to_owned())) {
            config.default_selection = None;
        }
        let _legacy_guard = lock(&self.auth_config.codex_home.join("auth.refresh.lock")).await?;
        let _auth_guard = lock(&self.credential_home(&account).join("auth.refresh.lock")).await?;
        codex_login::detach_managed_chatgpt_login(
            &self.auth_config.codex_home,
            &self.credential_home(&account),
            self.auth_config.auth_credentials_store_mode,
            self.auth_config.keyring_backend_kind,
        )?;
        codex_login::logout(
            &self.credential_home(&account),
            self.auth_config.auth_credentials_store_mode,
            self.auth_config.keyring_backend_kind,
        )?;
        write_json(&self.root().join("accounts.json"), &config)
    }

    pub async fn put_pool(&self, pool: AccountPool, create: bool) -> Result<()> {
        let _guard = lock(&self.root().join("config.lock")).await?;
        let mut config = self.read()?;
        let existing = config
            .pools
            .iter()
            .position(|entry| entry.name == pool.name);
        match (existing, create) {
            (Some(_), true) => bail!("Pool already exists"),
            (None, false) => bail!("Unknown pool"),
            (Some(index), false) => config.pools[index] = pool,
            (None, true) => config.pools.push(pool),
        }
        validate_config(&config)?;
        write_json(&self.root().join("accounts.json"), &config)
    }

    pub async fn remove_pool(&self, name: &str) -> Result<()> {
        let _guard = lock(&self.root().join("config.lock")).await?;
        let mut config = self.read()?;
        if !config.pools.iter().any(|pool| pool.name == name) {
            bail!("Unknown pool: {name}");
        }
        config.pools.retain(|pool| pool.name != name);
        if config.default_selection == Some(AccountSelection::Pool(name.to_owned())) {
            config.default_selection = None;
        }
        write_json(&self.root().join("accounts.json"), &config)
    }

    pub async fn select_default(&self, selection: Option<AccountSelection>) -> Result<()> {
        let _guard = lock(&self.root().join("config.lock")).await?;
        let mut config = self.read()?;
        config.default_selection = selection;
        validate_config(&config)?;
        write_json(&self.root().join("accounts.json"), &config)
    }

    pub async fn manager(&self, account: &ManagedAccount) -> Result<Arc<AuthManager>> {
        if !self.read()?.accounts.iter().any(|entry| {
            entry.alias == account.alias
                && entry.user_id == account.user_id
                && entry.workspace_id == account.workspace_id
        }) {
            bail!("Account was removed or changed: {}", account.alias);
        }
        let mut config = self.auth_config.clone();
        config.codex_home = self.credential_home(account);
        let manager = AuthManager::managed_from_auth_config(config).await;
        let auth = manager.auth().await.context("Account requires login")?;
        if auth.get_chatgpt_user_id().as_deref() != Some(&account.user_id)
            || auth.get_account_id().as_deref() != Some(&account.workspace_id)
            || !matches!(auth, CodexAuth::Chatgpt(_))
        {
            bail!("Stored credentials do not match account {}", account.alias);
        }
        Ok(manager)
    }

    /// Explicitly redeem a banked reset, retaining ambiguous attempts for safe retries.
    pub async fn redeem(
        &self,
        alias: &str,
        model: Option<&str>,
    ) -> Result<codex_protocol::account_pool::ManagedAccountUsage> {
        let _guard = lock(&self.root().join("recovery.lock")).await?;
        let account = self
            .read()?
            .accounts
            .into_iter()
            .find(|account| account.alias == alias)
            .context("Unknown account")?;
        let backend = crate::ManagedBackend::new(self.clone());
        let usage = backend.usage(&account, model).await;
        let credit = crate::redemption::pending_credit(self, &account)?
            .or_else(|| {
                crate::policy::earliest_credit(&usage, chrono::Utc::now().timestamp())
                    .map(|credit| credit.id.clone())
            })
            .context("No usable banked reset is known")?;
        crate::redemption::redeem(
            self,
            &backend,
            &account,
            model,
            &credit,
            crate::redemption::RedemptionMode::Explicit,
        )
        .await
    }
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
    {
        bail!("Names must contain 1-64 letters, digits, underscores or hyphens");
    }
    Ok(())
}

fn validate_config(config: &AccountPoolConfig) -> Result<()> {
    let mut aliases = std::collections::HashSet::new();
    let mut identities = std::collections::HashSet::new();
    for account in &config.accounts {
        validate_name(&account.alias)?;
        if account.user_id.is_empty()
            || account.workspace_id.is_empty()
            || !aliases.insert(&account.alias)
            || !identities.insert((&account.user_id, &account.workspace_id))
        {
            bail!("Duplicate or invalid account identity");
        }
    }
    let mut names = std::collections::HashSet::new();
    for pool in &config.pools {
        validate_name(&pool.name)?;
        let unique: std::collections::HashSet<_> = pool.accounts.iter().collect();
        if pool.accounts.is_empty()
            || pool.accounts.len() > 128
            || unique.len() != pool.accounts.len()
            || !unique.iter().all(|alias| aliases.contains(*alias))
            || !names.insert(&pool.name)
        {
            bail!("Pool must have a unique name and 1-128 distinct existing accounts");
        }
    }
    match &config.default_selection {
        Some(AccountSelection::Account(name)) if !aliases.contains(name) => {
            bail!("Unknown default account")
        }
        Some(AccountSelection::Pool(name)) if !names.contains(name) => {
            bail!("Unknown default pool")
        }
        Some(AccountSelection::Account(_) | AccountSelection::Pool(_)) | None => Ok(()),
    }
}

/// Advisory locks are owned by the open file, so process death releases them.
pub(crate) async fn lock(path: &Path) -> Result<File> {
    let parent = path.parent().context("Lock needs a parent directory")?;
    std::fs::create_dir_all(parent)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => {
                tokio::time::sleep(Duration::from_millis(50)).await
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }
}

pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("Invalid account state")?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Persist before sending a reset request. Both file contents and its directory entry are durable.
pub(crate) fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("State needs a parent directory")?;
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
