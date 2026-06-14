# Patch surface

This registry is being rebuilt incrementally in the patched checkout. Some in-tree `patch-surface §N`
comments may reference earlier entries that predate this reconstructed file.

## §15 Windows TUI input-mode lifetime

- **Surface:** `codex-rs/tui/src/tui.rs`, `codex-rs/tui/src/tui/event_stream.rs`
- **Status:** active
- **Reason:** Windows consoles inherited or corrupted with `ENABLE_VIRTUAL_TERMINAL_INPUT` can
  leak VT focus tails like `[I` and `[O`, and mid-session corruption of crossterm raw-mode bits can
  return cooked input semantics while Codex keeps polling the same event stream.
- **Patch:** assert the full Codex TUI input mode when the TUI enters and in the Windows
  Codex-owned crossterm reader immediately before each event read: clear `ENABLE_LINE_INPUT`,
  `ENABLE_ECHO_INPUT`,
  `ENABLE_PROCESSED_INPUT`, and `ENABLE_VIRTUAL_TERMINAL_INPUT`, preserving unrelated console
  flags. Mid-session restore paths keep VT input cleared, final TUI teardown restores the inherited
  VT-input bit, and Windows setup sends focus-reporting disable instead of enabling xterm focus
  reporting.
- **Safety:** Windows-only; it preserves the parent console mode on exit while preventing focus
  report leakage and self-healing cooked-mode drift during the active Codex session. POSIX setup,
  restore, and event polling are unchanged.

## §16 Retained transcript viewport gate

- **Surface:** `codex-rs/features/src/lib.rs`, `codex-rs/tui/src/app.rs`,
  `codex-rs/tui/src/app/event_dispatch.rs`, `codex-rs/tui/src/app/resize_reflow.rs`,
  `codex-rs/tui/src/app/thread_routing.rs`, `codex-rs/tui/src/app_backtrack.rs`,
  `codex-rs/tui/src/chatwidget/committed_transcript.rs`
- **Status:** active
- **Reason:** commit `8548182d` made the retained committed-transcript viewport ride on the
  default-enabled `terminal_resize_reflow` flag, which regressed `/resume` by trickling history
  paint and removed native terminal scroll-up by default. Commit `6f0137db` moved the viewport
  behind its own feature and batched replay frames, but retained-mode typing still hit an
  O(history) flex desired-height scan over every committed cell.
- **Patch:** split the retained viewport behind its own default-off experimental feature
  (`retained_transcript_viewport`), restore the legacy terminal-scrollback path as the default,
  batch retained replay to a single frame, and keep stream-finalization reflow on the default path
  limited to real resize-repair cases. The retained main-viewport renderable reports O(1)
  fill-available intent from `desired_height`, leaving the existing bounded `visible_height` prepass
  and visible-tail renderer to decide which committed rows are drawn.
- **Safety:** default behavior returns to the upstream/native scrollback model, while the retained
  viewport remains opt-in and still preserves its resize-friendly rendering when explicitly enabled.
  The typing-lag fix is contained to the fork-owned retained renderable and does not alter generic
  flex allocation or native history-cell height measurement.

## §17 Fork runtime flags migrated to experimental features

- **Surface:** `codex-rs/features/src/lib.rs`, `codex-rs/core/src/config/mod.rs`,
  `codex-rs/config/src/config_toml.rs`, `codex-rs/model-provider/src/anthropic_gate.rs`,
  `codex-rs/model-provider/src/copilot/gated_models_manager.rs`,
  `codex-rs/tui/src/app_server_session.rs`, `codex-rs/tui/src/app.rs`,
  `codex-rs/tui/src/chatwidget/constructor.rs`, `codex-rs/tui/src/style.rs`,
  `codex-rs/tui/src/bottom_pane/chat_composer.rs`
- **Status:** active
- **Reason:** fork-local runtime behavior should not be controlled by ad-hoc launcher/env/top-level
  booleans. `Feature::AnthropicModels` must be the single Anthropic authority so a stale
  `CODEX_ENABLE_ANTHROPIC` value or persisted Claude model cannot keep Claude visible when the
  feature is off.
- **Patch:** keep `features.anthropic_models` default-off and install its final resolved value into
  the model-provider gate; make it visible in `/experimental`; keep `--enable-anthropic` only as a CLI alias to the canonical feature;
  replace unavailable persisted Claude defaults with the gate-filtered catalog default in the
  Copilot model manager and during TUI bootstrap; add default-off experimental features
  `legacy_paste_burst_heuristic` and
  `user_message_styling`; map the old `disable_paste_burst` config key into
  `legacy_paste_burst_heuristic` only when the canonical feature key is absent; install
  `user_message_styling` once from resolved TUI config instead of reading a process env var; map the
  old `style_user_messages` launcher/config key to `user_message_styling`; add a default-off
  `auto_load_claude_md` experimental feature that appends `CLAUDE.md` as a project-doc fallback after
  explicit `project_doc_fallback_filenames`.
- **Safety:** canonical feature entries win over compatibility aliases. Custom non-catalog model
  strings remain untouched unless they are Claude/Anthropic slugs absent from the gate-filtered model
  list. `CLAUDE.md` is not auto-loaded unless `auto_load_claude_md` is explicitly enabled, and
  explicit fallback filenames retain priority. The standalone public composer keeps its existing
  non-config-backed paste behavior because it has no feature source to re-enable the detector.
- **Verification:** feature registry/config tests cover default-off paste semantics, legacy alias
  mapping, stale style-key mapping, picker visibility, and canonical precedence; model-provider tests
  cover the Anthropic gate; agents-md tests cover default-off/configured/feature-enabled `CLAUDE.md`
  fallback behavior; TUI tests cover persisted-Claude fallback and the feature-backed styling helper
  branches.

## §18 Copilot prompt budget context tiers

- **Surface:** `codex-rs/model-provider/src/copilot_models_endpoint.rs`,
  `codex-rs/models-manager/src/model_info.rs`
- **Status:** active
- **Reason:** Copilot `/models` reports gpt-5.5 long-context as a total window
  (`max_context_window_tokens=1_050_000`) plus a prompt/input cap
  (`billing.token_prices.long_context.context_max` / `max_prompt_tokens=922_000`) and output
  reserve (`max_output_tokens=128_000`). Budgeting input against the total lets normal turns and
  auto-compaction requests exceed the server-enforced prompt cap.
- **Patch:** parse `max_prompt_tokens`, `max_output_tokens`, and
  `billing.token_prices.{default,long_context}.context_max`; use those prompt/input caps for the
  default and long context tier windows that feed config tier selection and compaction budgeting.
  Fall back to the legacy total window only when Copilot does not provide prompt-specific limits.
- **Safety:** default-tier curated caps remain in place for older responses, single-tier models still
  collapse to one window, and non-Copilot budget consumers continue to read the same `ModelInfo`
  fields.

## §19 Windows console-mode corruption tracer

- **Surface:** `codex-rs/tui/src/tui/console_mode_trace.rs`,
  `codex-rs/tui/src/tui/event_stream.rs`, `codex-rs/tui/src/app.rs`,
  `codex-rs/tui/src/app/thread_routing.rs`,
  `docs/implementation/console-mode-trace-runbook.md`
- **Status:** active
- **Reason:** the live Windows input-mode corruption appears to be caused by an out-of-band runtime
  actor, so source inspection alone cannot identify the writer. The self-heal in §15 repairs the
  mode before input is consumed; an opt-in tracer is needed to capture the corrupted mode and nearby
  structural action marker before that repair happens.
- **Patch:** add a Windows-only, default-off `CODEX_CONSOLE_MODE_TRACE=1` tracer with a cached env
  gate. On the Windows production crossterm reader it snapshots `GetConsoleMode`, records a
  `console-mode-delta` JSONL row before reasserting the expected input mode, and appends to
  `console-mode-trace.jsonl` next to the active rollout directory (or a pid-named sessions fallback
  if no rollout path is known yet). Records contain named mode flags, unexpected bits, structural
  last-input/action/terminal markers, timestamps, and hashed ids only.
- **Safety:** when unset the tracer does not allocate, lock, open files, or perform extra console
  syscalls beyond the §15 guard snapshot. The marker path records no command text, prompts, paste
  content, tool output, absolute paths, or raw ids; privacy/off-path tests cover disabled no-write and
  command-notification redaction.

## §20 Managed hooks opt-in gate

- **Surface:** `codex-rs/features/src/lib.rs`, `codex-rs/core/src/config/mod.rs`,
  `codex-rs/hooks/src/managed_gate.rs`, `codex-rs/hooks/src/engine/discovery.rs`,
  `codex-rs/cli/src/main.rs`, `codex-rs/tui/src/chatwidget/settings_popups.rs`
- **Status:** active
- **Reason:** commit `883f75f50a` made the fork skip managed/admin-config-pushed hooks by default
  because enterprise-managed hook sources can run privileged policy code on every tool call. That
  behavior needs to remain default-off while still exposing an explicit opt-in path.
- **Patch:** keep `features.managed_hooks` default-off and resolve it as the single runtime gate for
  managed hook discovery. `--enable-managed-hooks`, `[features] managed_hooks = true`,
  `-c features.managed_hooks=true`, and the compatibility `CODEX_ENABLE_MANAGED_HOOKS` environment
  fallback all converge on the same gate. The feature is now visible in `/experimental` as "Allow
  managed (admin) hooks" with copy that calls out the enterprise/admin surface and the default-off
  behavior.
- **Safety:** when disabled, managed/admin-config hooks remain suppressed exactly as before; enabling
  the feature restores honored managed hooks without changing user/project hook discovery.

## §21 Windows Git Bash default-shell opt-in

- **Surface:** `codex-rs/features/src/lib.rs`, `codex-rs/core/src/windows_git_bash.rs`,
  `codex-rs/core/src/session/default_shell.rs`, `codex-rs/core/src/session/session.rs`,
  `codex-rs/core/src/session/turn_context.rs`,
  `codex-rs/core/src/tools/spec_plan.rs`, `codex-rs/core/src/tools/handlers/shell_spec.rs`,
  `codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs`,
  `codex-rs/core/src/tools/handlers/shell/shell_command.rs`,
  `codex-rs/core/src/context/environment_context_tests.rs`
- **Status:** active
- **Reason:** Windows users need a default-off experimental path to run sessions under Git Bash
  without launcher-specific `default_shell` wiring, while preserving explicit shell overrides and the
  Windows Job Object child-tree cleanup that protects shell grandchildren from hanging shutdown.
- **Patch:** add `features.windows_git_bash_shell` as a visible default-off experiment; when enabled
  on Windows and no explicit `default_shell` or zsh fork shell wins, detect Git Bash via fixed
  Git-for-Windows installs, `%LOCALAPPDATA%\Programs\Git`, `where git` candidates in
  `..\bin\bash.exe` / `.\bash.exe` / `..\usr\bin\bash.exe` order, then `where bash` last. The
  detector returns a normal Bash `Shell`, so `Session::new`, `exec_command`, and `shell_command`
  continue through `Shell::derive_exec_args` and the existing exec/session-shell path. The resolved
  session shell type now flows through `TurnContext` into model-visible shell tool descriptions so
  Git Bash sessions advertise bash syntax and PowerShell sessions retain PowerShell syntax.
- **Safety:** feature-off behavior is unchanged, the feature is a no-op off Windows, explicit
  `default_shell` and `ShellZshFork` stay higher priority, missing Git Bash emits a startup warning
  before falling back, no `features.unified_exec=false` guard is introduced, and no Git-Bash-specific
  process-spawn path bypasses the Windows Job Object wrapper. Replant by keeping the detector
  ordering tests, default-shell selector tests, shell-spec Bash/PowerShell assertions, and the
  `detected_shell_routes_through_existing_bash_exec_args` regression together with this seam.
