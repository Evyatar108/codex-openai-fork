use std::path::PathBuf;

use anyhow::Context;

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub app_dir: PathBuf,
    pub github_token_path: PathBuf,
    pub copilot_token_path: PathBuf,
    pub device_id_path: PathBuf,
    pub machine_id_path: PathBuf,
}

impl AppPaths {
    pub fn from_env() -> anyhow::Result<Self> {
        let app_dir = match std::env::var_os("COPILOT_API_HOME") {
            Some(value) => PathBuf::from(value),
            None => {
                let home = dirs::home_dir().context("home directory not found")?;
                home.join(".local").join("share").join("copilot-api")
            }
        };

        Ok(Self {
            github_token_path: app_dir.join("github_token"),
            copilot_token_path: app_dir.join("copilot_token"),
            device_id_path: app_dir.join("device_id"),
            machine_id_path: app_dir.join("machine_id"),
            app_dir,
        })
    }
}
