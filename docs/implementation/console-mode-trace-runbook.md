# Windows console-mode trace runbook

Use this when a Windows TUI session starts echoing focus tails such as `[I` or `[O`, Backspace
echoes instead of deleting, Enter/Escape stop acting as Codex controls, or special keys arrive as
literal input after agent, shell, or unified-exec activity.

## Enable the tracer

Set the environment variable before launching Codex:

```powershell
$env:CODEX_CONSOLE_MODE_TRACE = "1"
$env:RUST_LOG = "codex_console_mode_trace=debug,codex_tui=info"
codex.exe
```

The tracer is Windows-only and default-off. When the variable is unset, the marker and sidecar path
calls return before taking locks, allocating marker state, or opening files.

## Reproduce

Keep the TUI open and reproduce the mid-session corruption. The trace snapshot runs before the
production crossterm event read, records the corrupted live mode if it differs from the expected TUI
input mode, then Codex reasserts the expected mode before reading the next event.

## Find the sidecar

Preferred path:

```text
~\.codex\sessions\<yyyy>\<mm>\<dd>\console-mode-trace.jsonl
```

If corruption occurs before the TUI knows the active rollout path, also check:

```text
~\.codex\sessions\console-mode-trace-<pid>.jsonl
```

## Classify the first `console-mode-delta`

Use the first row because later rows may reflect already-repaired state:

| Record shape | Likely culprit |
|---|---|
| `unexpected_set` includes `ENABLE_LINE_INPUT`, `ENABLE_ECHO_INPUT`, and `ENABLE_PROCESSED_INPUT`; `recent_action.kind` is `command_execution` | Shell/unified-exec child restored cooked mode. |
| Same cooked bits; `recent_action.kind` is `collab_agent_tool_call` with spawn/send/resume status | Agent orchestration is the trigger; look for an external harness/process around it. |
| Only `ENABLE_VIRTUAL_TERMINAL_INPUT` is unexpected and the last marker is focus-related | VT input/focus-reporting leak rather than full cooked-mode loss. |
| Delta follows `resize`, `focus_gained`, `focus_lost`, `enter_alt_screen`, `leave_alt_screen`, or `with_restored.*` | Terminal/conhost/alt-screen or intentional restore/re-entry path. |
| Delta has no recent marker | Out-of-band writer or missing marker; correlate with the rollout around the same timestamp. |

The sidecar is metadata-only: named console-mode flags, timestamps, structural marker kinds/statuses,
and FNV-1a hashes for ids. It must not contain command text, prompts, paste content, tool output,
absolute paths, or raw thread/turn/item ids.
