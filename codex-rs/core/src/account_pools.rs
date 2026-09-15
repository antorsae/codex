//! Thin session integration for account selection and quota recovery.

use crate::client::ModelClientSession;
use crate::config::Config;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_account_pools::AccountStore;
use codex_account_pools::PoolSession;
use codex_login::AuthManager;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::protocol::EventMsg;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
#[path = "account_pools_tests.rs"]
mod tests;

pub async fn initialize(
    config: &Config,
    legacy: &AuthManager,
    resume_id: Option<&str>,
) -> anyhow::Result<Option<Arc<PoolSession>>> {
    let resume_id = resume_id.or(config.account_selection_source_thread_id.as_deref());
    let store = AccountStore::from_config(config);
    let saved = resume_id
        .map(|id| PoolSession::saved_selection(&store, id))
        .transpose()?
        .flatten();
    let selection = config.account_selection.clone().or(saved);
    // Explicit and saved selections take precedence over legacy authentication.
    let compatible = config.model_provider.is_openai()
        && codex_model_provider::provider_uses_first_party_auth_path(&config.model_provider)
        && (selection.is_some()
            || legacy
                .auth_cached()
                .is_none_or(|auth| matches!(auth, codex_login::CodexAuth::Chatgpt(_))));
    if !compatible {
        if selection.is_some() {
            anyhow::bail!(
                "Named accounts require locally managed ChatGPT authentication with the OpenAI provider"
            );
        }
        return Ok(None);
    }
    PoolSession::open(store, selection, resume_id).await
}

pub(crate) async fn recover(
    sess: &Session,
    turn: &TurnContext,
    model: &str,
    client_session: &mut ModelClientSession,
    cancellation: &CancellationToken,
) -> Result<bool> {
    let Some(pool) = sess
        .services
        .thread_extension_data
        .get::<Arc<PoolSession>>()
    else {
        return Ok(false);
    };
    // Drop account-bound sockets before awaiting anything that can switch or be cancelled.
    client_session.invalidate_account_transport();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let recovery = pool.recover(model, cancellation, |event| {
        let _ = tx.send(event);
    });
    tokio::pin!(recovery);
    loop {
        tokio::select! {
            biased;
            event = rx.recv() => if let Some(event) = event {
                sess.send_event(turn, EventMsg::AccountPool(event)).await;
            },
            result = &mut recovery => {
                while let Ok(event) = rx.try_recv() { sess.send_event(turn, EventMsg::AccountPool(event)).await; }
                if cancellation.is_cancelled() { return Err(CodexErr::TurnAborted); }
                result.map_err(|error| CodexErr::Fatal(error.to_string()))?;
                sess.send_event(turn, EventMsg::AccountPool(codex_protocol::account_pool::AccountPoolEvent::Selected { account: pool.selected_account().await })).await;
                return Ok(true);
            }
        }
    }
}

/// A restarted waiting thread resumes monitoring only when explicitly asked to run.
pub(crate) async fn before_request(
    sess: &Session,
    turn: &TurnContext,
    model: &str,
    client: &mut ModelClientSession,
    cancellation: &CancellationToken,
) -> Result<()> {
    if let Some(pool) = sess
        .services
        .thread_extension_data
        .get::<Arc<PoolSession>>()
    {
        let needs_recovery = pool
            .needs_recovery(model, cancellation)
            .await
            .map_err(|error| {
                if cancellation.is_cancelled() {
                    CodexErr::TurnAborted
                } else {
                    CodexErr::Fatal(error.to_string())
                }
            })?;
        if needs_recovery {
            recover(sess, turn, model, client, cancellation).await?;
        }
        sess.send_event(
            turn,
            EventMsg::AccountPool(codex_protocol::account_pool::AccountPoolEvent::Selected {
                account: pool.selected_account().await,
            }),
        )
        .await;
    }
    Ok(())
}

pub(crate) async fn record_success(sess: &Session) {
    if let Some(pool) = sess
        .services
        .thread_extension_data
        .get::<Arc<PoolSession>>()
        && let Err(error) = pool.record_success().await
    {
        tracing::warn!(%error, "Could not remember successful pool account");
    }
}

/// Validate the selected credential authority without logging out an unrelated legacy login.
pub async fn enforce_selection_restrictions(
    config: &Config,
    resume_id: Option<&str>,
) -> std::io::Result<()> {
    let legacy = AuthManager::shared_from_config(config, /*enable_codex_api_key_env*/ true)
        .await
        .map_err(std::io::Error::other)?;
    if initialize(config, &legacy, resume_id)
        .await
        .map_err(std::io::Error::other)?
        .is_none()
    {
        codex_login::enforce_login_restrictions(&config.auth_config()).await?;
    }
    Ok(())
}
