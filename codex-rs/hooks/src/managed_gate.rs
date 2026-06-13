// SANDBOX PATCH: process-independent resolution of the "managed hooks" opt-in
// gate.
//
// The fork does NOT honor managed/admin-config hooks (hooks discovered from
// MDM / system / legacy-managed-config sources, or from managed requirements —
// i.e. every hook whose trust is "Managed") by default. Enterprise admin policy
// can inject such a hook (for example a Microsoft Defender `PreToolUse` scan
// that runs on every tool call and times out), which the operator does not want
// the fork to execute. Managed hooks are re-enabled with an opt-in whose
// effective precedence (highest wins) is:
//
//   1. the `--enable-managed-hooks` CLI flag,
//   2. the `features.managed_hooks` config key,
//   3. the `CODEX_ENABLE_MANAGED_HOOKS` environment variable (back-compat),
//   4. the built-in default (`false`).
//
// This mirrors the `--enable-anthropic` / `features.anthropic_models` gate
// (see `codex-model-provider::anthropic_gate`).
// The CLI flag is realized at the argument-parsing boundary by injecting a
// `-c features.managed_hooks=true` override (see `codex-cli`), which outranks
// `config.toml`. So by the time config is resolved, the flag and the config key
// have collapsed into a single explicit tri-state `Option<bool>`: `Some` when
// either set it, `None` when neither did. [`resolve_managed_hooks_gate`]
// resolves that tri-state against the environment fallback.

/// Resolves the gate from the explicit config-layer value and the
/// environment-layer fallback. `config` is `Some` only when the
/// `--enable-managed-hooks` flag or the `features.managed_hooks` key set it
/// (the flag wins over `config.toml` via a `-c` override); `None` defers to
/// `env`. `env` is the `CODEX_ENABLE_MANAGED_HOOKS` value, already defaulted to
/// `false` when unset.
fn resolve_gate(config: Option<bool>, env: bool) -> bool {
    config.unwrap_or(env)
}

/// Reads the `CODEX_ENABLE_MANAGED_HOOKS` environment fallback. Unset, empty, or
/// any value other than a recognized truthy token resolves to `false`.
pub fn managed_hooks_env_enabled() -> bool {
    std::env::var("CODEX_ENABLE_MANAGED_HOOKS")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

/// Resolves the effective managed-hooks gate from the explicit config-layer
/// value (the `--enable-managed-hooks` flag or the `features.managed_hooks`
/// key), folding in the `CODEX_ENABLE_MANAGED_HOOKS` environment fallback when
/// nothing explicit was set. Returns `false` (managed hooks skipped) by default.
pub fn resolve_managed_hooks_gate(config: Option<bool>) -> bool {
    resolve_gate(config, managed_hooks_env_enabled())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    // Precedence ladder (highest wins): flag > config > env > default.
    //
    // The `--enable-managed-hooks` flag and the `features.managed_hooks` config
    // key collapse into the `config` tri-state at the CLI boundary (the flag is
    // injected as a `-c features.managed_hooks=true` override, which outranks
    // `config.toml`), so the flag and config levels are both represented by
    // `config = Some(_)` here. The CLI fold itself is covered in `codex-cli`.

    #[test]
    fn explicit_enable_wins_over_env_and_default() {
        // flag/config = Some(true) -> on, regardless of the env fallback.
        assert_eq!(resolve_gate(Some(true), false), true);
        assert_eq!(resolve_gate(Some(true), true), true);
    }

    #[test]
    fn explicit_disable_beats_env() {
        // An explicit config disable overrides the env fallback.
        assert_eq!(resolve_gate(Some(false), true), false);
        assert_eq!(resolve_gate(Some(false), false), false);
    }

    #[test]
    fn env_used_when_unset() {
        // Nothing explicit set -> the env var still enables the gate.
        assert_eq!(resolve_gate(None, true), true);
    }

    #[test]
    fn defaults_off_when_nothing_set() {
        // Nothing explicit and env unset -> default OFF (managed hooks skipped).
        assert_eq!(resolve_gate(None, false), false);
    }
}
