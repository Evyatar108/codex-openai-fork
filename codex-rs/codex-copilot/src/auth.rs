use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use anyhow::anyhow;
use reqwest::header::ACCEPT;
use reqwest::header::AUTHORIZATION;
use reqwest::header::CONTENT_TYPE;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::paths::AppPaths;
use crate::payload::Initiator;

const GITHUB_BASE_URL: &str = "https://github.com";
const GITHUB_API_BASE_URL: &str = "https://api.github.com";
const COPILOT_BASE_URL: &str = "https://api.githubcopilot.com";
const GITHUB_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const GITHUB_APP_SCOPES: &str = "read:user";
const API_VERSION: &str = "2025-10-01";
const USER_AGENT: &str = "GitHubCopilotChat/0.38.2";
const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.38.2";
const VSCODE_VERSION: &str = "1.110.1";

#[derive(Clone)]
pub struct CopilotAuth {
    client: reqwest::Client,
    paths: AppPaths,
    device_id: String,
    machine_id: String,
    session_id: String,
    github_base_url: String,
    github_api_base_url: String,
    copilot_base_url: String,
    token_lock: std::sync::Arc<Mutex<()>>,
}

#[derive(Debug, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Debug, Deserialize)]
struct AccessTokenResponse {
    access_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CopilotTokenResponse {
    token: String,
    expires_at: u64,
    refresh_in: u64,
}

#[derive(Debug, Deserialize)]
struct GitHubUser {
    login: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedCopilotToken {
    token: String,
    #[serde(deserialize_with = "deserialize_u64_from_number")]
    expires_at: u64,
    refresh_in: u64,
}

fn deserialize_u64_from_number<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let v = serde_json::Value::deserialize(deserializer)?;
    match v {
        serde_json::Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Ok(u)
            } else if let Some(f) = n.as_f64() {
                Ok(f as u64)
            } else {
                Err(D::Error::custom("expires_at not a representable number"))
            }
        }
        _ => Err(D::Error::custom("expires_at not a number")),
    }
}

impl CopilotAuth {
    pub fn new() -> anyhow::Result<Self> {
        let paths = AppPaths::from_env()?;
        Self::new_with_settings(
            paths,
            GITHUB_BASE_URL.to_string(),
            GITHUB_API_BASE_URL.to_string(),
            COPILOT_BASE_URL.to_string(),
        )
    }

    fn new_with_settings(
        paths: AppPaths,
        github_base_url: String,
        github_api_base_url: String,
        copilot_base_url: String,
    ) -> anyhow::Result<Self> {
        fs::create_dir_all(&paths.app_dir)
            .with_context(|| format!("creating app dir {}", paths.app_dir.display()))?;
        let device_id = load_or_create_uuid(&paths.device_id_path)?;
        let machine_id = load_or_create_machine_id(&paths.machine_id_path)?;
        let session_id = format!("{}{}", Uuid::new_v4(), now_epoch_millis());
        let client = codex_client::build_reqwest_client_with_custom_ca(reqwest::Client::builder())
            .context("building Copilot auth client")?;

        Ok(Self {
            client,
            paths,
            device_id,
            machine_id,
            session_id,
            github_base_url,
            github_api_base_url,
            copilot_base_url,
            token_lock: std::sync::Arc::new(Mutex::new(())),
        })
    }

    #[doc(hidden)]
    pub fn new_for_tests(
        paths: AppPaths,
        github_base_url: String,
        github_api_base_url: String,
        copilot_base_url: String,
    ) -> anyhow::Result<Self> {
        Self::new_with_settings(
            paths,
            github_base_url,
            github_api_base_url,
            copilot_base_url,
        )
    }

    pub async fn login(&self, force: bool) -> anyhow::Result<Option<String>> {
        if !force && self.read_github_token()?.is_some() {
            return self.fetch_user_login().await;
        }

        let response = self.request_device_code().await?;
        println!(
            "Please enter the code \"{}\" in {}",
            response.user_code, response.verification_uri
        );
        let token = self.poll_access_token(&response).await?;
        self.write_github_token(&token)?;
        self.invalidate_cached_copilot_token()?;
        self.fetch_user_login().await
    }

    pub async fn copilot_token(&self, force_refresh: bool) -> anyhow::Result<String> {
        let _guard = self.token_lock.lock().await;

        if !force_refresh
            && let Some(cached) = self.read_cached_copilot_token()?
            && cached.expires_at > now_epoch_seconds() + 60
        {
            return Ok(cached.token);
        }

        let github_token = self
            .read_github_token()?
            .ok_or_else(|| anyhow!("GitHub token not found. Run: codex-copilot-gateway login"))?;

        let response = self
            .client
            .get(format!(
                "{}/copilot_internal/v2/token",
                self.github_api_base_url
            ))
            .headers(self.github_headers(&github_token)?)
            .send()
            .await
            .context("requesting Copilot token")?;

        if !response.status().is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "(unable to read body)".to_string());
            return Err(anyhow!("failed to get Copilot token: {body}"));
        }

        let token = response
            .json::<CopilotTokenResponse>()
            .await
            .context("decoding Copilot token response")?;
        self.write_cached_copilot_token(&CachedCopilotToken {
            token: token.token.clone(),
            expires_at: token.expires_at,
            refresh_in: token.refresh_in,
        })?;
        Ok(token.token)
    }

    pub async fn request_headers(
        &self,
        initiator: Initiator,
        request_id: &str,
        interaction_id: Option<&str>,
        vision: bool,
        force_token_refresh: bool,
    ) -> anyhow::Result<HeaderMap> {
        let copilot_token = self.copilot_token(force_token_refresh).await?;
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {copilot_token}"))?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(
            "copilot-integration-id",
            HeaderValue::from_static("vscode-chat"),
        );
        headers.insert(
            "editor-version",
            HeaderValue::from_str(&format!("vscode/{VSCODE_VERSION}"))?,
        );
        headers.insert(
            "editor-plugin-version",
            HeaderValue::from_static(EDITOR_PLUGIN_VERSION),
        );
        headers.insert("user-agent", HeaderValue::from_static(USER_AGENT));
        headers.insert(
            "openai-intent",
            HeaderValue::from_static("conversation-agent"),
        );
        headers.insert(
            "x-github-api-version",
            HeaderValue::from_static(API_VERSION),
        );
        headers.insert(
            "x-vscode-user-agent-library-version",
            HeaderValue::from_static("electron-fetch"),
        );
        headers.insert("x-request-id", HeaderValue::from_str(request_id)?);
        headers.insert("x-agent-task-id", HeaderValue::from_str(request_id)?);
        headers.insert("x-initiator", HeaderValue::from_static(initiator.as_str()));
        headers.insert(
            "x-interaction-type",
            HeaderValue::from_static(match initiator {
                Initiator::Agent => "conversation-agent",
                Initiator::User => "conversation-user",
            }),
        );
        headers.insert("vscode-machineid", HeaderValue::from_str(&self.machine_id)?);
        headers.insert("vscode-sessionid", HeaderValue::from_str(&self.session_id)?);
        headers.insert(
            "x-codex-copilot-device-id",
            HeaderValue::from_str(&self.device_id)?,
        );
        if let Some(interaction_id) = interaction_id {
            headers.insert("x-interaction-id", HeaderValue::from_str(interaction_id)?);
        }
        if vision {
            headers.insert("copilot-vision-request", HeaderValue::from_static("true"));
        }

        Ok(headers)
    }

    pub fn copilot_base_url(&self) -> &str {
        &self.copilot_base_url
    }

    pub fn invalidate_cached_copilot_token(&self) -> anyhow::Result<()> {
        if self.paths.copilot_token_path.exists() {
            fs::remove_file(&self.paths.copilot_token_path).with_context(|| {
                format!(
                    "removing cached Copilot token {}",
                    self.paths.copilot_token_path.display()
                )
            })?;
        }
        Ok(())
    }

    async fn request_device_code(&self) -> anyhow::Result<DeviceCodeResponse> {
        let response = self
            .client
            .post(format!("{}/login/device/code", self.github_base_url))
            .headers(standard_headers()?)
            .json(&serde_json::json!({
                "client_id": GITHUB_CLIENT_ID,
                "scope": GITHUB_APP_SCOPES,
            }))
            .send()
            .await
            .context("requesting GitHub device code")?;

        if !response.status().is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "(unable to read body)".to_string());
            return Err(anyhow!("failed to get GitHub device code: {body}"));
        }

        response
            .json::<DeviceCodeResponse>()
            .await
            .context("decoding GitHub device code response")
    }

    async fn poll_access_token(&self, device_code: &DeviceCodeResponse) -> anyhow::Result<String> {
        let deadline = now_epoch_seconds() + device_code.expires_in;
        let interval = std::cmp::max(device_code.interval, 1) + 1;

        loop {
            if now_epoch_seconds() >= deadline {
                return Err(anyhow!(
                    "GitHub device login expired before authorization completed"
                ));
            }

            let response = self
                .client
                .post(format!("{}/login/oauth/access_token", self.github_base_url))
                .headers(standard_headers()?)
                .json(&serde_json::json!({
                    "client_id": GITHUB_CLIENT_ID,
                    "device_code": device_code.device_code,
                    "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
                }))
                .send()
                .await
                .context("polling GitHub access token")?;

            if response.status().is_success() {
                let body = response
                    .json::<AccessTokenResponse>()
                    .await
                    .context("decoding GitHub access token response")?;
                if let Some(token) = body.access_token {
                    return Ok(token);
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        }
    }

    async fn fetch_user_login(&self) -> anyhow::Result<Option<String>> {
        let Some(github_token) = self.read_github_token()? else {
            return Ok(None);
        };
        let response = self
            .client
            .get(format!("{}/user", self.github_api_base_url))
            .headers(self.github_headers(&github_token)?)
            .send()
            .await
            .context("requesting authenticated GitHub user")?;
        if !response.status().is_success() {
            return Ok(None);
        }
        let user = response
            .json::<GitHubUser>()
            .await
            .context("decoding GitHub user response")?;
        Ok(Some(user.login))
    }

    fn github_headers(&self, github_token: &str) -> anyhow::Result<HeaderMap> {
        let mut headers = standard_headers()?;
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("token {github_token}"))?,
        );
        headers.insert(
            "editor-version",
            HeaderValue::from_str(&format!("vscode/{VSCODE_VERSION}"))?,
        );
        headers.insert(
            "editor-plugin-version",
            HeaderValue::from_static(EDITOR_PLUGIN_VERSION),
        );
        headers.insert("user-agent", HeaderValue::from_static(USER_AGENT));
        headers.insert(
            "x-github-api-version",
            HeaderValue::from_static(API_VERSION),
        );
        headers.insert(
            "x-vscode-user-agent-library-version",
            HeaderValue::from_static("electron-fetch"),
        );
        Ok(headers)
    }

    fn read_github_token(&self) -> anyhow::Result<Option<String>> {
        read_trimmed_file(&self.paths.github_token_path)
    }

    fn write_github_token(&self, token: &str) -> anyhow::Result<()> {
        write_secret_file(
            &self.paths.github_token_path,
            format!("{token}\n").as_bytes(),
        )
        .with_context(|| {
            format!(
                "writing GitHub token to {}",
                self.paths.github_token_path.display()
            )
        })
    }

    fn read_cached_copilot_token(&self) -> anyhow::Result<Option<CachedCopilotToken>> {
        let Some(raw) = read_trimmed_file(&self.paths.copilot_token_path)? else {
            return Ok(None);
        };
        let parsed = serde_json::from_str(&raw).context("decoding cached Copilot token")?;
        Ok(Some(parsed))
    }

    fn write_cached_copilot_token(&self, token: &CachedCopilotToken) -> anyhow::Result<()> {
        let encoded = serde_json::to_vec(token).context("encoding cached Copilot token")?;
        write_secret_file(&self.paths.copilot_token_path, &encoded).with_context(|| {
            format!(
                "writing cached Copilot token to {}",
                self.paths.copilot_token_path.display()
            )
        })
    }
}

fn write_secret_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    Ok(())
}

fn load_or_create_uuid(path: &std::path::Path) -> anyhow::Result<String> {
    if let Some(existing) = read_trimmed_file(path)? {
        return Ok(existing);
    }

    let value = Uuid::new_v4().to_string().to_lowercase();
    fs::write(path, format!("{value}\n"))
        .with_context(|| format!("writing identifier {}", path.display()))?;
    Ok(value)
}

fn load_or_create_machine_id(path: &std::path::Path) -> anyhow::Result<String> {
    if let Some(existing) = read_trimmed_file(path)? {
        return Ok(existing);
    }

    let source = format!(
        "{}-{}",
        std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| Uuid::new_v4().to_string()),
        Uuid::new_v4()
    );
    let digest = Sha256::digest(source.as_bytes());
    let value = format!("{digest:x}");
    fs::write(path, format!("{value}\n"))
        .with_context(|| format!("writing machine identifier {}", path.display()))?;
    Ok(value)
}

fn read_trimmed_file(path: &std::path::Path) -> anyhow::Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("reading file {}", path.display()))?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(trimmed))
}

fn standard_headers() -> anyhow::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    Ok(headers)
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn now_epoch_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tempfile::tempdir;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    use super::CachedCopilotToken;
    use super::CopilotAuth;
    use crate::paths::AppPaths;

    #[tokio::test]
    async fn login_persists_github_token_and_returns_user() {
        let server = MockServer::start().await;
        let auth = test_auth(&server);

        Mock::given(method("POST"))
            .and(path("/login/device/code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "device_code": "device-code",
                "user_code": "user-code",
                "verification_uri": "https://github.com/login/device",
                "expires_in": 600,
                "interval": 0
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "github-token"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "login": "octocat"
            })))
            .mount(&server)
            .await;

        let login = auth.login(/*force*/ true).await.expect("login succeeds");

        assert_eq!(login, Some("octocat".to_string()));
        assert_eq!(
            fs::read_to_string(auth.paths.github_token_path.clone()).expect("read github token"),
            "github-token\n"
        );
    }

    #[tokio::test]
    async fn copilot_token_fetches_and_caches_token() {
        let server = MockServer::start().await;
        let auth = test_auth(&server);

        fs::write(auth.paths.github_token_path.clone(), "github-token\n")
            .expect("write github token");

        Mock::given(method("GET"))
            .and(path("/copilot_internal/v2/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "token": "copilot-token",
                "expires_at": 4_102_444_800u64,
                "refresh_in": 3600u64
            })))
            .mount(&server)
            .await;

        let token = auth
            .copilot_token(/*force_refresh*/ false)
            .await
            .expect("copilot token");

        assert_eq!(token, "copilot-token");
        let cached = serde_json::from_slice::<CachedCopilotToken>(
            &fs::read(auth.paths.copilot_token_path.clone()).expect("read cached token"),
        )
        .expect("decode cached token");
        assert_eq!(cached.token, "copilot-token");
    }

    fn test_auth(server: &MockServer) -> CopilotAuth {
        let temp = tempdir().expect("temp dir");
        let app_dir = temp.keep().join("copilot-home");
        fs::create_dir_all(&app_dir).expect("create app dir");
        let paths = AppPaths {
            app_dir: app_dir.clone(),
            github_token_path: app_dir.join("github_token"),
            copilot_token_path: app_dir.join("copilot_token"),
            device_id_path: app_dir.join("device_id"),
            machine_id_path: app_dir.join("machine_id"),
        };
        CopilotAuth::new_for_tests(paths, server.uri(), server.uri(), server.uri())
            .expect("test auth")
    }
}
