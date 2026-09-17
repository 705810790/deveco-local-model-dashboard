//! CLI 入口 — 从 deveco-local-model-dashboard/src/standalone.ts 移植。
//!
//! Flags:
//!   --port=<n>        proxy port (default 666 / $DEVECO_PROXY_PORT)
//!   --host=<addr>     bind address (default 127.0.0.1 / $DEVECO_PROXY_HOST)
//!   --no-dashboard    don't auto-open the browser

mod auth;
mod config;
mod dashboard;
mod lan;
mod models;
mod proxy;
mod transform;
mod win_autostart;

use std::net::SocketAddr;
use std::time::Duration;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "deveco-local-model-dashboard", version, about = "DevEco CodeGenie 本地模型代理")]
struct Args {
    /// Proxy port (default 666)
    #[arg(long)]
    port: Option<u16>,

    /// Bind address (default 127.0.0.1)
    #[arg(long)]
    host: Option<String>,

    /// Don't auto-open the browser
    #[arg(long)]
    no_dashboard: bool,
}

#[tokio::main]
async fn main() {
    config::init_log_level();
    init_tracing();

    let args = Args::parse();

    // Resolve port (CLI > env > default 666)
    let port = args
        .port
        .or_else(|| std::env::var("DEVECO_PROXY_PORT").ok().and_then(|s| s.parse().ok()))
        .unwrap_or(666);

    // Resolve host (CLI > env > default 127.0.0.1)
    let hostname = args
        .host
        .or_else(|| std::env::var("DEVECO_PROXY_HOST").ok())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    let dashboard_url = format!("http://127.0.0.1:{port}/v2/dashboard");

    // Build proxy state + router
    let state = proxy::server::ProxyState::new(port, hostname.clone(), true);

    // Try to restore an existing session from stored jwtToken (best-effort).
    state.try_restore_session().await;

    // Get rebind receiver before state is moved into router
    let mut rebind_rx = state.rebind_receiver();

    let router = proxy::server::build_router(state);

    // Bind
    let addr: SocketAddr = match hostname.parse() {
        Ok(a) => SocketAddr::new(a, port),
        Err(_) => {
            config::log_error(&format!("invalid host: {hostname}"));
            std::process::exit(1);
        }
    };

    let mut listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            config::log_error(&format!("端口 {port} 已被占用：代理可能已经在运行。"));
            config::log_info(&format!("正在打开现有服务的面板：{dashboard_url}"));
            if !args.no_dashboard {
                let _ = open::that(&dashboard_url);
            }
            wait_enter("按回车键退出…");
            std::process::exit(0);
        }
        Err(e) => {
            config::log_error(&format!("代理启动失败: {e}"));
            wait_enter("按回车键退出…");
            std::process::exit(1);
        }
    };

    config::log_info(&format!("面板地址：{dashboard_url}"));
    config::log_info(&format!("监听：http://{hostname}:{port}"));

    if !args.no_dashboard {
        let _ = open::that(&dashboard_url);
    }

    // Serve with rebind support: when /v2/network toggles LAN, we abort the
    // current serve task, rebind to the new host, and restart.

    loop {
        let serve_handle = tokio::spawn({
            let router = router.clone();
            // Move listener out; we'll replace it on rebind
            let l = std::mem::replace(
                &mut listener,
                tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
            );
            async move {
                axum::serve(
                    l,
                    router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
            }
        });

        // Wait for: ctrl_c (exit) or rebind signal (rebind)
        tokio::select! {
            _ = shutdown_signal() => {
                serve_handle.abort();
                let _ = serve_handle.await;
                break;
            }
            changed = rebind_rx.changed() => {
                if changed.is_err() {
                    serve_handle.abort();
                    let _ = serve_handle.await;
                    break;
                }
                let rebind_host = rebind_rx.borrow().clone();
                // Stop the current server
                serve_handle.abort();
                let _ = serve_handle.await;

                if let Some(new_host) = rebind_host {
                    config::log_info(&format!("rebinding to {new_host}:{port}"));
                    let new_addr: SocketAddr = match new_host.parse() {
                        Ok(a) => SocketAddr::new(a, port),
                        Err(_) => continue,
                    };
                    match tokio::net::TcpListener::bind(new_addr).await {
                        Ok(l) => {
                            listener = l;
                            config::log_info(&format!("rebound: http://{new_host}:{port}"));
                        }
                        Err(e) => {
                            config::log_error(&format!("rebind failed: {e}, falling back to 127.0.0.1"));
                            let fallback = SocketAddr::new("127.0.0.1".parse().unwrap(), port);
                            match tokio::net::TcpListener::bind(fallback).await {
                                Ok(l) => listener = l,
                                Err(_) => break,
                            }
                        }
                    }
                } else {
                    break;
                }
            }
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        let _ = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .await
            .map(|mut s| s.recv().await);
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

/// Pause so a double-clicked console window doesn't vanish with the error.
fn wait_enter(prompt: &str) {
    use std::io::{self, BufRead, Write};
    print!("\n{prompt}");
    let _ = io::stdout().flush();
    let _ = io::stdin().lock().read_line(&mut String::new());
}

/// Initialise tracing with dual output: console (stdout/stderr) + daily-rotated
/// log file at `~/.atomcode-deveco/logs/`.  The file writer is non-blocking
/// (runs on a dedicated thread via `NonBlocking`), so log calls never stall
/// the proxy hot path.
fn init_tracing() {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let level = std::env::var("DEVECO_LOG_LEVEL")
        .or_else(|_| std::env::var("LOG_LEVEL"))
        .unwrap_or_else(|_| "info".to_string());
    let filter = EnvFilter::try_new(&level).unwrap_or_else(|_| EnvFilter::new("info"));

    // Log directory: ~/.atomcode-deveco/logs/
    let log_dir = {
        let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        home.join(".atomcode-deveco").join("logs")
    };
    let _ = std::fs::create_dir_all(&log_dir);

    // Daily rotation, file name prefix "proxy" → proxy.YYYY-MM-DD
    let file_appender = tracing_appender::rolling::daily(&log_dir, "proxy.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    // Keep the guard alive for the entire process lifetime.
    std::mem::forget(_guard);

    let console_layer = fmt::layer().with_target(false);
    let file_layer = fmt::layer()
        .with_writer(non_blocking)
        .with_target(false)
        .with_ansi(false); // no colour codes in log files

    tracing_subscriber::registry()
        .with(filter)
        .with(console_layer)
        .with(file_layer)
        .init();
}
