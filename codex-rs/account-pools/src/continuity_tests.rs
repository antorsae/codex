use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn successful_account_is_shared_only_with_new_sessions() {
    let (_home, store) = populated_store().await;
    let selection = Some(AccountSelection::Pool("work".to_owned()));
    store.select_default(selection.clone()).await.unwrap();
    let original = store.read().unwrap();
    let first = PoolSession::open(store.clone(), selection.clone(), /*resume_id*/ None)
        .await
        .unwrap()
        .unwrap();
    first.bind_thread("first").await.unwrap();
    let second = PoolSession::open(store.clone(), selection.clone(), /*resume_id*/ None)
        .await
        .unwrap()
        .unwrap();
    let now = chrono::Utc::now().timestamp();
    let backend = FakeBackend::new(vec![
        usage("a", /*short*/ 0.0, /*weekly*/ 40.0, now),
        usage("b", /*short*/ 70.0, /*weekly*/ 70.0, now),
    ]);
    first
        .recover_with(&backend, "model", &CancellationToken::new(), |_| {})
        .await
        .unwrap();
    assert!(
        !store.root().join("pool-state.json").exists(),
        "selection alone is not success"
    );
    first.record_success().await.unwrap();
    let fresh = PoolSession::open(
        store.clone(),
        /*explicit*/ None,
        /*resume_id*/ None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(fresh.selected_account().await.alias, "b");
    second.record_success().await.unwrap();
    let next = PoolSession::open(store.clone(), selection, /*resume_id*/ None)
        .await
        .unwrap()
        .unwrap();
    let resumed = PoolSession::open(store.clone(), /*explicit*/ None, Some("first"))
        .await
        .unwrap()
        .unwrap();
    let pinned = PoolSession::open(
        store.clone(),
        Some(AccountSelection::Account("a".into())),
        Some("first"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        [
            first.selected_account().await.alias,
            second.selected_account().await.alias,
            next.selected_account().await.alias,
            resumed.selected_account().await.alias,
            pinned.selected_account().await.alias
        ],
        ["b", "a", "a", "b", "a"],
    );
    assert_eq!(store.read().unwrap(), original);
}

#[tokio::test]
async fn preferences_ignore_damaged_files_removed_members_and_reassigned_aliases() {
    let (_home, store) = populated_store().await;
    let selection = Some(AccountSelection::Pool("work".to_owned()));
    let path = store.root().join("pool-state.json");
    for value in [
        "{".to_owned(),
        serde_json::to_string(&json!({"work": account("removed")})).unwrap(),
        serde_json::to_string(
            &json!({"work": ManagedAccount { user_id: "different".into(), ..account("b") }}),
        )
        .unwrap(),
    ] {
        std::fs::write(&path, value).unwrap();
        let session = PoolSession::open(store.clone(), selection.clone(), /*resume_id*/ None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.selected_account().await.alias, "a");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_pool_preferences_keep_all_updates() {
    let (_home, store) = populated_store().await;
    let mut sessions = Vec::new();
    for index in 0..8 {
        let name = format!("pool-{index}");
        store
            .put_pool(
                AccountPool {
                    name: name.clone(),
                    accounts: vec!["b".into()],
                    redeem_weekly_resets: false,
                },
                /*create*/ true,
            )
            .await
            .unwrap();
        sessions.push(
            PoolSession::open(
                store.clone(),
                Some(AccountSelection::Pool(name)),
                /*resume_id*/ None,
            )
            .await
            .unwrap()
            .unwrap(),
        );
    }
    let mut writers = tokio::task::JoinSet::new();
    for session in sessions {
        writers.spawn(async move { session.record_success().await });
    }
    while let Some(result) = writers.join_next().await {
        result.unwrap().unwrap();
    }
    let preferences: std::collections::BTreeMap<String, ManagedAccount> =
        serde_json::from_slice(&std::fs::read(store.root().join("pool-state.json")).unwrap())
            .unwrap();
    assert_eq!(
        preferences.keys().cloned().collect::<Vec<_>>(),
        (0..8).map(|i| format!("pool-{i}")).collect::<Vec<_>>()
    );
    assert!(preferences.values().all(|account| account.alias == "b"));
}
