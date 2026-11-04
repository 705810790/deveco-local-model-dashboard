//! 请求转发：OpenAI Chat / Anthropic / Responses → DevEco 上游。
//!
//! 从 deveco-local-model-dashboard/src/proxy.ts 的 forwardChat / forwardAnthropic / forwardResponses 移植。
//! 核心特性：SSE 心跳（上游排队时防客户端超时）、空闲超时（120s 沉默才断）、
//! 401 重试、exitSessionQueue、TTFB 日志、排队（WaitingInLine）等待 + 自动重试。
//!
//! 排队语义（见 `queue.rs`）：
//!  - 上游发 WaitingInLine 事件 = 请求在队里：事件被吞掉不透传（harness 不会
//!    因这些非标准事件中断对话），连接保持、客户端心跳照常；
//!  - 队列连接被关闭且始终未被服务 → 等待片刻后自动重新请求（默认 5 次）；
//!  - 重试耗尽仍在排队 → 下发干净的 in-band 错误，且不调 exitSessionQueue
//!    （会话可能仍持有排队位）。

use std::sync::Arc;

use axum::{
    body::Body,
    http::{header, StatusCode},
    extract::Request,
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::config;
use crate::proxy::queue;
use crate::proxy::server::{self, ProxyState};
use crate::transform;

// ---------------------------------------------------------------------------
// Request capture helper
// ---------------------------------------------------------------------------

/// HeaderMap → (name, value) pairs for `config::capture_request`.
fn header_pairs(map: &axum::http::HeaderMap) -> Vec<(String, String)> {
    map.iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// SSE helpers (heartbeat + in-band error)
// ---------------------------------------------------------------------------

/// SSE wire type.
type SseWire = &'static str;

/// Heartbeat bytes for each wire type.
/// - openai/responses: SSE comment `: keepalive\n\n` (ignored by all SSE parsers)
/// - anthropic: `event: ping\ndata: {"type":"ping"}\n\n` (official Anthropic ping)
fn heartbeat_bytes(wire: SseWire) -> &'static [u8] {
    match wire {
        "anthropic" => b"event: ping\ndata: {\"type\":\"ping\"}\n\n",
        _ => b": keepalive\n\n",
    }
}

/// In-band SSE error bytes (after the 200 head is committed).
fn sse_error_bytes(wire: SseWire, message: &str) -> Vec<u8> {
    match wire {
        "anthropic" => format!(
            "event: error\ndata: {}\n\n",
            serde_json::json!({"type":"error","error":{"type":"api_error","message":message}})
        ),
        "responses" => format!(
            "event: error\ndata: {}\n\n",
            serde_json::json!({"type":"error","code":"api_error","message":message})
        ),
        _ => format!(
            "data: {}\n\n",
            serde_json::json!({"error":{"message":message,"type":"api_error"}})
        ),
    }
    .into_bytes()
}

/// Create an SSE response with heartbeat. Returns (Response, Sender).
///
/// The response is committed immediately (200 + text/event-stream). The caller
/// spawns a task that races heartbeats against the upstream fetch; once the
/// upstream arrives, heartbeats stop and real data flows through the sender.
fn commit_sse_stream(
    wire: SseWire,
) -> (
    Response,
    mpsc::Sender<Result<Bytes, std::io::Error>>,
) {
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);
    let resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("connection", "keep-alive")
        .body(Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
        .unwrap();
    (resp, tx)
}

// ---------------------------------------------------------------------------
// Shared forwarding logic
// ---------------------------------------------------------------------------

/// Build the upstream URL (stream vs no-stream path).
fn upstream_url(stream: bool) -> String {
    let (origin, prefix) = config::upstream_origin_and_prefix();
    if stream {
        format!("{origin}{prefix}/chat/completions")
    } else {
        format!("{origin}{prefix}/no-stream/chat/completions")
    }
}

/// Apply reasoning_effort "max" → "high" + thinking toggle (TS contract).
fn apply_max_effort(body: &mut serde_json::Value) {
    if let Some(effort) = body.get("reasoning_effort").and_then(|v| v.as_str()) {
        if effort.eq_ignore_ascii_case("max") {
            body["reasoning_effort"] = serde_json::json!("high");
            if body.get("thinking").is_none() {
                body["thinking"] = serde_json::json!({"type": "enabled"});
            }
        }
    }
}

/// Inject thinking toggle for models that hide reasoning without it.
fn apply_thinking_injection(body: &mut serde_json::Value, model: &str) {
    if config::THINKING_ENABLED_MODELS.contains(&model) && body.get("thinking").is_none() {
        body["thinking"] = serde_json::json!({"type": "enabled"});
    }
}

/// Update session after a successful 401-retry refresh.
async fn update_session_after_refresh(state: &ProxyState, access_token: &str) {
    let expires_at = chrono::Utc::now() + config::access_token_expiry_jitter();
    let mut guard = state.session.write().await;
    *guard = Some(server::AccessTokenSession {
        token: access_token.to_string(),
        expires_at,
    });
}

/// Try a 401 retry: refresh token and re-fetch. Returns the new response on
/// success, or the original 401 response if refresh failed.
/// Retry on transient upstream errors:
/// - 401: refresh token + retry once (existing behavior)
/// - 403: DevEco returns 403 "Built-in model service is currently overloaded"
///   when the maas backend is transiently overloaded. The model is still
///   reachable (probe succeeds) but the forward path gets 403. Retry with
///   exponential backoff (1s, 2s, 4s) up to 3 attempts. Vision-routing 403
///   ("Full inference timed out") is permanent, but vision routing should
///   have prevented it; retrying adds at most 7s of latency in that case.
async fn retry_on_upstream_errors(
    state: &ProxyState,
    upstream_resp: reqwest::Response,
    client: &reqwest::Client,
    upstream_url: &str,
    headers: &axum::http::HeaderMap,
    body: &str,
) -> reqwest::Response {
    if upstream_resp.status() == 401 {
        config::log_warn("upstream 401 → refreshing token, retrying once");
        match state.login_service.refresh_token().await {
            Ok(refresh) => {
                update_session_after_refresh(state, &refresh.access_token).await;
                let mut headers2 = headers.clone();
                headers2.insert(
                    "Authorization",
                    axum::http::HeaderValue::from_str(&format!("Bearer {}", refresh.access_token))
                        .unwrap(),
                );
                match client
                    .post(upstream_url)
                    .headers(headers2)
                    .body(body.to_string())
                    .send()
                    .await
                {
                    Ok(r) => r,
                    Err(_) => upstream_resp,
                }
            }
            Err(_) => upstream_resp,
        }
    } else if upstream_resp.status() == 403 {
        // 403 "overloaded" is transient — retry with exponential backoff.
        // last_resp is always a fresh, unconsumed Response (callers can read its body).
        config::log_warn("upstream 403 → retrying with backoff (up to 3 retries)");
        let mut last_resp = upstream_resp;
        for attempt in 0..3u32 {
            let delay = std::time::Duration::from_secs(1u64 << attempt); // 1s, 2s, 4s
            config::log_warn(&format!(
                "upstream 403 → retry {} in {}s",
                attempt + 1,
                delay.as_secs()
            ));
            tokio::time::sleep(delay).await;
            match client
                .post(upstream_url)
                .headers(headers.clone())
                .body(body.to_string())
                .send()
                .await
            {
                Ok(r) => {
                    if r.status().is_success() || r.status() != 403 {
                        return r;
                    }
                    last_resp = r;
                }
                Err(_) => continue,
            }
        }
        last_resp
    } else {
        upstream_resp
    }
}

// ---------------------------------------------------------------------------
// Queue-wait + retry machinery (streaming & non-stream)
// ---------------------------------------------------------------------------

/// Boxed upstream byte stream in io::Result form (shared by the queue filter
/// and the transform pipes).
type IoByteStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<Vec<u8>, std::io::Error>> + Send>>;

/// How a streaming pipe ended.
#[derive(Debug)]
enum StreamOutcome {
    /// Upstream stream finished normally.
    Ended,
    /// The client disconnected.
    ClientGone,
    /// Upstream stayed silent past the idle budget.
    IdleTimeout,
    /// Upstream stream errored.
    Errored(String),
}

/// Shared context for one streaming forward (all three protocols).
struct StreamFwd {
    state: ProxyState,
    wire: SseWire,
    client: reqwest::Client,
    url: String,
    headers: axum::http::HeaderMap,
    body: String,
    conv_key: String,
    chat_id: String,
    upstream_model: String,
    model: String,
    access_token: String,
    t0: std::time::Instant,
}

/// Pipe a (filtered, possibly transformed) stream into `tx` with heartbeat and
/// idle watchdog.
///
/// - Heartbeats are sent every [`config::SSE_HEARTBEAT_INTERVAL`] until the
///   first byte reaches the client — swallowed queue events do NOT count as a
///   first byte, so a request waiting in line keeps heartbeating.
/// - The idle watchdog only fires when the *upstream* went quiet: queue events
///   are recorded in `qstate` as activity even though nothing is forwarded.
/// - Forwarded bytes are accumulated into `tail` (bounded, char-boundary
///   safe) for later usage extraction; TTFB since `t0` lands in `wait_ms`.
async fn pipe_stream<S>(
    mut stream: S,
    tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    qstate: &queue::QueueState,
    heartbeat: &'static [u8],
    idle: std::time::Duration,
    t0: std::time::Instant,
    tail: &mut String,
    wait_ms: &mut Option<u64>,
) -> StreamOutcome
where
    S: futures::Stream<Item = Result<Vec<u8>, std::io::Error>> + Unpin,
{
    let mut first_byte = true;
    loop {
        let next_chunk = stream.next();
        let idle_timer = tokio::time::sleep(idle);
        let beat_timer = if first_byte {
            tokio::time::sleep(config::SSE_HEARTBEAT_INTERVAL)
        } else {
            tokio::time::sleep(idle) // no heartbeat after first byte
        };
        tokio::pin!(next_chunk, idle_timer, beat_timer);

        tokio::select! {
            chunk = &mut next_chunk => {
                match chunk {
                    Some(Ok(bytes)) => {
                        if first_byte {
                            first_byte = false;
                            *wait_ms = Some(t0.elapsed().as_millis() as u64);
                        }
                        let s = String::from_utf8_lossy(&bytes);
                        tail.push_str(&s);
                        if tail.len() > 8192 {
                            // Truncate to last 8192 bytes, snapping to a UTF-8
                            // char boundary (multi-byte chars can split across
                            // SSE chunks).
                            let cut = tail.len() - 8192;
                            let mut safe = cut;
                            while safe < tail.len() && !tail.is_char_boundary(safe) {
                                safe += 1;
                            }
                            *tail = tail.split_off(safe);
                        }
                        if tx.send(Ok(Bytes::from(bytes))).await.is_err() {
                            return StreamOutcome::ClientGone;
                        }
                    }
                    Some(Err(e)) => return StreamOutcome::Errored(e.to_string()),
                    None => return StreamOutcome::Ended,
                }
            }
            _ = &mut beat_timer, if first_byte => {
                if tx.send(Ok(Bytes::from(heartbeat.to_vec()))).await.is_err() {
                    return StreamOutcome::ClientGone;
                }
            }
            _ = &mut idle_timer => {
                // Swallowed queue events still count as upstream activity —
                // while WaitingInLine lines keep arriving we are in line,
                // not idle.
                if qstate.ms_since_activity() < idle.as_millis() as u64 {
                    continue;
                }
                return StreamOutcome::IdleTimeout;
            }
        }
    }
}

/// Sleep for `delay`, sending heartbeats so the client does not time out.
/// Returns false when the client disconnected meanwhile.
async fn sleep_with_heartbeat(
    tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    delay: std::time::Duration,
    heartbeat: &'static [u8],
) -> bool {
    let deadline = tokio::time::Instant::now() + delay;
    let mut next_beat = tokio::time::Instant::now() + config::SSE_HEARTBEAT_INTERVAL;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return true,
            _ = tokio::time::sleep_until(next_beat) => {
                if tx.send(Ok(Bytes::from(heartbeat.to_vec()))).await.is_err() {
                    return false;
                }
                next_beat += config::SSE_HEARTBEAT_INTERVAL;
            }
        }
    }
}

/// Drive one streaming forward with queue-wait + retry semantics:
///
/// 1. While the upstream sends WaitingInLine queue events, hold our place in
///    line: the filter swallows the events (the harness never sees them) and
///    heartbeats keep the client connection alive.
/// 2. If the queue connection closes (stream end / error / silence) without
///    ever delivering real data, wait briefly and re-request upstream —
///    default 5 times (`DEVECO_QUEUE_RETRIES`).
/// 3. Still queued after all retries → a clean in-band SSE error, and no
///    exitSessionQueue (the session may still hold its place in line).
///
/// `make_stream` wraps the queue-filtered upstream bytes into the protocol's
/// stream shape (identity for OpenAI passthrough, the Anthropic/Responses
/// transform pipes otherwise).
async fn stream_forward_with_queue_retries<S, F>(
    fwd: StreamFwd,
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    first_resp: reqwest::Response,
    make_stream: F,
) where
    S: futures::Stream<Item = Result<Vec<u8>, std::io::Error>> + Unpin,
    F: Fn(queue::QueueFilterStream<IoByteStream>) -> S,
{
    let max_retries = queue::queue_retry_count();
    let retry_delay = queue::queue_retry_delay();
    let heartbeat = heartbeat_bytes(fwd.wire);
    let idle = config::UPSTREAM_IDLE_TIMEOUT;

    let mut resp = retry_on_upstream_errors(
        &fwd.state,
        first_resp,
        &fwd.client,
        &fwd.url,
        &fwd.headers,
        &fwd.body,
    )
    .await;

    let mut attempt: u32 = 0;
    let mut wait_ms: Option<u64> = None;
    let mut queued_ever = false;
    let mut served_ever = false;

    loop {
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let msg = format!(
                "Upstream returned HTTP {status}: {}",
                truncate_utf8(&text, 500)
            );
            config::log_error(&msg);
            let _ = tx
                .send(Ok(Bytes::from(sse_error_bytes(fwd.wire, &msg))))
                .await;
            record_completion(
                &fwd.state, fwd.wire, &fwd.model, &fwd.upstream_model, fwd.t0,
                wait_ms, None, None, None, status.as_u16(),
            )
            .await;
            if !queued_ever {
                fwd.state
                    .exit_queue(&fwd.conv_key, &fwd.chat_id, &fwd.upstream_model, &fwd.access_token)
                    .await;
            }
            return;
        }

        let qstate = Arc::new(queue::QueueState::new());
        let raw: IoByteStream = Box::pin(resp.bytes_stream().map(|r| {
            r.map(|b| b.to_vec())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
        }));
        let filtered = queue::QueueFilterStream::new(raw, qstate.clone());
        let mut stream = make_stream(filtered);

        let mut tail = String::new();
        let outcome =
            pipe_stream(stream, &tx, &qstate, heartbeat, idle, fwd.t0, &mut tail, &mut wait_ms)
                .await;

        let queued_now = qstate.rejected_seen();
        let served_now = qstate.real_data_seen();
        queued_ever = queued_ever || queued_now;
        served_ever = served_ever || served_now;

        // Queue connection closed (ended / errored / went silent) without ever
        // serving us → re-request to rejoin the queue.
        if queued_now && !served_now && attempt < max_retries {
            attempt += 1;
            config::log_info(&format!(
                "upstream queue (WaitingInLine) closed without service; retry {attempt}/{max_retries} in {}ms",
                retry_delay.as_millis()
            ));
            if !sleep_with_heartbeat(&tx, retry_delay, heartbeat).await {
                record_completion(
                    &fwd.state, fwd.wire, &fwd.model, &fwd.upstream_model, fwd.t0,
                    wait_ms, None, None, None, 200,
                )
                .await;
                return;
            }
            match fwd
                .client
                .post(&fwd.url)
                .headers(fwd.headers.clone())
                .body(fwd.body.clone())
                .send()
                .await
            {
                Ok(r) => {
                    resp = retry_on_upstream_errors(
                        &fwd.state, r, &fwd.client, &fwd.url, &fwd.headers, &fwd.body,
                    )
                    .await;
                    continue;
                }
                Err(e) => {
                    let msg = format!("upstream fetch failed: {e}");
                    config::log_error(&msg);
                    let _ = tx
                        .send(Ok(Bytes::from(sse_error_bytes(fwd.wire, &msg))))
                        .await;
                    record_completion(
                        &fwd.state, fwd.wire, &fwd.model, &fwd.upstream_model, fwd.t0,
                        wait_ms, None, None, None, 502,
                    )
                    .await;
                    return;
                }
            }
        }

        // Retries exhausted while still queued — surface a clean in-band
        // error instead of leaking queue events into the harness.
        if queued_ever && !served_ever {
            let msg = if max_retries > 0 {
                format!("模型仍在排队中（已自动重试 {max_retries} 次），请稍后重试")
            } else {
                "模型正在排队中（WaitingInLine），暂不可用".to_string()
            };
            let _ = tx
                .send(Ok(Bytes::from(sse_error_bytes(fwd.wire, &msg))))
                .await;
            record_completion(
                &fwd.state, fwd.wire, &fwd.model, &fwd.upstream_model, fwd.t0,
                wait_ms, None, None, None, 503,
            )
            .await;
            // Deliberately no exitSessionQueue: the session may still hold its
            // place in line; releasing it would drop queue priority.
            return;
        }

        match outcome {
            StreamOutcome::Errored(e) => {
                let msg = format!("upstream stream error: {e}");
                let _ = tx
                    .send(Ok(Bytes::from(sse_error_bytes(fwd.wire, &msg))))
                    .await;
            }
            StreamOutcome::IdleTimeout => {
                let msg = format!("upstream silent for {}ms", idle.as_millis());
                let _ = tx
                    .send(Ok(Bytes::from(sse_error_bytes(fwd.wire, &msg))))
                    .await;
            }
            _ => {}
        }

        let (prompt, completion, backend) = extract_usage(&tail, true);
        record_completion(
            &fwd.state, fwd.wire, &fwd.model, &fwd.upstream_model, fwd.t0,
            wait_ms, prompt, completion, backend, 200,
        )
        .await;
        if served_ever || !queued_ever {
            fwd.state
                .exit_queue(&fwd.conv_key, &fwd.chat_id, &fwd.upstream_model, &fwd.access_token)
                .await;
        }
        return;
    }
}

/// Outcome of a non-stream upstream fetch with queue retry.
enum NoStreamOutcome {
    /// Success — full response body.
    Ok(Bytes),
    /// Still queue-rejected after all automatic retries.
    QueueExhausted { retries: u32 },
    /// Upstream answered with a non-success status (or the fetch failed).
    UpstreamError { status: reqwest::StatusCode, detail: String },
}

/// Non-stream upstream fetch: if the response body carries a WaitingInLine
/// queue rejection, wait and re-request (same retry budget as streaming).
async fn no_stream_with_queue_retries(
    state: &ProxyState,
    client: &reqwest::Client,
    url: &str,
    headers: &axum::http::HeaderMap,
    body: &str,
    first_resp: reqwest::Response,
) -> NoStreamOutcome {
    let max_retries = queue::queue_retry_count();
    let retry_delay = queue::queue_retry_delay();

    let mut resp =
        retry_on_upstream_errors(state, first_resp, client, url, headers, body).await;

    let mut attempt: u32 = 0;
    loop {
        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            return NoStreamOutcome::UpstreamError { status, detail };
        }
        let b = resp.bytes().await.unwrap_or_default();
        if !queue::is_queue_rejection(&b) {
            return NoStreamOutcome::Ok(b);
        }
        if attempt >= max_retries {
            config::log_warn("upstream queue (WaitingInLine) on no-stream; retries exhausted");
            return NoStreamOutcome::QueueExhausted { retries: max_retries };
        }
        attempt += 1;
        config::log_info(&format!(
            "upstream queue (WaitingInLine) on no-stream; retry {attempt}/{max_retries}"
        ));
        tokio::time::sleep(retry_delay).await;
        match client
            .post(url)
            .headers(headers.clone())
            .body(body.to_string())
            .send()
            .await
        {
            Ok(r) => {
                resp = retry_on_upstream_errors(state, r, client, url, headers, body).await;
            }
            Err(e) => {
                return NoStreamOutcome::UpstreamError {
                    status: reqwest::StatusCode::BAD_GATEWAY,
                    detail: e.to_string(),
                }
            }
        }
    }
}

/// UTF-8 safe truncation (multibyte text must not panic on slicing).
fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Extract usage info from SSE tail chunks or non-stream JSON for logging.
fn extract_usage(tail: &str, is_stream: bool) -> (Option<u64>, Option<u64>, Option<String>) {
    if is_stream {
        let data_lines: Vec<&str> = tail
            .lines()
            .filter(|l| l.starts_with("data:"))
            .map(|l| l.trim_start_matches("data:").trim())
            .filter(|l| !l.is_empty() && *l != "[DONE]")
            .collect();
        if let Some(last) = data_lines.last() {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(last) {
                let prompt = json
                    .get("usage")
                    .and_then(|u| u.get("prompt_tokens"))
                    .and_then(|v| v.as_u64());
                let completion = json
                    .get("usage")
                    .and_then(|u| u.get("completion_tokens"))
                    .and_then(|v| v.as_u64());
                let model = json
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                return (prompt, completion, model);
            }
        }
    } else {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(tail) {
            let prompt = json
                .get("usage")
                .and_then(|u| u.get("prompt_tokens"))
                .and_then(|v| v.as_u64());
            let completion = json
                .get("usage")
                .and_then(|u| u.get("completion_tokens"))
                .and_then(|v| v.as_u64());
            let model = json
                .get("model")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            return (prompt, completion, model);
        }
    }
    (None, None, None)
}

// ---------------------------------------------------------------------------
// Forward OpenAI Chat Completions
// ---------------------------------------------------------------------------

pub async fn forward_chat(state: ProxyState, req: Request) -> Response {
    let req_headers = req.headers().clone();
    let req_uri = req.uri().path().to_string();
    let body_bytes = match axum::body::to_bytes(req.into_body(), 128 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return server::json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": e.to_string()}),
            )
        }
    };

    config::capture_request(
        "in",
        "POST",
        &req_uri,
        &header_pairs(&req_headers),
        &String::from_utf8_lossy(&body_bytes),
    );

    let parsed: serde_json::Value =
        serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);
    let stream = parsed.get("stream").and_then(|v| v.as_bool()).unwrap_or(true);
    let model = parsed
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();

    // Session key: prefer explicit header, else conversation key
    let conv_key = server::session_key_from_headers(&req_headers)
        .unwrap_or_else(|| server::conversation_key(&parsed));

    // Token
    let access_token = match state.ensure_token().await {
        Ok(t) => t,
        Err(e) => {
            return server::json_response(
                StatusCode::UNAUTHORIZED,
                &serde_json::json!({"error": {"message": e, "type": "auth_error"}}),
            )
        }
    };

    // Normalize tool_choice
    let mut routed_body = parsed.clone();
    let (normalized, changed) = transform::normalize::normalize_openai_tool_choice(parsed.clone());
    if changed {
        routed_body = normalized;
    }

    // reasoning_effort "max" → "high" + thinking toggle
    apply_max_effort(&mut routed_body);

    // deepseek-v4-flash thinking injection
    apply_thinking_injection(&mut routed_body, &model);

    // Vision routing
    let mut upstream_model = model.clone();
    if let Some(routing) = transform::vision::apply_vision_routing(&routed_body) {
        upstream_model = routing.upstream_model.clone();
        if routing.rerouted || routing.images_stripped {
            routed_body = routing.body.clone();
        }
    }

    let chat_id = state.chat_id_for(&conv_key).await;
    let url = upstream_url(stream);
    let headers = server::upstream_headers(&access_token, &chat_id, &conv_key);
    let body_str = routed_body.to_string();

    config::capture_request("out", "POST", &url, &header_pairs(&headers), &body_str);

    config::log_info(&format!(
        "-> POST {} model={model}{}",
        if stream { "stream" } else { "no-stream" },
        if upstream_model != model {
            format!(" → vision:{upstream_model}")
        } else {
            String::new()
        }
    ));

    let client = state.http_client.clone();
    let t0 = std::time::Instant::now();

    let upstream_resp = match client
        .post(&url)
        .headers(headers.clone())
        .body(body_str.clone())
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("upstream fetch failed: {e}");
            config::log_error(&msg);
            return server::json_response(
                StatusCode::BAD_GATEWAY,
                &serde_json::json!({"error": {"message": msg, "type": "api_error"}}),
            );
        }
    };

    // Stream path: commit SSE early with heartbeat, then pipe upstream
    if stream {
        let (resp, tx) = commit_sse_stream("openai");

        let fwd = StreamFwd {
            state,
            wire: "openai",
            client,
            url,
            headers,
            body: body_str,
            conv_key,
            chat_id,
            upstream_model,
            model,
            access_token,
            t0,
        };

        tokio::spawn(async move {
            // Identity: the queue-filtered upstream bytes are already
            // OpenAI-wire SSE — forward them as-is.
            stream_forward_with_queue_retries(fwd, tx, upstream_resp, |filtered| filtered).await;
        });

        return resp;
    }

    // Non-stream path (with queue retry)
    match no_stream_with_queue_retries(&state, &client, &url, &headers, &body_str, upstream_resp)
        .await
    {
        NoStreamOutcome::Ok(b) => {
            let body_text = String::from_utf8_lossy(&b).to_string();
            let (prompt, completion, backend) = extract_usage(&body_text, false);
            record_completion(
                &state, "openai", &model, &upstream_model, t0, None,
                prompt, completion, backend, 200,
            )
            .await;
            state
                .exit_queue(&conv_key, &chat_id, &upstream_model, &access_token)
                .await;
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b))
                .unwrap()
        }
        NoStreamOutcome::QueueExhausted { retries } => {
            let msg = if retries > 0 {
                format!("模型仍在排队中（已自动重试 {retries} 次），请稍后重试")
            } else {
                "模型正在排队中（WaitingInLine），暂不可用".to_string()
            };
            record_completion(
                &state, "openai", &model, &upstream_model, t0, None,
                None, None, None, 503,
            )
            .await;
            // No exitSessionQueue: the session may still hold its place in line.
            server::json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &serde_json::json!({"error": {"message": msg, "type": "api_error"}}),
            )
        }
        NoStreamOutcome::UpstreamError { status, detail } => {
            let msg = format!(
                "Upstream returned HTTP {status}: {}",
                truncate_utf8(&detail, 500)
            );
            config::log_error(&msg);
            state
                .exit_queue(&conv_key, &chat_id, &upstream_model, &access_token)
                .await;
            server::json_response(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                &serde_json::json!({"error": {"message": msg, "type": "api_error"}}),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Forward Anthropic Messages
// ---------------------------------------------------------------------------

pub async fn forward_anthropic(state: ProxyState, req: Request) -> Response {
    let req_headers = req.headers().clone();
    let req_uri = req.uri().path().to_string();
    let body_bytes = match axum::body::to_bytes(req.into_body(), 128 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return server::json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"type":"error","error":{"type":"invalid_request_error","message":e.to_string()}}),
            )
        }
    };

    config::capture_request(
        "in",
        "POST",
        &req_uri,
        &header_pairs(&req_headers),
        &String::from_utf8_lossy(&body_bytes),
    );

    let anthropic_req: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            return server::json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"type":"error","error":{"type":"invalid_request_error","message":format!("Invalid JSON: {e}")}}),
            )
        }
    };

    let is_stream = anthropic_req
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let model = anthropic_req
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();

    let access_token = match state.ensure_token().await {
        Ok(t) => t,
        Err(e) => {
            return server::json_response(
                StatusCode::UNAUTHORIZED,
                &serde_json::json!({"type":"error","error":{"type":"authentication_error","message":e}}),
            )
        }
    };

    // Transform Anthropic → OpenAI
    let mut openai_req = transform::anthropic::anthropic_to_openai_chat(&anthropic_req);
    apply_thinking_injection(&mut openai_req, &model);

    // Vision routing
    let mut upstream_model = model.clone();
    if let Some(routing) = transform::vision::apply_vision_routing(&openai_req) {
        upstream_model = routing.upstream_model.clone();
        openai_req = routing.body.clone();
    }

    let conv_key = server::session_key_from_headers(&req_headers)
        .unwrap_or_else(|| server::conversation_key(&openai_req));
    let chat_id = state.chat_id_for(&conv_key).await;
    let url = upstream_url(is_stream);
    let headers = server::upstream_headers(&access_token, &chat_id, &conv_key);
    let openai_body = openai_req.to_string();

    config::capture_request("out", "POST", &url, &header_pairs(&headers), &openai_body);

    config::log_info(&format!(
        "-> POST anthropic/{} model={model}{}",
        if is_stream { "stream" } else { "no-stream" },
        if upstream_model != model {
            format!(" → vision:{upstream_model}")
        } else {
            String::new()
        }
    ));

    let client = state.http_client.clone();
    let t0 = std::time::Instant::now();

    let upstream_resp = match client
        .post(&url)
        .headers(headers.clone())
        .body(openai_body.clone())
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return server::json_response(
                StatusCode::BAD_GATEWAY,
                &serde_json::json!({"type":"error","error":{"type":"api_error","message":e.to_string()}}),
            )
        }
    };

    if is_stream {
        let (resp, tx) = commit_sse_stream("anthropic");

        let pipe_model = model.clone();
        let fwd = StreamFwd {
            state,
            wire: "anthropic",
            client,
            url,
            headers,
            body: openai_body,
            conv_key,
            chat_id,
            upstream_model,
            model,
            access_token,
            t0,
        };

        tokio::spawn(async move {
            stream_forward_with_queue_retries(fwd, tx, upstream_resp, |filtered| {
                transform::anthropic::AnthropicStreamPipe::new(filtered, pipe_model.clone())
            })
            .await;
        });

        return resp;
    }

    // Non-stream (with queue retry)
    match no_stream_with_queue_retries(&state, &client, &url, &headers, &openai_body, upstream_resp)
        .await
    {
        NoStreamOutcome::Ok(b) => {
            let openai_resp: serde_json::Value = serde_json::from_slice(&b).unwrap_or_default();
            let anthropic_resp = transform::anthropic::openai_chat_to_anthropic(&openai_resp, &model);
            let (prompt, completion) = {
                let u = openai_resp.get("usage");
                (u.and_then(|u| u.get("prompt_tokens")).and_then(|v| v.as_u64()),
                 u.and_then(|u| u.get("completion_tokens")).and_then(|v| v.as_u64()))
            };
            record_completion(
                &state, "anthropic", &model, &upstream_model, t0, None,
                prompt, completion, None, 200,
            )
            .await;
            state
                .exit_queue(&conv_key, &chat_id, &upstream_model, &access_token)
                .await;
            server::json_response(StatusCode::OK, &anthropic_resp)
        }
        NoStreamOutcome::QueueExhausted { retries } => {
            let msg = if retries > 0 {
                format!("模型仍在排队中（已自动重试 {retries} 次），请稍后重试")
            } else {
                "模型正在排队中（WaitingInLine），暂不可用".to_string()
            };
            record_completion(
                &state, "anthropic", &model, &upstream_model, t0, None,
                None, None, None, 503,
            )
            .await;
            // No exitSessionQueue: the session may still hold its place in line.
            server::json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &serde_json::json!({"type":"error","error":{"type":"api_error","message":msg}}),
            )
        }
        NoStreamOutcome::UpstreamError { status, detail } => {
            let msg = format!(
                "Upstream HTTP {status}: {}",
                truncate_utf8(&detail, 500)
            );
            state
                .exit_queue(&conv_key, &chat_id, &upstream_model, &access_token)
                .await;
            server::json_response(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                &serde_json::json!({"type":"error","error":{"type":"api_error","message":msg}}),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Forward Responses API (Codex CLI)
// ---------------------------------------------------------------------------

pub async fn forward_responses(state: ProxyState, req: Request) -> Response {
    let req_headers = req.headers().clone();
    let req_uri = req.uri().path().to_string();
    let body_bytes = match axum::body::to_bytes(req.into_body(), 128 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return server::json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": e.to_string()}),
            )
        }
    };

    config::capture_request(
        "in",
        "POST",
        &req_uri,
        &header_pairs(&req_headers),
        &String::from_utf8_lossy(&body_bytes),
    );

    let responses_req: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            return server::json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": {"message": format!("Invalid JSON: {e}"), "type": "invalid_request_error", "code": "invalid_json"}}),
            )
        }
    };

    let model = responses_req
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if model.is_empty() {
        return server::json_response(
            StatusCode::BAD_REQUEST,
            &serde_json::json!({"error": {"message": "model is required", "type": "invalid_request_error", "code": "model_missing"}}),
        );
    }

    let is_stream = responses_req
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let access_token = match state.ensure_token().await {
        Ok(t) => t,
        Err(e) => {
            return server::json_response(
                StatusCode::UNAUTHORIZED,
                &serde_json::json!({"error": {"message": e, "type": "authentication_error", "code": "unauthorized"}}),
            )
        }
    };

    // Transform Responses → OpenAI Chat
    let mut openai_req = transform::responses::responses_to_openai_chat(&responses_req);

    // reasoning_effort "max" mapping (same as chat path)
    apply_max_effort(&mut openai_req);
    apply_thinking_injection(&mut openai_req, &model);

    // Vision routing
    let mut upstream_model = model.clone();
    if let Some(routing) = transform::vision::apply_vision_routing(&openai_req) {
        upstream_model = routing.upstream_model.clone();
        openai_req = routing.body.clone();
    }

    let conv_key = server::session_key_from_headers(&req_headers)
        .unwrap_or_else(|| server::conversation_key(&openai_req));
    let chat_id = state.chat_id_for(&conv_key).await;
    let url = upstream_url(is_stream);
    let headers = server::upstream_headers(&access_token, &chat_id, &conv_key);
    let openai_body = openai_req.to_string();

    config::capture_request("out", "POST", &url, &header_pairs(&headers), &openai_body);

    config::log_info(&format!(
        "-> POST responses/{} model={model}{}",
        if is_stream { "stream" } else { "no-stream" },
        if upstream_model != model {
            format!(" → vision:{upstream_model}")
        } else {
            String::new()
        }
    ));

    let client = state.http_client.clone();
    let t0 = std::time::Instant::now();

    let upstream_resp = match client
        .post(&url)
        .headers(headers.clone())
        .body(openai_body.clone())
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return server::json_response(
                StatusCode::BAD_GATEWAY,
                &serde_json::json!({"error": {"message": e.to_string(), "type": "api_error", "code": "upstream_fetch_failed"}}),
            )
        }
    };

    if is_stream {
        let (resp, tx) = commit_sse_stream("responses");

        let pipe_model = model.clone();
        let fwd = StreamFwd {
            state,
            wire: "responses",
            client,
            url,
            headers,
            body: openai_body,
            conv_key,
            chat_id,
            upstream_model,
            model,
            access_token,
            t0,
        };

        tokio::spawn(async move {
            stream_forward_with_queue_retries(fwd, tx, upstream_resp, |filtered| {
                transform::responses::ResponsesStreamPipe::new(filtered, pipe_model.clone())
            })
            .await;
        });

        return resp;
    }

    // Non-stream (with queue retry)
    match no_stream_with_queue_retries(&state, &client, &url, &headers, &openai_body, upstream_resp)
        .await
    {
        NoStreamOutcome::Ok(b) => {
            let openai_resp: serde_json::Value = serde_json::from_slice(&b).unwrap_or_default();
            let responses_resp = transform::responses::openai_chat_to_responses(&openai_resp, &model);
            let (prompt, completion) = {
                let u = openai_resp.get("usage");
                (u.and_then(|u| u.get("prompt_tokens")).and_then(|v| v.as_u64()),
                 u.and_then(|u| u.get("completion_tokens")).and_then(|v| v.as_u64()))
            };
            record_completion(
                &state, "responses", &model, &upstream_model, t0, None,
                prompt, completion, None, 200,
            )
            .await;
            state
                .exit_queue(&conv_key, &chat_id, &upstream_model, &access_token)
                .await;
            server::json_response(StatusCode::OK, &responses_resp)
        }
        NoStreamOutcome::QueueExhausted { retries } => {
            let msg = if retries > 0 {
                format!("模型仍在排队中（已自动重试 {retries} 次），请稍后重试")
            } else {
                "模型正在排队中（WaitingInLine），暂不可用".to_string()
            };
            record_completion(
                &state, "responses", &model, &upstream_model, t0, None,
                None, None, None, 503,
            )
            .await;
            // No exitSessionQueue: the session may still hold its place in line.
            server::json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &serde_json::json!({"error": {"message": msg, "type": "api_error", "code": "queue_exhausted"}}),
            )
        }
        NoStreamOutcome::UpstreamError { status, detail } => {
            let msg = format!(
                "Upstream HTTP {status}: {}",
                truncate_utf8(&detail, 500)
            );
            state
                .exit_queue(&conv_key, &chat_id, &upstream_model, &access_token)
                .await;
            server::json_response(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                &serde_json::json!({"error": {"message": msg, "type": "api_error", "code": status.as_u16().to_string()}}),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Probe model (dashboard "测试在线")
// ---------------------------------------------------------------------------

pub async fn probe_model(state: ProxyState, req: Request) -> Response {
    let body_bytes = axum::body::to_bytes(req.into_body(), 1024 * 1024)
        .await
        .unwrap_or_default();
    let parsed: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap_or_default();
    let model = parsed
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if model.is_empty() {
        return server::json_response(
            StatusCode::BAD_REQUEST,
            &serde_json::json!({"error": {"message": "model is required", "type": "invalid_request_error"}}),
        );
    }

    let access_token = match state.ensure_token().await {
        Ok(t) => t,
        Err(e) => {
            return server::json_response(
                StatusCode::UNAUTHORIZED,
                &serde_json::json!({"error": {"message": e, "type": "auth_error"}}),
            )
        }
    };

    let (origin, prefix) = config::upstream_origin_and_prefix();
    let probe_chat_id = uuid::Uuid::new_v4().to_string().replace('-', "");
    let probe_key = uuid::Uuid::new_v4().to_string().replace('-', "");
    let headers = server::upstream_headers(&access_token, &probe_chat_id, &probe_key);
    let probe_body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "你好"}],
        "max_tokens": 8,
        "reasoning_effort": "low",
    });

    let client = state.http_client.clone();
    let started = std::time::Instant::now();

    // Try streaming first (cheap first-byte check). While the upstream answers
    // with a WaitingInLine queue rejection, retry with the SAME session
    // (DEVECO_QUEUE_RETRIES / DEVECO_QUEUE_RETRY_DELAY_MS) — the queue
    // position advances between attempts, so a retry may get served.
    let stream_url = format!("{origin}{prefix}/chat/completions");
    let stream_body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "你好"}],
        "max_tokens": 8,
        "reasoning_effort": "low",
        "stream": true,
    });

    let max_retries = queue::queue_retry_count();
    let retry_delay = queue::queue_retry_delay();
    let mut stream_attempt: u32 = 0;

    loop {
        let stream_result = tokio::time::timeout(
            config::PROBE_TIMEOUT,
            client
                .post(&stream_url)
                .headers(headers.clone())
                .body(stream_body.to_string())
                .send(),
        )
        .await;

        match stream_result {
            Ok(Ok(resp)) if resp.status().is_success() => {
                // Read full body — SSE streams always return HTTP 200, so we
                // must inspect the content to distinguish a real response from
                // a WaitingInLine queue rejection (which also comes back as 200).
                let body = resp.bytes().await.unwrap_or_default();
                let latency = started.elapsed().as_millis() as u64;
                config::log_debug(&format!(
                    "probe stream body ({}ms, {}B): {}",
                    latency,
                    body.len(),
                    String::from_utf8_lossy(&body[..body.len().min(400)]).replace('\n', "\\n")
                ));
                if queue::is_queue_rejection(&body) {
                    if stream_attempt < max_retries {
                        stream_attempt += 1;
                        config::log_info(&format!(
                            "probe queue rejection ({}ms); retry {}/{} in {:?}",
                            latency, stream_attempt, max_retries, retry_delay
                        ));
                        tokio::time::sleep(retry_delay).await;
                        continue;
                    }
                    let qmsg = queue::find_sse_error(&body)
                        .map(|e| e.message)
                        .unwrap_or_default();
                    let qsuffix = if qmsg.is_empty() {
                        String::new()
                    } else {
                        format!("：{qmsg}")
                    };
                    let msg = if stream_attempt > 0 {
                        format!("模型仍在排队中（已自动重试 {stream_attempt} 次）{qsuffix}")
                    } else {
                        format!("模型正在排队中（WaitingInLine），暂不可用{qsuffix}")
                    };
                    state
                        .exit_queue(&probe_key, &probe_chat_id, &model, &access_token)
                        .await;
                    return server::json_response(
                        StatusCode::OK,
                        &serde_json::json!({"ok": false, "model": model, "channel": "maas HTTP", "latency_ms": latency, "error": {"message": msg, "type": "queue_rejection"}}),
                    );
                }
                state
                    .exit_queue(&probe_key, &probe_chat_id, &model, &access_token)
                    .await;
                if let Some(err) = queue::find_sse_error(&body) {
                    // HTTP 200 + in-band SSE error (e.g. UserSessionLimitExceeded
                    // after rapid probes) — NOT a successful probe.
                    config::log_warn(&format!(
                        "probe stream in-band error ({}ms): {} [{}]",
                        latency, err.message, err.error_type
                    ));
                    return server::json_response(
                        StatusCode::OK,
                        &serde_json::json!({"ok": false, "model": model, "channel": "maas HTTP", "latency_ms": latency, "error": {"message": err.message, "type": err.error_type, "code": err.code}}),
                    );
                }
                let reply = queue::extract_sse_reply(&body);
                return server::json_response(
                    StatusCode::OK,
                    &serde_json::json!({"ok": true, "model": model, "channel": "maas HTTP", "latency_ms": latency, "detail": reply}),
                );
            }
            Ok(Ok(_)) | Ok(Err(_)) => {
                // Stream failed (not timeout) → fall back to no-stream
                let no_stream_url = format!("{origin}{prefix}/no-stream/chat/completions");
                let no_stream_result = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    client
                        .post(&no_stream_url)
                        .headers(headers)
                        .body(probe_body.to_string())
                        .send(),
                )
                .await;

                let latency = started.elapsed().as_millis() as u64;
                state
                    .exit_queue(&probe_key, &probe_chat_id, &model, &access_token)
                    .await;

                return match no_stream_result {
                    Ok(Ok(resp)) if resp.status().is_success() => {
                        let body = resp.bytes().await.unwrap_or_default();
                        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) {
                            if let Some(err) = v.get("error") {
                                let msg = err
                                    .get("message")
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("upstream error");
                                let etype = err
                                    .get("type")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("upstream_error");
                                return server::json_response(
                                    StatusCode::OK,
                                    &serde_json::json!({"ok": false, "model": model, "channel": "maas HTTP", "latency_ms": latency, "error": {"message": msg, "type": etype}}),
                                );
                            }
                            let content = v
                                .get("choices")
                                .and_then(|c| c.as_array())
                                .and_then(|a| a.first())
                                .and_then(|c| c.get("message"))
                                .and_then(|m| m.get("content"))
                                .and_then(|c| c.as_str())
                                .unwrap_or("")
                                .trim()
                                .to_string();
                            return server::json_response(
                                StatusCode::OK,
                                &serde_json::json!({"ok": true, "model": model, "channel": "maas HTTP", "latency_ms": latency, "detail": content}),
                            );
                        }
                        server::json_response(
                            StatusCode::OK,
                            &serde_json::json!({"ok": true, "model": model, "channel": "maas HTTP", "latency_ms": latency}),
                        )
                    }
                    Ok(Ok(resp)) => {
                        let status = resp.status();
                        let detail = resp.text().await.unwrap_or_default();
                        server::json_response(
                            StatusCode::BAD_GATEWAY,
                            &serde_json::json!({"error": {"message": format!("upstream HTTP {status}: {}", &detail[..detail.len().min(300)]), "type": "probe_error"}}),
                        )
                    }
                    Ok(Err(e)) => server::json_response(
                        StatusCode::BAD_GATEWAY,
                        &serde_json::json!({"error": {"message": e.to_string(), "type": "probe_error"}}),
                    ),
                    Err(_) => server::json_response(
                        StatusCode::BAD_GATEWAY,
                        &serde_json::json!({"error": {"message": "probe timed out", "type": "probe_error"}}),
                    ),
                };
            }
            Err(_) => {
                // Stream timed out → don't fall back (same queue)
                state
                    .exit_queue(&probe_key, &probe_chat_id, &model, &access_token)
                    .await;
                return server::json_response(
                    StatusCode::BAD_GATEWAY,
                    &serde_json::json!({"error": {"message": "probe timed out (upstream queueing)", "type": "probe_error"}}),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Logging helper
// ---------------------------------------------------------------------------

fn log_completion(
    model: &str,
    upstream_model: &str,
    t0: std::time::Instant,
    wait_ms: Option<u64>,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    backend_model: Option<String>,
) {
    let dur = t0.elapsed().as_millis();
    let wait_str = match wait_ms {
        Some(w) => format!(" wait={:.1}s", w as f64 / 1000.0),
        None => String::new(),
    };
    let tok_str = match (prompt_tokens, completion_tokens) {
        (Some(p), Some(c)) => format!("in={p} out={c}"),
        (Some(p), None) => format!("in={p} out=?"),
        (None, Some(c)) => format!("in=? out={c}"),
        _ => "tokens=?".to_string(),
    };
    let backend_str = match backend_model {
        Some(b) => format!(" (backend: {b})"),
        None => String::new(),
    };
    config::log_info(&format!(
        "<- 200 {dur}ms{wait_str} {tok_str} model={model}{backend_str}"
    ));
}

/// Log a completion AND record it into the in-memory ring buffer for the dashboard.
async fn record_completion(
    state: &ProxyState,
    protocol: &str,
    model: &str,
    upstream_model: &str,
    t0: std::time::Instant,
    wait_ms: Option<u64>,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    backend_model: Option<String>,
    status: u16,
) {
    log_completion(model, upstream_model, t0, wait_ms, prompt_tokens, completion_tokens, backend_model);
    state.record_request(server::RecentRequest {
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        protocol: protocol.to_string(),
        model: model.to_string(),
        status,
        duration_ms: t0.elapsed().as_millis() as u64,
        prompt_tokens,
        completion_tokens,
    }).await;
}
