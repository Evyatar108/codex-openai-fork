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
