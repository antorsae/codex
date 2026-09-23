use super::*;
use pretty_assertions::assert_eq;
use std::time::Duration;
use tokio::sync::mpsc;

#[test]
fn weekly_backoff_retries_unknown_credit_metadata_only_when_redemption_is_enabled() {
    let valid = BankedReset {
        id: "valid".to_owned(),
        expires_at: Some(1500),
    };
    let expired = BankedReset {
        id: "expired".to_owned(),
        expires_at: Some(1000),
    };
    for (available_resets, resets, delay) in [
        (Some(2), None, 600),
        (None, None, 600),
        (None, Some(vec![valid]), 600),
        (Some(0), None, 3600),
        (Some(2), Some(Vec::new()), 3600),
        (None, Some(vec![expired]), 3600),
    ] {
        let mut reading = usage(
            "a", /*short*/ 90.0, /*weekly*/ 0.0, /*now*/ 1000,
        );
        reading.available_resets = available_resets;
        reading.resets = resets;
        for redeem_weekly in [true, false] {
            assert_eq!(
                decide(
                    std::slice::from_ref(&reading),
                    /*current*/ 0,
                    WindowKind::Weekly,
                    redeem_weekly,
                    /*now*/ 1000
                ),
                Decision::Wait(
                    PoolWaitReason::WeeklyQuota,
                    1000 + if redeem_weekly { delay } else { 3600 }
                ),
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn weekly_wait_recovers_when_missing_credit_details_return_after_ten_minutes() {
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
    let mut pending = usage("a", /*short*/ 90.0, /*weekly*/ 0.0, now);
    pending.available_resets = Some(2);
    pending.resets = None;
    let backend = Arc::new(FakeBackend::new(vec![
        pending.clone(),
        usage("b", /*short*/ 90.0, /*weekly*/ 0.0, now),
    ]));
    let (events, mut received) = mpsc::unbounded_channel();
    let recovery = tokio::spawn({
        let backend = Arc::clone(&backend);
        let session = Arc::clone(&session);
        async move {
            session
                .recover_with(&*backend, "model", &CancellationToken::new(), |event| {
                    events.send(event).unwrap();
                })
                .await
        }
    });
    assert_eq!(
        received.recv().await,
        Some(AccountPoolEvent::Waiting {
            account: "a".to_owned(),
            reason: PoolWaitReason::WeeklyQuota,
            next_check_at: now + 600,
        })
    );
    let mut credited = with_credits(pending, /*count*/ 2);
    // Banked credits must outlive the ten-minute wait.
    for credit in credited.resets.as_mut().unwrap() {
        credit.expires_at = Some(now + 5_000);
    }
    backend
        .usage
        .lock()
        .unwrap()
        .insert("a".to_owned(), credited);
    tokio::time::advance(Duration::from_secs(/*secs*/ 599)).await;
    tokio::task::yield_now().await;
    assert_eq!(backend.usage_calls.load(Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_secs(/*secs*/ 1)).await;
    recovery.await.unwrap().unwrap();
    assert_eq!(
        (
            received.recv().await,
            session.is_waiting(),
            backend.calls.lock().unwrap().len()
        ),
        (
            Some(AccountPoolEvent::Redeemed {
                account: "a".to_owned()
            }),
            false,
            1
        ),
    );
}

#[tokio::test(start_paused = true)]
async fn exhausted_weekly_pool_reads_again_at_hourly_probe_or_earlier_weekly_reset() {
    for (reset_after, wait) in [(5 * 24 * 3600, 3600), (120, 120)] {
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
            usage("a", /*short*/ 90.0, /*weekly*/ 0.0, now),
            usage("b", /*short*/ 90.0, /*weekly*/ 0.0, now),
        ];
        for reading in &mut readings {
            reading.windows[1].resets_at = Some(now + reset_after);
        }
        let backend = Arc::new(FakeBackend::new(readings));
        let cancel = CancellationToken::new();
        let (events, mut received) = mpsc::unbounded_channel();
        let recovery = tokio::spawn({
            let backend = Arc::clone(&backend);
            let cancel = cancel.clone();
            async move {
                session
                    .recover_with(&*backend, "model", &cancel, |event| {
                        events.send(event).unwrap();
                    })
                    .await
            }
        });
        assert_eq!(
            received.recv().await,
            Some(AccountPoolEvent::Waiting {
                account: "a".to_owned(),
                reason: PoolWaitReason::WeeklyQuota,
                next_check_at: now + wait,
            })
        );
        tokio::time::advance(Duration::from_secs((wait - 1) as u64)).await;
        tokio::task::yield_now().await;
        assert_eq!(backend.usage_calls.load(Ordering::SeqCst), 2);
        tokio::time::advance(Duration::from_secs(/*secs*/ 1)).await;
        received.recv().await.unwrap();
        assert_eq!(backend.usage_calls.load(Ordering::SeqCst), 4);
        cancel.cancel();
        assert!(recovery.await.unwrap().is_err());
        tokio::time::advance(Duration::from_secs(/*secs*/ 3600)).await;
        assert_eq!(backend.usage_calls.load(Ordering::SeqCst), 4);
        assert!(backend.calls.lock().unwrap().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn long_weekly_wait_reloads_membership_within_a_minute_and_rechecks_candidate() {
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
    let backend = Arc::new(FakeBackend::new(vec![
        usage("a", /*short*/ 90.0, /*weekly*/ 0.0, now),
        usage("b", /*short*/ 90.0, /*weekly*/ 0.0, now),
    ]));
    let (events, mut received) = mpsc::unbounded_channel();
    let recovery = tokio::spawn({
        let backend = Arc::clone(&backend);
        let session = Arc::clone(&session);
        async move {
            session
                .recover_with(&*backend, "model", &CancellationToken::new(), |event| {
                    events.send(event).unwrap();
                })
                .await
        }
    });
    assert_eq!(
        received.recv().await,
        Some(AccountPoolEvent::Waiting {
            account: "a".to_owned(),
            reason: PoolWaitReason::WeeklyQuota,
            next_check_at: now + 3600,
        })
    );
    backend.usage.lock().unwrap().insert(
        "b".to_owned(),
        usage("b", /*short*/ 90.0, /*weekly*/ 90.0, now),
    );
    store
        .put_pool(
            AccountPool {
                name: "work".to_owned(),
                accounts: vec!["b".to_owned()],
                redeem_weekly_resets: true,
            },
            /*create*/ false,
        )
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(/*secs*/ 60)).await;
    recovery.await.unwrap().unwrap();
    assert_eq!(
        (
            session.selected_account().await,
            backend.usage_calls.load(Ordering::SeqCst)
        ),
        (account("b"), 4),
    );
    assert_eq!(
        received.recv().await,
        Some(AccountPoolEvent::Switched {
            account: "b".to_owned(),
            previous_account: "a".to_owned(),
        })
    );
    assert!(!session.is_waiting());
}
