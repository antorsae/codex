use super::*;
use crate::storage::write_json;
use pretty_assertions::assert_eq;
use std::time::Duration;
use wiremock::matchers::header;

#[tokio::test]
async fn waiting_session_picks_up_new_accounts_without_restarting() {
    let (home, mut store) = populated_store().await;
    let server = wiremock::MockServer::start().await;
    store.auth_config.chatgpt_base_url = Some(server.uri());
    for alias in ["a", "b", "c"] {
        let ready = alias == "c";
        Mock::given(method("GET")).and(path("/api/codex/usage"))
            .and(header("chatgpt-account-id", format!("workspace-{alias}")))
            .respond_with(move |_: &wiremock::Request| ResponseTemplate::new(/*s*/ 200).set_body_json(json!({
                "plan_type": "pro", "user_id": format!("user-{alias}"), "account_id": format!("workspace-{alias}"),
                "rate_limit": {"allowed": ready, "limit_reached": !ready,
                    "primary_window": {"used_percent": if ready { 10 } else { 100 },
                        "limit_window_seconds": 604800, "reset_after_seconds": 1,
                        "reset_at": chrono::Utc::now().timestamp() + 1}},
                "rate_limit_reset_credits": {"available_count": 0},
            }))).mount(&server).await;
    }
    Mock::given(method("GET"))
        .and(path("/api/codex/models"))
        .respond_with(
            ResponseTemplate::new(/*s*/ 200).set_body_json(json!({"models": [{"slug": "model"}]})),
        )
        .mount(&server)
        .await;
    let session = PoolSession::open(
        store.clone(),
        Some(AccountSelection::Pool("work".to_owned())),
        /*resume_id*/ None,
    )
    .await
    .unwrap()
    .unwrap();
    session.bind_thread("reload-thread").await.unwrap();
    let replacement = AccountPool {
        name: "work".to_owned(),
        // Remove the current member and reuse its old index for a different identity.
        accounts: vec!["c".to_owned(), "b".to_owned()],
        redeem_weekly_resets: false,
    };
    let (send, mut events) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    tokio::time::timeout(Duration::from_secs(/*secs*/ 15), async {
        let (recovered, ()) = tokio::join!(
            session.recover("model", &cancel, |event| {
                send.send(event).unwrap();
            }),
            async {
                assert!(matches!(
                    events.recv().await,
                    Some(AccountPoolEvent::Waiting { .. })
                ));
                // A separate store uses exactly the writes performed by another CLI process.
                let editor = super::store(home.path());
                let source = tempfile::tempdir().unwrap();
                write_auth(source.path(), "c");
                editor.import("c", source.path()).await.unwrap();
                editor
                    .put_pool(replacement.clone(), /*create*/ false)
                    .await
                    .unwrap();
                editor
                    .select_default(Some(AccountSelection::Account("b".to_owned())))
                    .await
                    .unwrap();
            }
        );
        recovered.unwrap();
    })
    .await
    .unwrap();
    assert_eq!(
        events.try_recv().unwrap(),
        AccountPoolEvent::Switched {
            account: "c".to_owned(),
            previous_account: "a".to_owned(),
        }
    );
    assert_eq!(session.selected_account().await, account("c"));
    assert_eq!(session.pool(), Some(replacement));
    assert_eq!(
        session
            .auth_manager()
            .auth()
            .await
            .unwrap()
            .get_account_id(),
        Some("workspace-c".to_owned())
    );
    assert!(!session.is_waiting());
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.method == "GET")
    );
    let saved: serde_json::Value = serde_json::from_slice(
        &std::fs::read(store.root().join("sessions/reload-thread.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        saved,
        json!({
            "selection": {"type": "pool", "name": "work"}, "account": "c", "waiting": false,
        })
    );
}

#[tokio::test(start_paused = true)]
async fn incomplete_configuration_waits_for_a_valid_snapshot() {
    for incomplete in [
        None,
        Some("{\"accounts\":"),
        Some(r#"{"accounts":[],"pools":[],"defaultSelection":null}"#),
        Some(
            r#"{"accounts":[],"pools":[{"name":"work","accounts":["missing"],"redeemWeeklyResets":false}],"defaultSelection":null}"#,
        ),
    ] {
        let (_home, store) = populated_store().await;
        let config = store.read().unwrap();
        let session = PoolSession::open(
            store.clone(),
            Some(AccountSelection::Pool("work".to_owned())),
            /*resume_id*/ None,
        )
        .await
        .unwrap()
        .unwrap();
        let original_pool = session.pool();
        let path = store.root().join("accounts.json");
        match incomplete {
            Some(contents) => std::fs::write(&path, contents).unwrap(),
            None => std::fs::remove_file(&path).unwrap(),
        }
        let now = chrono::Utc::now().timestamp();
        let backend = FakeBackend::new(vec![
            with_credits(
                usage("a", /*short*/ 0.0, /*weekly*/ 0.0, now),
                /*count*/ 2,
            ),
            usage("b", /*short*/ 80.0, /*weekly*/ 90.0, now),
        ]);
        let mut events = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(/*secs*/ 90),
            session.recover_with(&backend, "model", &CancellationToken::new(), |event| {
                if matches!(&event, AccountPoolEvent::Waiting { .. }) {
                    assert_eq!(session.pool(), original_pool);
                    assert_eq!(
                        event,
                        AccountPoolEvent::Waiting {
                            account: "a".to_owned(),
                            reason: PoolWaitReason::UnknownAvailability,
                            next_check_at: now + 60,
                        }
                    );
                    write_json(&path, &config).unwrap();
                }
                events.push(event);
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(session.selected_account().await, account("b"));
        assert!(!session.is_waiting());
        assert!(backend.calls.lock().unwrap().is_empty());
    }
}

struct UpdatingBackend {
    inner: FakeBackend,
    store: AccountStore,
    replacement: Mutex<Option<AccountPool>>,
}

impl AccountBackend for UpdatingBackend {
    async fn usage(&self, account: &ManagedAccount, model: Option<&str>) -> ManagedAccountUsage {
        let replacement = self.replacement.lock().unwrap().take();
        if let Some(replacement) = replacement {
            self.store
                .put_pool(replacement, /*create*/ false)
                .await
                .unwrap();
        }
        self.inner.usage(account, model).await
    }

    async fn redeem(
        &self,
        account: &ManagedAccount,
        key: &str,
        credit: &str,
    ) -> anyhow::Result<ConsumeRateLimitResetCreditCode> {
        self.inner.redeem(account, key, credit).await
    }
}

#[tokio::test(start_paused = true)]
async fn edits_during_usage_reads_invalidate_switches_and_reset_permission() {
    for remaining in [0.0, 90.0] {
        let (_home, store) = populated_store().await;
        let session = PoolSession::open(
            store.clone(),
            Some(AccountSelection::Pool("work".to_owned())),
            /*resume_id*/ None,
        )
        .await
        .unwrap()
        .unwrap();
        let now = chrono::Utc::now().timestamp();
        let replacement = AccountPool {
            name: "work".to_owned(),
            accounts: vec!["a".to_owned()],
            redeem_weekly_resets: false,
        };
        let backend = UpdatingBackend {
            inner: FakeBackend::new(vec![
                with_credits(
                    usage("a", /*short*/ 0.0, /*weekly*/ 0.0, now),
                    /*count*/ 2,
                ),
                with_credits(usage("b", remaining, remaining, now), /*count*/ 2),
            ]),
            store,
            replacement: Mutex::new(Some(replacement.clone())),
        };
        let cancel = CancellationToken::new();
        let mut events = Vec::new();
        assert!(
            session
                .recover_with(&backend, "model", &cancel, |event| {
                    events.push(event);
                    cancel.cancel();
                })
                .await
                .is_err()
        );
        assert_eq!(
            events,
            vec![AccountPoolEvent::Waiting {
                account: "a".to_owned(),
                reason: PoolWaitReason::WeeklyQuota,
                next_check_at: now + 30,
            }]
        );
        assert_eq!(session.pool(), Some(replacement));
        assert_eq!(session.selected_account().await, account("a"));
        assert!(backend.inner.calls.lock().unwrap().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn interrupted_credential_update_does_not_end_recovery() {
    let (_home, store) = populated_store().await;
    let session = PoolSession::open(
        store.clone(),
        Some(AccountSelection::Pool("work".to_owned())),
        /*resume_id*/ None,
    )
    .await
    .unwrap()
    .unwrap();
    let path = store.credential_home(&account("b")).join("auth.json");
    let credentials = std::fs::read(&path).unwrap();
    std::fs::write(&path, "{").unwrap();
    let now = chrono::Utc::now().timestamp();
    let backend = FakeBackend::new(vec![
        usage("a", /*short*/ 0.0, /*weekly*/ 0.0, now),
        usage("b", /*short*/ 90.0, /*weekly*/ 90.0, now),
    ]);
    let mut events = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(/*secs*/ 90),
        session.recover_with(&backend, "model", &CancellationToken::new(), |event| {
            if let AccountPoolEvent::Waiting { reason, .. } = &event {
                assert_eq!(*reason, PoolWaitReason::UnknownAvailability);
                std::fs::write(&path, &credentials).unwrap();
            }
            events.push(event);
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(session.selected_account().await, account("b"));
    assert!(!session.is_waiting());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_pool_writers_publish_complete_snapshots_without_lost_updates() {
    let (_home, store) = populated_store().await;
    let finished = Arc::new(AtomicBool::new(/*v*/ false));
    let reader_store = store.clone();
    let reader_finished = finished.clone();
    let reader = tokio::task::spawn_blocking(move || {
        let mut reads = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(/*secs*/ 10);
        loop {
            let config = reader_store.read().unwrap();
            assert_eq!(config.accounts, vec![account("a"), account("b")]);
            for pool in config.pools {
                assert_eq!(pool.accounts, vec!["a".to_owned(), "b".to_owned()]);
            }
            reads += 1;
            if reader_finished.load(Ordering::Acquire) || std::time::Instant::now() >= deadline {
                break;
            }
        }
        reads
    });
    let mut writers = Vec::new();
    // Each writer must wait on the same cross-process lock before reading its base snapshot.
    let guard = crate::storage::lock(&store.root().join("config.lock"))
        .await
        .unwrap();
    for index in 0..16 {
        let writer = store.clone();
        writers.push(tokio::spawn(async move {
            writer
                .put_pool(
                    AccountPool {
                        name: format!("pool-{index}"),
                        accounts: vec!["a".to_owned(), "b".to_owned()],
                        redeem_weekly_resets: false,
                    },
                    /*create*/ true,
                )
                .await
                .unwrap();
        }));
    }
    drop(guard);
    for writer in writers {
        writer.await.unwrap();
    }
    finished.store(/*val*/ true, Ordering::Release);
    assert!(reader.await.unwrap() > 0);
    let mut names: Vec<_> = store
        .read()
        .unwrap()
        .pools
        .into_iter()
        .map(|pool| pool.name)
        .collect();
    let mut expected: Vec<_> = (0..16).map(|index| format!("pool-{index}")).collect();
    expected.push("work".to_owned());
    names.sort();
    expected.sort();
    assert_eq!(names, expected);
}
