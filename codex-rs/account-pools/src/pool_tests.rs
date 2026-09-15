use crate::AccountBackend;
use crate::AccountStore;
use crate::ManagedBackend;
use crate::PoolSession;
use crate::policy::Decision;
use crate::policy::WindowKind;
use crate::policy::decide;
use base64::Engine;
use codex_backend_client::ConsumeRateLimitResetCreditCode;
use codex_login::AuthConfig;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use codex_login::AuthManager;
use codex_protocol::account_pool::AccountPool;
use codex_protocol::account_pool::AccountPoolEvent;
use codex_protocol::account_pool::AccountQuotaWindow;
use codex_protocol::account_pool::AccountSelection;
use codex_protocol::account_pool::BankedReset;
use codex_protocol::account_pool::ManagedAccount;
use codex_protocol::account_pool::ManagedAccountUsage;
use codex_protocol::account_pool::PoolWaitReason;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[path = "pool_reload_tests.rs"]
mod pool_reload;

#[path = "continuity_tests.rs"]
mod continuity;

pub(super) fn account(alias: &str) -> ManagedAccount {
    ManagedAccount {
        alias: alias.to_owned(),
        user_id: format!("user-{alias}"),
        workspace_id: format!("workspace-{alias}"),
        email: None,
        plan: Some("pro".to_owned()),
    }
}

pub(super) fn usage(alias: &str, short: f64, weekly: f64, now: i64) -> ManagedAccountUsage {
    ManagedAccountUsage {
        account: account(alias),
        pools: vec!["work".to_owned()],
        model: Some("model".to_owned()),
        model_supported: Some(true),
        ordinary_usage_allowed: Some(short > 0.0 && weekly > 0.0),
        windows: vec![
            AccountQuotaWindow {
                limit_id: "codex".to_owned(),
                model: None,
                remaining_percent: short,
                window_minutes: 300,
                resets_at: Some(now + 30),
            },
            AccountQuotaWindow {
                limit_id: "codex".to_owned(),
                model: None,
                remaining_percent: weekly,
                window_minutes: 10080,
                resets_at: Some(now + 3600),
            },
        ],
        available_resets: Some(0),
        resets: Some(Vec::new()),
        checked_at: now,
        error: None,
    }
}

pub(super) fn with_credits(mut usage: ManagedAccountUsage, count: i64) -> ManagedAccountUsage {
    usage.available_resets = Some(count);
    usage.resets = Some(
        (0..count)
            .map(|id| BankedReset {
                id: format!("credit-{id}"),
                expires_at: Some(usage.checked_at + 500 - id),
            })
            .collect(),
    );
    usage
}

#[test]
fn combined_windows_rank_only_usable_accounts_and_keep_current_until_blocked() {
    let now = 1000;
    let mut readings = vec![
        usage("a", /*short*/ 20.0, /*weekly*/ 10.0, now),
        usage("b", /*short*/ 90.0, /*weekly*/ 90.0, now),
    ];
    assert_eq!(
        decide(
            &readings,
            /*current*/ 0,
            WindowKind::Short,
            /*redeem_weekly*/ true,
            now
        ),
        Decision::Use(0)
    );
    readings[0] = usage("a", /*short*/ 0.0, /*weekly*/ 10.0, now);
    readings.push(usage("c", /*short*/ 95.0, /*weekly*/ 0.0, now));
    assert_eq!(
        decide(
            &readings,
            /*current*/ 0,
            WindowKind::Short,
            /*redeem_weekly*/ true,
            now
        ),
        Decision::Use(1)
    );
}

#[test]
fn triggering_window_ranking_and_pool_order_break_ties() {
    let readings = vec![
        usage(
            "a", /*short*/ 0.0, /*weekly*/ 0.0, /*now*/ 1000,
        ),
        usage(
            "b", /*short*/ 90.0, /*weekly*/ 40.0, /*now*/ 1000,
        ),
        usage(
            "c", /*short*/ 50.0, /*weekly*/ 80.0, /*now*/ 1000,
        ),
        usage(
            "d", /*short*/ 50.0, /*weekly*/ 80.0, /*now*/ 1000,
        ),
    ];
    assert_eq!(
        decide(
            &readings,
            /*current*/ 0,
            WindowKind::Weekly,
            /*redeem_weekly*/ true,
            /*now*/ 1000
        ),
        Decision::Use(2)
    );
    assert_eq!(
        decide(
            &readings,
            /*current*/ 0,
            WindowKind::Short,
            /*redeem_weekly*/ true,
            /*now*/ 1000
        ),
        Decision::Use(1)
    );
}

#[test]
fn all_weekly_exhausted_chooses_most_resets_then_earliest_expiry() {
    let readings = vec![
        with_credits(
            usage(
                "a", /*short*/ 0.0, /*weekly*/ 0.0, /*now*/ 1000,
            ),
            1,
        ),
        with_credits(
            usage(
                "b", /*short*/ 50.0, /*weekly*/ 0.0, /*now*/ 1000,
            ),
            3,
        ),
        with_credits(
            usage(
                "c", /*short*/ 50.0, /*weekly*/ 0.0, /*now*/ 1000,
            ),
            3,
        ),
    ];
    assert_eq!(
        decide(
            &readings,
            /*current*/ 0,
            WindowKind::Weekly,
            /*redeem_weekly*/ true,
            /*now*/ 1000
        ),
        Decision::Redeem(1, "credit-2".to_owned())
    );
    assert_eq!(
        decide(
            &readings,
            /*current*/ 0,
            WindowKind::Weekly,
            /*redeem_weekly*/ false,
            /*now*/ 1000
        ),
        Decision::Wait(PoolWaitReason::WeeklyQuota, 1030)
    );
}

#[test]
fn short_exhaustion_with_weekly_capacity_never_spends_a_reset() {
    let readings = vec![
        with_credits(
            usage(
                "a", /*short*/ 50.0, /*weekly*/ 0.0, /*now*/ 1000,
            ),
            3,
        ),
        with_credits(
            usage(
                "b", /*short*/ 0.0, /*weekly*/ 70.0, /*now*/ 1000,
            ),
            3,
        ),
    ];
    assert_eq!(
        decide(
            &readings,
            /*current*/ 0,
            WindowKind::Weekly,
            /*redeem_weekly*/ true,
            /*now*/ 1000
        ),
        Decision::Wait(PoolWaitReason::ShortWindow, 1030)
    );
}

#[test]
fn single_reported_window_controls_switching_waiting_and_redemption() {
    let now = 1000;
    for (trigger, minutes) in [(WindowKind::Short, 300), (WindowKind::Weekly, 10080)] {
        let mut readings = vec![
            with_credits(
                usage("a", /*short*/ 0.0, /*weekly*/ 0.0, now),
                /*count*/ 1,
            ),
            with_credits(
                usage("b", /*short*/ 70.0, /*weekly*/ 70.0, now),
                /*count*/ 3,
            ),
        ];
        for reading in &mut readings {
            reading
                .windows
                .retain(|window| window.window_minutes == minutes);
        }
        assert_eq!(
            decide(
                &readings, /*current*/ 0, trigger, /*redeem_weekly*/ true, now
            ),
            Decision::Use(1)
        );
        readings[1].windows[0].remaining_percent = 0.0;
        readings[1].ordinary_usage_allowed = Some(false);
        assert_eq!(
            decide(
                &readings, /*current*/ 0, trigger, /*redeem_weekly*/ true, now
            ),
            match trigger {
                WindowKind::Short => Decision::Wait(PoolWaitReason::ShortWindow, now + 30),
                WindowKind::Weekly => Decision::Redeem(1, "credit-2".to_owned()),
            }
        );
    }
}

#[test]
fn unknown_or_stale_readings_and_missing_windows_never_authorize_redemption() {
    for mutate in [
        |value: &mut ManagedAccountUsage| value.error = Some("offline".to_owned()),
        |value: &mut ManagedAccountUsage| value.ordinary_usage_allowed = None,
        |value: &mut ManagedAccountUsage| value.model_supported = None,
        |value: &mut ManagedAccountUsage| value.windows.clear(),
        |value: &mut ManagedAccountUsage| value.checked_at = 800,
        |value: &mut ManagedAccountUsage| value.windows[0].remaining_percent = f64::NAN,
    ] {
        let mut unknown = usage(
            "b", /*short*/ 0.0, /*weekly*/ 0.0, /*now*/ 1000,
        );
        mutate(&mut unknown);
        let readings = vec![
            with_credits(
                usage(
                    "a", /*short*/ 50.0, /*weekly*/ 0.0, /*now*/ 1000,
                ),
                3,
            ),
            unknown,
        ];
        assert!(matches!(
            decide(
                &readings,
                /*current*/ 0,
                WindowKind::Weekly,
                /*redeem_weekly*/ true,
                /*now*/ 1000
            ),
            Decision::Wait(PoolWaitReason::UnknownAvailability, _)
        ));
    }
}

#[test]
fn model_specific_windows_and_unsupported_models_are_respected() {
    let mut limited = usage(
        "b", /*short*/ 90.0, /*weekly*/ 90.0, /*now*/ 1000,
    );
    limited.windows.push(AccountQuotaWindow {
        limit_id: "special".to_owned(),
        model: Some("model".to_owned()),
        remaining_percent: 0.0,
        window_minutes: 300,
        resets_at: Some(1040),
    });
    let mut unsupported = usage(
        "c", /*short*/ 99.0, /*weekly*/ 99.0, /*now*/ 1000,
    );
    unsupported.model_supported = Some(false);
    let readings = vec![
        usage(
            "a", /*short*/ 0.0, /*weekly*/ 50.0, /*now*/ 1000,
        ),
        limited,
        unsupported,
        usage(
            "d", /*short*/ 10.0, /*weekly*/ 10.0, /*now*/ 1000,
        ),
    ];
    assert_eq!(
        decide(
            &readings,
            /*current*/ 0,
            WindowKind::Short,
            /*redeem_weekly*/ true,
            /*now*/ 1000
        ),
        Decision::Use(3)
    );
}

#[test]
fn expired_credits_and_unknown_credit_details_wait_until_the_earliest_reset() {
    let mut reading = with_credits(
        usage(
            "a", /*short*/ 90.0, /*weekly*/ 0.0, /*now*/ 1000,
        ),
        1,
    );
    reading.resets.as_mut().unwrap()[0].expires_at = Some(1000);
    assert_eq!(
        decide(
            &[reading.clone()],
            /*current*/ 0,
            WindowKind::Weekly,
            /*redeem_weekly*/ true,
            /*now*/ 1000
        ),
        Decision::Wait(PoolWaitReason::WeeklyQuota, 1030)
    );
    reading.resets = None;
    for window in &mut reading.windows {
        window.resets_at = None;
    }
    assert_eq!(
        decide(
            &[reading],
            /*current*/ 0,
            WindowKind::Weekly,
            /*redeem_weekly*/ true,
            /*now*/ 1000
        ),
        Decision::Wait(PoolWaitReason::WeeklyQuota, 1060)
    );
}

pub(super) fn store(home: &std::path::Path) -> AccountStore {
    AccountStore::new(AuthConfig {
        codex_home: home.to_owned(),
        auth_credentials_store_mode: AuthCredentialsStoreMode::File,
        keyring_backend_kind: AuthKeyringBackendKind::default(),
        forced_login_method: None,
        forced_chatgpt_workspace_id: None,
        managed_auth_policy: Default::default(),
        chatgpt_base_url: Some("https://unused.invalid".to_owned()),
        auth_route_config: codex_login::test_support::transport_default_auth_route_config(),
    })
}

fn write_auth(home: &std::path::Path, alias: &str) {
    std::fs::create_dir_all(home).unwrap();
    let payload = json!({ "exp": 4_102_444_800i64,
        "https://api.openai.com/auth": {"chatgpt_user_id": format!("user-{alias}"),
            "chatgpt_account_id": format!("workspace-{alias}"), "chatgpt_plan_type": "pro"} });
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&payload).unwrap());
    let token = format!("e30.{payload}.c2ln");
    std::fs::write(home.join("auth.json"), serde_json::to_vec(&json!({
        "auth_mode": "chatgpt", "tokens": { "id_token": token,
            "access_token": token, "refresh_token": format!("secret-refresh-{alias}"), "account_id": format!("workspace-{alias}") },
        "last_refresh": chrono::Utc::now(),
    })).unwrap()).unwrap();
}

pub(super) async fn populated_store() -> (tempfile::TempDir, AccountStore) {
    let home = tempfile::tempdir().unwrap();
    let store = store(home.path());
    for alias in ["a", "b"] {
        let source = tempfile::tempdir().unwrap();
        write_auth(source.path(), alias);
        store.import(alias, source.path()).await.unwrap();
    }
    store
        .put_pool(
            AccountPool {
                name: "work".to_owned(),
                accounts: vec!["a".to_owned(), "b".to_owned()],
                redeem_weekly_resets: true,
            },
            /*create*/ true,
        )
        .await
        .unwrap();
    (home, store)
}

#[tokio::test]
async fn duplicate_identity_has_one_alias_and_one_quota_slot() {
    let (_home, store) = populated_store().await;
    let source = tempfile::tempdir().unwrap();
    write_auth(source.path(), "a");
    let imported = store.import("duplicate", source.path()).await.unwrap();
    assert_eq!(imported, account("a"));
    assert_eq!(
        store.read().unwrap().accounts,
        vec![account("a"), account("b")]
    );
    assert!(
        store
            .put_pool(
                AccountPool {
                    name: "bad".to_owned(),
                    accounts: vec!["a".to_owned(), "a".to_owned()],
                    redeem_weekly_resets: true
                },
                /*create*/ true
            )
            .await
            .is_err()
    );
    assert!(store.import("../escape", source.path()).await.is_err());
}

#[tokio::test]
async fn imported_login_tracks_canonical_credentials_without_changing_a_later_login() {
    let home = tempfile::tempdir().unwrap();
    let store = store(home.path());
    write_auth(home.path(), "a");
    let account = store.import("a", home.path()).await.unwrap();
    let canonical = store.credential_home(&account);
    let mut raw = codex_login::load_auth_dot_json(
        &canonical,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )
    .unwrap()
    .unwrap();
    raw.tokens.as_mut().unwrap().access_token = "rotated-access".to_owned();
    codex_login::save_auth(
        &canonical,
        &raw,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )
    .unwrap();
    let legacy = AuthManager::managed_from_auth_config(store.auth_config.clone()).await;
    assert_eq!(
        legacy.auth_cached().unwrap().get_token().unwrap(),
        "rotated-access"
    );
    write_auth(home.path(), "b");
    legacy.reload().await;
    assert_eq!(
        legacy.auth_cached().unwrap().get_account_id(),
        Some("workspace-b".to_owned())
    );
}

#[derive(Default)]
struct FakeBackend {
    usage: Mutex<HashMap<String, ManagedAccountUsage>>,
    calls: Mutex<Vec<(String, String)>>,
    fail_once: AtomicBool,
    outcome_once: Mutex<Option<ConsumeRateLimitResetCreditCode>>,
}

impl FakeBackend {
    fn new(values: Vec<ManagedAccountUsage>) -> Self {
        Self {
            usage: Mutex::new(
                values
                    .into_iter()
                    .map(|value| (value.account.alias.clone(), value))
                    .collect(),
            ),
            ..Default::default()
        }
    }
}

impl AccountBackend for FakeBackend {
    async fn usage(&self, account: &ManagedAccount, _model: Option<&str>) -> ManagedAccountUsage {
        self.usage.lock().unwrap()[&account.alias].clone()
    }
    async fn redeem(
        &self,
        account: &ManagedAccount,
        key: &str,
        credit: &str,
    ) -> anyhow::Result<ConsumeRateLimitResetCreditCode> {
        self.calls
            .lock()
            .unwrap()
            .push((key.to_owned(), credit.to_owned()));
        if self.fail_once.swap(/*val*/ false, Ordering::SeqCst) {
            anyhow::bail!("ambiguous timeout");
        }
        if let Some(outcome) = self.outcome_once.lock().unwrap().take() {
            return Ok(outcome);
        }
        let mut values = self.usage.lock().unwrap();
        let value = values.get_mut(&account.alias).unwrap();
        value.ordinary_usage_allowed = Some(true);
        for window in &mut value.windows {
            window.remaining_percent = 100.0;
        }
        value.available_resets = value.available_resets.map(|count| count - 1);
        value
            .resets
            .as_mut()
            .unwrap()
            .retain(|item| item.id != credit);
        Ok(ConsumeRateLimitResetCreditCode::Reset)
    }
}

#[tokio::test]
async fn concurrent_and_crashed_redemptions_reuse_one_key_and_credit() {
    let (_home, store) = populated_store().await;
    let now = chrono::Utc::now().timestamp();
    let backend = FakeBackend::new(vec![with_credits(
        usage("a", /*short*/ 40.0, /*weekly*/ 0.0, now),
        2,
    )]);
    backend.fail_once.store(/*val*/ true, Ordering::SeqCst);
    let account = account("a");
    assert!(
        crate::redemption::redeem(
            &store,
            &backend,
            &account,
            Some("model"),
            "credit-1",
            crate::redemption::RedemptionMode::Automatic
        )
        .await
        .is_err()
    );
    let on_disk =
        std::fs::read_to_string(store.credential_home(&account).join("redemption.json")).unwrap();
    assert!(on_disk.contains(&backend.calls.lock().unwrap()[0].0));
    let (first, second) = tokio::join!(
        crate::redemption::redeem(
            &store,
            &backend,
            &account,
            Some("model"),
            "credit-0",
            crate::redemption::RedemptionMode::Automatic
        ),
        crate::redemption::redeem(
            &store,
            &backend,
            &account,
            Some("model"),
            "credit-0",
            crate::redemption::RedemptionMode::Automatic
        ),
    );
    assert!(first.is_ok() && second.is_ok());
    let calls = backend.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], calls[1]);
    assert_eq!(backend.usage.lock().unwrap()["a"].available_resets, Some(1));
}

#[tokio::test]
async fn sessions_switch_independently_and_resume_their_own_selection() {
    let (_home, store) = populated_store().await;
    let selection = Some(AccountSelection::Pool("work".to_owned()));
    let first = PoolSession::open(store.clone(), selection.clone(), /*resume_id*/ None)
        .await
        .unwrap()
        .unwrap();
    let second = PoolSession::open(store.clone(), selection, /*resume_id*/ None)
        .await
        .unwrap()
        .unwrap();
    first.bind_thread("thread-a").await.unwrap();
    let now = chrono::Utc::now().timestamp();
    let backend = FakeBackend::new(vec![
        usage("a", /*short*/ 0.0, /*weekly*/ 40.0, now),
        usage("b", /*short*/ 70.0, /*weekly*/ 70.0, now),
    ]);
    let mut events = Vec::new();
    first
        .recover_with(&backend, "model", &CancellationToken::new(), |event| {
            events.push(event)
        })
        .await
        .unwrap();
    assert_eq!(
        events,
        vec![AccountPoolEvent::Switched {
            account: "b".to_owned(),
            previous_account: "a".to_owned()
        }]
    );
    assert_eq!(
        first.auth_manager().auth().await.unwrap().get_account_id(),
        Some("workspace-b".to_owned())
    );
    assert_eq!(
        second.auth_manager().auth().await.unwrap().get_account_id(),
        Some("workspace-a".to_owned())
    );
    let resumed = PoolSession::open(store, /*explicit*/ None, Some("thread-a"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        resumed
            .auth_manager()
            .auth()
            .await
            .unwrap()
            .get_account_id(),
        Some("workspace-b".to_owned())
    );
}

#[tokio::test(start_paused = true)]
async fn waiting_is_cancellable_and_does_not_run_after_cancellation() {
    let (_home, store) = populated_store().await;
    let session = PoolSession::open(
        store.clone(),
        Some(AccountSelection::Pool("work".to_owned())),
        /*resume_id*/ None,
    )
    .await
    .unwrap()
    .unwrap();
    session.bind_thread("waiting-thread").await.unwrap();
    let now = chrono::Utc::now().timestamp();
    let backend = Arc::new(FakeBackend::new(vec![
        usage("a", /*short*/ 0.0, /*weekly*/ 40.0, now),
        usage("b", /*short*/ 0.0, /*weekly*/ 40.0, now),
    ]));
    let cancel = CancellationToken::new();
    let on_wait = cancel.clone();
    let result = session
        .recover_with(backend.as_ref(), "model", &cancel, |event| {
            assert!(matches!(
                event,
                AccountPoolEvent::Waiting {
                    reason: PoolWaitReason::ShortWindow,
                    ..
                }
            ));
            on_wait.cancel();
        })
        .await;
    assert!(result.is_err());
    tokio::time::advance(std::time::Duration::from_secs(600)).await;
    assert!(backend.calls.lock().unwrap().is_empty());
    let saved = std::fs::read_to_string(store.root().join("sessions/waiting-thread.json")).unwrap();
    assert!(saved.contains("\"waiting\": true"));
    let resumed = PoolSession::open(
        store.clone(),
        /*explicit*/ None,
        Some("waiting-thread"),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(resumed.is_waiting());
    resumed.bind_thread("waiting-thread").await.unwrap();
    let saved: serde_json::Value = serde_json::from_slice(
        &std::fs::read(store.root().join("sessions/waiting-thread.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["waiting"], true);
}

#[tokio::test]
async fn backend_errors_are_unknown_and_do_not_expose_response_secrets() {
    let (_home, mut store) = populated_store().await;
    let server = wiremock::MockServer::start().await;
    store.auth_config.chatgpt_base_url = Some(server.uri());
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .respond_with(ResponseTemplate::new(500).set_body_string("secret-access secret-refresh"))
        .mount(&server)
        .await;
    let usage = ManagedBackend::new(store)
        .usage(&account("a"), Some("model"))
        .await;
    assert_eq!(usage.ordinary_usage_allowed, None);
    assert_eq!(usage.resets, None);
    assert!(usage.error.is_some());
    let serialized = serde_json::to_string(&usage).unwrap();
    assert!(!serialized.contains("secret-access") && !serialized.contains("secret-refresh"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_rotation_is_shared_between_processes() -> anyhow::Result<()> {
    const CHILD_HOME: &str = "CODEX_POOL_TEST_REFRESH_HOME";
    if let Some(home) = std::env::var_os(CHILD_HOME) {
        let home = std::path::PathBuf::from(home);
        let manager = AuthManager::managed_from_auth_config(store(&home).auth_config).await;
        let barrier =
            std::path::PathBuf::from(std::env::var_os("CODEX_POOL_TEST_BARRIER").unwrap());
        std::fs::write(
            barrier.join(format!("ready-{}", std::process::id())),
            "ready",
        )?;
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while !barrier.join("go").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await?;
        manager.refresh_token().await?;
        assert_eq!(
            manager.auth_cached().unwrap().get_token()?,
            "refreshed-access"
        );
        return Ok(());
    }
    let home = tempfile::tempdir()?;
    let store = store(home.path());
    write_auth(home.path(), "a");
    let account = store.import("a", home.path()).await?;
    let server = wiremock::MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(100))
                .set_body_json(json!({
                    "access_token": "refreshed-access", "refresh_token": "refreshed-refresh"
                })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut children = Vec::new();
    for auth_home in [home.path().to_path_buf(), store.credential_home(&account)] {
        children.push(
            tokio::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "tests::refresh_rotation_is_shared_between_processes",
                    "--nocapture",
                ])
                .env(CHILD_HOME, auth_home)
                .env("CODEX_POOL_TEST_BARRIER", home.path())
                .env(
                    codex_login::REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR,
                    format!("{}/oauth/token", server.uri()),
                )
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()?,
        );
    }
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while std::fs::read_dir(home.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("ready-"))
            .count()
            < 2
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await?;
    std::fs::write(home.path().join("go"), "go")?;
    for child in children {
        let output = child.wait_with_output().await?;
        assert!(
            output.status.success(),
            "child failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[tokio::test]
async fn definitive_declines_allow_a_new_credit_but_applied_resets_wait_for_quota() {
    for outcome in [
        ConsumeRateLimitResetCreditCode::NoCredit,
        ConsumeRateLimitResetCreditCode::NothingToReset,
        ConsumeRateLimitResetCreditCode::AlreadyRedeemed,
    ] {
        let (_home, store) = populated_store().await;
        let backend = FakeBackend::new(vec![with_credits(
            usage(
                "a",
                /*short*/ 0.0,
                /*weekly*/ 0.0,
                chrono::Utc::now().timestamp(),
            ),
            2,
        )]);
        *backend.outcome_once.lock().unwrap() = Some(outcome);
        let _ = crate::redemption::redeem(
            &store,
            &backend,
            &account("a"),
            Some("model"),
            "credit-0",
            crate::redemption::RedemptionMode::Automatic,
        )
        .await;
        let next = crate::redemption::redeem(
            &store,
            &backend,
            &account("a"),
            Some("model"),
            "credit-1",
            crate::redemption::RedemptionMode::Automatic,
        )
        .await;
        if outcome == ConsumeRateLimitResetCreditCode::AlreadyRedeemed {
            assert!(next.is_err());
            assert_eq!(backend.calls.lock().unwrap().len(), 1);
        } else {
            assert_eq!(next.unwrap().ordinary_usage_allowed, Some(true));
            assert_eq!(backend.calls.lock().unwrap().len(), 2);
        }
    }
}

#[tokio::test]
async fn pending_automatic_intent_cannot_spend_for_short_quota_after_a_crash() {
    let (_home, store) = populated_store().await;
    let now = chrono::Utc::now().timestamp();
    let backend = FakeBackend::new(vec![with_credits(
        usage("a", /*short*/ 0.0, /*weekly*/ 0.0, now),
        2,
    )]);
    backend.fail_once.store(/*val*/ true, Ordering::SeqCst);
    let _ = crate::redemption::redeem(
        &store,
        &backend,
        &account("a"),
        Some("model"),
        "credit-0",
        crate::redemption::RedemptionMode::Automatic,
    )
    .await;
    backend.usage.lock().unwrap().insert(
        "a".to_owned(),
        with_credits(usage("a", /*short*/ 0.0, /*weekly*/ 70.0, now), 2),
    );
    assert!(
        crate::redemption::redeem(
            &store,
            &backend,
            &account("a"),
            Some("model"),
            "credit-1",
            crate::redemption::RedemptionMode::Automatic
        )
        .await
        .is_err()
    );
    assert_eq!(backend.calls.lock().unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn waiting_checks_at_advertised_reset_without_spending_short_window_credits() {
    let (_home, store) = populated_store().await;
    let session = PoolSession::open(
        store,
        Some(AccountSelection::Pool("work".to_owned())),
        /*resume_id*/ None,
    )
    .await
    .unwrap()
    .unwrap();
    let now = chrono::Utc::now().timestamp();
    let mut readings = vec![
        with_credits(usage("a", /*short*/ 0.0, /*weekly*/ 40.0, now), 2),
        with_credits(usage("b", /*short*/ 0.0, /*weekly*/ 40.0, now), 2),
    ];
    for usage in &mut readings {
        usage.windows[0].resets_at = Some(now + 5);
    }
    let backend = FakeBackend::new(readings);
    let started = tokio::time::Instant::now();
    let mut events = Vec::new();
    session
        .recover_with(&backend, "model", &CancellationToken::new(), |event| {
            if let AccountPoolEvent::Waiting { next_check_at, .. } = &event {
                assert_eq!(*next_check_at, now + 5);
                backend.usage.lock().unwrap().insert(
                    "b".to_owned(),
                    usage("b", /*short*/ 90.0, /*weekly*/ 40.0, now),
                );
            }
            events.push(event);
        })
        .await
        .unwrap();
    assert_eq!(started.elapsed(), std::time::Duration::from_secs(5));
    assert_eq!(events.len(), 2);
    assert!(backend.calls.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn failed_usage_backoff_still_checks_at_a_reset_after_the_normal_poll_interval() {
    let (_home, store) = populated_store().await;
    let session = PoolSession::open(
        store,
        Some(AccountSelection::Pool("work".to_owned())),
        /*resume_id*/ None,
    )
    .await
    .unwrap()
    .unwrap();
    let now = chrono::Utc::now().timestamp();
    let mut readings = vec![
        usage("a", /*short*/ 0.0, /*weekly*/ 0.0, now),
        usage("b", /*short*/ 0.0, /*weekly*/ 0.0, now),
    ];
    for reading in &mut readings {
        reading.error = Some("usage unavailable".to_owned());
        for window in &mut reading.windows {
            window.resets_at = Some(now + 75);
        }
    }
    let backend = FakeBackend::new(readings);
    let cancel = CancellationToken::new();
    let mut events = Vec::new();
    let result = session
        .recover_with(&backend, "model", &cancel, |event| {
            events.push(event);
            cancel.cancel();
        })
        .await;
    assert!(result.is_err());
    assert_eq!(
        events,
        vec![AccountPoolEvent::Waiting {
            account: "a".to_owned(),
            reason: PoolWaitReason::UnknownAvailability,
            next_check_at: now + 75,
        }]
    );
    assert!(backend.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn candidate_usage_401_refreshes_before_becoming_unknown() -> anyhow::Result<()> {
    const CHILD_HOME: &str = "CODEX_POOL_TEST_USAGE_REFRESH_HOME";
    if let Some(home) = std::env::var_os(CHILD_HOME) {
        let mut store = store(std::path::Path::new(&home));
        store.auth_config.chatgpt_base_url = Some(std::env::var("CODEX_POOL_TEST_USAGE_URL")?);
        let usage = ManagedBackend::new(store)
            .usage(&account("a"), Some("model"))
            .await;
        assert_eq!(
            (
                usage.error,
                usage.model_supported,
                usage.ordinary_usage_allowed
            ),
            (None, Some(true), Some(true))
        );
        return Ok(());
    }
    let (home, _store) = populated_store().await;
    let server = wiremock::MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"access_token":"refreshed-access","refresh_token":"refreshed-refresh"}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/api/codex/usage"))
        .and(wiremock::matchers::header("authorization", "Bearer refreshed-access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"plan_type":"pro","rate_limit":{"allowed":true,"limit_reached":false,
            "primary_window":{"used_percent":10,"limit_window_seconds":18000,"reset_after_seconds":3600,"reset_at":2_000_000_000},
            "secondary_window":{"used_percent":10,"limit_window_seconds":604800,"reset_after_seconds":3600,"reset_at":2_000_000_000}}}))).with_priority(1)
        .expect(1).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/codex/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"models":[{"slug":"model"}]})),
        )
        .mount(&server)
        .await;
    let output = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "tests::candidate_usage_401_refreshes_before_becoming_unknown",
            "--nocapture",
        ])
        .env(CHILD_HOME, home.path())
        .env("CODEX_POOL_TEST_USAGE_URL", server.uri())
        .env(
            codex_login::REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR,
            format!("{}/oauth/token", server.uri()),
        )
        .output()
        .await?;
    assert!(
        output.status.success(),
        "child failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn pending_reset_cannot_override_disabled_policy_or_unsupported_model() {
    for disabled in [true, false] {
        let (_home, store) = populated_store().await;
        let backend = FakeBackend::new(vec![
            with_credits(
                usage(
                    "a",
                    /*short*/ 0.0,
                    /*weekly*/ 0.0,
                    chrono::Utc::now().timestamp(),
                ),
                2,
            ),
            usage(
                "b",
                /*short*/ 0.0,
                /*weekly*/ 0.0,
                chrono::Utc::now().timestamp(),
            ),
        ]);
        backend.fail_once.store(/*val*/ true, Ordering::SeqCst);
        let _ = crate::redemption::redeem(
            &store,
            &backend,
            &account("a"),
            Some("model"),
            "credit-0",
            crate::redemption::RedemptionMode::Automatic,
        )
        .await;
        if disabled {
            let mut pool = store.read().unwrap().pools.remove(0);
            pool.redeem_weekly_resets = false;
            store.put_pool(pool, /*create*/ false).await.unwrap();
        } else {
            backend
                .usage
                .lock()
                .unwrap()
                .get_mut("a")
                .unwrap()
                .model_supported = Some(false);
        }
        let session = PoolSession::open(
            store,
            Some(AccountSelection::Pool("work".to_owned())),
            /*resume_id*/ None,
        )
        .await
        .unwrap()
        .unwrap();
        let cancel = CancellationToken::new();
        let on_wait = cancel.clone();
        assert!(
            session
                .recover_with(&backend, "model", &cancel, |_| on_wait.cancel())
                .await
                .is_err()
        );
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
    }
}
