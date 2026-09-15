//! Pool preferences affect new sessions only. Existing threads retain their own account.

use super::*;
use crate::policy::known;
use crate::storage::lock;
use std::collections::BTreeMap;

type Preferences = BTreeMap<String, ManagedAccount>;

pub(super) fn preferred_account(
    store: &AccountStore,
    selection: &AccountSelection,
    accounts: &[ManagedAccount],
) -> Option<usize> {
    let AccountSelection::Pool(pool) = selection else {
        return None;
    };
    // This optional hint must never prevent startup if absent or externally damaged.
    let preferences: Preferences = read_json(&store.root().join("pool-state.json"))
        .ok()
        .flatten()?;
    let saved = preferences.get(pool)?;
    accounts.iter().position(|account| {
        account.alias == saved.alias
            && account.user_id == saved.user_id
            && account.workspace_id == saved.workspace_id
    })
}

impl PoolSession {
    /// Remember successful inference without changing any running session or configured default.
    pub async fn record_success(&self) -> Result<()> {
        let AccountSelection::Pool(pool) = &self.selection else {
            return Ok(());
        };
        let current = self.selected_account().await;
        let _guard = tokio::time::timeout(
            Duration::from_secs(/*secs*/ 5),
            lock(&self.store.root().join("config.lock")),
        )
        .await??;
        let config = self.store.read()?;
        if !PoolMembership::from_config(&config, &self.selection)?
            .accounts
            .iter()
            .any(|account| {
                account.alias == current.alias
                    && account.user_id == current.user_id
                    && account.workspace_id == current.workspace_id
            })
        {
            return Ok(());
        }
        let path = self.store.root().join("pool-state.json");
        let mut preferences: Preferences = read_json(&path)?.unwrap_or_default();
        if preferences.get(pool) == Some(&current) {
            return Ok(());
        }
        preferences.retain(|name, _| config.pools.iter().any(|pool| &pool.name == name));
        preferences.insert(pool.clone(), current);
        write_json(&path, &preferences)
    }

    /// Check quota once before this session's first inference. Unknown usage is not rejection.
    pub async fn needs_recovery(&self, model: &str, cancel: &CancellationToken) -> Result<bool> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("Account preflight cancelled"),
            result = async {
                if self.is_waiting() {
                    self.needs_preflight.store(/*val*/ false, Ordering::Release);
                    return Ok(true);
                }
                if !self.needs_preflight.load(Ordering::Acquire) {
                    return Ok(false);
                }
                let current = self.selected_account().await;
                let usage = ManagedBackend::new(self.store.clone()).usage(&current, Some(model)).await;
                let now = chrono::Utc::now().timestamp();
                let blocked = known(&usage, now) && !usable(&usage, now);
                self.needs_preflight.store(blocked, Ordering::Release);
                Ok(blocked)
            } => result,
        }
    }
}
