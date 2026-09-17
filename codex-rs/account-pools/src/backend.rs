use crate::AccountStore;
use crate::observations;
use crate::observations::StoredModelSlugs;
use crate::observations::StoredUsage;
use crate::policy::WindowKind;
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

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;

/// The coordinator's network boundary. Implementations must return identity-checked observations
/// that are fresh enough for recovery decisions and must preserve the caller's idempotency key on
/// every redemption attempt.
pub(crate) trait AccountBackend: Send + Sync {
    fn usage(
        &self,
        account: &ManagedAccount,
        model: Option<&str>,
    ) -> impl Future<Output = ManagedAccountUsage> + Send;
    /// A read that bypasses observations shared with other sessions: after a redemption, or
    /// before activating an account.
    fn fresh_usage(
        &self,
        account: &ManagedAccount,
        model: Option<&str>,
    ) -> impl Future<Output = ManagedAccountUsage> + Send {
        self.usage(account, model)
    }
    fn redeem(
        &self,
        account: &ManagedAccount,
        key: &str,
        credit: &str,
    ) -> impl Future<Output = Result<ConsumeRateLimitResetCreditCode>> + Send;
}

#[derive(Clone, Copy)]
enum UsageDetails<'a> {
    /// Quota windows only, for display.
    Quota,
    /// Quota, model support, and banked resets once the weekly window is exhausted.
    Recovery { model: Option<&'a str> },
    /// Quota, model support and banked resets, for explicit reports and redemptions.
    Full { model: Option<&'a str> },
}

impl<'a> UsageDetails<'a> {
    fn model(self) -> Option<&'a str> {
        match self {
            UsageDetails::Quota => None,
            UsageDetails::Recovery { model } | UsageDetails::Full { model } => model,
        }
    }

    fn needs_credit_details(self, usage: &ManagedAccountUsage) -> bool {
        match self {
            UsageDetails::Quota => false,
            UsageDetails::Recovery { .. } => crate::policy::blocked(usage, WindowKind::Weekly),
            UsageDetails::Full { .. } => true,
        }
    }
}

#[derive(Clone)]
pub struct ManagedBackend {
    store: AccountStore,
}

impl ManagedBackend {
    pub fn new(store: AccountStore) -> Self {
        Self { store }
    }

    /// An explicit usage report: fresh quota, model support and banked reset details.
    pub async fn usage(
        &self,
        account: &ManagedAccount,
        model: Option<&str>,
    ) -> ManagedAccountUsage {
        self.observe_usage(account, UsageDetails::Full { model }, Duration::ZERO)
            .await
    }

    /// Quota windows for display. Observations up to `max_age` old, including the windows that
    /// inference responses report, stand in for a request.
    pub async fn quota(&self, account: &ManagedAccount, max_age: Duration) -> ManagedAccountUsage {
        self.observe_usage(account, UsageDetails::Quota, max_age)
            .await
    }

    /// Recovery evidence, shared across sessions for [`observations::RECOVERY_MAX_AGE`].
    pub async fn recovery_usage(
        &self,
        account: &ManagedAccount,
        model: Option<&str>,
    ) -> ManagedAccountUsage {
        self.observe_usage(
            account,
            UsageDetails::Recovery { model },
            observations::RECOVERY_MAX_AGE,
        )
        .await
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

    /// Whether `model` is in the account's catalog, from the shared catalog when it is recent.
    async fn model_support(
        &self,
        client: Option<&Client>,
        account: &ManagedAccount,
        model: &str,
        now: i64,
    ) -> Result<bool> {
        // The catalog URL carries the workspace version, so cached entries are keyed by it too.
        let client_version = env!("CARGO_PKG_VERSION");
        if let Some(supported) = observations::cached_model_support(
            observations::load_model_slugs(&self.store, account).as_ref(),
            model,
            client_version,
            now,
        ) {
            return Ok(supported);
        }
        let owned;
        let client = match client {
            Some(client) => client,
            None => {
                owned = self.client(account).await?.0;
                &owned
            }
        };
        let slugs = client.get_account_model_slugs().await?;
        let supported = slugs.iter().any(|slug| slug == model);
        observations::store_model_slugs(
            &self.store,
            account,
            &StoredModelSlugs {
                fetched_at: now,
                client_version: client_version.to_owned(),
                slugs,
            },
        );
        Ok(supported)
    }

    async fn read_usage(
        &self,
        account: &ManagedAccount,
        details: UsageDetails<'_>,
        result: &mut ManagedAccountUsage,
    ) -> Result<Client> {
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
        if let Some(model) = details.model() {
            result.model_supported = Some(
                self.model_support(Some(&client), account, model, result.checked_at)
                    .await?,
            );
        }
        Ok(client)
    }

    fn base_usage(&self, account: &ManagedAccount, model: Option<&str>) -> ManagedAccountUsage {
        ManagedAccountUsage {
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
        }
    }

    /// Serve a stored observation when it is recent enough and carries the requested details.
    async fn cached(
        &self,
        account: &ManagedAccount,
        stored: &StoredUsage,
        details: UsageDetails<'_>,
        max_age: Duration,
        now: i64,
    ) -> Option<ManagedAccountUsage> {
        if max_age.is_zero() {
            return None;
        }
        let observation = match details {
            UsageDetails::Quota => observations::display_usage(stored)?,
            UsageDetails::Recovery { .. } | UsageDetails::Full { .. } => stored.usage.clone()?,
        };
        if !observations::fresh_enough(&observation, now, max_age)
            || (details.needs_credit_details(&observation) && !stored.usage_has_credit_details)
        {
            return None;
        }
        let mut result = self.base_usage(account, details.model());
        if let Some(model) = details.model() {
            // The account lock is held here; a catalog fetch must not stall other readers.
            let supported = tokio::time::timeout(
                Duration::from_secs(/*secs*/ 20),
                self.model_support(None, account, model, now),
            )
            .await
            .ok()?
            .ok()?;
            result.model_supported = Some(supported);
        }
        if observation.account.plan.is_some() {
            result.account.plan = observation.account.plan;
        }
        result.ordinary_usage_allowed = observation.ordinary_usage_allowed;
        result.windows = observation.windows;
        result.available_resets = observation.available_resets;
        result.resets = observation.resets;
        result.checked_at = observation.checked_at;
        Some(result)
    }

    async fn observe_usage(
        &self,
        account: &ManagedAccount,
        details: UsageDetails<'_>,
        max_age: Duration,
    ) -> ManagedAccountUsage {
        let mut result = self.base_usage(account, details.model());
        // Concurrent readers of one account wait for the first read instead of repeating it.
        let _guard =
            observations::lock_usage(&self.store, account, observations::READ_LOCK_WAIT).await;
        let now = chrono::Utc::now().timestamp();
        result.checked_at = now;
        let stored = observations::load_usage(&self.store, account);
        // A shared read may lag a rejection this session saw; a fresh read is trusted as is.
        let rejected = !max_age.is_zero() && stored.rejected(now);
        if let Some(mut cached) = self.cached(account, &stored, details, max_age, now).await {
            if rejected && !matches!(details, UsageDetails::Quota) {
                cached.ordinary_usage_allowed = Some(false);
            }
            tracing::debug!(alias = %account.alias, checked_at = cached.checked_at, "Serving stored account usage");
            return cached;
        }
        let unknown = |mut result: ManagedAccountUsage| {
            // Backend errors may contain response bodies or credential-bearing URLs.
            result.error =
                Some("Authentication or usage lookup failed; availability is unknown".to_owned());
            result.ordinary_usage_allowed = None;
            result.model_supported = None;
            result.resets = None;
            result
        };
        if !max_age.is_zero()
            && stored.error_at.is_some_and(|error_at| {
                (0..observations::ERROR_MAX_AGE.as_secs() as i64)
                    .contains(&now.saturating_sub(error_at))
            })
        {
            return unknown(result);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(/*secs*/ 20);
        let client =
            match tokio::time::timeout_at(deadline, self.read_usage(account, details, &mut result))
                .await
            {
                Ok(Ok(client)) => client,
                Ok(Err(_)) | Err(_) => {
                    observations::store_usage(
                        &self.store,
                        account,
                        &StoredUsage {
                            error_at: Some(now),
                            ..stored
                        },
                    );
                    return unknown(result);
                }
            };
        let mut credit_details = false;
        // Optional reset details share the deadline but cannot invalidate verified quota.
        if details.needs_credit_details(&result)
            && let Ok(Ok(details)) =
                tokio::time::timeout_at(deadline, client.list_rate_limit_reset_credits()).await
        {
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
            credit_details = true;
        }
        let mut observation = result.clone();
        observation.model = None;
        observation.model_supported = None;
        if !credit_details
            && stored.usage_has_credit_details
            && let Some(previous) = &stored.usage
            && previous.available_resets == observation.available_resets
        {
            // A read that did not ask for banked reset details keeps the ones already known.
            observation.resets = previous.resets.clone();
            credit_details = true;
        }
        // A fresh read that shows the account usable ends any rejection hold, e.g. after a reset.
        let rejected_until = if max_age.is_zero() && crate::policy::usable(&result, now) {
            None
        } else {
            stored.rejected_until
        };
        observations::store_usage(
            &self.store,
            account,
            &StoredUsage {
                usage: Some(observation),
                usage_has_credit_details: credit_details,
                response: stored.response,
                error_at: None,
                rejected_until,
            },
        );
        if rejected && !matches!(details, UsageDetails::Quota) {
            result.ordinary_usage_allowed = Some(false);
        }
        result
    }
}

impl AccountBackend for ManagedBackend {
    async fn usage(&self, account: &ManagedAccount, model: Option<&str>) -> ManagedAccountUsage {
        self.recovery_usage(account, model).await
    }

    async fn fresh_usage(
        &self,
        account: &ManagedAccount,
        model: Option<&str>,
    ) -> ManagedAccountUsage {
        self.observe_usage(account, UsageDetails::Recovery { model }, Duration::ZERO)
            .await
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
