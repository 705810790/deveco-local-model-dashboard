# 架构设计文档

> deveco-local-model-dashboard — 内部架构与数据流

---

## 1. 总体架构

```
┌─────────────────────────────────────────────────────────┐
│                    harness (atomcode / Claude Code / …)  │
└──────────────────────────┬──────────────────────────────┘
                           │ HTTP POST (OpenAI / Anthropic / Responses)
                           ▼
┌─────────────────────────────────────────────────────────┐
│  proxy/server.rs   axum Router                          │
│  ┌─────────────┐  ┌──────────────┐  ┌───────────────┐  │
│  │ /v2/status  │  │ /v2/models   │  │ /v2/dashboard │  │
│  │ /v2/login   │  │ /v2/probe    │  │ /v2/network   │  │
│  └─────────────┘  └──────────────┘  └───────────────┘  │
│  ┌─────────────────────────────────────────────────┐   │
│  │ proxy/forward.rs                                 │   │
│  │  forward_chat()      forward_anthropic()         │   │
│  │  forward_responses() probe_model()               │   │
│  └─────────────────────────────────────────────────┘   │
└──────────────────────────┬──────────────────────────────┘
                           │
          ┌────────────────┼────────────────┐
          ▼                ▼                 ▼
   ┌──────────┐    ┌──────────────┐  ┌──────────────┐
   │ auth/    │    │ transform/   │  │ config.rs    │
   │ login   │    │ anthropic    │  │ 常量/URL     │
   │ jwt     │    │ responses    │  │ 超时/模型    │
   │ token   │    │ vision       │  │ 日志         │
   └──────────┘    │ normalize    │  └──────────────┘
                   └──────────────┘
                           │
                           ▼
              https://cn.devecostudio.huawei.com
                 /sse/codeGenie/maas/v2
```

## 2. 模块职责

| 模块 | 职责 | 依赖 |
|---|---|---|
| `config.rs` | 全局常量、URL、超时、模型默认值、日志 | 无 |
| `auth/jwt.rs` | JWT 解码（base64）、过期判定 | base64, chrono |
| `auth/token_store.rs` | JWT 持久化（JSON 文件, 0600） | tokio::fs |
| `auth/login.rs` | OAuth 登录流程、回调服务器、token 交换/刷新 | reqwest, tokio::net |
| `models.rs` | 模型探测（/modelConfig API）、缓存、blacklist | reqwest, config |
| `transform/normalize.rs` | tool_choice 对象 → 字符串归一化 | serde_json |
| `transform/vision.rs` | 图片检测 → vision 模型路由 | serde_json, config |
| `transform/anthropic.rs` | Anthropic Messages ↔ OpenAI Chat 双向转换 | serde_json |
| `transform/responses.rs` | Responses API ↔ Chat Completions 双向转换 | serde_json |
| `dashboard.rs` | HTML 仪表板页面生成 | 无 |
| `lan.rs` | LAN IP 枚举、TCP 自连、防火墙诊断 | tokio::net |
| `proxy/server.rs` | axum 路由、状态管理、token 管理、Chat-Id | axum, auth, models |
| `proxy/forward.rs` | 请求转发 + 协议转换 + 401 重试 | reqwest, transform, proxy/server |
| `main.rs` | CLI 参数解析、启动服务器、信号处理 | clap, axum |

## 3. 请求生命周期（以 Anthropic 为例）

```
1. harness → POST /v2/anthropic/v1/messages
   body: { model: "GLM-5.1", messages: [...], stream: true }

2. proxy/server.rs: handle_anthropic()
   → proxy/forward.rs: forward_anthropic()

3. forward_anthropic():
   a. ensure_token() → auth/login.rs: 检查/刷新 accessToken
   b. transform::anthropic::anthropic_to_openai_chat() → 转为 OpenAI 格式
   c. transform::vision::apply_vision_routing() → 检测图片，必要时换模型
   d. chat_id_for() → 生成/复用 Chat-Id
   e. POST → https://cn.devecostudio.huawei.com/sse/codeGenie/maas/v2/chat/completions

4. 上游响应:
   a. 401 → refresh_token() + 重试一次
   b. 200 + stream → transform::anthropic::AnthropicStreamPipe 转换 SSE
   c. 200 + non-stream → openai_chat_to_anthropic() 转换 JSON

5. 响应回传 harness
   → finally: exit_queue() 释放服务端槽位
```

## 4. Token 管理流程

```
                    ┌──────────────┐
                    │ ensure_token │
                    └──────┬───────┘
                           │
              ┌────────────┼────────────┐
              ▼            ▼            ▼
        session有效?    session过期    无session
              │            │            │
             yes           │            │
              │            ▼            ▼
          return token   refresh_token()
                          │
              ┌───────────┼───────────┐
              ▼                       ▼
          jwtToken有效?            jwtToken过期
              │                       │
             yes                     no
              │                       │
              ▼                       ▼
        GET jwToken/check      返回 Err
        (refresh=true)         (需重新登录)
              │
              ▼
        更新 session
        (25-30min jitter)
        return token
```

## 5. 协议转换策略

### Anthropic → OpenAI Chat

| Anthropic | OpenAI |
|---|---|
| `messages[].content[]` (blocks) | `messages[].content` (string or array) |
| `system` (top-level) | `messages[0]` (role=system) |
| `tools[]` (input_schema) | `tools[]` (function.parameters) |
| `tool_choice` (object) | `tool_choice` (string or object) |
| `max_tokens` | `max_tokens` |
| `thinking` | `thinking` (passthrough) |

### Responses → OpenAI Chat

| Responses | OpenAI Chat |
|---|---|
| `input` (items array) | `messages` |
| `reasoning.effort` | `reasoning_effort` + `thinking` |
| `tools[]` (function tool) | `tools[]` |
| `tool_choice` | `tool_choice` |
| `stream` | `stream` |

### Vision Routing

```
请求到达
  │
  ├─ model 是 text-only?  no → 直接转发
  │
  ├─ yes → 当前 turn 有图片?
  │         yes → 换模型为 vision fallback，删除 tools/tool_choice
  │         no  → 历史有图片?
  │                yes → 替换历史图片为 [图片] 文本占位
  │                no  → 直接转发
```

## 6. 并发与槽位

- `active_requests`: AtomicU32，当前活跃请求数
- `queued_requests`: AtomicU32，排队请求数
- `chat_ids`: Mutex<HashMap>，每会话一个 Chat-Id
- `session`: RwLock<Option<AccessTokenSession>>，读写分离
- 401 重试：同一请求最多刷新一次 token

## 7. 跨平台处理

| 功能 | Windows | Linux/macOS |
|---|---|---|
| TLS | native-tls (SChannel) | rustls |
| 文件权限 | 忽略 chmod | chmod 0600 |
| 防火墙 | PowerShell + UAC | 返回 unsupported |
| 开机自启 | 注册表 | systemd（未实现） |
| 浏览器打开 | `open::that` | `open::that` (xdg-open) |
| 信号 | ctrl_c | ctrl_c + SIGTERM |

## 8. 从 TS 移植的映射关系

| TS 源文件 | Rust 目标文件 | 行数变化 |
|---|---|---|
| config.ts (273) | config.rs (334) | +61 (类型定义更详细) |
| auth-login.ts (678) | auth/login.rs (575) + auth/jwt.rs (125) | +22 |
| token-store.ts (185) | auth/token_store.rs (235) | +50 (async) |
| models.ts (218) | models.rs (358) | +140 (Rust 类型) |
| proxy.ts (1936) | proxy/server.rs (397) + proxy/forward.rs (394) | -1145 (axum 简化) |
| anthropic-transform.ts (865) | transform/anthropic.rs (710) | -155 |
| responses-transform.ts (862) | transform/responses.rs (624) | -238 |
| vision-routing.ts (173) | transform/vision.rs (234) | +61 |
| openai-normalize.ts (76) | transform/normalize.rs (121) | +45 |
| dashboard.ts (568) | dashboard.rs (417) | -151 |
| lan-status.ts (310) | lan.rs (449) | +139 |
| standalone.ts (136) | main.rs (133) | -3 |
| **总计 (6829)** | **~4492** | **-2337 (34% 减少)** |
