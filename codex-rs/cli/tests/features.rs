use anyhow::Result;
use codex_cli::build_feature_rows;
use codex_cli::disable_feature_in_codex_home;
use codex_cli::enable_feature_in_codex_home;
use codex_config::CONFIG_TOML_FILE;
use codex_core::config::Config;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[tokio::test]
async fn features_enable_writes_feature_flag_to_config() -> Result<()> {
    let codex_home = TempDir::new()?;

    let warning = enable_feature_in_codex_home(codex_home.path(), None, "unified_exec").await?;
    assert_eq!(warning, None);

    let config = std::fs::read_to_string(codex_home.path().join(CONFIG_TOML_FILE))?;
    assert!(config.contains("[features]"));
    assert!(config.contains("unified_exec = true"));

    Ok(())
}

#[tokio::test]
async fn features_disable_writes_feature_flag_to_config() -> Result<()> {
    let codex_home = TempDir::new()?;

    disable_feature_in_codex_home(codex_home.path(), None, "shell_tool").await?;

    let config = std::fs::read_to_string(codex_home.path().join(CONFIG_TOML_FILE))?;
    assert!(config.contains("[features]"));
    assert!(config.contains("shell_tool = false"));

    Ok(())
}

#[tokio::test]
async fn features_enable_under_development_feature_prints_warning() -> Result<()> {
    let codex_home = TempDir::new()?;

    let warning = enable_feature_in_codex_home(codex_home.path(), None, "runtime_metrics").await?;
    assert_eq!(
        warning,
        Some(format!(
            "Under-development features enabled: runtime_metrics. Under-development features are incomplete and may behave unpredictably. To suppress this warning, set `suppress_unstable_features_warning = true` in {}.",
            codex_home.path().join(CONFIG_TOML_FILE).display()
        ))
    );

    Ok(())
}

#[tokio::test]
async fn features_list_is_sorted_alphabetically_by_feature_name() -> Result<()> {
    let codex_home = TempDir::new()?;

    let config = Config::load_default_with_cli_overrides_for_codex_home(
        codex_home.path().to_path_buf(),
        vec![],
    )?;
    let actual_names = build_feature_rows(&config)
        .into_iter()
        .map(|row| row.name.to_string())
        .collect::<Vec<_>>();
    let mut expected_names = actual_names.clone();
    expected_names.sort();

    assert_eq!(actual_names, expected_names);

    Ok(())
}
