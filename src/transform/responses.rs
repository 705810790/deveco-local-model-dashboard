//! OpenAI Responses API (Codex CLI) ↔ Chat Completions 双向转换。
//!
//! 从 deveco-local-model-dashboard/src/transform/responses-transform.ts 移植。
//! 全部基于 `serde_json::Value` 操作，贴近原 TS 实现。
//!
//! SSE 事件格式：`event: xxx\ndata: {json}\n\n`
//!
//! Responses API 的 `input` 是一个 item 数组，每个 item 有 `type`：
//! - `message` (role + content) → chat message
//! - `function_call` → assistant tool_calls
//! - `function_call_output` → role:"tool" message
//! - `reasoning` → 透传为 thinking 元数据

use serde_json::{json, Value};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// 请求方向：Responses → OpenAI Chat
// ---------------------------------------------------------------------------

/// 把 Responses API 的一个 content part 转成 OpenAI chat content part。
///
/// TS `convertResponsesMessage` 内联的 part 映射 (responses-transform.ts:257-264):
/// - text 类: `input_text` | `output_text` | `text` | `summary_text`（需 `typeof text === "string"`）
/// - image 类: `input_image`（`image_url` 需为 string）
fn convert_responses_content_part_to_chat(part: &Value) -> Value {
    let ty = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match ty {
        "input_text" | "output_text" | "text" | "summary_text" => {
            // TS: typeof part.text === "string" 时才 push
            match part.get("text").and_then(|v| v.as_str()) {
                Some(text) => json!({"type": "text", "text": text}),
                None => json!({"type": "text", "text": ""}),
            }
        }
        "input_image" => {
            // TS: typeof part.image_url === "string" 时才 push
            let url = part.get("image_url").and_then(|v| v.as_str()).unwrap_or("");
            json!({"type": "image_url", "image_url": {"url": url}})
        }
        _ => part.clone(),
    }
}

/// 把 Responses API 的 `input`（item 数组或 string）转成 OpenAI chat `messages`。
///
/// TS `convertInputItems` (responses-transform.ts:188-236):
/// - `function_call` 用 pending buffer，连续多个合并为一条 assistant + 多 tool_calls
/// - 遇到 `message` / `function_call_output` 时 flushPending
/// - `function_call` 仅当 `typeof item.name === "string" && item.name` 非空时入 buffer
/// - `function_call_output`: content = string ? output : output === undefined ? "" : JSON.stringify(output)
/// - `reasoning` 及其他类型：跳过
/// - 末尾 flushPending()
fn convert_responses_input_to_messages(input: &Value) -> Value {
    // TS: typeof body.input === "string" → [{role:"user", content: input}]
    if let Some(s) = input.as_str() {
        return json!([{"role": "user", "content": s}]);
    }
    let arr = match input.as_array() {
        Some(a) => a,
        None => return json!([]),
    };

    let mut out: Vec<Value> = Vec::new();

    // pending buffer for consecutive function_call items
    struct PendingCall {
        call_id: String,
        name: String,
        args: String,
    }
    let mut pending: Vec<PendingCall> = Vec::new();

    let flush_pending = |pending: &mut Vec<PendingCall>, out: &mut Vec<Value>| {
        if pending.is_empty() {
            return;
        }
        let tool_calls: Vec<Value> = pending
            .drain(..)
            .map(|c| {
                json!({
                    "id": c.call_id,
                    "type": "function",
                    "function": {"name": c.name, "arguments": c.args}
                })
            })
            .collect();
        out.push(json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": tool_calls
        }));
    };

    for item in arr {
        // TS: if (raw === null || typeof raw !== "object") continue
        if !item.is_object() {
            continue;
        }
        let ty = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match ty {
            "message" => {
                flush_pending(&mut pending, &mut out);
                // TS convertResponsesMessage: role = assistant→assistant, user→user, else→system
                let raw_role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
                let role = match raw_role {
                    "assistant" => "assistant",
                    "user" => "user",
                    _ => "system",
                };
                let content = item.get("content").cloned().unwrap_or(json!(""));
                // TS: typeof content === "string" → push as-is
                if let Some(s) = content.as_str() {
                    out.push(json!({"role": role, "content": s}));
                    continue;
                }
                // TS: !Array.isArray(content) → push {role, content: ""}
                let content_arr = match content.as_array() {
                    Some(a) => a,
                    None => {
                        out.push(json!({"role": role, "content": ""}));
                        continue;
                    }
                };
                // TS: build parts, then content = parts.length === 1 && parts[0].type === "text" ? parts[0].text : parts
                let parts: Vec<Value> = content_arr
                    .iter()
                    .map(convert_responses_content_part_to_chat)
                    .collect();
                if parts.len() == 1
                    && parts[0].get("type").and_then(|v| v.as_str()) == Some("text")
                {
                    let text = parts[0].get("text").and_then(|v| v.as_str()).unwrap_or("");
                    out.push(json!({"role": role, "content": text}));
                } else {
                    out.push(json!({"role": role, "content": Value::Array(parts)}));
                }
            }
            "function_call" => {
                // TS: if (typeof item.name === "string" && item.name) push to pending
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if !name.is_empty() {
                    let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                    let args = item.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
                    pending.push(PendingCall {
                        call_id: call_id.to_string(),
                        name: name.to_string(),
                        args: args.to_string(),
                    });
                }
            }
            "function_call_output" => {
                flush_pending(&mut pending, &mut out);
                let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                let output = item.get("output");
                // TS: typeof output === "string" ? output : output === undefined ? "" : JSON.stringify(output)
                let text = match output {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => String::new(),
                };
                out.push(json!({"role": "tool", "tool_call_id": call_id, "content": text}));
            }
            _ => {
                // TS: `reasoning` items (and anything else) are skipped.
            }
        }
    }
    flush_pending(&mut pending, &mut out);
    Value::Array(out)
}

/// TS `isResponsesFunctionTool` (responses-transform.ts:295-299):
/// `typeof t.name === "string"`（不是 `t.type === "function"`）
fn is_responses_function_tool(t: &Value) -> bool {
    t.get("name").and_then(|v| v.as_str()).is_some()
}

/// TS `isParametersObject` (responses-transform.ts:301-303):
/// `typeof p === "object" && p !== null && !Array.isArray(p)`
fn is_parameters_object(p: &Value) -> bool {
    p.is_object()
}

/// TS `forcedResponsesToolName` (responses-transform.ts:274-278):
/// `{type:"function", name:"x"}` → `"x"`, else null
fn forced_responses_tool_name(choice: Option<&Value>) -> Option<String> {
    let c = choice?;
    if c.get("type").and_then(|v| v.as_str()) == Some("function") {
        c.get("name").and_then(|v| v.as_str()).map(|s| s.to_string())
    } else {
        None
    }
}

/// TS `responsesToolChoiceToString` (responses-transform.ts:280-293):
/// - string "auto"/"none"/"required" → 透传
/// - object {type:"auto"}→"auto", {type:"none"}→"none", {type:"any"|"required"}→"required"
/// - else → None
fn responses_tool_choice_to_string(choice: &Value) -> Option<String> {
    if let Some(s) = choice.as_str() {
        return match s {
            "auto" | "none" | "required" => Some(s.to_string()),
            _ => None,
        };
    }
    if choice.is_object() {
        let ty = choice.get("type").and_then(|v| v.as_str()).unwrap_or("");
        return match ty {
            "auto" => Some("auto".to_string()),
            "none" => Some("none".to_string()),
            "any" | "required" => Some("required".to_string()),
            _ => None,
        };
    }
    None
}

/// 把 Responses API 的 `tools` 转成 OpenAI chat `tools`。
///
/// TS (responses-transform.ts:144-158):
/// - filter `isResponsesFunctionTool`（`typeof t.name === "string"`）
/// - forcedTool narrowing: 仅保留 name === forcedTool 的
/// - description 仅当 `typeof t.description === "string"` 时加入
/// - parameters: `isParametersObject(t.parameters) ? t.parameters : {type:"object",properties:{}}`
fn convert_responses_tools_to_chat(tools: &Value, forced_tool: &Option<String>) -> Value {
    let arr = match tools.as_array() {
        Some(a) => a,
        None => return tools.clone(),
    };
    let fns: Vec<&Value> = arr.iter().filter(|t| is_responses_function_tool(t)).collect();
    let narrowed: Vec<&Value> = match forced_tool {
        Some(forced) => fns
            .iter()
            .copied()
            .filter(|t| t.get("name").and_then(|v| v.as_str()) == Some(forced.as_str()))
            .collect(),
        None => fns,
    };
    if narrowed.is_empty() {
        return Value::Array(vec![]);
    }
    let out: Vec<Value> = narrowed
        .iter()
        .map(|t| {
            let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let mut func = json!({
                "type": "function",
                "function": {
                    "name": name,
                    "parameters": match t.get("parameters") {
                        Some(p) if is_parameters_object(p) => p.clone(),
                        _ => json!({"type": "object", "properties": {}}),
                    }
                }
            });
            // TS: ...(typeof t.description === "string" ? { description: t.description } : {})
            if let Some(desc) = t.get("description").and_then(|v| v.as_str()) {
                func["function"]["description"] = json!(desc);
            }
            func
        })
        .collect();
    Value::Array(out)
}

/// TS `effortToChatFields` (responses-transform.ts:89-107):
/// ```text
/// max    → { reasoning_effort: "high", thinking: { type: "enabled" } }
/// xhigh  → { reasoning_effort: "high" }
/// high   → { reasoning_effort: "high" }
/// medium → { reasoning_effort: "medium" }
/// low    → { reasoning_effort: "low" }
/// minimal→ { reasoning_effort: "low" }
/// default→ {}
/// ```
/// 对 effort 做 `.toLowerCase()`。
fn effort_to_chat_fields(effort: Option<&str>) -> (Option<String>, Option<Value>) {
    let effort = match effort {
        Some(e) => e.to_lowercase(),
        None => return (None, None),
    };
    match effort.as_str() {
        "max" => (Some("high".to_string()), Some(json!({"type": "enabled"}))),
        "xhigh" | "high" => (Some("high".to_string()), None),
        "medium" => (Some("medium".to_string()), None),
        "low" | "minimal" => (Some("low".to_string()), None),
        _ => (None, None),
    }
}

/// Responses API 请求 → OpenAI Chat Completions 请求。
///
/// TS `responsesToOpenaiChat` (responses-transform.ts:111-186):
/// 1. instructions → system message
/// 2. input string → user message; input array → convertInputItems
/// 3. messages 为空时 fallback `{role:"user",content:""}`
/// 4. stream 默认 true（`body.stream !== false`），设 stream_options
/// 5. model 取 string 否则 ""
/// 6. tools: forcedTool narrowing + isResponsesFunctionTool + isParametersObject
/// 7. tool_choice: responsesToolChoiceToString + forcedTool→"required" + required&&no tools→delete
/// 8. reasoning: body.reasoning.effort 或 body.reasoning_effort → effortToChatFields
/// 9. max_output_tokens → max_tokens
pub fn responses_to_openai_chat(body: &Value) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    // 1. instructions → system message
    if let Some(inst) = body.get("instructions").and_then(|v| v.as_str()) {
        if !inst.is_empty() {
            messages.push(json!({"role": "system", "content": inst}));
        }
    }

    // 2. input → messages
    if let Some(input) = body.get("input") {
        if let Some(s) = input.as_str() {
            messages.push(json!({"role": "user", "content": s}));
        } else if input.is_array() {
            let converted = convert_responses_input_to_messages(input);
            if let Some(a) = converted.as_array() {
                messages.extend(a.iter().cloned());
            }
        }
    }

    // 3. messages 为空时 fallback
    if messages.is_empty() {
        messages.push(json!({"role": "user", "content": ""}));
    }

    // 4. stream: default true (only explicit false opts out)
    let is_stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(true);

    // 5. model: string or ""
    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");

    let mut result = json!({
        "model": model,
        "messages": Value::Array(messages),
        "stream": is_stream,
    });
    if is_stream {
        result["stream_options"] = json!({"include_usage": true});
    }

    // 6. Tools
    let forced_tool = forced_responses_tool_name(body.get("tool_choice"));
    if let Some(tools_val) = body.get("tools") {
        if tools_val.is_array() {
            let converted = convert_responses_tools_to_chat(tools_val, &forced_tool);
            if let Some(arr) = converted.as_array() {
                if !arr.is_empty() {
                    result["tools"] = converted;
                }
            }
        }
    }

    // 7. tool_choice
    let choice = body
        .get("tool_choice")
        .and_then(responses_tool_choice_to_string);
    match choice {
        Some(c) => result["tool_choice"] = json!(c),
        None => {
            if forced_tool.is_some() {
                result["tool_choice"] = json!("required");
            }
        }
    }
    // DevEco rejects tool_choice:"required" with nothing to call — degrade
    if result.get("tool_choice").and_then(|v| v.as_str()) == Some("required") {
        let has_tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        if !has_tools {
            result.as_object_mut().unwrap().remove("tool_choice");
        }
    }

    // 8. reasoning → effortToChatFields
    let effort = if body.get("reasoning").is_some_and(|v| !v.is_null() && v.is_object()) {
        body.get("reasoning")
            .and_then(|r| r.get("effort"))
            .and_then(|v| v.as_str())
    } else {
        None
    };
    let effort = effort.or_else(|| body.get("reasoning_effort").and_then(|v| v.as_str()));
    let (reasoning_effort, thinking) = effort_to_chat_fields(effort);
    if let Some(re) = reasoning_effort {
        result["reasoning_effort"] = json!(re);
    }
    if let Some(th) = thinking {
        result["thinking"] = th;
    }

    // 9. max_output_tokens → max_tokens
    if let Some(v) = body.get("max_output_tokens") {
        if v.is_number() {
            result["max_tokens"] = v.clone();
        }
    }
    if let Some(v) = body.get("temperature") {
        if v.is_number() {
            result["temperature"] = v.clone();
        }
    }
    if let Some(v) = body.get("top_p") {
        if v.is_number() {
            result["top_p"] = v.clone();
        }
    }

    result
}

// ---------------------------------------------------------------------------
// 响应方向：OpenAI Chat → Responses
// ---------------------------------------------------------------------------

/// 把 OpenAI Chat Completions **非流式**响应转成 Responses API 响应。
pub fn openai_chat_to_responses(response: &Value, model: &str) -> Value {
    let id = response.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let choice = response.get("choices").and_then(|c| c.get(0)).cloned().unwrap_or(json!({}));
    let msg = choice.get("message").cloned().unwrap_or(json!({}));
    let finish = choice.get("finish_reason").and_then(|v| v.as_str()).unwrap_or("stop");
    let usage = response.get("usage").cloned().unwrap_or(json!({}));

    let mut output: Vec<Value> = Vec::new();

    // 文本 → output_text item。
    if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
        if !text.is_empty() {
            output.push(json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}]
            }));
        }
    }

    // tool_calls → function_call items。
    if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in tcs {
            let call_id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let name = tc.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("");
            let args = tc.get("function").and_then(|f| f.get("arguments")).and_then(|v| v.as_str()).unwrap_or("{}").to_string();
            output.push(json!({
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": args
            }));
        }
    }

    let status = match finish {
        "tool_calls" | "function_call" => "tool_calls",
        "length" => "incomplete",
        "stop" => "completed",
        other => other,
    };

    json!({
        "id": id,
        "object": "response",
        "model": model,
        "output": output,
        "status": status,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").cloned().unwrap_or(json!(0)),
            "output_tokens": usage.get("completion_tokens").cloned().unwrap_or(json!(0))
        }
    })
}

// ---------------------------------------------------------------------------
// 流式：OpenAI Chat stream → Responses SSE
// ---------------------------------------------------------------------------

/// Responses 流式转换的内部状态。
#[derive(Debug, Clone)]
pub struct ResponsesStreamState {
    pub response_id: String,
    pub model: String,
    pub created_at: i64,
    pub queue: Vec<(String, Value)>,
    pub next_output_index: i64,
    pub completed_items: Vec<(i64, Value)>,
    /// "reasoning" | "message" | None
    pub open_type: Option<String>,
    pub open_item_id: String,
    pub open_index: i64,
    pub reasoning_text: String,
    pub message_text: String,
    pub tool_items: BTreeMap<i64, ToolItemState>,
    pub created_sent: bool,
    pub completed_sent: bool,
    pub usage: Option<Value>,
    pub finish_reason: Option<String>,
}

/// 单个 function_call item 的累积状态。
#[derive(Debug, Clone)]
pub struct ToolItemState {
    pub item_id: String,
    pub call_id: String,
    pub name: String,
    pub args: String,
    pub output_index: i64,
    pub started: bool,
}

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn uuid_v4_short() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}", nanos)
}

fn next_item_id(prefix: &str) -> String {
    format!("{}_{}", prefix, uuid_v4_short())
}

/// 创建一个 Responses 流式状态。
pub fn create_responses_stream_state(model: &str) -> ResponsesStreamState {
    ResponsesStreamState {
        response_id: next_item_id("resp"),
        model: model.to_string(),
        created_at: now_secs(),
        queue: Vec::new(),
        next_output_index: 0,
        completed_items: Vec::new(),
        open_type: None,
        open_item_id: String::new(),
        open_index: -1,
        reasoning_text: String::new(),
        message_text: String::new(),
        tool_items: BTreeMap::new(),
        created_sent: false,
        completed_sent: false,
        usage: None,
        finish_reason: None,
    }
}

fn sse(event: &str, data: &Value) -> String {
    format!("event: {}\ndata: {}\n\n", event, data)
}

fn emit(state: &mut ResponsesStreamState, event: &str, data: Value) {
    state.queue.push((event.to_string(), data));
}

fn ensure_created(state: &mut ResponsesStreamState) {
    if state.created_sent {
        return;
    }
    state.created_sent = true;
    let base = json!({
        "id": state.response_id,
        "object": "response",
        "created_at": state.created_at,
        "status": "in_progress",
        "model": state.model,
        "output": []
    });
    emit(
        state,
        "response.created",
        json!({ "type": "response.created", "response": base.clone() }),
    );
    emit(
        state,
        "response.in_progress",
        json!({ "type": "response.in_progress", "response": base }),
    );
}

fn extract_reasoning_text(delta: &Value) -> Option<String> {
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

fn chat_usage_to_responses(usage: &Value) -> Value {
    let input_tokens = usage.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let output_tokens = usage
        .get("completion_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let mut out = json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
    });
    if let Some(total) = usage.get("total_tokens").and_then(|v| v.as_i64()) {
        out["total_tokens"] = json!(total);
    }
    out
}

fn zero_responses_usage() -> Value {
    json!({ "input_tokens": 0, "output_tokens": 0 })
}

fn open_reasoning_item(state: &mut ResponsesStreamState) {
    close_open_item(state);
    state.open_type = Some("reasoning".to_string());
    state.open_item_id = next_item_id("rs");
    state.open_index = state.next_output_index;
    state.next_output_index += 1;
    let item_id = state.open_item_id.clone();
    let output_index = state.open_index;
    emit(
        state,
        "response.output_item.added",
        json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": {
                "id": item_id,
                "type": "reasoning",
                "summary": []
            }
        }),
    );
    emit(
        state,
        "response.reasoning_summary_part.added",
        json!({
            "type": "response.reasoning_summary_part.added",
            "item_id": item_id,
            "output_index": output_index,
            "summary_index": 0,
            "part": { "type": "summary_text", "text": "" }
        }),
    );
}

fn append_reasoning_text(state: &mut ResponsesStreamState, text: &str) {
    ensure_created(state);
    if state.open_type.as_deref() != Some("reasoning") {
        open_reasoning_item(state);
    }
    state.reasoning_text.push_str(text);
    let item_id = state.open_item_id.clone();
    let output_index = state.open_index;
    emit(
        state,
        "response.reasoning_summary_text.delta",
        json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": item_id,
            "output_index": output_index,
            "summary_index": 0,
            "delta": text
        }),
    );
}

fn open_message_item(state: &mut ResponsesStreamState) {
    close_open_item(state);
    state.open_type = Some("message".to_string());
    state.open_item_id = next_item_id("msg");
    state.open_index = state.next_output_index;
    state.next_output_index += 1;
    let item_id = state.open_item_id.clone();
    let output_index = state.open_index;
    emit(
        state,
        "response.output_item.added",
        json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": {
                "id": item_id,
                "type": "message",
                "role": "assistant",
                "status": "in_progress",
                "content": []
            }
        }),
    );
    emit(
        state,
        "response.content_part.added",
        json!({
            "type": "response.content_part.added",
            "item_id": item_id,
            "output_index": output_index,
            "content_index": 0,
            "part": { "type": "output_text", "text": "", "annotations": [] }
        }),
    );
}

fn append_message_text(state: &mut ResponsesStreamState, text: &str) {
    ensure_created(state);
    if state.open_type.as_deref() != Some("message") {
        open_message_item(state);
    }
    state.message_text.push_str(text);
    let item_id = state.open_item_id.clone();
    let output_index = state.open_index;
    emit(
        state,
        "response.output_text.delta",
        json!({
            "type": "response.output_text.delta",
            "item_id": item_id,
            "output_index": output_index,
            "content_index": 0,
            "delta": text
        }),
    );
}

fn handle_tool_call_delta(tc: &Value, state: &mut ResponsesStreamState) {
    let index = tc.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
    // 取出当前 entry（或新建）。
    let mut entry = state
        .tool_items
        .get(&index)
        .cloned()
        .unwrap_or(ToolItemState {
            item_id: next_item_id("fc"),
            call_id: String::new(),
            name: String::new(),
            args: String::new(),
            output_index: -1,
            started: false,
        });

    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
        if !id.is_empty() {
            entry.call_id = id.to_string();
        }
    }
    if let Some(name) = tc
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(|v| v.as_str())
    {
        if !name.is_empty() {
            entry.name.push_str(name);
        }
    }
    let args_delta = tc
        .get("function")
        .and_then(|f| f.get("arguments"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if !entry.started && !entry.call_id.is_empty() && !entry.name.is_empty() {
        if state.open_type.is_some() {
            close_open_item(state);
        }
        entry.started = true;
        entry.output_index = state.next_output_index;
        state.next_output_index += 1;
        let item_id = entry.item_id.clone();
        let call_id = entry.call_id.clone();
        let name = entry.name.clone();
        let output_index = entry.output_index;
        let args_so_far = entry.args.clone();
        // 先把 entry 放回 map（后续可能继续更新）。
        state.tool_items.insert(index, entry.clone());
        emit(
            state,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": {
                    "id": item_id,
                    "type": "function_call",
                    "status": "in_progress",
                    "call_id": call_id,
                    "name": name,
                    "arguments": ""
                }
            }),
        );
        if !args_so_far.is_empty() {
            emit(
                state,
                "response.function_call_arguments.delta",
                json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": item_id,
                    "output_index": output_index,
                    "delta": args_so_far
                }),
            );
        }
        return;
    }

    if entry.started && !args_delta.is_empty() {
        entry.args.push_str(args_delta);
        let item_id = entry.item_id.clone();
        let output_index = entry.output_index;
        state.tool_items.insert(index, entry);
        emit(
            state,
            "response.function_call_arguments.delta",
            json!({
                "type": "response.function_call_arguments.delta",
                "item_id": item_id,
                "output_index": output_index,
                "delta": args_delta
            }),
        );
        return;
    }

    // 没有触发 emit 的分支也要把 entry 写回（保留累积的 call_id/name/args）。
    state.tool_items.insert(index, entry);
}

fn close_open_item(state: &mut ResponsesStreamState) {
    if state.open_type.as_deref() == Some("reasoning") {
        let text = state.reasoning_text.clone();
        let item_id = state.open_item_id.clone();
        let output_index = state.open_index;
        emit(
            state,
            "response.reasoning_summary_text.done",
            json!({
                "type": "response.reasoning_summary_text.done",
                "item_id": item_id,
                "output_index": output_index,
                "summary_index": 0,
                "text": text
            }),
        );
        emit(
            state,
            "response.reasoning_summary_part.done",
            json!({
                "type": "response.reasoning_summary_part.done",
                "item_id": item_id,
                "output_index": output_index,
                "summary_index": 0,
                "part": { "type": "summary_text", "text": text }
            }),
        );
        let item = json!({
            "id": item_id,
            "type": "reasoning",
            "summary": [{ "type": "summary_text", "text": text }]
        });
        emit(
            state,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": item.clone()
            }),
        );
        state.completed_items.push((output_index, item));
    } else if state.open_type.as_deref() == Some("message") {
        let text = state.message_text.clone();
        let item_id = state.open_item_id.clone();
        let output_index = state.open_index;
        emit(
            state,
            "response.output_text.done",
            json!({
                "type": "response.output_text.done",
                "item_id": item_id,
                "output_index": output_index,
                "content_index": 0,
                "text": text
            }),
        );
        emit(
            state,
            "response.content_part.done",
            json!({
                "type": "response.content_part.done",
                "item_id": item_id,
                "output_index": output_index,
                "content_index": 0,
                "part": { "type": "output_text", "text": text, "annotations": [] }
            }),
        );
        let item = json!({
            "id": item_id,
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }]
        });
        emit(
            state,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": item.clone()
            }),
        );
        state.completed_items.push((output_index, item));
    }
    state.open_type = None;
    state.open_item_id = String::new();
    state.open_index = -1;
    state.reasoning_text = String::new();
    state.message_text = String::new();
}

fn finalize_responses_stream(state: &mut ResponsesStreamState) {
    if state.completed_sent {
        return;
    }
    state.completed_sent = true;
    ensure_created(state);
    close_open_item(state);

    // 关闭所有 tool items。
    let tool_indices: Vec<i64> = state.tool_items.keys().cloned().collect();
    for idx in tool_indices {
        let entry = match state.tool_items.get(&idx) {
            Some(e) => e.clone(),
            None => continue,
        };
        if !entry.started {
            continue;
        }
        let item_id = entry.item_id.clone();
        let call_id = entry.call_id.clone();
        let name = entry.name.clone();
        let args = entry.args.clone();
        let output_index = entry.output_index;
        emit(
            state,
            "response.function_call_arguments.done",
            json!({
                "type": "response.function_call_arguments.done",
                "item_id": item_id,
                "output_index": output_index,
                "arguments": args
            }),
        );
        let item = json!({
            "id": item_id,
            "type": "function_call",
            "status": "completed",
            "call_id": call_id,
            "name": name,
            "arguments": args
        });
        emit(
            state,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": item.clone()
            }),
        );
        state.completed_items.push((output_index, item));
    }
    state.tool_items.clear();

    // 按 output_index 排序。
    state.completed_items.sort_by_key(|(idx, _)| *idx);

    let incomplete = state.finish_reason.as_deref() == Some("length");
    let status = if incomplete { "incomplete" } else { "completed" };
    let incomplete_details = if incomplete {
        json!({ "reason": "max_output_tokens" })
    } else {
        Value::Null
    };
    let usage = state.usage.clone().unwrap_or_else(zero_responses_usage);
    let output: Vec<Value> = state
        .completed_items
        .iter()
        .map(|(_, item)| item.clone())
        .collect();
    emit(
        state,
        "response.completed",
        json!({
            "type": "response.completed",
            "response": {
                "id": state.response_id,
                "object": "response",
                "created_at": state.created_at,
                "status": status,
                "incomplete_details": incomplete_details,
                "error": Value::Null,
                "model": state.model,
                "output": output,
                "usage": usage
            }
        }),
    );
}

fn process_responses_chunk(chunk: &Value, state: &mut ResponsesStreamState) {
    if let Some(usage) = chunk.get("usage") {
        if !usage.is_null() {
            state.usage = Some(chat_usage_to_responses(usage));
        }
    }
    let choice = match chunk.get("choices").and_then(|c| c.get(0)) {
        Some(c) => c,
        None => return,
    };
    ensure_created(state);
    let delta = choice.get("delta").cloned().unwrap_or(json!({}));
    let tool_calls = delta.get("tool_calls").and_then(|v| v.as_array());
    if let Some(tcs) = tool_calls {
        if !tcs.is_empty() {
            for tc in tcs {
                handle_tool_call_delta(tc, state);
            }
        }
    } else if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
        if !content.is_empty() {
            append_message_text(state, content);
        }
    } else if let Some(reasoning) = extract_reasoning_text(&delta) {
        append_reasoning_text(state, &reasoning);
    }
    if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
        if !fr.is_empty() {
            state.finish_reason = Some(fr.to_string());
        }
    }
}

/// 把一个 OpenAI Chat Completions **流式 chunk** 转成若干 Responses SSE 事件字符串。
pub fn openai_chat_stream_to_responses_stream(
    chunk: &Value,
    state: &mut ResponsesStreamState,
) -> Vec<String> {
    process_responses_chunk(chunk, state);
    if state.finish_reason.is_some() {
        finalize_responses_stream(state);
    }
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
    fn converts_simple_input() {
        let body = json!({
            "model": "gpt-5",
            "input": [{"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "hi"}
            ]}]
        });
        let out = responses_to_openai_chat(&body);
        assert_eq!(out["model"], "gpt-5");
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"], "hi");
    }

    #[test]
    fn converts_string_input() {
        let body = json!({"model": "gpt-5", "input": "hello"});
        let out = responses_to_openai_chat(&body);
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"], "hello");
    }

    #[test]
    fn converts_function_call_items() {
        let body = json!({
            "model": "gpt-5",
            "input": [
                {"type": "message", "role": "user", "content": "查天气"},
                {"type": "function_call", "call_id": "c1", "name": "get_weather", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "晴"}
            ]
        });
        let out = responses_to_openai_chat(&body);
        let msgs = out["messages"].as_array().unwrap();
        let assistant = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(assistant["tool_calls"][0]["id"], "c1");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "get_weather");
        let tool = msgs.iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(tool["tool_call_id"], "c1");
        assert_eq!(tool["content"], "晴");
    }

    #[test]
    fn converts_reasoning_effort() {
        // TS: reasoning.effort "high" → effortToChatFields → { reasoning_effort: "high" }
        let body = json!({
            "model": "gpt-5",
            "input": "hi",
            "reasoning": {"effort": "high"}
        });
        let out = responses_to_openai_chat(&body);
        assert_eq!(out["reasoning_effort"], "high");
    }

    #[test]
    fn converts_reasoning_effort_max() {
        // TS: "max" → { reasoning_effort: "high", thinking: { type: "enabled" } }
        let body = json!({
            "model": "gpt-5",
            "input": "hi",
            "reasoning": {"effort": "max"}
        });
        let out = responses_to_openai_chat(&body);
        assert_eq!(out["reasoning_effort"], "high");
        assert_eq!(out["thinking"]["type"], "enabled");
    }

    #[test]
    fn converts_instructions_and_empty_fallback() {
        // TS: instructions → system message; no input → messages.length === 1 (not 0),
        // so the empty-fallback ({role:"user",content:""}) does NOT trigger.
        let body = json!({"model": "gpt-5", "instructions": "You are helpful"});
        let out = responses_to_openai_chat(&body);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "You are helpful");
    }

    #[test]
    fn converts_no_input_empty_fallback() {
        // TS: no instructions + no input → messages.length === 0 → fallback user message
        let body = json!({"model": "gpt-5"});
        let out = responses_to_openai_chat(&body);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "");
    }

    #[test]
    fn converts_stream_default_true() {
        let body = json!({"model": "gpt-5", "input": "hi"});
        let out = responses_to_openai_chat(&body);
        assert_eq!(out["stream"], true);
        assert_eq!(out["stream_options"]["include_usage"], true);
    }

    #[test]
    fn converts_stream_explicit_false() {
        let body = json!({"model": "gpt-5", "input": "hi", "stream": false});
        let out = responses_to_openai_chat(&body);
        assert_eq!(out["stream"], false);
        assert!(out.get("stream_options").is_none());
    }

    #[test]
    fn converts_tools() {
        let body = json!({
            "model": "gpt-5",
            "input": "hi",
            "tools": [{
                "type": "function",
                "name": "get_weather",
                "description": "天气",
                "parameters": {"type": "object"}
            }]
        });
        let out = responses_to_openai_chat(&body);
        assert_eq!(out["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(out["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn converts_response_back() {
        let args = serde_json::to_string(&json!({"city": "BJ"})).unwrap();
        let resp = json!({
            "id": "c1",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "调用工具",
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": args}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3}
        });
        let out = openai_chat_to_responses(&resp, "gpt-5");
        assert_eq!(out["status"], "tool_calls");
        let output = out["output"].as_array().unwrap();
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[1]["type"], "function_call");
        assert_eq!(output[1]["name"], "get_weather");
        assert_eq!(out["usage"]["input_tokens"], 5);
    }

    #[test]
    fn stream_text_emits_events() {
        let mut state = create_responses_stream_state("gpt-5");
        let chunk = json!({"choices": [{"delta": {"content": "Hi"}, "finish_reason": null}]});
        let evs = openai_chat_stream_to_responses_stream(&chunk, &mut state);
        assert!(evs.iter().any(|e| e.contains("response.created")));
        assert!(evs.iter().any(|e| e.contains("output_item.added")));
        assert!(evs.iter().any(|e| e.contains("output_text.delta")));
    }

    #[test]
    fn stream_finish_completes() {
        let mut state = create_responses_stream_state("gpt-5");
        state.created_sent = true;
        state.open_type = Some("message".to_string());
        state.open_item_id = next_item_id("msg");
        state.open_index = 0;
        state.next_output_index = 1;
        let chunk = json!({"choices": [{"delta": {}, "finish_reason": "stop"}]});
        let evs = openai_chat_stream_to_responses_stream(&chunk, &mut state);
        assert!(evs.iter().any(|e| e.contains("output_item.done")));
        assert!(evs.iter().any(|e| e.contains("response.completed")));
        assert!(evs.iter().any(|e| e.contains("completed")));
    }
}

// ---------------------------------------------------------------------------
// Stream pipe: upstream OpenAI SSE bytes → Responses SSE bytes
// ---------------------------------------------------------------------------

use futures::stream::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Wraps an upstream byte stream (OpenAI SSE) and transforms it to Responses SSE.
pub struct ResponsesStreamPipe<S>
where
    S: Stream<Item = Result<Vec<u8>, std::io::Error>> + Unpin,
{
    upstream: S,
    state: ResponsesStreamState,
    buffer: Vec<u8>,
    pending_output: Vec<u8>,
}

impl<S> ResponsesStreamPipe<S>
where
    S: Stream<Item = Result<Vec<u8>, std::io::Error>> + Unpin,
{
    pub fn new(upstream: S, model: String) -> Self {
        Self {
            upstream,
            state: create_responses_stream_state(&model),
            buffer: Vec::new(),
            pending_output: Vec::new(),
        }
    }
}

impl<S> Stream for ResponsesStreamPipe<S>
where
    S: Stream<Item = Result<Vec<u8>, std::io::Error>> + Unpin,
{
    type Item = Result<Vec<u8>, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if !self.pending_output.is_empty() {
            let out = std::mem::take(&mut self.pending_output);
            return Poll::Ready(Some(Ok(out)));
        }

        match Pin::new(&mut self.upstream).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                self.buffer.extend_from_slice(&chunk);
                while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = self.buffer.drain(..=pos).collect();
                    let line_str = String::from_utf8_lossy(&line);
                    let trimmed = line_str.trim();
                    if let Some(json_str) = trimmed.strip_prefix("data: ") {
                        if json_str == "[DONE]" {
                            continue;
                        }
                        if let Ok(chunk_val) = serde_json::from_str::<Value>(json_str) {
                            let events = openai_chat_stream_to_responses_stream(&chunk_val, &mut self.state);
                            for evt in events {
                                self.pending_output.extend_from_slice(evt.as_bytes());
                            }
                        }
                    }
                }
                if self.pending_output.is_empty() {
                    Poll::Pending
                } else {
                    let out = std::mem::take(&mut self.pending_output);
                    Poll::Ready(Some(Ok(out)))
                }
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => {
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
