use super::*;
use pretty_assertions::assert_eq;

const NOW: i64 = 1_800_000_000;

fn usage(alias: &str, remaining: f64, checked_at: i64) -> ManagedAccountUsage {
    ManagedAccountUsage {
        account: ManagedAccount {
            alias: alias.to_owned(),
            user_id: format!("user-{alias}"),
            workspace_id: format!("workspace-{alias}"),
            email: None,
            plan: None,
        },
        pools: vec!["work".to_owned()],
        model: None,
        model_supported: None,
        ordinary_usage_allowed: Some(remaining > 0.0),
        windows: vec![AccountQuotaWindow {
            limit_id: "codex".to_owned(),
            model: None,
            remaining_percent: remaining,
            window_minutes: 7 * 24 * 60,
            resets_at: Some(NOW + 2 * 86_400),
        }],
        available_resets: Some(2),
        resets: None,
        checked_at,
        error: None,
    }
}

#[test]
fn schedules_selected_inactive_and_exhausted_accounts_and_backs_off_errors() {
    let mut cache = AccountStatusCache::default();
    let mut observations = vec![
        usage("active", 50.0, NOW),
        usage("inactive", 50.0, NOW),
        usage("empty", 0.0, NOW),
    ];
    let mut unavailable = usage("unknown", 50.0, NOW);
    unavailable.error = Some("offline".to_owned());
    observations.push(unavailable);
    for observation in &observations {
        cache.record(observation.account.clone(), Some(observation.clone()), NOW);
    }
    let due: Vec<_> = [59, 60, 300, 600, 3600]
        .into_iter()
        .map(|elapsed| {
            observations
                .iter()
                .filter(|observation| cache.due(&observation.account, "active", NOW + elapsed))
                .map(|observation| observation.account.alias.as_str())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(
        due,
        vec![
            vec![],
            vec!["active"],
            vec!["active", "unknown"],
            vec!["active", "inactive", "unknown"],
            vec!["active", "inactive", "empty", "unknown"]
        ]
    );
    // Selecting an inactive account accelerates its checks; selecting an exhausted one does not.
    assert!(cache.due(&observations[1].account, "inactive", NOW + 60));
    assert!(!cache.due(&observations[2].account, "empty", NOW + 60));
    assert!(cache.due(&observations[3].account, "unknown", NOW + 60));
}

#[test]
fn hourly_exhausted_cache_preserves_timestamps_and_expires_at_reset() {
    let mut cache = AccountStatusCache::default();
    let mut exhausted = usage("empty", 0.0, NOW);
    exhausted.windows[0].resets_at = Some(NOW + 1800);
    cache.record(exhausted.account.clone(), Some(exhausted.clone()), NOW);
    let entry = &cache.entries["empty"];
    assert_eq!(
        (
            entry.refresh_at("empty"),
            entry.valid_until(),
            entry.usage.clone()
        ),
        (NOW + 1800, Some(NOW + 1800), Some(exhausted))
    );
    let mut unknown_reset = usage("missing", 0.0, NOW);
    unknown_reset.windows[0].resets_at = None;
    cache.record(unknown_reset.account.clone(), Some(unknown_reset), NOW);
    assert_eq!(
        (
            cache.entries["missing"].refresh_at("empty"),
            cache.entries["missing"].valid_until()
        ),
        (NOW + 600, Some(NOW + 900))
    );
    let future = usage("future", 50.0, NOW + 1);
    cache.record(future.account.clone(), Some(future), NOW);
    assert_eq!(cache.entries["future"].valid_until(), None);
}

#[test]
fn unexpected_reset_sweeps_are_coalesced_and_exclude_scheduled_or_redeemed_resets() {
    let mut cache = AccountStatusCache::default();
    for alias in ["a", "b", "c"] {
        let old = usage(alias, 0.0, NOW);
        cache.record(old.account.clone(), Some(old), NOW);
    }
    let early = usage("a", 100.0, NOW + 60);
    assert!(cache.record(early.account.clone(), Some(early), NOW + 60));
    cache.last_reset_sweep = Some(NOW + 60);
    let second = usage("b", 100.0, NOW + 120);
    assert!(!cache.record(second.account.clone(), Some(second), NOW + 120));
    let later = usage("c", 100.0, NOW + 660);
    assert!(cache.record(later.account.clone(), Some(later), NOW + 660));

    for (alias, reset, credits) in [("scheduled", NOW + 60, 2), ("redeemed", NOW + 86_400, 1)] {
        let mut old = usage(alias, 0.0, NOW);
        old.windows[0].resets_at = Some(reset);
        cache.record(old.account.clone(), Some(old), NOW);
        let mut new = usage(alias, 100.0, NOW + 1200);
        new.available_resets = Some(credits);
        assert!(!cache.record(new.account.clone(), Some(new), NOW + 1200));
    }
}

#[test]
fn warm_pool_of_twenty_five_accounts_needs_eighty_four_hourly_reads() {
    let mut cache = AccountStatusCache::default();
    let observations: Vec<_> = (0..25)
        .map(|index| usage(&index.to_string(), if index == 0 { 50.0 } else { 0.0 }, NOW))
        .collect();
    for observation in &observations {
        cache.record(observation.account.clone(), Some(observation.clone()), NOW);
    }
    let mut requests = 0;
    for minute in 1..=60 {
        let now = NOW + minute * 60;
        for observation in &observations {
            if cache.due(&observation.account, "0", now) {
                requests += 1;
                let mut refreshed = observation.clone();
                refreshed.checked_at = now;
                cache.record(refreshed.account.clone(), Some(refreshed), now);
            }
        }
    }
    assert_eq!(requests, 84);
    assert_eq!(cache.entries.len(), 25);
}

#[tokio::test]
async fn replacing_footer_work_cancels_the_previous_task() {
    let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
    let mut pending = AccountStatusTask(Some(tokio::spawn(async move {
        let _sender = sender;
        std::future::pending::<()>().await;
    })));
    assert!(pending.0.is_some());
    pending = AccountStatusTask::default();
    assert!(receiver.await.is_err());
    assert!(pending.0.is_none());
}

#[test]
fn cancelled_reset_sweep_resumes_remaining_members_without_restarting_completed_batches() {
    let mut cache = AccountStatusCache::default();
    let observations: Vec<_> = (0..25)
        .map(|index| usage(&index.to_string(), 0.0, NOW))
        .collect();
    for observation in &observations {
        cache.record(observation.account.clone(), Some(observation.clone()), NOW);
    }
    cache.begin_reset_sweep(&HashSet::new(), NOW + 60);
    for observation in &observations[..20] {
        let mut refreshed = observation.clone();
        refreshed.checked_at = NOW + 60;
        cache.record(refreshed.account.clone(), Some(refreshed), NOW + 60);
    }
    // A cancelled worker leaves no local scheduling state; only unfinished entries remain due.
    let remaining: Vec<_> = observations
        .iter()
        .filter(|observation| cache.due(&observation.account, "0", NOW + 120))
        .map(|observation| observation.account.alias.as_str())
        .collect();
    assert_eq!(remaining, vec!["20", "21", "22", "23", "24"]);
}

#[test]
fn manual_footer_invalidations_are_bounded_and_survive_interrupted_refresh() {
    let mut cache = AccountStatusCache::default();
    let observations = [usage("active", 50.0, NOW), usage("empty", 0.0, NOW)];
    for observation in &observations {
        cache.record(observation.account.clone(), Some(observation.clone()), NOW);
    }
    let mut pending = AccountStatusInvalidations::default();
    pending.extend(["empty"].into_iter());
    cache.invalidate(std::mem::take(&mut pending));
    // Draining the mailbox persists work before awaiting a request that could be cancelled.
    assert_eq!(pending, AccountStatusInvalidations::default());
    assert_eq!(
        observations
            .iter()
            .map(|observation| cache.due(&observation.account, "active", NOW + 1))
            .collect::<Vec<_>>(),
        [false, true]
    );
    let mut refreshed = observations[1].clone();
    refreshed.checked_at = NOW + 1;
    cache.record(refreshed.account.clone(), Some(refreshed), NOW + 1);
    assert!(!cache.due(&observations[1].account, "active", NOW + 2));

    let aliases: Vec<_> = (0..130).map(|index| index.to_string()).collect();
    pending.extend(aliases.iter().map(String::as_str));
    assert_eq!(
        pending,
        AccountStatusInvalidations {
            aliases: HashSet::new(),
            all_accounts: true
        }
    );
    cache.invalidate(pending);
    assert!(observations.iter().all(|observation| cache.due(
        &observation.account,
        "active",
        NOW + 2
    )));
}
