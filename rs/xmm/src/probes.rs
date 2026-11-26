//! 服务拨测（移植自 server/probes.py）：TCP / HTTP / ICMP，独立于 SSH 资源采集。
//!
//! - 监控机直接发起探测（不走 SSH），每条拨测一个独立的 tokio 任务；
//! - 防抖：连续失败 fail_threshold 次才算"宕"，恢复一次成功即"恢复"；
//! - 状态机：unknown → up/down；边沿变化产生事件（接告警推送）；
//! - 每次探测的结果都回调落盘；
//! - ping 用系统命令（Windows: ping -n 1；POSIX: ping -c 1），成败看返回码。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::now_ts;

pub const PROBE_FIELDS: [&str; 10] = [
    "type", "host", "port", "path", "expect", "interval_s", "timeout_s", "fail_threshold", "status_codes", "max_ms",
];

fn get_str<'a>(cfg: &'a Value, key: &str, default: &'a str) -> &'a str {
    cfg.get(key).and_then(|v| v.as_str()).unwrap_or(default)
}

fn get_f(cfg: &Value, key: &str, default: f64) -> f64 {
    cfg.get(key)
        .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .unwrap_or(default)
}

pub fn status_ranges(spec: &str) -> Option<Vec<std::ops::RangeInclusive<u16>>> {
    spec.split(',').map(|part| {
        let part = part.trim();
        let (lo, hi) = part.split_once('-').unwrap_or((part, part));
        let lo: u16 = lo.trim().parse().ok()?;
        let hi: u16 = hi.trim().parse().ok()?;
        (lo >= 100 && hi <= 599 && lo <= hi).then_some(lo..=hi)
    }).collect()
}

#[derive(Clone, Debug)]
pub struct Probe {
    pub name: String,
    pub ptype: String,
    pub host: String,
    pub port: i64,
    pub path: String,
    pub expect: String,
    pub status_codes: String,
    pub max_ms: f64,
    pub interval: f64,
    pub timeout: f64,
    pub threshold: i64,
    pub state: String, // unknown | up | down
    pub ms: Option<f64>,
    pub fails: i64,
    pub last_check: f64,
    pub detail: String,
    pub down_since: Option<f64>,
}

impl Probe {
    pub fn new(name: &str, cfg: &Value) -> Self {
        Self {
            name: name.to_string(),
            ptype: get_str(cfg, "type", "tcp").to_lowercase(),
            host: get_str(cfg, "host", "").trim().to_string(),
            port: get_f(cfg, "port", 80.0) as i64,
            path: get_str(cfg, "path", "/").to_string(),
            expect: get_str(cfg, "expect", "").trim().to_string(),
            status_codes: get_str(cfg, "status_codes", "200-399").to_string(),
            max_ms: get_f(cfg, "max_ms", 0.0),
            interval: get_f(cfg, "interval_s", 30.0).max(5.0),
            timeout: get_f(cfg, "timeout_s", 5.0).max(1.0),
            threshold: (get_f(cfg, "fail_threshold", 2.0) as i64).max(1),
            state: "unknown".into(),
            ms: None,
            fails: 0,
            last_check: 0.0,
            detail: String::new(),
            down_since: None,
        }
    }

    pub fn public(&self) -> Value {
        json!({
            "name": self.name,
            "type": self.ptype,
            "host": self.host,
            "port": if self.ptype != "ping" { json!(self.port) } else { Value::Null },
            "path": if self.ptype == "http" { json!(self.path) } else { Value::Null },
            "interval_s": self.interval, "fail_threshold": self.threshold, "expect": self.expect,
            "max_ms": self.max_ms, "status_codes": self.status_codes,
            "state": self.state,
            "ms": self.ms,
            "fails": self.fails,
            "last_check": self.last_check,
            "detail": self.detail,
        })
    }

    /// 配置签名：变了才重建任务
    fn sig(cfg: &Value) -> (String, String, i64, String, String, i64, i64, i64) {
        (
            get_str(cfg, "type", "tcp").to_lowercase(),
            get_str(cfg, "host", "").trim().to_string(),
            get_f(cfg, "port", 80.0) as i64,
            get_str(cfg, "path", "/").to_string(),
            get_str(cfg, "expect", "").trim().to_string(),
            (get_f(cfg, "interval_s", 30.0).max(5.0) * 1000.0) as i64,
            (get_f(cfg, "timeout_s", 5.0).max(1.0) * 1000.0) as i64,
            (get_f(cfg, "fail_threshold", 2.0) as i64).max(1),
        )
    }
}

/// 事件回调：on_point(host_metric_name...)；用闭包注入，避免循环依赖。
pub type OnPoint = Arc<dyn Fn(&str, f64, Option<f64>) + Send + Sync>;
pub type OnEvent = Arc<dyn Fn(Probe, bool, String) + Send + Sync>;

pub struct ProbeManager {
    get_probes: Arc<dyn Fn() -> Value + Send + Sync>,
    on_point: OnPoint,
    on_event: OnEvent,
    probes: Arc<Mutex<HashMap<String, Arc<Mutex<Probe>>>>>,
    tasks: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    http: reqwest::Client,
}

impl ProbeManager {
    pub fn new(
        get_probes: Arc<dyn Fn() -> Value + Send + Sync>,
        on_point: OnPoint,
        on_event: OnEvent,
    ) -> Arc<Self> {
        Arc::new(Self {
            get_probes,
            on_point,
            on_event,
            probes: Arc::new(Mutex::new(HashMap::new())),
            tasks: Arc::new(Mutex::new(HashMap::new())),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(35))
                .build()
                .unwrap_or_default(),
        })
    }

    /// 按当前配置重建探针任务（改配置/删改拨测后调用）。
    pub fn restart(&self) {
        let want = (self.get_probes)();
        let empty = json!({});
        let want = want.as_object().unwrap_or_else(|| empty.as_object().unwrap());

        let mut tasks = self.tasks.lock().unwrap();
        let mut probes = self.probes.lock().unwrap();

        // 移除已删除的
        let existing: Vec<String> = tasks.keys().cloned().collect();
        for name in existing {
            if !want.contains_key(&name) {
                if let Some(t) = tasks.remove(&name) {
                    t.abort();
                }
                probes.remove(&name);
                tracing::info!("拨测已移除：{}", name);
            }
        }
        for (name, pc) in want {
            if let Some(cur) = probes.get(name) {
                let cur = cur.lock().unwrap();
                let cur_sig = (
                    cur.ptype.clone(), cur.host.clone(), cur.port, cur.path.clone(),
                    cur.expect.clone(), (cur.interval * 1000.0) as i64,
                    (cur.timeout * 1000.0) as i64, cur.threshold,
                );
                if cur_sig == Probe::sig(pc) && cur.status_codes == get_str(pc, "status_codes", "200-399")
                    && cur.max_ms == get_f(pc, "max_ms", 0.0) {
                    continue; // 配置没变就不动它
                }
                drop(cur);
                if let Some(t) = tasks.remove(name) {
                    t.abort();
                }
            }
            let p = Probe::new(name, pc);
            if p.host.is_empty() {
                // 新配置 host 为空（手改 config.toml）：旧探针不能残留在状态表里
                probes.remove(name);
                continue;
            }
            let p = Arc::new(Mutex::new(p));
            probes.insert(name.clone(), p.clone());
            let handle = self.spawn_loop(p);
            tasks.insert(name.clone(), handle);
            tracing::info!("拨测已启动：{}", name);
        }
    }

    fn spawn_loop(&self, p: Arc<Mutex<Probe>>) -> tokio::task::JoinHandle<()> {
        let on_point = self.on_point.clone();
        let on_event = self.on_event.clone();
        let http = self.http.clone();
        tokio::spawn(async move {
            loop {
                let (interval, result) = {
                    let probe = p.lock().unwrap().clone();
                    let res = check(&http, &probe).await;
                    (probe.interval, res)
                };
                let (ok, ms, detail) = result;
                let mut event: Option<(Probe, bool, String)> = None;
                {
                    let mut probe = p.lock().unwrap();
                    probe.ms = ms;
                    probe.last_check = now_ts();
                    probe.detail = detail.chars().take(200).collect();
                    let prev_state = probe.state.clone();
                    if ok {
                        probe.fails = 0;
                        if prev_state != "up" {
                            probe.state = "up".into();
                            if prev_state == "down" {
                                tracing::info!("拨测恢复：{}（{:.0} ms）", probe.name, ms.unwrap_or(-1.0));
                                event = Some((probe.clone(), true, format!("{:.0} ms", ms.unwrap_or(-1.0))));
                            }
                            probe.down_since = None;
                        }
                    } else {
                        probe.fails += 1;
                        if probe.fails >= probe.threshold && prev_state != "down" {
                            probe.state = "down".into();
                            probe.down_since = Some(now_ts());
                            tracing::warn!("拨测失败：{} 连续 {} 次（{}）", probe.name, probe.fails, detail);
                            event = Some((probe.clone(), false, detail.clone()));
                        }
                    }
                    on_point(&probe.name.clone(), if ok { 1.0 } else { 0.0 }, ms);
                }
                if let Some((probe, up, d)) = event {
                    on_event(probe, up, d);
                }
                tokio::time::sleep(std::time::Duration::from_secs_f64(interval)).await;
            }
        })
    }

    pub fn stop(&self) {
        let mut tasks = self.tasks.lock().unwrap();
        for (_, t) in tasks.drain() {
            t.abort();
        }
    }

    pub fn status(&self) -> Vec<Value> {
        self.probes
            .lock()
            .unwrap()
            .values()
            .map(|p| p.lock().unwrap().public())
            .collect()
    }
}

async fn check(http: &reqwest::Client, p: &Probe) -> (bool, Option<f64>, String) {
    let t0 = std::time::Instant::now();
    match p.ptype.as_str() {
        "tcp" => {
            let addr = format!("{}:{}", p.host, p.port);
            match tokio::time::timeout(
                std::time::Duration::from_secs_f64(p.timeout),
                tokio::net::TcpStream::connect(&addr),
            )
            .await
            {
                Ok(Ok(_)) => (true, Some(t0.elapsed().as_secs_f64() * 1000.0), "connect ok".into()),
                Ok(Err(e)) => (false, None, e.to_string()),
                Err(_) => (false, None, "连接超时".into()),
            }
        }
        "http" => {
            let url = if p.host.contains("://") {
                if p.path == "/" || p.path.is_empty() {
                    p.host.clone()
                } else {
                    format!("{}{}", p.host.trim_end_matches('/'), p.path)
                }
            } else {
                format!("http://{}:{}{}", p.host, p.port, if p.path.is_empty() { "/" } else { &p.path })
            };
            let result = http
                .get(&url)
                .timeout(std::time::Duration::from_secs_f64(p.timeout))
                .send()
                .await;
            match result {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let body = if p.expect.is_empty() { String::new() } else {
                        match resp.text().await {
                            Ok(body) => body,
                            Err(_) => return (false, None, "响应正文读取失败".into()),
                        }
                    };
                    let ms = t0.elapsed().as_secs_f64() * 1000.0;
                    if !status_ranges(&p.status_codes).is_some_and(|ranges| ranges.iter().any(|r| r.contains(&status))) {
                        (false, Some(ms), format!("HTTP {} 不在期望范围 {}", status, p.status_codes))
                    } else if !p.expect.is_empty() && !body.contains(&p.expect) {
                        (false, Some(ms), format!("HTTP {} 但未找到期望内容", status))
                    } else if p.max_ms > 0.0 && ms > p.max_ms {
                        (false, Some(ms), format!("响应 {:.0} ms 超过 {:.0} ms", ms, p.max_ms))
                    } else {
                        (true, Some(ms), format!("HTTP {}", status))
                    }
                }
                Err(e) => (false, None, e.to_string()),
            }
        }
        "ping" => {
            let (flag_n, flag_w, wval) = if cfg!(windows) {
                ("-n", "-w", (p.timeout * 1000.0) as i64)
            } else {
                ("-c", "-W", p.timeout as i64)
            };
            let result = tokio::time::timeout(
                std::time::Duration::from_secs_f64(p.timeout + 2.0),
                tokio::process::Command::new("ping")
                    .args([flag_n, "1", flag_w, &wval.to_string(), &p.host])
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output(),
            )
            .await;
            match result {
                Ok(Ok(out)) => {
                    let ms = t0.elapsed().as_secs_f64() * 1000.0;
                    let rc = out.status.code().unwrap_or(-1);
                    let ok = ping_reply_ok(rc, &out.stdout);
                    let detail = if rc == 0 && !ok {
                        "ping 不可达（无 TTL 回包）".to_string()
                    } else {
                        format!("ping rc={}", rc)
                    };
                    (ok, Some(ms), detail)
                }
                Ok(Err(e)) => (false, None, e.to_string()),
                Err(_) => (false, None, "ping 超时".into()),
            }
        }
        t => (false, None, format!("未知类型 {}", t)),
    }
}

/// ping 回包判定：rc==0 还不够——Windows 的 ping.exe 收到路由器代答的
/// "Destination host unreachable" / "TTL expired in transit" 也返回 0。
/// 只有回包里带 TTL 才算真正通（中英文 Windows 与 Linux 输出都含 "TTL="）。
fn ping_reply_ok(rc: i32, stdout: &[u8]) -> bool {
    rc == 0 && String::from_utf8_lossy(stdout).to_ascii_lowercase().contains("ttl=")
}

#[cfg(test)]
mod ping_tests {
    use super::ping_reply_ok;

    #[test]
    fn windows_unreachable_reply_with_rc0_is_not_up() {
        // 路由器代答不可达：ping.exe 返回码是 0，但没有 TTL
        let out = "Reply from 192.168.1.1: Destination host unreachable.\r\n";
        assert!(!ping_reply_ok(0, out.as_bytes()));
        let cn = "来自 192.168.1.1 的回复: 无法访问目标主机。\r\n";
        assert!(!ping_reply_ok(0, cn.as_bytes()));
    }

    #[test]
    fn real_reply_with_ttl_is_up() {
        let en = "Reply from 10.0.0.1: bytes=32 time=1ms TTL=64\r\n";
        assert!(ping_reply_ok(0, en.as_bytes()));
        let cn = "来自 10.0.0.1 的回复: 字节=32 时间<1ms TTL=64\r\n";
        assert!(ping_reply_ok(0, cn.as_bytes()));
        let linux = "64 bytes from 10.0.0.1: icmp_seq=1 ttl=64 time=0.5 ms\n";
        assert!(ping_reply_ok(0, linux.as_bytes()));
    }

    #[test]
    fn nonzero_rc_is_never_up() {
        let out = "Reply from 10.0.0.1: bytes=32 time=1ms TTL=64\r\n";
        assert!(!ping_reply_ok(1, out.as_bytes()));
        assert!(!ping_reply_ok(-1, b""));
    }
}
