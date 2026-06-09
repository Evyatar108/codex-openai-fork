use codex_features::Feature;
use codex_features::Features;
use codex_protocol::config_types::ModeKind;
use codex_protocol::openai_models::ConfigShellToolType;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::TruncationPolicyConfig;
use pretty_assertions::assert_eq;

use super::*;

fn model_with_shell_type(shell_type: ConfigShellToolType) -> ModelInfo {
    ModelInfo {
        slug: "test-model".to_string(),
        display_name: "Test Model".to_string(),
        description: None,
        default_reasoning_level: None,
        supported_reasoning_levels: Vec::new(),
        shell_type,
        visibility: ModelVisibility::List,
        supported_in_api: true,
        priority: 0,
        additional_speed_tiers: Vec::new(),
        service_tiers: Vec::new(),
        default_service_tier: None,
        availability_nux: None,
        upgrade: None,
        base_instructions: String::new(),
        model_messages: None,
        supports_reasoning_summaries: false,
        default_reasoning_summary: Default::default(),
        support_verbosity: false,
        default_verbosity: None,
        apply_patch_tool_type: None,
        web_search_tool_type: Default::default(),
        truncation_policy: TruncationPolicyConfig::tokens(/*limit*/ 1024),
        supports_parallel_tool_calls: true,
        supports_image_detail_original: false,
        context_window: None,
        max_context_window: None,
        auto_compact_token_limit: None,
        effective_context_window_percent: 95,
        experimental_supported_tools: Vec::new(),
        input_modalities: codex_protocol::openai_models::default_input_modalities(),
        used_fallback_model_metadata: false,
        supports_search_tool: false,
        wire_route: codex_protocol::openai_models::ModelWireRoute::ProviderDefault,
    }
}

fn shell_features() -> Features {
    let mut features = Features::with_defaults();
    features.enable(Feature::ShellTool);
    features.disable(Feature::ShellZshFork);
    features.disable(Feature::UnifiedExec);
    features
}

#[test]
fn shell_type_is_derived_from_model_and_feature_gates() {
    let model = model_with_shell_type(ConfigShellToolType::UnifiedExec);
    let mut features = shell_features();
    assert_eq!(
        shell_type_for_model_and_features(&model, &features),
        ConfigShellToolType::ShellCommand
    );

    features.enable(Feature::UnifiedExec);
    let expected_unified_exec = if codex_utils_pty::conpty_supported() {
        ConfigShellToolType::UnifiedExec
    } else {
        ConfigShellToolType::ShellCommand
    };
    assert_eq!(
        shell_type_for_model_and_features(&model, &features),
        expected_unified_exec
    );

    features.enable(Feature::ShellZshFork);
    assert_eq!(
        shell_type_for_model_and_features(&model, &features),
        ConfigShellToolType::ShellCommand
    );

    features.disable(Feature::ShellTool);
    assert_eq!(
        shell_type_for_model_and_features(&model, &features),
        ConfigShellToolType::Disabled
    );
}

#[test]
fn shell_command_backend_requires_both_shell_tool_and_zsh_fork() {
    let mut features = shell_features();
    assert_eq!(
        shell_command_backend_for_features(&features),
        ShellCommandBackendConfig::Classic
    );

    features.enable(Feature::ShellZshFork);
    assert_eq!(
        shell_command_backend_for_features(&features),
        ShellCommandBackendConfig::ZshFork
    );

    features.disable(Feature::ShellTool);
    assert_eq!(
        shell_command_backend_for_features(&features),
        ShellCommandBackendConfig::Classic
    );
}

#[test]
fn request_user_input_modes_follow_default_mode_feature() {
    let mut features = Features::with_defaults();
    features.disable(Feature::DefaultModeRequestUserInput);
    assert_eq!(
        request_user_input_available_modes(&features),
        vec![ModeKind::Plan]
    );

    features.enable(Feature::DefaultModeRequestUserInput);
    assert_eq!(
        request_user_input_available_modes(&features),
        vec![ModeKind::Default, ModeKind::Plan]
    );
}

#[test]
fn unified_exec_shell_mode_uses_zsh_fork_only_when_all_inputs_match() {
    let exe = std::env::current_exe().expect("current exe path");
    let shell = exe.clone();

    let mode = UnifiedExecShellMode::for_session(
        ShellCommandBackendConfig::ZshFork,
        ToolUserShellType::Zsh,
        Some(&shell),
        Some(&exe),
    );
    if cfg!(unix) {
        assert!(matches!(mode, UnifiedExecShellMode::ZshFork(_)));
    } else {
        assert_eq!(mode, UnifiedExecShellMode::Direct);
    }

    assert_eq!(
        UnifiedExecShellMode::for_session(
            ShellCommandBackendConfig::Classic,
            ToolUserShellType::Zsh,
            Some(&shell),
            Some(&exe),
        ),
        UnifiedExecShellMode::Direct
    );
    assert_eq!(
        tools_config
            .with_unified_exec_shell_mode_for_session(
                ToolUserShellType::Zsh,
                Some(&PathBuf::from(if cfg!(windows) {
                    r"C:\opt\codex\zsh"
                } else {
                    "/opt/codex/zsh"
                })),
                Some(&PathBuf::from(if cfg!(windows) {
                    r"C:\opt\codex\codex-execve-wrapper"
                } else {
                    "/opt/codex/codex-execve-wrapper"
                })),
            )
            .unified_exec_shell_mode,
        if cfg!(unix) {
            UnifiedExecShellMode::ZshFork(ZshForkConfig {
                shell_zsh_path: AbsolutePathBuf::from_absolute_path("/opt/codex/zsh").unwrap(),
                main_execve_wrapper_exe: AbsolutePathBuf::from_absolute_path(
                    "/opt/codex/codex-execve-wrapper",
                )
                .unwrap(),
            })
        } else {
            UnifiedExecShellMode::Direct
        }
    );
}

#[test]
fn subagents_keep_request_user_input_config_and_agent_jobs_workers_opt_in_by_label() {
    let model_info = model_info();
    let mut features = Features::with_defaults();
    features.enable(Feature::DefaultModeRequestUserInput);
    features.enable(Feature::SpawnCsv);

    let available_models = Vec::new();
    let tools_config = ToolsConfig::new(&ToolsConfigParams {
        model_info: &model_info,
        available_models: &available_models,
        features: &features,
        image_generation_tool_auth_allowed: true,
        web_search_mode: Some(WebSearchMode::Cached),
        session_source: SessionSource::SubAgent(SubAgentSource::Other(
            "agent_job:test".to_string(),
        )),
        permission_profile: &PermissionProfile::Disabled,
        windows_sandbox_level: WindowsSandboxLevel::Disabled,
    });

    assert_eq!(
        tools_config.request_user_input_available_modes,
        request_user_input_available_modes(&features)
    );
    assert!(tools_config.agent_jobs_tools);
    assert!(tools_config.agent_jobs_worker_tools);
}

#[test]
fn spawn_agent_is_top_level_only_and_agent_spawner_gets_top_level_session_tool() {
    let model_info = model_info();
    let mut features = Features::with_defaults();
    features.enable(Feature::Collab);

    let available_models = Vec::new();
    let top_level = ToolsConfig::new(&ToolsConfigParams {
        model_info: &model_info,
        available_models: &available_models,
        features: &features,
        image_generation_tool_auth_allowed: true,
        web_search_mode: Some(WebSearchMode::Cached),
        session_source: SessionSource::Cli,
        permission_profile: &PermissionProfile::Disabled,
        windows_sandbox_level: WindowsSandboxLevel::Disabled,
    });
    assert!(top_level.spawn_agent);
    assert!(!top_level.spawn_top_level_session);

    let subagent = ToolsConfig::new(&ToolsConfigParams {
        model_info: &model_info,
        available_models: &available_models,
        features: &features,
        image_generation_tool_auth_allowed: true,
        web_search_mode: Some(WebSearchMode::Cached),
        session_source: SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: codex_protocol::ThreadId::new(),
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: Some("agent-spawner".to_string()),
        }),
        permission_profile: &PermissionProfile::Disabled,
        windows_sandbox_level: WindowsSandboxLevel::Disabled,
    });
    assert!(!subagent.spawn_agent);
    assert!(subagent.spawn_top_level_session);
}

#[test]
fn image_generation_requires_feature_and_supported_model() {
    let supported_model_info = model_info();
    let mut unsupported_model_info = supported_model_info.clone();
    unsupported_model_info.input_modalities = vec![InputModality::Text];

    let mut image_generation_disabled_features = Features::with_defaults();
    image_generation_disabled_features.disable(Feature::ImageGeneration);
    let mut image_generation_features = Features::with_defaults();
    image_generation_features.enable(Feature::ImageGeneration);

    let available_models = Vec::new();
    let default_tools_config = ToolsConfig::new(&ToolsConfigParams {
        model_info: &supported_model_info,
        available_models: &available_models,
        features: &image_generation_disabled_features,
        image_generation_tool_auth_allowed: true,
        web_search_mode: Some(WebSearchMode::Cached),
        session_source: SessionSource::Cli,
        permission_profile: &PermissionProfile::Disabled,
        windows_sandbox_level: WindowsSandboxLevel::Disabled,
    });
    let supported_tools_config = ToolsConfig::new(&ToolsConfigParams {
        model_info: &supported_model_info,
        available_models: &available_models,
        features: &image_generation_features,
        image_generation_tool_auth_allowed: true,
        web_search_mode: Some(WebSearchMode::Cached),
        session_source: SessionSource::Cli,
        permission_profile: &PermissionProfile::Disabled,
        windows_sandbox_level: WindowsSandboxLevel::Disabled,
    });
    let auth_disallowed_tools_config = ToolsConfig::new(&ToolsConfigParams {
        model_info: &supported_model_info,
        available_models: &available_models,
        features: &image_generation_features,
        image_generation_tool_auth_allowed: false,
        web_search_mode: Some(WebSearchMode::Cached),
        session_source: SessionSource::Cli,
        permission_profile: &PermissionProfile::Disabled,
        windows_sandbox_level: WindowsSandboxLevel::Disabled,
    });
    let unsupported_tools_config = ToolsConfig::new(&ToolsConfigParams {
        model_info: &unsupported_model_info,
        available_models: &available_models,
        features: &image_generation_features,
        image_generation_tool_auth_allowed: true,
        web_search_mode: Some(WebSearchMode::Cached),
        session_source: SessionSource::Cli,
        permission_profile: &PermissionProfile::Disabled,
        windows_sandbox_level: WindowsSandboxLevel::Disabled,
    });
    assert!(!default_tools_config.image_gen_tool);
    assert!(supported_tools_config.image_gen_tool);
    assert!(!auth_disallowed_tools_config.image_gen_tool);
    assert!(!unsupported_tools_config.image_gen_tool);
}

#[test]
fn provider_capability_methods_disable_provider_bound_tool_surfaces() {
    let model_info = model_info();
    let features = Features::with_defaults();
    let available_models = Vec::new();
    let mut tools_config = ToolsConfig::new(&ToolsConfigParams {
        model_info: &model_info,
        available_models: &available_models,
        features: &features,
        image_generation_tool_auth_allowed: true,
        web_search_mode: Some(WebSearchMode::Cached),
        session_source: SessionSource::Cli,
        permission_profile: &PermissionProfile::Disabled,
        windows_sandbox_level: WindowsSandboxLevel::Disabled,
    });
    tools_config.search_tool = true;
    tools_config.tool_suggest = true;
    tools_config.image_gen_tool = true;
    tools_config.namespace_tools = true;

    let tools_config = tools_config
        .with_namespace_tools_capability(/*namespace_tools*/ false)
        .with_image_generation_capability(/*image_generation*/ false)
        .with_web_search_capability(/*web_search*/ false);

    assert!(tools_config.search_tool);
    assert!(tools_config.tool_suggest);
    assert!(!tools_config.image_gen_tool);
    assert!(!tools_config.namespace_tools);
    assert_eq!(tools_config.web_search_mode, None);
}
