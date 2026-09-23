use anyhow::Context;
use anyhow::Result;
use base64::Engine;
use codex_account_pools::AccountStore;
use codex_account_pools::PoolSession;
use codex_login::AuthConfig;
use codex_login::AuthCredentialsStoreMode;
use codex_protocol::account_pool::AccountPool;
use codex_protocol::account_pool::AccountPoolEvent;
use codex_protocol::account_pool::AccountSelection;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

enum PoolUpdate {
    Unchanged,
    AddAvailableMember,
}

async fn verify_weekly_recovery(update: PoolUpdate) -> Result<()> {
    // The backend may return one quota window in either primary or secondary position.
    for position in ["primary_window", "secondary_window"] {
        let server = MockServer::start().await;
        let home = tempfile::tempdir()?;
        let store = AccountStore::new(AuthConfig {
            codex_home: home.path().to_path_buf(),
            auth_credentials_store_mode: AuthCredentialsStoreMode::File,
            keyring_backend_kind: Default::default(),
            forced_login_method: None,
            forced_chatgpt_workspace_id: None,
            managed_auth_policy: Default::default(),
            chatgpt_base_url: Some(server.uri()),
            auth_route_config: codex_login::test_support::transport_default_auth_route_config(),
        });
        for alias in ["oa", "ob", "oc"] {
            let source = tempfile::tempdir()?;
            let claims = json!({
                "exp": 4_102_444_800i64,
                "https://api.openai.com/auth": {
                    "chatgpt_user_id": format!("user-{alias}"),
                    "chatgpt_account_id": format!("workspace-{alias}"),
                    "chatgpt_plan_type": "pro",
                },
            });
            let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&claims)?);
            let token = format!("e30.{payload}.c2ln");
            std::fs::write(
                source.path().join("auth.json"),
                serde_json::to_vec(&json!({
                    "auth_mode": "chatgpt",
                    "tokens": {
                        "id_token": token, "access_token": token,
                        "refresh_token": format!("refresh-{alias}"),
                        "account_id": format!("workspace-{alias}"),
                    },
                    "last_refresh": chrono::Utc::now(),
                }))?,
            )?;
            store.import(alias, source.path()).await?;
            let mut limit = json!({
                "allowed": alias == "oc", "limit_reached": alias != "oc",
                "primary_window": null, "secondary_window": null,
            });
            limit[position] = json!({
                "used_percent": if alias == "oc" { 0 } else { 100 },
                "limit_window_seconds": 604800,
                "reset_after_seconds": 3600,
                "reset_at": chrono::Utc::now().timestamp() + 3600,
            });
            Mock::given(method("GET"))
                .and(path("/api/codex/usage"))
                .and(header("chatgpt-account-id", format!("workspace-{alias}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "plan_type": "pro",
                    "user_id": format!("user-{alias}"),
                    "account_id": format!("workspace-{alias}"),
                    "rate_limit": limit,
                })))
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/api/codex/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "models": [{"slug": "gpt-6-astra"}],
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/codex/rate-limit-reset-credits"))
            .and(header("chatgpt-account-id", "workspace-oa"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "available_count": 1,
                "credits": [{
                    "id": "credit-oa", "reset_type": "codex_rate_limits",
                    "status": "available", "granted_at": "2026-09-10T00:00:00Z",
                    "expires_at": null,
                }],
            })))
            .mount(&server)
            .await;
        store
            .put_pool(
                AccountPool {
                    name: "nano".to_owned(),
                    accounts: match update {
                        PoolUpdate::Unchanged => {
                            vec!["oa".to_owned(), "ob".to_owned(), "oc".to_owned()]
                        }
                        PoolUpdate::AddAvailableMember => vec!["oa".to_owned(), "ob".to_owned()],
                    },
                    redeem_weekly_resets: true,
                },
                /*create*/ true,
            )
            .await?;
        let pool = PoolSession::open(
            store.clone(),
            Some(AccountSelection::Pool("nano".to_owned())),
            /*resume_id*/ None,
        )
        .await?
        .context("configured pool")?;
        if matches!(update, PoolUpdate::AddAvailableMember) {
            store
                .put_pool(
                    AccountPool {
                        name: "nano".to_owned(),
                        accounts: ["oa", "ob", "oc"].map(str::to_owned).to_vec(),
                        redeem_weekly_resets: true,
                    },
                    /*create*/ false,
                )
                .await?;
        }
        let cancel = CancellationToken::new();
        let mut events = Vec::new();
        let recovery = tokio::time::timeout(
            Duration::from_secs(/*secs*/ 10),
            pool.recover("gpt-6-astra", &cancel, |event| {
                if matches!(event, AccountPoolEvent::Waiting { .. }) {
                    cancel.cancel();
                }
                events.push(event);
            }),
        )
        .await?;
        assert_eq!(
            events,
            vec![AccountPoolEvent::Switched {
                account: "oc".to_owned(),
                previous_account: "oa".to_owned(),
            }],
            "weekly quota in {position} must allow recovery"
        );
        recovery?;
        assert_eq!(
            (
                pool.selected_account().await.alias,
                pool.auth_manager()
                    .auth()
                    .await
                    .context("selected account has auth")?
                    .get_account_id(),
                pool.is_waiting(),
            ),
            ("oc".to_owned(), Some("workspace-oc".to_owned()), false)
        );
        assert!(
            server
                .received_requests()
                .await
                .context("request recording enabled")?
                .iter()
                .all(|request| request.method == "GET")
        );
    }
    Ok(())
}

#[tokio::test]
async fn weekly_only_pool_switches_to_available_account_without_spending_a_reset() -> Result<()> {
    verify_weekly_recovery(PoolUpdate::Unchanged).await
}

#[tokio::test]
async fn recovery_reloads_members_added_after_the_session_opened() -> Result<()> {
    verify_weekly_recovery(PoolUpdate::AddAvailableMember).await
}
