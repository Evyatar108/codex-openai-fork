use super::*;
use base64::Engine;
use codex_copilot::CopilotAuth;
use codex_copilot::CopilotHeaderSource;
use codex_copilot::paths::AppPaths;
use pretty_assertions::assert_eq;
use std::fs;
use std::sync::Arc;
use tempfile::tempdir;

#[test]
fn map_api_error_maps_server_overloaded() {
    let err = map_api_error(ApiError::ServerOverloaded);
    assert!(matches!(err, CodexErr::ServerOverloaded));
}

#[test]
fn map_api_error_maps_server_overloaded_from_503_body() {
    let body = serde_json::json!({
        "error": {
            "code": "server_is_overloaded"
        }
    })
    .to_string();
    let err = map_api_error(ApiError::Transport(TransportError::Http {
        status: http::StatusCode::SERVICE_UNAVAILABLE,
        url: Some("http://example.com/v1/responses".to_string()),
        headers: None,
        body: Some(body),
    }));

    assert!(matches!(err, CodexErr::ServerOverloaded));
}

#[test]
fn map_api_error_maps_usage_limit_limit_name_header() {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACTIVE_LIMIT_HEADER,
        http::HeaderValue::from_static("codex_other"),
    );
    headers.insert(
        "x-codex-other-limit-name",
        http::HeaderValue::from_static("codex_other"),
    );
    let body = serde_json::json!({
        "error": {
            "type": "usage_limit_reached",
            "plan_type": "pro",
        }
    })
    .to_string();
    let err = map_api_error(ApiError::Transport(TransportError::Http {
        status: http::StatusCode::TOO_MANY_REQUESTS,
        url: Some("http://example.com/v1/responses".to_string()),
        headers: Some(headers),
        body: Some(body),
    }));

    let CodexErr::UsageLimitReached(usage_limit) = err else {
        panic!("expected CodexErr::UsageLimitReached, got {err:?}");
    };
    assert_eq!(
        usage_limit
            .rate_limits
            .as_ref()
            .and_then(|snapshot| snapshot.limit_name.as_deref()),
        Some("codex_other")
    );
}

#[test]
fn map_api_error_does_not_fallback_limit_name_to_limit_id() {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACTIVE_LIMIT_HEADER,
        http::HeaderValue::from_static("codex_other"),
    );
    let body = serde_json::json!({
        "error": {
            "type": "usage_limit_reached",
            "plan_type": "pro",
        }
    })
    .to_string();
    let err = map_api_error(ApiError::Transport(TransportError::Http {
        status: http::StatusCode::TOO_MANY_REQUESTS,
        url: Some("http://example.com/v1/responses".to_string()),
        headers: Some(headers),
        body: Some(body),
    }));

    let CodexErr::UsageLimitReached(usage_limit) = err else {
        panic!("expected CodexErr::UsageLimitReached, got {err:?}");
    };
    assert_eq!(
        usage_limit
            .rate_limits
            .as_ref()
            .and_then(|snapshot| snapshot.limit_name.as_deref()),
        None
    );
}

#[test]
fn map_api_error_extracts_identity_auth_details_from_headers() {
    let mut headers = HeaderMap::new();
    headers.insert(REQUEST_ID_HEADER, http::HeaderValue::from_static("req-401"));
    headers.insert(CF_RAY_HEADER, http::HeaderValue::from_static("ray-401"));
    headers.insert(
        X_OPENAI_AUTHORIZATION_ERROR_HEADER,
        http::HeaderValue::from_static("missing_authorization_header"),
    );
    let x_error_json =
        base64::engine::general_purpose::STANDARD.encode(r#"{"error":{"code":"token_expired"}}"#);
    headers.insert(
        X_ERROR_JSON_HEADER,
        http::HeaderValue::from_str(&x_error_json).expect("valid x-error-json header"),
    );

    let err = map_api_error(ApiError::Transport(TransportError::Http {
        status: http::StatusCode::UNAUTHORIZED,
        url: Some("https://chatgpt.com/backend-api/codex/models".to_string()),
        headers: Some(headers),
        body: Some(r#"{"detail":"Unauthorized"}"#.to_string()),
    }));

    let CodexErr::UnexpectedStatus(err) = err else {
        panic!("expected CodexErr::UnexpectedStatus, got {err:?}");
    };
    assert_eq!(err.request_id.as_deref(), Some("req-401"));
    assert_eq!(err.cf_ray.as_deref(), Some("ray-401"));
    assert_eq!(
        err.identity_authorization_error.as_deref(),
        Some("missing_authorization_header")
    );
    assert_eq!(err.identity_error_code.as_deref(), Some("token_expired"));
}

#[test]
fn core_auth_provider_reports_when_auth_header_will_attach() {
    let auth = CoreAuthProvider::new_legacy(Some("access-token".to_string()), None);

    assert!(auth.auth_header_attached());
    assert_eq!(auth.auth_header_name(), Some("authorization"));
}

#[test]
fn core_auth_provider_adds_auth_headers() {
    let auth = CoreAuthProvider::for_test(Some("access-token"), Some("workspace-123"));
    let mut headers = HeaderMap::new();

    crate::AuthProvider::add_auth_headers(&auth, &mut headers);

    assert_eq!(
        headers
            .get(http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer access-token")
    );
    assert_eq!(
        headers
            .get("ChatGPT-Account-ID")
            .and_then(|value| value.to_str().ok()),
        Some("workspace-123")
    );
}

#[allow(non_snake_case)]
#[tokio::test]
async fn CoreAuthProvider_with_copilot_injects_session_headers() {
    let (paths, source) = test_copilot_source().await;
    let auth = CoreAuthProvider::new_legacy(None, None).with_copilot(source);
    let mut headers = HeaderMap::new();

    crate::AuthProvider::add_auth_headers(&auth, &mut headers);

    assert!(auth.auth_header_attached());
    assert_eq!(auth.auth_header_name(), Some("authorization"));
    assert_eq!(
        headers
            .get(http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer copilot-token")
    );
    assert_eq!(
        headers
            .get("copilot-integration-id")
            .and_then(|value| value.to_str().ok()),
        Some("vscode-chat")
    );
    assert_eq!(
        headers
            .get("openai-intent")
            .and_then(|value| value.to_str().ok()),
        Some("conversation-agent")
    );
    assert!(
        headers
            .get("vscode-sessionid")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| !value.is_empty())
    );

    let request_id = headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .expect("x-request-id");
    assert_eq!(
        headers
            .get("x-interaction-id")
            .and_then(|value| value.to_str().ok()),
        Some(request_id)
    );
    assert_eq!(
        headers
            .get("x-agent-task-id")
            .and_then(|value| value.to_str().ok()),
        Some(request_id)
    );
    assert!(paths.copilot_token_path.exists());
}

#[tokio::test]
async fn on_unauthorized_clears_copilot_token_file() {
    let (paths, source) = test_copilot_source().await;
    let auth = CoreAuthProvider::new_legacy(None, None).with_copilot(source);

    assert!(paths.copilot_token_path.exists());

    auth.on_unauthorized();

    assert!(!paths.copilot_token_path.exists());
}

async fn test_copilot_source() -> (AppPaths, Arc<CopilotHeaderSource>) {
    let paths = test_copilot_paths();
    fs::write(
        &paths.copilot_token_path,
        r#"{"token":"copilot-token","expires_at":4102444800,"refresh_in":3600}"#,
    )
    .expect("write cached copilot token");

    let auth = Arc::new(
        CopilotAuth::new_for_tests(
            paths.clone(),
            "http://unused.example".to_string(),
            "http://unused.example".to_string(),
            "http://unused.example".to_string(),
        )
        .expect("test copilot auth"),
    );
    let source = Arc::new(
        CopilotHeaderSource::new(auth)
            .await
            .expect("test copilot source"),
    );

    (paths, source)
}

fn test_copilot_paths() -> AppPaths {
    let temp = tempdir().expect("temp dir");
    let app_dir = temp.keep().join("copilot-home");
    fs::create_dir_all(&app_dir).expect("create app dir");
    AppPaths {
        app_dir: app_dir.clone(),
        github_token_path: app_dir.join("github_token"),
        copilot_token_path: app_dir.join("copilot_token"),
        device_id_path: app_dir.join("device_id"),
        machine_id_path: app_dir.join("machine_id"),
    }
}
