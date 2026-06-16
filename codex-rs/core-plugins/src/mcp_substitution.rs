//! Plugin `.mcp.json` `${CLAUDE_PLUGIN_ROOT}` / `${CODEX_PLUGIN_ROOT}` substitution.
//!
//! SANDBOX PATCH: lets Claude- and Copilot-style plugin manifests work in codex
//! unmodified. See AGENTS.override.md → "Core engineering tenants" §1 and
//! docs/implementation/patch-surface.md §16 for the rationale and rebase notes.

use serde_json::Map as JsonMap;
use serde_json::Value as JsonValue;
use std::path::Path;

const CLAUDE_PLACEHOLDER: &str = "${CLAUDE_PLUGIN_ROOT}";
const CODEX_PLACEHOLDER: &str = "${CODEX_PLUGIN_ROOT}";

/// Substitute `${CLAUDE_PLUGIN_ROOT}` and `${CODEX_PLUGIN_ROOT}` tokens in the
/// `command`, `args`, `cwd`, and `env` values of a plugin-bundled MCP server
/// config JSON object.
///
/// `env_vars` is intentionally NOT touched — its `name` field is an environment
/// **variable name**, not a path. Unknown `${...}` tokens pass through verbatim;
/// substitution is plain string replacement, not shell expansion.
pub(crate) fn apply_plugin_root_substitution(
    plugin_root: &Path,
    object: &mut JsonMap<String, JsonValue>,
) {
    let root = plugin_root.display().to_string();

    for key in ["command", "cwd"] {
        if let Some(JsonValue::String(s)) = object.get_mut(key) {
            *s = substitute(s, &root);
        }
    }

    if let Some(JsonValue::Array(items)) = object.get_mut("args") {
        for item in items.iter_mut() {
            if let JsonValue::String(s) = item {
                *s = substitute(s, &root);
            }
        }
    }

    if let Some(JsonValue::Object(map)) = object.get_mut("env") {
        for value in map.values_mut() {
            if let JsonValue::String(s) = value {
                *s = substitute(s, &root);
            }
        }
    }
}

fn substitute(value: &str, root: &str) -> String {
    value
        .replace(CLAUDE_PLACEHOLDER, root)
        .replace(CODEX_PLACEHOLDER, root)
}

/// SANDBOX PATCH: invariant 20 — apply [`apply_plugin_root_substitution`] to every server
/// object in a raw plugin `.mcp.json` document, before upstream's structured
/// `parse_plugin_mcp_config`. Re-serializing through `serde_json` keeps substituted
/// (Windows backslash) paths valid JSON. Supports both the `{ "mcpServers": { … } }` and the
/// flat `{ name: { … } }` document shapes parsed by `parse_plugin_mcp_config`.
pub(crate) fn apply_plugin_root_substitution_to_contents(
    plugin_root: &Path,
    contents: &str,
) -> String {
    let mut doc: JsonValue = match serde_json::from_str(contents) {
        Ok(doc) => doc,
        Err(_) => return contents.to_string(),
    };
    let Some(object) = doc.as_object_mut() else {
        return contents.to_string();
    };
    if let Some(JsonValue::Object(servers)) = object.get_mut("mcpServers") {
        for value in servers.values_mut() {
            if let JsonValue::Object(server) = value {
                apply_plugin_root_substitution(plugin_root, server);
            }
        }
    } else {
        for value in object.values_mut() {
            if let JsonValue::Object(server) = value {
                apply_plugin_root_substitution(plugin_root, server);
            }
        }
    }
    serde_json::to_string(&doc).unwrap_or_else(|_| contents.to_string())
}
