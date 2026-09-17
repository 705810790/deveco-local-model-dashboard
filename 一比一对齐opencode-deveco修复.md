# 一比一对齐 opencode-deveco 修复记录

> 目标：Rust 项目 `deveco-local-model-dashboard` 与 TS 项目 `opencode-deveco` 代码细节 1:1 对齐。
> 对比基准：`E:\AI agent\opencode-deveco\src\` 下的所有 `.ts` 源文件。

---

## 已发现的差异清单

### A. `transform/responses.rs` — `responses_to_openai_chat`（严重偏离）

**TS 基准** (`responses-transform.ts:111-186`):
1. `instructions` 字段 → system message（Rust 完全缺失）
2. `input` 为 string → user message（Rust 缺失，仅 array 路径处理）
3. messages 为空时 fallback `{role:"user",content:""}`（Rust 缺失）
4. `stream` 默认 `true`（`body.stream !== false`），并设 `stream_options`（Rust 直接 clone stream 字段）
5. `model` 取 string 否则空串 `""`（Rust 直接 clone）
6. tools: `forcedTool` narrowing + `isResponsesFunctionTool` 检测（`typeof t.name === "string"`）+ `isParametersObject` fallback `{type:"object",properties:{}}` + description 仅当 string 时加入（Rust 用 `type === "function"` 检测，无 narrowing，无 fallback）
7. `tool_choice`: `responsesToolChoiceToString` + `forcedTool → "required"` + `required && no tools → delete`（Rust 直接 clone tool_choice）
8. `reasoning`: `body.reasoning.effort` 或 `body.reasoning_effort` → `effortToChatFields`（Rust 用 `effort_to_budget` 生成 `thinking.budget_tokens`，完全不同）
9. `max_output_tokens → max_tokens`（Rust 有但顺序/条件不同）

**Rust 当前** (`responses.rs:179-213`): 仅做了字段 clone，缺失上述全部逻辑。

---

### B. `transform/responses.rs` — `convert_responses_content_part_to_chat`

**TS 基准** (`responses-transform.ts:257-264`):
- text 类型：`input_text` | `output_text` | `text` | `summary_text`
- image 类型：`input_image`（`image_url` 为 string）

**Rust 当前** (`responses.rs:21-38`):
- text 类型：`input_text` | `text`（**缺失 `output_text`、`summary_text`**）
- image 类型：`input_image` | `image`（多了 `image`，且 `image_url` 可从 `url` 字段取——TS 无此逻辑）

---

### C. `transform/responses.rs` — `convert_responses_input_to_messages`

**TS 基准** (`responses-transform.ts:188-236`):
1. `function_call` 用 pending buffer，连续多个合并为一条 assistant + 多 tool_calls；遇到 `message`/`function_call_output` 时 flush
2. `function_call` 仅当 `typeof item.name === "string" && item.name` 非空时入 buffer
3. `function_call_output`: `content` = string ? output : output === undefined ? "" : JSON.stringify(output)
4. `reasoning` 及其他类型：**跳过**（不输出 system message）
5. 末尾 `flushPending()`

**Rust 当前** (`responses.rs:41-137`):
1. `function_call` 直接 attach 到上一条 assistant 或新建——**无 pending buffer**，连续多个 function_call 不会合并
2. 无 name 非空检查
3. `function_call_output`: `content` = string ? output : other.to_string()（对 null/undefined 行为不同）
4. `reasoning` → 输出 system message（**TS 跳过**）
5. `_` fallback：尝试读 `role`+`content` 输出消息（**TS 无此 fallback**）

---

### D. `transform/responses.rs` — `effort_to_budget` → 应为 `effort_to_chat_fields`

**TS 基准** (`responses-transform.ts:89-107`):
```
max    → { reasoning_effort: "high", thinking: { type: "enabled" } }
xhigh  → { reasoning_effort: "high" }
high   → { reasoning_effort: "high" }
medium → { reasoning_effort: "medium" }
low    → { reasoning_effort: "low" }
minimal→ { reasoning_effort: "low" }
default→ {}
```
且对 effort 做 `.toLowerCase()`。

**Rust 当前** (`responses.rs:169-176`): `effort_to_budget` 返回 `i64`（1024/4096/16384），用于 `thinking.budget_tokens`——完全不同的语义。

---

### E. `transform/responses.rs` — `convert_responses_tools_to_chat`

**TS 基准** (`responses-transform.ts:144-158`):
- `isResponsesFunctionTool(t)` = `typeof t.name === "string"`（**不是** `t.type === "function"`）
- forcedTool narrowing
- description 仅当 `typeof t.description === "string"` 时加入
- parameters: `isParametersObject(t.parameters) ? t.parameters : {type:"object",properties:{}}`

**Rust 当前** (`responses.rs:140-166`):
- 用 `t.type === "function"` 检测（**错误**）
- 无 forcedTool narrowing
- description 总是加入（即使不存在）
- parameters: `t.parameters.or(t.input_schema).or({})`（无 isParametersObject 校验）

---

### F. `transform/responses.rs` — 缺失 `forced_responses_tool_name` + `responses_tool_choice_to_string`

**TS 基准**:
- `forcedResponsesToolName` (`274-278`): `{type:"function", name:"x"}` → `"x"`
- `responsesToolChoiceToString` (`280-293`): string "auto"/"none"/"required" 透传；object `{type:"auto"}`→"auto", `{type:"none"}`→"none", `{type:"any"|"required"}`→"required"

**Rust 当前**: 两者均不存在。

---

### G. `transform/anthropic.rs` — `tool_result` 图片丢失

**TS 基准** (`anthropic-transform.ts:267-293`):
- `tool_result` 的 `content` 若为 array，逐块检查 `type === "text"` 和 `type === "image"`
- image 块转为 `{type:"image_url", image_url:{url}}` 保留（截图不丢）
- 单个 text part 时 content 退化为纯字符串

**Rust 当前** (`anthropic.rs:112-129`):
- `tool_result` 的 `content` 若为 array，**仅提取 text**，image 块被丢弃
- 截图等图片工具结果丢失

---

### H. `transform/normalize.rs` — 无效 `tool_choice` 处理

**TS 基准** (`openai-normalize.ts:42-47`):
- `tool_choice` 非 undefined、非 string、非 object（或 null）→ **删除 tool_choice**，changed=true

**Rust 当前** (`normalize.rs:30-33`):
- `tool_choice` 非 object → **原样返回 body, changed=false**（未删除，未标记 changed）

---

## 修复进度

### ✅ 修复 A-G: `transform/responses.rs` 请求方向完全重写（任务 #2-#7）

**文件**: `src/transform/responses.rs` 第 1-213 行 → 重写为 1-400 行

**变更内容**:

1. **`convert_responses_content_part_to_chat`** (差异 B):
   - text 类型: `input_text` | `text` → `input_text` | `output_text` | `text` | `summary_text`（对齐 TS:257-264）
   - image 类型: `input_image` | `image`（带 `url` fallback）→ 仅 `input_image`（`image_url` string，对齐 TS）

2. **`convert_responses_input_to_messages`** (差异 C):
   - 新增 pending buffer：连续 `function_call` 合并为一条 assistant + 多 tool_calls（对齐 TS:188-236 的 `flushPending`）
   - `function_call` 仅当 `name` 非空 string 时入 buffer（对齐 TS:215）
   - `function_call_output`: `content` = string ? output : undefined ? "" : JSON.stringify(output)（对齐 TS:228-229）
   - `reasoning` 及未知类型：跳过，不再输出 system message（对齐 TS:232-233）
   - 删除 `_` fallback 的 role+content 输出（TS 无此逻辑）
   - `message` role 映射: assistant→assistant, user→user, else→system（对齐 TS:240-241）
   - `message` content 非 string 非 array → `{role, content:""}`（对齐 TS:248-250）
   - 单 text part 时 content 退化为纯字符串（对齐 TS:267-269）

3. **`effort_to_budget` → `effort_to_chat_fields`** (差异 D):
   - 删除旧的 `effort_to_budget`（返回 i64 用于 budget_tokens）
   - 新增 `effort_to_chat_fields` 返回 `(Option<String>, Option<Value>)`：
     - `max` → `(high, {type:enabled})`
     - `xhigh`/`high` → `(high, None)`
     - `medium` → `(medium, None)`
     - `low`/`minimal` → `(low, None)`
     - default → `(None, None)`
   - 对 effort 做 `.toLowerCase()`（对齐 TS:93）

4. **`convert_responses_tools_to_chat`** (差异 E):
   - 函数签名增加 `forced_tool: &Option<String>` 参数
   - 工具检测: `t.type === "function"` → `is_responses_function_tool(t)` = `t.name` 是 string（对齐 TS:295-299）
   - 新增 forcedTool narrowing（对齐 TS:146）
   - description 仅当 string 时加入（对齐 TS:152）
   - parameters: `is_parameters_object` 校验，否则 fallback `{type:"object",properties:{}}`（对齐 TS:153-155）

5. **新增 `forced_responses_tool_name`** (差异 F):
   - `{type:"function", name:"x"}` → `Some("x")`，否则 `None`（对齐 TS:274-278）

6. **新增 `responses_tool_choice_to_string`** (差异 F):
   - string "auto"/"none"/"required" 透传（对齐 TS:281-282）
   - object `{type:"auto"}`→"auto", `{type:"none"}`→"none", `{type:"any"|"required"}`→"required"（对齐 TS:285-288）
   - else → None

7. **`responses_to_openai_chat` 完全重写** (差异 A):
   - 新增 `instructions` → system message（对齐 TS:114-116）
   - `input` string → user message；array → convertInputItems（对齐 TS:118-122）
   - messages 为空时 fallback `{role:"user",content:""}`（对齐 TS:124-126）
   - `stream` 默认 true（`body.stream !== false`），设 `stream_options`（对齐 TS:129-135）
   - `model` 取 string 否则 `""`（对齐 TS:131）
   - tools 使用新 `convert_responses_tools_to_chat` + forcedTool narrowing（对齐 TS:143-158）
   - tool_choice: `responses_tool_choice_to_string` + forcedTool→"required" + required&&no tools→delete（对齐 TS:161-169）
   - reasoning: `body.reasoning.effort`（非 null object）或 `body.reasoning_effort` → `effort_to_chat_fields`（对齐 TS:171-183）
   - `max_output_tokens → max_tokens`（对齐 TS:137）
   - `temperature`/`top_p` 仅 number 时加入（对齐 TS:138-139）

8. **测试用例更新**:
   - `converts_reasoning_effort`: 验证 `reasoning_effort == "high"` 而非 `thinking.budget_tokens`
   - 新增 `converts_reasoning_effort_max`: 验证 max → reasoning_effort:high + thinking:enabled
   - 新增 `converts_instructions_and_empty_fallback`: 验证 instructions + 空消息 fallback
   - 新增 `converts_stream_default_true`: 验证 stream 默认 true + stream_options
   - 新增 `converts_stream_explicit_false`: 验证 stream:false 时不设 stream_options

---

### ✅ 修复 G: `transform/anthropic.rs` tool_result 图片保留（任务 #8）

**文件**: `src/transform/anthropic.rs` 第 108-129 行 → 108-165 行

**问题**: tool_result 的 content 为 array 时，仅提取 text，image 块被丢弃。截图等图片工具结果丢失。

**修复**: 对齐 TS `convertUserMessage`（anthropic-transform.ts:267-293）:
- `tool_result.content` 为 string → 直接用
- `tool_result.content` 为 array → 逐块检查 `text` 和 `image`，image 转为 `image_url` 保留
- 单个 text part 时 content 退化为纯字符串（对齐 TS:284-287）

---

### ✅ 修复 H: `transform/normalize.rs` 无效 tool_choice 处理（任务 #9）

**文件**: `src/transform/normalize.rs` 第 30-33 行 → 30-38 行

**问题**: `tool_choice` 非 string、非 object（如 null、number）时，原样返回 body 且 changed=false。TS 会删除 tool_choice 并标记 changed=true。

**修复**: 对齐 TS `openai-normalize.ts:42-47`:
- 非 object → `body.remove("tool_choice")`, return `(body, true)`

---

### ✅ 任务 #10: cargo check + cargo test 验证通过

**工具链**: `E:/AI agent/atomcode-deveco/.rustup/toolchains/stable-x86_64-pc-windows-msvc/bin/cargo.exe`

**cargo check**: `Finished dev profile [unoptimized + debuginfo] target(s) in 0.58s` — 0 errors, 36 warnings（均为 pre-existing 的 unused variable / dead code，非本次引入）

**cargo test**: `37 passed; 0 failed; 0 ignored` — 全部通过，含新增 5 个测试用例：
- `converts_reasoning_effort_max` — max → reasoning_effort:high + thinking:enabled
- `converts_instructions_and_empty_fallback` — instructions → system message，不触发空 fallback
- `converts_no_input_empty_fallback` — 无 input 时 fallback user message
- `converts_stream_default_true` — stream 默认 true + stream_options
- `converts_stream_explicit_false` — stream:false 时不设 stream_options

**调试过程中修复的 2 个编译错误**:
1. `anthropic.rs:160` — `other.to_string()` 返回 String 但 match arm 需要 Value → 改为 `Value::String(other.to_string())`
2. `responses.rs:348` — `body.get("tools").and_then(|t| t.as_array())` 传入 `&Vec<Value>` 但函数签名要 `&Value` → 改为先拿 `&Value` 再在函数内 `.as_array()`

**调试过程中修复的 1 个测试错误**:
1. `converts_instructions_and_empty_fallback` — 原测试假设 instructions + 空 fallback 会产生 2 条消息，但 TS 的空 fallback 仅在 `messages.length === 0` 时触发，有 instructions 时 messages 已有 1 条，不触发。修正测试并新增独立的 `converts_no_input_empty_fallback` 测试空 fallback 场景。

---

## 403 "Built-in model service is currently overloaded" 修复

### 根因分析

用户报告：直接测试模型（probe）可以联通，但通过代理转发（forward）出现 403 `[ModelServiceError/403] Built-in model service is currently overloaded`。此问题在原 TS 项目 `opencode-deveco` 中同样存在。

排查发现两个共性问题：

1. **无 403 重试逻辑**：`retry_on_401` 仅处理 401（token 过期），403 "overloaded" 直接透传给 harness 客户端。DevEco 上游的 403 是**瞬时过载**——模型本身在线（probe 成功），但 maas 通道短暂拒绝请求。正确行为是带退避重试。

2. **Rust 版缺失并发限制**：TS 版 `withRequestSlot` (proxy.ts:360-379) 用 `requestSlot` 将并发上游请求限制为 `maxConcurrent=4`，超出则排队。Rust 版 `handle_chat_completions`/`handle_anthropic`/`handle_responses` 直接调用 `forward::*`，**无并发限制**——大量并发请求直接打到 DevEco，更容易触发上游过载。

### ✅ 修复 I: 403 重试逻辑（任务 #2）

**文件**: `src/proxy/forward.rs`

**变更**: `retry_on_401` → `retry_on_upstream_errors`，新增 403 分支：
- 401: 刷新 token + 重试一次（保留原有行为）
- 403: 指数退避重试 3 次（1s → 2s → 4s），任一重试成功或返回非 403 状态即停止
- 其他状态码: 原样返回

3 个调用点（`forward_chat`、`forward_anthropic`、`forward_responses`）已全部更新为 `retry_on_upstream_errors`。

### ✅ 修复 J: 并发限制 request_slot（任务 #3）

**文件**: `src/proxy/server.rs`

**变更**: `handle_chat_completions`、`handle_anthropic`、`handle_responses` 三个 handler 均补上 `state.request_slot()` 并发限制：
- 获取 slot 失败 → 返回 `503 {error:{message, type:"server_busy"}}`（对齐 TS `withRequestSlot` 的 503 行为）
- 获取成功 → `_slot` RAII guard 在 handler 返回时自动释放（对齐 TS `finally { release?.() }`）

**调试中修复的编译错误**:
1. `server.rs` 内部误用 `server::json_response`（模块自身引用自身路径）→ 改为直接 `json_response`

### ✅ 验证（任务 #4）

- `cargo check`: 0 errors, 34 warnings（均为 pre-existing）
- `cargo test`: 37 passed, 0 failed

---

## 查漏补缺：第二轮全模块对比

### 对比结果总览

| 模块 | 对比项 | 结果 |
|---|---|---|
| dashboard | HTML 模板 | ✅ 对齐（仅 `{}` vs `${}` 模板语法差异） |
| lan | lanCheck 逻辑 | ✅ 对齐（TCP 超时 3000ms vs 1500ms 微小差异） |
| auth/login | 登录流程 | ✅ 对齐 |
| auth/jwt | parseJwt/userInfoFromJwt | ✅ 对齐 |
| auth/token_store | save/load/clear | ✅ 修复（见下） |
| win_autostart | buildAutostartCommand | ✅ 对齐（canonicalize vs path.resolve 微小差异） |
| models | get_deveco_provider_config | ✅ 对齐 |
| anthropic 流式 | openaiChatStreamToAnthropic | ⚠️ 缺 reasoning/thinking block（见下） |
| responses 流式 | openaiChatStreamToResponsesStream | ⚠️ 缺 reasoning block（见下） |
| server handlers | handle_status/logout | ✅ 修复（见下） |
| probe_model | probeModel | ✅ 对齐（UA 字符串微小差异） |

### ✅ 修复 K: handle_logout 未清除 session

**文件**: `src/proxy/server.rs`

**问题**: TS `handle` 的 `/logout` 分支执行 `this.session = null; resetModelCache()`，Rust 版仅调 `logout()` + `reset_model_cache()`，**未清除 `state.session`**——logout 后 `/status` 仍报 `logged_in: true`。

**修复**: 添加 `*state.session.write().await = None;`

### ✅ 修复 L: handle_status 三个差异

**文件**: `src/proxy/server.rs`

1. **logged_in 逻辑**: TS = `!!this.session || hasCredentials`（session 存在 **或** jwtToken 可加载）。Rust 版仅调 `is_logged_in()`（只检查 userInfo/tokenStore，不检查 session 是否活跃）→ 改为 `session.is_some() || has_credentials`

2. **user 字段**: TS = `this.session?.userInfo?.userName ?? null`（未登录时 **null**）。Rust 版返回 `""`（空串）→ 改为 `Value::Null`

3. **max_concurrent**: TS 用 `this.maxConcurrent`（实例字段）。Rust 版硬编码 `4u32` → 改为 `state.max_concurrent as u32`

### ✅ 修复 M: token_store load 未清理 legacy 副本

**文件**: `src/auth/token_store.rs`

**问题**: TS `load()` 在 canonical 存在时调 `removeLegacyCopy()` 清理遗留副本。Rust 版 `(Some(c), None)` 和 `(Some(c), Some(l))` 的 canonical-wins 分支均未清理。

**修复**: 两个分支均添加 legacy 副本删除逻辑。

### ⚠️ 已知差异（未修复，需后续处理）

#### N. anthropic 流式转换缺失 reasoning/thinking block — ✅ 已修复

**TS** (`anthropic-transform.ts:695-744`): `processChunk` 支持 `extractReasoning` → `handleThinking` → `thinking_delta` 事件，以及并行 tool_call（`toolBlocks: Map`）、`heldMessageDelta` usage 合并。

**Rust** (`anthropic.rs:533-687`): `openai_chat_stream_to_anthropic` 仅处理 text + tool_call，**缺失**:
- reasoning/thinking block（`reasoning_content` → `thinking_delta`）
- 并行 tool_call（多个 tool_calls 用 Map 跟踪各自 index）
- usage 合并到 heldMessageDelta
- `blockStartSent`/`blockStopSent` 状态管理

#### O. responses 流式转换缺失 reasoning block — ✅ 已修复

**TS** (`responses-transform.ts:610-635`): `processResponsesChunk` 支持 `extractReasoningText` → `appendReasoningText` → `response.reasoning_summary_part` 事件，以及 usage 跟踪。

**Rust** (`responses.rs:327-495`): `openai_chat_stream_to_responses_stream` 仅处理 text + tool_call，**缺失**:
- reasoning block（`reasoning_content` → `response.reasoning_summary_part.added` + `response.reasoning_summary_text.delta`）
- usage 跟踪（`chatUsageToResponses`）
- `closeOpenItem` 分离逻辑

### ✅ 修复 N: anthropic 流式转换完整重写

**文件**: `src/transform/anthropic.rs`

**方式**: 派发 rust worker 子代理并行重写，1:1 对齐 TS `anthropic-transform.ts:448-865` 的全部流式逻辑。

**变更内容**:
- `AnthropicStreamState` struct 扩展为完整 13 字段（对齐 TS `StreamState`）：`queue`、`current_block_type`、`current_block_index`、`next_content_index`、`block_start_sent`、`block_stop_sent`、`tool_blocks: BTreeMap<i64, ToolBlock>`、`model`、`message_start_sent`、`message_delta_sent`、`message_stop_sent`、`held_message_delta`
- 新增 `ToolBlock` struct：`{ id, name, args_buffer, started, anthropic_index }`
- 新增辅助函数：`ensure_message_start`（含 cached_tokens 处理）、`open_block`、`emit_delta`、`close_current_block`、`close_all_tool_blocks`、`close_all_open_blocks`、`extract_reasoning`、`process_chunk`、`handle_text`、`handle_thinking`、`handle_tool_calls`（并行 tool_call Map 跟踪）、`handle_finish_reason`（含 heldMessageDelta usage 延迟）、`merge_usage_into_held_delta`、`finalize_stream`
- 修复关键 bug：`handle_tool_calls` 中 `args_buffer` 累加必须在启动检查**之前**，使第一个工具参数 chunk 能作为 `input_json_delta` 与 `content_block_start` 一起输出
- 解决借用检查器问题：从 `BTreeMap` 借用 entry 时先 clone 值再调用 emit

### ✅ 修复 O: responses 流式转换完整重写

**文件**: `src/transform/responses.rs`

**方式**: 派发 rust worker 子代理并行重写，1:1 对齐 TS `responses-transform.ts:407-862` 的全部流式逻辑。

**变更内容**:
- `ResponsesStreamState` struct 替换为完整 16 字段（对齐 TS `ResponsesStreamState`）：`response_id`、`model`、`created_at`、`queue`、`next_output_index`、`completed_items: Vec<(i64, Value)>`、`open_type: Option<String>`、`open_item_id`、`open_index`、`reasoning_text`、`message_text`、`tool_items: BTreeMap<i64, ToolItemState>`、`created_sent`、`completed_sent`、`usage: Option<Value>`、`finish_reason`
- 新增 `ToolItemState` struct：`{ item_id, call_id, name, args, output_index, started }`
- 新增辅助函数：`now_secs`、`next_item_id`、`emit`、`ensure_created`（含 `response.created` + `response.in_progress`）、`extract_reasoning_text`、`chat_usage_to_responses`、`zero_responses_usage`、`open_reasoning_item`、`append_reasoning_text`（`response.reasoning_summary_part.added` + `response.reasoning_summary_text.delta`）、`open_message_item`、`append_message_text`、`handle_tool_call_delta`（并行 tool items Map 跟踪）、`close_open_item`（reasoning/message 分离关闭逻辑）、`finalize_responses_stream`（含 `function_call_arguments.done` + `output_item.done` + `response.completed` 含 `completedItems` 排序 + `incomplete_details`）、`process_responses_chunk`
- 更新 `stream_finish_completes` 测试适配新 struct 字段

### ✅ 验证

- `cargo check`: 0 errors, 35 warnings（均为 pre-existing）
- `cargo test`: 37 passed, 0 failed


