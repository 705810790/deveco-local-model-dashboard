//! 排队（WaitingInLine）处理：事件过滤 + 状态跟踪 + 重试配置。
//!
//! DevEco 上游在模型繁忙时通过 SSE 事件 `data: {"type":"WaitingInLine",...}`
//! 告知请求正在排队。直接把这些事件透传给外接 harness 会破坏其 SSE 解析、
//! 中断对话。本模块提供：
//!
//!  1. [`QueueFilterStream`] —— 包在上游字节流外，逐行识别并**吞掉**排队事件
//!     （客户端永远看不到它们），其余字节原样透传；
//!  2. [`QueueState`] —— 排队/真实数据/最近上游活动时间跟踪，供转发层判断
//!     是否仍在队中、是否需要继续心跳、是否值得重试；
//!  3. 重试配置 —— 队列连接被上游关闭且始终未被服务时，自动重新请求
//!     （默认 5 次，`DEVECO_QUEUE_RETRIES` / `DEVECO_QUEUE_RETRY_DELAY_MS`）。

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::stream::Stream;

// ---------------------------------------------------------------------------
// Queue rejection marker
// ---------------------------------------------------------------------------

/// Substring marker carried by upstream queue-rejection SSE events.
pub const QUEUE_REJECT_MARKER: &[u8] = b"\"type\":\"WaitingInLine";

/// Raw substring check on bytes (used by the probe on whole response bodies).
pub fn is_queue_rejection(bytes: &[u8]) -> bool {
    bytes
        .windows(QUEUE_REJECT_MARKER.len())
        .any(|w| w == QUEUE_REJECT_MARKER)
}

// ---------------------------------------------------------------------------
// Retry configuration
// ---------------------------------------------------------------------------

/// How many times we re-request after the upstream queue connection closes
/// without ever serving us. Default 30; override with `DEVECO_QUEUE_RETRIES`.
/// (Busy queues like glm-5.2 / deepseek-v4-flash need ~2s per position —
/// 30 × 2s ≈ 60s covers a ~30-position queue.)
pub fn queue_retry_count() -> u32 {
    std::env::var("DEVECO_QUEUE_RETRIES")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n <= 60)
        .unwrap_or(30)
}

/// Delay between queue retries. Default 2s; override with
/// `DEVECO_QUEUE_RETRY_DELAY_MS`.
pub fn queue_retry_delay() -> Duration {
    std::env::var("DEVECO_QUEUE_RETRY_DELAY_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&ms| ms > 0 && ms <= 600_000)
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_secs(2))
}

// ---------------------------------------------------------------------------
// Line classification
// ---------------------------------------------------------------------------

/// True if a complete SSE line is a queue-rejection `data:` line. Two formats:
///
///  - A: `data: {"type":"WaitingInLine",...}` — top-level `type`;
///  - B: `data: {"error":{"message":"...position N of the queue...","type":"WaitingInLine","code":"406"}}`
///    — nested under `error` (glm-5.2 / deepseek-v4-flash use this), wrapped
///    in `event: error`, and the upstream closes the connection right after.
///
/// The JSON-parse keeps real content lines that merely *embed* the marker
/// string (e.g. a chat answer quoting it) intact — those are forwarded
/// untouched.
fn is_queue_line(line: &[u8]) -> bool {
    let s = match std::str::from_utf8(line) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let trimmed = s.trim();
    let payload = match trimmed.strip_prefix("data:") {
        Some(p) => p.trim(),
        None => return false,
    };
    if payload.is_empty() {
        return false;
    }
    let v = match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(v) => v,
        Err(_) => return false,
    };
    // Format A: top-level {"type":"WaitingInLine",...}
    if let Some(t) = v.get("type").and_then(|t| t.as_str()) {
        if t.starts_with("WaitingInLine") {
            return true;
        }
    }
    // Format B: {"error":{...,"type":"WaitingInLine","code":"406"}}
    if let Some(err) = v.get("error") {
        if let Some(t) = err.get("type").and_then(|t| t.as_str()) {
            if t.starts_with("WaitingInLine") {
                return true;
            }
        }
    }
    false
}

/// True if a complete SSE line carries real model data (a non-empty `data:`
/// line other than `[DONE]`). Used to track "we were actually served".
fn is_real_data_line(line: &[u8]) -> bool {
    let s = match std::str::from_utf8(line) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let trimmed = s.trim();
    match trimmed.strip_prefix("data:") {
        Some(p) => {
            let payload = p.trim();
            !payload.is_empty() && payload != "[DONE]"
        }
        None => false,
    }
}

/// True if a complete line is whitespace-only (SSE event separator).
fn is_blank_line(line: &[u8]) -> bool {
    line.iter().all(|&b| matches!(b, b'\n' | b'\r' | b' ' | b'\t'))
}

// ---------------------------------------------------------------------------
// In-band SSE errors & reply extraction (probe support)
// ---------------------------------------------------------------------------

/// An in-band error event extracted from an upstream SSE body, e.g.
/// `event: error` + `data: {"error":{"message":"...","type":"UserSessionLimitExceeded","code":"403"}}`.
/// The upstream delivers these with HTTP 200, so a status check alone misses them.
pub struct SseError {
    pub message: String,
    pub error_type: String,
    pub code: Option<String>,
}

/// Scan a full SSE body for an in-band error event: a `data:` line whose JSON
/// carries a top-level `error` object and no `choices` (normal completion
/// chunks always carry `choices`). Returns the parsed error when found.
pub fn find_sse_error(body: &[u8]) -> Option<SseError> {
    let s = std::str::from_utf8(body).ok()?;
    for line in s.lines() {
        let trimmed = line.trim();
        let payload = match trimmed.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => continue,
        };
        if payload.is_empty() {
            continue;
        }
        let v = match serde_json::from_str::<serde_json::Value>(payload) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("choices").is_some() {
            continue;
        }
        if let Some(err) = v.get("error") {
            return Some(SseError {
                message: err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("upstream error")
                    .to_string(),
                error_type: err
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("upstream_error")
                    .to_string(),
                code: err.get("code").and_then(|c| c.as_str()).map(String::from),
            });
        }
    }
    None
}

/// Concatenate `choices[].delta.content` from an SSE body — the model's actual
/// reply text. Used by the probe to show what the model really answered.
pub fn extract_sse_reply(body: &[u8]) -> String {
    let mut out = String::new();
    let s = match std::str::from_utf8(body) {
        Ok(s) => s,
        Err(_) => return out,
    };
    for line in s.lines() {
        let trimmed = line.trim();
        let payload = match trimmed.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => continue,
        };
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let v = match serde_json::from_str::<serde_json::Value>(payload) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(choices) = v.get("choices").and_then(|c| c.as_array()) {
            for choice in choices {
                if let Some(content) = choice
                    .get("delta")
                    .and_then(|d| d.get("content"))
                    .and_then(|c| c.as_str())
                {
                    out.push_str(content);
                }
            }
        }
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------------------
// Queue state (shared between the filter and the forwarding loop)
// ---------------------------------------------------------------------------

/// Tracks what the upstream told us on the current connection.
pub struct QueueState {
    rejected_seen: AtomicBool,
    real_data_seen: AtomicBool,
    /// Epoch ms of the last upstream chunk, INCLUDING swallowed queue events.
    /// Queue events keep arriving while we hold our place in line, so the
    /// forwarding loop must not treat that silence-to-us as upstream silence.
    last_activity_ms: AtomicI64,
}

impl QueueState {
    pub fn new() -> Self {
        Self {
            rejected_seen: AtomicBool::new(false),
            real_data_seen: AtomicBool::new(false),
            last_activity_ms: AtomicI64::new(now_ms()),
        }
    }

    pub fn mark_rejected(&self) {
        self.rejected_seen.store(true, Ordering::Relaxed);
    }

    pub fn mark_real_data(&self) {
        self.real_data_seen.store(true, Ordering::Relaxed);
    }

    pub fn touch(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub fn rejected_seen(&self) -> bool {
        self.rejected_seen.load(Ordering::Relaxed)
    }

    pub fn real_data_seen(&self) -> bool {
        self.real_data_seen.load(Ordering::Relaxed)
    }

    /// Milliseconds since the last upstream chunk (queue events included).
    pub fn ms_since_activity(&self) -> u64 {
        (now_ms() - self.last_activity_ms.load(Ordering::Relaxed)).max(0) as u64
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// QueueFilterStream
// ---------------------------------------------------------------------------

/// Max bytes buffered while waiting for a newline — guards against a
/// non-SSE response body so the filter can never stall forever.
const LINE_BUFFER_CAP: usize = 1024 * 1024;

/// Wraps the raw upstream byte stream, swallowing queue-rejection
/// (`WaitingInLine`) SSE lines and passing everything else through verbatim.
///
/// While queue events keep arriving, the request stays in line: the filter
/// produces no output (the client sees only heartbeats from the forwarding
/// layer), but [`QueueState::touch`] keeps recording upstream activity so
/// the idle watchdog does not kill a healthy queue wait.
pub struct QueueFilterStream<S>
where
    S: Stream<Item = Result<Vec<u8>, std::io::Error>> + Unpin,
{
    upstream: S,
    state: Arc<QueueState>,
    buf: Vec<u8>,
    out: Vec<u8>,
    upstream_ended: bool,
    /// Swallow blank lines that directly follow a swallowed queue event —
    /// the SSE event separator belongs to the removed event.
    skip_blank: bool,
    /// Index in `out` where the current (still incomplete) SSE event begins.
    /// When a queue data line shows up, the whole event is dropped — including
    /// its leading `id:` / `event:` / empty `data:` lines that were already
    /// forwarded — by truncating `out` back to this position.
    event_start: usize,
}

impl<S> QueueFilterStream<S>
where
    S: Stream<Item = Result<Vec<u8>, std::io::Error>> + Unpin,
{
    pub fn new(upstream: S, state: Arc<QueueState>) -> Self {
        Self {
            upstream,
            state,
            buf: Vec::new(),
            out: Vec::new(),
            upstream_ended: false,
            skip_blank: false,
            event_start: 0,
        }
    }

    /// Classify buffered bytes line-by-line: queue events (all their lines,
    /// including leading `id:` / `event: error` and trailing separator) are
    /// swallowed, everything else passes through byte-identical.
    fn drain_lines(&mut self) {
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let blank = is_blank_line(&line);
            if is_queue_line(&line) {
                // Drop the whole event: the lines of this event that were
                // already forwarded (`id:`, `event:`, empty `data:`) go too.
                self.out.truncate(self.event_start);
                self.state.mark_rejected();
                self.skip_blank = true;
                continue;
            }
            if self.skip_blank && blank {
                continue;
            }
            self.skip_blank = false;
            if is_real_data_line(&line) {
                self.state.mark_real_data();
            }
            self.out.extend_from_slice(&line);
            if blank {
                // Event boundary forwarded — the next event starts here.
                self.event_start = self.out.len();
            }
        }
        // Pathological guard: over 1MB without a newline — flush as-is.
        if self.buf.len() > LINE_BUFFER_CAP {
            self.state.mark_real_data();
            self.out.append(&mut self.buf);
        }
    }

    /// Flush an unterminated tail at upstream end (with the same queue check).
    fn flush_tail(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let tail = std::mem::take(&mut self.buf);
        if is_queue_line(&tail) {
            self.out.truncate(self.event_start);
            self.state.mark_rejected();
        } else if self.skip_blank && is_blank_line(&tail) {
            // Trailing separator of a swallowed queue event — drop it.
        } else {
            if is_real_data_line(&tail) {
                self.state.mark_real_data();
            }
            self.out.extend_from_slice(&tail);
        }
    }
}

impl<S> Stream for QueueFilterStream<S>
where
    S: Stream<Item = Result<Vec<u8>, std::io::Error>> + Unpin,
{
    type Item = Result<Vec<u8>, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if !this.out.is_empty() {
                return Poll::Ready(Some(Ok(std::mem::take(&mut this.out))));
            }
            if this.upstream_ended {
                return Poll::Ready(None);
            }
            match Pin::new(&mut this.upstream).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    this.state.touch();
                    this.buf.extend_from_slice(&chunk);
                    this.drain_lines();
                    // loop: flush `out` if any, else poll upstream again
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => {
                    this.upstream_ended = true;
                    this.flush_tail();
                    // loop: flush remaining tail bytes or return None
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    async fn collect_filtered(chunks: Vec<Vec<u8>>) -> (Vec<u8>, bool, bool) {
        let state = Arc::new(QueueState::new());
        let src = futures::stream::iter(
            chunks.into_iter().map(Ok::<Vec<u8>, std::io::Error>).collect::<Vec<_>>(),
        );
        let mut f = QueueFilterStream::new(src, state.clone());
        let mut out: Vec<u8> = Vec::new();
        while let Some(item) = f.next().await {
            out.extend_from_slice(&item.unwrap());
        }
        (out, state.rejected_seen(), state.real_data_seen())
    }

    #[tokio::test]
    async fn swallows_queue_events_and_passes_real_data() {
        let (out, rejected, real) = collect_filtered(vec![
            b"data: {\"type\":\"WaitingInLine\",\"position\":1}\n\n".to_vec(),
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n".to_vec(),
        ])
        .await;
        assert!(rejected);
        assert!(real);
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains("WaitingInLine"));
        assert!(s.contains("choices"));
    }

    #[tokio::test]
    async fn queue_only_stream_yields_nothing() {
        let (out, rejected, real) = collect_filtered(vec![
            b"data: {\"type\":\"WaitingInLine\",\"position\":1}\n\n".to_vec(),
            b"data: {\"type\":\"WaitingInLine\",\"position\":1}\n\n".to_vec(),
        ])
        .await;
        assert!(rejected);
        assert!(!real);
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn queue_event_split_across_chunks() {
        let (out, rejected, real) = collect_filtered(vec![
            b"data: {\"type\":\"WaitingIn".to_vec(),
            b"Line\",\"position\":2}\n\ndata: {\"choices\":[]}\n\n".to_vec(),
        ])
        .await;
        assert!(rejected);
        assert!(real);
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains("WaitingInLine"));
        assert!(s.contains("choices"));
    }

    #[tokio::test]
    async fn marker_embedded_in_real_content_is_not_swallowed() {
        let line = format!(
            "data: {}\n\n",
            serde_json::json!({"choices":[{"delta":{"content":"marker \"type\":\"WaitingInLine appears here"}}]})
        );
        let (out, rejected, real) = collect_filtered(vec![line.into_bytes()]).await;
        assert!(!rejected);
        assert!(real);
        assert!(String::from_utf8_lossy(&out).contains("WaitingInLine"));
    }

    #[tokio::test]
    async fn done_line_is_not_real_data() {
        let (_, _, real) = collect_filtered(vec![b"data: [DONE]\n\n".to_vec()]).await;
        assert!(!real);
    }

    #[tokio::test]
    async fn queue_line_without_trailing_newline_at_end() {
        let (out, rejected, real) =
            collect_filtered(vec![b"data: {\"type\":\"WaitingInLine\"}".to_vec()]).await;
        assert!(rejected);
        assert!(!real);
        assert!(out.is_empty());
    }

    #[test]
    fn is_queue_rejection_substring() {
        assert!(is_queue_rejection(b"data: {\"type\":\"WaitingInLine\"}"));
        assert!(!is_queue_rejection(b"data: {\"choices\":[]}"));
    }

    #[test]
    fn finds_in_band_sse_error() {
        let body = b"id: 0\ndata: \nevent: error\ndata: {\"error\":{\"message\":\"New session request rate exceeded. Please retry later or set up a custom model.\",\"type\":\"UserSessionLimitExceeded\",\"code\":\"403\"}}\n\n";
        let e = find_sse_error(body).expect("error should be found");
        assert_eq!(e.error_type, "UserSessionLimitExceeded");
        assert!(e.message.contains("rate exceeded"));
        assert_eq!(e.code.as_deref(), Some("403"));
    }

    #[test]
    fn no_error_in_normal_stream() {
        let body = b"data: {\"id\":\"x\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        assert!(find_sse_error(body).is_none());
        assert_eq!(extract_sse_reply(body), "hi");
    }

    #[test]
    fn extracts_multi_chunk_reply() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"！\"}}]}\n\ndata: [DONE]\n\n".as_bytes();
        assert_eq!(extract_sse_reply(body), "你好！");
    }

    #[tokio::test]
    async fn nested_error_queue_event_swallowed_whole() {
        // Format B (glm-5.2 / deepseek-v4-flash): WaitingInLine nested under
        // `error`, wrapped in `event: error`, connection closes right after.
        let (out, rejected, real) = collect_filtered(vec![b"id: 0\ndata: \nevent: error\ndata: {\"error\":{\"message\":\"The model is currently under high demand. You are now in position 4 of the queue.\",\"type\":\"WaitingInLine\",\"code\":\"406\"}}\n\n".to_vec()])
            .await;
        assert!(rejected);
        assert!(!real);
        assert!(
            out.is_empty(),
            "whole queue event should be swallowed, got: {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[tokio::test]
    async fn nested_queue_then_real_data() {
        let (out, rejected, real) = collect_filtered(vec![
            b"id: 0\ndata: \nevent: error\ndata: {\"error\":{\"message\":\"queued\",\"type\":\"WaitingInLine\",\"code\":\"406\"}}\n\n".to_vec(),
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n".to_vec(),
        ])
        .await;
        assert!(rejected);
        assert!(real);
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains("WaitingInLine"));
        assert!(!s.contains("event: error"));
        assert!(s.contains("choices"));
    }

    #[tokio::test]
    async fn non_queue_event_lines_pass_through() {
        let (out, _, real) =
            collect_filtered(vec![b"event: ping\ndata: {}\n\n".to_vec()]).await;
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("event: ping"));
        assert!(s.contains("data: {}"));
        assert!(real);
    }
}
