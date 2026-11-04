# deveco-local-model-dashboard

> DevEco CodeGenie（华为）免费模型本地代理 — 纯 Rust 实现，跨平台。
>
> 这是 [`deveco-local-model-dashboard`](https://github.com/atomgit-atomcode/deveco-local-model-dashboard)（Node.js/TypeScript）的 Rust 重写，
> 消除了 Node.js 运行时依赖，编译为单个原生二进制（~10MB），可在 Windows / Linux / macOS 上直接运行。

---

## 它做什么

一个本地 HTTP 代理，让任何 OpenAI / Anthropic / Responses 兼容的 harness（atomcode-deveco、Claude Code、Codex CLI…）
通过标准 API 接入华为 DevEco CodeGenie 的免费模型（GLM-5.x / DeepSeek 等）：

```
harness  ──►  http://127.0.0.1:666/v2  (本代理)
                     │  + 注入 Authorization: Bearer <devEco token>
                     │  + 协议转换（Anthropic/Responses → OpenAI Chat）
                     │  + 图片路由 / tool_choice 归一化 / thinking 注入
                     ▼
              https://cn.devecostudio.huawei.com/sse/codeGenie/maas/v2
```

代理负责：华为 OAuth 登录、accessToken 缓存与 30 分钟刷新、header 注入、流式/非流式转发、
Anthropic ↔ OpenAI 协议转换、Responses API ↔ OpenAI 转换、图片 vision 路由、SSE 心跳。

---

## 快速开始

### 编译

```bash
cargo build --release
# 产物：target/release/deveco-local-model-dashboard(.exe)
```

### 运行

```bash
# 默认端口 666，自动打开浏览器仪表板
./deveco-local-model-dashboard

# 指定端口 + 不自动打开浏览器
./deveco-local-model-dashboard --port=7777 --no-dashboard

# 允许局域网访问
./deveco-local-model-dashboard --host=0.0.0.0
```

### 首次登录

1. 打开 `http://127.0.0.1:666/v2/dashboard`
2. 点击「登录」→ 浏览器跳转华为账号登录页
3. 登录成功后回调自动完成，token 持久化到 `~/.config/opencode/deveco-local-model-dashboard/jwt.json`

### 接入 harness

在 harness 的配置中指向本代理：

```toml
# atomcode / opencode config.toml
[providers.codegenie]
type = "openai-compatible"
api = "http://127.0.0.1:666/v2"
api_key = "dummy"   # 代理注入真实 token，这里填占位符
model = "glm-5.2"
```

---

## API 端点

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/v2/chat/completions` | OpenAI Chat Completions 转发（流式/非流式） |
| POST | `/v2/anthropic/v1/messages` | Anthropic Messages API 转发（自动转换 → OpenAI） |
| POST | `/v2/responses` | OpenAI Responses API 转发（Codex CLI，自动转换 → Chat） |
| GET | `/v2/models` | 列出可用 DevEco 模型 |
| GET | `/v2/status` | 代理状态（登录、token、负载） |
| GET | `/v2/login` | 触发华为 OAuth 登录 |
| GET | `/v2/logout` | 清除凭据 |
| POST | `/v2/probe` | 模型在线探测（仪表板「测试」按钮） |
| GET/POST | `/v2/network` | 局域网访问开关 |
| POST | `/v2/lan-check` | 局域网可达性诊断 |
| POST | `/v2/lan-fix-firewall` | 一键放行防火墙（Windows UAC） |
| POST | `/v2/shutdown` | 关闭代理 |
| GET | `/v2/dashboard` | HTML 仪表板 |

---

## 支持的模型

| 模型 ID | 上下文 | 输出 | 说明 |
|---|---|---|---|
| `GLM-5.1` | 202K | 131K | 智谱 GLM，文本 |
| `glm-5.2` | 1M | 131K | 智谱 GLM 5.2，文本 |
| `deepseek-v4-flash` | 1M | 393K | DeepSeek V4 Flash，需 thinking 注入 |
| `Qwen2.5-VL-72B` | 32K | 8K | 通义千问 VL，图片 |
| `Qwen3_VL_235B_A22B_Instruct` | — | — | Vision fallback（自动路由） |

动态模型列表每小时从华为 `/codeGenie/modelConfig` API 刷新一次。

---

## 配置

### 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `DEVECO_PROXY_PORT` | 666 | 代理端口 |
| `DEVECO_PROXY_HOST` | 127.0.0.1 | 绑定地址 |
| `DEVECO_LOG_LEVEL` | info | 日志级别（debug/info/warn/error） |
| `DEVECO_VISION_MODEL` | Qwen3_VL_235B… | Vision fallback 模型 |
| `DEVECO_TEXT_ONLY_MODELS` | GLM-5.1 | 纯文本模型（逗号分隔） |
| `DEVECO_QUEUE_RETRIES` | 30 | 上游排队（WaitingInLine）连接断开后未获服务时的自动重试次数（上限 60；重试保留排队位，繁忙队列约 2s/位） |
| `DEVECO_QUEUE_RETRY_DELAY_MS` | 2000 | 排队重试间隔毫秒数（1–600000） |
| `OPENCODE_CONFIG_DIR` | ~/.config/opencode | 配置目录 |

### 排队（WaitingInLine）处理

上游模型繁忙时会通过 SSE 事件 `data: {"type":"WaitingInLine",...}` 告知请求正在排队。代理会：

- **吞掉排队事件**，不透传给客户端（避免破坏外接工具的 SSE 解析），期间持续发送心跳保活；
- 排队期间**留在队中等待**；若队列连接被上游关闭且始终未获服务，自动重新请求（最多 `DEVECO_QUEUE_RETRIES` 次，间隔 `DEVECO_QUEUE_RETRY_DELAY_MS`）；
- 重试耗尽仍在排队 → 返回 503 与干净的错误信息，且不调用 exitSessionQueue（会话可能仍持有排队位）；
- 一旦收到真实模型数据，正常透传并按原有逻辑收尾。

### Token 存储路径

- Linux/macOS: `~/.config/opencode/deveco-local-model-dashboard/jwt.json`
- Windows: `%USERPROFILE%\.config\opencode\deveco-local-model-dashboard\jwt.json`

文件权限 0600（Unix），格式：`{"jwt": "...", "savedAt": 1234567890}`

---

## 与原 deveco-local-model-dashboard 的关系

| 维度 | deveco-local-model-dashboard (TS) | deveco-local-model-dashboard (Rust) |
|---|---|---|
| 运行时 | Node.js 18+ | 无（原生二进制） |
| 打包 | `@yao-pkg/pkg` → 92MB exe | `cargo build` → ~10MB |
| 跨平台 | 需分别打包 | 同一代码编译三平台 |
| 功能 | 完整 | 完整（1:1 移植） |
| 测试 | vitest | `cargo test` |

---

## 开发

```bash
# 编译检查
cargo check

# 测试
cargo test

# Release 构建
cargo build --release

# 运行（debug）
cargo run -- --port=666
```

### 项目结构

```
src/
├── main.rs                 CLI 入口
├── config.rs               全局常量、URL、超时、模型默认值、日志
├── models.rs               模型探测（/modelConfig API）+ 缓存
├── dashboard.rs            HTML 仪表板生成
├── lan.rs                  LAN 可达性诊断 + 防火墙
├── auth/
│   ├── mod.rs
│   ├── jwt.rs              JWT 解码 + 过期判定
│   ├── token_store.rs      JWT 持久化（JSON, 0600）
│   └── login.rs            OAuth 登录流程 + token 刷新
├── transform/
│   ├── mod.rs
│   ├── normalize.rs        tool_choice 归一化
│   ├── vision.rs           图片路由
│   ├── anthropic.rs        Anthropic ↔ OpenAI 转换
│   └── responses.rs        Responses API ↔ Chat 转换
└── proxy/
    ├── mod.rs
    ├── server.rs           axum 路由 + 状态管理 + token 管理
    └── forward.rs          请求转发（Chat/Anthropic/Responses → DevEco）
```

---

## License

MIT
