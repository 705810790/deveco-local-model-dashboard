//! HTTP 代理服务器：axum 路由 + 请求槽位 + token 管理 + SSE 心跳。
//!
//! 从 deveco-local-model-dashboard/src/proxy.ts 的 DevEcoProxy 类移植。

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use std::net::SocketAddr;
use tokio::sync::{Mutex, RwLock, Semaphore};

use crate::auth::login::LoginService;
use crate::auth::token_store::TokenStore;
use crate::config;
use crate::models;

use super::forward;

// ---------------------------------------------------------------------------
// Proxy state (shared across all request handlers)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ProxyState {
    pub login_service: Arc<LoginService>,
    pub token_store: Arc<TokenStore>,
    /// Shared HTTP client with connection pool (keep-alive).
    /// TS uses Node's built-in fetch which reuses connections automatically;
    /// reqwest::Client::new() per-request would open a fresh TCP+TLS connection
    /// every time, exhausting the upstream's connection limit on long tasks.
    pub http_client: reqwest::Client,
    /// Current access token session (token + expiry).
    pub session: Arc<RwLock<Option<AccessTokenSession>>>,
    /// Active request count for concurrency limiting + status display.
    active_requests: Arc<std::sync::atomic::AtomicU32>,
    queued_requests: Arc<std::sync::atomic::AtomicU32>,
    /// Per-conversation Chat-Id map (conv_key → chat_id).
    chat_ids: Arc<Mutex<HashMap<String, String>>>,
    /// Bind hostname (for LAN access toggle).
    hostname: Arc<RwLock<String>>,
    /// Signal: rebind the TCP listener to a new host (LAN toggle).
    pub rebind_tx: Arc<tokio::sync::watch::Sender<Option<String>>>,
    /// Single-flight refresh: concurrent callers share one in-flight refresh.
    /// Uses a serialization mutex — the second caller waits, then re-checks
    /// the session (likely already fresh from the first caller's refresh).
    refresh_lock: Arc<Mutex<()>>,
    /// Timestamp of last refresh failure (epoch ms); 0 = no recent failure.
    last_refresh_failed_at: Arc<std::sync::atomic::AtomicI64>,
    /// Timestamp of last auto-triggered login (epoch ms); 0 = never.
    last_login_triggered_at: Arc<std::sync::atomic::AtomicI64>,
    /// Single-flight login: prevents concurrent browser logins.
    login_inflight: Arc<Mutex<bool>>,
    /// Concurrency limiter: max 4 concurrent upstream requests, rest queue.
    request_slots: Arc<Semaphore>,
    /// Recent request log (ring buffer, max 50 entries, in-memory only).
    recent_requests: Arc<Mutex<std::collections::VecDeque<RecentRequest>>>,
    pub port: u16,
    pub allow_machine_control: bool,
    pub max_concurrent: usize,
}

/// A single recent request summary for the dashboard log.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RecentRequest {
    /// Epoch millis.
    pub ts: u64,
    /// "openai" | "anthropic" | "responses"
    pub protocol: String,
    pub model: String,
    /// HTTP status code from upstream.
    pub status: u16,
    /// Total duration in ms.
    pub duration_ms: u64,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
}

const RECENT_REQUEST_CAP: usize = 50;

#[derive(Clone)]
pub struct AccessTokenSession {
    pub token: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

impl ProxyState {
    pub fn new(port: u16, hostname: String, allow_machine_control: bool) -> Self {
        let token_store = Arc::new(TokenStore::new());
        let login_service = Arc::new(LoginService::new((*token_store).clone()));
        let max_concurrent = 4usize;
        let (rebind_tx, _) = tokio::sync::watch::channel(None);
        // Shared HTTP client with connection pool — mirrors Node's built-in
        // fetch which reuses TCP+TLS connections. Without this, every request
        // opens a fresh connection, exhausting the upstream's limit on long tasks.
        // NO global timeout: TS uses idleBudget (120s silence) instead of a total
        // duration cap — a long agentic turn can legitimately stream for minutes.
        let http_client = reqwest::Client::builder()
            .pool_max_idle_per_host(8)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client");
        Self {
            login_service,
            token_store,
            http_client,
            session: Arc::new(RwLock::new(None)),
            active_requests: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            queued_requests: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            chat_ids: Arc::new(Mutex::new(HashMap::new())),
            hostname: Arc::new(RwLock::new(hostname)),
            rebind_tx: Arc::new(rebind_tx),
            refresh_lock: Arc::new(Mutex::new(())),
            last_refresh_failed_at: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            last_login_triggered_at: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            login_inflight: Arc::new(Mutex::new(false)),
            request_slots: Arc::new(Semaphore::new(max_concurrent)),
            recent_requests: Arc::new(Mutex::new(std::collections::VecDeque::with_capacity(RECENT_REQUEST_CAP))),
            port,
            allow_machine_control,
            max_concurrent,
        }
    }

    /// Get a receiver for rebind signals (used by main.rs rebind loop).
    pub fn rebind_receiver(&self) -> tokio::sync::watch::Receiver<Option<String>> {
        self.rebind_tx.subscribe()
    }

    /// Ensure we have a valid access token (refresh if needed).
    /// With `allow_login = true` (default), a missing credential starts a browser login.
    /// With `allow_login = false`, returns an empty token instead of popping a browser.
    pub async fn ensure_token(&self) -> Result<String, String> {
        self.ensure_token_inner(true).await
    }

    /// Like ensure_token but never triggers a browser login.
    pub async fn ensure_token_no_login(&self) -> Result<String, String> {
        self.ensure_token_inner(false).await
    }

    async fn ensure_token_inner(&self, allow_login: bool) -> Result<String, String> {
        // Check current session
        {
            let guard = self.session.read().await;
            if let Some(ref sess) = *guard {
                if chrono::Utc::now() < sess.expires_at {
                    return Ok(sess.token.clone());
                }
            }
        }

        // Try refresh via jwtToken (with cooldown + single-flight)
        let jwt_token = self.token_store.load().await;
        if jwt_token.is_some() {
            // Acquire refresh lock (single-flight): concurrent callers wait,
            // then re-check the session — likely already fresh from the first.
            let _refresh_guard = self.refresh_lock.lock().await;

            // Re-check session after acquiring lock — another caller may have refreshed
            {
                let guard = self.session.read().await;
                if let Some(ref sess) = *guard {
                    if chrono::Utc::now() < sess.expires_at {
                        return Ok(sess.token.clone());
                    }
                }
            }

            // Cooldown: 30s after a recent failure
            let now = chrono::Utc::now().timestamp_millis();
            let last_fail = self.last_refresh_failed_at.load(std::sync::atomic::Ordering::Relaxed);
            if last_fail > 0 && now - last_fail < 30_000 {
                config::log_warn("token refresh skipped: in cooldown after recent failure");
            } else {
                match self.login_service.refresh_token().await {
                    Ok(refresh) => {
                        self.last_refresh_failed_at.store(0, std::sync::atomic::Ordering::Relaxed);
                        let expires_at = chrono::Utc::now() + config::access_token_expiry_jitter();
                        let mut guard = self.session.write().await;
                        *guard = Some(AccessTokenSession {
                            token: refresh.access_token.clone(),
                            expires_at,
                        });
                        config::log_info("refreshed DevEco access token");
                        return Ok(refresh.access_token);
                    }
                    Err(e) => {
                        config::log_warn(&format!("token refresh failed: {e}"));
                        self.last_refresh_failed_at.store(now, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        }

        if !allow_login {
            return Ok(String::new());
        }

        // Login throttle: 5 min cooldown
        let now = chrono::Utc::now().timestamp_millis();
        let last = self.last_login_triggered_at.load(std::sync::atomic::Ordering::Relaxed);
        if last > 0 && now - last < 300_000 {
            return Err("DevEco login required (auto-login throttled). Visit GET /v2/login to start a new login.".to_string());
        }

        // Single-flight login
        {
            let mut inflight = self.login_inflight.lock().await;
            if *inflight {
                return Err("DevEco login already in progress. Visit GET /v2/login in your browser.".to_string());
            }
            *inflight = true;
        }

        self.last_login_triggered_at.store(now, std::sync::atomic::Ordering::Relaxed);
        config::log_info("no valid DevEco token; starting browser login");

        match self.login_service.start_login(true).await {
            Ok(url) => {
                let svc = self.login_service.clone();
                let login_inflight = self.login_inflight.clone();
                tokio::spawn(async move {
                    match svc.complete_login().await {
                        Ok(info) => config::log_info(&format!("login completed: {}", info.user_name)),
                        Err(e) => config::log_error(&format!("login failed: {e}")),
                    }
                    *login_inflight.lock().await = false;
                });
                Err(format!("DevEco login required. A browser should have opened — if it did not, visit: {url}"))
            }
            Err(e) => {
                *self.login_inflight.lock().await = false;
                Err(format!("DevEco login required: {e}"))
            }
        }
    }

    /// Try to restore a session from the stored jwtToken on startup.
    /// Best-effort: failures are logged and ignored (the next request will
    /// trigger a browser login). Mirrors TS tryRestoreSession().
    pub async fn try_restore_session(&self) {
        let jwt_token = match self.token_store.load().await {
            Some(t) => t,
            None => return,
        };
        match self.login_service.refresh_token().await {
            Ok(refresh) => {
                let user_info = self.login_service.get_user_info().await;
                let expires_at = chrono::Utc::now() + config::access_token_expiry_jitter();
                let mut guard = self.session.write().await;
                *guard = Some(AccessTokenSession {
                    token: refresh.access_token.clone(),
                    expires_at,
                });
                let name = user_info
                    .as_ref()
                    .map(|u| u.user_name.as_str())
                    .unwrap_or("?");
                config::log_info(&format!(
                    "restored DevEco session from stored jwtToken (user: {name})"
                ));
            }
            Err(e) => {
                config::log_warn(&format!(
                    "could not restore session from stored jwtToken: {e}"
                ));
            }
        }
    }

    /// Get or create a Chat-Id for a conversation key.
    pub async fn chat_id_for(&self, conv_key: &str) -> String {
        let mut ids = self.chat_ids.lock().await;
        if let Some(id) = ids.get(conv_key) {
            return id.clone();
        }
        // Bound the map: 500 entries, evict oldest (TS behavior)
        if ids.len() >= 500 {
            if let Some(first_key) = ids.keys().next().cloned() {
                ids.remove(&first_key);
            }
        }
        let id = uuid::Uuid::new_v4().to_string().replace('-', "");
        ids.insert(conv_key.to_string(), id.clone());
        id
    }

    /// Release the queue slot this turn held (exitSessionQueue).
    /// Fire-and-forget with one retry, exactly as upstream treats it.
    /// Mirrors TS exitQueue(): POST with modelId, Session-Id, Chat-Id, Bearer.
    pub async fn exit_queue(&self, conv_key: &str, chat_id: &str, model: &str, token: &str) {
        let url = format!(
            "{}?modelId={}",
            config::DEVECO_EXIT_QUEUE_URL,
            urlencoding::encode(model)
        );
        let client = self.http_client.clone();
        let token = token.to_string();
        let chat_id = chat_id.to_string();
        let conv_key = conv_key.to_string();

        // Fire-and-forget: one initial attempt + one retry
        tokio::spawn(async move {
            for attempt in 0..2u32 {
                let resp = client
                    .post(&url)
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Chat-Id", &chat_id)
                    .header("Session-Id", &conv_key)
                    .header("Content-Type", "application/json")
                    .header("accept-language", "zh-CN")
                    .timeout(std::time::Duration::from_secs(5))
                    .send()
                    .await;
                match resp {
                    Ok(r) if r.status().is_success() => {
                        config::log_debug(&format!("exitSessionQueue -> {}", r.status()));
                        return;
                    }
                    Ok(r) => {
                        if attempt == 0 {
                            config::log_warn(&format!(
                                "exitSessionQueue -> HTTP {}, retrying",
                                r.status()
                            ));
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        } else {
                            config::log_warn(&format!(
                                "exitSessionQueue -> HTTP {}",
                                r.status()
                            ));
                        }
                    }
                    Err(e) => {
                        if attempt == 0 {
                            config::log_warn(&format!(
                                "exitSessionQueue failed, retrying: {e}"
                            ));
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        } else {
                            config::log_warn(&format!("exitSessionQueue failed: {e}"));
                        }
                    }
                }
            }
        });
    }

    pub fn active_count(&self) -> u32 {
        self.active_requests
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn queued_count(&self) -> u32 {
        self.queued_requests
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn hostname(&self) -> String {
        self.hostname.read().await.clone()
    }

    /// Acquire a request slot (concurrency-limited). Returns a guard that
    /// releases the slot on drop. While waiting, `queued_requests` is
    /// incremented; once acquired, `active_requests` is incremented.
    ///
    /// Mirrors TS requestSlot()/withRequestSlot().
    pub async fn request_slot(&self) -> Result<RequestSlotGuard, String> {
        self.queued_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let permit = self.request_slots.clone().acquire_owned().await
            .map_err(|_| "request slot closed".to_string())?;
        self.queued_requests.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        self.active_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(RequestSlotGuard {
            active: self.active_requests.clone(),
            _permit: permit,
        })
    }

    /// Record a completed request into the in-memory ring buffer (max 50).
    pub async fn record_request(&self, req: RecentRequest) {
        let mut buf = self.recent_requests.lock().await;
        if buf.len() >= RECENT_REQUEST_CAP {
            buf.pop_front();
        }
        buf.push_back(req);
    }

    /// Get all recent requests (newest last).
    pub async fn get_recent_requests(&self) -> Vec<RecentRequest> {
        self.recent_requests.lock().await.iter().cloned().collect()
    }
}

/// RAII guard: releases the active-request counter and semaphore permit on drop.
pub struct RequestSlotGuard {
    active: Arc<std::sync::atomic::AtomicU32>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for RequestSlotGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn build_router(state: ProxyState) -> Router {
    Router::new()
        // Management endpoints (with /v2 prefix)
        .route("/v2/status", get(handle_status))
        .route("/v2/login", get(handle_login))
        .route("/v2/logout", get(handle_logout))
        .route("/v2/models", get(handle_models))
        .route("/v2/probe", post(handle_probe))
        .route("/v2/network", get(handle_network_get))
        .route("/v2/network", post(handle_network_post))
        .route("/v2/shutdown", post(handle_shutdown))
        .route("/v2/autostart", get(handle_autostart_get))
        .route("/v2/autostart", post(handle_autostart_post))
        .route("/v2/lan-check", post(handle_lan_check))
        .route("/v2/lan-fix-firewall", post(handle_lan_fix))
        .route("/v2/dashboard", get(handle_dashboard))
        .route("/v2/recent-requests", get(handle_recent_requests))
        .route("/v2/status-stream", get(handle_status_stream))
        .route("/v2/", get(handle_dashboard))
        .route("/v2", get(handle_dashboard))
        // Chat forwarding (with /v2 prefix)
        .route("/v2/chat/completions", post(handle_chat_completions))
        .route("/v2/anthropic/v1/messages", post(handle_anthropic))
        .route("/v2/responses", post(handle_responses))
        // No-prefix routes (TS strips /v2, so both work)
        .route("/status", get(handle_status))
        .route("/login", get(handle_login))
        .route("/logout", get(handle_logout))
        .route("/models", get(handle_models))
        .route("/probe", post(handle_probe))
        .route("/network", get(handle_network_get))
        .route("/network", post(handle_network_post))
        .route("/shutdown", post(handle_shutdown))
        .route("/autostart", get(handle_autostart_get))
        .route("/autostart", post(handle_autostart_post))
        .route("/lan-check", post(handle_lan_check))
        .route("/lan-fix-firewall", post(handle_lan_fix))
        .route("/dashboard", get(handle_dashboard))
        .route("/recent-requests", get(handle_recent_requests))
        .route("/status-stream", get(handle_status_stream))
        .route("/", get(handle_dashboard))
        // Chat forwarding (no prefix)
        .route("/chat/completions", post(handle_chat_completions))
        .route("/anthropic/v1/messages", post(handle_anthropic))
        .route("/responses", post(handle_responses))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Handlers — management endpoints
// ---------------------------------------------------------------------------

async fn handle_status(State(state): State<ProxyState>) -> Response {
    // TS: hasCredentials = (await this.tokenStore.load()) !== null
    //     loggedIn = !!this.session || hasCredentials
    let has_credentials = state.token_store.load().await.is_some();
    let logged_in = {
        let guard = state.session.read().await;
        guard.is_some() || has_credentials
    };
    // TS: user = this.session?.userInfo?.userName ?? null
    let user = state.login_service.get_user_info().await;
    let session_guard = state.session.read().await;
    let expires_in_ms = if let Some(ref sess) = *session_guard {
        let now = chrono::Utc::now();
        (sess.expires_at - now).num_milliseconds().max(0) as u64
    } else {
        0
    };
    drop(session_guard);

    let body = serde_json::json!({
        "logged_in": logged_in,
        "user": user.as_ref().map(|u| serde_json::Value::String(u.user_name.clone())).unwrap_or(serde_json::Value::Null),
        "expires_in_ms": expires_in_ms,
        "active_requests": state.active_count(),
        "queued_requests": state.queued_count(),
        "max_concurrent": state.max_concurrent as u32,
    });
    json_response(StatusCode::OK, &body)
}

async fn handle_login(State(state): State<ProxyState>) -> Response {
    match state.login_service.start_login(false).await {
        Ok(url) => {
            // Start a background task to complete the login
            let svc = state.login_service.clone();
            tokio::spawn(async move {
                match svc.complete_login().await {
                    Ok(info) => config::log_info(&format!("login completed: {}", info.user_name)),
                    Err(e) => config::log_error(&format!("login failed: {e}")),
                }
            });
            // 302 redirect to the Huawei login page (TS behavior).
            // Also include login_url in JSON body for non-redirecting clients (curl).
            Response::builder()
                .status(StatusCode::FOUND)
                .header("Location", &url)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::json!({ "login_url": url }).to_string()))
                .unwrap()
        }
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &serde_json::json!({ "error": e.to_string() }),
        ),
    }
}

async fn handle_logout(State(state): State<ProxyState>) -> Response {
    state.login_service.logout().await;
    // TS: this.session = null; resetModelCache()
    *state.session.write().await = None;
    models::reset_model_cache();
    json_response(StatusCode::OK, &serde_json::json!({ "ok": true }))
}

async fn handle_models(State(state): State<ProxyState>) -> Response {
    // Listing models must not pop a browser: use no-login variant.
    let token = match state.ensure_token_no_login().await {
        Ok(t) => t,
        Err(_) => String::new(),
    };
    let cfg = models::get_deveco_provider_config(&token).await;
    let data: Vec<_> = cfg
        .models
        .unwrap_or_default()
        .iter()
        .map(|(id, info)| {
            let mut entry = serde_json::json!({ "id": id, "object": "model" });
            if let Some(ref limit) = info.limit {
                entry["limit"] = serde_json::to_value(limit).unwrap_or(serde_json::Value::Null);
            }
            entry
        })
        .collect();
    json_response(
        StatusCode::OK,
        &serde_json::json!({ "object": "list", "data": data }),
    )
}

async fn handle_probe(State(state): State<ProxyState>, req: Request) -> Response {
    forward::probe_model(state, req).await
}

async fn handle_network_get(State(state): State<ProxyState>) -> Response {
    let host = state.hostname().await;
    json_response(
        StatusCode::OK,
        &serde_json::json!({
            "lan_access": host == "0.0.0.0",
            "bind_host": host,
            "port": state.port,
        }),
    )
}

async fn handle_network_post(State(state): State<ProxyState>, req: Request) -> Response {
    let body = axum::body::to_bytes(req.into_body(), 1024 * 1024).await;
    let enabled = body
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("lan_access").and_then(|v| v.as_bool()))
        .unwrap_or(false);

    let new_host = if enabled { "0.0.0.0" } else { "127.0.0.1" };
    *state.hostname.write().await = new_host.to_string();

    // Signal main.rs to rebind the TCP listener to the new host.
    let _ = state.rebind_tx.send(Some(new_host.to_string()));

    json_response(
        StatusCode::ACCEPTED,
        &serde_json::json!({ "ok": true, "changing": true, "lan_access": enabled, "port": state.port }),
    )
}

async fn handle_shutdown(
    State(state): State<ProxyState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    if !machine_control_gate(&state, addr) {
        return json_response(
            StatusCode::FORBIDDEN,
            &serde_json::json!({ "error": { "message": "machine control is disabled or non-loopback", "type": "forbidden" } }),
        );
    }
    config::log_info("shutdown requested via dashboard; stopping proxy");
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        std::process::exit(0);
    });
    json_response(StatusCode::OK, &serde_json::json!({ "ok": true, "shutting_down": true }))
}

async fn handle_autostart_get() -> Response {
    let status = crate::win_autostart::get_autostart_status();
    json_response(StatusCode::OK, &serde_json::to_value(status).unwrap_or_default())
}

async fn handle_autostart_post(
    State(state): State<ProxyState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
) -> Response {
    if !machine_control_gate(&state, addr) {
        return json_response(
            StatusCode::FORBIDDEN,
            &serde_json::json!({ "error": { "message": "machine control is disabled or non-loopback", "type": "forbidden" } }),
        );
    }

    let body = axum::body::to_bytes(req.into_body(), 1024 * 1024).await;
    let enabled = body
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("enabled").and_then(|v| v.as_bool()));
    let enabled = match enabled {
        Some(b) => b,
        None => return json_response(StatusCode::BAD_REQUEST, &serde_json::json!({ "error": { "message": "enabled must be a boolean", "type": "invalid_request_error" } })),
    };

    // Windows uses the Run-key command builder; Linux/macOS build the unit/plist internally.
    #[cfg(windows)]
    let command = crate::win_autostart::build_autostart_command(
        &std::env::current_exe().unwrap_or_default().to_string_lossy(),
        &["--no-dashboard".to_string()],
    );
    #[cfg(not(windows))]
    let command = String::new();

    let status = crate::win_autostart::set_autostart(enabled, &command);
    json_response(
        if status.error.is_some() { StatusCode::INTERNAL_SERVER_ERROR } else { StatusCode::OK },
        &serde_json::to_value(status).unwrap_or_default(),
    )
}

async fn handle_lan_check(State(state): State<ProxyState>) -> Response {
    let host = state.hostname().await;
    let result = crate::lan::lan_check(state.port, host == "0.0.0.0").await;
    json_response(StatusCode::OK, &serde_json::to_value(result).unwrap_or_default())
}

async fn handle_lan_fix(
    State(state): State<ProxyState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    if !machine_control_gate(&state, addr) {
        return json_response(
            StatusCode::FORBIDDEN,
            &serde_json::json!({ "error": { "message": "machine control is disabled or non-loopback", "type": "forbidden" } }),
        );
    }
    let result = crate::lan::request_firewall_fix(state.port).await;
    json_response(
        if result.ok { StatusCode::OK } else { StatusCode::INTERNAL_SERVER_ERROR },
        &serde_json::to_value(result).unwrap_or_default(),
    )
}

async fn handle_dashboard(State(state): State<ProxyState>) -> Response {
    let host = state.hostname().await;
    let html = crate::dashboard::render_dashboard(state.port, &host);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store, no-cache, must-revalidate")
        .body(Body::from(html))
        .unwrap()
}

async fn handle_recent_requests(State(state): State<ProxyState>) -> Response {
    let items = state.get_recent_requests().await;
    json_response(StatusCode::OK, &serde_json::json!({ "items": items }))
}

/// SSE stream that pushes status + recent-requests every 5s (and on connect).
/// Replaces front-end polling for `/v2/status` + `/v2/recent-requests`.
async fn handle_status_stream(State(state): State<ProxyState>) -> Response {
    use tokio_stream::StreamExt;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(16);

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;

            // Build status snapshot
            let has_credentials = state.token_store.load().await.is_some();
            let logged_in = {
                let guard = state.session.read().await;
                guard.is_some() || has_credentials
            };
            let user = state.login_service.get_user_info().await;
            let session_guard = state.session.read().await;
            let expires_in_ms = if let Some(ref sess) = *session_guard {
                let now = chrono::Utc::now();
                (sess.expires_at - now).num_milliseconds().max(0) as u64
            } else {
                0
            };
            drop(session_guard);

            let status = serde_json::json!({
                "logged_in": logged_in,
                "user": user.as_ref().map(|u| serde_json::Value::String(u.user_name.clone())).unwrap_or(serde_json::Value::Null),
                "expires_in_ms": expires_in_ms,
                "active_requests": state.active_count(),
                "queued_requests": state.queued_count(),
                "max_concurrent": state.max_concurrent as u32,
            });
            let recent = state.get_recent_requests().await;
            let payload = serde_json::json!({ "status": status, "recent": recent });

            let msg = format!("data: {}\n\n", payload);
            if tx.send(Ok(bytes::Bytes::from(msg))).await.is_err() {
                break; // client disconnected
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("connection", "keep-alive")
        .body(Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Handlers — chat forwarding
// ---------------------------------------------------------------------------

async fn handle_chat_completions(State(state): State<ProxyState>, req: Request) -> Response {
    let _slot = match state.request_slot().await {
        Ok(g) => g,
        Err(e) => {
            return json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &serde_json::json!({"error": {"message": e, "type": "server_busy"}}),
            )
        }
    };
    forward::forward_chat(state, req).await
}

async fn handle_anthropic(State(state): State<ProxyState>, req: Request) -> Response {
    let _slot = match state.request_slot().await {
        Ok(g) => g,
        Err(e) => {
            return json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &serde_json::json!({"error": {"message": e, "type": "server_busy"}}),
            )
        }
    };
    forward::forward_anthropic(state, req).await
}

async fn handle_responses(State(state): State<ProxyState>, req: Request) -> Response {
    let _slot = match state.request_slot().await {
        Ok(g) => g,
        Err(e) => {
            return json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &serde_json::json!({"error": {"message": e, "type": "server_busy"}}),
            )
        }
    };
    forward::forward_responses(state, req).await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn json_response(status: StatusCode, body: &serde_json::Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Conversation key from request body — SHA256 of the first *user* message.
///
/// Mirrors TS conversationKey(): the conversation's opening user message never
/// changes as it grows, so hashing only that keeps the Chat-Id stable even when
/// the system prompt carries volatile content (date, cwd, …). The OpenAI wire
/// format puts the system prompt at messages[0], so we skip to the first
/// role=="user" message. Anthropic keeps `system` in its own field, so the
/// first user message works for both wire formats.
///
/// Set DEVECO_SESSION_KEY_MODE=system-first to key on [systemText, firstUser]
/// instead (the old behaviour).
pub fn conversation_key(body: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};

    let mode = std::env::var("DEVECO_SESSION_KEY_MODE")
        .unwrap_or_else(|_| "first-message".to_string())
        .to_lowercase();

    let messages = body.get("messages").and_then(|m| m.as_array());

    // Find the first user message (stable anchor — not messages[0] which may
    // be a volatile system prompt on the OpenAI wire).
    let first_user = messages.and_then(|msgs| {
        msgs.iter().find(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"))
    });

    // system-first mode also keys on the system prompt.
    let system_text = if mode == "system-first" {
        // Anthropic: top-level `system` field (string)
        let top_system = body.get("system").and_then(|v| v.as_str()).unwrap_or("");
        // OpenAI: first role=="system" message content (string)
        let msg_system = messages
            .and_then(|msgs| {
                msgs.iter().find(|m| m.get("role").and_then(|v| v.as_str()) == Some("system"))
            })
            .and_then(|m| m.get("content"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !top_system.is_empty() { top_system } else { msg_system }
    } else {
        ""
    };

    let head = if mode == "system-first" {
        // [systemText, firstUserMessage] — matches TS JSON.stringify([systemText, firstUserMessage])
        let user_json = first_user
            .map(|m| serde_json::to_string(m).unwrap_or_default())
            .unwrap_or_default();
        format!("[\"{}\",{}]", system_text, user_json)
    } else {
        first_user
            .map(|m| serde_json::to_string(m).unwrap_or_default())
            .unwrap_or_else(|| serde_json::to_string(body).unwrap_or_default())
    };

    let mut hasher = Sha256::new();
    hasher.update(head.as_bytes());
    let hash = hasher.finalize();
    // Take first 32 hex chars (128 bits) — TS does .slice(0, 32)
    let hex: String = hash.iter().take(16).map(|b| format!("{:02x}", b)).collect();
    hex
}

/// Extract session key from headers.
/// TS checks: x-deveco-session, x-session-affinity, x-session-id.
pub fn session_key_from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-deveco-session")
        .or_else(|| headers.get("x-session-affinity"))
        .or_else(|| headers.get("x-session-id"))
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(|s| s.trim().to_string())
}

/// Check if a remote address is loopback.
pub fn is_loopback(addr: &str) -> bool {
    addr == "127.0.0.1" || addr == "::1" || addr == "::ffff:127.0.0.1"
}

/// Shared gate for machine-control routes: require the standalone proxy
/// (allowMachineControl) AND a loopback caller. Mirrors TS machineControlGate().
pub fn machine_control_gate(state: &ProxyState, addr: SocketAddr) -> bool {
    if !state.allow_machine_control {
        return false;
    }
    addr.ip().is_loopback()
}

/// Build upstream headers for DevEco API — 1:1 replica of the native CodeGenie
/// plugin's headers (verified against a captured native request, see
/// `原生请求报文对齐.md`). The native plugin (langchain4j InnerClient) sends
/// exactly: authorization / chat-id / content-type / lang / user-agent, and
/// does NOT send session-id or accept-language.
///
/// `_session_id` is kept in the signature for call-site compatibility but is
/// intentionally not sent upstream, mirroring the native behavior.
pub fn upstream_headers(access_token: &str, chat_id: &str, _session_id: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {access_token}")).unwrap(),
    );
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert("lang", HeaderValue::from_static("cn"));
    headers.insert("Chat-Id", HeaderValue::from_str(chat_id).unwrap_or(HeaderValue::from_static("")));
    headers.insert("User-Agent", HeaderValue::from_static("langchain4j-openai"));
    headers
}
