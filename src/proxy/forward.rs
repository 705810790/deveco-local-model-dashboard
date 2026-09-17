//! 请求转发：OpenAI Chat / Anthropic / Responses → DevEco 上游。
//!
//! 从 deveco-local-model-dashboard/src/proxy.ts 的 forwardChat / forwardAnthropic / forwardResponses 移植。
//! 核心特性：SSE 心跳（上游排队时防客户端超时）、空闲超时（120s 沉默才断）、
//! 401 重试、exitSessionQueue、TTFB 日志。

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
use crate::proxy::server::{self, ProxyState};
use crate::transform;

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

/// Pipe an upstream response stream through an mpsc sender, with idle timeout.
/// Each chunk resets the idle timer; if no data for `idle_timeout`, we send
/// an in-band SSE error and stop.
async fn pipe_upstream_with_idle(
    upstream: reqwest::Response,
    tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    wire: SseWire,
    idle_timeout: std::time::Duration,
    touch: &mut dyn FnMut(),
) {
    let mut stream = upstream.bytes_stream();
    loop {
        let next_chunk = stream.next();
        let idle = tokio::time::sleep(idle_timeout);
        tokio::pin!(next_chunk, idle);

        tokio::select! {
            chunk = &mut next_chunk => {
                match chunk {
                    Some(Ok(bytes)) => {
                        touch();
                        let b = Bytes::from(bytes.to_vec());
                        if tx.send(Ok(b)).await.is_err() {
                            return; // client disconnected
                        }
                    }
                    Some(Err(e)) => {
                        let msg = format!("upstream stream error: {e}");
                        let _ = tx.send(Ok(Bytes::from(sse_error_bytes(wire, &msg)))).await;
                        return;
                    }
                    None => return, // upstream done
                }
            }
            _ = &mut idle => {
                let msg = format!("upstream silent for {}ms", idle_timeout.as_millis());
                let _ = tx.send(Ok(Bytes::from(sse_error_bytes(wire, &msg)))).await;
                return;
            }
        }
    }
}

/// Queue rejection marker: when this appears in the upstream SSE stream, the
/// request is waiting in line and we must NOT call exitSessionQueue (that slot
/// IS the session's place in line).
const QUEUE_REJECT_MARKER: &str = "\"type\":\"WaitingInLine";

/// Check if a byte chunk contains the queue rejection marker.
fn is_queue_rejection(bytes: &[u8]) -> bool {
    // Simple substring search without full UTF-8 validation
    bytes
        .windows(QUEUE_REJECT_MARKER.len())
        .any(|w| w == QUEUE_REJECT_MARKER.as_bytes())
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
    let body_bytes = match axum::body::to_bytes(req.into_body(), 128 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return server::json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": e.to_string()}),
            )
        }
    };

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

    // 401 retry
    let upstream_resp =
        retry_on_upstream_errors(&state, upstream_resp, &client, &url, &headers, &body_str).await;

    let status = upstream_resp.status();

    // Stream path: commit SSE early with heartbeat, then pipe upstream
    if stream {
        let (resp, tx) = commit_sse_stream("openai");

        // Spawn heartbeat + upstream pipe
        let idle = config::UPSTREAM_IDLE_TIMEOUT;
        let state2 = state.clone();
        let conv_key2 = conv_key.clone();
        let chat_id2 = chat_id.clone();
        let upstream_model2 = upstream_model.clone();
        let model2 = model.clone();
        let t02 = t0;

        tokio::spawn(async move {
            // Heartbeat loop until upstream first byte or done
            let heartbeat = heartbeat_bytes("openai");
            let mut first_byte = true;
            let mut queue_rejected = false;

            if !status.is_success() {
                let text = upstream_resp.text().await.unwrap_or_default();
                let msg = format!(
                    "Upstream returned HTTP {}: {}",
                    status,
                    &text[..text.len().min(500)]
                );
                let _ = tx
                    .send(Ok(Bytes::from(sse_error_bytes("openai", &msg))))
                    .await;
                record_completion(&state2, "openai", &model2, &upstream_model2, t02, None, None, None, None, status.as_u16()).await;
                if !queue_rejected {
                    state2
                        .exit_queue(&conv_key2, &chat_id2, &upstream_model2, &access_token)
                        .await;
                }
                return;
            }

            // Pipe upstream with idle timeout + heartbeat
            let mut stream = upstream_resp.bytes_stream();
            let mut tail_buf = String::new();
            let mut wait_ms: Option<u64> = None;
            let upstream_start = std::time::Instant::now();

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
                                    wait_ms = Some(upstream_start.elapsed().as_millis() as u64);
                                }
                                // Detect queue rejection marker
                                if !queue_rejected && is_queue_rejection(&bytes) {
                                    queue_rejected = true;
                                }
                                // Accumulate tail for usage parsing
                                let s = String::from_utf8_lossy(&bytes);
                                tail_buf.push_str(&s);
                                if tail_buf.len() > 8192 {
                                    // Truncate to last 8192 bytes, but snap to a UTF-8
                                    // char boundary to avoid split_off panic on multi-byte
                                    // characters split across SSE chunks.
                                    let cut = tail_buf.len() - 8192;
                                    let mut safe = cut;
                                    while safe < tail_buf.len() && !tail_buf.is_char_boundary(safe) {
                                        safe += 1;
                                    }
                                    tail_buf = tail_buf.split_off(safe);
                                }
                                let b = Bytes::from(bytes.to_vec());
                                if tx.send(Ok(b)).await.is_err() {
                                    break; // client gone
                                }
                            }
                            Some(Err(e)) => {
                                let msg = format!("upstream stream error: {e}");
                                let _ = tx.send(Ok(Bytes::from(sse_error_bytes("openai", &msg)))).await;
                                break;
                            }
                            None => break, // upstream done
                        }
                    }
                    _ = &mut beat_timer, if first_byte => {
                        let _ = tx.send(Ok(Bytes::from(heartbeat.to_vec()))).await;
                    }
                    _ = &mut idle_timer => {
                        let msg = format!("upstream silent for {}ms", idle.as_millis());
                        let _ = tx.send(Ok(Bytes::from(sse_error_bytes("openai", &msg)))).await;
                        break;
                    }
                }
            }

            let (prompt, completion, backend) = extract_usage(&tail_buf, true);
            record_completion(&state2, "openai", &model2, &upstream_model2, t02, wait_ms, prompt, completion, backend, 200).await;
            if !queue_rejected {
                state2
                    .exit_queue(&conv_key2, &chat_id2, &upstream_model2, &access_token)
                    .await;
            }
        });

        return resp;
    }

    // Non-stream path
    if !status.is_success() {
        let text = upstream_resp.text().await.unwrap_or_default();
        let msg = format!("Upstream returned HTTP {status}: {}", &text[..text.len().min(500)]);
        config::log_error(&msg);
        state
            .exit_queue(&conv_key, &chat_id, &upstream_model, &access_token)
            .await;
        return server::json_response(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            &serde_json::json!({"error": {"message": msg, "type": "api_error"}}),
        );
    }

    let content_type = upstream_resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let body = upstream_resp.bytes().await.unwrap_or_default();
    let body_str = String::from_utf8_lossy(&body).to_string();
    let (prompt, completion, backend) = extract_usage(&body_str, false);
    record_completion(&state, "openai", &model, &upstream_model, t0, None, prompt, completion, backend, 200).await;
    state
        .exit_queue(&conv_key, &chat_id, &upstream_model, &access_token)
        .await;

    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, &content_type)
        .body(Body::from(body))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Forward Anthropic Messages
// ---------------------------------------------------------------------------

pub async fn forward_anthropic(state: ProxyState, req: Request) -> Response {
    let req_headers = req.headers().clone();
    let body_bytes = match axum::body::to_bytes(req.into_body(), 128 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return server::json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"type":"error","error":{"type":"invalid_request_error","message":e.to_string()}}),
            )
        }
    };

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

    let upstream_resp =
        retry_on_upstream_errors(&state, upstream_resp, &client, &url, &headers, &openai_body).await;
    let status = upstream_resp.status();

    if is_stream {
        let (resp, tx) = commit_sse_stream("anthropic");
        let idle = config::UPSTREAM_IDLE_TIMEOUT;
        let state2 = state.clone();
        let conv_key2 = conv_key.clone();
        let chat_id2 = chat_id.clone();
        let upstream_model2 = upstream_model.clone();
        let model2 = model.clone();
        let t02 = t0;

        tokio::spawn(async move {
            if !status.is_success() {
                let text = upstream_resp.text().await.unwrap_or_default();
                let msg = format!("Upstream HTTP {status}: {}", &text[..text.len().min(500)]);
                let _ = tx
                    .send(Ok(Bytes::from(sse_error_bytes("anthropic", &msg))))
                    .await;
                record_completion(&state2, "anthropic", &model2, &upstream_model2, t02, None, None, None, None, status.as_u16()).await;
                state2
                    .exit_queue(&conv_key2, &chat_id2, &upstream_model2, &access_token)
                    .await;
                return;
            }

            // Transform OpenAI SSE → Anthropic SSE
            let upstream_stream = upstream_resp.bytes_stream().map(|result| {
                result
                    .map(|bytes| bytes.to_vec())
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
            });
            let mut anthropic_pipe =
                transform::anthropic::AnthropicStreamPipe::new(upstream_stream, model2.clone());

            let heartbeat = heartbeat_bytes("anthropic");
            let mut first_byte = true;
            let upstream_start = std::time::Instant::now();
            let mut wait_ms: Option<u64> = None;

            use std::pin::Pin;
            use std::task::{Context, Poll};
            use futures::Stream;

            loop {
                // Poll the anthropic stream for next chunk
                let poll_chunk = async {
                    use futures::StreamExt;
                    anthropic_pipe.next().await
                };
                let idle_timer = tokio::time::sleep(idle);
                let beat_timer = if first_byte {
                    tokio::time::sleep(config::SSE_HEARTBEAT_INTERVAL)
                } else {
                    tokio::time::sleep(idle)
                };
                tokio::pin!(poll_chunk, idle_timer, beat_timer);

                tokio::select! {
                    chunk = &mut poll_chunk => {
                        match chunk {
                            Some(Ok(bytes)) => {
                                if first_byte {
                                    first_byte = false;
                                    wait_ms = Some(upstream_start.elapsed().as_millis() as u64);
                                }
                                let b = Bytes::from(bytes);
                                if tx.send(Ok(b)).await.is_err() {
                                    break;
                                }
                            }
                            Some(Err(_)) => break,
                            None => break,
                        }
                    }
                    _ = &mut beat_timer, if first_byte => {
                        let _ = tx.send(Ok(Bytes::from(heartbeat.to_vec()))).await;
                    }
                    _ = &mut idle_timer => {
                        let msg = format!("upstream silent for {}ms", idle.as_millis());
                        let _ = tx.send(Ok(Bytes::from(sse_error_bytes("anthropic", &msg)))).await;
                        break;
                    }
                }
            }

            record_completion(&state2, "anthropic", &model2, &upstream_model2, t02, wait_ms, None, None, None, 200).await;
            state2
                .exit_queue(&conv_key2, &chat_id2, &upstream_model2, &access_token)
                .await;
        });

        return resp;
    }

    // Non-stream
    if !status.is_success() {
        let text = upstream_resp.text().await.unwrap_or_default();
        let msg = format!("Upstream HTTP {status}: {}", &text[..text.len().min(500)]);
        state
            .exit_queue(&conv_key, &chat_id, &upstream_model, &access_token)
            .await;
        return server::json_response(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            &serde_json::json!({"type":"error","error":{"type":"api_error","message":msg}}),
        );
    }

    let openai_resp: serde_json::Value = upstream_resp.json().await.unwrap_or_default();
    let anthropic_resp = transform::anthropic::openai_chat_to_anthropic(&openai_resp, &model);
    let (prompt, completion) = {
        let u = openai_resp.get("usage");
        (u.and_then(|u| u.get("prompt_tokens")).and_then(|v| v.as_u64()),
         u.and_then(|u| u.get("completion_tokens")).and_then(|v| v.as_u64()))
    };
    record_completion(&state, "anthropic", &model, &upstream_model, t0, None, prompt, completion, None, 200).await;
    state
        .exit_queue(&conv_key, &chat_id, &upstream_model, &access_token)
        .await;
    server::json_response(StatusCode::OK, &anthropic_resp)
}

// ---------------------------------------------------------------------------
// Forward Responses API (Codex CLI)
// ---------------------------------------------------------------------------

pub async fn forward_responses(state: ProxyState, req: Request) -> Response {
    let req_headers = req.headers().clone();
    let body_bytes = match axum::body::to_bytes(req.into_body(), 128 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return server::json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": e.to_string()}),
            )
        }
    };

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

    let upstream_resp =
        retry_on_upstream_errors(&state, upstream_resp, &client, &url, &headers, &openai_body).await;
    let status = upstream_resp.status();

    if is_stream {
        let (resp, tx) = commit_sse_stream("responses");
        let idle = config::UPSTREAM_IDLE_TIMEOUT;
        let state2 = state.clone();
        let conv_key2 = conv_key.clone();
        let chat_id2 = chat_id.clone();
        let upstream_model2 = upstream_model.clone();
        let model2 = model.clone();
        let t02 = t0;

        tokio::spawn(async move {
            if !status.is_success() {
                let text = upstream_resp.text().await.unwrap_or_default();
                let msg = format!("Upstream HTTP {status}: {}", &text[..text.len().min(500)]);
                let _ = tx
                    .send(Ok(Bytes::from(sse_error_bytes("responses", &msg))))
                    .await;
                record_completion(&state2, "responses", &model2, &upstream_model2, t02, None, None, None, None, status.as_u16()).await;
                state2
                    .exit_queue(&conv_key2, &chat_id2, &upstream_model2, &access_token)
                    .await;
                return;
            }

            let upstream_stream = upstream_resp.bytes_stream().map(|result| {
                result
                    .map(|bytes| bytes.to_vec())
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
            });
            let mut responses_pipe =
                transform::responses::ResponsesStreamPipe::new(upstream_stream, model2.clone());

            let heartbeat = heartbeat_bytes("responses");
            let mut first_byte = true;
            let upstream_start = std::time::Instant::now();
            let mut wait_ms: Option<u64> = None;

            use futures::StreamExt;

            loop {
                let poll_chunk = async { responses_pipe.next().await };
                let idle_timer = tokio::time::sleep(idle);
                let beat_timer = if first_byte {
                    tokio::time::sleep(config::SSE_HEARTBEAT_INTERVAL)
                } else {
                    tokio::time::sleep(idle)
                };
                tokio::pin!(poll_chunk, idle_timer, beat_timer);

                tokio::select! {
                    chunk = &mut poll_chunk => {
                        match chunk {
                            Some(Ok(bytes)) => {
                                if first_byte {
                                    first_byte = false;
                                    wait_ms = Some(upstream_start.elapsed().as_millis() as u64);
                                }
                                let b = Bytes::from(bytes);
                                if tx.send(Ok(b)).await.is_err() {
                                    break;
                                }
                            }
                            Some(Err(_)) => break,
                            None => break,
                        }
                    }
                    _ = &mut beat_timer, if first_byte => {
                        let _ = tx.send(Ok(Bytes::from(heartbeat.to_vec()))).await;
                    }
                    _ = &mut idle_timer => {
                        let msg = format!("upstream silent for {}ms", idle.as_millis());
                        let _ = tx.send(Ok(Bytes::from(sse_error_bytes("responses", &msg)))).await;
                        break;
                    }
                }
            }

            record_completion(&state2, "responses", &model2, &upstream_model2, t02, wait_ms, None, None, None, 200).await;
            state2
                .exit_queue(&conv_key2, &chat_id2, &upstream_model2, &access_token)
                .await;
        });

        return resp;
    }

    // Non-stream
    if !status.is_success() {
        let text = upstream_resp.text().await.unwrap_or_default();
        let msg = format!("Upstream HTTP {status}: {}", &text[..text.len().min(500)]);
        state
            .exit_queue(&conv_key, &chat_id, &upstream_model, &access_token)
            .await;
        return server::json_response(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            &serde_json::json!({"error": {"message": msg, "type": "api_error", "code": status.as_u16().to_string()}}),
        );
    }

    let openai_resp: serde_json::Value = upstream_resp.json().await.unwrap_or_default();
    let responses_resp = transform::responses::openai_chat_to_responses(&openai_resp, &model);
    let (prompt, completion) = {
        let u = openai_resp.get("usage");
        (u.and_then(|u| u.get("prompt_tokens")).and_then(|v| v.as_u64()),
         u.and_then(|u| u.get("completion_tokens")).and_then(|v| v.as_u64()))
    };
    record_completion(&state, "responses", &model, &upstream_model, t0, None, prompt, completion, None, 200).await;
    state
        .exit_queue(&conv_key, &chat_id, &upstream_model, &access_token)
        .await;
    server::json_response(StatusCode::OK, &responses_resp)
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

    // Try streaming first (cheap first-byte check)
    let stream_url = format!("{origin}{prefix}/chat/completions");
    let stream_body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "你好"}],
        "max_tokens": 8,
        "reasoning_effort": "low",
        "stream": true,
    });

    let stream_result = tokio::time::timeout(
        config::PROBE_TIMEOUT,
        client
            .post(&stream_url)
            .headers(headers.clone())
            .body(stream_body.to_string())
            .send(),
    )
    .await;

    let latency_ms = started.elapsed().as_millis() as u64;

    match stream_result {
        Ok(Ok(resp)) if resp.status().is_success() => {
            // Read first byte to confirm
            let _ = resp.bytes().await;
            let latency = started.elapsed().as_millis() as u64;
            state
                .exit_queue(&probe_key, &probe_chat_id, &model, &access_token)
                .await;
            server::json_response(
                StatusCode::OK,
                &serde_json::json!({"ok": true, "model": model, "channel": "maas HTTP", "latency_ms": latency}),
            )
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

            match no_stream_result {
                Ok(Ok(resp)) if resp.status().is_success() => {
                    let _ = resp.json::<serde_json::Value>().await;
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
            }
        }
        Err(_) => {
            // Stream timed out → don't fall back (same queue)
            state
                .exit_queue(&probe_key, &probe_chat_id, &model, &access_token)
                .await;
            server::json_response(
                StatusCode::BAD_GATEWAY,
                &serde_json::json!({"error": {"message": "probe timed out (upstream queueing)", "type": "probe_error"}}),
            )
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
