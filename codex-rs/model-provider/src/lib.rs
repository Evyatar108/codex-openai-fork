mod auth;
mod bearer_auth_provider;
mod copilot;
mod provider;

pub use bearer_auth_provider::AuthorizationHeaderAuthProvider;
pub use bearer_auth_provider::BearerAuthProvider;
// SANDBOX PATCH: dropped `pub use BearerAuthProvider as CoreAuthProvider` alias.
// It was unused externally and collided with the separate `codex_api::CoreAuthProvider`
// type in any scope that imported both.
pub use provider::ModelProvider;
pub use provider::SharedModelProvider;
pub use provider::create_model_provider;
