//! 主机监控小助手 —— Rust 后端核心库
//!
//! 由 Python 后端（server/ + collector/）逐模块移植，对外 HTTP/WebSocket/JSON
//! 契约与 Python 版完全一致，web/ 前端零改动。

pub mod alerts;
pub mod api;
pub mod auth;
pub mod config;
pub mod history;
pub mod mock;
pub mod notify;
pub mod parser;
pub mod probes;
pub mod secrets;
pub mod service;
pub mod ssh;
pub mod state;
pub mod store;
pub mod ws;

/// 当前版本号（桌面 / 独立服务共用）；beta 标记随版本徽标同步
pub const VERSION: &str = "beta1.4.3";

/// Unix 时间戳（秒，带小数）
pub fn now_ts() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Python round(x, 1) 的近似（四舍五入到 1 位小数）
pub fn r1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Python round(x, 2)
pub fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Python round(x, 3)
pub fn r3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}
