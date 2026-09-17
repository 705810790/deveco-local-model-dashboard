//! 协议转换模块。
//!
//! 从 deveco-local-model-dashboard 的 anthropic-transform.ts + responses-transform.ts
//! + vision-routing.ts + openai-normalize.ts 移植。
//!
//! 所有函数操作 `serde_json::Value`，用 JSON 操作贴近原 TS 实现。

pub mod anthropic;
pub mod responses;
pub mod vision;
pub mod normalize;
