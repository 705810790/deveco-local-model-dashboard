//! Anthropic Messages API ↔ OpenAI Chat Completions 双向转换。
//!
//! 从 deveco-local-model-dashboard/src/transform/anthropic-transform.ts 移植。
//! 全部基于 `serde_json::Value` 操作，贴近原 TS 实现。
//!
//! SSE 事件格式：`event: xxx\ndata: {json}\n\n`

use serde_json::{json, Value};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// 请求方向：Anthropic → OpenAI Chat
// ---------------------------------------------------------------------------

/// 把一条 Anthropic message 的 content（字符串或 content blocks 数组）
/// 转成 OpenAI 的 content（字符串或 parts 数组）。
fn convert_anthropic_content_to_openai(content: &Value) -> Value {
    // 纯字符串直接透传。
    if content.is_string() {
        return content.clone();
    }
    let blocks = match content.as_array() {
        Some(a) => a,
        None => return content.clone(),
    };

    let mut parts: Vec<Value> = Vec::new();
    for block in blocks {
        let ty = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match ty {
            "text" => {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    parts.push(json!({"type": "text", "text": text}));
                }
            }
            "image" => {
                // Anthropic: {type:"image", source:{type:"base64", media_type:"...", data:"..."}}
                if let Some(src) = block.get("source") {
                    let media_type = src
                        .get("media_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("image/png");
                    let data = src.get("data").and_then(|v| v.as_str()).unwrap_or("");
                    let url = format!("data:{};base64,{}", media_type, data);
                    parts.push(json!({
                        "type": "image_url",
                        "image_url": {"url": url}
                    }));
                }
            }
            "tool_use" => {
                if let Some(id) = block.get("id").and_then(|v| v.as_str()) {
                    if let Some(name) = block.get("name").and_then(|v| v.as_str()) {
                        let input = block.get("input").cloned().unwrap_or(json!({}));
                        parts.push(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input
                        }));
                    }
                }
            }
            "tool_result" => {
                if let Some(id) = block.get("tool_use_id").and_then(|v| v.as_str()) {
                    let inner = block.get("content").cloned().unwrap_or(json!(""));
                    parts.push(json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": inner
                    }));
                }
            }
            _ => {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    parts.push(json!({"type": "text", "text": text}));
                }
            }
        }
    }
    Value::Array(parts)
}

/// 把 Anthropic messages 数组转成 OpenAI messages 数组。
///
/// Anthropic 一条 message 可能同时含 text + tool_use（assistant）或
/// text + tool_result（user）。OpenAI 要求 tool_use 拆成 assistant 的
/// `tool_calls`，tool_result 拆成独立的 `role:"tool"` 消息。
fn convert_anthropic_messages_to_openai(messages: &Value) -> Value {
    let arr = match messages.as_array() {
        Some(a) => a,
        None => return messages.clone(),
    };
    let mut out: Vec<Value> = Vec::new();
    for msg in arr {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        let content = msg.get("content").cloned().unwrap_or(json!(""));
        // 纯字符串 content：直接作为一条消息。
        if !content.is_array() {
            out.push(json!({"role": role, "content": content}));
            continue;
        }
        let parts = match content.as_array() {
            Some(a) => a.clone(),
            None => vec![],
        };

        // 把 tool_result blocks 拆成独立的 tool 消息。
        // TS convertUserMessage (anthropic-transform.ts:267-293):
        // - tool_result.content 为 string → 直接用
        // - tool_result.content 为 array → 逐块检查 text 和 image，image 转为 image_url 保留
        // - 单个 text part 时 content 退化为纯字符串
        let tool_results: Vec<&Value> = parts
            .iter()
            .filter(|p| p.get("type").and_then(|v| v.as_str()) == Some("tool_result"))
            .collect();
        for tr in &tool_results {
            let id = tr.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
            let inner = tr.get("content").cloned().unwrap_or(json!(""));
            let tool_content = match &inner {
                Value::String(s) => Value::String(s.clone()),
                Value::Array(a) => {
                    let mut tool_parts: Vec<Value> = Vec::new();
                    for b in a {
                        let bty = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        match bty {
                            "text" => {
                                if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                                    tool_parts.push(json!({"type": "text", "text": t}));
                                }
                            }
                            "image" => {
                                // 保留截图：image → image_url
                                if let Some(src) = b.get("source") {
                                    let media_type = src
                                        .get("media_type")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("image/png");
                                    let data = src.get("data").and_then(|v| v.as_str()).unwrap_or("");
                                    let url = format!("data:{};base64,{}", media_type, data);
                                    tool_parts.push(json!({
                                        "type": "image_url",
                                        "image_url": {"url": url}
                                    }));
                                }
                            }
                            _ => {}
                        }
                    }
                    // TS: toolContent = toolParts.length === 1 && toolParts[0].type === "text" ? toolParts[0].text : toolParts
                    if tool_parts.len() == 1
                        && tool_parts[0].get("type").and_then(|v| v.as_str()) == Some("text")
                    {
                        Value::String(
                            tool_parts[0].get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        )
                    } else {
                        Value::Array(tool_parts)
                    }
                }
                other => Value::String(other.to_string()),
            };
            out.push(json!({"role": "tool", "tool_call_id": id, "content": tool_content}));
        }

        // 剩余部分（text / image / tool_use）组成一条消息。
        let mut remaining: Vec<Value> = parts
            .iter()
            .filter(|p| p.get("type").and_then(|v| v.as_str()) != Some("tool_result"))
            .cloned()
            .collect();

        // assistant 的 tool_use blocks 提升为顶层 tool_calls。
        let tool_uses: Vec<&Value> = remaining
            .iter()
            .filter(|p| p.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
            .collect();
        let mut tool_calls: Vec<Value> = Vec::new();
        for tu in tool_uses {
            let id = tu.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let name = tu.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = tu.get("input").cloned().unwrap_or(json!({}));
            let args_str = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
            tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": args_str}
            }));
        }
        remaining.retain(|p| p.get("type").and_then(|v| v.as_str()) != Some("tool_use"));

        // 构造 OpenAI content：若 remaining 全是 text，合并为字符串更干净。
        let oai_content: Value = if remaining.is_empty() {
            Value::Null
        } else if remaining.iter().all(|p| {
            p.get("type").and_then(|v| v.as_str()) == Some("text")
        }) {
            let s: String = remaining
                .iter()
                .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join("");
            Value::String(s)
        } else {
            convert_anthropic_content_to_openai(&Value::Array(remaining))
        };

        let mut m = json!({"role": role});
        if !oai_content.is_null() {
            m["content"] = oai_content;
        }
        if !tool_calls.is_empty() {
            m["tool_calls"] = Value::Array(tool_calls);
        }
        if role == "user" && tool_results.len() == parts.len() {
            continue;
        }
        out.push(m);
    }
    Value::Array(out)
}

/// 把 Anthropic tools 数组转成 OpenAI tools 数组。
fn convert_anthropic_tools_to_openai(tools: &Value) -> Value {
    let arr = match tools.as_array() {
        Some(a) => a,
        None => return tools.clone(),
    };
    let out: Vec<Value> = arr
        .iter()
        .map(|t| {
            let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let desc = t.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let schema = t.get("input_schema").cloned().unwrap_or(json!({}));
            json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": desc,
                    "parameters": schema
                }
            })
        })
        .collect();
    Value::Array(out)
}

/// Extract forced tool name from tool_choice object.
/// TS forcedToolName: {type:"tool", name:"x"} -> "x", else null.
fn forced_tool_name(choice: Option<&Value>) -> Option<String> {
    let c = choice?;
    if c.get("type").and_then(|v| v.as_str()) == Some("tool") {
        c.get("name").and_then(|v| v.as_str()).map(|s| s.to_string())
    } else {
        None
    }
}

/// 把 Anthropic `tool_choice` 转成 OpenAI `tool_choice`。
/// TS convertToolChoice: "any"->"required", {type:"tool"}->"required" (tools narrowed separately).
fn convert_anthropic_tool_choice_to_openai(tc: &Value) -> Value {
    if tc.is_string() {
        return tc.clone();
    }
    let ty = tc.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match ty {
        "auto" => json!("auto"),
        "any" => json!("required"),
        "none" => json!("none"),
        "tool" => json!("required"),
        _ => json!("auto"),
    }
}

/// Anthropic Messages 请求 → OpenAI Chat Completions 请求。
pub fn anthropic_to_openai_chat(body: &Value) -> Value {
    let mut out = json!({});

    if let Some(m) = body.get("model") {
        out["model"] = m.clone();
    }

    let mut messages: Vec<Value> = Vec::new();
    if let Some(sys) = body.get("system") {
        let sys_text = match sys {
            Value::String(s) => s.clone(),
            Value::Array(a) => {
                // TS: .map(b => b.text).filter(Boolean).join("\n")
                let texts: Vec<&str> = a
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                    .filter(|s| !s.is_empty())
                    .collect();
                texts.join("\n")
            }
            _ => String::new(),
        };
        if !sys_text.is_empty() {
            messages.push(json!({"role": "system", "content": sys_text}));
        }
    }

    if let Some(msgs) = body.get("messages") {
        let converted = convert_anthropic_messages_to_openai(msgs);
        if let Some(a) = converted.as_array() {
            messages.extend(a.iter().cloned());
        }
    }
    out["messages"] = Value::Array(messages);

    // stream: default true (only explicit false opts out), TS behavior
    let is_stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(true);
    out["stream"] = json!(is_stream);
    if is_stream {
        out["stream_options"] = json!({"include_usage": true});
    }

    // Tools: filter BatchTool, apply forcedTool narrowing
    let forced_tool = forced_tool_name(body.get("tool_choice"));
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        let filtered: Vec<Value> = tools
            .iter()
            .filter(|t| {
                // Filter out BatchTool (TS behavior)
                t.get("name").and_then(|v| v.as_str()) != Some("BatchTool")
            })
            .filter(|t| {
                // If a tool is forced, keep only that one
                if let Some(ref forced) = forced_tool {
                    t.get("name").and_then(|v| v.as_str()) == Some(forced.as_str())
                } else {
                    true
                }
            })
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name").cloned().unwrap_or(json!(null)),
                        "description": t.get("description").cloned().unwrap_or(json!(null)),
                        "parameters": t.get("input_schema").cloned().unwrap_or(json!({}))
                    }
                })
            })
            .collect();
        if !filtered.is_empty() {
            out["tools"] = Value::Array(filtered);
        }
    }

    if let Some(tc) = body.get("tool_choice") {
        out["tool_choice"] = convert_anthropic_tool_choice_to_openai(tc);
    }

    // DevEco rejects tool_choice:"required" with no tools — degrade to no constraint
    if out.get("tool_choice").and_then(|v| v.as_str()) == Some("required") {
        let has_tools = out.get("tools").and_then(|t| t.as_array()).map(|a| !a.is_empty()).unwrap_or(false);
        if !has_tools {
            out.as_object_mut().unwrap().remove("tool_choice");
        }
    }

    if let Some(v) = body.get("max_tokens") {
        out["max_tokens"] = v.clone();
    }
    if let Some(v) = body.get("temperature") {
        out["temperature"] = v.clone();
    }
    if let Some(v) = body.get("top_p") {
        out["top_p"] = v.clone();
    }
    if let Some(v) = body.get("stop_sequences") {
        out["stop"] = v.clone();
    }

    // thinking → reasoning_effort mapping (TS behavior)
    if let Some(thinking) = body.get("thinking") {
        if let Some(budget) = thinking.get("budget_tokens").and_then(|v| v.as_u64()) {
            if budget <= 1024 {
                out["reasoning_effort"] = json!("low");
            } else if budget <= 8192 {
                out["reasoning_effort"] = json!("medium");
            } else {
                out["reasoning_effort"] = json!("high");
            }
        } else if thinking.get("type").and_then(|v| v.as_str()) == Some("enabled") {
            out["reasoning_effort"] = json!("high");
        }
        // Forward thinking verbatim so Anthropic thinking turns into visible thinking blocks
        if let Some(ty) = thinking.get("type").and_then(|v| v.as_str()) {
            if ty == "enabled" || ty == "disabled" {
                out["thinking"] = json!({"type": ty});
            }
        }
    }

    out
}

// ---------------------------------------------------------------------------
// 响应方向：OpenAI Chat → Anthropic
// ---------------------------------------------------------------------------

/// 把 OpenAI Chat Completions **非流式**响应转成 Anthropic Messages 响应。
pub fn openai_chat_to_anthropic(response: &Value, model: &str) -> Value {
    let id = response
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let choice = response
        .get("choices")
        .and_then(|c| c.get(0))
        .cloned()
        .unwrap_or(json!({}));
    let msg = choice.get("message").cloned().unwrap_or(json!({}));
    let finish = choice
        .get("finish_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("stop");
    let usage = response.get("usage").cloned().unwrap_or(json!({}));

    let mut content: Vec<Value> = Vec::new();
    if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
    }
    if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in tcs {
            let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let args_str = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
                .unwrap_or("{}");
            let input: Value =
                serde_json::from_str(args_str).unwrap_or_else(|_| json!({}));
            content.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input
            }));
        }
    }

    let stop_reason = match finish {
        "tool_calls" | "function_call" => "tool_use",
        "length" => "max_tokens",
        "stop" => "end_turn",
        other => other,
    };

    let anthropic_usage = json!({
        "input_tokens": usage.get("prompt_tokens").cloned().unwrap_or(json!(0)),
        "output_tokens": usage.get("completion_tokens").cloned().unwrap_or(json!(0))
    });

    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "usage": anthropic_usage
    })
}

// ---------------------------------------------------------------------------
// 流式：OpenAI Chat stream → Anthropic SSE
// ---------------------------------------------------------------------------

/// 单个 tool_use 块的累积状态。
#[derive(Debug, Clone, Default)]
pub struct ToolBlock {
    pub id: String,
    pub name: String,
    pub args_buffer: String,
    pub started: bool,
    pub anthropic_index: i64,
}

/// Anthropic 流式转换的内部状态。逐行对齐 TS 原版 `StreamState`。
#[derive(Debug, Clone)]
pub struct AnthropicStreamState {
    pub queue: Vec<(String, Value)>,
    pub current_block_type: String,
    pub current_block_index: i64,
    pub next_content_index: i64,
    pub block_start_sent: bool,
    pub block_stop_sent: bool,
    pub tool_blocks: BTreeMap<i64, ToolBlock>,
    pub model: String,
    pub message_start_sent: bool,
    pub message_delta_sent: bool,
    pub message_stop_sent: bool,
    pub held_message_delta: Option<(String, Value)>,
}

impl Default for AnthropicStreamState {
    fn default() -> Self {
        Self {
            queue: Vec::new(),
            current_block_type: "text".to_string(),
            current_block_index: -1,
            next_content_index: 0,
            block_start_sent: false,
            block_stop_sent: false,
            tool_blocks: BTreeMap::new(),
            model: String::new(),
            message_start_sent: false,
            message_delta_sent: false,
            message_stop_sent: false,
            held_message_delta: None,
        }
    }
}

/// 创建一个 Anthropic 流式状态。
pub fn create_anthropic_stream_state(model: &str) -> AnthropicStreamState {
    let mut s = AnthropicStreamState::default();
    s.model = model.to_string();
    s
}

fn uuid_v4_short() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}", nanos)
}

fn sse(event: &str, data: &Value) -> String {
    format!("event: {}\ndata: {}\n\n", event, data)
}

// --- 内部辅助函数，逐行对齐 TS 原版 ---

fn enqueue(state: &mut AnthropicStreamState, event: &str, data: Value) {
    state.queue.push((event.to_string(), data));
}

fn ensure_message_start(state: &mut AnthropicStreamState, chunk_id: Option<&str>, usage: Option<&Value>) {
    if state.message_start_sent {
        return;
    }
    state.message_start_sent = true;
    let prompt_tokens = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let cached_read = usage
        .and_then(|u| u.get("prompt_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let id = chunk_id
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("msg_{}", uuid_v4_short()));
    let input_tokens = if cached_read > 0 {
        (prompt_tokens - cached_read).max(0)
    } else {
        prompt_tokens
    };
    let mut usage_obj = json!({ "input_tokens": input_tokens, "output_tokens": 0 });
    if cached_read > 0 {
        usage_obj["cache_read_input_tokens"] = json!(cached_read);
    }
    enqueue(
        state,
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": state.model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": usage_obj,
            }
        }),
    );
}

fn open_block(state: &mut AnthropicStreamState, block_type: &str, block: Value) -> i64 {
    let index = state.next_content_index;
    state.next_content_index += 1;
    state.current_block_type = block_type.to_string();
    state.current_block_index = index;
    state.block_start_sent = true;
    state.block_stop_sent = false;
    enqueue(
        state,
        "content_block_start",
        json!({ "type": "content_block_start", "index": index, "content_block": block }),
    );
    index
}

fn emit_delta(state: &mut AnthropicStreamState, index: i64, delta: Value) {
    enqueue(
        state,
        "content_block_delta",
        json!({ "type": "content_block_delta", "index": index, "delta": delta }),
    );
}

fn close_current_block(state: &mut AnthropicStreamState) {
    if !state.block_start_sent || state.block_stop_sent {
        return;
    }
    state.block_stop_sent = true;
    enqueue(
        state,
        "content_block_stop",
        json!({ "type": "content_block_stop", "index": state.current_block_index }),
    );
}

fn close_all_tool_blocks(state: &mut AnthropicStreamState) {
    let indices: Vec<i64> = state
        .tool_blocks
        .iter()
        .filter(|(_, b)| b.started)
        .map(|(_, b)| b.anthropic_index)
        .collect();
    for idx in indices {
        enqueue(
            state,
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": idx }),
        );
    }
    state.tool_blocks.clear();
    if state.current_block_type == "tool_use" {
        state.block_stop_sent = true;
    }
}

fn close_all_open_blocks(state: &mut AnthropicStreamState) {
    if state.current_block_type != "tool_use" {
        close_current_block(state);
    }
    close_all_tool_blocks(state);
}

fn extract_reasoning(delta: &Value) -> Option<String> {
    if let Some(s) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    if let Some(s) = delta.get("reasoning").and_then(|v| v.as_str()) {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    None
}

fn handle_text(delta: &Value, state: &mut AnthropicStreamState) {
    let text = match delta.get("content").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };
    if state.current_block_type != "text" || !state.block_start_sent {
        open_block(state, "text", json!({ "type": "text", "text": "" }));
    }
    emit_delta(
        state,
        state.current_block_index,
        json!({ "type": "text_delta", "text": text }),
    );
}

fn handle_thinking(thinking: &str, state: &mut AnthropicStreamState) {
    if state.current_block_type != "thinking" || !state.block_start_sent {
        open_block(state, "thinking", json!({ "type": "thinking", "thinking": "" }));
    }
    emit_delta(
        state,
        state.current_block_index,
        json!({ "type": "thinking_delta", "thinking": thinking }),
    );
}

fn handle_tool_calls(tool_calls: &[Value], state: &mut AnthropicStreamState) {
    for tc in tool_calls {
        let tc_index = tc.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
        if !state.tool_blocks.contains_key(&tc_index) {
            state.tool_blocks.insert(
                tc_index,
                ToolBlock {
                    id: String::new(),
                    name: String::new(),
                    args_buffer: String::new(),
                    started: false,
                    anthropic_index: -1,
                },
            );
        }
        let block = state.tool_blocks.get_mut(&tc_index).unwrap();
        if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
            if !id.is_empty() {
                block.id = id.to_string();
            }
        }
        if let Some(name) = tc
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str())
        {
            if !name.is_empty() {
                block.name.push_str(name);
            }
        }
        let args_delta = tc
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !args_delta.is_empty() {
            block.args_buffer.push_str(args_delta);
        }
        // Capture what to emit while `block` borrows state.tool_blocks; drop the
        // borrow before calling enqueue/emit_delta (which need &mut state).
        // start_emit: (index, id, name, maybe buffered partial_json)
        // delta_emit: (index, partial_json)
        let start_emit: Option<(i64, String, String, Option<String>)>;
        let delta_emit: Option<(i64, String)>;
        if !block.started && !block.id.is_empty() && !block.name.is_empty() {
            block.started = true;
            block.anthropic_index = state.next_content_index;
            state.next_content_index += 1;
            state.current_block_type = "tool_use".to_string();
            state.current_block_index = block.anthropic_index;
            state.block_start_sent = true;
            state.block_stop_sent = false;
            let idx = block.anthropic_index;
            let bid = block.id.clone();
            let bname = block.name.clone();
            let partial = if !block.args_buffer.is_empty() {
                Some(block.args_buffer.clone())
            } else {
                None
            };
            start_emit = Some((idx, bid, bname, partial));
            delta_emit = None;
        } else if block.started && !args_delta.is_empty() {
            start_emit = None;
            delta_emit = Some((block.anthropic_index, args_delta.to_string()));
        } else {
            start_emit = None;
            delta_emit = None;
        }
        if let Some((idx, bid, bname, partial)) = start_emit {
            enqueue(
                state,
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": { "type": "tool_use", "id": bid, "name": bname, "input": {} }
                }),
            );
            if let Some(p) = partial {
                emit_delta(
                    state,
                    idx,
                    json!({ "type": "input_json_delta", "partial_json": p }),
                );
            }
        } else if let Some((idx, partial)) = delta_emit {
            emit_delta(
                state,
                idx,
                json!({ "type": "input_json_delta", "partial_json": partial }),
            );
        }
    }
}

fn map_finish_reason(finish_reason: &str) -> &str {
    match finish_reason {
        "tool_calls" | "function_call" => "tool_use",
        "length" => "max_tokens",
        "stop" => "end_turn",
        other => other,
    }
}

fn handle_finish_reason(finish_reason: &str, chunk: &Value, state: &mut AnthropicStreamState) {
    if state.message_delta_sent {
        return;
    }
    close_all_open_blocks(state);
    let stop_reason = map_finish_reason(finish_reason);
    let usage = match chunk.get("usage") {
        Some(u) => {
            let out = u.get("completion_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
            json!({ "output_tokens": out })
        }
        None => json!({ "output_tokens": 0 }),
    };
    let message_delta = (
        "message_delta".to_string(),
        json!({
            "type": "message_delta",
            "delta": { "stop_reason": stop_reason, "stop_sequence": null },
            "usage": usage
        }),
    );
    if chunk.get("usage").is_some() {
        state.message_delta_sent = true;
        state.queue.push(message_delta);
    } else {
        state.held_message_delta = Some(message_delta);
    }
}

fn merge_usage_into_held_delta(state: &mut AnthropicStreamState, usage: &Value) {
    let held = match &state.held_message_delta {
        Some(h) => h.clone(),
        None => return,
    };
    let (event, mut data) = held;
    let out = usage
        .get("completion_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if let Some(u) = data.get_mut("usage") {
        u["output_tokens"] = json!(out);
    }
    state.message_delta_sent = true;
    state.queue.push((event, data));
    state.held_message_delta = None;
}

fn process_chunk(chunk: &Value, state: &mut AnthropicStreamState) {
    let choice = match chunk.get("choices").and_then(|c| c.as_array()).and_then(|a| a.get(0)) {
        Some(c) => c,
        None => {
            if let Some(usage) = chunk.get("usage") {
                if state.held_message_delta.is_some() {
                    merge_usage_into_held_delta(state, usage);
                }
            }
            return;
        }
    };
    let chunk_id = chunk.get("id").and_then(|v| v.as_str());
    let usage = chunk.get("usage");
    ensure_message_start(state, chunk_id, usage);
    let delta = choice.get("delta").cloned().unwrap_or(json!({}));
    let tool_calls: Vec<Value> = delta
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let has_tool_calls = !tool_calls.is_empty();
    let has_text = delta
        .get("content")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let reasoning = extract_reasoning(&delta);

    if has_tool_calls {
        let first_has_name = tool_calls
            .get(0)
            .and_then(|tc| tc.get("function"))
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        let is_new = state.current_block_type != "tool_use" || first_has_name;
        if is_new && state.block_start_sent && !state.block_stop_sent {
            if state.current_block_type != "tool_use" {
                close_current_block(state);
            }
        }
        handle_tool_calls(&tool_calls, state);
    } else if has_text {
        let is_new = state.current_block_type != "text" || !state.block_start_sent;
        if is_new && state.block_start_sent && !state.block_stop_sent {
            if state.current_block_type == "tool_use" {
                close_all_tool_blocks(state);
            } else {
                close_current_block(state);
            }
        }
        handle_text(&delta, state);
    } else if let Some(ref thinking) = reasoning {
        let is_new = state.current_block_type != "thinking" || !state.block_start_sent;
        if is_new && state.block_start_sent && !state.block_stop_sent {
            if state.current_block_type == "tool_use" {
                close_all_tool_blocks(state);
            } else {
                close_current_block(state);
            }
        }
        handle_thinking(thinking, state);
    }

    if let Some(finish_reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
        if !finish_reason.is_empty() {
            handle_finish_reason(finish_reason, chunk, state);
        }
    }
}

fn finalize_stream(state: &mut AnthropicStreamState) {
    if state.message_stop_sent {
        return;
    }
    state.message_stop_sent = true;
    ensure_message_start(state, None, None);
    close_all_open_blocks(state);
    if state.held_message_delta.is_some() && !state.message_delta_sent {
        state.message_delta_sent = true;
        let held = state.held_message_delta.take().unwrap();
        state.queue.push(held);
    }
    if !state.message_delta_sent {
        state.message_delta_sent = true;
        enqueue(
            state,
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn", "stop_sequence": null },
                "usage": { "output_tokens": 0 }
            }),
        );
    }
    enqueue(state, "message_stop", json!({ "type": "message_stop" }));
}

/// 把一个 OpenAI Chat Completions **流式 chunk** 转成若干 Anthropic SSE 事件字符串。
///
/// 返回的字符串已包含 `event:` / `data:` 行和结尾空行，可直接写往 SSE body。
pub fn openai_chat_stream_to_anthropic(
    chunk: &Value,
    state: &mut AnthropicStreamState,
) -> Vec<String> {
    process_chunk(chunk, state);
    // 若上游已结束（[DONE] 由 pipe 处理），此处不负责 finalize。
    let out: Vec<String> = state
        .queue
        .drain(..)
        .map(|(event, data)| sse(&event, &data))
        .collect();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn converts_simple_request() {
        let body = json!({
            "model": "claude-3",
            "max_tokens": 1024,
            "system": "你是助手",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = anthropic_to_openai_chat(&body);
        assert_eq!(out["model"], "claude-3");
        assert_eq!(out["max_tokens"], 1024);
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][0]["content"], "你是助手");
        assert_eq!(out["messages"][1]["role"], "user");
        assert_eq!(out["messages"][1]["content"], "hi");
    }

    #[test]
    fn converts_tools() {
        let body = json!({
            "model": "claude-3",
            "max_tokens": 1024,
            "messages": [],
            "tools": [{
                "name": "get_weather",
                "description": "天气",
                "input_schema": {"type": "object", "properties": {}}
            }]
        });
        let out = anthropic_to_openai_chat(&body);
        assert_eq!(out["tools"][0]["type"], "function");
        assert_eq!(out["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(out["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn converts_tool_use_and_result_messages() {
        let body = json!({
            "model": "claude-3",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "天气?"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "查一下"},
                    {"type": "tool_use", "id": "t1", "name": "get_weather", "input": {"city": "BJ"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "晴"}
                ]}
            ]
        });
        let out = anthropic_to_openai_chat(&body);
        let msgs = out["messages"].as_array().unwrap();
        let assistant = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(assistant["content"], "查一下");
        assert_eq!(assistant["tool_calls"][0]["id"], "t1");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "get_weather");
        let expected_args = serde_json::to_string(&json!({"city": "BJ"})).unwrap();
        assert_eq!(assistant["tool_calls"][0]["function"]["arguments"], expected_args);
        let tool = msgs.iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(tool["tool_call_id"], "t1");
        assert_eq!(tool["content"], "晴");
    }

    #[test]
    fn converts_response_with_tool_calls() {
        let args = serde_json::to_string(&json!({"city": "BJ"})).unwrap();
        let resp = json!({
            "id": "c1",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "调用工具",
                    "tool_calls": [{
                        "id": "t1",
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": args}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3}
        });
        let out = openai_chat_to_anthropic(&resp, "claude-3");
        assert_eq!(out["stop_reason"], "tool_use");
        let content = out["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(content[1]["input"]["city"], "BJ");
        assert_eq!(out["usage"]["input_tokens"], 5);
        assert_eq!(out["usage"]["output_tokens"], 3);
    }

    #[test]
    fn stream_text_delta() {
        let mut state = create_anthropic_stream_state("claude-3");
        let chunk = json!({
            "choices": [{"delta": {"content": "Hello"}, "finish_reason": null}]
        });
        let evs = openai_chat_stream_to_anthropic(&chunk, &mut state);
        assert!(evs.iter().any(|e| e.contains("message_start")));
        assert!(evs.iter().any(|e| e.contains("content_block_start")));
        assert!(evs.iter().any(|e| e.contains("text_delta")));
    }

    #[test]
    fn stream_finish_emits_stop() {
        let mut state = create_anthropic_stream_state("claude-3");
        state.message_start_sent = true;
        state.current_block_type = "text".to_string();
        state.current_block_index = 0;
        state.block_start_sent = true;
        state.block_stop_sent = false;
        state.next_content_index = 1;
        let chunk = json!({
            "choices": [{"delta": {}, "finish_reason": "stop"}]
        });
        let mut evs = openai_chat_stream_to_anthropic(&chunk, &mut state);
        // finish_reason without usage holds the message_delta; finalizeStream flushes it.
        finalize_stream(&mut state);
        let flushed: Vec<String> = state
            .queue
            .drain(..)
            .map(|(event, data)| sse(&event, &data))
            .collect();
        evs.extend(flushed);
        assert!(evs.iter().any(|e| e.contains("content_block_stop")));
        assert!(evs.iter().any(|e| e.contains("message_delta")));
        assert!(evs.iter().any(|e| e.contains("message_stop")));
        assert!(evs.iter().any(|e| e.contains("end_turn")));
    }
}

// ---------------------------------------------------------------------------
// Stream pipe: upstream OpenAI SSE bytes → Anthropic SSE bytes
// ---------------------------------------------------------------------------

use futures::stream::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Wraps an upstream byte stream (OpenAI SSE) and transforms it to Anthropic SSE.
pub struct AnthropicStreamPipe<S>
where
    S: Stream<Item = Result<Vec<u8>, std::io::Error>> + Unpin,
{
    upstream: S,
    state: AnthropicStreamState,
    buffer: Vec<u8>,
    pending_output: Vec<u8>,
}

impl<S> AnthropicStreamPipe<S>
where
    S: Stream<Item = Result<Vec<u8>, std::io::Error>> + Unpin,
{
    pub fn new(upstream: S, model: String) -> Self {
        Self {
            upstream,
            state: create_anthropic_stream_state(&model),
            buffer: Vec::new(),
            pending_output: Vec::new(),
        }
    }
}

impl<S> Stream for AnthropicStreamPipe<S>
where
    S: Stream<Item = Result<Vec<u8>, std::io::Error>> + Unpin,
{
    type Item = Result<Vec<u8>, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // First, flush any pending output
        if !self.pending_output.is_empty() {
            let out = std::mem::take(&mut self.pending_output);
            return Poll::Ready(Some(Ok(out)));
        }

        // Poll upstream for more data
        match Pin::new(&mut self.upstream).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                self.buffer.extend_from_slice(&chunk);

                // Process complete SSE lines (terminated by \n\n or \n)
                while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = self.buffer.drain(..=pos).collect();
                    let line_str = String::from_utf8_lossy(&line);

                    // Parse "data: {...}" lines
                    let trimmed = line_str.trim();
                    if let Some(json_str) = trimmed.strip_prefix("data: ") {
                        if json_str == "[DONE]" {
                            continue;
                        }
                        if let Ok(chunk_val) = serde_json::from_str::<Value>(json_str) {
                            let events = openai_chat_stream_to_anthropic(&chunk_val, &mut self.state);
                            for evt in events {
                                self.pending_output.extend_from_slice(evt.as_bytes());
                            }
                        }
                    }
                }

                if self.pending_output.is_empty() {
                    // No complete lines yet, keep waiting
                    Poll::Pending
                } else {
                    let out = std::mem::take(&mut self.pending_output);
                    Poll::Ready(Some(Ok(out)))
                }
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => {
                // Upstream ended — flush any remaining buffer
                if !self.buffer.is_empty() {
                    let remaining = std::mem::take(&mut self.buffer);
                    Poll::Ready(Some(Ok(remaining)))
                } else {
                    Poll::Ready(None)
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
