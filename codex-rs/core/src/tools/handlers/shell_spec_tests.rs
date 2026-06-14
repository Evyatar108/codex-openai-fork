use super::*;
use codex_tools::ToolUserShellType;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

#[test]
fn exec_command_tool_matches_expected_spec() {
    let tool = create_exec_command_tool(CommandToolOptions {
        allow_login_shell: true,
        exec_permission_approvals_enabled: false,
    });

    let description = if cfg!(windows) {
        windows_exec_command_description(ToolUserShellType::PowerShell)
    } else {
        "Runs a command in a PTY, returning output or a session ID for ongoing interaction."
            .to_string()
    };

    let mut properties = BTreeMap::from([
        (
            "cmd".to_string(),
            JsonSchema::string(Some("Shell command to execute.".to_string())),
        ),
        (
            "workdir".to_string(),
            JsonSchema::string(Some(
                    "Optional working directory to run the command in; defaults to the turn cwd."
                        .to_string(),
                )),
        ),
        (
            "shell".to_string(),
            JsonSchema::string(Some(
                    "Shell binary to launch. Defaults to the user's default shell.".to_string(),
                )),
        ),
        (
            "tty".to_string(),
            JsonSchema::boolean(Some(
                    "Whether to allocate a TTY for the command. Defaults to false (plain pipes); set to true to open a PTY and access TTY process."
                        .to_string(),
                )),
        ),
        (
            "yield_time_ms".to_string(),
            JsonSchema::number(Some(
                    "How long to wait (in milliseconds) for output before yielding.".to_string(),
                )),
        ),
        (
            "max_output_tokens".to_string(),
            JsonSchema::number(Some(
                    "Maximum number of tokens to return. Excess output will be truncated."
                        .to_string(),
                )),
        ),
        (
            "login".to_string(),
            JsonSchema::boolean(Some(
                    "Whether to run the shell with -l/-i semantics. Defaults to true.".to_string(),
                )),
        ),
    ]);
    properties.extend(create_approval_parameters(
        /*exec_permission_approvals_enabled*/ false,
    ));

    assert_eq!(
        tool,
        ToolSpec::Function(ResponsesApiTool {
            name: "exec_command".to_string(),
            description,
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["cmd".to_string()]),
                Some(false.into())
            ),
            output_schema: Some(unified_exec_output_schema()),
        })
    );
}

#[test]
fn write_stdin_tool_matches_expected_spec() {
    let tool = create_write_stdin_tool();

    let properties = BTreeMap::from([
        (
            "session_id".to_string(),
            JsonSchema::number(Some(
                "Identifier of the running unified exec session.".to_string(),
            )),
        ),
        (
            "chars".to_string(),
            JsonSchema::string(Some(
                "Bytes to write to stdin (may be empty to poll).".to_string(),
            )),
        ),
        (
            "yield_time_ms".to_string(),
            JsonSchema::number(Some(
                "How long to wait (in milliseconds) for output before yielding.".to_string(),
            )),
        ),
        (
            "max_output_tokens".to_string(),
            JsonSchema::number(Some(
                "Maximum number of tokens to return. Excess output will be truncated.".to_string(),
            )),
        ),
    ]);

    assert_eq!(
        tool,
        ToolSpec::Function(ResponsesApiTool {
            name: "write_stdin".to_string(),
            description:
                "Writes characters to an existing unified exec session and returns recent output."
                    .to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["session_id".to_string()]),
                Some(false.into())
            ),
            output_schema: Some(unified_exec_output_schema()),
        })
    );
}

#[test]
fn request_permissions_tool_includes_full_permission_schema() {
    let tool =
        create_request_permissions_tool("Request extra permissions for this turn.".to_string());

    let properties = BTreeMap::from([
        (
            "reason".to_string(),
            JsonSchema::string(Some(
                "Optional short explanation for why additional permissions are needed.".to_string(),
            )),
        ),
        ("permissions".to_string(), permission_profile_schema()),
    ]);

    assert_eq!(
        tool,
        ToolSpec::Function(ResponsesApiTool {
            name: "request_permissions".to_string(),
            description: "Request extra permissions for this turn.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["permissions".to_string()]),
                Some(false.into())
            ),
            output_schema: None,
        })
    );
}

#[test]
fn shell_command_tool_matches_expected_spec() {
    let tool = create_shell_command_tool(CommandToolOptions {
        allow_login_shell: true,
        exec_permission_approvals_enabled: false,
    });

    let description = if cfg!(windows) {
        windows_shell_command_description(ToolUserShellType::PowerShell)
    } else {
        r#"Runs a shell command and returns its output.
- Always set the `workdir` param when using the shell_command function. Do not use `cd` unless absolutely necessary."#
            .to_string()
    };

    let mut properties = BTreeMap::from([
        (
            "command".to_string(),
            JsonSchema::string(Some(
                "The shell script to execute in the user's default shell".to_string(),
            )),
        ),
        (
            "workdir".to_string(),
            JsonSchema::string(Some(
                "The working directory to execute the command in".to_string(),
            )),
        ),
        (
            "timeout_ms".to_string(),
            JsonSchema::number(Some(
                "The timeout for the command in milliseconds".to_string(),
            )),
        ),
        (
            "login".to_string(),
            JsonSchema::boolean(Some(
                "Whether to run the shell with login shell semantics. Defaults to true."
                    .to_string(),
            )),
        ),
    ]);
    properties.extend(create_approval_parameters(
        /*exec_permission_approvals_enabled*/ false,
    ));

    assert_eq!(
        tool,
        ToolSpec::Function(ResponsesApiTool {
            name: "shell_command".to_string(),
            description,
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["command".to_string()]),
                Some(false.into())
            ),
            output_schema: None,
        })
    );
}

#[cfg(windows)]
#[test]
fn windows_bash_shell_tools_describe_git_bash_syntax() {
    // SANDBOX PATCH: model-facing shell hints must follow active Git Bash sessions.
    let exec_tool = create_exec_command_tool_with_environment_id(
        CommandToolOptions {
            allow_login_shell: false,
            exec_permission_approvals_enabled: false,
        },
        /*include_environment_id*/ false,
        ToolUserShellType::Bash,
    );
    let shell_tool = create_shell_command_tool_for_shell(
        CommandToolOptions {
            allow_login_shell: false,
            exec_permission_approvals_enabled: false,
        },
        ToolUserShellType::Bash,
    );

    let ToolSpec::Function(exec_tool) = exec_tool else {
        panic!("exec_command should be a function tool");
    };
    let ToolSpec::Function(shell_tool) = shell_tool else {
        panic!("shell_command should be a function tool");
    };

    assert!(exec_tool.description.contains("Git Bash/bash shell"));
    assert!(exec_tool.description.contains("grep -R 'TODO' ."));
    assert!(!exec_tool.description.contains("PowerShell cmdlets"));
    assert!(
        shell_tool
            .description
            .contains("Runs a Git Bash/bash command")
    );
    assert!(shell_tool.description.contains("find . -name '*.py'"));
    assert!(shell_tool.description.contains("export FOO=bar"));
    assert!(!shell_tool.description.contains("Get-ChildItem"));
}

#[cfg(windows)]
#[test]
fn windows_powershell_shell_tools_describe_powershell_syntax() {
    // SANDBOX PATCH: preserve PowerShell model-facing hints for PowerShell sessions.
    let shell_tool = create_shell_command_tool_for_shell(
        CommandToolOptions {
            allow_login_shell: false,
            exec_permission_approvals_enabled: false,
        },
        ToolUserShellType::PowerShell,
    );

    let ToolSpec::Function(shell_tool) = shell_tool else {
        panic!("shell_command should be a function tool");
    };

    assert!(shell_tool.description.contains("Powershell command"));
    assert!(shell_tool.description.contains("Get-ChildItem"));
    assert!(!shell_tool.description.contains("Git Bash/bash"));
}
