//! 告警推送（移植自 server/notify.py）：Server酱 / Telegram / SMTP / WebHook。
//!
//! - 通道按配置自动发现（填了凭据即启用），支持多通道同时发送；
//! - 收敛：同一告警（host:kind:zone）未恢复前不重复推；恢复后重新计；
//! - 恢复通知：告警解除时推送"已恢复"，带持续时间；
//! - 静默时段：quiet_start/quiet_end（HH:MM），期间告警与恢复都只记不发；
//!   心跳不受静默限制；
//! - 发送失败重试 3 次（2s/4s/8s 退避），最终失败只记日志，绝不拖垮采集。

use chrono::{Local, Timelike};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::now_ts;

pub fn alert_key(host: &str, item: &Value) -> String {
    format!(
        "{}:{}:{}",
        host,
        item.get("kind").and_then(|v| v.as_str()).unwrap_or("?"),
        item.get("zone").and_then(|v| v.as_str()).unwrap_or("")
    )
}

fn get_str<'a>(cfg: &'a Value, key: &str) -> &'a str {
    cfg.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

fn get_f(cfg: &Value, key: &str, default: f64) -> f64 {
    cfg.get(key)
        .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .unwrap_or(default)
}

/// 静默时段判断；起止为空 = 不静默。支持跨夜（如 22:00-07:00）。
pub fn in_quiet(cfg: &Value) -> bool {
    let start = get_str(cfg, "quiet_start").trim();
    let end = get_str(cfg, "quiet_end").trim();
    if start.is_empty() || end.is_empty() {
        return false;
    }
    let parse = |s: &str| -> Option<i64> {
        let (h, m) = s.split_once(':')?;
        Some(h.parse::<i64>().ok()? * 60 + m.parse::<i64>().ok()?)
    };
    let (Some(l), Some(r)) = (parse(start), parse(end)) else { return false };
    let now = Local::now();
    let cur = now.hour() as i64 * 60 + now.minute() as i64;
    if l == r {
        return false;
    }
    if l < r {
        l <= cur && cur < r
    } else {
        cur >= l || cur < r // 跨夜
    }
}

struct DeliveryError {
    detail: String,
    retry: bool,
}

impl DeliveryError {
    fn new(detail: impl Into<String>, retry: bool) -> Self {
        Self { detail: detail.into(), retry }
    }
}

pub struct Notify {
    store: Arc<crate::store::Store>,
    cfg_getter: Arc<dyn Fn() -> Value + Send + Sync>,
    delivery: tokio::sync::mpsc::Sender<(String, String)>,
    boot_ts: f64,
    last_sent: Mutex<HashMap<String, f64>>, // alert_key -> ts（恢复后的冷却）
    active: Mutex<HashSet<String>>,         // 已推送且未恢复的告警 key
    http: reqwest::Client,
}

impl Notify {
    pub fn new(cfg_getter: Arc<dyn Fn() -> Value + Send + Sync>, store: Arc<crate::store::Store>) -> Arc<Self> {
        let (delivery, mut messages) = tokio::sync::mpsc::channel(512);
        let notifier = Arc::new(Self {
            delivery,
            store,
            cfg_getter,
            boot_ts: now_ts(),
            last_sent: Mutex::new(HashMap::new()),
            active: Mutex::new(HashSet::new()),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
        });
        let weak = Arc::downgrade(&notifier);
        tokio::spawn(async move {
            while let Some((title, body)) = messages.recv().await {
                let Some(notifier) = weak.upgrade() else { break };
                notifier.broadcast_all(&title, &body, false).await;
            }
        });
        notifier
    }

    fn cfg(&self) -> Value {
        let all = (self.cfg_getter)();
        all.get("notify").cloned().unwrap_or_else(|| json!({}))
    }

    pub fn channels(&self) -> Vec<&'static str> {
        let c = self.cfg();
        let mut names = Vec::new();
        if !get_str(&c, "serverchan_key").trim().is_empty() {
            names.push("serverchan");
        }
        if !get_str(&c, "telegram_token").trim().is_empty()
            && !get_str(&c, "telegram_chat").trim().is_empty()
        {
            names.push("telegram");
        }
        if !get_str(&c, "smtp_host").trim().is_empty() && !get_str(&c, "smtp_to").trim().is_empty() {
            names.push("smtp");
        }
        if !get_str(&c, "webhook_url").trim().is_empty() {
            names.push("webhook");
        }
        names
    }

    pub fn status(&self) -> Value {
        let c = self.cfg();
        json!({
            "enabled": c.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
            "channels": self.channels(),
            "dedup_minutes": get_f(&c, "dedup_minutes", 10.0),
            "quiet_start": get_str(&c, "quiet_start"),
            "quiet_end": get_str(&c, "quiet_end"),
            "heartbeat_minutes": get_f(&c, "heartbeat_minutes", 0.0),
            "quiet_now": in_quiet(&c),
        })
    }

    // ---------- 事件入口 ----------

    pub async fn alert(&self, host: &str, item: &Value, since_ts: Option<f64>) {
        let key = alert_key(host, item);
        let cfg = self.cfg();
        if self.active.lock().unwrap().contains(&key) {
            return; // 未恢复不重复推
        }
        let dedup = get_f(&cfg, "dedup_minutes", 10.0).max(0.0) * 60.0;
        let last = self.last_sent.lock().unwrap().get(&key).copied().unwrap_or(0.0);
        if now_ts() - last < dedup {
            return; // 冷却期内
        }
        self.active.lock().unwrap().insert(key.clone());
        self.last_sent.lock().unwrap().insert(key, now_ts());
        let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("异常");
        let when = chrono::DateTime::from_timestamp(since_ts.unwrap_or_else(now_ts) as i64, 0)
            .map(|t| t.with_timezone(&Local).format("%m-%d %H:%M:%S").to_string())
            .unwrap_or_default();
        self.broadcast(
            &format!("⚠ {} {}", host, text),
            &format!("主机：{}\n{}\n发生时间：{}", host, text, when),
        )
        .await;
    }

    pub async fn recovered(&self, host: &str, item: &Value, since_ts: Option<f64>) {
        let key = alert_key(host, item);
        if !self.active.lock().unwrap().remove(&key) {
            return;
        }
        let mins = ((now_ts() - since_ts.unwrap_or_else(now_ts)) / 60.0).max(0.0);
        let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("异常");
        self.broadcast(
            &format!("✓ {} 已恢复", host),
            &format!("{}\n已恢复正常（持续 {:.0} 分钟）", text, mins),
        )
        .await;
    }

    /// 心跳：不受静默时段限制（它的意义就是"我还活着"）。
    pub async fn heartbeat(&self) {
        let hours = (now_ts() - self.boot_ts) / 3600.0;
        self.broadcast_all(
            "[心跳] 主机监控小助手运行中",
            &format!("监控服务正常，已连续运行 {:.1} 小时。", hours),
            true,
        )
        .await;
    }

    /// 告警/恢复广播：受 enabled 与静默时段约束。
    pub async fn broadcast(&self, title: &str, body: &str) {
        let cfg = self.cfg();
        if !cfg.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true) {
            return;
        }
        if in_quiet(&cfg) {
            tracing::info!("静默时段内，跳过推送：{}", title);
            return;
        }
        if !self.channels().is_empty() && self.delivery.try_send((title.into(), body.into())).is_err() {
            self.store.record_delivery("queue", title, 0, false, "队列已满或关闭");
            tracing::warn!("告警通知队列已满或已关闭，未发送：{}", title);
        }
    }

    /// 设置面板的"发送测试消息"。
    pub async fn test(&self, channel: &str) -> (bool, String) {
        if !["serverchan", "telegram", "smtp", "webhook"].contains(&channel) {
            return (false, "未知通道".into());
        }
        match self.attempt(channel, "测试消息", "这是一条来自主机监控小助手的测试推送。", 1).await {
            Ok(()) => (true, format!("{} 通道已接受", channel)),
            Err(e) => (false, e.detail),
        }
    }

    // ---------- 发送 ----------

    async fn broadcast_all(&self, title: &str, body: &str, ignore_quiet: bool) {
        let names = self.channels();
        if names.is_empty() {
            return;
        }
        if !ignore_quiet && in_quiet(&self.cfg()) {
            return;
        }
        // 并发发送（future 借用 &self，无需 Arc 克隆）
        futures::future::join_all(
            names.iter().map(|n| self.send_retry(n, title, body)),
        )
        .await;
    }

    async fn send_retry(&self, channel: &str, title: &str, body: &str) {
        for (i, delay) in [0u64, 2, 4, 8].iter().enumerate() {
            if *delay > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(*delay)).await;
            }
            match self.attempt(channel, title, body, i + 1).await {
                Ok(()) => return,
                Err(e) if !e.retry => return,
                Err(e) => tracing::warn!("推送 {} 第 {} 次失败：{}", channel, i + 1, e.detail),
            }
        }
        tracing::error!("推送 {} 重试 3 次均失败，放弃（{}）", channel, title);
    }

    async fn attempt(&self, channel: &str, title: &str, body: &str, attempt: usize) -> Result<(), DeliveryError> {
        let result = tokio::time::timeout(std::time::Duration::from_secs(20), self.send(channel, title, body))
            .await.unwrap_or_else(|_| Err(DeliveryError::new("通道响应超时", true)));
        let detail = result.as_ref().map(|_| "通道已接受").unwrap_or_else(|e| e.detail.as_str());
        self.store.record_delivery(channel, title, attempt, result.is_ok(), detail);
        result
    }

    async fn send(&self, channel: &str, title: &str, body: &str) -> Result<(), DeliveryError> {
        let cfg = self.cfg();
        let request = match channel {
            "serverchan" => {
                let url = format!("https://sctapi.ftqq.com/{}.send", get_str(&cfg, "serverchan_key").trim());
                let title: String = title.chars().take(32).collect();
                self.http.post(url).form(&[("title", title.as_str()), ("desp", body)])
            }
            "telegram" => {
                let url = format!("https://api.telegram.org/bot{}/sendMessage", get_str(&cfg, "telegram_token").trim());
                self.http.post(url).json(&json!({"chat_id":get_str(&cfg,"telegram_chat"), "text":format!("{}\n{}", title,body)}))
            }
            "webhook" => self.http.post(get_str(&cfg, "webhook_url").trim())
                .json(&json!({"title": title, "body": body, "ts": now_ts()})),
            "smtp" => return self.send_smtp(&cfg,title,body).await.map_err(|e| DeliveryError::new(e,true)),
            _ => return Err(DeliveryError::new("未知通道", false)),
        };
        let response = request.send().await.map_err(|e| DeliveryError::new(e.without_url().to_string(), true))?;
        let status = response.status();
        if !status.is_success() {
            return Err(DeliveryError::new(format!("HTTP {}", status.as_u16()),
                status.is_server_error() || matches!(status.as_u16(), 408 | 429)));
        }
        if channel == "telegram" || channel == "serverchan" {
            let data: Value = response.json().await.map_err(|_| DeliveryError::new("通道回执格式错误", false))?;
            let accepted = if channel == "telegram" { data["ok"].as_bool() == Some(true) }
                else { data["code"].as_i64() == Some(0) };
            if !accepted { return Err(DeliveryError::new("通道回执拒绝了消息", false)); }
        }
        Ok(())
    }

    async fn send_smtp(&self, c: &Value, title: &str, body: &str) -> Result<(), String> {
        use lettre::message::Message;
        use lettre::transport::smtp::authentication::Credentials;
        use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

        let host = get_str(c, "smtp_host").trim().to_string();
        let port = get_f(c, "smtp_port", 465.0) as u16;
        let user = get_str(c, "smtp_user").trim().to_string();
        let pass = get_str(c, "smtp_pass").trim().to_string();
        let to_list: Vec<String> = get_str(c, "smtp_to")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if to_list.is_empty() {
            return Err("未配置收件人".into());
        }
        let from_addr = if !user.is_empty() { user.clone() } else { to_list[0].clone() };
        let mut builder = Message::builder()
            .from(format!("主机监控小助手 <{}>", from_addr).parse().map_err(|e| format!("{:?}", e))?)
            .subject(title);
        for to in &to_list {
            builder = builder.to(to.parse().map_err(|e| format!("{:?}", e))?);
        }
        let msg = builder.body(body.to_string()).map_err(|e| e.to_string())?;

        let transport = if port == 465 {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&host)
                .map_err(|e| e.to_string())?
                .port(465)
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&host)
                .map_err(|e| e.to_string())?
                .port(port)
        };
        let transport = if !user.is_empty() && !pass.is_empty() {
            transport.credentials(Credentials::new(user, pass))
        } else {
            transport
        };
        transport.build().send(msg).await.map_err(|e| e.to_string())?;
        Ok(())
    }
}
