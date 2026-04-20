use anyhow::Result;
use codex_config::CONFIG_TOML_FILE;
use codex_core::config::Config;
use codex_core::config::edit::ConfigEditsBuilder;
use codex_core::config::find_codex_home;
use codex_features::FEATURES;
use codex_features::Stage;
use codex_features::is_known_feature_key;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeatureRow {
    pub name: &'static str,
    pub stage: &'static str,
    pub enabled: bool,
}

pub fn build_feature_rows(config: &Config) -> Vec<FeatureRow> {
    let mut rows = FEATURES
        .iter()
        .map(|feature| FeatureRow {
            name: feature.key,
            stage: stage_str(feature.stage),
            enabled: config.features.enabled(feature.id),
        })
        .collect::<Vec<_>>();
    rows.sort_unstable_by_key(|row| row.name);
    rows
}

pub async fn enable_feature(feature: &str, profile: Option<&str>) -> Result<Option<String>> {
    let codex_home = find_codex_home()?;
    enable_feature_in_codex_home(&codex_home, profile, feature).await
}

pub async fn enable_feature_in_codex_home(
    codex_home: &Path,
    profile: Option<&str>,
    feature: &str,
) -> Result<Option<String>> {
    validate_feature(feature)?;
    ConfigEditsBuilder::new(codex_home)
        .with_profile(profile)
        .set_feature_enabled(feature, /*enabled*/ true)
        .apply()
        .await?;
    Ok(under_development_feature_warning(
        codex_home, profile, feature,
    ))
}

pub async fn disable_feature(feature: &str, profile: Option<&str>) -> Result<()> {
    let codex_home = find_codex_home()?;
    disable_feature_in_codex_home(&codex_home, profile, feature).await
}

pub async fn disable_feature_in_codex_home(
    codex_home: &Path,
    profile: Option<&str>,
    feature: &str,
) -> Result<()> {
    validate_feature(feature)?;
    ConfigEditsBuilder::new(codex_home)
        .with_profile(profile)
        .set_feature_enabled(feature, /*enabled*/ false)
        .apply()
        .await?;
    Ok(())
}

fn under_development_feature_warning(
    codex_home: &Path,
    profile: Option<&str>,
    feature: &str,
) -> Option<String> {
    if profile.is_some() {
        return None;
    }

    let spec = FEATURES.iter().find(|spec| spec.key == feature)?;
    if !matches!(spec.stage, Stage::UnderDevelopment) {
        return None;
    }

    let config_path = codex_home.join(CONFIG_TOML_FILE);
    Some(format!(
        "Under-development features enabled: {feature}. Under-development features are incomplete and may behave unpredictably. To suppress this warning, set `suppress_unstable_features_warning = true` in {}.",
        config_path.display()
    ))
}

fn stage_str(stage: Stage) -> &'static str {
    match stage {
        Stage::UnderDevelopment => "under development",
        Stage::Experimental { .. } => "experimental",
        Stage::Stable => "stable",
        Stage::Deprecated => "deprecated",
        Stage::Removed => "removed",
    }
}

fn validate_feature(feature: &str) -> Result<()> {
    anyhow::ensure!(
        is_known_feature_key(feature),
        "unknown feature `{feature}`. Run `codex features list` to see available options."
    );
    Ok(())
}
