#![cfg(not(debug_assertions))]

// SANDBOX PATCH: Config unused when get_upgrade_version returns early
#[allow(unused_imports)]
use crate::legacy_core::config::Config;
#[allow(unused_imports)]
use crate::npm_registry;
#[allow(unused_imports)]
use crate::npm_registry::NpmPackageInfo;
#[allow(unused_imports)]
use crate::update_action;
#[allow(unused_imports)]
use crate::update_action::UpdateAction;
#[allow(unused_imports)]
use crate::update_versions::extract_version_from_latest_tag;
#[allow(unused_imports)]
use crate::update_versions::is_newer;
use crate::update_versions::is_source_build_version;
use chrono::DateTime;
#[allow(unused_imports)]
use chrono::Duration;
use chrono::Utc;
use codex_login::default_client::create_client;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;

#[allow(unused_imports)]
use crate::version::CODEX_CLI_VERSION;

// SANDBOX PATCH: version update check disabled — no HTTP requests to GitHub releases API
pub fn get_upgrade_version(_config: &Config) -> Option<String> {
    None
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct VersionInfo {
    latest_version: String,
    // ISO-8601 timestamp (RFC3339)
    last_checked_at: DateTime<Utc>,
    #[serde(default)]
    dismissed_version: Option<String>,
}

const VERSION_FILENAME: &str = "version.json";
// SANDBOX PATCH: this entire module is release-only (`#![cfg(not(debug_assertions))]`) and
// dead (`get_upgrade_version` returns None). The upstream release API and the Homebrew cask
// API are redirected/neutralized to the fork's GitHub releases API so no upstream
// install/update references remain. The fork has no Homebrew cask.
#[allow(dead_code)]
const HOMEBREW_CASK_API_URL: &str = "https://api.github.com/repos/gim-home/codex/releases/latest";
#[allow(dead_code)]
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/gim-home/codex/releases/latest";

#[allow(dead_code)]
#[derive(Deserialize, Debug, Clone)]
struct ReleaseInfo {
    tag_name: String,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug, Clone)]
struct HomebrewCaskInfo {
    version: String,
}

fn version_filepath(config: &Config) -> PathBuf {
    config.codex_home.join(VERSION_FILENAME).into_path_buf()
}

fn read_version_info(version_file: &Path) -> anyhow::Result<VersionInfo> {
    let contents = std::fs::read_to_string(version_file)?;
    Ok(serde_json::from_str(&contents)?)
}

#[allow(dead_code)]
async fn check_for_update(version_file: &Path, action: Option<UpdateAction>) -> anyhow::Result<()> {
    let latest_version = match action {
        Some(UpdateAction::BrewUpgrade) => {
            let HomebrewCaskInfo { version } = create_client()
                .get(HOMEBREW_CASK_API_URL)
                .send()
                .await?
                .error_for_status()?
                .json::<HomebrewCaskInfo>()
                .await?;
            version
        }
        Some(UpdateAction::NpmGlobalLatest) | Some(UpdateAction::BunGlobalLatest) => {
            let latest_version = fetch_latest_github_release_version().await?;
            let package_info = create_client()
                .get(npm_registry::PACKAGE_URL)
                .send()
                .await?
                .error_for_status()?
                .json::<NpmPackageInfo>()
                .await?;
            npm_registry::ensure_version_ready(&package_info, &latest_version)?;
            latest_version
        }
        Some(UpdateAction::StandaloneUnix) | Some(UpdateAction::StandaloneWindows) | None => {
            fetch_latest_github_release_version().await?
        }
    };

    // Preserve any previously dismissed version if present.
    let prev_info = read_version_info(version_file).ok();
    let info = VersionInfo {
        latest_version,
        last_checked_at: Utc::now(),
        dismissed_version: prev_info.and_then(|p| p.dismissed_version),
    };

    let json_line = format!("{}\n", serde_json::to_string(&info)?);
    if let Some(parent) = version_file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(version_file, json_line).await?;
    Ok(())
}

#[allow(dead_code)]
async fn fetch_latest_github_release_version() -> anyhow::Result<String> {
    let ReleaseInfo {
        tag_name: latest_tag_name,
    } = create_client()
        .get(LATEST_RELEASE_URL)
        .send()
        .await?
        .error_for_status()?
        .json::<ReleaseInfo>()
        .await?;
    extract_version_from_latest_tag(&latest_tag_name)
}

pub async fn dismiss_version(config: &Config, version: &str) -> anyhow::Result<()> {
    let version_file = version_filepath(config);
    let mut info = match read_version_info(&version_file) {
        Ok(info) => info,
        Err(_) => return Ok(()),
    };
    info.dismissed_version = Some(version.to_string());
    let json_line = format!("{}\n", serde_json::to_string(&info)?);
    if let Some(parent) = version_file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(version_file, json_line).await?;
    Ok(())
}

/// Returns the latest version to show in a popup, if it should be shown.
/// This respects the user's dismissal choice for the current latest version.
pub fn get_upgrade_version_for_popup(config: &Config) -> Option<String> {
    if !config.check_for_update_on_startup || is_source_build_version(CODEX_CLI_VERSION) {
        return None;
    }

    let version_file = version_filepath(config);
    let latest = get_upgrade_version(config)?;
    // If the user dismissed this exact version previously, do not show the popup.
    if let Ok(info) = read_version_info(&version_file)
        && info.dismissed_version.as_deref() == Some(latest.as_str())
    {
        return None;
    }
    Some(latest)
}
