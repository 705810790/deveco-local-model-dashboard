//! 全局常量、URL、超时配置。
//!
//! 从 deveco-local-model-dashboard/src/config.ts 移植，保留全部常量和默认值。

use std::time::Duration;

// ---------------------------------------------------------------------------
// Provider identity
// ---------------------------------------------------------------------------

pub const PROVIDER_ID: &str = "deveco";
pub const OAUTH_DUMMY_KEY: &str = "opencode-oauth-dummy-key";

// ---------------------------------------------------------------------------
// DevEco endpoints
// ---------------------------------------------------------------------------

pub const DEVECO_BASE_URL: &str = "https://cn.devecostudio.huawei.com";

/// OpenAI-compatible Chat Completions base (maas HTTP channel).
pub const DEVECO_API_BASE: &str = "https://cn.devecostudio.huawei.com/sse/codeGenie/maas/v2";

/// Models that need the zhipu-style `thinking` toggle explicitly enabled.
pub const THINKING_ENABLED_MODELS: &[&str] = &["deepseek-v4-flash"];

/// Dynamic model list endpoint (Bearer auth).
pub const DEVECO_MODEL_CONFIG_URL: &str =
    "https://cn.devecostudio.huawei.com/codeGenie/modelConfig";

/// Releases the server-side queue slot a turn holds.
pub const DEVECO_EXIT_QUEUE_URL: &str =
    "https://cn.devecostudio.huawei.com/sse/codeGenie/exitSessionQueue";

/// Vision fallback for text-only models.
pub const DEVECO_VISION_FALLBACK_MODEL: &str = "Qwen3_VL_235B_A22B_Instruct";

/// Models that the proxy treats as text-only for the vision fallback.
pub const DEVECO_TEXT_ONLY_MODELS: &[&str] = &["GLM-5.1"];

// ---------------------------------------------------------------------------
// Timeouts (as Durations)
// ---------------------------------------------------------------------------

/// How long the upstream may stay *silent* before we give up (120s).
pub const UPSTREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// SSE keepalive cadence while the upstream is queueing (10s).
pub const SSE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// Budget for the dashboard's "测试在线" maas probe (60s).
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// accessToken lifetime (30 min).
pub const ACCESS_TOKEN_EXPIRES: Duration = Duration::from_secs(30 * 60);

/// Callback server port candidates.
pub const CALLBACK_PORTS: &[u16] = &[10101, 34567, 34568, 34569, 34570];

/// Huawei app id used by the login page.
pub const DEVECO_APP_ID: &str = "1008";

/// Plugin version (for modelConfig request).
pub const PLUGIN_VERSION: &str = "1.0.0";

// ---------------------------------------------------------------------------
// Login flow config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct LoginConfig {
    pub base_url: String,
    pub auth_url: String,
    pub temp_token_check_url: String,
    pub jwt_token_check_url: String,
    pub success_redirect_url: String,
    pub failed_redirect_url: String,
    pub app_id: String,
    pub default_port: u16,
    pub timeout: Duration,
}

impl Default for LoginConfig {
    fn default() -> Self {
        Self {
            base_url: DEVECO_BASE_URL.to_string(),
            auth_url: "console/DevEcoIDE/apply".to_string(),
            temp_token_check_url: "authrouter/auth/api/temptoken/check".to_string(),
            jwt_token_check_url: "authrouter/auth/api/jwToken/check".to_string(),
            success_redirect_url: "console/DevEcoCode/loginSuccess".to_string(),
            failed_redirect_url: "console/DevEcoCode/loginFailed".to_string(),
            app_id: DEVECO_APP_ID.to_string(),
            default_port: CALLBACK_PORTS[0],
            timeout: Duration::from_secs(600), // 10 min
        }
    }
}

// ---------------------------------------------------------------------------
// Model info types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct ModelLimit {
    pub context: Option<u64>,
    pub input: Option<u64>,
    pub output: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct ModelModalities {
    pub input: Option<Vec<String>>,
    pub output: Option<Vec<String>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct ModelInfo {
    pub name: Option<String>,
    pub reasoning: Option<bool>,
    pub tool_call: Option<bool>,
    pub attachment: Option<bool>,
    pub temperature: Option<bool>,
    pub limit: Option<ModelLimit>,
    pub modalities: Option<ModelModalities>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct ProviderInfo {
    pub name: Option<String>,
    pub npm: Option<String>,
    pub api: Option<String>,
    pub env: Option<Vec<String>>,
    pub id: Option<String>,
    pub whitelist: Option<Vec<String>>,
    pub blacklist: Option<Vec<String>>,
    pub models: Option<std::collections::BTreeMap<String, ModelInfo>>,
}

/// Static provider fields reused when building a config from dynamic models.
pub fn static_provider_fields() -> ProviderInfo {
    ProviderInfo {
        name: Some("DevEco Code".to_string()),
        npm: Some("@ai-sdk/openai-compatible".to_string()),
        api: Some(DEVECO_API_BASE.to_string()),
        env: Some(vec![]),
        ..Default::default()
    }
}

/// Extra maas models the API may not list but the proxy serves.
pub fn extra_maas_models() -> std::collections::BTreeMap<String, ModelInfo> {
    let mut m = std::collections::BTreeMap::new();
    m.insert(
        "glm-5.2".to_string(),
        ModelInfo {
            name: Some("glm-5.2".to_string()),
            reasoning: Some(true),
            tool_call: Some(true),
            limit: Some(ModelLimit {
                context: Some(1_048_576),
                output: Some(131_072),
                ..Default::default()
            }),
            modalities: Some(ModelModalities {
                input: Some(vec!["text".to_string()]),
                output: Some(vec!["text".to_string()]),
            }),
            ..Default::default()
        },
    );
    m.insert(
        "deepseek-v4-flash".to_string(),
        ModelInfo {
            name: Some("deepseek-v4-flash".to_string()),
            reasoning: Some(true),
            tool_call: Some(true),
            limit: Some(ModelLimit {
                context: Some(1_000_000),
                output: Some(393_216),
                ..Default::default()
            }),
            modalities: Some(ModelModalities {
                input: Some(vec!["text".to_string()]),
                output: Some(vec!["text".to_string()]),
            }),
            ..Default::default()
        },
    );
    m
}

/// Default provider config (fallback when API is unreachable).
pub fn deveco_defaults() -> ProviderInfo {
    let mut models = std::collections::BTreeMap::new();
    models.insert(
        "GLM-5.1".to_string(),
        ModelInfo {
            name: Some("GLM-5.1".to_string()),
            reasoning: Some(true),
            tool_call: Some(true),
            limit: Some(ModelLimit {
                context: Some(202_752),
                output: Some(131_072),
                ..Default::default()
            }),
            modalities: Some(ModelModalities {
                input: Some(vec!["text".to_string()]),
                output: Some(vec!["text".to_string()]),
            }),
            ..Default::default()
        },
    );
    models.insert(
        "Qwen2.5-VL-72B".to_string(),
        ModelInfo {
            name: Some("Qwen2.5-VL-72B".to_string()),
            limit: Some(ModelLimit {
                context: Some(32_768),
                output: Some(8_192),
                ..Default::default()
            }),
            modalities: Some(ModelModalities {
                input: Some(vec!["text".to_string(), "image".to_string()]),
                output: Some(vec!["text".to_string()]),
            }),
            ..Default::default()
        },
    );
    // Merge extra maas models
    for (id, info) in extra_maas_models() {
        models.insert(id, info);
    }
    ProviderInfo {
        name: Some("DevEco Code".to_string()),
        npm: Some("@ai-sdk/openai-compatible".to_string()),
        api: Some(DEVECO_API_BASE.to_string()),
        env: Some(vec![]),
        models: Some(models),
        ..Default::default()
    }
}

/// Default task→model map.
pub fn default_task_model_map() -> std::collections::BTreeMap<String, String> {
    let mut m = std::collections::BTreeMap::new();
    m.insert("small_model".to_string(), "GLM-5.1".to_string());
    m.insert("ui_verification".to_string(), "Qwen2.5-VL-72B".to_string());
    m.insert("blacklist".to_string(), "Qwen2.5-VL-72B".to_string());
    m
}

// ---------------------------------------------------------------------------
// Token expiry jitter
// ---------------------------------------------------------------------------

/// Jittered accessToken lifetime (25–30 min) to prevent fingerprinting.
pub fn access_token_expiry_jitter() -> Duration {
    use rand::Rng;
    let min = 25 * 60 * 1000;
    let max = 30 * 60 * 1000;
    let ms = rand::thread_rng().gen_range(min..max);
    Duration::from_millis(ms)
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicU8, Ordering};

const LOG_DEBUG: u8 = 10;
const LOG_INFO: u8 = 20;
const LOG_WARN: u8 = 30;
const LOG_ERROR: u8 = 40;

static LOG_LEVEL: AtomicU8 = AtomicU8::new(LOG_INFO);

fn parse_level(s: &str) -> u8 {
    match s.to_ascii_lowercase().as_str() {
        "debug" => LOG_DEBUG,
        "info" => LOG_INFO,
        "warn" => LOG_WARN,
        "error" => LOG_ERROR,
        _ => LOG_INFO,
    }
}

pub fn init_log_level() {
    let level = std::env::var("DEVECO_LOG_LEVEL")
        .or_else(|_| std::env::var("LOG_LEVEL"))
        .unwrap_or_else(|_| "info".to_string());
    LOG_LEVEL.store(parse_level(&level), Ordering::Relaxed);
}

fn current_level() -> u8 {
    LOG_LEVEL.load(Ordering::Relaxed)
}

fn fmt_log(level: &str, msg: &str) -> String {
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    format!("[{}] [{}] {}", ts, level, msg)
}

pub fn log_debug(msg: &str) {
    if current_level() <= LOG_DEBUG {
        println!("{}", fmt_log("DEBUG", msg));
    }
}

pub fn log_info(msg: &str) {
    if current_level() <= LOG_INFO {
        println!("{}", fmt_log("INFO", msg));
    }
}

pub fn log_warn(msg: &str) {
    if current_level() <= LOG_WARN {
        eprintln!("{}", fmt_log("WARN", msg));
    }
}

pub fn log_error(msg: &str) {
    if current_level() <= LOG_ERROR {
        eprintln!("{}", fmt_log("ERROR", msg));
    }
}

// ---------------------------------------------------------------------------
// Upstream URL helpers
// ---------------------------------------------------------------------------

/// Returns (origin, api_prefix) e.g. ("https://cn.devecostudio.huawei.com", "/sse/codeGenie/maas/v2")
pub fn upstream_origin_and_prefix() -> (&'static str, &'static str) {
    (DEVECO_BASE_URL, "/sse/codeGenie/maas/v2")
}
