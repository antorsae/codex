use crate::AccountStore;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_backend_client::Client;
use codex_backend_client::ConsumeRateLimitResetCreditCode;
use codex_protocol::account_pool::AccountQuotaWindow;
use codex_protocol::account_pool::BankedReset;
use codex_protocol::account_pool::ManagedAccount;
use codex_protocol::account_pool::ManagedAccountUsage;
use std::future::Future;
use std::time::Duration;

/// The coordinator's network boundary. Implementations must return fresh, identity-checked
/// observations and must preserve the caller's idempotency key on every redemption attempt.
pub(crate) trait AccountBackend: Send + Sync {
    fn usage(
        &self,
        account: &ManagedAccount,
        model: Option<&str>,
    ) -> impl Future<Output = ManagedAccountUsage> + Send;
    fn redeem(
        &self,
        account: &ManagedAccount,
        key: &str,
        credit: &str,
    ) -> impl Future<Output = Result<ConsumeRateLimitResetCreditCode>> + Send;
}

#[derive(Clone)]
pub struct ManagedBackend {
    store: AccountStore,
}

impl ManagedBackend {
    pub fn new(store: AccountStore) -> Self {
        Self { store }
    }

    pub async fn usage(
        &self,
        account: &ManagedAccount,
        model: Option<&str>,
    ) -> ManagedAccountUsage {
        <Self as AccountBackend>::usage(self, account, model).await
    }

    async fn client(
        &self,
        account: &ManagedAccount,
    ) -> Result<(Client, std::sync::Arc<codex_login::AuthManager>)> {
        let manager = self.store.manager(account).await?;
        let auth = manager.auth().await.context("Account requires login")?;
        Ok((
            Client::from_auth(
                self.store
                    .auth_config
                    .chatgpt_base_url
                    .as_deref()
                    .context("Missing ChatGPT backend URL")?,
                &auth,
                self.store
                    .auth_config
                    .auth_route_config
                    .http_client_factory()
                    .clone(),
            ),
            manager,
        ))
    }

    async fn read_usage(
        &self,
        account: &ManagedAccount,
        model: Option<&str>,
        result: &mut ManagedAccountUsage,
    ) -> Result<()> {
        let (mut client, manager) = self.client(account).await?;
        let usage = match client.get_rate_limits_with_reset_credits().await {
            Err(error)
                if error
                    .downcast_ref::<codex_backend_client::RequestError>()
                    .is_some_and(codex_backend_client::RequestError::is_unauthorized) =>
            {
                manager.refresh_token().await?;
                client = self.client(account).await?.0;
                client.get_rate_limits_with_reset_credits().await?
            }
            result => result?,
        };
        if usage
            .account_id
            .as_deref()
            .is_some_and(|id| id != account.workspace_id)
            || usage
                .user_id
                .as_deref()
                .is_some_and(|id| id != account.user_id)
        {
            bail!("Usage identity does not match account");
        }
        result.ordinary_usage_allowed = usage.ordinary_usage_allowed;
        if let Some(plan) = usage.rate_limits.first().and_then(|limit| limit.plan_type) {
            result.account.plan = serde_json::to_value(plan)?.as_str().map(str::to_owned);
        }
        for limit in usage.rate_limits {
            let limit_id = limit.limit_id.unwrap_or_else(|| "codex".to_owned());
            if limit.spend_control_reached == Some(true) {
                bail!("Spend control prevents quota recovery");
            }
            for window in [limit.primary, limit.secondary].into_iter().flatten() {
                let minutes = window
                    .window_minutes
                    .context("Unknown quota window duration")?;
                if minutes <= 0
                    || !window.used_percent.is_finite()
                    || !(0.0..=100.0).contains(&window.used_percent)
                {
                    bail!("Invalid quota window");
                }
                result.windows.push(AccountQuotaWindow {
                    limit_id: limit_id.clone(),
                    model: limit.normal_model_slug.clone(),
                    remaining_percent: 100.0 - window.used_percent,
                    window_minutes: minutes,
                    resets_at: window.resets_at,
                });
            }
        }
        result.available_resets = usage
            .rate_limit_reset_credits
            .map(|credits| credits.available_count);
        if let Some(model) = model {
            result.model_supported = Some(
                client
                    .get_account_model_slugs()
                    .await?
                    .iter()
                    .any(|slug| slug == model),
            );
        }
        // Reset details are a separate runtime capability. Failure never authorizes a reset.
        if let Ok(details) = client.list_rate_limit_reset_credits().await {
            let now = chrono::Utc::now().timestamp();
            let mut credits = Vec::new();
            for credit in details.credits {
                if credit.status != "available" || credit.reset_type != "codex_rate_limits" {
                    continue;
                }
                let expires_at = match credit.expires_at {
                    Some(value) => match chrono::DateTime::parse_from_rfc3339(&value) {
                        Ok(value) => Some(value.timestamp()),
                        Err(_) => continue,
                    },
                    None => None,
                };
                if expires_at.is_none_or(|expiration| expiration > now) {
                    credits.push(BankedReset {
                        id: credit.id,
                        expires_at,
                    });
                }
            }
            result.available_resets = Some(details.available_count);
            result.resets = Some(credits);
        }
        Ok(())
    }
}

impl AccountBackend for ManagedBackend {
    async fn usage(&self, account: &ManagedAccount, model: Option<&str>) -> ManagedAccountUsage {
        let mut result = ManagedAccountUsage {
            account: self
                .store
                .read()
                .ok()
                .and_then(|config| {
                    config.accounts.into_iter().find(|entry| {
                        entry.alias == account.alias
                            && entry.user_id == account.user_id
                            && entry.workspace_id == account.workspace_id
                    })
                })
                .unwrap_or_else(|| account.clone()),
            pools: self
                .store
                .read()
                .map(|config| {
                    config
                        .pools
                        .into_iter()
                        .filter(|pool| pool.accounts.contains(&account.alias))
                        .map(|pool| pool.name)
                        .collect()
                })
                .unwrap_or_default(),
            model: model.map(str::to_owned),
            model_supported: None,
            ordinary_usage_allowed: None,
            windows: Vec::new(),
            available_resets: None,
            resets: None,
            checked_at: chrono::Utc::now().timestamp(),
            error: None,
        };
        if !matches!(
            tokio::time::timeout(
                Duration::from_secs(20),
                self.read_usage(account, model, &mut result)
            )
            .await,
            Ok(Ok(()))
        ) {
            // Backend errors may contain response bodies or credential-bearing URLs.
            result.error =
                Some("Authentication or usage lookup failed; availability is unknown".to_owned());
            result.ordinary_usage_allowed = None;
            result.model_supported = None;
            result.resets = None;
        }
        result
    }

    async fn redeem(
        &self,
        account: &ManagedAccount,
        key: &str,
        credit: &str,
    ) -> Result<ConsumeRateLimitResetCreditCode> {
        let result = tokio::time::timeout(Duration::from_secs(20), async {
            self.client(account)
                .await?
                .0
                .consume_rate_limit_reset_credit_by_id(key, credit)
                .await
        })
        .await;
        match result {
            Ok(Ok(response)) => Ok(response.code),
            Ok(Err(_)) | Err(_) => {
                bail!("Reset response is unknown; the persisted attempt will be reconciled")
            }
        }
    }
}
