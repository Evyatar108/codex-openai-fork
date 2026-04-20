use anyhow::Result;
use clap::Parser;
use codex_cli::mcp_cmd::GetArgs;
use codex_cli::mcp_cmd::ListArgs;
use codex_cli::mcp_cmd::McpCli;
use codex_cli::mcp_cmd::McpSubcommand;
use codex_cli::mcp_cmd::add_server;
use codex_cli::mcp_cmd::get_server;
use codex_cli::mcp_cmd::list_servers;
use codex_config::types::McpServerTransportConfig;
use codex_core::config::Config;
use codex_core::config::edit::ConfigEditsBuilder;
use codex_core::config::load_global_mcp_servers;
use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::path::Path;
use std::sync::Mutex;
use std::sync::OnceLock;
use tempfile::TempDir;

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

async fn load_config_for_codex_home(codex_home: &Path) -> Result<Config> {
    let _guard = env_lock().lock().expect("env mutex poisoned");
    unsafe {
        std::env::set_var("CODEX_HOME", codex_home);
    }
    let config =
        Config::load_with_cli_overrides_and_harness_overrides(vec![], Default::default()).await?;
    unsafe {
        std::env::remove_var("CODEX_HOME");
    }
    Ok(config)
}

#[tokio::test]
async fn list_shows_empty_state() -> Result<()> {
    let codex_home = TempDir::new()?;
    let config = Config::load_default_with_cli_overrides_for_codex_home(
        codex_home.path().to_path_buf(),
        vec![],
    )?;

    let stdout = list_servers(&config, ListArgs { json: false }).await?;
    assert!(stdout.contains("No MCP servers configured yet."));

    Ok(())
}

#[tokio::test]
async fn list_and_get_render_expected_output() -> Result<()> {
    let codex_home = TempDir::new()?;
    let config = Config::load_default_with_cli_overrides_for_codex_home(
        codex_home.path().to_path_buf(),
        vec![],
    )?;

    let McpSubcommand::Add(add_args) = McpCli::try_parse_from([
        "mcp",
        "add",
        "docs",
        "--env",
        "TOKEN=secret",
        "--",
        "docs-server",
        "--port",
        "4000",
    ])?
    .subcommand
    else {
        panic!("expected add subcommand");
    };
    add_server(&config, add_args).await?;

    let mut servers = load_global_mcp_servers(codex_home.path()).await?;
    let docs_entry = servers
        .get_mut("docs")
        .expect("docs server should exist after add");
    match &mut docs_entry.transport {
        McpServerTransportConfig::Stdio { env_vars, .. } => {
            *env_vars = vec!["APP_TOKEN".to_string(), "WORKSPACE_ID".to_string()];
        }
        other => panic!("unexpected transport: {other:?}"),
    }
    ConfigEditsBuilder::new(codex_home.path())
        .replace_mcp_servers(&servers)
        .apply_blocking()?;

    let config = load_config_for_codex_home(codex_home.path()).await?;
    let stdout = list_servers(&config, ListArgs { json: false }).await?;
    assert!(stdout.contains("Name"));
    assert!(stdout.contains("docs"));
    assert!(stdout.contains("docs-server"));
    assert!(stdout.contains("TOKEN=*****"));
    assert!(stdout.contains("APP_TOKEN=*****"));
    assert!(stdout.contains("WORKSPACE_ID=*****"));
    assert!(stdout.contains("Status"));
    assert!(stdout.contains("Auth"));
    assert!(stdout.contains("enabled"));
    assert!(stdout.contains("Unsupported"));

    let list_json_output = list_servers(&config, ListArgs { json: true }).await?;
    let parsed: JsonValue = serde_json::from_str(&list_json_output)?;
    assert_eq!(
        parsed,
        json!([
          {
            "name": "docs",
            "enabled": true,
            "disabled_reason": null,
            "transport": {
              "type": "stdio",
              "command": "docs-server",
              "args": [
                "--port",
                "4000"
              ],
              "env": {
                "TOKEN": "secret"
              },
              "env_vars": [
                "APP_TOKEN",
                "WORKSPACE_ID"
              ],
              "cwd": null
            },
            "startup_timeout_sec": null,
            "tool_timeout_sec": null,
            "auth_status": "unsupported"
          }
        ]
        )
    );

    let get_output = get_server(
        &config,
        GetArgs {
            name: "docs".into(),
            json: false,
        },
    )
    .await?;
    assert!(get_output.contains("docs"));
    assert!(get_output.contains("transport: stdio"));
    assert!(get_output.contains("command: docs-server"));
    assert!(get_output.contains("args: --port 4000"));
    assert!(get_output.contains("env: TOKEN=*****"));
    assert!(get_output.contains("APP_TOKEN=*****"));
    assert!(get_output.contains("WORKSPACE_ID=*****"));
    assert!(get_output.contains("enabled: true"));
    assert!(get_output.contains("remove: codex mcp remove docs"));

    let get_json_output = get_server(
        &config,
        GetArgs {
            name: "docs".into(),
            json: true,
        },
    )
    .await?;
    let parsed_get_json: JsonValue = serde_json::from_str(&get_json_output)?;
    assert_eq!(parsed_get_json["name"], "docs");
    assert_eq!(parsed_get_json["enabled"], true);

    Ok(())
}

#[tokio::test]
async fn get_disabled_server_shows_single_line() -> Result<()> {
    let codex_home = TempDir::new()?;
    let config = Config::load_default_with_cli_overrides_for_codex_home(
        codex_home.path().to_path_buf(),
        vec![],
    )?;

    let McpSubcommand::Add(add_args) =
        McpCli::try_parse_from(["mcp", "add", "docs", "--", "docs-server"])?.subcommand
    else {
        panic!("expected add subcommand");
    };
    add_server(&config, add_args).await?;

    let mut servers = load_global_mcp_servers(codex_home.path()).await?;
    let docs = servers
        .get_mut("docs")
        .expect("docs server should exist after add");
    docs.enabled = false;
    ConfigEditsBuilder::new(codex_home.path())
        .replace_mcp_servers(&servers)
        .apply_blocking()?;

    let config = load_config_for_codex_home(codex_home.path()).await?;
    let stdout = get_server(
        &config,
        GetArgs {
            name: "docs".into(),
            json: false,
        },
    )
    .await?;
    assert_eq!(stdout.trim_end(), "docs (disabled)");

    Ok(())
}
