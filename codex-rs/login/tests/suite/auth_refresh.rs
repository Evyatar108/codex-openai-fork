use anyhow::Context;
use anyhow::Result;
use base64::Engine;
use chrono::Duration;
use chrono::Utc;
use codex_app_server_protocol::AuthMode;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthDotJson;
use codex_login::AuthManager;
use codex_login::REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR;
use codex_login::load_auth_dot_json;
use codex_login::save_auth;
use codex_login::token_data::IdTokenInfo;
use codex_login::token_data::TokenData;
use codex_protocol::auth::RefreshTokenFailedReason;
use core_test_support::skip_if_no_network;
use pretty_assertions::assert_eq;
use serde::Serialize;
use serde_json::json;
use std::ffi::OsString;
use std::sync::Arc;
use tempfile::TempDir;
use wiremock::MockServer;

const INITIAL_ACCESS_TOKEN: &str = "initial-access-token";
const INITIAL_REFRESH_TOKEN: &str = "initial-refresh-token";

#[serial_test::serial(auth_refresh)]
#[tokio::test]
async fn refresh_token_methods_are_noops_under_sandbox_patch() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let ctx = RefreshTokenTestContext::new(&server).await?;
    let stale_refresh = Utc::now() - Duration::days(9);
    let initial_tokens = build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(initial_tokens.clone()),
        last_refresh: Some(stale_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;

    ctx.auth_manager.refresh_token_from_authority().await?;
    ctx.auth_manager.refresh_token().await?;

    let stored = ctx.load_auth()?;
    assert_eq!(stored, initial_auth);

    let cached_auth = ctx
        .auth_manager
        .auth()
        .await
        .context("auth should be cached")?;
    let cached = cached_auth
        .get_token_data()
        .context("token data should remain cached")?;
    assert_eq!(cached, initial_tokens);

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(requests.is_empty(), "expected no refresh token requests");

    Ok(())
}

#[serial_test::serial(auth_refresh)]
#[tokio::test]
async fn auth_keeps_stale_cached_auth_when_disk_changes() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let ctx = RefreshTokenTestContext::new(&server).await?;
    let stale_refresh = Utc::now() - Duration::days(9);
    let initial_tokens = build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(initial_tokens.clone()),
        last_refresh: Some(stale_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;

    let disk_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(build_tokens("disk-access-token", "disk-refresh-token")),
        last_refresh: Some(Utc::now() - Duration::days(1)),
        agent_identity: None,
    };
    save_auth(
        ctx.codex_home.path(),
        &disk_auth,
        AuthCredentialsStoreMode::File,
    )?;

    let cached_auth = ctx
        .auth_manager
        .auth()
        .await
        .context("auth should be cached")?;
    let cached = cached_auth
        .get_token_data()
        .context("token data should remain cached")?;
    assert_eq!(cached, initial_tokens);

    let stored = ctx.load_auth()?;
    assert_eq!(stored, disk_auth);

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(requests.is_empty(), "expected no refresh token requests");

    Ok(())
}

#[serial_test::serial(auth_refresh)]
#[tokio::test]
async fn unauthorized_recovery_reloads_disk_auth_then_noops_refresh() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let ctx = RefreshTokenTestContext::new(&server).await?;
    let initial_last_refresh = Utc::now() - Duration::days(1);
    let initial_tokens = build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(initial_tokens),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;

    let disk_tokens = build_tokens("disk-access-token", "disk-refresh-token");
    let disk_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(disk_tokens.clone()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    save_auth(
        ctx.codex_home.path(),
        &disk_auth,
        AuthCredentialsStoreMode::File,
    )?;

    let mut recovery = ctx.auth_manager.unauthorized_recovery();
    assert!(recovery.has_next());

    let reload_result = recovery.next().await?;
    assert_eq!(reload_result.auth_state_changed(), Some(true));

    let cached_after_reload = ctx
        .auth_manager
        .auth_cached()
        .context("auth should be cached after reload")?;
    let cached_after_reload_tokens = cached_after_reload
        .get_token_data()
        .context("token data should reload from disk")?;
    assert_eq!(cached_after_reload_tokens, disk_tokens);

    let refresh_result = recovery.next().await?;
    assert_eq!(refresh_result.auth_state_changed(), Some(true));
    assert!(!recovery.has_next());

    let stored = ctx.load_auth()?;
    assert_eq!(stored, disk_auth);

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(requests.is_empty(), "expected no refresh token requests");

    Ok(())
}

#[serial_test::serial(auth_refresh)]
#[tokio::test]
async fn unauthorized_recovery_still_errors_on_account_mismatch() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let ctx = RefreshTokenTestContext::new(&server).await?;
    let initial_last_refresh = Utc::now() - Duration::days(1);
    let initial_tokens = build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(initial_tokens.clone()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;

    let mut disk_tokens = build_tokens("disk-access-token", "disk-refresh-token");
    disk_tokens.account_id = Some("other-account".to_string());
    let disk_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(disk_tokens),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    save_auth(
        ctx.codex_home.path(),
        &disk_auth,
        AuthCredentialsStoreMode::File,
    )?;

    let mut recovery = ctx.auth_manager.unauthorized_recovery();
    assert!(recovery.has_next());

    let err = recovery
        .next()
        .await
        .err()
        .context("recovery should fail due to account mismatch")?;
    assert_eq!(err.failed_reason(), Some(RefreshTokenFailedReason::Other));

    let stored = ctx.load_auth()?;
    assert_eq!(stored, disk_auth);

    let cached_after = ctx
        .auth_manager
        .auth_cached()
        .context("auth should remain cached after mismatch")?;
    let cached_after_tokens = cached_after
        .get_token_data()
        .context("token data should remain cached")?;
    assert_eq!(cached_after_tokens, initial_tokens);

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(requests.is_empty(), "expected no refresh token requests");

    Ok(())
}

struct RefreshTokenTestContext {
    codex_home: TempDir,
    auth_manager: Arc<AuthManager>,
    _env_guard: EnvGuard,
}

impl RefreshTokenTestContext {
    async fn new(server: &MockServer) -> Result<Self> {
        let codex_home = TempDir::new()?;
        let endpoint = format!("{}/oauth/token", server.uri());
        let env_guard = EnvGuard::set(REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR, endpoint);
        let auth_manager = AuthManager::shared(
            codex_home.path().to_path_buf(),
            /*enable_codex_api_key_env*/ false,
            AuthCredentialsStoreMode::File,
            /*chatgpt_base_url*/ None,
        )
        .await;

        Ok(Self {
            codex_home,
            auth_manager,
            _env_guard: env_guard,
        })
    }

    fn load_auth(&self) -> Result<AuthDotJson> {
        load_auth_dot_json(self.codex_home.path(), AuthCredentialsStoreMode::File)
            .context("load auth.json")?
            .context("auth.json should exist")
    }

    async fn write_auth(&self, auth_dot_json: &AuthDotJson) -> Result<()> {
        save_auth(
            self.codex_home.path(),
            auth_dot_json,
            AuthCredentialsStoreMode::File,
        )?;
        self.auth_manager.reload().await;
        Ok(())
    }
}

struct EnvGuard {
    key: &'static str,
    original: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: String) -> Self {
        let original = std::env::var_os(key);
        // SAFETY: these tests execute serially, so updating the process environment is safe.
        unsafe {
            std::env::set_var(key, &value);
        }
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: the guard restores the original environment value before other tests run.
        unsafe {
            match &self.original {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

fn jwt_with_payload(payload: serde_json::Value) -> String {
    #[derive(Serialize)]
    struct Header {
        alg: &'static str,
        typ: &'static str,
    }

    let header = Header {
        alg: "none",
        typ: "JWT",
    };

    fn b64(data: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
    }

    let header_bytes = serde_json::to_vec(&header).expect("serialize header");
    let payload_bytes = serde_json::to_vec(&payload).expect("serialize payload");
    let header_b64 = b64(&header_bytes);
    let payload_b64 = b64(&payload_bytes);
    let signature_b64 = b64(b"sig");
    format!("{header_b64}.{payload_b64}.{signature_b64}")
}

fn minimal_jwt() -> String {
    jwt_with_payload(json!({ "sub": "user-123" }))
}

fn build_tokens(access_token: &str, refresh_token: &str) -> TokenData {
    let id_token = IdTokenInfo {
        raw_jwt: minimal_jwt(),
        ..Default::default()
    };
    TokenData {
        id_token,
        access_token: access_token.to_string(),
        refresh_token: refresh_token.to_string(),
        account_id: Some("account-id".to_string()),
    }
}
