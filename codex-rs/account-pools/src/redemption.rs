//! Durable, per-account reset transactions. A failed request never causes a new key.

use crate::AccountBackend;
use crate::AccountStore;
use crate::policy;
use crate::storage::read_json;
use crate::storage::write_json;
use anyhow::Result;
use anyhow::bail;
use codex_backend_client::ConsumeRateLimitResetCreditCode;
use codex_protocol::account_pool::ManagedAccount;
use codex_protocol::account_pool::ManagedAccountUsage;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Intent {
    key: String,
    credit: String,
    model: Option<String>,
    windows: Vec<(String, i64, Option<i64>)>,
    completed: bool,
    #[serde(default)]
    declined: bool,
    #[serde(default)]
    automatic: bool,
    observed_usable: bool,
}

fn windows(usage: &ManagedAccountUsage) -> Vec<(String, i64, Option<i64>)> {
    let mut windows: Vec<_> = usage
        .windows
        .iter()
        .filter(|window| window.limit_id == "codex")
        .map(|window| {
            (
                window.limit_id.clone(),
                window.window_minutes,
                window.resets_at,
            )
        })
        .collect();
    windows.sort();
    windows
}

#[derive(Clone, Copy)]
pub(crate) enum RedemptionMode {
    Automatic,
    Explicit,
}

pub(crate) async fn redeem(
    store: &AccountStore,
    backend: &impl AccountBackend,
    account: &ManagedAccount,
    model: Option<&str>,
    credit: &str,
    mode: RedemptionMode,
) -> Result<ManagedAccountUsage> {
    let path = store.credential_home(account).join("redemption.json");
    let _lock = crate::storage::lock(&path.with_extension("lock")).await?;
    let fresh = backend.usage(account, model).await;
    let now = chrono::Utc::now().timestamp();
    if !policy::known(&fresh, now) {
        bail!("Availability is unknown; no reset was sent");
    }
    let previous = read_json::<Intent>(&path)?;
    let mut intent = match previous.filter(|intent| !intent.declined) {
        Some(mut intent) if !intent.completed => {
            // Even if the selected credit has disappeared, reconcile with the same key.
            if policy::usable(&fresh, now) {
                intent.completed = true;
                intent.observed_usable = true;
                write_json(&path, &intent)?;
                return Ok(fresh);
            }
            intent
        }
        Some(mut intent) if !intent.observed_usable || intent.windows == windows(&fresh) => {
            if policy::usable(&fresh, now) {
                intent.observed_usable = true;
                write_json(&path, &intent)?;
                return Ok(fresh);
            }
            bail!("Previous reset is awaiting verified quota recovery");
        }
        Some(_) | None => {
            if policy::usable(&fresh, now) {
                return Ok(fresh);
            }
            if !fresh.resets.as_ref().is_some_and(|credits| {
                credits.iter().any(|item| {
                    item.id == credit && item.expires_at.is_none_or(|expiry| expiry > now)
                })
            }) {
                bail!("Reset credit is no longer available");
            }
            Intent {
                key: uuid::Uuid::new_v4().to_string(),
                credit: credit.to_owned(),
                model: model.map(str::to_owned),
                windows: windows(&fresh),
                completed: false,
                declined: false,
                automatic: matches!(mode, RedemptionMode::Automatic),
                observed_usable: false,
            }
        }
    };
    if matches!(mode, RedemptionMode::Automatic)
        && !policy::blocked(&fresh, policy::WindowKind::Weekly)
    {
        bail!(
            "Weekly capacity is available; a pending automatic reset will not be sent for short-window exhaustion"
        );
    }
    write_json(&path, &intent)?;
    let outcome = backend.redeem(account, &intent.key, &intent.credit).await?;
    intent.completed = true;
    intent.declined = matches!(
        outcome,
        ConsumeRateLimitResetCreditCode::NoCredit | ConsumeRateLimitResetCreditCode::NothingToReset
    );
    write_json(&path, &intent)?;
    let fresh = backend.usage(account, model).await;
    if policy::usable(&fresh, chrono::Utc::now().timestamp()) {
        intent.observed_usable = true;
        write_json(&path, &intent)?;
    }
    match outcome {
        ConsumeRateLimitResetCreditCode::Reset
        | ConsumeRateLimitResetCreditCode::AlreadyRedeemed => Ok(fresh),
        ConsumeRateLimitResetCreditCode::NothingToReset
        | ConsumeRateLimitResetCreditCode::NoCredit => {
            bail!("Backend declined reset; waiting for verified quota recovery")
        }
    }
}

pub(crate) async fn observe_usable(store: &AccountStore, account: &ManagedAccount) -> Result<()> {
    let path = store.credential_home(account).join("redemption.json");
    let _lock = crate::storage::lock(&path.with_extension("lock")).await?;
    if let Some(mut intent) = read_json::<Intent>(&path)? {
        intent.completed = true;
        intent.observed_usable = true;
        write_json(&path, &intent)?;
    }
    Ok(())
}

pub(crate) fn pending_credit(
    store: &AccountStore,
    account: &ManagedAccount,
) -> Result<Option<String>> {
    Ok(
        read_json::<Intent>(&store.credential_home(account).join("redemption.json"))?
            .filter(|intent| !intent.observed_usable && !intent.declined)
            .map(|intent| intent.credit),
    )
}
