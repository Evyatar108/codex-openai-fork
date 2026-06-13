# Patch surface

This registry is being rebuilt incrementally in the patched checkout. Some in-tree `patch-surface §N`
comments may reference earlier entries that predate this reconstructed file.

## §15 Windows TUI VT-input lifetime

- **Surface:** `codex-rs/tui/src/tui.rs`
- **Status:** active
- **Reason:** Windows consoles inherited with `ENABLE_VIRTUAL_TERMINAL_INPUT` let crossterm's
  console-record path lose the `ESC` leader for VT focus reports after a mid-session restore,
  which leaks tails like `[I` and `[O` into Codex input.
- **Patch:** clear the inherited VT-input bit when Codex TUI enters, keep it cleared across
  mid-session `restore_common()` / `set_modes()` cycles, and restore the original inherited bit
  only from final TUI teardown (`restore_after_exit()`).
- **Safety:** Windows-only; it preserves the parent console mode on exit while preventing focus
  report leakage during the active Codex session.

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
  the model-provider gate; keep `--enable-anthropic` only as a CLI alias to the canonical feature;
  replace unavailable persisted Claude defaults with the gate-filtered catalog default in the
  Copilot model manager and during TUI bootstrap; add default-off experimental features
  `legacy_paste_burst_heuristic` and
  `user_message_styling`; map the old `disable_paste_burst` config key into
  `legacy_paste_burst_heuristic` only when the canonical feature key is absent; install
  `user_message_styling` once from resolved TUI config instead of reading a process env var.
- **Safety:** canonical feature entries win over compatibility aliases. Custom non-catalog model
  strings remain untouched unless they are Claude/Anthropic slugs absent from the gate-filtered model
  list. The standalone public composer keeps its existing non-config-backed paste behavior because it
  has no feature source to re-enable the detector.
- **Verification:** feature registry/config tests cover default-off paste semantics, legacy alias
  mapping, and canonical precedence; model-provider tests cover the Anthropic gate; TUI tests cover
  persisted-Claude fallback and the feature-backed styling helper branches.

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
