//! OAuth 登录流程：本地回调服务器 + 华为登录页 + token 交换。
//!
//! 从 deveco-local-model-dashboard/src/auth-login.ts 移植。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Notify};

use crate::auth::jwt::{self, JwtPayload, RefreshResult, UserInfo};
use crate::auth::token_store::TokenStore;
use crate::config::{self, LoginConfig};

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("login cancelled by user")]
    Cancelled,
    #[error("unsupported region")]
    UnsupportedRegion,
    #[error("callback server failed: {0}")]
    ServerFailed(String),
    #[error("callback timed out")]
    Timeout,
    #[error("token exchange failed: {0}")]
    TokenExchange(String),
    #[error("http error: {0}")]
    Http(String),
    #[error("invalid callback: {0}")]
    InvalidCallback(String),
}

// ---------------------------------------------------------------------------
// Callback data
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CallbackData {
    pub temp_token: String,
    pub site_id: String,
    pub csrf_token: String,
}

// ---------------------------------------------------------------------------
// Token check response (from Huawei API)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize)]
pub struct TokenCheckResponse {
    pub status: Option<bool>,
    #[serde(default)]
    pub user_info: Option<UserInfoRaw>,
    // Some responses use camelCase
    #[serde(default, rename = "userInfo")]
    pub user_info_camel: Option<UserInfoRaw>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct UserInfoRaw {
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default, rename = "accessToken")]
    pub access_token_camel: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default, rename = "refreshToken")]
    pub refresh_token_camel: Option<String>,
    #[serde(default)]
    pub user_name: Option<String>,
    #[serde(default, rename = "userName")]
    pub user_name_camel: Option<String>,
}

impl UserInfoRaw {
    pub fn access_token(&self) -> Option<&str> {
        self.access_token
            .as_deref()
            .or(self.access_token_camel.as_deref())
    }
    pub fn refresh_token(&self) -> Option<&str> {
        self.refresh_token
            .as_deref()
            .or(self.refresh_token_camel.as_deref())
    }
}

// ---------------------------------------------------------------------------
// Login session (local callback server)
// ---------------------------------------------------------------------------

/// Manages a local HTTP callback server for the OAuth flow.
pub struct LoginSession {
    port: u16,
    client_secret: String,
    callback_rx: Arc<Mutex<Option<CallbackData>>>,
    cancel_notify: Arc<Notify>,
    server_handle: Option<tokio::task::JoinHandle<()>>,
}

impl LoginSession {
    /// Start the callback server on the first available port from CALLBACK_PORTS.
    pub async fn start(cfg: &LoginConfig) -> Result<Self, LoginError> {
        let client_secret = Self::generate_client_secret();
        let callback_rx = Arc::new(Mutex::new(None::<CallbackData>));
        let cancel_notify = Arc::new(Notify::new());

        // Try each port until one works
        for &port in config::CALLBACK_PORTS {
            match Self::try_port(port, callback_rx.clone(), cancel_notify.clone(), &client_secret, cfg).await {
                Ok(handle) => {
                    return Ok(Self {
                        port,
                        client_secret,
                        callback_rx,
                        cancel_notify,
                        server_handle: Some(handle),
                    });
                }
                Err(_) => continue,
            }
        }
        Err(LoginError::ServerFailed("no available port".to_string()))
    }

    /// Get the port the callback server is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Get the client secret for this session.
    pub fn client_secret(&self) -> &str {
        &self.client_secret
    }

    /// Build the Huawei login URL.
    /// Matches TS buildLoginUrl: `?port={port}&appid={appid}&code={clientSecret}`
    pub fn login_url(&self, cfg: &LoginConfig) -> String {
        format!(
            "{}/{}?port={}&appid={}&code={}",
            cfg.base_url, cfg.auth_url, self.port, cfg.app_id, self.client_secret
        )
    }

    /// Validate the login URL before it reaches the OS launcher.
    /// Matches TS validateLoginUrl: reject non-http(s) schemes and disallowed chars.
    pub fn validate_login_url(login_url: &str) -> Result<(), LoginError> {
        let parsed = url::Url::parse(login_url)
            .map_err(|_| LoginError::ServerFailed("invalid login URL".to_string()))?;
        if parsed.scheme() != "https" && parsed.scheme() != "http" {
            return Err(LoginError::ServerFailed(format!(
                "Unsupported login URL scheme: {}",
                parsed.scheme()
            )));
        }
        // Allowlist: A-Za-z0-9._~:/?=&%#@+-
        if !login_url
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._~:/?=&%#@+-".contains(c))
        {
            return Err(LoginError::ServerFailed(
                "Login URL contains disallowed characters".to_string(),
            ));
        }
        Ok(())
    }

    /// Wait for the OAuth callback (blocking, with timeout).
    pub async fn wait_for_callback(&self, timeout: Duration) -> Result<CallbackData, LoginError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let guard = self.callback_rx.lock().await;
                if let Some(data) = guard.clone() {
                    return Ok(data);
                }
            }
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return Err(LoginError::Timeout),
                _ = self.cancel_notify.notified() => return Err(LoginError::Cancelled),
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
        }
    }

    /// Cancel the waiting callback.
    pub fn cancel(&self) {
        self.cancel_notify.notify_one();
    }

    /// Open the login URL in the default browser.
    pub fn open_browser(&self, cfg: &LoginConfig) {
        let url = self.login_url(cfg);
        let _ = open::that(&url);
    }

    fn generate_client_secret() -> String {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        (0..32)
            .map(|_| {
                let c = rng.gen_range(0..36);
                if c < 10 {
                    (b'0' + c) as char
                } else {
                    (b'a' + c - 10) as char
                }
            })
            .collect()
    }

    async fn try_port(
        port: u16,
        callback_rx: Arc<Mutex<Option<CallbackData>>>,
        cancel_notify: Arc<Notify>,
        client_secret: &str,
        cfg: &LoginConfig,
    ) -> Result<tokio::task::JoinHandle<()>, std::io::Error> {
        let addr: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let client_secret = client_secret.to_string();
        let base_url = cfg.base_url.clone();
        let success_url = cfg.success_redirect_url.clone();
        let failed_url = cfg.failed_redirect_url.clone();

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        if result.is_err() { break; }
                        let (mut stream, _) = result.unwrap();
                        let rx = callback_rx.clone();
                        let cs = client_secret.clone();
                        let bu = base_url.clone();
                        let su = success_url.clone();
                        let fu = failed_url.clone();
                        tokio::spawn(async move {
                            handle_callback_connection(&mut stream, rx, &cs, &bu, &su, &fu).await;
                        });
                    }
                    _ = cancel_notify.notified() => break,
                }
            }
        });
        Ok(handle)
    }
}

impl Drop for LoginSession {
    fn drop(&mut self) {
        if let Some(handle) = self.server_handle.take() {
            handle.abort();
        }
    }
}

async fn handle_callback_connection(
    stream: &mut tokio::net::TcpStream,
    callback_rx: Arc<Mutex<Option<CallbackData>>>,
    client_secret: &str,
    base_url: &str,
    success_redirect_url: &str,
    failed_redirect_url: &str,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = vec![0u8; 16384];
    let n = match stream.read(&mut buf).await {
        Ok(n) if n > 0 => n,
        _ => return,
    };
    let request = String::from_utf8_lossy(&buf[..n]).to_string();

    let first_line = request.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0];
    let path = parts[1];

    if !path.starts_with("/callback") {
        let response = "HTTP/1.1 404 Not Found\r\n\r\n";
        let _ = stream.write_all(response.as_bytes()).await;
        return;
    }

    let query = path.split('?').nth(1).unwrap_or("");
    let mut params = parse_query_params(query);

    // POST: body may carry params as URL-encoded form data
    if method == "POST" {
        if let Some(blank_idx) = request.find("\r\n\r\n") {
            let body = &request[blank_idx + 4..];
            if !body.trim().is_empty() {
                for (k, v) in parse_query_params(body) {
                    params.insert(k, v);
                }
            }
        }
    }

    let code = params.get("code").cloned().unwrap_or_default();
    let temp_token = params.get("tempToken").cloned().unwrap_or_default();
    let site_id = params.get("siteId").cloned().unwrap_or_default();
    let quit = params.get("quit").cloned().unwrap_or_default();

    // code must match clientSecret (CSRF protection)
    if code.is_empty() || code != client_secret {
        config::log_warn("login callback: code mismatch or missing, ignoring");
        let response = "HTTP/1.1 200 OK\r\n\r\n";
        let _ = stream.write_all(response.as_bytes()).await;
        return;
    }

    // User cancelled / denied
    if quit == "true" || quit == "access_denied" {
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: {}/{}\r\n\r\n",
            base_url, failed_redirect_url
        );
        let _ = stream.write_all(response.as_bytes()).await;
        return;
    }

    // Missing required params
    if temp_token.is_empty() || site_id.is_empty() {
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: {}/{}\r\n\r\n",
            base_url, failed_redirect_url
        );
        let _ = stream.write_all(response.as_bytes()).await;
        return;
    }

    // Non-China site rejected
    if site_id != "1" {
        config::log_warn(&format!("login callback: unsupported siteId={site_id}"));
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: {}/{}\r\n\r\n",
            base_url, failed_redirect_url
        );
        let _ = stream.write_all(response.as_bytes()).await;
        return;
    }

    // Success
    let data = CallbackData {
        temp_token,
        site_id,
        csrf_token: String::new(),
    };
    {
        let mut guard = callback_rx.lock().await;
        *guard = Some(data);
    }

    let response = format!(
        "HTTP/1.1 302 Found\r\nLocation: {}/{}\r\n\r\n",
        base_url, success_redirect_url
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

fn parse_query_params(query: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for pair in query.split('&') {
        let mut kv = pair.splitn(2, '=');
        if let (Some(k), Some(v)) = (kv.next(), kv.next()) {
            params.insert(k.to_string(), urlencoding::decode(v).unwrap_or_default().into_owned());
        }
    }
    params
}

// ---------------------------------------------------------------------------
// Login service — orchestrates the full login flow
// ---------------------------------------------------------------------------

pub struct LoginService {
    token_store: TokenStore,
    config: LoginConfig,
    session: Arc<Mutex<Option<LoginSession>>>,
    user_info: Arc<Mutex<Option<UserInfo>>>,
    /// Shared HTTP client with connection pool (keep-alive).
    http_client: reqwest::Client,
}

impl LoginService {
    pub fn new(token_store: TokenStore) -> Self {
        let http_client = reqwest::Client::builder()
            .pool_max_idle_per_host(4)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build login HTTP client");
        Self {
            token_store,
            config: LoginConfig::default(),
            session: Arc::new(Mutex::new(None)),
            user_info: Arc::new(Mutex::new(None)),
            http_client,
        }
    }

    pub fn with_config(token_store: TokenStore, config: LoginConfig) -> Self {
        let http_client = reqwest::Client::builder()
            .pool_max_idle_per_host(4)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build login HTTP client");
        Self {
            token_store,
            config,
            session: Arc::new(Mutex::new(None)),
            user_info: Arc::new(Mutex::new(None)),
            http_client,
        }
    }

    /// Start a login session (opens browser) and return the login URL.
    pub async fn start_login(&self, open_browser: bool) -> Result<String, LoginError> {
        let session = LoginSession::start(&self.config).await?;
        let url = session.login_url(&self.config);
        LoginSession::validate_login_url(&url)?;
        if open_browser {
            session.open_browser(&self.config);
        }
        *self.session.lock().await = Some(session);
        Ok(url)
    }

    /// Complete the login flow: wait for callback, exchange tokens, persist.
    pub async fn complete_login(&self) -> Result<UserInfo, LoginError> {
        let session_guard = self.session.lock().await;
        let session = session_guard
            .as_ref()
            .ok_or_else(|| LoginError::ServerFailed("no active login session".to_string()))?;

        let callback = session.wait_for_callback(self.config.timeout).await?;

        // Validate site_id (China site = "1")
        if !callback.site_id.is_empty() && callback.site_id != "1" {
            return Err(LoginError::UnsupportedRegion);
        }

        // Exchange tempToken for jwtToken
        let jwt_token = self.get_jwt_token(&callback.temp_token).await?;

        // Check JWT to get access token
        let check_response = self.check_jwt_token(&jwt_token).await?;
        let access_token = check_response
            .user_info
            .as_ref()
            .or(check_response.user_info_camel.as_ref())
            .and_then(|u| u.access_token().map(|s| s.to_string()))
            .ok_or_else(|| LoginError::TokenExchange("no access_token in response".to_string()))?;

        let refresh_token = check_response
            .user_info
            .as_ref()
            .or(check_response.user_info_camel.as_ref())
            .and_then(|u| u.refresh_token().map(|s| s.to_string()))
            .unwrap_or_default();

        let mut user_name = check_response
            .user_info
            .as_ref()
            .or(check_response.user_info_camel.as_ref())
            .and_then(|u| u.user_name.as_deref().or(u.user_name_camel.as_deref()).map(|s| s.to_string()))
            .unwrap_or_default();
        // Fall back to the JWT payload if the API response omitted userName.
        if user_name.is_empty() {
            if let Ok(payload) = jwt::parse_jwt(&jwt_token) {
                if let Some(name) = payload.user_name {
                    user_name = name;
                }
            }
        }

        let info = UserInfo {
            user_name,
            user_id: String::new(),
            access_token: access_token.clone(),
            refresh_token: refresh_token.clone(),
            jwt_token: jwt_token.clone(),
            country_code: "CN".to_string(),
            language: "zh_CN".to_string(),
            is_real_name: false,
        };

        // Persist jwtToken
        self.token_store.save(&jwt_token).await;

        *self.user_info.lock().await = Some(info.clone());
        Ok(info)
    }

    /// Check if credentials exist (does NOT check JWT expiry — the TS version
    /// reports logged_in whenever a jwtToken is stored, since a silent refresh
    /// may still recover the session).
    pub async fn is_logged_in(&self) -> bool {
        if self.user_info.lock().await.is_some() {
            return true;
        }
        self.token_store.load().await.is_some()
    }

    /// Get cached user info.
    pub async fn get_user_info(&self) -> Option<UserInfo> {
        self.user_info.lock().await.clone()
    }

    /// Logout: clear stored tokens.
    pub async fn logout(&self) {
        self.token_store.clear().await;
        *self.user_info.lock().await = None;
    }

    /// Refresh the access token using the stored jwtToken.
    pub async fn refresh_token(&self) -> Result<RefreshResult, LoginError> {
        let jwt_token = self
            .token_store
            .load()
            .await
            .ok_or_else(|| LoginError::TokenExchange("no stored jwtToken".to_string()))?;

        if jwt::is_expired(&jwt_token) {
            return Err(LoginError::TokenExchange("jwtToken expired".to_string()));
        }

        self.do_refresh(&jwt_token).await
    }

    /// Get the stored jwtToken.
    pub async fn get_jwt(&self) -> Option<String> {
        self.token_store.load().await
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Exchange tempToken for jwtToken (GET, returns plain text).
    /// Mirrors TS getJwtToken(): tempToken is split on `&` (only the first
    /// part is the real token), and the request carries site/version/appid.
    async fn get_jwt_token(&self, temp_token: &str) -> Result<String, LoginError> {
        let actual_temp_token = temp_token.split('&').next().unwrap_or(temp_token);
        let url = format!(
            "{}/{}?tempToken={}&site=CN&version=1.0.0&appid={}",
            self.config.base_url, self.config.temp_token_check_url, actual_temp_token, self.config.app_id
        );
        let client = self.http_client.clone();
        let resp = client
            .get(&url)
            .header("accept-language", "zh-CN")
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| LoginError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(LoginError::TokenExchange(format!(
                "tempToken check failed: HTTP {}",
                resp.status()
            )));
        }

        // Response is plain text (the jwtToken)
        let jwt = resp
            .text()
            .await
            .map_err(|e| LoginError::Http(e.to_string()))?
            .trim()
            .to_string();

        if jwt.is_empty() || jwt.split('.').count() != 3 {
            return Err(LoginError::TokenExchange("invalid jwtToken response".to_string()));
        }
        Ok(jwt)
    }

    /// Check jwtToken and get access token (GET, returns JSON).
    async fn check_jwt_token(&self, jwt_token: &str) -> Result<TokenCheckResponse, LoginError> {
        let url = format!("{}/{}", self.config.base_url, self.config.jwt_token_check_url);
        let client = self.http_client.clone();
        let resp = client
            .get(&url)
            .header("jwtToken", jwt_token)
            .header("refresh", "false")
            .header("accept-language", "zh-CN")
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| LoginError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(LoginError::TokenExchange(format!(
                "jwtToken check failed: HTTP {}",
                resp.status()
            )));
        }

        resp.json::<TokenCheckResponse>()
            .await
            .map_err(|e| LoginError::Http(e.to_string()))
    }

    /// Refresh access token via jwtToken (GET with refresh=true).
    async fn do_refresh(&self, jwt_token: &str) -> Result<RefreshResult, LoginError> {
        let url = format!("{}/{}", self.config.base_url, self.config.jwt_token_check_url);
        let client = self.http_client.clone();
        let resp = client
            .get(&url)
            .header("jwtToken", jwt_token)
            .header("refresh", "true")
            .header("accept-language", "zh-CN")
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| LoginError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(LoginError::TokenExchange(format!(
                "refresh failed: HTTP {}",
                resp.status()
            )));
        }

        let result: TokenCheckResponse = resp
            .json()
            .await
            .map_err(|e| LoginError::Http(e.to_string()))?;

        let user_info = result
            .user_info
            .as_ref()
            .or(result.user_info_camel.as_ref())
            .ok_or_else(|| LoginError::TokenExchange("no userInfo in refresh response".to_string()))?;

        let access_token = user_info
            .access_token()
            .ok_or_else(|| LoginError::TokenExchange("no accessToken in refresh response".to_string()))?
            .to_string();

        let refresh_token = user_info.refresh_token().unwrap_or("").to_string();

        // Update user_info so that /v2/status can display the user name after
        // a restart/refresh (not just after an interactive browser login).
        // The Huawei API response sometimes omits userName even though the JWT
        // payload carries it — fall back to parsing the jwtToken itself.
        let mut user_name = user_info
            .user_name
            .as_deref()
            .or(user_info.user_name_camel.as_deref())
            .unwrap_or("")
            .to_string();
        if user_name.is_empty() {
            if let Ok(payload) = jwt::parse_jwt(jwt_token) {
                if let Some(name) = payload.user_name {
                    user_name = name;
                }
            }
        }
        if !user_name.is_empty() {
            let existing = self.user_info.lock().await.clone();
            let info = UserInfo {
                user_name: user_name.clone(),
                user_id: existing.as_ref().map(|u| u.user_id.clone()).unwrap_or_default(),
                access_token: access_token.clone(),
                refresh_token: refresh_token.clone(),
                jwt_token: existing.as_ref().map(|u| u.jwt_token.clone()).unwrap_or_default(),
                country_code: existing.as_ref().map(|u| u.country_code.clone()).unwrap_or_else(|| "CN".to_string()),
                language: existing.as_ref().map(|u| u.language.clone()).unwrap_or_else(|| "zh_CN".to_string()),
                is_real_name: existing.as_ref().map(|u| u.is_real_name).unwrap_or(false),
            };
            *self.user_info.lock().await = Some(info);
        }

        Ok(RefreshResult {
            access_token,
            refresh_token,
        })
    }
}

// ---------------------------------------------------------------------------
// Real-name auth
// ---------------------------------------------------------------------------

pub const REALNAME_AUTH_URL: &str = "https://cn.devecostudio.huawei.com/console/DevEcoCode/realNameAuth";

pub fn open_realname_auth_page() -> bool {
    open::that(REALNAME_AUTH_URL).is_ok()
}

/// Check if the user has a valid stored jwtToken (without network check).
pub fn has_valid_jwt() -> bool {
    let store = TokenStore::new();
    // We can't do async in a sync function, so use blocking_read
    // This is only used for quick UI checks; the full async path uses LoginService.
    let rt = tokio::runtime::Runtime::new().ok();
    if let Some(rt) = rt {
        rt.block_on(async {
            if let Some(jwt) = store.load().await {
                !jwt::is_expired(&jwt)
            } else {
                false
            }
        })
    } else {
        false
    }
}

/// Run the full login flow (blocking — convenience for CLI).
pub fn run_login_blocking() -> Result<UserInfo, LoginError> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| LoginError::ServerFailed(e.to_string()))?;
    rt.block_on(async {
        let store = TokenStore::new();
        let service = LoginService::new(store);
        let url = service.start_login(true).await?;
        config::log_info(&format!("登录 URL: {url}"));
        config::log_info("请在浏览器中完成登录...");
        service.complete_login().await
    })
}
