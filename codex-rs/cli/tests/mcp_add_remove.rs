use anyhow::Result;
use clap::Parser;
use codex_cli::mcp_cmd::McpCli;
use codex_cli::mcp_cmd::McpSubcommand;
use codex_cli::mcp_cmd::RemoveArgs;
use codex_cli::mcp_cmd::add_server;
use codex_cli::mcp_cmd::remove_server;
use codex_config::types::McpServerTransportConfig;
use codex_core::config::Config;
use codex_core::config::load_global_mcp_servers;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[tokio::test]
async fn add_and_remove_server_updates_global_config() -> Result<()> {
    let codex_home = TempDir::new()?;
    let config = Config::load_default_with_cli_overrides_for_codex_home(
        codex_home.path().to_path_buf(),
        vec![],
    )?;

    let McpSubcommand::Add(add_args) =
        McpCli::try_parse_from(["mcp", "add", "docs", "--", "echo", "hello"])?.subcommand
    else {
        panic!("expected add subcommand");
    };
    let add_output = add_server(&config, add_args).await?;
    assert!(add_output.contains("Added global MCP server 'docs'."));

    let servers = load_global_mcp_servers(codex_home.path()).await?;
    assert_eq!(servers.len(), 1);
    let docs = servers.get("docs").expect("server should exist");
    match &docs.transport {
        McpServerTransportConfig::Stdio {
            command,
            args,
            env,
            env_vars,
            cwd,
        } => {
            assert_eq!(command, "echo");
            assert_eq!(args, &vec!["hello".to_string()]);
            assert!(env.is_none());
            assert!(env_vars.is_empty());
            assert!(cwd.is_none());
        }
        other => panic!("unexpected transport: {other:?}"),
    }
    assert!(docs.enabled);

    let remove_output = remove_server(
        codex_home.path(),
        RemoveArgs {
            name: "docs".into(),
        },
    )
    .await?;
    assert!(remove_output.contains("Removed global MCP server 'docs'."));

    let servers = load_global_mcp_servers(codex_home.path()).await?;
    assert!(servers.is_empty());

    let remove_again_output = remove_server(
        codex_home.path(),
        RemoveArgs {
            name: "docs".into(),
        },
    )
    .await?;
    assert!(remove_again_output.contains("No MCP server named 'docs' found."));

    let servers = load_global_mcp_servers(codex_home.path()).await?;
    assert!(servers.is_empty());

    Ok(())
}

#[tokio::test]
async fn add_with_env_preserves_key_order_and_values() -> Result<()> {
    let codex_home = TempDir::new()?;
    let config = Config::load_default_with_cli_overrides_for_codex_home(
        codex_home.path().to_path_buf(),
        vec![],
    )?;

    let McpSubcommand::Add(add_args) = McpCli::try_parse_from([
        "mcp",
        "add",
        "envy",
        "--env",
        "FOO=bar",
        "--env",
        "ALPHA=beta",
        "--",
        "python",
        "server.py",
    ])?
    .subcommand
    else {
        panic!("expected add subcommand");
    };
    add_server(&config, add_args).await?;

    let servers = load_global_mcp_servers(codex_home.path()).await?;
    let envy = servers.get("envy").expect("server should exist");
    let env = match &envy.transport {
        McpServerTransportConfig::Stdio { env: Some(env), .. } => env,
        other => panic!("unexpected transport: {other:?}"),
    };

    assert_eq!(env.len(), 2);
    assert_eq!(env.get("FOO"), Some(&"bar".to_string()));
    assert_eq!(env.get("ALPHA"), Some(&"beta".to_string()));
    assert!(envy.enabled);

    Ok(())
}

#[tokio::test]
async fn add_streamable_http_without_manual_token() -> Result<()> {
    let codex_home = TempDir::new()?;
    let config = Config::load_default_with_cli_overrides_for_codex_home(
        codex_home.path().to_path_buf(),
        vec![],
    )?;

    let McpSubcommand::Add(add_args) =
        McpCli::try_parse_from(["mcp", "add", "github", "--url", "https://example.com/mcp"])?
            .subcommand
    else {
        panic!("expected add subcommand");
    };
    add_server(&config, add_args).await?;

    let servers = load_global_mcp_servers(codex_home.path()).await?;
    let github = servers.get("github").expect("github server should exist");
    match &github.transport {
        McpServerTransportConfig::StreamableHttp {
            url,
            bearer_token_env_var,
            http_headers,
            env_http_headers,
        } => {
            assert_eq!(url, "https://example.com/mcp");
            assert!(bearer_token_env_var.is_none());
            assert!(http_headers.is_none());
            assert!(env_http_headers.is_none());
        }
        other => panic!("unexpected transport: {other:?}"),
    }
    assert!(github.enabled);

    assert!(!codex_home.path().join(".credentials.json").exists());
    assert!(!codex_home.path().join(".env").exists());

    Ok(())
}

#[tokio::test]
async fn add_streamable_http_with_custom_env_var() -> Result<()> {
    let codex_home = TempDir::new()?;
    let config = Config::load_default_with_cli_overrides_for_codex_home(
        codex_home.path().to_path_buf(),
        vec![],
    )?;

    let McpSubcommand::Add(add_args) = McpCli::try_parse_from([
        "mcp",
        "add",
        "issues",
        "--url",
        "https://example.com/issues",
        "--bearer-token-env-var",
        "GITHUB_TOKEN",
    ])?
    .subcommand
    else {
        panic!("expected add subcommand");
    };
    add_server(&config, add_args).await?;

    let servers = load_global_mcp_servers(codex_home.path()).await?;
    let issues = servers.get("issues").expect("issues server should exist");
    match &issues.transport {
        McpServerTransportConfig::StreamableHttp {
            url,
            bearer_token_env_var,
            http_headers,
            env_http_headers,
        } => {
            assert_eq!(url, "https://example.com/issues");
            assert_eq!(bearer_token_env_var.as_deref(), Some("GITHUB_TOKEN"));
            assert!(http_headers.is_none());
            assert!(env_http_headers.is_none());
        }
        other => panic!("unexpected transport: {other:?}"),
    }
    assert!(issues.enabled);
    Ok(())
}

#[tokio::test]
async fn add_streamable_http_rejects_removed_flag() -> Result<()> {
    let codex_home = TempDir::new()?;

    let err = McpCli::try_parse_from([
        "mcp",
        "add",
        "github",
        "--url",
        "https://example.com/mcp",
        "--with-bearer-token",
    ])
    .expect_err("removed flag should be rejected");
    assert!(err.to_string().contains("--with-bearer-token"));

    let servers = load_global_mcp_servers(codex_home.path()).await?;
    assert!(servers.is_empty());

    Ok(())
}

#[tokio::test]
async fn add_cant_add_command_and_url() -> Result<()> {
    let codex_home = TempDir::new()?;

    let err = McpCli::try_parse_from([
        "mcp",
        "add",
        "github",
        "--url",
        "https://example.com/mcp",
        "--command",
        "--",
        "echo",
        "hello",
    ])
    .expect_err("conflicting command/url should fail");
    assert!(
        err.to_string()
            .contains("unexpected argument '--command' found")
    );

    let servers = load_global_mcp_servers(codex_home.path()).await?;
    assert!(servers.is_empty());

    Ok(())
}
