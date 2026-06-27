// SANDBOX PATCH: process-global resolution of the "Anthropic models"
// (Claude-via-Copilot) opt-in gate.
//
// `Feature::AnthropicModels` is the sole runtime source of truth. The
// `--enable-anthropic` CLI flag folds into `-c features.anthropic_models=true`
// before config resolution, so every runtime call site can read the final
// resolved feature value installed here.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

struct AnthropicGate {
    enabled: AtomicBool,
}

impl AnthropicGate {
    const fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
        }
    }

    fn install(&self, enabled: bool) -> bool {
        self.enabled.store(enabled, Ordering::Relaxed);
        enabled
    }

    fn resolved(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
}

static CONFIG_GATE: AnthropicGate = AnthropicGate::new();

// SANDBOX PATCH: stricter sub-gate for the SIGNED Anthropic Messages
// (`/v1/messages`) transport. `Feature::AnthropicSignedMessages` is the sole
// runtime source of truth; it is ANDed with `AnthropicModels` at every routing/
// cache/filter site. Installed once at config-build time alongside the parent
// gate; without that install path the flag stays `false`.
static SIGNED_MESSAGES_GATE: AnthropicGate = AnthropicGate::new();

/// Installs the final resolved `features.anthropic_models` value at config-build
/// time and returns it for the caller's resolved config bookkeeping.
pub fn install_anthropic_gate(enabled: bool) -> bool {
    CONFIG_GATE.install(enabled)
}

/// Returns the final resolved Anthropic feature gate value at transport and
/// model-catalog call sites.
pub fn anthropic_models_resolved() -> bool {
    CONFIG_GATE.resolved()
}

/// Installs the final resolved `features.anthropic_signed_messages` value at
/// config-build time and returns it for the caller's resolved config
/// bookkeeping. The signed path is only taken when this AND
/// `install_anthropic_gate` are both `true`.
pub fn install_anthropic_signed_messages_gate(enabled: bool) -> bool {
    SIGNED_MESSAGES_GATE.install(enabled)
}

/// Returns the final resolved signed-messages sub-gate value at the routing,
/// model-cache identity, and picker-filter call sites.
pub fn anthropic_signed_messages_resolved() -> bool {
    SIGNED_MESSAGES_GATE.resolved()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_gate_defaults_off() {
        let gate = AnthropicGate::new();

        assert!(!gate.resolved());
    }

    #[test]
    fn installed_value_is_runtime_value() {
        let gate = AnthropicGate::new();

        assert!(gate.install(true));
        assert!(gate.resolved());

        assert!(!gate.install(false));
        assert!(!gate.resolved());
    }
}
