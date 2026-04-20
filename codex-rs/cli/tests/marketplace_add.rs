use anyhow::Result;
use codex_config::CONFIG_TOML_FILE;
use codex_core::plugins::MarketplaceAddRequest;
use codex_core::plugins::add_marketplace;
use codex_core::plugins::marketplace_install_root;
use pretty_assertions::assert_eq;
use std::path::Path;
use tempfile::TempDir;

fn write_marketplace_source(source: &Path, marker: &str) -> Result<()> {
    std::fs::create_dir_all(source.join(".agents/plugins"))?;
    std::fs::create_dir_all(source.join("plugins/sample/.codex-plugin"))?;
    std::fs::write(
        source.join(".agents/plugins/marketplace.json"),
        r#"{
  "name": "debug",
  "plugins": [
    {
      "name": "sample",
      "source": {
        "source": "local",
        "path": "./plugins/sample"
      }
    }
  ]
}"#,
    )?;
    std::fs::write(
        source.join("plugins/sample/.codex-plugin/plugin.json"),
        r#"{"name":"sample"}"#,
    )?;
    std::fs::write(source.join("plugins/sample/marker.txt"), marker)?;
    Ok(())
}

#[tokio::test]
async fn marketplace_add_local_directory_source() -> Result<()> {
    let codex_home = TempDir::new()?;
    let source = TempDir::new()?;
    write_marketplace_source(source.path(), "local ref")?;
    add_marketplace(
        codex_home.path().to_path_buf(),
        MarketplaceAddRequest {
            source: source.path().display().to_string(),
            ref_name: None,
            sparse_paths: Vec::new(),
        },
    )
    .await?;

    let installed_root = marketplace_install_root(codex_home.path()).join("debug");
    assert!(!installed_root.exists());

    let config = std::fs::read_to_string(codex_home.path().join(CONFIG_TOML_FILE))?;
    let config: toml::Value = toml::from_str(&config)?;
    let expected_source = source.path().canonicalize()?.display().to_string();
    assert_eq!(
        config["marketplaces"]["debug"]["source_type"].as_str(),
        Some("local")
    );
    assert_eq!(
        config["marketplaces"]["debug"]["source"].as_str(),
        Some(expected_source.as_str())
    );

    Ok(())
}

#[tokio::test]
async fn marketplace_add_rejects_local_manifest_file_source() -> Result<()> {
    let codex_home = TempDir::new()?;
    let source = TempDir::new()?;
    write_marketplace_source(source.path(), "local ref")?;
    let manifest_path = source.path().join(".agents/plugins/marketplace.json");

    let err = add_marketplace(
        codex_home.path().to_path_buf(),
        MarketplaceAddRequest {
            source: manifest_path.display().to_string(),
            ref_name: None,
            sparse_paths: Vec::new(),
        },
    )
    .await
    .expect_err("manifest file should be rejected");
    assert!(
        err.to_string()
            .contains("local marketplace source must be a directory, not a file")
    );

    Ok(())
}
