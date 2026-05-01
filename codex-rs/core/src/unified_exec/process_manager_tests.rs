use super::*;
use crate::exec::ExecCapturePolicy;
use crate::exec::ExecExpiration;
use crate::sandboxing::ExecRequest;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ExecCommandToolOutput;
use crate::unified_exec::AwaitBackgroundCompletionRequest;
use crate::unified_exec::NoopSpawnLifecycle;
use crate::unified_exec::UnifiedExecContext;
use codex_sandboxing::SandboxType;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use tokio::time::Duration;
use tokio::time::Instant;

async fn test_session_and_turn() -> (Arc<Session>, Arc<TurnContext>) {
    let (session, turn) = make_session_and_context().await;
    (Arc::new(session), Arc::new(turn))
}

fn test_exec_request(
    turn: &TurnContext,
    command: Vec<String>,
    cwd: AbsolutePathBuf,
) -> ExecRequest {
    ExecRequest::new(
        command,
        cwd,
        std::env::vars().collect(),
        /*network*/ None,
        ExecExpiration::DefaultTimeout,
        ExecCapturePolicy::ShellTool,
        SandboxType::None,
        turn.windows_sandbox_level,
        /*windows_sandbox_private_desktop*/ false,
        turn.sandbox_policy.get().clone(),
        turn.file_system_sandbox_policy.clone(),
        turn.network_sandbox_policy,
        /*arg0*/ None,
    )
}

async fn spawn_background_process(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    cmd: &str,
) -> Result<i32, UnifiedExecError> {
    let manager = &session.services.unified_exec_manager;
    let process_id = manager.allocate_process_id().await;
    let command = vec!["bash".to_string(), "-lc".to_string(), cmd.to_string()];
    let request = test_exec_request(turn, command.clone(), turn.cwd.clone());
    let process = Arc::new(
        manager
            .open_session_with_exec_env(
                process_id,
                &request,
                /*tty*/ false,
                Box::new(NoopSpawnLifecycle),
                turn.environment.as_ref().expect("turn environment"),
            )
            .await?,
    );

    let context = UnifiedExecContext::new(
        Arc::clone(session),
        Arc::clone(turn),
        "await-call".to_string(),
    );
    let transcript = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
    start_streaming_output(&process, &context, Arc::clone(&transcript));
    let started_at = Instant::now();

    let entry = ProcessEntry {
        process,
        transcript,
        call_id: context.call_id,
        process_id,
        hook_command: cmd.to_string(),
        tty: false,
        network_approval_id: None,
        session: Arc::downgrade(session),
        last_used: started_at,
    };
    manager
        .process_store
        .lock()
        .await
        .processes
        .insert(process_id, entry);

    Ok(process_id)
}

async fn await_background_completion(
    session: &Arc<Session>,
    process_id: i32,
    timeout_ms: Option<u64>,
) -> Result<ExecCommandToolOutput, UnifiedExecError> {
    session
        .services
        .unified_exec_manager
        .await_background_completion(AwaitBackgroundCompletionRequest {
            process_id,
            timeout_ms,
            max_output_tokens: None,
        })
        .await
}

#[test]
fn unified_exec_env_injects_defaults() {
    let env = apply_unified_exec_env(HashMap::new());
    let expected = HashMap::from([
        ("NO_COLOR".to_string(), "1".to_string()),
        ("TERM".to_string(), "dumb".to_string()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
        ("LC_CTYPE".to_string(), "C.UTF-8".to_string()),
        ("LC_ALL".to_string(), "C.UTF-8".to_string()),
        ("COLORTERM".to_string(), String::new()),
        ("PAGER".to_string(), "cat".to_string()),
        ("GIT_PAGER".to_string(), "cat".to_string()),
        ("GH_PAGER".to_string(), "cat".to_string()),
        ("CODEX_CI".to_string(), "1".to_string()),
    ]);

    assert_eq!(env, expected);
}

#[test]
fn unified_exec_env_overrides_existing_values() {
    let mut base = HashMap::new();
    base.insert("NO_COLOR".to_string(), "0".to_string());
    base.insert("PATH".to_string(), "/usr/bin".to_string());

    let env = apply_unified_exec_env(base);

    assert_eq!(env.get("NO_COLOR"), Some(&"1".to_string()));
    assert_eq!(env.get("PATH"), Some(&"/usr/bin".to_string()));
}

#[test]
fn env_overlay_for_exec_server_keeps_runtime_changes_only() {
    let local_policy_env = HashMap::from([
        ("HOME".to_string(), "/client-home".to_string()),
        ("PATH".to_string(), "/client-path".to_string()),
        ("SHELL_SET".to_string(), "policy".to_string()),
    ]);
    let request_env = HashMap::from([
        ("HOME".to_string(), "/client-home".to_string()),
        ("PATH".to_string(), "/sandbox-path".to_string()),
        ("SHELL_SET".to_string(), "policy".to_string()),
        ("CODEX_THREAD_ID".to_string(), "thread-1".to_string()),
        (
            "CODEX_SANDBOX_NETWORK_DISABLED".to_string(),
            "1".to_string(),
        ),
    ]);

    assert_eq!(
        env_overlay_for_exec_server(&request_env, &local_policy_env),
        HashMap::from([
            ("PATH".to_string(), "/sandbox-path".to_string()),
            ("CODEX_THREAD_ID".to_string(), "thread-1".to_string()),
            (
                "CODEX_SANDBOX_NETWORK_DISABLED".to_string(),
                "1".to_string()
            ),
        ])
    );
}

#[test]
fn exec_server_params_use_env_policy_overlay_contract() {
    let cwd: codex_utils_absolute_path::AbsolutePathBuf = std::env::current_dir()
        .expect("current dir")
        .try_into()
        .expect("absolute path");
    let sandbox_policy = codex_protocol::protocol::SandboxPolicy::DangerFullAccess;
    let file_system_sandbox_policy =
        codex_protocol::permissions::FileSystemSandboxPolicy::from(&sandbox_policy);
    let network_sandbox_policy = codex_protocol::permissions::NetworkSandboxPolicy::Restricted;
    let permission_profile =
        codex_protocol::models::PermissionProfile::from_runtime_permissions_with_enforcement(
            codex_protocol::models::SandboxEnforcement::from_legacy_sandbox_policy(&sandbox_policy),
            &file_system_sandbox_policy,
            network_sandbox_policy,
        );
    let request = ExecRequest {
        command: vec!["bash".to_string(), "-lc".to_string(), "true".to_string()],
        cwd: cwd.clone(),
        env: HashMap::from([
            ("HOME".to_string(), "/client-home".to_string()),
            ("PATH".to_string(), "/sandbox-path".to_string()),
            ("CODEX_THREAD_ID".to_string(), "thread-1".to_string()),
        ]),
        exec_server_env_config: Some(ExecServerEnvConfig {
            policy: codex_exec_server::ExecEnvPolicy {
                inherit: codex_protocol::config_types::ShellEnvironmentPolicyInherit::Core,
                ignore_default_excludes: false,
                exclude: Vec::new(),
                r#set: HashMap::new(),
                include_only: Vec::new(),
            },
            local_policy_env: HashMap::from([
                ("HOME".to_string(), "/client-home".to_string()),
                ("PATH".to_string(), "/client-path".to_string()),
            ]),
        }),
        network: None,
        expiration: crate::exec::ExecExpiration::DefaultTimeout,
        capture_policy: crate::exec::ExecCapturePolicy::ShellTool,
        sandbox: codex_sandboxing::SandboxType::None,
        windows_sandbox_policy_cwd: cwd,
        windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::Disabled,
        windows_sandbox_private_desktop: false,
        permission_profile,
        file_system_sandbox_policy,
        network_sandbox_policy,
        windows_sandbox_filesystem_overrides: None,
        arg0: None,
    };

    let params =
        exec_server_params_for_request(/*process_id*/ 123, &request, /*tty*/ true);

    assert_eq!(params.process_id.as_str(), "123");
    assert!(params.env_policy.is_some());
    assert_eq!(
        params.env,
        HashMap::from([
            ("PATH".to_string(), "/sandbox-path".to_string()),
            ("CODEX_THREAD_ID".to_string(), "thread-1".to_string()),
        ])
    );
}

#[test]
fn exec_server_process_id_matches_unified_exec_process_id() {
    assert_eq!(exec_server_process_id(/*process_id*/ 4321), "4321");
}

#[tokio::test]
async fn network_denial_fallback_message_names_sandbox_network_proxy() {
    let message = network_denial_message_for_session(/*session*/ None, /*deferred*/ None).await;

    assert_eq!(
        message,
        "Network access was denied by the Codex sandbox network proxy."
    );
}

#[tokio::test]
async fn late_network_denial_grace_observes_cancellation_after_exit() {
    let cancellation = CancellationToken::new();
    let cancellation_for_task = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancellation_for_task.cancel();
    });

    assert!(wait_for_late_network_denial(Some(cancellation)).await);
}

#[tokio::test]
async fn failed_initial_end_for_unstored_process_uses_fallback_output() {
    let (session, turn, rx_event) = crate::session::tests::make_session_and_context_with_rx().await;
    let context = UnifiedExecContext::new(
        Arc::clone(&session),
        Arc::clone(&turn),
        "call-unified-denied".to_string(),
    );
    let request = ExecCommandRequest {
        command: vec![
            "sh".to_string(),
            "-lc".to_string(),
            "echo before".to_string(),
        ],
        hook_command: "echo before".to_string(),
        process_id: 123,
        yield_time_ms: 1000,
        max_output_tokens: None,
        workdir: None,
        network: None,
        tty: true,
        sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
        additional_permissions: None,
        additional_permissions_preapproved: false,
        justification: None,
        prefix_rule: None,
    };

    let transcript = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
    transcript
        .lock()
        .await
        .push_chunk(b"PARTIAL_TRANSCRIPT".to_vec());

    emit_failed_initial_exec_end_if_unstored(
        /*process_started_alive*/ false,
        &context,
        &request,
        turn.cwd.clone(),
        transcript,
        "PRE_DENIAL_MARKER".to_string(),
        "Network access denied".to_string(),
        Duration::from_millis(7),
    )
    .await;

    let event = tokio::time::timeout(Duration::from_secs(1), rx_event.recv())
        .await
        .expect("timed out waiting for failed exec end event")
        .expect("event channel closed");
    let codex_protocol::protocol::EventMsg::ExecCommandEnd(end_event) = event.msg else {
        panic!("expected ExecCommandEnd event");
    };
    assert_eq!(end_event.call_id, "call-unified-denied");
    assert_eq!(
        end_event.status,
        codex_protocol::protocol::ExecCommandStatus::Failed
    );
    assert_eq!(end_event.exit_code, -1);
    assert_eq!(end_event.process_id.as_deref(), Some("123"));
    assert_eq!(
        end_event.aggregated_output,
        "PRE_DENIAL_MARKER\nNetwork access denied"
    );
}

#[test]
fn pruning_prefers_exited_processes_outside_recently_used() {
    let now = Instant::now();
    let meta = vec![
        (1, now - Duration::from_secs(40), false),
        (2, now - Duration::from_secs(30), true),
        (3, now - Duration::from_secs(20), false),
        (4, now - Duration::from_secs(19), false),
        (5, now - Duration::from_secs(18), false),
        (6, now - Duration::from_secs(17), false),
        (7, now - Duration::from_secs(16), false),
        (8, now - Duration::from_secs(15), false),
        (9, now - Duration::from_secs(14), false),
        (10, now - Duration::from_secs(13), false),
    ];

    let candidate = UnifiedExecProcessManager::process_id_to_prune_from_meta(&meta);

    assert_eq!(candidate, Some(2));
}

#[test]
fn pruning_falls_back_to_lru_when_no_exited() {
    let now = Instant::now();
    let meta = vec![
        (1, now - Duration::from_secs(40), false),
        (2, now - Duration::from_secs(30), false),
        (3, now - Duration::from_secs(20), false),
        (4, now - Duration::from_secs(19), false),
        (5, now - Duration::from_secs(18), false),
        (6, now - Duration::from_secs(17), false),
        (7, now - Duration::from_secs(16), false),
        (8, now - Duration::from_secs(15), false),
        (9, now - Duration::from_secs(14), false),
        (10, now - Duration::from_secs(13), false),
    ];

    let candidate = UnifiedExecProcessManager::process_id_to_prune_from_meta(&meta);

    assert_eq!(candidate, Some(1));
}

#[test]
fn pruning_protects_recent_processes_even_if_exited() {
    let now = Instant::now();
    let meta = vec![
        (1, now - Duration::from_secs(40), false),
        (2, now - Duration::from_secs(30), false),
        (3, now - Duration::from_secs(20), true),
        (4, now - Duration::from_secs(19), false),
        (5, now - Duration::from_secs(18), false),
        (6, now - Duration::from_secs(17), false),
        (7, now - Duration::from_secs(16), false),
        (8, now - Duration::from_secs(15), false),
        (9, now - Duration::from_secs(14), false),
        (10, now - Duration::from_secs(13), true),
    ];

    let candidate = UnifiedExecProcessManager::process_id_to_prune_from_meta(&meta);

    // (10) is exited but among the last 8; we should drop the LRU outside that set.
    assert_eq!(candidate, Some(1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn await_background_completion_waits_for_exit_and_returns_exit_code() -> anyhow::Result<()> {
    let (session, turn) = test_session_and_turn().await;
    let process_id = spawn_background_process(
        &session,
        &turn,
        "sleep 0.2; printf 'await-finished'; exit 7",
    )
    .await?;

    let output = await_background_completion(&session, process_id, Some(2_500)).await?;

    assert_eq!(output.process_id, None);
    assert_eq!(output.exit_code, Some(7));
    assert!(
        output.truncated_output().contains("await-finished"),
        "await should return the aggregated background transcript"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn await_background_completion_timeout_returns_buffered_output() -> anyhow::Result<()> {
    let (session, turn) = test_session_and_turn().await;
    let process_id = spawn_background_process(
        &session,
        &turn,
        "sleep 0.05; printf 'before-timeout'; sleep 2; printf 'after-timeout'",
    )
    .await?;

    let output = await_background_completion(&session, process_id, Some(500)).await?;

    assert_eq!(output.process_id, Some(process_id));
    assert_eq!(output.exit_code, None);
    let text = output.truncated_output();
    assert!(
        text.contains("before-timeout"),
        "timeout response should include already buffered transcript"
    );
    assert!(
        !text.contains("after-timeout"),
        "timeout response should return before process completion"
    );

    await_background_completion(&session, process_id, Some(2_500)).await?;

    Ok(())
}

#[tokio::test]
async fn await_background_completion_unknown_process_returns_error() {
    let (session, _) = test_session_and_turn().await;

    let err = await_background_completion(&session, 98_765, Some(10))
        .await
        .expect_err("expected unknown process error");

    match err {
        UnifiedExecError::UnknownProcessId { process_id } => assert_eq!(process_id, 98_765),
        other => panic!("expected UnknownProcessId, got {other:?}"),
    }
}
