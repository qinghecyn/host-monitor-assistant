//! SQLite 历史落盘（移植自 server/store.py）。
//!
//! - 长表 samples(ts, host, metric, value)，一行一个指标点；(host, metric, ts) 建索引；
//! - 写入走内存队列，后台任务每 flush_interval 秒批量进一个事务；
//! - WAL 模式，读写互不阻塞；
//! - 查询自动降采样：按时间范围把桶数压到 max_points 以内（桶内取平均）；
//! - 过期清理每小时跑一次（保留天数可配）。

use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::parser::pick_primary_network;
use crate::{now_ts, r3};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS deliveries (
    id INTEGER PRIMARY KEY, ts REAL NOT NULL, channel TEXT NOT NULL,
    title TEXT NOT NULL, attempt INTEGER NOT NULL, ok INTEGER NOT NULL, detail TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS samples (
    ts     REAL NOT NULL,
    host   TEXT NOT NULL,
    metric TEXT NOT NULL,
    value  REAL
);
CREATE INDEX IF NOT EXISTS idx_samples ON samples (host, metric, ts);
";

type Point = (f64, String, String, f64);

/// 挂载点 → 指标名安全片段（/mnt/data → _mnt_data）；只保留字母数字与 _.-。
fn sanitize_metric(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        out.push(if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') { c } else { '_' });
    }
    out
}

/// 从一帧数据提取要落盘的指标点。
pub fn frame_metrics(frame: &Value, host: &str) -> Vec<Point> {
    let mut out = Vec::new();
    let ts = frame.get("ts").and_then(|v| v.as_f64()).unwrap_or_else(now_ts);
    let mut add = |metric: &str, value: Option<f64>| {
        if let Some(v) = value {
            out.push((ts, host.to_string(), metric.to_string(), v));
        }
    };

    if let Some(cpu) = frame.get("cpu") {
        add("cpu.percent", cpu.get("percent").and_then(|v| v.as_f64()));
    }
    if let Some(mem) = frame.get("memory") {
        if let Some(ap) = mem.get("available_percent").and_then(|v| v.as_f64()) {
            add("mem.used_percent", Some(100.0 - ap));
        }
    }
    let temps: Vec<f64> = frame
        .get("temperatures")
        .and_then(|t| t.as_array())
        .map(|a| a.iter().filter_map(|t| t.get("temp_c").and_then(|v| v.as_f64())).collect())
        .unwrap_or_default();
    if let Some(max) = temps.iter().copied().reduce(f64::max) {
        add("temp.max", Some(max));
    }
    let fans: Vec<f64> = frame
        .get("fans")
        .and_then(|t| t.as_array())
        .map(|a| a.iter().filter_map(|t| t.get("rpm").and_then(|v| v.as_f64())).collect())
        .unwrap_or_default();
    if let Some(max) = fans.iter().copied().reduce(f64::max) {
        add("fan.max", Some(max));
    }

    // 主网卡：与采集/前端同口径（primary 标记，其次重新评选）
    if let Some(nets) = frame.get("network").and_then(|n| n.as_array()) {
        if !nets.is_empty() {
            let idx = nets
                .iter()
                .position(|n| n.get("primary").and_then(|p| p.as_bool()).unwrap_or(false))
                .or_else(|| pick_primary_network(nets));
            if let Some(i) = idx {
                add("net.rx_kbps", nets[i].get("rx_kbps").and_then(|v| v.as_f64()));
                add("net.tx_kbps", nets[i].get("tx_kbps").and_then(|v| v.as_f64()));
            }
        }
    }

    if let Some(disks) = frame.get("disks").and_then(|d| d.as_array()) {
        for d in disks {
            let mount = d.get("mount").and_then(|m| m.as_str()).unwrap_or("");
            if let (Some(total), Some(avail)) = (
                d.get("total_kb").and_then(|v| v.as_f64()),
                d.get("available_kb").and_then(|v| v.as_f64()),
            ) {
                if total > 0.0 {
                    // 根分区沿用旧指标名（历史连续、前端指标表兼容）；其余挂载点逐盘落使用率
                    if mount == "/" {
                        add("disk.root_used_percent", Some(100.0 * (1.0 - avail / total)));
                    } else {
                        add(&format!("disk.{}.used_percent", sanitize_metric(mount)), Some(100.0 * (1.0 - avail / total)));
                    }
                }
            }
        }
    }

    if let Some(dk) = frame.get("docker") {
        if dk.get("daemon").and_then(|d| d.as_str()) == Some("ok") {
            add("docker.running", dk.get("running").and_then(|v| v.as_f64()).or(Some(0.0)));
            add("docker.total", dk.get("total").and_then(|v| v.as_f64()).or(Some(0.0)));
        }
    }

    // 进程汇总 / 连接概览（beta1.4.3）：低基数标量，直接落
    if let Some(ps) = frame.get("proc_summary") {
        add("proc.count", ps.get("total").and_then(|v| v.as_f64()));
        add("proc.running", ps.get("running").and_then(|v| v.as_f64()));
        add("proc.zombie", ps.get("zombie").and_then(|v| v.as_f64()));
    }
    if let Some(c) = frame.get("net_connections") {
        add("con.established", c.get("established").and_then(|v| v.as_f64()));
        add("con.time_wait", c.get("time_wait").and_then(|v| v.as_f64()));
        add("con.listen", c.get("listen").and_then(|v| v.as_f64()));
    }

    // GPU（beta2.4）：多卡取最大值作汇总指标（占用/显存/温度），功耗取总和
    if let Some(gpus) = frame.get("gpus").and_then(|g| g.as_array()) {
        let max_of = |key: &str| -> Option<f64> {
            gpus.iter()
                .filter_map(|g| g.get(key).and_then(|v| v.as_f64()))
                .fold(None::<f64>, |acc, v| Some(acc.map_or(v, |m: f64| m.max(v))))
        };
        let busy = max_of("busy_percent");
        let memp = max_of("mem_percent");
        let temp = max_of("temp_c");
        let power: Option<f64> = gpus
            .iter()
            .filter_map(|g| g.get("power_w").and_then(|v| v.as_f64()))
            .reduce(|a, b| a + b);
        add("gpu.busy_percent", busy);
        add("gpu.mem_used_percent", memp);
        add("gpu.temp_c", temp);
        add("gpu.power_w", power);
    }
    out
}

pub struct Store {
    inner: Arc<Mutex<StoreInner>>,
    flush_interval: f64,
}

/// 写时聚合窗口：sample_s 秒内的帧值取平均后落一个点，
/// 避免 1 秒采集 × 90 天保留把 samples 表写到亿级行（beta1.4.3 修复）。
struct PendWin {
    start_ts: f64,
    sum: f64,
    n: u64,
}

struct StoreInner {
    conn: Option<Connection>,
    queue: Vec<Point>,
    pub keep_days: f64,
    sample_s: f64,
    pend: HashMap<(String, String), PendWin>,
}

impl Store {
    pub fn new(path: PathBuf, keep_days: f64, flush_interval: f64) -> Arc<Self> {
        let inner = Arc::new(Mutex::new(StoreInner {
            conn: None,
            queue: Vec::new(),
            keep_days: keep_days.max(1.0),
            sample_s: 60.0,
            pend: HashMap::new(),
        }));
        let st = Arc::new(Self {
            inner: inner.clone(),
            flush_interval: flush_interval.max(1.0),
        });
        // 打开数据库（失败只记日志，不影响主流程）
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match Connection::open(&path) {
            Ok(conn) => {
                let _ = conn.execute_batch("PRAGMA journal_mode=WAL");
                let _ = conn.execute_batch(SCHEMA);
                inner.lock().unwrap().conn = Some(conn);
                tracing::info!("历史落盘已启动：{}（保留 {:.0} 天）", path.display(), keep_days);
            }
            Err(e) => {
                tracing::error!("历史数据库打开失败 {}：{}", path.display(), e);
            }
        }
        st
    }

    pub fn record_delivery(&self, channel: &str, title: &str, attempt: usize, ok: bool, detail: &str) {
        let inner = self.inner.lock().unwrap();
        let Some(conn) = &inner.conn else { return };
        let result = (|| -> rusqlite::Result<()> {
            let tx = conn.unchecked_transaction()?;
            tx.execute("INSERT INTO deliveries(ts,channel,title,attempt,ok,detail) VALUES(?,?,?,?,?,?)",
                rusqlite::params![now_ts(), channel, title, attempt, ok, detail])?;
            tx.execute("DELETE FROM deliveries WHERE id <= (SELECT MAX(id)-200 FROM deliveries)", [])?;
            tx.commit()
        })();
        if let Err(e) = result { tracing::warn!("通知投递记录写入失败：{}", e); }
    }

    pub fn deliveries(&self) -> rusqlite::Result<Vec<Value>> {
        let inner = self.inner.lock().unwrap();
        let conn = inner.conn.as_ref().ok_or(rusqlite::Error::InvalidQuery)?;
        let mut stmt = conn.prepare("SELECT ts,channel,title,attempt,ok,detail FROM deliveries ORDER BY id DESC LIMIT 200")?;
        let rows = stmt.query_map([], |r| Ok(serde_json::json!({
            "ts":r.get::<_,f64>(0)?, "channel":r.get::<_,String>(1)?, "title":r.get::<_,String>(2)?,
            "attempt":r.get::<_,i64>(3)?, "ok":r.get::<_,bool>(4)?, "detail":r.get::<_,String>(5)?
        })))?;
        rows.collect()
    }

    pub fn set_keep_days(&self, days: f64) {
        self.inner.lock().unwrap().keep_days = days.max(1.0);
    }

    /// 历史落盘采样间隔（秒）：窗口内同名指标取平均落一个点。
    pub fn set_sample_s(&self, seconds: f64) {
        let mut inner = self.inner.lock().unwrap();
        inner.sample_s = seconds.clamp(5.0, 3600.0);
    }

    pub fn add_frame(&self, host: &str, frame: &Value) {
        let pts = frame_metrics(frame, host);
        if pts.is_empty() {
            return;
        }
        use std::collections::hash_map::Entry;
        let mut inner = self.inner.lock().unwrap();
        let sample_s = inner.sample_s;
        for (ts, host, metric, value) in pts {
            let key = (host, metric);
            let mut flush_pt: Option<Point> = None;
            match inner.pend.entry(key) {
                Entry::Occupied(mut e) => {
                    let (h, m) = e.key().clone();
                    let win = e.get_mut();
                    if ts - win.start_ts < sample_s {
                        win.sum += value;
                        win.n += 1;
                    } else {
                        flush_pt = Some((win.start_ts, h, m, win.sum / win.n as f64));
                        win.start_ts = ts;
                        win.sum = value;
                        win.n = 1;
                    }
                }
                Entry::Vacant(e) => {
                    e.insert(PendWin { start_ts: ts, sum: value, n: 1 });
                }
            }
            if let Some(p) = flush_pt {
                inner.queue.push(p);
            }
        }
    }

    pub fn add_point(&self, host: &str, metric: &str, value: f64, ts: Option<f64>) {
        self.inner.lock().unwrap().queue.push((
            ts.unwrap_or_else(now_ts),
            host.to_string(),
            metric.to_string(),
            value,
        ));
    }

    /// 启动后台 flush / 清理循环。
    pub fn start(self: &Arc<Self>) {
        let st = self.clone();
        tokio::spawn(async move {
            let mut last_cleanup = 0.0f64;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs_f64(st.flush_interval)).await;
                st.flush();
                let now = now_ts();
                if now - last_cleanup >= 3600.0 {
                    st.cleanup();
                    last_cleanup = now;
                }
            }
        });
    }

    pub fn flush(&self) {
        let mut inner = self.inner.lock().unwrap();
        // 断连/停帧时窗口不会自然收口：超过 2×sample_s 的残留窗口直接落均值
        let stale = now_ts() - inner.sample_s * 2.0;
        let StoreInner { pend, queue, .. } = &mut *inner;
        pend.retain(|(host, metric), win| {
            if win.start_ts < stale {
                queue.push((win.start_ts, host.clone(), metric.clone(), win.sum / win.n as f64));
                false
            } else {
                true
            }
        });
        if inner.queue.is_empty() {
            return;
        }
        // 先把队列摘出来，再拿 conn 的不可变引用，避开双重借用
        let n = inner.queue.len().min(5000);
        let batch: Vec<Point> = inner.queue.drain(..n).collect();
        let Some(conn) = &inner.conn else { return };
        let result = (|| -> rusqlite::Result<()> {
            let tx = conn.unchecked_transaction()?;
            {
                let mut stmt =
                    tx.prepare("INSERT INTO samples (ts, host, metric, value) VALUES (?,?,?,?)")?;
                for (ts, host, metric, value) in &batch {
                    stmt.execute(rusqlite::params![ts, host, metric, value])?;
                }
            }
            tx.commit()
        })();
        if let Err(e) = result {
            tracing::warn!("批量写入失败（{} 行丢弃）：{}", batch.len(), e);
        }
    }

    fn cleanup(&self) {
        let inner = self.inner.lock().unwrap();
        let Some(conn) = &inner.conn else { return };
        let cutoff = now_ts() - inner.keep_days * 86400.0;
        match conn.execute("DELETE FROM samples WHERE ts < ?1", rusqlite::params![cutoff]) {
            Ok(n) if n > 0 => {
                tracing::info!("已清理 {} 行过期历史（保留 {:.0} 天）", n, inner.keep_days);
            }
            Err(e) => tracing::warn!("清理过期历史失败：{}", e),
            _ => {}
        }
    }

    /// 返回 [[ts, value], ...]，按时间范围分桶取平均，桶数不超过 max_points。
    pub fn query(&self, host: &str, metric: &str, from_ts: f64, to_ts: f64, max_points: i64) -> Vec<[f64; 2]> {
        let inner = self.inner.lock().unwrap();
        let Some(conn) = &inner.conn else { return vec![] };
        let to_ts = if to_ts > 0.0 { to_ts } else { now_ts() };
        let span = (to_ts - from_ts).max(1.0);
        let nb = max_points.clamp(1, 4000) as usize;
        let bucket = span / nb as f64;
        let mut stmt = match conn.prepare(
            "SELECT ts, value FROM samples WHERE host=?1 AND metric=?2 AND ts>=?3 AND ts<=?4 ORDER BY ts",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let rows = stmt.query_map(rusqlite::params![host, metric, from_ts, to_ts], |r| {
            Ok((r.get::<_, f64>(0)?, r.get::<_, Option<f64>>(1)?))
        });
        let mut buckets: HashMap<usize, (f64, u64)> = HashMap::new();
        if let Ok(rows) = rows {
            for r in rows.flatten() {
                let (ts, v) = r;
                let Some(v) = v else { continue };
                let b = (((ts - from_ts) / bucket) as usize).min(nb - 1);
                let e = buckets.entry(b).or_insert((0.0, 0));
                e.0 += v;
                e.1 += 1;
            }
        }
        let mut keys: Vec<usize> = buckets.keys().copied().collect();
        keys.sort_unstable();
        keys.into_iter()
            .filter_map(|b| {
                let (total, n) = buckets[&b];
                if n > 0 {
                    Some([r3(from_ts + (b as f64 + 0.5) * bucket), r3(total / n as f64)])
                } else {
                    None
                }
            })
            .collect()
    }
}
