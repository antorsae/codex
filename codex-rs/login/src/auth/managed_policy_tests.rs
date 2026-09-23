use super::*;
use base64::Engine;
use codex_http_client::DestinationPolicy;
use codex_http_client::NetworkPolicyController;
use pretty_assertions::assert_eq;
use serde_json::json;

struct AccountAuth(CodexAuth);

impl ExternalAuth for AccountAuth {
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth> {
        let auth = self.0.clone();
        Box::pin(async move { Ok(auth) })
    }

    fn refresh(&self, _context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth> {
        self.resolve()
    }
}

fn chatgpt_auth(user: &str, workspace: &str) -> CodexAuth {
    let claims = json!({
        "jti": user,
        "https://api.openai.com/auth": {"chatgpt_user_id": user},
    });
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
    CodexAuth::from_external_chatgpt_tokens(
        &format!("header.{payload}.signature"),
        workspace,
        /*chatgpt_plan_type*/ None,
    )
    .unwrap()
}

#[tokio::test]
async fn managed_account_switches_keep_the_shared_application_policy() {
    let home = tempfile::tempdir().unwrap();
    let controller = NetworkPolicyController::default();
    let policy = controller.policy();
    let manager = AuthManager::managed_from_auth_config(AuthConfig {
        codex_home: home.path().to_path_buf(),
        auth_credentials_store_mode: AuthCredentialsStoreMode::File,
        keyring_backend_kind: AuthKeyringBackendKind::default(),
        forced_login_method: None,
        chatgpt_base_url: None,
        forced_chatgpt_workspace_id: None,
        managed_auth_policy: ManagedAuthPolicy::default(),
        auth_route_config: AuthRouteConfig::from_http_client_factory(
            crate::test_support::transport_default_auth_route_config()
                .http_client_factory()
                .clone()
                .with_network_policy(policy.clone()),
        ),
    })
    .await;
    assert!(controller.publish(policy.revision(), DestinationPolicy::Unrestricted));
    let endpoint = "https://example.com/".parse().unwrap();

    // A pooled session installs its first account, then quota recovery switches to another.
    let mut usable = Vec::new();
    for (user, workspace) in [("user-a", "workspace-a"), ("user-b", "workspace-b")] {
        let factory = manager.http_client_factory();
        manager
            .set_external_auth(Arc::new(AccountAuth(chatgpt_auth(user, workspace))))
            .await
            .unwrap();
        usable.push((
            factory.network_policy().acquire(&endpoint).is_ok(),
            policy.acquire(&endpoint).is_ok(),
        ));
    }
    assert_eq!(usable, vec![(true, true), (true, true)]);
}
