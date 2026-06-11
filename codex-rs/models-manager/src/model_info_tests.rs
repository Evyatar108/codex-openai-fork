use super::*;
use crate::ModelsManagerConfig;
use codex_protocol::openai_models::ContextWindowTier;
use pretty_assertions::assert_eq;

#[test]
fn reasoning_summaries_override_true_enables_support() {
    let model = model_info_from_slug("unknown-model");
    let config = ModelsManagerConfig {
        model_supports_reasoning_summaries: Some(true),
        ..Default::default()
    };

    let updated = with_config_overrides(model.clone(), &config);
    let mut expected = model;
    expected.supports_reasoning_summaries = true;

    assert_eq!(updated, expected);
}

#[test]
fn reasoning_summaries_override_false_does_not_disable_support() {
    let mut model = model_info_from_slug("unknown-model");
    model.supports_reasoning_summaries = true;
    let config = ModelsManagerConfig {
        model_supports_reasoning_summaries: Some(false),
        ..Default::default()
    };

    let updated = with_config_overrides(model.clone(), &config);

    assert_eq!(updated, model);
}

#[test]
fn reasoning_summaries_override_false_is_noop_when_model_is_false() {
    let model = model_info_from_slug("unknown-model");
    let config = ModelsManagerConfig {
        model_supports_reasoning_summaries: Some(false),
        ..Default::default()
    };

    let updated = with_config_overrides(model.clone(), &config);

    assert_eq!(updated, model);
}

#[test]
fn model_context_window_override_clamps_to_max_context_window() {
    let mut model = model_info_from_slug("unknown-model");
    model.context_window = Some(273_000);
    model.max_context_window = Some(400_000);
    let config = ModelsManagerConfig {
        model_context_window: Some(500_000),
        ..Default::default()
    };

    let updated = with_config_overrides(model.clone(), &config);
    let mut expected = model;
    expected.context_window = Some(400_000);

    assert_eq!(updated, expected);
}

#[test]
fn model_context_window_uses_model_value_without_override() {
    let mut model = model_info_from_slug("unknown-model");
    model.context_window = Some(273_000);
    model.max_context_window = Some(400_000);
    let config = ModelsManagerConfig::default();

    let updated = with_config_overrides(model.clone(), &config);

    assert_eq!(updated, model);
}

#[test]
fn context_tier_long_context_widens_to_full() {
    let mut model = model_info_from_slug("gpt-5.5");
    model.context_window = Some(400_000);
    model.max_context_window = Some(1_050_000);
    let config = ModelsManagerConfig {
        model_context_tier: Some(ContextWindowTier::LongContext),
        ..Default::default()
    };

    let updated = with_config_overrides(model, &config);

    assert_eq!(updated.context_window, Some(1_050_000));
    assert_eq!(updated.max_context_window, Some(1_050_000));
}

#[test]
fn context_tier_default_keeps_default_window() {
    let mut model = model_info_from_slug("gpt-5.5");
    model.context_window = Some(400_000);
    model.max_context_window = Some(1_050_000);
    let config = ModelsManagerConfig {
        model_context_tier: Some(ContextWindowTier::Default),
        ..Default::default()
    };

    let updated = with_config_overrides(model, &config);

    assert_eq!(updated.context_window, Some(400_000));
}

#[test]
fn stale_long_context_does_not_widen_single_tier_model() {
    // Finding-3: a persisted `long_context` must never widen a single-tier model.
    let mut model = model_info_from_slug("claude-sonnet-4.5");
    model.context_window = Some(200_000);
    model.max_context_window = Some(200_000);
    let config = ModelsManagerConfig {
        model_context_tier: Some(ContextWindowTier::LongContext),
        ..Default::default()
    };

    let updated = with_config_overrides(model, &config);

    assert_eq!(updated.context_window, Some(200_000));
    assert_eq!(updated.max_context_window, Some(200_000));
}

#[test]
fn context_tier_applied_before_numeric_clamp() {
    // long_context widens to the full ceiling, then the numeric override clamps it.
    let mut model = model_info_from_slug("gpt-5.5");
    model.context_window = Some(400_000);
    model.max_context_window = Some(1_050_000);
    let config = ModelsManagerConfig {
        model_context_tier: Some(ContextWindowTier::LongContext),
        model_context_window: Some(500_000),
        ..Default::default()
    };

    let updated = with_config_overrides(model, &config);

    assert_eq!(updated.context_window, Some(500_000));
}

#[test]
fn stale_long_context_noop_when_no_max_context_window() {
    // A model without a distinct max ceiling cannot expose tiers, so the tier
    // selection resolves to the single window and does not widen it.
    let mut model = model_info_from_slug("gpt-x");
    model.context_window = Some(272_000);
    model.max_context_window = None;
    let config = ModelsManagerConfig {
        model_context_tier: Some(ContextWindowTier::LongContext),
        ..Default::default()
    };

    let updated = with_config_overrides(model, &config);

    assert_eq!(updated.context_window, Some(272_000));
}
