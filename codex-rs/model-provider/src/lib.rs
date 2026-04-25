mod amazon_bedrock;
mod auth;
mod bearer_auth_provider;
mod copilot;
mod copilot_models_endpoint;
mod models_endpoint;
mod provider;

pub use auth::auth_provider_from_auth;
pub use auth::unauthenticated_auth_provider;
pub use bearer_auth_provider::BearerAuthProvider;
// SANDBOX PATCH: dropped `pub use BearerAuthProvider as CoreAuthProvider` alias.
// It was unused externally and collided with the separate `codex_api::CoreAuthProvider`
// type in any scope that imported both.
pub use codex_protocol::account::ProviderAccount;
pub use provider::ModelProvider;
pub use provider::ProviderAccountError;
pub use provider::ProviderAccountResult;
pub use provider::ProviderAccountState;
pub use provider::SharedModelProvider;
pub use provider::create_model_provider;
