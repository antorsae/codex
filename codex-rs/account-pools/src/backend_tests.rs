use super::*;
use crate::tests::account;
use crate::tests::populated_store;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Notify;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn usage_body(alias: &str, weekly_used: u32) -> serde_json::Value {
    json!({
        "account_id": format!("workspace-{alias}"), "user_id": format!("user-{alias}"), "plan_type": "pro",
        "rate_limit": {"allowed": weekly_used < 100, "limit_reached": weekly_used >= 100,
            "primary_window": {"used_percent": 10, "limit_window_seconds": 18000,
                "reset_after_seconds": 3600, "reset_at": 2_000_000_000},
            "secondary_window": {"used_percent": weekly_used, "limit_window_seconds": 604800,
                "reset_after_seconds": 3600, "reset_at": 2_000_000_000}},
        "rate_limit_reset_credits": {"available_count": 1}
    })
}

async fn mount_catalog_and_credits(server: &wiremock::MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/codex/models"))
        .respond_with(
            ResponseTemplate::new(/*s*/ 200).set_body_json(json!({"models": [{"slug": "model"}]})),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/codex/rate-limit-reset-credits"))
        .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_json(json!({
            "available_count": 1,
            "credits": [{"id": "credit-a", "reset_type": "codex_rate_limits", "status": "available",
                "granted_at": "2026-09-10T00:00:00Z", "expires_at": null}],
        })))
        .mount(server)
        .await;
}

async fn requests(server: &wiremock::MockServer, endpoint: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.url.path() == endpoint)
        .count()
}

fn weekly_snapshot(used_percent: f64) -> codex_protocol::protocol::RateLimitSnapshot {
    codex_protocol::protocol::RateLimitSnapshot {
        limit_id: Some("codex".to_owned()),
        limit_name: None,
        normal_model_slug: None,
        primary: Some(codex_protocol::protocol::RateLimitWindow {
            used_percent: 10.0,
            window_minutes: Some(300),
            resets_at: Some(2_000_000_000),
        }),
        secondary: Some(codex_protocol::protocol::RateLimitWindow {
            used_percent,
            window_minutes: Some(10_080),
            resets_at: Some(2_000_000_000),
        }),
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    }
}

fn weekly_remaining(usage: &ManagedAccountUsage) -> Option<f64> {
    usage
        .windows
        .iter()
        .find(|window| window.limit_id == "codex" && window.window_minutes == 10_080)
        .map(|window| window.remaining_percent)
}

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

    let actual = ManagedBackend::new(store)
        .quota(&account("a"), Duration::ZERO)
        .await;
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

#[tokio::test]
async fn display_reads_share_one_request_and_accept_response_windows() {
    let (_home, mut store) = populated_store().await;
    let server = wiremock::MockServer::start().await;
    store.auth_config.chatgpt_base_url = Some(server.uri());
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_json(usage_body("a", 20)))
        .mount(&server)
        .await;
    let backend = ManagedBackend::new(store.clone());
    let first = backend.quota(&account("a"), crate::DISPLAY_MAX_AGE).await;
    let second = backend.quota(&account("a"), crate::DISPLAY_MAX_AGE).await;
    assert_eq!(first.error, None);
    assert_eq!(first, second);
    assert_eq!(weekly_remaining(&first), Some(80.0));
    assert_eq!(requests(&server, "/api/codex/usage").await, 1);

    // A later response overlays an earlier read.
    let mut stored = crate::observations::load_usage(&store, &account("a"));
    stored.usage.as_mut().unwrap().checked_at -= 10;
    crate::observations::store_usage(&store, &account("a"), &stored);
    crate::observations::record_response(
        &store,
        &account("a"),
        &weekly_snapshot(45.0),
        first.checked_at,
    )
    .await;
    let overlaid = backend.quota(&account("a"), crate::DISPLAY_MAX_AGE).await;
    assert_eq!(overlaid.checked_at, first.checked_at);
    assert_eq!(weekly_remaining(&overlaid), Some(55.0));
    assert_eq!(overlaid.ordinary_usage_allowed, Some(true));
    assert_eq!(requests(&server, "/api/codex/usage").await, 1);

    // A zero age always reads.
    let fresh = backend.quota(&account("a"), Duration::ZERO).await;
    assert_eq!(weekly_remaining(&fresh), Some(80.0));
    assert_eq!(requests(&server, "/api/codex/usage").await, 2);
}

#[tokio::test]
async fn recovery_reads_reuse_recent_reads_and_catalogs_but_not_response_windows() {
    let (_home, mut store) = populated_store().await;
    let server = wiremock::MockServer::start().await;
    store.auth_config.chatgpt_base_url = Some(server.uri());
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_json(usage_body("a", 20)))
        .mount(&server)
        .await;
    mount_catalog_and_credits(&server).await;
    let backend = ManagedBackend::new(store.clone());
    let first = backend.recovery_usage(&account("a"), Some("model")).await;
    assert_eq!(first.model_supported, Some(true));
    assert_eq!(first.ordinary_usage_allowed, Some(true));
    // Credit details are only needed once the weekly window is exhausted.
    assert_eq!(first.resets, None);
    assert_eq!(first.available_resets, Some(1));

    let mut stored = crate::observations::load_usage(&store, &account("a"));
    stored.usage.as_mut().unwrap().checked_at -= 10;
    crate::observations::store_usage(&store, &account("a"), &stored);
    crate::observations::record_response(
        &store,
        &account("a"),
        &weekly_snapshot(100.0),
        first.checked_at,
    )
    .await;
    let second = backend.recovery_usage(&account("a"), Some("model")).await;
    assert_eq!(
        second,
        ManagedAccountUsage {
            checked_at: first.checked_at - 10,
            ..first.clone()
        }
    );
    let display = backend.quota(&account("a"), crate::DISPLAY_MAX_AGE).await;
    assert_eq!(weekly_remaining(&display), Some(0.0));
    let other_model = backend.recovery_usage(&account("a"), Some("missing")).await;
    assert_eq!(other_model.model_supported, Some(false));
    assert_eq!(
        (
            requests(&server, "/api/codex/usage").await,
            requests(&server, "/api/codex/models").await,
            requests(&server, "/api/codex/rate-limit-reset-credits").await,
        ),
        (1, 1, 0)
    );

    let fresh =
        <ManagedBackend as AccountBackend>::fresh_usage(&backend, &account("a"), Some("model"))
            .await;
    assert_eq!(fresh.model_supported, Some(true));
    assert_eq!(requests(&server, "/api/codex/usage").await, 2);
    assert_eq!(requests(&server, "/api/codex/models").await, 1);
}

#[tokio::test]
async fn exhausted_accounts_keep_credit_details_and_are_re_read_hourly() {
    let (_home, mut store) = populated_store().await;
    let server = wiremock::MockServer::start().await;
    store.auth_config.chatgpt_base_url = Some(server.uri());
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .and(header("chatgpt-account-id", "workspace-a"))
        .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_json(usage_body("a", 100)))
        .mount(&server)
        .await;
    mount_catalog_and_credits(&server).await;
    let backend = ManagedBackend::new(store.clone());
    let first = backend.recovery_usage(&account("a"), Some("model")).await;
    assert_eq!(first.ordinary_usage_allowed, Some(false));
    assert_eq!(
        first.resets,
        Some(vec![BankedReset {
            id: "credit-a".to_owned(),
            expires_at: None
        }])
    );
    assert_eq!(
        requests(&server, "/api/codex/rate-limit-reset-credits").await,
        1
    );

    let age = |seconds: i64| {
        let mut stored = crate::observations::load_usage(&store, &account("a"));
        stored.usage.as_mut().unwrap().checked_at -= seconds;
        crate::observations::store_usage(&store, &account("a"), &stored);
    };
    age(/*seconds*/ 1_800);
    let aged = backend.recovery_usage(&account("a"), Some("model")).await;
    assert_eq!(aged.checked_at, first.checked_at - 1_800);
    assert_eq!(aged.resets, first.resets);
    assert!(crate::policy::known(&aged, chrono::Utc::now().timestamp()));
    assert!(!crate::policy::usable(
        &aged,
        chrono::Utc::now().timestamp()
    ));
    assert_eq!(requests(&server, "/api/codex/usage").await, 1);

    age(/*seconds*/ 2_400);
    let reread = backend.recovery_usage(&account("a"), Some("model")).await;
    assert!(reread.checked_at >= first.checked_at);
    assert_eq!(requests(&server, "/api/codex/usage").await, 2);
    assert_eq!(requests(&server, "/api/codex/models").await, 1);
}

#[tokio::test]
async fn failed_reads_back_off_but_explicit_reads_retry_immediately() {
    let (_home, mut store) = populated_store().await;
    let server = wiremock::MockServer::start().await;
    store.auth_config.chatgpt_base_url = Some(server.uri());
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .respond_with(ResponseTemplate::new(/*s*/ 503))
        .mount(&server)
        .await;
    let backend = ManagedBackend::new(store.clone());
    let first = backend.quota(&account("a"), crate::DISPLAY_MAX_AGE).await;
    let second = backend.quota(&account("a"), crate::DISPLAY_MAX_AGE).await;
    assert!(first.error.is_some());
    assert!(second.error.is_some());
    assert_eq!(requests(&server, "/api/codex/usage").await, 1);
    let explicit = backend.usage(&account("a"), None).await;
    assert!(explicit.error.is_some());
    assert_eq!(requests(&server, "/api/codex/usage").await, 2);
}

#[tokio::test]
async fn quota_rejections_hold_shared_reads_but_not_fresh_ones() {
    let (_home, mut store) = populated_store().await;
    let server = wiremock::MockServer::start().await;
    store.auth_config.chatgpt_base_url = Some(server.uri());
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_json(usage_body("a", 20)))
        .mount(&server)
        .await;
    mount_catalog_and_credits(&server).await;
    let backend = ManagedBackend::new(store.clone());
    let first = backend.recovery_usage(&account("a"), Some("model")).await;
    assert_eq!(first.ordinary_usage_allowed, Some(true));

    let now = chrono::Utc::now().timestamp();
    crate::observations::record_rejection(&store, &account("a"), now).await;
    // The rejection discards the shared read, and a lagging usage payload is not trusted.
    let held = backend.recovery_usage(&account("a"), Some("model")).await;
    assert_eq!(held.ordinary_usage_allowed, Some(false));
    assert_eq!(requests(&server, "/api/codex/usage").await, 2);
    let display = backend.quota(&account("a"), crate::DISPLAY_MAX_AGE).await;
    assert_eq!(display.ordinary_usage_allowed, Some(true));
    assert_eq!(requests(&server, "/api/codex/usage").await, 2);

    // A fresh read is trusted as is and, when usable, ends the hold for everyone.
    let fresh =
        <ManagedBackend as AccountBackend>::fresh_usage(&backend, &account("a"), Some("model"))
            .await;
    assert_eq!(fresh.ordinary_usage_allowed, Some(true));
    assert!(!crate::observations::load_usage(&store, &account("a")).rejected(now));
    let shared = backend.recovery_usage(&account("a"), Some("model")).await;
    assert_eq!(shared.ordinary_usage_allowed, Some(true));
    assert_eq!(requests(&server, "/api/codex/usage").await, 3);
}
