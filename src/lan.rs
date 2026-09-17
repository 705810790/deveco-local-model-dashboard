//! LAN reachability diagnostics. From deveco-local-model-dashboard/src/lan-status.ts 移植。
//!
//! 三部分检查：
//! 1. 代理监听 0.0.0.0 + 有可用 LAN IPv4（虚拟网卡过滤）
//! 2. TCP 自连验证端口监听
//! 3. Windows 防火墙入站规则查询

use serde::Serialize;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time;

/// Firewall rule name used by the one-click fix.
pub const FIREWALL_RULE_NAME: &str = "deveco-local-model-dashboard-proxy";

// ---------------------------------------------------------------------------
// Result types (match TS LanCheckResult / LanInterfaceStatus / FirewallStatus)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct LanIpCandidate {
    pub name: String,
    pub address: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TcpProbeResult {
    pub ok: bool,
    pub latency_ms: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FirewallStatus {
    pub checked: bool,
    pub allowed: bool,
    pub rules: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LanInterfaceStatus {
    pub name: String,
    pub address: String,
    pub url: String,
    pub reachable: bool,
    pub latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LanCheckResult {
    pub verdict: String,
    pub lan_open: bool,
    pub port: u16,
    pub interfaces: Vec<LanInterfaceStatus>,
    pub firewall: FirewallStatus,
    pub hint: String,
}

/// 防火墙修复结果。
#[derive(Debug, Clone, Serialize)]
pub struct FirewallFixResult {
    pub ok: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

// ---------------------------------------------------------------------------
// LAN IPv4 enumeration (with virtual adapter filtering)
// ---------------------------------------------------------------------------

/// Virtual/loopback-ish adapter fragments to filter out.
/// Names like "vEthernet (以太网)" are real LAN NICs bridged through Hyper-V
/// and must be KEPT; the ones listed here are purely virtual switches.
const VIRTUAL_ADAPTER_RE: &[&str] = &[
    "vmware", "vmnet", "virtualbox", "loopback", "bluetooth", "蓝牙",
    "tap-adapter", "default switch", "wsl",
];

fn is_virtual_adapter(name: &str) -> bool {
    let lower = name.to_lowercase();
    VIRTUAL_ADAPTER_RE.iter().any(|frag| lower.contains(frag))
}

/// LAN IPv4 candidates: non-internal, non-APIPA, non-virtual adapters.
/// Returns {name, address} pairs (TS lanIpv4s).
pub fn lan_ipv4s() -> Vec<LanIpCandidate> {
    let mut result = Vec::new();

    // Windows: parse ipconfig output to get adapter names + IPs
    // 中文 Windows 的 ipconfig 输出用 GBK 编码，需要用 encoding_rs 解码。
    #[cfg(windows)]
    {
        if let Ok(o) = std::process::Command::new("ipconfig").output() {
            // 尝试 GBK 解码（中文 Windows 默认代码页），失败则回退 UTF-8
            let (s, _, had_errors) = encoding_rs::GBK.decode(&o.stdout);
            let s = if had_errors {
                String::from_utf8_lossy(&o.stdout).to_string()
            } else {
                s.into_owned()
            };
            parse_ipconfig(&s, &mut result);
        }
    }

    // Linux/macOS: use UDP socket trick + platform commands
    #[cfg(not(windows))]
    {
        // UDP socket trick for default route IP
        if let Some(ip) = default_route_ipv4() {
            push_unique(&mut result, "default", &ip);
        }
        // Parse `ip addr` or `ifconfig`
        for (name, ip) in platform_enum_ipv4s() {
            push_unique(&mut result, &name, &ip);
        }
    }

    result
}

#[cfg(windows)]
fn parse_ipconfig(output: &str, result: &mut Vec<LanIpCandidate>) {
    let mut current_name: Option<String> = None;
    for line in output.lines() {
        let trimmed = line.trim();
        // Adapter name lines end with ':' and don't contain "IPv4"
        if trimmed.ends_with(':') && !trimmed.contains("IPv4") && !trimmed.contains("IPv4 地址") {
            // Strip trailing ':'
            let name = trimmed.trim_end_matches(':').trim();
            if !name.is_empty() {
                current_name = Some(name.to_string());
            }
        }
        // IPv4 address lines
        if let Some(ip) = extract_ipv4_after_colon(trimmed) {
            if is_valid_lan_ipv4(&ip) {
                if let Some(ref name) = current_name {
                    if !is_virtual_adapter(name) {
                        push_unique(result, name, &ip);
                    }
                }
            }
        }
    }
}

#[cfg(not(windows))]
fn default_route_ipv4() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    let local = sock.local_addr().ok()?;
    let ip = local.ip();
    if ip.is_loopback() || ip.is_unspecified() {
        None
    } else {
        Some(ip.to_string())
    }
}

#[cfg(not(windows))]
fn platform_enum_ipv4s() -> Vec<(String, String)> {
    let mut out = Vec::new();
    // Try `ip addr` first (Linux), fall back to `ifconfig` (macOS)
    if let Ok(o) = std::process::Command::new("ip").args(["addr"]).output() {
        if o.status.success() {
            let s = String::from_utf8_lossy(&o.stdout);
            for line in s.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("inet ") {
                    if let Some(ip) = trimmed.split_whitespace().nth(1) {
                        let ip = ip.split('/').next().unwrap_or("");
                        if is_valid_lan_ipv4(ip) {
                            out.push(("iface".to_string(), ip.to_string()));
                        }
                    }
                }
            }
            return out;
        }
    }
    // Fall back to ifconfig
    if let Ok(o) = std::process::Command::new("ifconfig").output() {
        let s = String::from_utf8_lossy(&o.stdout);
        for line in s.lines() {
            let trimmed = line.trim();
            if let Some(ip) = extract_ipv4_after_colon(trimmed) {
                if is_valid_lan_ipv4(&ip) {
                    out.push(("iface".to_string(), ip));
                }
            }
        }
    }
    out
}

fn extract_ipv4_after_colon(line: &str) -> Option<String> {
    // Lines like "IPv4 Address. . . . . . . . . . . : 192.168.1.10"
    // or "IPv4 地址 . . . . . . . . . . . . : 192.168.1.10"
    // or "inet addr:192.168.1.10"
    if let Some(colon_idx) = line.rfind(':') {
        let after = line[colon_idx + 1..].trim();
        let parts: Vec<&str> = after.split_whitespace().collect();
        for part in parts {
            if is_valid_ipv4_format(part) {
                return Some(part.to_string());
            }
        }
    }
    None
}

fn is_valid_ipv4_format(s: &str) -> bool {
    s.split('.').count() == 4 && s.split('.').all(|p| p.parse::<u8>().is_ok())
}

fn is_valid_lan_ipv4(s: &str) -> bool {
    if !is_valid_ipv4_format(s) {
        return false;
    }
    if s.starts_with("127.") {
        return false;
    }
    if s.starts_with("169.254.") {
        return false;
    }
    if s == "0.0.0.0" {
        return false;
    }
    true
}

fn push_unique(v: &mut Vec<LanIpCandidate>, name: &str, address: &str) {
    if address.is_empty() {
        return;
    }
    if !v.iter().any(|c| c.address == address) {
        v.push(LanIpCandidate {
            name: name.to_string(),
            address: address.to_string(),
        });
    }
}

// ---------------------------------------------------------------------------
// TCP self-connect test
// ---------------------------------------------------------------------------

pub async fn tcp_self_connect(ip: &str, port: u16, timeout_ms: u64) -> TcpProbeResult {
    let addr = format!("{}:{}", ip, port);
    let start = std::time::Instant::now();

    let connect = TcpStream::connect(&addr);
    let timeout = time::timeout(Duration::from_millis(timeout_ms.max(1)), connect);

    match timeout.await {
        Ok(Ok(_stream)) => TcpProbeResult {
            ok: true,
            latency_ms: Some(start.elapsed().as_millis() as u64),
            error: None,
        },
        Ok(Err(e)) => TcpProbeResult {
            ok: false,
            latency_ms: None,
            error: Some(e.to_string()),
        },
        Err(_) => TcpProbeResult {
            ok: false,
            latency_ms: None,
            error: Some(format!("连接超时（{}ms）", timeout_ms)),
        },
    }
}

// ---------------------------------------------------------------------------
// Firewall check (Windows only)
// ---------------------------------------------------------------------------

pub fn check_firewall_rule(port: u16) -> FirewallStatus {
    #[cfg(windows)]
    {
        let script = format!(
            "$ErrorActionPreference='Stop'\n\
             $f = Get-NetFirewallPortFilter -Protocol TCP | Where-Object {{ $_.LocalPort -eq {port} }}\n\
             $rules = @()\n\
             if ($f) {{ $rules = $f | Get-NetFirewallRule -ErrorAction SilentlyContinue | Where-Object {{ $_.Direction -eq 'Inbound' -and $_.Action -eq 'Allow' -and $_.Enabled -eq 'True' }} }}\n\
             if ($rules) {{ Write-Output ('ALLOWED;;' + (($rules | Select-Object -ExpandProperty DisplayName) -join ';;')) }} else {{ Write-Output 'NOTFOUND;;' }}",
            port = port,
        );

        match std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
            .output()
        {
            Ok(o) if o.status.success() => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                parse_firewall_check_output(&stdout)
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr).to_string();
                FirewallStatus {
                    checked: false,
                    allowed: false,
                    rules: vec![],
                    error: Some(stderr),
                }
            }
            Err(e) => FirewallStatus {
                checked: false,
                allowed: false,
                rules: vec![],
                error: Some(e.to_string()),
            },
        }
    }
    #[cfg(not(windows))]
    {
        let _ = port;
        FirewallStatus {
            checked: false,
            allowed: false,
            rules: vec![],
            error: Some("防火墙规则检测仅支持 Windows".to_string()),
        }
    }
}

fn parse_firewall_check_output(stdout: &str) -> FirewallStatus {
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("ALLOWED;;") {
            let rules: Vec<String> = trimmed["ALLOWED;;".len()..]
                .split(";;")
                .map(|r| r.trim().to_string())
                .filter(|r| !r.is_empty())
                .collect();
            return FirewallStatus {
                checked: true,
                allowed: true,
                rules,
                error: None,
            };
        }
        if trimmed.starts_with("NOTFOUND;;") {
            return FirewallStatus {
                checked: true,
                allowed: false,
                rules: vec![],
                error: None,
            };
        }
    }
    FirewallStatus {
        checked: false,
        allowed: false,
        rules: vec![],
        error: Some("无法解析防火墙查询输出".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Firewall fix (Windows only, UAC elevated)
// ---------------------------------------------------------------------------

pub async fn request_firewall_fix(port: u16) -> FirewallFixResult {
    #[cfg(windows)]
    {
        let script = format!(
            "$ErrorActionPreference='Stop'\n\
             Start-Process -FilePath 'netsh.exe' -ArgumentList 'advfirewall','firewall','add','rule','name={rule}','dir=in','action=allow','protocol=TCP','localport={port}' -Verb RunAs -WindowStyle Hidden\n\
             Write-Output 'LAUNCHED;;'",
            rule = FIREWALL_RULE_NAME,
            port = port,
        );

        match std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
            .output()
        {
            Ok(o) if o.status.success() => FirewallFixResult {
                ok: true,
                message: "已弹出 UAC 提权窗口，请在弹窗中确认".to_string(),
                command: Some(script),
            },
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr).to_string();
                FirewallFixResult {
                    ok: false,
                    message: format!("防火墙规则添加失败：{}", stderr.trim()),
                    command: Some(script),
                }
            }
            Err(e) => FirewallFixResult {
                ok: false,
                message: format!("无法启动 PowerShell：{}", e),
                command: None,
            },
        }
    }
    #[cfg(not(windows))]
    {
        let _ = port;
        FirewallFixResult {
            ok: false,
            message: "一键放行防火墙仅支持 Windows".to_string(),
            command: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Aggregate LAN check
// ---------------------------------------------------------------------------

/// Aggregate the three checks into one verdict + hint. Matches TS lanCheck().
pub async fn lan_check(port: u16, lan_open: bool) -> LanCheckResult {
    if !lan_open {
        return LanCheckResult {
            verdict: "loopback_only".to_string(),
            lan_open: false,
            port,
            interfaces: vec![],
            firewall: FirewallStatus { checked: false, allowed: false, rules: vec![], error: None },
            hint: "当前代理仅监听本机（127.0.0.1）。请先开启「局域网访问」，再进行外部可达性检测。".to_string(),
        };
    }

    let candidates = lan_ipv4s();
    if candidates.is_empty() {
        return LanCheckResult {
            verdict: "no_lan_ip".to_string(),
            lan_open: true,
            port,
            interfaces: vec![],
            firewall: FirewallStatus { checked: false, allowed: false, rules: vec![], error: None },
            hint: "未检测到可用的局域网 IPv4 地址。请确认电脑已连接路由器（有线或 Wi-Fi）后重试。".to_string(),
        };
    }

    // Probe each IP (concurrent)
    let mut handles = Vec::with_capacity(candidates.len());
    for c in &candidates {
        let addr = c.address.clone();
        let name = c.name.clone();
        handles.push(tokio::spawn(async move {
            let probe = tcp_self_connect(&addr, port, 3_000).await;
            (name, addr, probe)
        }));
    }

    let mut interfaces = Vec::with_capacity(handles.len());
    for h in handles {
        match h.await {
            Ok((name, addr, probe)) => interfaces.push(LanInterfaceStatus {
                name,
                address: addr.clone(),
                url: format!("http://{}:{}/v2", addr, port),
                reachable: probe.ok,
                latency_ms: probe.latency_ms,
                error: probe.error,
            }),
            Err(e) => interfaces.push(LanInterfaceStatus {
                name: "unknown".to_string(),
                address: String::new(),
                url: String::new(),
                reachable: false,
                latency_ms: None,
                error: Some(format!("任务异常: {}", e)),
            }),
        }
    }

    let any_reachable = interfaces.iter().any(|i| i.reachable);
    let firewall = check_firewall_rule(port);

    if !any_reachable {
        return LanCheckResult {
            verdict: "unreachable".to_string(),
            lan_open: true,
            port,
            interfaces,
            firewall,
            hint: format!(
                "服务未在局域网地址上完成监听（本机自连全部失败）。请确认代理仍在运行、端口 {port} 未被其他程序抢占。"
            ),
        };
    }

    if firewall.checked && !firewall.allowed {
        return LanCheckResult {
            verdict: "firewall_blocked".to_string(),
            lan_open: true,
            port,
            interfaces,
            firewall,
            hint: format!(
                "服务正在监听局域网地址，但 Windows 防火墙未放行该端口。点击「一键放行防火墙」并在 UAC 弹窗中确认；或让管理员执行：netsh advfirewall firewall add rule name=\"{rule}\" dir=in action=allow protocol=TCP localport={port}",
                rule = FIREWALL_RULE_NAME, port = port
            ),
        };
    }

    if !firewall.checked {
        let fw_error = firewall.error.clone();
        return LanCheckResult {
            verdict: "ok".to_string(),
            lan_open: true,
            port,
            interfaces,
            firewall,
            hint: format!(
                "监听正常；防火墙规则检测未完成（{}），外部设备能否访问请以实际连接为准。",
                fw_error.as_deref().unwrap_or("未知原因")
            ),
        };
    }

    LanCheckResult {
        verdict: "ok".to_string(),
        lan_open: true,
        port,
        interfaces,
        firewall,
        hint: "监听与防火墙放行均正常，局域网内其他设备应可通过上方地址访问。".to_string(),
    }
}
