use super::*;
use crate::tests::account;
use crate::tests::populated_store;
use crate::tests::usage;
use codex_protocol::protocol::RateLimitWindow;
use pretty_assertions::assert_eq;

fn snapshot(limit_id: Option<&str>, used: f64, minutes: Option<i64>) -> RateLimitSnapshot {
    RateLimitSnapshot {
        limit_id: limit_id.map(str::to_owned),
        limit_name: None,
        normal_model_slug: None,
        primary: Some(RateLimitWindow {
            used_percent: used,
            window_minutes: minutes,
            resets_at: Some(5_000),
        }),
        secondary: Some(RateLimitWindow {
            used_percent: used / 2.0,
            window_minutes: Some(10_080),
            resets_at: Some(9_000),
        }),
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    }
}

#[test]
fn response_windows_map_like_usage_windows_and_skip_unknown_durations() {
    let windows = snapshot(None, 40.0, Some(300));
    assert_eq!(
        snapshot_windows(&windows),
        vec![
            AccountQuotaWindow {
                limit_id: "codex".to_owned(),
                model: None,
                remaining_percent: 60.0,
                window_minutes: 300,
                resets_at: Some(5_000),
            },
            AccountQuotaWindow {
                limit_id: "codex".to_owned(),
                model: None,
                remaining_percent: 80.0,
                window_minutes: 10_080,
                resets_at: Some(9_000),
            },
        ]
    );
    let unknown = snapshot_windows(&snapshot(Some("gpt-x"), 10.0, None));
    assert_eq!(unknown.len(), 1);
    assert_eq!(unknown[0].limit_id, "gpt-x");
    assert_eq!(unknown[0].window_minutes, 10_080);
}

#[tokio::test]
async fn responses_merge_by_limit_and_only_display_once_the_ordinary_limit_is_known() {
    let (_home, store) = populated_store().await;
    let account = account("a");
    record_response(
        &store,
        &account,
        &snapshot(Some("gpt-x"), 10.0, Some(300)),
        1_000,
    )
    .await;
    let stored = load_usage(&store, &account);
    assert_eq!(display_usage(&stored), None);
    assert_eq!(stored.response.as_ref().unwrap().windows.len(), 2);

    record_response(&store, &account, &snapshot(None, 50.0, Some(300)), 1_100).await;
    record_response(
        &store,
        &account,
        &snapshot(Some("gpt-x"), 30.0, Some(300)),
        1_200,
    )
    .await;
    let stored = load_usage(&store, &account);
    let response = stored.response.as_ref().unwrap();
    assert_eq!(response.checked_at, 1_200);
    assert_eq!(response.windows.len(), 4);
    let display = display_usage(&stored).unwrap();
    assert_eq!(display.ordinary_usage_allowed, None);
    assert_eq!(
        display
            .windows
            .iter()
            .filter(|window| window.limit_id == "gpt-x")
            .map(|window| window.remaining_percent)
            .collect::<Vec<_>>(),
        vec![70.0, 85.0]
    );

    // Newer response windows overlay a usage read; older ones do not.
    let mut read = usage("a", /*short*/ 20.0, /*weekly*/ 20.0, 1_150);
    read.windows.push(AccountQuotaWindow {
        limit_id: "other".to_owned(),
        model: None,
        remaining_percent: 5.0,
        window_minutes: 300,
        resets_at: None,
    });
    store_usage(
        &store,
        &account,
        &StoredUsage {
            usage: Some(read.clone()),
            usage_has_credit_details: true,
            response: stored.response.clone(),
            error_at: None,
            rejected_until: None,
        },
    );
    let display = display_usage(&load_usage(&store, &account)).unwrap();
    assert_eq!(display.checked_at, 1_200);
    assert_eq!(display.ordinary_usage_allowed, Some(true));
    assert!(
        display
            .windows
            .iter()
            .any(|window| window.limit_id == "other" && window.remaining_percent == 5.0)
    );
    assert!(
        display
            .windows
            .iter()
            .any(|window| window.limit_id == "codex" && window.remaining_percent == 75.0)
    );
    read.checked_at = 1_300;
    store_usage(
        &store,
        &account,
        &StoredUsage {
            usage: Some(read.clone()),
            usage_has_credit_details: true,
            response: load_usage(&store, &account).response,
            error_at: None,
            rejected_until: None,
        },
    );
    assert_eq!(display_usage(&load_usage(&store, &account)), Some(read));
}

#[test]
fn freshness_extends_only_for_weekly_exhaustion_before_its_reset() {
    let now = 10_000;
    let recent = usage("a", /*short*/ 50.0, /*weekly*/ 50.0, now - 30);
    assert!(fresh_enough(&recent, now, Duration::from_secs(60)));
    assert!(!fresh_enough(&recent, now, Duration::from_secs(10)));
    assert!(fresh_for_recovery(&recent, now));
    assert!(fresh_for_recovery(&recent, now + 300));
    assert!(!fresh_for_recovery(&recent, now + 400));

    let mut exhausted = usage("a", /*short*/ 50.0, /*weekly*/ 0.0, now - 1_800);
    exhausted.windows[1].resets_at = Some(now + 86_400);
    assert!(fresh_enough(&exhausted, now, Duration::from_secs(60)));
    assert!(fresh_for_recovery(&exhausted, now));
    assert!(!fresh_enough(
        &exhausted,
        now + 2_000,
        Duration::from_secs(60)
    ));
    assert!(!fresh_for_recovery(&exhausted, now + 2_000));
    assert_eq!(weekly_exhausted_until(&exhausted), Some(now + 86_400));

    // A reset that has already passed ends the extension even though it postdates the read.
    exhausted.windows[1].resets_at = Some(now - 10);
    assert_eq!(weekly_exhausted_until(&exhausted), Some(now - 10));
    assert!(!fresh_enough(&exhausted, now, Duration::from_secs(60)));
    assert!(!fresh_for_recovery(&exhausted, now));

    let mut short_only = usage("a", /*short*/ 0.0, /*weekly*/ 50.0, now - 1_800);
    short_only.windows[0].resets_at = Some(now + 600);
    assert!(!fresh_enough(&short_only, now, Duration::from_secs(60)));

    let mut failed = usage("a", /*short*/ 50.0, /*weekly*/ 0.0, now - 10);
    failed.error = Some("down".to_owned());
    assert!(!fresh_enough(&failed, now, Duration::from_secs(60)));
    let future = usage("a", /*short*/ 50.0, /*weekly*/ 50.0, now + 10);
    assert!(!fresh_enough(&future, now, Duration::from_secs(60)));
}

#[test]
fn cached_model_support_trusts_recent_catalogs_and_rechecks_old_misses() {
    let now = 100_000;
    let slugs = StoredModelSlugs {
        fetched_at: now - 3_600,
        client_version: "v".to_owned(),
        slugs: vec!["known".to_owned()],
    };
    assert_eq!(
        cached_model_support(Some(&slugs), "known", "v", now),
        Some(true)
    );
    assert_eq!(cached_model_support(Some(&slugs), "new", "v", now), None);
    assert_eq!(
        cached_model_support(Some(&slugs), "new", "v", slugs.fetched_at + 500),
        Some(false)
    );
    assert_eq!(
        cached_model_support(Some(&slugs), "known", "other", now),
        None
    );
    assert_eq!(
        cached_model_support(Some(&slugs), "known", "v", now + 24 * 3_600),
        None
    );
    assert_eq!(cached_model_support(None, "known", "v", now), None);
}

#[tokio::test]
async fn sparse_snapshots_replace_only_the_windows_they_report() {
    let (_home, store) = populated_store().await;
    let account = account("a");
    record_response(&store, &account, &snapshot(None, 50.0, Some(300)), 1_000).await;
    let mut primary_only = snapshot(None, 80.0, Some(300));
    primary_only.secondary = None;
    record_response(&store, &account, &primary_only, 1_100).await;
    let response = load_usage(&store, &account).response.unwrap();
    assert_eq!(
        response
            .windows
            .iter()
            .map(|window| (window.window_minutes, window.remaining_percent))
            .collect::<Vec<_>>(),
        vec![(10_080, 75.0), (300, 20.0)]
    );
}

#[test]
fn observations_go_stale_once_one_of_their_windows_resets() {
    let now = 10_000;
    let mut usage = usage("a", /*short*/ 0.0, /*weekly*/ 50.0, now - 100);
    usage.windows[0].resets_at = Some(now - 50);
    usage.windows[1].resets_at = Some(now + 86_400);
    assert!(reset_since(&usage, now));
    assert!(!fresh_enough(&usage, now, Duration::from_secs(600)));
    assert!(!fresh_for_recovery(&usage, now));
    assert!(!reset_since(&usage, now - 60));
    assert!(fresh_enough(&usage, now - 60, Duration::from_secs(600)));
    // A reset of a window that still had quota only adds capacity.
    usage.windows[0].remaining_percent = 40.0;
    assert!(!reset_since(&usage, now));
    assert!(fresh_enough(&usage, now, Duration::from_secs(600)));
    assert!(fresh_for_recovery(&usage, now));
    // Weekly exhaustion keeps an observation known through a short-window reset.
    let mut exhausted =
        crate::tests::usage("a", /*short*/ 0.0, /*weekly*/ 0.0, now - 1_000);
    exhausted.windows[0].resets_at = Some(now - 500);
    exhausted.windows[1].resets_at = Some(now + 86_400);
    assert!(fresh_for_recovery(&exhausted, now));
    assert!(fresh_enough(&exhausted, now, Duration::from_secs(60)));
}
