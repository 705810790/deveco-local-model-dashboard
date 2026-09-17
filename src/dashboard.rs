//! HTML 仪表板页面生成器。从 deveco-local-model-dashboard/src/dashboard.ts 1:1 移植。
//!
//! `render_dashboard(port, host)` 返回一个自包含的 HTML 字符串（内联 CSS + JS），
//! 深色主题、中文界面，通过 fetch 调用 `/v2/status`、`/v2/models`、`/v2/login` 等端点。

/// 生成完整 HTML 仪表板页面。与 TS renderDashboard() 完全一致。
pub fn render_dashboard(port: u16, host: &str) -> String {
    let page_version = "V2.0";
    format!(r##"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="Cache-Control" content="no-store, no-cache, must-revalidate, max-age=0">
<meta http-equiv="Pragma" content="no-cache">
<meta http-equiv="Expires" content="0">
<title>deveco-local-model-dashboard</title>
<style>
  /* ---- Dark theme (default) ---- */
  :root {{
    color-scheme: dark;
    --bg: #0f1420; --fg: #e6ecf5; --muted: #8a97ab;
    --card-bg: #141b2b; --card-border: #222d45; --h2: #c6d3ea;
    --row-border: #1d2840; --id-color: #9fc1ff;
    --ep-bg: #161d2e; --ep-border: #24304a;
    --up-bg: #10351f; --up-fg: #4ade80;
    --down-bg: #3b1220; --down-fg: #f87171;
    --untested-bg: #26314d; --untested-fg: #93a4c2;
    --btn-bg: #1d2b47; --btn-fg: #dfe9ff; --btn-border: #2e3f63; --btn-hover: #26395f;
    --inset-bg: #0c1220; --inset-border: #1d2840; --inset-fg: #c9d7f2;
    --danger-bg: #3b1220; --danger-fg: #f87171; --danger-border: #5c1f31; --danger-hover: #4a1727;
  }}
  /* ---- Light theme ---- */
  :root.light {{
    color-scheme: light;
    --bg: #f5f7fa; --fg: #1a2332; --muted: #6b7a90;
    --card-bg: #ffffff; --card-border: #e0e6ef; --h2: #2d3a50;
    --row-border: #e0e6ef; --id-color: #2563eb;
    --ep-bg: #e8edf5; --ep-border: #d0d9e8;
    --up-bg: #d1fae5; --up-fg: #059669;
    --down-bg: #fee2e2; --down-fg: #dc2626;
    --untested-bg: #f0f2f5; --untested-fg: #6b7a90;
    --btn-bg: #e8edf5; --btn-fg: #1e40af; --btn-border: #c7d3e8; --btn-hover: #d8e0f0;
    --inset-bg: #f8fafc; --inset-border: #e0e6ef; --inset-fg: #2d3a50;
    --danger-bg: #fee2e2; --danger-fg: #dc2626; --danger-border: #fca5a5; --danger-hover: #fecaca;
  }}
  * {{ box-sizing: border-box; margin: 0; padding: 0; }}
  body {{ font-family: "Segoe UI", "Microsoft YaHei", system-ui, sans-serif; background: var(--bg); color: var(--fg); padding: 32px 16px; transition: background .2s, color .2s; }}
  .wrap {{ max-width: 900px; margin: 0 auto; }}
  h1 {{ font-size: 22px; display: flex; align-items: center; gap: 12px; flex-wrap: wrap; }}
  h1 .dot {{ width: 12px; height: 12px; border-radius: 50%; background: #888; flex: none; }}
  h1 .dot.ok {{ background: #2ecc71; box-shadow: 0 0 8px #2ecc7199; }}
  h1 .dot.bad {{ background: #e74c3c; box-shadow: 0 0 8px #e74c3c99; }}
  .sub {{ color: var(--muted); font-size: 13px; margin: 8px 0 24px; }}
  .endpoint {{ font-family: ui-monospace, Consolas, monospace; font-size: 12px; background: var(--ep-bg); border: 1px solid var(--ep-border); padding: 2px 8px; border-radius: 4px; color: var(--id-color); word-break: break-all; }}
  section {{ background: var(--card-bg); border: 1px solid var(--card-border); border-radius: 10px; padding: 20px; margin-bottom: 20px; transition: background .2s, border-color .2s; }}
  section > h2 {{ font-size: 15px; margin-bottom: 14px; color: var(--h2); }}
  table {{ width: 100%; border-collapse: collapse; font-size: 13px; }}
  th, td {{ text-align: left; padding: 8px 10px; border-bottom: 1px solid var(--row-border); vertical-align: middle; }}
  th {{ color: var(--muted); font-weight: 600; font-size: 12px; }}
  td.id {{ font-family: ui-monospace, Consolas, monospace; color: var(--id-color); }}
  .badge {{ display: inline-block; padding: 2px 10px; border-radius: 999px; font-size: 12px; font-weight: 600; }}
  .up {{ background: var(--up-bg); color: var(--up-fg); }}
  .down {{ background: var(--down-bg); color: var(--down-fg); }}
  .untested {{ background: var(--untested-bg); color: var(--untested-fg); }}
  .btn {{ background: var(--btn-bg); color: var(--btn-fg); border: 1px solid var(--btn-border); border-radius: 6px; padding: 4px 12px; font-size: 12px; cursor: pointer; transition: background .15s; }}
  .btn:hover {{ background: var(--btn-hover); }}
  .btn:disabled {{ opacity: .5; cursor: default; }}
  .lat {{ color: var(--muted); font-size: 12px; margin-left: 6px; }}
  .snippet {{ background: var(--inset-bg); border: 1px solid var(--inset-border); border-radius: 8px; padding: 14px; font-family: ui-monospace, Consolas, monospace; font-size: 12.5px; line-height: 1.7; color: var(--inset-fg); white-space: pre-wrap; word-break: break-all; }}
  .field {{ display: flex; align-items: center; gap: 10px; background: var(--inset-bg); border: 1px solid var(--inset-border); border-radius: 8px; padding: 8px 10px; margin-bottom: 8px; flex-wrap: wrap; }}
  .field .k {{ flex: none; width: 110px; color: var(--muted); font-size: 12px; }}
  .field .v {{ font-family: ui-monospace, Consolas, monospace; font-size: 12.5px; color: var(--inset-fg); word-break: break-all; min-width: 0; flex: 1 1 auto; }}
  select.v {{ background: var(--ep-bg); color: var(--id-color); border: 1px solid var(--ep-border); border-radius: 4px; padding: 4px 8px; outline: none; cursor: pointer; }}
  .copy {{ font-size: 11px; padding: 3px 10px; flex: none; }}
  .grid {{ display: grid; grid-template-columns: 1fr 1fr; gap: 16px; }}
  @media (max-width: 720px) {{ .grid {{ grid-template-columns: 1fr; }} }}
  .hint {{ color: var(--muted); font-size: 12.5px; margin-top: 6px; }}
  button.probe {{ min-width: 110px; }}
  pre.err {{ color: var(--down-fg); font-size: 12px; white-space: pre-wrap; word-break: break-all; }}
  .topbar {{ display: flex; align-items: center; justify-content: space-between; gap: 12px; flex-wrap: wrap; }}
  .top-actions {{ display: flex; gap: 8px; align-items: center; }}
  .btn.danger {{ background: var(--danger-bg); color: var(--danger-fg); border-color: var(--danger-border); }}
  .btn.danger:hover {{ background: var(--danger-hover); }}
  .theme-toggle {{ font-size: 16px; padding: 4px 10px; }}
  .countdown {{ font-family: ui-monospace, Consolas, monospace; font-size: 12px; }}
  .countdown.warn {{ color: var(--down-fg); font-weight: 600; }}
  .req-log {{ max-height: 280px; overflow-y: auto; }}
  .req-log table {{ font-size: 12px; }}
  .req-log td {{ padding: 4px 8px; }}
</style>
</head>
<body>
<div class="wrap">
  <div class="topbar">
    <h1><span class="dot" id="probe-dot"></span>deveco-local-model-dashboard</h1>
    <div class="top-actions" id="top-actions">
      <button class="btn theme-toggle" id="theme-btn" onclick="toggleTheme()" title="切换浅色/深色主题">🌙</button>
    </div>
  </div>
  <div class="sub">代理地址 <span class="endpoint">http://{host}:{port}</span> · 版本 <span class="endpoint">{page_version}</span> · 数据自动刷新 · 每个模型可单独"在线测试"</div>

  <section>
    <h2>登录 / 代理状态</h2>
    <table>
      <tr><th style="width:140px">登录状态</th><td><span id="login-state">检测中…</span> <button class="btn" id="login-btn" onclick="startLogin(this)">登录</button></td></tr>
      <tr><th>登录用户</th><td id="login-user">—</td></tr>
      <tr><th>accessToken 剩余</th><td><span id="token-left">—</span> <span class="countdown" id="token-countdown"></span></td></tr>
      <tr><th>请求负载</th><td id="request-load">—</td></tr>
      <tr><th>局域网访问</th><td><span id="lan-state">检测中…</span> <button class="btn" id="lan-btn" onclick="toggleLan(this)">检测中…</button></td></tr>
      <tr><th>开机自启动</th><td><span id="autostart-state">检测中…</span> <button class="btn" id="autostart-btn" style="display:none" onclick="toggleAutostart(this)">…</button></td></tr>
      <tr><th>共享端口</th><td><span class="endpoint" id="share-port">—</span> <button class="btn copy" data-target="share-port">复制</button></td></tr>
    </table>
  </section>

  <section>
    <h2>局域网共享 / 外部可达性</h2>
    <div class="hint">开启「局域网访问」后，同一网络下的手机 / 电脑可以用检测到的地址接入本代理。检测包含：本机自连（验证端口监听）与 Windows 防火墙入站规则查询。</div>
    <div style="display:flex;gap:10px;margin-top:12px;flex-wrap:wrap;align-items:center">
      <button class="btn" id="lan-check-btn" onclick="runLanCheck(this)">检测外部可达性</button>
      <button class="btn" id="lan-fix-btn" style="display:none" onclick="fixFirewall(this)">一键放行防火墙（UAC 提权）</button>
    </div>
    <div id="lan-check-result" style="margin-top:14px"></div>
  </section>

  <section>
    <h2>模型在线情况</h2>
    <table>
      <thead><tr><th>模型 ID</th><th>通道</th><th style="width:170px">上下文 / 输出</th><th style="width:190px">状态</th><th style="width:220px">操作</th></tr></thead>
      <tbody id="model-rows"><tr><td colspan="5" style="color:#8a97ab">加载中…</td></tr></tbody>
    </table>
    <div style="display:flex;justify-content:flex-end;margin-top:12px"><button class="btn" id="probe-all-btn" onclick="probeAll(this)">一键测速全部模型</button></div>
    <div class="hint">"测试"会发送一个极小的请求（思考关闭、max_tokens=8，仅验证连通性与首字节延迟）：先探测流式通道，无响应时自动改走非流式通道，任一通道应答即视为在线。<b>上游排队时探测可能等待几十秒（最长约 60s）</b>，期间徽标会跳动计时——排队结束即返回在线，属正常现象。新上线的远端模型会自动出现在这里（模型列表每小时向上游刷新一次）。</div>
  </section>

  <section>
    <h2>接入第三方工具（harness）</h2>
    <div class="hint">以下任一方式都可用同一个代理和同一个华为账号的多处接入。占位 API Key 由代理自动换成真实 token。</div>
    <div class="grid" style="margin-top:14px">
      <div>
        <h3 style="font-size:13px;color:#c6d3ea;margin-bottom:8px">OpenAI 兼容（推荐，通用）</h3>
        <div class="field"><span class="k">API 类型 (协议)</span><span class="v" id="v-oai-type">OpenAI Compatible</span><button class="btn copy" data-target="v-oai-type">复制</button></div>
        <div class="field"><span class="k">Base URL</span><span class="v" id="v-oai-base">http://{host}:{port}/v2</span><button class="btn copy" data-target="v-oai-base">复制</button></div>
        <div class="field"><span class="k">API Key</span><span class="v" id="v-oai-key">opencode-oauth-dummy-key</span><button class="btn copy" data-target="v-oai-key">复制</button></div>
        <div class="field"><span class="k">模型 (Model)</span><select id="v-oai-model" class="v"></select><button class="btn copy" data-target="v-oai-model">复制</button></div>
      </div>
      <div>
        <h3 style="font-size:13px;color:#c6d3ea;margin-bottom:8px">Anthropic / Claude Code</h3>
        <div class="field"><span class="k">API 类型</span><span class="v" id="v-an-type">Anthropic Messages</span><button class="btn copy" data-target="v-an-type">复制</button></div>
        <div class="field"><span class="k">Base URL</span><span class="v" id="v-an-base">http://{host}:{port}/anthropic</span><button class="btn copy" data-target="v-an-base">复制</button></div>
        <div class="field"><span class="k">API Key</span><span class="v" id="v-an-key">opencode-oauth-dummy-key</span><button class="btn copy" data-target="v-an-key">复制</button></div>
        <div class="field"><span class="k">模型 (Model)</span><select id="v-an-model" class="v"></select><button class="btn copy" data-target="v-an-model">复制</button></div>
      </div>
      <div>
        <h3 style="font-size:13px;color:#c6d3ea;margin-bottom:8px">OpenAI Responses / Codex CLI</h3>
        <div class="field"><span class="k">API 类型</span><span class="v" id="v-rs-type">OpenAI Responses（wire_api="responses"）</span><button class="btn copy" data-target="v-rs-type">复制</button></div>
        <div class="field"><span class="k">Base URL</span><span class="v" id="v-rs-base">http://{host}:{port}/v2</span><button class="btn copy" data-target="v-rs-base">复制</button></div>
        <div class="field"><span class="k">API Key</span><span class="v" id="v-rs-key">opencode-oauth-dummy-key</span><button class="btn copy" data-target="v-rs-key">复制</button></div>
        <div class="field"><span class="k">模型 (Model)</span><select id="v-rs-model" class="v"></select><button class="btn copy" data-target="v-rs-model">复制</button></div>
        <div class="hint">Codex CLI 在 <span class="endpoint">~/.codex/config.toml</span> 设 <span class="endpoint">wire_api = "responses"</span>；请求经代理转换为 maas 通道转发，全部模型可用。</div>
      </div>
    </div>
  </section>

  <section>
    <h2>最近请求日志</h2>
    <div class="hint">代理内存中保留最近 50 条请求摘要（不落盘，重启后清空）。</div>
    <div class="req-log" style="margin-top:12px">
      <table>
        <thead><tr><th style="width:70px">时间</th><th style="width:80px">协议</th><th>模型</th><th style="width:60px">状态</th><th style="width:80px">耗时</th><th style="width:90px">Token (入/出)</th></tr></thead>
        <tbody id="req-log-rows"><tr><td colspan="6" style="color:var(--muted)">加载中…</td></tr></tbody>
      </table>
    </div>
  </section>
</div>

<script>
const $ = (id) => document.getElementById(id);
const FETCH_TIMEOUT = 8000;
const fetchT = (url, init) => fetch(url, Object.assign({{ signal: AbortSignal.timeout(FETCH_TIMEOUT) }}, init));
const fetchSlow = (url, init) => fetch(url, Object.assign({{ signal: AbortSignal.timeout(30_000) }}, init));
const fetchProbe = (url, init) => fetch(url, Object.assign({{ signal: AbortSignal.timeout(70_000) }}, init));

const IS_LOCAL = ["127.0.0.1", "localhost", "::1"].includes(location.hostname);

function escapeHtml(s) {{
  return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}}

const timers = [];

function stopTimers() {{ timers.forEach((t) => clearInterval(t)); }}

if (IS_LOCAL) {{
  const btn = document.createElement("button");
  btn.className = "btn danger";
  btn.textContent = "⏻ 关闭服务";
  btn.onclick = () => shutdownService(btn);
  $("top-actions").appendChild(btn);
}}

async function shutdownService(btn) {{
  if (!confirm("确定要关闭代理服务吗？\\n关闭后本页面将不可用，重新使用请再次启动程序。")) return;
  btn.disabled = true;
  btn.textContent = "正在关闭…";
  try {{
    const r = await fetchT("/v2/shutdown", {{ method: "POST" }});
    const j = await r.json().catch(() => ({{}}));
    if (!r.ok) throw new Error((j.error && j.error.message) || ("HTTP " + r.status));
    stopTimers();
    document.body.innerHTML =
      '<div class="wrap" style="text-align:center;padding-top:120px">' +
      '<div style="font-size:40px">⏻</div>' +
      '<h1 style="justify-content:center">服务已关闭</h1>' +
      '<div class="sub" style="text-align:center">本页面已失效，可以直接关闭浏览器标签。<br>重新使用：双击 deveco-local-model-dashboard.exe（或运行原启动命令）。</div>' +
      '</div>';
  }} catch (e) {{
    btn.disabled = false;
    btn.textContent = "⏻ 关闭服务";
    alert("关闭失败：" + String(e));
  }}
}}

const probeResults = {{}};

async function startLogin(btn) {{
  btn.disabled = true;
  btn.textContent = "打开登录页…";
  try {{
    window.location.href = "/v2/login";
  }} catch (e) {{
    btn.textContent = "登录";
    btn.disabled = false;
  }}
}}

async function refreshStatus() {{
  const dot = $("probe-dot");
  try {{
    const r = await fetchT("/v2/status");
    const s = await r.json();
    dot.className = s.logged_in ? "dot ok" : "dot bad";
    $("login-state").textContent = s.logged_in ? "已登录 ✅" : "未登录 ⚠️";
    $("login-user").textContent = s.user || "—";
    $("token-left").textContent = s.expires_in_ms > 0 ? Math.round(s.expires_in_ms / 60000) + " 分钟" : "—";
    if (s.expires_in_ms > 0) startCountdown(s.expires_in_ms); else $("token-countdown").textContent = "";
    $("request-load").textContent = (s.active_requests || 0) + " 个处理中，" + (s.queued_requests || 0) + " 个排队（上限 " + (s.max_concurrent || "—") + "）";
    const btn = $("login-btn");
    if (btn) btn.textContent = s.logged_in ? "重新登录" : "登录";
  }} catch (e) {{
    dot.className = "dot bad";
    $("login-state").textContent = "代理不可达 / 状态请求失败";
    $("login-user").textContent = "—";
    $("token-left").textContent = "—";
    $("login-state").title = String(e);
  }}
}}

async function refreshNetwork() {{
  try {{
    const r = await fetchT("/v2/network");
    const s = await r.json();
    const open = s.lan_access === true;
    $("lan-state").textContent = open ? "已开放（局域网可访问）" : "仅本机访问";
    $("lan-btn").textContent = open ? "关闭局域网访问" : "开放局域网访问";
    $("lan-btn").dataset.open = String(open);
    $("share-port").textContent = s.port || "—";
  }} catch (e) {{
    $("lan-state").textContent = "状态获取失败";
    $("lan-btn").textContent = "重试";
  }}
}}

async function toggleLan(btn) {{
  const enabled = btn.dataset.open !== "true";
  btn.disabled = true;
  btn.textContent = enabled ? "正在开放…" : "正在关闭…";
  try {{
    const r = await fetchT("/v2/network", {{
      method: "POST",
      headers: {{ "Content-Type": "application/json" }},
      body: JSON.stringify({{ lan_access: enabled }}),
    }});
    const result = await r.json().catch(() => ({{}}));
    if (!r.ok || !result.ok) throw new Error(result.error?.message || ("HTTP " + r.status));
    btn.textContent = "切换中…";
    for (let i = 0; i < 10; i++) {{
      await new Promise((resolve) => setTimeout(resolve, 250));
      await refreshNetwork();
      if (btn.dataset.open === String(enabled)) break;
    }}
  }} catch (e) {{
    $("lan-state").textContent = "切换失败：" + String(e);
    btn.textContent = enabled ? "开放局域网访问" : "关闭局域网访问";
  }} finally {{
    btn.disabled = false;
  }}
}}

async function refreshAutostart() {{
  const state = $("autostart-state");
  const btn = $("autostart-btn");
  try {{
    const r = await fetchT("/v2/autostart");
    const s = await r.json();
    if (s.supported === false) {{
      state.textContent = "仅支持 Windows";
      btn.style.display = "none";
      return;
    }}
    state.textContent = s.enabled ? "已开启 ✅" : "未开启";
    if (s.error) state.title = s.error;
    if (IS_LOCAL) {{
      btn.style.display = "";
      btn.textContent = s.enabled ? "关闭开机自启" : "开启开机自启";
      btn.dataset.enabled = String(s.enabled);
    }}
  }} catch (e) {{
    state.textContent = "状态获取失败";
  }}
}}

async function toggleAutostart(btn) {{
  const enabled = btn.dataset.enabled !== "true";
  btn.disabled = true;
  btn.textContent = enabled ? "正在开启…" : "正在关闭…";
  try {{
    const r = await fetchT("/v2/autostart", {{
      method: "POST",
      headers: {{ "Content-Type": "application/json" }},
      body: JSON.stringify({{ enabled: enabled }}),
    }});
    const j = await r.json().catch(() => ({{}}));
    if (!r.ok) throw new Error(j.error || (j.error && j.error.message) || ("HTTP " + r.status));
    await refreshAutostart();
  }} catch (e) {{
    alert("设置失败：" + String(e));
    await refreshAutostart();
  }} finally {{
    btn.disabled = false;
  }}
}}

async function runLanCheck(btn) {{
  if (btn) {{
    btn.disabled = true;
    btn.textContent = "检测中…";
  }}
  const box = $("lan-check-result");
  box.innerHTML = '<span class="hint">正在检测（含防火墙规则查询，最多约半分钟）…</span>';
  try {{
    const r = await fetchSlow("/v2/lan-check", {{ method: "POST" }});
    const j = await r.json();
    renderLanCheck(j);
  }} catch (e) {{
    box.innerHTML = '<span style="color:#f87171">检测失败：' + escapeHtml(String(e)) + '</span>';
  }} finally {{
    if (btn) {{
      btn.disabled = false;
      btn.textContent = "检测外部可达性";
    }}
  }}
}}

function renderLanCheck(j) {{
  const box = $("lan-check-result");
  const fixBtn = $("lan-fix-btn");
  const heads = {{
    ok: '<div class="badge up" style="font-size:13px">✅ 外部可访问</div>',
    loopback_only: '<div class="badge untested" style="font-size:13px">⚠️ 仅本机监听，请先开启「局域网访问」</div>',
    firewall_blocked: '<div class="badge down" style="font-size:13px">🚧 服务在监听，但防火墙未放行该端口</div>',
    unreachable: '<div class="badge down" style="font-size:13px">❌ 服务未在局域网地址上完成监听</div>',
    no_lan_ip: '<div class="badge untested" style="font-size:13px">⚠️ 未检测到局域网 IP</div>',
  }};
  let rows = "";
  (j.interfaces || []).forEach((itf) => {{
    rows +=
      '<div class="field"><span class="k">' + escapeHtml(itf.name) + '</span>' +
      '<span class="v"><a href="' + escapeHtml(itf.url) + '" target="_blank" rel="noopener" style="color:#9fc1ff">' + escapeHtml(itf.url) + '</a></span>' +
      '<button class="btn copy" data-copy="' + escapeHtml(itf.url) + '">复制</button>' +
      '<span class="lat">' + (itf.reachable
        ? "自连 " + (itf.latencyMs == null ? "?" : itf.latencyMs) + "ms"
        : "自连失败" + (itf.error ? "：" + escapeHtml(itf.error) : "")) + '</span></div>';
  }});
  let fw = "";
  if (j.firewall && j.firewall.checked) {{
    fw = '<div class="hint">防火墙入站规则：' + (j.firewall.allowed
      ? "已放行" + (j.firewall.rules && j.firewall.rules.length ? "（" + j.firewall.rules.map(escapeHtml).join("、") + "）" : "")
      : "未放行") + '</div>';
  }} else if (j.firewall && j.firewall.error) {{
    fw = '<div class="hint">防火墙规则检测失败：' + escapeHtml(j.firewall.error) + '</div>';
  }}
  box.innerHTML =
    (heads[j.verdict] || "") +
    (rows ? '<div style="margin-top:10px">' + rows + '</div>' : "") +
    fw +
    '<div class="hint" style="margin-top:8px">' + escapeHtml(j.hint || "") + '</div>';
  box.querySelectorAll("button.copy").forEach((b) => {{
    b.addEventListener("click", () => copyText(b.dataset.copy, b));
  }});
  fixBtn.style.display = j.verdict === "firewall_blocked" && IS_LOCAL ? "" : "none";
}}

async function fixFirewall(btn) {{
  btn.disabled = true;
  btn.textContent = "正在请求提权…";
  try {{
    const r = await fetchSlow("/v2/lan-fix-firewall", {{ method: "POST" }});
    const j = await r.json().catch(() => ({{}}));
    if (!r.ok) throw new Error(j.error || (j.error && j.error.message) || ("HTTP " + r.status));
    btn.textContent = "已弹出 UAC，确认后约 8 秒自动重新检测…";
    setTimeout(() => runLanCheck(null), 8000);
  }} catch (e) {{
    alert("放行失败：" + String(e));
    btn.textContent = "一键放行防火墙（UAC 提权）";
    btn.disabled = false;
  }}
}}

function channelCellHtml(id) {{
  return "maas HTTP";
}}

function fillModelSelect(sel, ids) {{
  if (!sel) return;
  const prev = sel.value;
  sel.innerHTML = ids
    .map((id) =>
      '<option value="' + id.replace(/&/g, "&amp;").replace(/"/g, "&quot;") + '">' +
      id.replace(/&/g, "&amp;").replace(/</g, "&lt;") + "</option>")
    .join("");
  if (prev && ids.indexOf(prev) >= 0) sel.value = prev;
}}

function badgeHtml(result) {{
  if (!result) return '<span class="badge untested">未测试</span>';
  if (result.status === "up") {{
    const tail = result.tail || "";
    return '<span class="badge up">在线 ✅ <span class="lat">' + result.ms + "ms" +
      (tail ? " · \u201c" + tail + "\u201d" : "") + '</span></span>';
  }}
  if (result.status === "down") {{
    return '<span class="badge down">失败 ❌ <span class="lat">' + result.ms + "ms</span>" +
      (result.err ? '<span class="lat" title="' + result.err + '"> ⓘ</span>' : "") + '</span>';
  }}
  return '<span class="badge untested">测试中…</span>';
}}

async function refreshModels() {{
  try {{
    const r = await fetchT("/v2/models");
    const j = await r.json();
    const rows = (j.data || []).map((m) => {{
      const id = typeof m === "string" ? m : (m.id || "?");
      const limit = (m && m.limit) || {{}};
      const ctx = limit.context ? limit.context.toLocaleString("en-US") : "—";
      const out = limit.output ? limit.output.toLocaleString("en-US") : "—";
      return {{ id, channel: channelCellHtml(id), ctx, out }};
    }});
    if (rows.length === 0) {{
      $("model-rows").innerHTML = '<tr><td colspan="5" style="color:#f87171">未获取到模型（可能未登录）</td></tr>';
      return;
    }}
    const resolved = rows.map((m) => ({{ m, r: probeResults[m.id] || null }}));
    const rowsHtml = resolved
      .map(({{ m, r }}, i) =>
        '<tr>' +
          '<td class="id">' + m.id.replace(/&/g, "&amp;").replace(/</g, "&lt;") + '</td>' +
          '<td>' + m.channel + '</td>' +
          '<td class="lim" style="white-space:nowrap">' + m.ctx + ' / ' + m.out + '</td>' +
          '<td id="cell-' + i + '">' + badgeHtml(r) + '</td>' +
          '<td><button class="btn probe" data-i="' + i + '" data-id="' +
            m.id.replace(/&/g, "&amp;").replace(/"/g, "&quot;") + '">测试在线</button></td>' +
        '</tr>'
      )
      .join("");
    $("model-rows").innerHTML = rowsHtml;
    document.querySelectorAll("button.probe").forEach((b) => {{
      b.addEventListener("click", () => probe(b.dataset.i, b.dataset.id));
    }});
    const ids = rows.map((r) => r.id);
    fillModelSelect($("v-oai-model"), ids);
    fillModelSelect($("v-an-model"), ids);
    fillModelSelect($("v-rs-model"), ids);
  }} catch (e) {{
    $("model-rows").innerHTML = '<tr><td colspan="5" style="color:#f87171">代理不可达</td></tr>';
  }}
}}

function setProbeState(id, result) {{
  probeResults[id] = result;
  const btn = document.querySelector('button.probe[data-id="' + id.replace(/"/g, '\\"') + '"]');
  if (btn) $("cell-" + btn.dataset.i).innerHTML = badgeHtml(result);
}}

async function probe(idx, id) {{
  const btn = document.querySelector('button.probe[data-i="' + idx + '"]');
  if (!btn) return;
  btn.disabled = true;
  setProbeState(id, {{ status: "testing" }});
  const t0 = performance.now();
  const tick = setInterval(() => {{
    const b = document.querySelector('button.probe[data-id="' + id.replace(/"/g, '\\"') + '"]');
    if (b) $("cell-" + b.dataset.i).innerHTML =
      '<span class="badge untested">测试中… ' + Math.round((performance.now() - t0) / 1000) + "s</span>";
  }}, 1000);
  try {{
    const r = await fetchProbe("/v2/probe", {{
      method: "POST",
      headers: {{ "Content-Type": "application/json" }},
      body: JSON.stringify({{ model: id }}),
    }});
    const j = await r.json().catch(() => ({{}}));
    if (!r.ok || !j.ok) {{
      const err = (j.error && (j.error.message || j.error.type)) || ("HTTP " + r.status);
      setProbeState(id, {{ status: "down", ms: Math.round(performance.now() - t0), err: String(err).slice(0, 300) }});
      btn.disabled = false;
      return;
    }}
    const ms = typeof j.latency_ms === "number" ? j.latency_ms : Math.round(performance.now() - t0);
    setProbeState(id, {{ status: "up", ms }});
  }} catch (e) {{
    const err = e.name === "AbortError" || e.name === "TimeoutError"
      ? "等待超时（约 70s）——上游排队过久或不可达"
      : String(e).slice(0, 300);
    setProbeState(id, {{ status: "down", ms: Math.round(performance.now() - t0), err }});
  }} finally {{
    clearInterval(tick);
    btn.disabled = false;
  }}
}}

async function probeAll(btn) {{
  btn.disabled = true;
  btn.textContent = "正在测速…";
  const buttons = Array.from(document.querySelectorAll("button.probe"));
  try {{
    for (const probeButton of buttons) {{
      if (probeButton.dataset.id) await probe(probeButton.dataset.i, probeButton.dataset.id);
    }}
  }} finally {{
    btn.disabled = false;
    btn.textContent = "一键测速全部模型";
  }}
}}

async function copyText(text, btn) {{
  try {{
    await navigator.clipboard.writeText(text);
  }} catch {{
    const ta = document.createElement("textarea");
    ta.value = text;
    document.body.appendChild(ta);
    ta.select();
    document.execCommand("copy");
    ta.remove();
  }}
  const old = btn.textContent;
  btn.textContent = "已复制 ✓";
  setTimeout(() => {{ btn.textContent = old; }}, 1200);
}}

document.querySelectorAll(".copy").forEach((btn) => {{
  btn.addEventListener("click", () => {{
    const el = $(btn.dataset.target);
    copyText(el.value || el.textContent, btn);
  }});
}});

window.addEventListener("error", (ev) => {{
  try {{
    const d = document.createElement("div");
    d.style.cssText = "background:#3b1220;color:#f87171;padding:10px;margin-top:12px;border-radius:8px;font:12px monospace;white-space:pre-wrap";
    d.textContent = "页面脚本错误: " + (ev.message || String(ev));
    (document.querySelector(".wrap") || document.body).appendChild(d);
  }} catch {{}}
}});
window.addEventListener("unhandledrejection", (ev) => {{
  try {{
    const d = document.createElement("div");
    d.style.cssText = "background:#3b1220;color:#f87171;padding:10px;margin-top:12px;border-radius:8px;font:12px monospace;white-space:pre-wrap";
    d.textContent = "未处理 Promise 失败: " + String(ev.reason);
    (document.querySelector(".wrap") || document.body).appendChild(d);
  }} catch {{}}
}});

refreshStatus();
refreshNetwork();
refreshAutostart();
refreshModels();
timers.push(setInterval(() => {{ refreshModels(); }}, 60000));

// ---- #5 SSE status stream (replaces polling for status + recent-requests) ----
let _sse = new EventSource("/v2/status-stream");
_sse.onmessage = (ev) => {{
  try {{
    const payload = JSON.parse(ev.data);
    if (payload.status) updateStatusFromSSE(payload.status);
    if (payload.recent) updateRecentFromSSE(payload.recent);
  }} catch {{}}
}};
function updateStatusFromSSE(s) {{
  const dot = $("probe-dot");
  dot.className = s.logged_in ? "dot ok" : "dot bad";
  $("login-state").textContent = s.logged_in ? "已登录 ✅" : "未登录 ⚠️";
  $("login-user").textContent = s.user || "—";
  $("token-left").textContent = s.expires_in_ms > 0 ? Math.round(s.expires_in_ms / 60000) + " 分钟" : "—";
  if (s.expires_in_ms > 0) startCountdown(s.expires_in_ms); else $("token-countdown").textContent = "";
  $("request-load").textContent = (s.active_requests || 0) + " 个处理中，" + (s.queued_requests || 0) + " 个排队（上限 " + (s.max_concurrent || "—") + "）";
  const btn = $("login-btn");
  if (btn) btn.textContent = s.logged_in ? "重新登录" : "登录";
}}
function updateRecentFromSSE(items) {{
  if (!items || items.length === 0) {{
    $("req-log-rows").innerHTML = '<tr><td colspan="6" style="color:var(--muted)">暂无请求记录</td></tr>';
    return;
  }}
  $("req-log-rows").innerHTML = items.map((it) => {{
    const time = new Date(it.ts).toLocaleTimeString("zh-CN", {{ hour12: false }});
    const statusCls = it.status === 200 ? "up" : "down";
    const tok = (it.prompt_tokens != null && it.completion_tokens != null)
      ? it.prompt_tokens + "/" + it.completion_tokens
      : "—";
    return '<tr>' +
      '<td>' + time + '</td>' +
      '<td>' + escapeHtml(it.protocol || "?") + '</td>' +
      '<td class="id">' + escapeHtml(it.model || "?") + '</td>' +
      '<td><span class="badge ' + statusCls + '">' + (it.status || "?") + '</span></td>' +
      '<td>' + (it.duration_ms != null ? it.duration_ms + "ms" : "—") + '</td>' +
      '<td>' + tok + '</td>' +
    '</tr>';
  }}).join("");
}}

// ---- #4 Theme toggle ----
function toggleTheme() {{
  const root = document.documentElement;
  const isLight = root.classList.toggle("light");
  $("theme-btn").textContent = isLight ? "☀️" : "🌙";
  try {{ localStorage.setItem("deveco-theme", isLight ? "light" : "dark"); }} catch {{}}
}}
try {{ if (localStorage.getItem("deveco-theme") === "light") toggleTheme(); }} catch {{}}

// ---- #8 Token expiry countdown ----
let _tokenExpiresMs = 0;
let _countdownTimer = null;
function startCountdown(expiresInMs) {{
  _tokenExpiresMs = Date.now() + expiresInMs;
  if (_countdownTimer) clearInterval(_countdownTimer);
  const update = () => {{
    const remain = _tokenExpiresMs - Date.now();
    const el = $("token-countdown");
    if (!el) return;
    if (remain <= 0) {{ el.textContent = ""; return; }}
    const m = Math.floor(remain / 60000);
    const s = Math.floor((remain % 60000) / 1000);
    el.textContent = "(倒计时 " + m + ":" + String(s).padStart(2, "0") + ")";
    el.className = "countdown" + (remain < 300000 ? " warn" : "");  // <5min → warn
  }};
  update();
  _countdownTimer = setInterval(update, 1000);
  timers.push(_countdownTimer);
}}

// ---- #6 Recent requests log ----
async function refreshRecentRequests() {{
  try {{
    const r = await fetchT("/v2/recent-requests");
    const j = await r.json();
    const items = j.items || [];
    if (items.length === 0) {{
      $("req-log-rows").innerHTML = '<tr><td colspan="6" style="color:var(--muted)">暂无请求记录</td></tr>';
      return;
    }}
    $("req-log-rows").innerHTML = items.map((it) => {{
      const time = new Date(it.ts).toLocaleTimeString("zh-CN", {{ hour12: false }});
      const statusCls = it.status === 200 ? "up" : "down";
      const tok = (it.prompt_tokens != null && it.completion_tokens != null)
        ? it.prompt_tokens + "/" + it.completion_tokens
        : "—";
      return '<tr>' +
        '<td>' + time + '</td>' +
        '<td>' + escapeHtml(it.protocol || "?") + '</td>' +
        '<td class="id">' + escapeHtml(it.model || "?") + '</td>' +
        '<td><span class="badge ' + statusCls + '">' + (it.status || "?") + '</span></td>' +
        '<td>' + (it.duration_ms != null ? it.duration_ms + "ms" : "—") + '</td>' +
        '<td>' + tok + '</td>' +
      '</tr>';
    }}).join("");
  }} catch (e) {{
    $("req-log-rows").innerHTML = '<tr><td colspan="6" style="color:var(--muted)">—</td></tr>';
  }}
}}
</script>
</body>
</html>"##)
}
