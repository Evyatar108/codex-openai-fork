// SANDBOX PATCH: process-global resolution of the "Anthropic models"
// (Claude-via-Copilot) opt-in gate.
//
// Historically the gate had a single source: the `CODEX_ENABLE_ANTHROPIC`
// environment variable, resolved by the overlay `codex_copilot` crate. This
// module layers two friendlier opt-ins on top while keeping the env var as a
// back-compat fallback. The effective precedence (highest wins) is:
//
//   1. the `--enable-anthropic` CLI flag,
//   2. the `features.anthropic_models` config key,
//   3. the `CODEX_ENABLE_ANTHROPIC` environment variable (back-compat),
//   4. the built-in default (`false`).
//
// The CLI flag is realized at the argument-parsing boundary by injecting a
// `-c features.anthropic_models=true` override (see `codex-cli`), which outranks
// `config.toml`. So by the time config is resolved, the flag and the config key
// have collapsed into a single tri-state `Option<bool>`: `Some` when either set
// it, `None` when neither did. [`install_anthropic_gate`] installs that tri-state
// at config-build time; [`anthropic_models_resolved`] reads it at the gate call
// sites and falls back to the environment value when nothing explicit was set.
//
// The gate is process-global (matching the env var it supersedes). Only an
// explicit `Some` installs the global, so a default/empty config build can never
// freeze the gate to `false`; an unset config leaves the env fallback in force.
// As with the env var, the first explicit value wins for the process.

use std::sync::OnceLock;

/// Resolves the gate from the explicit config-layer value and the
/// environment-layer fallback. `config` is `Some` only when the
/// `--enable-anthropic` flag or the `features.anthropic_models` key set it
/// (the flag wins over `config.toml` via a `-c` override); `None` defers to
/// `env`. `env` is the legacy `CODEX_ENABLE_ANTHROPIC` value, already defaulted
/// to `false` when unset.
fn resolve_anthropic_gate(config: Option<bool>, env: bool) -> bool {
    config.unwrap_or(env)
}

static CONFIG_GATE: OnceLock<bool> = OnceLock::new();

/// Installs the explicit config-layer value (the `--enable-anthropic` flag or the
/// `features.anthropic_models` key) once at config-build time and returns the
/// effective gate value for this resolution.
///
/// Only an explicit `Some` installs the process-global; an unset config leaves
/// the environment fallback in force, so a default/empty config build can never
/// freeze the gate to `false`. The first explicit value wins for the process.
pub fn install_anthropic_gate(config: Option<bool>) -> bool {
    let effective = resolve_anthropic_gate(config, codex_copilot::anthropic_models_enabled());
    if let Some(value) = config {
        let _ = CONFIG_GATE.set(value);
    }
    effective
}

/// Returns the effective gate value at a call site: the installed explicit
/// config value when present, otherwise the `CODEX_ENABLE_ANTHROPIC` environment
/// fallback resolved by the overlay helper (default `false`).
pub fn anthropic_models_resolved() -> bool {
    resolve_anthropic_gate(
        CONFIG_GATE.get().copied(),
        codex_copilot::anthropic_models_enabled(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    // Precedence ladder (highest wins): flag > config > env > default.
    //
    // The `--enable-anthropic` flag and the `features.anthropic_models` config
    // key collapse into the `config` tri-state at the CLI boundary (the flag is
    // injected as a `-c features.anthropic_models=true` override, which outranks
    // `config.toml`), so the flag and config levels are both represented by
    // `config = Some(_)` here. The CLI fold itself is covered in `codex-cli`.

    #[test]
    fn explicit_enable_wins_over_env_and_default() {
        // flag/config = Some(true) -> on, regardless of the env fallback.
        assert_eq!(resolve_anthropic_gate(Some(true), false), true);
        assert_eq!(resolve_anthropic_gate(Some(true), true), true);
    }

    #[test]
    fn explicit_disable_beats_env_back_compat() {
        // An explicit config disable overrides the env back-compat fallback.
        assert_eq!(resolve_anthropic_gate(Some(false), true), false);
        assert_eq!(resolve_anthropic_gate(Some(false), false), false);
    }

    #[test]
    fn env_back_compat_used_when_unset() {
        // Nothing explicit set -> the legacy env var still enables the gate.
        assert_eq!(resolve_anthropic_gate(None, true), true);
    }

    #[test]
    fn defaults_off_when_nothing_set() {
        // Nothing explicit and env unset -> default OFF.
        assert_eq!(resolve_anthropic_gate(None, false), false);
    }
}
