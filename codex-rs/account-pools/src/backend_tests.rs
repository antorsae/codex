use super::*;
use crate::tests::account;
use crate::tests::populated_store;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Notify;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[tokio::test]
async fn quota_observation_fetches_only_usage_and_retains_reported_reset_count() {
    let (_home, mut store) = populated_store().await;
    let server = wiremock::MockServer::start().await;
    store.auth_config.chatgpt_base_url = Some(server.uri());
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_json(json!({
            "account_id": "workspace-a", "user_id": "user-a", "plan_type": "pro",
            "rate_limit": {"allowed": true, "limit_reached": false,
                "primary_window": {"used_percent": 10, "limit_window_seconds": 604800,
                    "reset_after_seconds": 3600, "reset_at": 2_000_000_000}},
            "rate_limit_reset_credits": {"available_count": 2}
        })))
        .expect(/*r*/ 1)
        .mount(&server)
        .await;
    for endpoint in ["/api/codex/models", "/api/codex/rate-limit-reset-credits"] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(/*s*/ 503))
            .expect(/*r*/ 0)
            .mount(&server)
            .await;
    }

    let actual = ManagedBackend::new(store).quota(&account("a")).await;
    assert_eq!(
        actual,
        ManagedAccountUsage {
            account: account("a"),
            pools: vec!["work".to_owned()],
            model: None,
            model_supported: None,
            ordinary_usage_allowed: Some(true),
            windows: vec![AccountQuotaWindow {
                limit_id: "codex".to_owned(),
                model: None,
                remaining_percent: 90.0,
                window_minutes: 10080,
                resets_at: Some(2_000_000_000),
            }],
            available_resets: Some(2),
            resets: None,
            checked_at: actual.checked_at,
            error: None,
        }
    );
}

#[tokio::test]
async fn optional_reset_timeout_preserves_usage_while_model_timeout_fails_closed() {
    for stalled_path in ["/api/codex/rate-limit-reset-credits", "/api/codex/models"] {
        let (_home, mut store) = populated_store().await;
        let server = wiremock::MockServer::start().await;
        store.auth_config.chatgpt_base_url = Some(server.uri());
        Mock::given(method("GET"))
            .and(path("/api/codex/usage"))
            .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_json(json!({
                "account_id": "workspace-a", "user_id": "user-a", "plan_type": "pro",
                "rate_limit": {"allowed": true, "limit_reached": false,
                    "primary_window": {"used_percent": 10, "limit_window_seconds": 18000,
                        "reset_after_seconds": 3600, "reset_at": 2_000_000_000},
                    "secondary_window": {"used_percent": 20, "limit_window_seconds": 604800,
                        "reset_after_seconds": 3600, "reset_at": 2_000_000_000}},
                "rate_limit_reset_credits": {"available_count": 2}
            })))
            .expect(/*r*/ 1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/codex/models"))
            .respond_with(
                ResponseTemplate::new(/*s*/ 200)
                    .set_body_json(json!({"models": [{"slug": "model"}]})),
            )
            .mount(&server)
            .await;
        let requested = Arc::new(Notify::new());
        let notify = Arc::clone(&requested);
        Mock::given(method("GET"))
            .and(path(stalled_path))
            .respond_with(move |_: &wiremock::Request| {
                notify.notify_one();
                ResponseTemplate::new(/*s*/ 200).set_delay(Duration::from_secs(/*secs*/ 60))
            })
            .with_priority(/*p*/ 1)
            .expect(/*r*/ 1)
            .mount(&server)
            .await;

        let read = tokio::spawn(async move {
            ManagedBackend::new(store)
                .usage(&account("a"), Some("model"))
                .await
        });
        tokio::time::timeout(Duration::from_secs(/*secs*/ 5), requested.notified())
            .await
            .expect("required reads must reach the delayed endpoint");
        // Pause only after real HTTP requests arrive, avoiding timer races with socket setup.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(/*secs*/ 20)).await;
        let actual = read.await.unwrap();
        tokio::time::resume();
        let required_read_failed = stalled_path == "/api/codex/models";
        assert_eq!(
            actual,
            ManagedAccountUsage {
                account: account("a"),
                pools: vec!["work".to_owned()],
                model: Some("model".to_owned()),
                model_supported: (!required_read_failed).then_some(/*t*/ true),
                ordinary_usage_allowed: (!required_read_failed).then_some(/*t*/ true),
                windows: [(300, 90.0), (10080, 80.0)]
                    .into_iter()
                    .map(|(window_minutes, remaining_percent)| AccountQuotaWindow {
                        limit_id: "codex".to_owned(),
                        model: None,
                        remaining_percent,
                        window_minutes,
                        resets_at: Some(2_000_000_000),
                    })
                    .collect(),
                available_resets: Some(2),
                resets: None,
                checked_at: actual.checked_at,
                error: required_read_failed.then(|| {
                    "Authentication or usage lookup failed; availability is unknown".to_owned()
                }),
            }
        );
    }
}
