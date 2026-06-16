mod amazon_bedrock;
// SANDBOX PATCH: process-global resolution of the Anthropic-models opt-in gate
// (`--enable-anthropic` flag / `features.anthropic_models` config).
mod anthropic_gate;
mod auth;
mod bearer_auth_provider;
// SANDBOX PATCH: Copilot session routing lives in `copilot.rs`
// (`CopilotModelProvider`) and `copilot_models_endpoint.rs`
// (`CopilotModelsEndpoint`). Both must be declared as modules so
// `provider.rs::create_model_provider` can route `is_copilot()` providers
// through them; otherwise Copilot requests fall through to the OpenAI
// Bearer path which has no token, producing "Authorization header is
// badly formatted" 400s from api.githubcopilot.com.
mod copilot;
mod copilot_models_endpoint;
mod models_endpoint;
mod provider;

pub use auth::auth_provider_from_auth;
pub use auth::unauthenticated_auth_provider;
// SANDBOX PATCH: Anthropic-models gate accessors. `install_anthropic_gate` is
// called once at config-build time; `anthropic_models_resolved` is read at the
// transport/model-list gate call sites.
pub use anthropic_gate::anthropic_models_resolved;
pub use anthropic_gate::install_anthropic_gate;
pub use bearer_auth_provider::BearerAuthProvider;
// SANDBOX PATCH: dropped `pub use BearerAuthProvider as CoreAuthProvider` alias.
// It collides with `codex_api::CoreAuthProvider` (the fork's Copilot-aware
// provider in codex-api/src/api_bridge.rs) in any scope that imports both,
// and silently shadowing it makes Copilot requests fall back to a tokenless
// Bearer path.
pub use codex_protocol::account::ProviderAccount;
pub use provider::ModelProvider;
pub use provider::ModelProviderFuture;
pub use provider::ProviderAccountError;
pub use provider::ProviderAccountResult;
pub use provider::ProviderAccountState;
pub use provider::ProviderCapabilities;
pub use provider::SharedModelProvider;
pub use provider::create_model_provider;
