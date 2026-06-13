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
  `codex-rs/tui/src/app/thread_routing.rs`, `codex-rs/tui/src/app_backtrack.rs`
- **Status:** active
- **Reason:** commit `8548182d` made the retained committed-transcript viewport ride on the
  default-enabled `terminal_resize_reflow` flag, which regressed `/resume` by trickling history
  paint and removed native terminal scroll-up by default.
- **Patch:** split the retained viewport behind its own default-off experimental feature
  (`retained_transcript_viewport`), restore the legacy terminal-scrollback path as the default,
  batch retained replay to a single frame, and keep stream-finalization reflow on the default path
  limited to real resize-repair cases.
- **Safety:** default behavior returns to the upstream/native scrollback model, while the retained
  viewport remains opt-in and still preserves its resize-friendly rendering when explicitly enabled.
