use codex_protocol::account_pool::BankedReset;
use codex_protocol::account_pool::ManagedAccountUsage;
use codex_protocol::account_pool::PoolWaitReason;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowKind {
    Weekly,
    Short,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Use(usize),
    Redeem(usize, String),
    Wait(PoolWaitReason, i64),
}

fn applicable(
    usage: &ManagedAccountUsage,
    limit: &codex_protocol::account_pool::AccountQuotaWindow,
) -> bool {
    limit.limit_id == "codex"
        || usage
            .model
            .as_ref()
            .is_none_or(|model| limit.model.as_ref() == Some(model) || &limit.limit_id == model)
}

pub(crate) fn known(usage: &ManagedAccountUsage, now: i64) -> bool {
    usage.error.is_none()
        && (0..=90).contains(&(now - usage.checked_at))
        && usage.ordinary_usage_allowed.is_some()
        && (usage.model.is_none() || usage.model_supported == Some(true))
        // Some plans expose only one ordinary quota window. Validate the windows
        // the backend reports instead of requiring both a short and a weekly one.
        && usage
            .windows
            .iter()
            .any(|window| window.limit_id == "codex")
        && usage
            .windows
            .iter()
            .filter(|window| applicable(usage, window))
            .all(|window| {
                window.window_minutes > 0
                    && window.remaining_percent.is_finite()
                    && (0.0..=100.0).contains(&window.remaining_percent)
            })
}

pub(crate) fn blocked(usage: &ManagedAccountUsage, kind: WindowKind) -> bool {
    usage
        .windows
        .iter()
        .filter(|window| applicable(usage, window))
        .any(|window| {
            window.remaining_percent <= 0.0
                && match kind {
                    WindowKind::Weekly => window.window_minutes > 24 * 60,
                    WindowKind::Short => window.window_minutes <= 24 * 60,
                }
        })
}

pub(crate) fn usable(usage: &ManagedAccountUsage, now: i64) -> bool {
    known(usage, now)
        && usage.ordinary_usage_allowed == Some(true)
        && !blocked(usage, WindowKind::Weekly)
        && !blocked(usage, WindowKind::Short)
}

pub(crate) fn earliest_credit(usage: &ManagedAccountUsage, now: i64) -> Option<&BankedReset> {
    usage
        .resets
        .as_ref()?
        .iter()
        .filter(|credit| credit.expires_at.is_none_or(|expiry| expiry > now))
        .min_by_key(|credit| credit.expires_at.unwrap_or(i64::MAX))
}

pub(crate) fn decide(
    usages: &[ManagedAccountUsage],
    current: usize,
    trigger: WindowKind,
    redeem_weekly: bool,
    now: i64,
) -> Decision {
    if usages.get(current).is_some_and(|usage| usable(usage, now)) {
        return Decision::Use(current);
    }
    let mut best = None;
    let mut most_remaining = -1.0;
    for (index, usage) in usages.iter().enumerate() {
        if !usable(usage, now) {
            continue;
        }
        let remaining = usage
            .windows
            .iter()
            .filter(|window| applicable(usage, window))
            .filter(|window| match trigger {
                WindowKind::Weekly => window.window_minutes > 24 * 60,
                WindowKind::Short => window.window_minutes <= 24 * 60,
            })
            .map(|window| window.remaining_percent)
            .fold(100.0, f64::min);
        if remaining > most_remaining {
            most_remaining = remaining;
            best = Some(index);
        }
    }
    if let Some(index) = best {
        return Decision::Use(index);
    }
    let eligible: Vec<_> = usages
        .iter()
        .enumerate()
        .filter(|(_, usage)| usage.model_supported != Some(false))
        .collect();
    let unknown = eligible.is_empty() || eligible.iter().any(|(_, usage)| !known(usage, now));
    let all_weekly = !unknown
        && eligible
            .iter()
            .all(|(_, usage)| blocked(usage, WindowKind::Weekly));
    if all_weekly && redeem_weekly {
        let mut candidate = None;
        let mut count = 0;
        for (index, usage) in &eligible {
            if let Some(credit) = earliest_credit(usage, now)
                && let Some(available) = usage.available_resets
                && available > count
            {
                count = available;
                candidate = Some((*index, credit.id.clone()));
            }
        }
        if let Some((index, credit)) = candidate {
            return Decision::Redeem(index, credit);
        }
    }
    let reason = if unknown {
        PoolWaitReason::UnknownAvailability
    } else if all_weekly {
        PoolWaitReason::WeeklyQuota
    } else {
        PoolWaitReason::ShortWindow
    };
    let next = usages
        .iter()
        .flat_map(|usage| &usage.windows)
        .filter_map(|window| window.resets_at)
        .filter(|reset| *reset > now)
        .min()
        .unwrap_or(now + 60)
        .min(now + 60);
    Decision::Wait(reason, next)
}
