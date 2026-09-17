//! Windows "start on boot" support via the per-user HKCU Run key.
//!
//! From deveco-local-model-dashboard/src/win-autostart.ts 移植。
//!
//! Why a Run key and not schtasks: `schtasks /sc onlogon` requires admin
//! rights, while an HKCU\...\Run entry works per-user with no elevation. The
//! registered command wraps the program in `cmd /c start "" /min` so logon
//! starts the proxy with a minimized console window and the wrapper exits
//! immediately.

use serde::Serialize;

/// The Run key value name this module owns.
pub const AUTOSTART_VALUE_NAME: &str = "deveco-local-model-dashboard-proxy";

/// The HKCU Run key path.
pub const AUTOSTART_KEY_PATH: &str = "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run";

#[derive(Debug, Clone, Serialize)]
pub struct AutostartStatus {
    pub supported: bool,
    pub enabled: bool,
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Whether autostart is supported on this platform.
pub fn autostart_supported() -> bool {
    cfg!(windows) || cfg!(target_os = "linux") || cfg!(target_os = "macos")
}

/// Build the command string stored in the Run key:
/// `cmd /c start "" /min "<exec>" <args> --no-dashboard`
pub fn build_autostart_command(exec_path: &str, args: &[String]) -> String {
    let mut tokens: Vec<String> = Vec::new();

    let push = |tokens: &mut Vec<String>, raw: &str| {
        if raw.is_empty() {
            return;
        }
        if raw == "--no-dashboard" || raw == "--dashboard" {
            return;
        }
        if raw.starts_with("--") {
            tokens.push(raw.to_string());
            return;
        }
        // Strip surrounding quotes, resolve to absolute path
        let unquoted = raw.trim_matches('"');
        let abs = if std::path::Path::new(unquoted).is_absolute() {
            unquoted.to_string()
        } else {
            std::fs::canonicalize(unquoted)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| unquoted.to_string())
        };
        tokens.push(format!("\"{abs}\""));
    };

    push(&mut tokens, exec_path);
    for a in args {
        push(&mut tokens, a);
    }
    tokens.push("--no-dashboard".to_string());
    format!("cmd /c start \"\" /min {}", tokens.join(" "))
}

#[cfg(windows)]
pub fn get_autostart_status() -> AutostartStatus {
    use std::process::Command;

    let output = Command::new("reg")
        .args(["query", AUTOSTART_KEY_PATH, "/v", AUTOSTART_VALUE_NAME])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let command = parse_reg_query_output(&stdout);
            AutostartStatus {
                supported: true,
                enabled: command.is_some(),
                command,
                error: None,
            }
        }
        _ => AutostartStatus {
            supported: true,
            enabled: false,
            command: None,
            error: None,
        },
    }
}

#[cfg(target_os = "linux")]
pub fn get_autostart_status() -> AutostartStatus {
    let unit_path = systemd_unit_path();
    let exists = unit_path.exists();
    // Check if systemd knows about it (enabled)
    let enabled = if exists {
        std::process::Command::new("systemctl")
            .args(["--user", "is-enabled", &systemd_unit_name()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    } else {
        false
    };
    let command = if exists {
        std::fs::read_to_string(&unit_path).ok()
    } else {
        None
    };
    AutostartStatus {
        supported: true,
        enabled,
        command,
    }
}

#[cfg(target_os = "macos")]
pub fn get_autostart_status() -> AutostartStatus {
    let plist_path = launchd_plist_path();
    let exists = plist_path.exists();
    let command = if exists {
        std::fs::read_to_string(&plist_path).ok()
    } else {
        None
    };
    AutostartStatus {
        supported: true,
        enabled: exists,
        command,
    }
}

/// Extract the value data from `reg query` output, or None when absent.
pub fn parse_reg_query_output(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with(AUTOSTART_VALUE_NAME) {
            continue;
        }
        let rest = trimmed[AUTOSTART_VALUE_NAME.len()..].trim();
        // Format: "VALUE_NAME    REG_SZ    <data>"
        if let Some(pos) = rest.find("REG_") {
            // Skip the type token (e.g. "REG_SZ")
            let after_type = &rest[pos..];
            if let Some(space) = after_type.find(char::is_whitespace) {
                let data = after_type[space..].trim();
                if !data.is_empty() {
                    return Some(data.to_string());
                }
            }
        }
        return None;
    }
    None
}

#[cfg(windows)]
pub fn set_autostart(enabled: bool, command: &str) -> AutostartStatus {
    use std::process::Command;

    if !enabled {
        // Probe first: reg delete on a missing value exits 1 with a localized
        // message. Disabling an already-absent entry is a success.
        let current = get_autostart_status();
        if !current.enabled {
            return AutostartStatus {
                supported: true,
                enabled: false,
                command: None,
                error: None,
            };
        }
        let _ = Command::new("reg")
            .args(["delete", AUTOSTART_KEY_PATH, "/v", AUTOSTART_VALUE_NAME, "/f"])
            .output();
    } else {
        let _ = Command::new("reg")
            .args([
                "add",
                AUTOSTART_KEY_PATH,
                "/v",
                AUTOSTART_VALUE_NAME,
                "/t",
                "REG_SZ",
                "/d",
                command,
                "/f",
            ])
            .output();
    }

    let status = get_autostart_status();
    if status.enabled != enabled {
        return AutostartStatus {
            supported: true,
            enabled: status.enabled,
            command: status.command,
            error: Some("注册表写入后状态与预期不一致".to_string()),
        };
    }
    status
}

#[cfg(target_os = "linux")]
pub fn set_autostart(enabled: bool, command: &str) -> AutostartStatus {
    let unit_path = systemd_unit_path();
    let unit_name = systemd_unit_name();

    if enabled {
        // `command` is the exec path + args (same as Windows build_autostart_command output,
        // but on Linux we use it directly as ExecStart=).
        let exec = std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| command.to_string());
        let unit_content = format!(
            "[Unit]\n\
             Description=DevEco CodeGenie Local Model Proxy\n\
             After=network.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={exec} --no-dashboard\n\
             Restart=on-failure\n\
             RestartSec=5\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n"
        );
        if let Some(parent) = unit_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&unit_path, &unit_content) {
            return AutostartStatus {
                supported: true,
                enabled: false,
                command: None,
                error: Some(format!("写入 systemd unit 失败: {e}")),
            };
        }
        // Reload + enable
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "enable", &unit_name])
            .output();
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "start", &unit_name])
            .output();
    } else {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "stop", &unit_name])
            .output();
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "disable", &unit_name])
            .output();
        let _ = std::fs::remove_file(&unit_path);
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
    }

    get_autostart_status()
}

#[cfg(target_os = "macos")]
pub fn set_autostart(enabled: bool, command: &str) -> AutostartStatus {
    let plist_path = launchd_plist_path();

    if enabled {
        let exec = std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| command.to_string());
        let plist_content = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
               <key>Label</key>\n\
               <string>{label}</string>\n\
               <key>ProgramArguments</key>\n\
               <array>\n\
                 <string>{exec}</string>\n\
                 <string>--no-dashboard</string>\n\
               </array>\n\
               <key>RunAtLoad</key>\n\
               <true/>\n\
               <key>KeepAlive</key>\n\
               <false/>\n\
             </dict>\n\
             </plist>\n",
            label = AUTOSTART_VALUE_NAME,
            exec = exec
        );
        if let Some(parent) = plist_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&plist_path, &plist_content) {
            return AutostartStatus {
                supported: true,
                enabled: false,
                command: None,
                error: Some(format!("写入 launchd plist 失败: {e}")),
            };
        }
        let _ = std::process::Command::new("launchctl")
            .args(["load", &plist_path.to_string_lossy()])
            .output();
    } else {
        let _ = std::process::Command::new("launchctl")
            .args(["unload", &plist_path.to_string_lossy()])
            .output();
        let _ = std::fs::remove_file(&plist_path);
    }

    get_autostart_status()
}

// ---------------------------------------------------------------------------
// Linux systemd helpers
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn systemd_unit_name() -> String {
    format!("{AUTOSTART_VALUE_NAME}.service")
}

#[cfg(target_os = "linux")]
fn systemd_unit_path() -> std::path::PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join(".config")
        .join("systemd")
        .join("user")
        .join(systemd_unit_name())
}

// ---------------------------------------------------------------------------
// macOS launchd helpers
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn launchd_plist_path() -> std::path::PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{AUTOSTART_VALUE_NAME}.plist"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reg_query_finds_value() {
        let stdout = "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\r\n    deveco-local-model-dashboard-proxy    REG_SZ    cmd /c start \"\" /min \"C:\\proxy.exe\" --no-dashboard\r\n";
        let cmd = parse_reg_query_output(stdout);
        assert!(cmd.is_some());
        assert!(cmd.unwrap().contains("proxy.exe"));
    }

    #[test]
    fn parse_reg_query_missing_returns_none() {
        let stdout = "ERROR: The system was unable to find the specified registry key or value.";
        assert!(parse_reg_query_output(stdout).is_none());
    }

    #[test]
    fn build_command_includes_no_dashboard() {
        let cmd = build_autostart_command("C:\\proxy.exe", &["--port=666".to_string()]);
        assert!(cmd.contains("--no-dashboard"));
        assert!(cmd.starts_with("cmd /c start"));
    }
}
