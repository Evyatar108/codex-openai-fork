use codex_core::config::Config;
use codex_model_provider_info::COPILOT_BASE_URL;
use codex_model_provider_info::COPILOT_PROVIDER_ID;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

const CONFIG_TOML: &str = "config.toml";

#[tokio::test]
async fn builtin_copilot_attack_rejected_at_config_load() {
    let codex_home = tempdir().expect("temp dir");

    let err = Config::load_default_with_cli_overrides_for_codex_home(
        codex_home.path().to_path_buf(),
        vec![(
            "model_providers.copilot.base_url".to_string(),
            toml::Value::String("http://evil.example.com".to_string()),
        )],
    )
    .await
    .expect_err("reserved built-in copilot override should fail");

    let message = err.to_string();
    assert!(
        message.contains("copilot") || message.contains("model_providers"),
        "unexpected config-load error: {message}"
    );
}

#[tokio::test]
async fn builtin_copilot_provider_resolves_through_config_load() {
    let codex_home = tempdir().expect("temp dir");

    let cfg = Config::load_default_with_cli_overrides_for_codex_home(
        codex_home.path().to_path_buf(),
        vec![],
    )
    .await
    .expect("default config should load without error");

    let copilot = cfg
        .model_providers
        .get(COPILOT_PROVIDER_ID)
        .expect("built-in copilot provider should be present");

    assert_eq!(
        copilot.base_url.as_deref(),
        Some(COPILOT_BASE_URL),
        "copilot base_url should be the canonical Copilot API URL"
    );
    assert!(
        !copilot.supports_websockets,
        "copilot provider should have supports_websockets = false"
    );
    assert!(
        !copilot.requires_openai_auth,
        "copilot provider should have requires_openai_auth = false"
    );
    assert!(
        copilot.is_copilot_trusted(),
        "copilot provider should satisfy is_copilot_trusted()"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn override_turn_context_does_not_persist_when_config_exists() {
    let server = start_mock_server().await;
    let initial_contents = "model = \"gpt-4o\"\n";
    let mut builder = test_codex()
        .with_pre_build_hook(move |home| {
            let config_path = home.join(CONFIG_TOML);
            std::fs::write(config_path, initial_contents).expect("seed config.toml");
        })
        .with_config(|config| {
            config.model = Some("gpt-4o".to_string());
        });
    let test = builder.build(&server).await.expect("create conversation");
    let codex = test.codex.clone();
    let config_path = test.home.path().join(CONFIG_TOML);

    codex
        .submit(Op::OverrideTurnContext {
            cwd: None,
            approval_policy: None,
            approvals_reviewer: None,
            sandbox_policy: None,
            permission_profile: None,
            windows_sandbox_level: None,
            model: Some("o3".to_string()),
            effort: Some(Some(ReasoningEffort::High)),
            summary: None,
            service_tier: None,
            collaboration_mode: None,
            personality: None,
        })
        .await
        .expect("submit override");

    codex.submit(Op::Shutdown).await.expect("request shutdown");
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::ShutdownComplete)).await;

    let contents = tokio::fs::read_to_string(&config_path)
        .await
        .expect("read config.toml after override");
    assert_eq!(contents, initial_contents);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn override_turn_context_does_not_create_config_file() {
    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build(&server).await.expect("create conversation");
    let codex = test.codex.clone();
    let config_path = test.home.path().join(CONFIG_TOML);
    assert!(
        !config_path.exists(),
        "test setup should start without config"
    );

    codex
        .submit(Op::OverrideTurnContext {
            cwd: None,
            approval_policy: None,
            approvals_reviewer: None,
            sandbox_policy: None,
            permission_profile: None,
            windows_sandbox_level: None,
            model: Some("o3".to_string()),
            effort: Some(Some(ReasoningEffort::Medium)),
            summary: None,
            service_tier: None,
            collaboration_mode: None,
            personality: None,
        })
        .await
        .expect("submit override");

    codex.submit(Op::Shutdown).await.expect("request shutdown");
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::ShutdownComplete)).await;

    assert!(
        !config_path.exists(),
        "override should not create config.toml"
    );
}
