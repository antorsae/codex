use super::*;
use crate::config_types::SERVICE_TIER_ULTRAFAST_REQUEST_VALUE;
use pretty_assertions::assert_eq;

fn model_with_tiers(tiers: &[&str]) -> ModelInfo {
    ModelInfo {
        service_tiers: tiers
            .iter()
            .map(|id| ModelServiceTier {
                id: (*id).to_string(),
                name: (*id).to_string(),
                description: String::new(),
            })
            .collect(),
        ..super::tests::test_model(/*spec*/ None)
    }
}

#[test]
fn unsupported_speed_tiers_fall_back_to_the_next_slower_supported_tier() {
    let fast = ServiceTier::Fast.request_value();
    let ultrafast = SERVICE_TIER_ULTRAFAST_REQUEST_VALUE;
    let flex = ServiceTier::Flex.request_value();
    let cases = [
        (vec![fast, ultrafast], Some(ultrafast), Some(ultrafast)),
        (vec![fast], Some(ultrafast), Some(fast)),
        (vec![], Some(ultrafast), None),
        (vec![fast], Some(fast), Some(fast)),
        (vec![], Some(fast), None),
        (vec![], Some(flex), Some(flex)),
        (vec![fast], Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE), None),
        (vec![fast], Some("unknown"), None),
        (vec![fast], None, None),
    ];

    let actual: Vec<_> = cases
        .iter()
        .map(|(tiers, requested, _)| {
            model_with_tiers(tiers).service_tier_for_request(requested.map(str::to_string))
        })
        .collect();
    let expected: Vec<_> = cases
        .iter()
        .map(|(_, _, expected)| expected.map(str::to_string))
        .collect();
    assert_eq!(actual, expected);
}
