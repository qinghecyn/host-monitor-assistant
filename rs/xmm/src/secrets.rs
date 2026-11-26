//! 凭据管理：密码不写日志、不进前端（移植自 server/secrets.py）。
//!
//! SSH 密码来源优先级（对每个目标主机）：
//!   1) 目标配置里的 password（明文，仅作兜底，不推荐）
//!   2) 环境变量（目标 password_env 或全局 MON_PASSWORD）
//!   3) 系统密钥环（Windows 凭据管理器）
//! Web 登录密码来源：环境变量 MON_WEB_PASSWORD > config [auth].password。

use serde_json::Value;

const SERVICE: &str = "xiaozhushou_monitor";

fn target_key(tcfg: &Value) -> String {
    format!(
        "{}@{}:{}",
        tcfg.get("username").and_then(|v| v.as_str()).unwrap_or(""),
        tcfg.get("host").and_then(|v| v.as_str()).unwrap_or(""),
        tcfg.get("port").and_then(|v| v.as_i64()).unwrap_or(22),
    )
}

pub fn get_ssh_password(tcfg: &Value, global_auth: Option<&Value>) -> String {
    if let Some(pw) = tcfg.get("password").and_then(|v| v.as_str()) {
        if !pw.is_empty() {
            return pw.to_string();
        }
    }
    let env_name = tcfg
        .get("password_env")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            global_auth
                .and_then(|a| a.get("password_env"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "MON_PASSWORD".into());
    if let Ok(pw) = std::env::var(&env_name) {
        if !pw.is_empty() {
            return pw;
        }
    }
    if tcfg.get("host").is_some() {
        if let Ok(entry) = keyring::Entry::new(SERVICE, &target_key(tcfg)) {
            if let Ok(pw) = entry.get_password() {
                if !pw.is_empty() {
                    return pw;
                }
            }
        }
    }
    global_auth
        .and_then(|a| a.get("password"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// 优先写系统密钥环；不可用则返回 false，由调用方决定是否落盘。
pub fn save_ssh_password(tcfg: &Value, password: &str) -> bool {
    if let Ok(entry) = keyring::Entry::new(SERVICE, &target_key(tcfg)) {
        if entry.set_password(password).is_ok() {
            return true;
        }
    }
    false
}

pub fn get_web_password(auth_cfg: &Value) -> String {
    if let Ok(pw) = std::env::var("MON_WEB_PASSWORD") {
        if !pw.is_empty() {
            return pw;
        }
    }
    auth_cfg
        .get("password")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("admin123")
        .to_string()
}
