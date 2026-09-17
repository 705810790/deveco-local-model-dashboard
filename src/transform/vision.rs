//! 图片路由。
//!
//! GLM-5.1 等纯文本模型无法处理图片。当请求里出现图片时：
//! - 若**当前 turn**（最新 user message）含图片 → 换模型为
//!   `DEVECO_VISION_FALLBACK_MODEL`，并删除 `tools` / `tool_choice`
//!   （vision 模型通常不支持 function calling）。
//! - 若仅**历史**含图片（当前 turn 无图片）→ 把历史图片替换为 `[图片]`
//!   文本占位，保持原模型。
//!
//! 从 deveco-local-model-dashboard/src/transform/vision-routing.ts 移植。
//! 引用 `crate::config::{DEVECO_VISION_FALLBACK_MODEL, DEVECO_TEXT_ONLY_MODELS}`。

use crate::config::{DEVECO_TEXT_ONLY_MODELS, DEVECO_VISION_FALLBACK_MODEL};
use serde_json::{json, Value};

/// 图片路由结果。
#[derive(Debug, Clone)]
pub struct VisionRoutingResult {
    /// 可能被改写后的请求体。
    pub body: Value,
    /// 请求中原始的 model。
    pub requested_model: String,
    /// 实际将发往上游的 model（可能被 reroute）。
    pub upstream_model: String,
    /// 是否发生了模型 reroute。
    pub rerouted: bool,
    /// 是否把图片替换成了文本占位（历史图片剥离）。
    pub images_stripped: bool,
}

/// 判断 model 是否为纯文本模型。
/// 读取 `DEVECO_TEXT_ONLY_MODELS` 环境变量覆盖（逗号分隔）。
fn is_text_only(model: &str) -> bool {
    let models: Vec<String> = std::env::var("DEVECO_TEXT_ONLY_MODELS")
        .unwrap_or_else(|_| DEVECO_TEXT_ONLY_MODELS.join(","))
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    models.iter().any(|m| m == model)
}

/// Vision fallback model id.
/// 读取 `DEVECO_VISION_MODEL` 环境变量覆盖。
fn vision_model_id() -> String {
    std::env::var("DEVECO_VISION_MODEL")
        .unwrap_or_else(|_| DEVECO_VISION_FALLBACK_MODEL.to_string())
}

/// 检测一个 OpenAI content 块数组中是否含 `image_url` part。
fn content_has_image(content: &Value) -> bool {
    if let Some(arr) = content.as_array() {
        for part in arr {
            if part.get("type").and_then(|v| v.as_str()) == Some("image_url") {
                return true;
            }
        }
        false
    } else {
        false
    }
}

/// 把一个 content 数组里所有 `image_url` part 替换为文本占位 `[图片]`。
/// 返回新的 content（若没有图片则返回 None）。
fn strip_images_in_content(content: &Value) -> Option<Value> {
    let arr = content.as_array()?;
    let mut changed = false;
    let new_arr: Vec<Value> = arr
        .iter()
        .map(|part| {
            if part.get("type").and_then(|v| v.as_str()) == Some("image_url") {
                changed = true;
                json!({"type": "text", "text": "[图片]"})
            } else {
                part.clone()
            }
        })
        .collect();
    if changed {
        Some(Value::Array(new_arr))
    } else {
        None
    }
}

/// 对请求体应用图片路由。返回 `None` 表示无需处理（非 text-only 模型）。
///
/// 期望 `body` 是 OpenAI Chat Completions 请求格式：
/// ```json
/// { "model": "...", "messages": [ {"role": "user", "content": [...]}, ... ] }
/// ```
pub fn apply_vision_routing(body: &Value) -> Option<VisionRoutingResult> {
    let model = body.get("model").and_then(|v| v.as_str())?;
    if !is_text_only(model) {
        return None;
    }

    let mut new_body = body.clone();
    let mut rerouted = false;
    let mut images_stripped = false;

    let messages = match new_body.get("messages").and_then(|m| m.as_array()) {
        Some(a) => a.clone(),
        None => {
            // 没有 messages，无法判断，原样返回。
            return Some(VisionRoutingResult {
                body: new_body,
                requested_model: model.to_string(),
                upstream_model: model.to_string(),
                rerouted: false,
                images_stripped: false,
            });
        }
    };

    if messages.is_empty() {
        return Some(VisionRoutingResult {
            body: new_body,
            requested_model: model.to_string(),
            upstream_model: model.to_string(),
            rerouted: false,
            images_stripped: false,
        });
    }

    // 当前 turn = 最后一条 assistant 之后的所有消息（含 user + tool_result 截图）。
    // TS currentTurnHasImage: 找最后一条 assistant，从它之后开始扫描所有消息。
    let mut last_assistant_idx: i64 = -1;
    for (i, m) in messages.iter().enumerate() {
        if m.get("role").and_then(|v| v.as_str()) == Some("assistant") {
            last_assistant_idx = i as i64;
        }
    }
    let start = (last_assistant_idx + 1) as usize;

    let current_has_image = (start..messages.len()).any(|i| {
        messages
            .get(i)
            .and_then(|m| m.get("content"))
            .map(|c| content_has_image(c))
            .unwrap_or(false)
    });

    if current_has_image {
        // 当前 turn 有图片 → reroute 到 vision 模型，删除 tools/tool_choice。
        new_body["model"] = json!(vision_model_id());
        rerouted = true;
        if new_body.get("tools").is_some() {
            new_body.as_object_mut().unwrap().remove("tools");
        }
        if new_body.get("tool_choice").is_some() {
            new_body.as_object_mut().unwrap().remove("tool_choice");
        }
    } else {
        // 当前 turn 无图片，但历史可能有 → 替换历史图片为 [图片] 占位。
        let mut new_messages = messages.clone();
        let mut stripped = false;
        for msg in new_messages.iter_mut() {
            if let Some(content) = msg.get("content") {
                if let Some(replaced) = strip_images_in_content(content) {
                    msg["content"] = replaced;
                    stripped = true;
                }
            }
        }
        if stripped {
            new_body["messages"] = Value::Array(new_messages);
            images_stripped = true;
        }
    }

    let upstream_model = new_body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(model)
        .to_string();

    Some(VisionRoutingResult {
        body: new_body,
        requested_model: model.to_string(),
        upstream_model,
        rerouted,
        images_stripped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn non_text_only_returns_none() {
        let body = json!({"model": "Qwen3_VL_235B_A22B_Instruct", "messages": []});
        assert!(apply_vision_routing(&body).is_none());
    }

    #[test]
    fn current_turn_image_reroutes() {
        let body = json!({
            "model": "GLM-5.1",
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "看图"},
                    {"type": "image_url", "image_url": {"url": "data:x"}}
                ]}
            ],
            "tools": [{"type": "function", "function": {"name": "f"}}],
            "tool_choice": "auto"
        });
        let r = apply_vision_routing(&body).unwrap();
        assert!(r.rerouted);
        assert!(!r.images_stripped);
        assert_eq!(r.upstream_model, DEVECO_VISION_FALLBACK_MODEL);
        assert!(r.body.get("tools").is_none());
        assert!(r.body.get("tool_choice").is_none());
    }

    #[test]
    fn history_image_stripped() {
        let body = json!({
            "model": "GLM-5.1",
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "看图"},
                    {"type": "image_url", "image_url": {"url": "data:x"}}
                ]},
                {"role": "assistant", "content": "好的"},
                {"role": "user", "content": "继续"}
            ]
        });
        let r = apply_vision_routing(&body).unwrap();
        assert!(!r.rerouted);
        assert!(r.images_stripped);
        assert_eq!(r.upstream_model, "GLM-5.1");
        // 第一条 user 的图片被替换为文本占位。
        let c = &r.body["messages"][0]["content"];
        assert_eq!(c[1]["type"], "text");
        assert_eq!(c[1]["text"], "[图片]");
    }

    #[test]
    fn no_images_no_change() {
        let body = json!({
            "model": "GLM-5.1",
            "messages": [{"role": "user", "content": "纯文本"}]
        });
        let r = apply_vision_routing(&body).unwrap();
        assert!(!r.rerouted);
        assert!(!r.images_stripped);
        assert_eq!(r.upstream_model, "GLM-5.1");
    }
}
