//! OpenAI `tool_choice` 归一化。
//!
//! DevEco 后端只接受字符串形式 `"auto" | "none" | "required"`，不接受对象形式。
//! 从 deveco-local-model-dashboard/src/transform/openai-normalize.ts 移植。
//!
//! 规则：
//! - `{type:"function", function:{name:"x"}}` → `"required"`，并把 `tools` 收窄到该工具。
//! - `{type:"auto"}` / `{type:"none"}` / `{type:"required"}` → 对应字符串。
//! - 字符串原样保留。
//! - 缺省 → 不变。

use serde_json::Value;

/// 归一化请求体中的 `tool_choice`。
///
/// 返回 `(body, changed)`：`changed` 为 true 时表示 body 被修改过。
/// 入参 `body` 会被消耗并返回（若未改变则原样返回）。
pub fn normalize_openai_tool_choice(mut body: Value) -> (Value, bool) {
    let tool_choice = match body.get("tool_choice") {
        Some(v) => v.clone(),
        None => return (body, false),
    };

    // 字符串形式已经合规。
    if tool_choice.is_string() {
        return (body, false);
    }

    // 只处理对象形式。
    // TS (openai-normalize.ts:42-47): 非 string、非 object（或 null）→ 删除 tool_choice, changed=true
    let obj = match tool_choice.as_object() {
        Some(o) => o,
        None => {
            let map = body.as_object_mut().unwrap();
            map.remove("tool_choice");
            return (body, true);
        }
    };

    let ty = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match ty {
        // 枚举对象折叠为字符串。
        "auto" | "none" | "required" => {
            body["tool_choice"] = Value::String(ty.to_string());
            (body, true)
        }
        // 指定具体函数：{type:"function",function:{name}} 或 {type:"tool",name}
        "function" | "tool" => {
            let name = obj
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .or_else(|| obj.get("name").and_then(|n| n.as_str()));

            if let Some(name) = name {
                if let Some(tools) = body.get("tools").and_then(|t| t.as_array()).cloned() {
                    let narrowed: Vec<Value> = tools
                        .into_iter()
                        .filter(|t| {
                            t.get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str())
                                == Some(name)
                        })
                        .collect();
                    if !narrowed.is_empty() {
                        body["tools"] = Value::Array(narrowed);
                        body["tool_choice"] = Value::String("required".to_string());
                        return (body, true);
                    }
                }
                // 强制工具未找到 → 回退 auto
                body["tool_choice"] = Value::String("auto".to_string());
                (body, true)
            } else {
                // 没有合法 name → 回退 auto
                body["tool_choice"] = Value::String("auto".to_string());
                (body, true)
            }
        }
        // 未知 type：回退 auto（TS behavior）
        _ => {
            body["tool_choice"] = Value::String("auto".to_string());
            (body, true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn string_choice_unchanged() {
        let body = json!({"tool_choice": "auto"});
        let (out, changed) = normalize_openai_tool_choice(body);
        assert!(!changed);
        assert_eq!(out["tool_choice"], "auto");
    }

    #[test]
    fn enum_object_collapsed() {
        let body = json!({"tool_choice": {"type": "auto"}});
        let (out, changed) = normalize_openai_tool_choice(body);
        assert!(changed);
        assert_eq!(out["tool_choice"], "auto");
    }

    #[test]
    fn function_choice_narrows_tools() {
        let body = json!({
            "tool_choice": {"type": "function", "function": {"name": "bar"}},
            "tools": [
                {"type": "function", "function": {"name": "foo"}},
                {"type": "function", "function": {"name": "bar"}},
            ]
        });
        let (out, changed) = normalize_openai_tool_choice(body);
        assert!(changed);
        assert_eq!(out["tool_choice"], "required");
        let tools = out["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "bar");
    }

    #[test]
    fn missing_choice_unchanged() {
        let body = json!({"model": "x"});
        let (out, changed) = normalize_openai_tool_choice(body);
        assert!(!changed);
        assert_eq!(out["model"], "x");
    }
}
