//! 模型探测模块 —— 动态模型列表获取 + 回退。
//!
//! 从 deveco-local-model-dashboard/src/models.ts 移植。行为等价：
//!  1. 用 access_token GET /codeGenie/modelConfig 获取动态模型列表
//!  2. 映射为 ProviderInfo/ModelInfo
//!  3. 应用 blacklist 过滤
//!  4. 合并 extra_maas_models（glm-5.2 / deepseek-v4-flash）
//!  5. 1 小时缓存，失败回退到静态默认
//!
//! 缓存策略：std::sync::RwLock 持有缓存值（同步快路径读取），
//! tokio::sync::Mutex 作为 fetch 锁串行化重新拉取，避免惊群与重复请求。

use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::config::{
    default_task_model_map, deveco_defaults, extra_maas_models, log_debug, log_warn,
    static_provider_fields, ModelInfo, ModelLimit, ModelModalities, ProviderInfo,
    DEVECO_MODEL_CONFIG_URL, PLUGIN_VERSION,
};

// ---------------------------------------------------------------------------
// 缓存 TTL
// ---------------------------------------------------------------------------

/// 缓存有效期 —— 超过后下一次请求重新拉取 API。
const CACHE_TTL: Duration = Duration::from_secs(60 * 60); // 1 小时

/// HTTP 请求超时。
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// 原始 API 形状（仅反序列化我们实际消费的字段子集）
// ---------------------------------------------------------------------------

/// output 字段可能是 string 或 number，用 serde_json::Value 统一接收。
#[derive(Debug, Default, serde::Deserialize)]
struct RawModelConfig {
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    thinking_mode: Option<String>,
    #[serde(default)]
    tool_call_mode: Option<String>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    output: Option<serde_json::Value>,
    #[serde(default)]
    input_modalities: Option<Vec<String>>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct RawInnerModel {
    #[serde(default)]
    model_configs: Option<Vec<RawModelConfig>>,
    #[serde(default)]
    task_default_model_map: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct RawBody {
    #[serde(default)]
    inner_models: Option<Vec<RawInnerModel>>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct RawApiResponse {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    body: Option<RawBody>,
}

// ---------------------------------------------------------------------------
// 映射
// ---------------------------------------------------------------------------

/// 把 output（string | number）解析为 u64。
fn parse_output_limit(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.parse::<u64>().ok(),
        _ => None,
    }
}

/// 将原始 RawModelConfig 映射为内部 ModelInfo。
fn map_model_config_to_internal(c: &RawModelConfig) -> ModelInfo {
    let mut info = ModelInfo {
        name: c.model_id.clone(),
        ..Default::default()
    };

    if c.thinking_mode.as_deref() == Some("on") {
        info.reasoning = Some(true);
    }
    if c.tool_call_mode.as_deref() == Some("tool_calls") {
        info.tool_call = Some(true);
    }

    let mut limit = ModelLimit::default();
    if let Some(ctx) = c.context_window {
        limit.context = Some(ctx);
    }
    if let Some(out) = c.output.as_ref().and_then(parse_output_limit) {
        limit.output = Some(out);
    }
    if limit.context.is_some() || limit.output.is_some() {
        info.limit = Some(limit);
    }

    if let Some(im) = &c.input_modalities {
        if !im.is_empty() {
            info.modalities = Some(ModelModalities {
                input: Some(im.clone()),
                output: Some(vec!["text".to_string()]),
            });
        }
    }

    info
}

/// 用 blacklist 过滤模型表，返回带静态 provider 字段的 ProviderInfo。
fn filter_blacklist(models: BTreeMap<String, ModelInfo>, blacklist: &[String]) -> ProviderInfo {
    let filtered: BTreeMap<String, ModelInfo> = models
        .into_iter()
        .filter(|(id, _)| !blacklist.iter().any(|b| b == id))
        .collect();
    let mut p = static_provider_fields();
    p.models = Some(filtered);
    p
}

/// 把逗号分隔的 blacklist 字符串拆成 Vec<String>。
fn split_blacklist(s: Option<&String>) -> Vec<String> {
    match s {
        Some(s) => s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        None => Vec::new(),
    }
}

/// 合并静态 maas 条目（glm-5.2 / deepseek-v4-flash）。
///
/// modelConfig API 可能不列出它们，但代理仍通过 maas HTTP 通道服务，
/// 因此必须对外广播以便客户端选择；当 API 已列出时，API 数据优先。
fn with_extra_models(mut config: ProviderInfo) -> ProviderInfo {
    let models = config.models.get_or_insert_with(BTreeMap::new);
    for (id, info) in extra_maas_models() {
        models.entry(id).or_insert(info);
    }
    config
}

/// 取静态默认的 models 表（deveco_defaults().models 的克隆）。
fn deveco_defaults_models() -> BTreeMap<String, ModelInfo> {
    deveco_defaults().models.unwrap_or_default()
}

// ---------------------------------------------------------------------------
// API 拉取
// ---------------------------------------------------------------------------

fn model_config_url() -> String {
    format!(
        "{}?localVersion=0&pluginVersion=CLI.{}",
        DEVECO_MODEL_CONFIG_URL, PLUGIN_VERSION
    )
}

struct FetchedModels {
    models: BTreeMap<String, ModelInfo>,
    task_default_model_map: Option<BTreeMap<String, String>>,
}

async fn fetch_models_from_api(access_token: &str) -> Result<FetchedModels, anyhow::Error> {
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()?;
    let url = model_config_url();

    let resp = client
        .get(&url)
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Model config API returned {status}: {body}");
    }

    let raw: RawApiResponse = resp.json().await?;
    if raw.code != Some(200) {
        anyhow::bail!("Model config API returned code {:?}", raw.code);
    }

    let inner_models = raw
        .body
        .and_then(|b| b.inner_models)
        .unwrap_or_default();

    // task_default_model_map 直接从 inner_models 中提取（最后一个非空者胜出，与 TS 一致）
    let mut task_default_model_map: Option<BTreeMap<String, String>> = None;
    let mut models: BTreeMap<String, ModelInfo> = BTreeMap::new();

    for group in &inner_models {
        if let Some(m) = &group.task_default_model_map {
            task_default_model_map = Some(m.clone());
        }
        if let Some(cfgs) = &group.model_configs {
            for c in cfgs {
                if let Some(id) = &c.model_id {
                    if !id.is_empty() {
                        models.insert(id.clone(), map_model_config_to_internal(c));
                    }
                }
            }
        }
    }

    log_debug(&format!("fetched models config: count={}", models.len()));
    Ok(FetchedModels {
        models,
        task_default_model_map,
    })
}

// ---------------------------------------------------------------------------
// 缓存状态
// ---------------------------------------------------------------------------

struct CacheState {
    config: Option<ProviderInfo>,
    cached_at: Option<Instant>,
    task_default_model_map: BTreeMap<String, String>,
}

static CACHE: OnceLock<RwLock<CacheState>> = OnceLock::new();
/// 串行化重新拉取的 fetch 锁。
static FETCH_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn cache() -> &'static RwLock<CacheState> {
    CACHE.get_or_init(|| {
        RwLock::new(CacheState {
            config: None,
            cached_at: None,
            task_default_model_map: default_task_model_map(),
        })
    })
}

fn fetch_lock() -> &'static Mutex<()> {
    FETCH_LOCK.get_or_init(|| Mutex::new(()))
}

// ---------------------------------------------------------------------------
// 公共 API
// ---------------------------------------------------------------------------

/// 返回 DevEco provider 配置。优先用动态 API（成功时缓存），
/// 任何失败都回退到静态默认。
///
/// 语义说明（与 TS 版一致）：上游把视觉专用模型（Qwen3_VL_...；
/// tool_call: none）放入 task_default_model_map.blacklist，对其应用后
/// 公开列表只剩 GLM-5.1 等可全流程 agent/tool 的模型；被隐藏的 VL 模型
/// 仍作为图片回退在内部使用，故无需改动 blacklist。
pub async fn get_deveco_provider_config(access_token: &str) -> ProviderInfo {
    // 快路径：读锁下检查缓存命中
    {
        let s = cache().read().unwrap();
        if let (Some(cfg), Some(at)) = (&s.config, s.cached_at) {
            if at.elapsed() < CACHE_TTL {
                return cfg.clone();
            }
        }
    }

    // 慢路径：取 fetch 锁，double-check 后拉取
    let _fetch = fetch_lock().lock().await;
    {
        let s = cache().read().unwrap();
        if let (Some(cfg), Some(at)) = (&s.config, s.cached_at) {
            if at.elapsed() < CACHE_TTL {
                return cfg.clone();
            }
        }
    }

    // 已过期 —— 先清空，避免失败后仍返回陈旧数据
    {
        let mut s = cache().write().unwrap();
        s.config = None;
    }

    let default_blacklist = split_blacklist(default_task_model_map().get("blacklist"));

    match fetch_models_from_api(access_token).await {
        Ok(fetched) => {
            if fetched.models.is_empty() {
                log_warn("API returned empty models, using defaults");
                return with_extra_models(filter_blacklist(
                    deveco_defaults_models(),
                    &default_blacklist,
                ));
            }

            let blacklist = fetched
                .task_default_model_map
                .as_ref()
                .and_then(|m| m.get("blacklist"))
                .map(|s| split_blacklist(Some(s)))
                .unwrap_or_else(|| default_blacklist.clone());

            let cfg = with_extra_models(filter_blacklist(fetched.models, &blacklist));

            {
                let mut s = cache().write().unwrap();
                s.config = Some(cfg.clone());
                s.cached_at = Some(Instant::now());
                s.task_default_model_map = fetched
                    .task_default_model_map
                    .unwrap_or_else(default_task_model_map);
            }
            cfg
        }
        Err(e) => {
            log_warn(&format!("failed to fetch models, using defaults: {e}"));
            with_extra_models(filter_blacklist(
                deveco_defaults_models(),
                &default_blacklist,
            ))
        }
    }
}

/// 返回缓存的 task→model 映射（动态配置拉取前用默认值）。
pub fn get_task_default_model_map() -> BTreeMap<String, String> {
    cache().read().unwrap().task_default_model_map.clone()
}

/// 重置缓存（用户重新登录时使用）。
pub fn reset_model_cache() {
    let mut s = cache().write().unwrap();
    s.config = None;
    s.cached_at = None;
    s.task_default_model_map = default_task_model_map();
}
