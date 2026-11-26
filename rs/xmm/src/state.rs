//! 应用状态（移植自 server/app.py 的 State）。
//!
//! 与 Python 版的差异：
//! - 采集器回调不再是闭包，而是走 tokio mpsc 事件通道（CollectorEvent），
//!   由一个 dispatcher 任务统一处理，避免 Arc 循环引用；
//! - 配置放在 Arc<RwLock<Value>> 里，notify/probe 的"动态读配置"闭包直接捕获它。

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use crate::auth::TokenStore;
use crate::config::CONN_FIELDS;
use crate::history::History;
use crate::notify::{alert_key, Notify};
use crate::probes::{Probe, ProbeManager};
use crate::store::Store;
use crate::ws::Hub;
use crate::{alerts, now_ts, VERSION};

// ---------------------------------------------------------------- 事件通道

pub enum CollectorEvent {
    Frame { name: String, frame: Value },
    Status { name: String, status: String },
    Probe { probe: Probe, up: bool, detail: String },
}

pub type EventTx = tokio::sync::mpsc::UnboundedSender<CollectorEvent>;
pub type EventRx = tokio::sync::mpsc::UnboundedReceiver<CollectorEvent>;

// ---------------------------------------------------------------- 采集器句柄

pub struct CollectorHandle {
    pub status: Arc<RwLock<String>>,
    pub latest: Arc<RwLock<Option<Value>>>,
    pub task: tokio::task::JoinHandle<()>,
}

impl CollectorHandle {
    pub fn stop(&self) {
        self.task.abort();
    }
}

// ---------------------------------------------------------------- State

pub struct State {
    pub cfg: Arc<RwLock<Value>>,
    pub config_edit: Mutex<()>,
    pub cfg_path: PathBuf,
    pub data_dir: PathBuf,
    pub desktop_token: String,
    pub boot_ts: f64,
    pub hub: Hub,
    pub tokens: TokenStore,
    pub collectors: Mutex<HashMap<String, CollectorHandle>>,
    pub histories: Mutex<HashMap<String, History>>,
    pub last_alerts: Mutex<HashMap<String, Vec<Value>>>,
    pub alert_active: Mutex<HashMap<String, HashMap<String, (Value, f64)>>>,
    resource_rules: RwLock<Vec<alerts::ResourceRule>>,
    resource_trackers: Mutex<HashMap<String, alerts::ResourceTracker>>,
    pub store: Arc<Store>,
    pub notify: Arc<Notify>,
    pub probe_mgr: Arc<ProbeManager>,
    pub event_tx: EventTx,
}

impl State {
    pub fn new(cfg: Value, cfg_path: &Path, data_dir: Option<&Path>) -> (Arc<Self>, EventRx) {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let data_dir = data_dir
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| cfg_path.parent().unwrap_or(Path::new(".")).to_path_buf());
        let cfg = Arc::new(RwLock::new(cfg));

        let keep_days = cfg
            .read()
            .unwrap()
            .get("history")
            .and_then(|h| h.get("keep_days"))
            .and_then(|v| v.as_f64())
            .unwrap_or(90.0);
        let store = Store::new(data_dir.join("history.db"), keep_days, 5.0);

        let cfg_for_notify = cfg.clone();
        let notify = Notify::new(Arc::new(move || cfg_for_notify.read().unwrap().clone()), store.clone());

        let hub = Hub::new();

        let cfg_for_probes = cfg.clone();
        let store_for_points = store.clone();
        let tx_for_events = event_tx.clone();
        let probe_mgr = ProbeManager::new(
            Arc::new(move || {
                cfg_for_probes
                    .read()
                    .unwrap()
                    .get("probes")
                    .cloned()
                    .unwrap_or_else(|| json!({}))
            }),
            // 落盘拨测结果（文档口径：probe.<name>.up / probe.<name>.ms）
            Arc::new(move |name, up, ms| {
                store_for_points.add_point("拨测", &format!("probe.{}.up", name), up, None);
                if let Some(ms) = ms {
                    store_for_points.add_point("拨测", &format!("probe.{}.ms", name), ms, None);
                }
            }),
            // 事件走 dispatcher 统一做异步推送/广播
            Arc::new(move |probe, up, detail| {
                let _ = tx_for_events.send(CollectorEvent::Probe { probe, up, detail });
            }),
        );

        let resource_rules = alerts::parse_rules(&cfg.read().unwrap()["alerts"]["resource_rules"])
            .unwrap_or_else(|e| { tracing::warn!("资源规则未加载：{}", e); Vec::new() });
        let st = Arc::new(Self {
            cfg,
            config_edit: Mutex::new(()),
            cfg_path: cfg_path.to_path_buf(),
            data_dir,
            desktop_token: std::env::var("XMM_DESKTOP_TOKEN").unwrap_or_default(),
            boot_ts: now_ts(),
            hub,
            tokens: TokenStore::new(24.0),
            collectors: Mutex::new(HashMap::new()),
            histories: Mutex::new(HashMap::new()),
            last_alerts: Mutex::new(HashMap::new()),
            alert_active: Mutex::new(HashMap::new()),
            resource_rules: RwLock::new(resource_rules),
            resource_trackers: Mutex::new(HashMap::new()),
            store,
            notify,
            probe_mgr,
            event_tx,
        });
        // 按配置初始化采集器（不启动）
        {
            let targets = st.cfg.read().unwrap().get("targets").cloned().unwrap_or_else(|| json!({}));
            let (capacity, seconds) = st.history_window();
            if let Some(obj) = targets.as_object() {
                for (name, tcfg) in obj {
                    let h = st.build_collector(name, tcfg);
                    st.collectors.lock().unwrap().insert(name.clone(), h);
                    st.histories.lock().unwrap().insert(name.clone(), History::new(capacity, seconds));
                    st.statuses_insert(name, "connecting");
                    st.last_alerts.lock().unwrap().insert(name.clone(), vec![]);
                }
            }
        }
        (st, event_rx)
    }

    // ------------------------------------------------------------ 内部工具

    fn history_window(&self) -> (usize, f64) {
        let cfg = self.cfg.read().unwrap();
        let interval = cfg
            .get("collect")
            .and_then(|c| c.get("interval_s"))
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0)
            .max(0.5);
        let minutes = cfg
            .get("collect")
            .and_then(|c| c.get("history_minutes"))
            .and_then(|v| v.as_f64())
            .unwrap_or(5.0);
        (((minutes * 60.0 / interval).ceil() as usize).max(1), minutes * 60.0)
    }

    fn statuses_insert(&self, name: &str, s: &str) {
        if let Some(h) = self.collectors.lock().unwrap().get_mut(name) {
            *h.status.write().unwrap() = s.to_string();
        }
    }

    fn build_collector(&self, name: &str, tcfg: &Value) -> CollectorHandle {
        let cfg = self.cfg.read().unwrap();
        let mock = cfg.get("_mock").and_then(|v| v.as_bool()).unwrap_or(false);
        let dropout = cfg.get("_mock_dropout").and_then(|v| v.as_bool()).unwrap_or(false);
        let collect_cfg = cfg.get("collect").cloned().unwrap_or_else(|| json!({}));
        drop(cfg);

        if mock {
            let mc = Arc::new(crate::mock::MockCollector::new(name, &collect_cfg, dropout));
            let status = Arc::new(RwLock::new("connecting".to_string()));
            let latest = Arc::new(RwLock::new(None));
            // mock 的 status/latest 由 State 直接管理（帧事件里更新）
            let task = mc.spawn(self.event_tx.clone());
            CollectorHandle { status, latest, task }
        } else {
            let hc = Arc::new(crate::ssh::HostCollector::new(
                name, tcfg, &collect_cfg, Some(self.data_dir.clone()),
            ));
            let status = hc.status.clone();
            let latest = hc.latest.clone();
            let task = hc.spawn(self.event_tx.clone());
            CollectorHandle { status, latest, task }
        }
    }

    // ------------------------------------------------------------ dispatcher

    /// 启动事件分发任务（消费采集器/拨测事件）。
    pub fn spawn_dispatcher(self: &Arc<Self>, mut rx: EventRx) {
        let st = self.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                match ev {
                    CollectorEvent::Frame { name, frame } => st.on_frame(&name, frame).await,
                    CollectorEvent::Status { name, status } => st.on_status(&name, &status).await,
                    CollectorEvent::Probe { probe, up, detail } => {
                        st.on_probe_event(probe, up, detail).await
                    }
                }
            }
        });
    }

    async fn on_frame(&self, name: &str, frame: Value) {
        // 历史只留曲线需要的字段；docker 容器列表体积大，只随 latest 推送
        let mut hist_frame = frame.clone();
        if let Some(o) = hist_frame.as_object_mut() {
            o.remove("docker");
        }
        if let Some(h) = self.histories.lock().unwrap().get_mut(name) {
            h.push(hist_frame);
        }
        // mock 采集器不走 ssh::HostCollector，latest 由这里维护。
        // 先读 cfg 再拿 collectors 锁，避免两把锁嵌套（与 meta() 保持同一顺序）。
        let is_mock = self
            .cfg
            .read()
            .unwrap()
            .get("_mock")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if is_mock {
            if let Some(h) = self.collectors.lock().unwrap().get(name) {
                *h.latest.write().unwrap() = Some(frame.clone());
            }
        }
        self.store.add_frame(name, &frame);
        let (thresholds, interval) = {
            let cfg = self.cfg.read().unwrap();
            (cfg["alerts"].clone(), cfg["collect"]["interval_s"].as_f64().unwrap_or(1.0))
        };
        let mut items = alerts::evaluate(&frame, &thresholds);
        {
            let rules = self.resource_rules.read().unwrap();
            let mut trackers = self.resource_trackers.lock().unwrap();
            items.extend(trackers.entry(name.into()).or_default()
                .update(name, &frame, &rules, now_ts(), (interval * 3.0).max(5.0)));
        }
        let was_active = self.last_alerts.lock().unwrap().insert(name.into(), items.clone())
            .is_some_and(|previous| !previous.is_empty());
        self.alert_edges(name, &items).await;
        self.hub.broadcast(&json!({"type": "metrics", "target": name, "data": frame, "alerts":items}));
        if was_active || !items.is_empty() {
            self.hub.broadcast(&json!({"type":"alert", "target":name, "items":items}));
        }
    }

    /// 告警边沿检测：新增的推告警，消失的推恢复。
    async fn alert_edges(&self, name: &str, items: &[Value]) {
        let mut cur: HashMap<String, (Value, f64)> = HashMap::new();
        for it in items {
            cur.insert(alert_key(name, it), (it.clone(), it["since"].as_f64().unwrap_or_else(now_ts)));
        }
        // 守卫在块内结束，绝不能跨 await（否则 dispatcher future 非 Send）
        let prev = {
            let mut active = self.alert_active.lock().unwrap();
            let prev = active.entry(name.to_string()).or_default().clone();
            for (key, (_, since)) in &mut cur {
                if let Some((_, first_seen)) = prev.get(key) {
                    *since = *first_seen;
                }
            }
            *active.entry(name.to_string()).or_default() = cur.clone();
            prev
        };
        for (key, (item, since)) in &cur {
            if !prev.contains_key(key) {
                let mut item = item.clone();
                item.as_object_mut().unwrap().insert("since".into(), json!(since));
                self.notify.alert(name, &item, Some(*since)).await;
            }
        }
        for (key, (item, since)) in &prev {
            if !cur.contains_key(key) {
                self.notify.recovered(name, item, Some(*since)).await;
            }
        }
    }

    async fn on_probe_event(&self, probe: Probe, up: bool, detail: String) {
        let item = json!({
            "kind": "probe", "zone": probe.name,
            "text": format!("{} 失败（{}）", probe.name, detail),
        });
        let since = probe.down_since.unwrap_or_else(now_ts);
        if up {
            self.notify.recovered("拨测", &item, Some(since)).await;
        } else {
            self.notify.alert("拨测", &item, Some(since)).await;
        }
        self.hub.broadcast(&json!({"type": "probe", "probes": self.probe_mgr.status()}));
    }

    async fn on_status(&self, name: &str, status: &str) {
        if status != "online" {
            if let Some(t) = self.resource_trackers.lock().unwrap().get_mut(name) { t.interrupted(); }
        }
        self.statuses_insert(name, status);
        self.hub.broadcast(&json!({"type": "status", "target": name, "state": status}));
    }

    // ------------------------------------------------------------ 目标 CRUD

    pub fn target_names(&self) -> Vec<String> {
        self.cfg
            .read()
            .unwrap()
            .get("targets")
            .and_then(|t| t.as_object())
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn meta(&self, name: &str) -> Value {
        // 两把锁分开拿，绝不嵌套（否则和 dispatcher 的加锁顺序相反会死锁）
        let t = {
            let cfg = self.cfg.read().unwrap();
            cfg.get("targets").and_then(|ts| ts.get(name)).cloned().unwrap_or_else(|| json!({}))
        };
        let status = self
            .collectors
            .lock()
            .unwrap()
            .get(name)
            .map(|h| h.status.read().unwrap().clone())
            .unwrap_or_else(|| "offline".into());
        json!({
            "name": name,
            "host": t.get("host").and_then(|v| v.as_str()).unwrap_or(""),
            "port": t.get("port").and_then(|v| v.as_i64()).unwrap_or(22),
            "username": t.get("username").and_then(|v| v.as_str()).unwrap_or(""),
            "label": t.get("label").and_then(|v| v.as_str()).unwrap_or(name),
            "note": t.get("note").and_then(|v| v.as_str()).unwrap_or(""),
            "has_password": t.get("password").and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false),
            "private_key": t.get("private_key").and_then(|v| v.as_str()).unwrap_or(""),
            "host_key_policy": t.get("host_key_policy").and_then(|v| v.as_str()).unwrap_or("autoadd"),
            "status": status,
        })
    }

    pub fn targets_payload(&self) -> Vec<Value> {
        self.target_names().iter().map(|n| self.meta(n)).collect()
    }

    /// Apply runtime changes only after the complete configuration is durable.
    pub fn apply_config(&self, old: &Value, new: &Value) {
        if old["alerts"]["resource_rules"] != new["alerts"]["resource_rules"] {
            if let Ok(rules) = alerts::parse_rules(&new["alerts"]["resource_rules"]) {
                *self.resource_rules.write().unwrap() = rules;
            }
        }
        let old_targets = old["targets"].as_object().unwrap();
        let targets = new["targets"].as_object().unwrap();
        let interval_changed = old["collect"]["interval_s"].as_f64() != new["collect"]["interval_s"].as_f64();
        let (maxlen, seconds) = self.history_window();
        for name in old_targets.keys().filter(|name| !targets.contains_key(*name)) {
            if let Some(handle) = self.collectors.lock().unwrap().remove(name) {
                handle.stop();
            }
            self.histories.lock().unwrap().remove(name);
            self.last_alerts.lock().unwrap().remove(name);
            self.alert_active.lock().unwrap().remove(name);
            self.resource_trackers.lock().unwrap().remove(name);
        }
        for (name, target) in targets {
            let reconnect = old_targets.get(name).map_or(true, |before| {
                CONN_FIELDS.iter().any(|key| before.get(*key) != target.get(*key))
            });
            if interval_changed || reconnect {
                if let Some(handle) = self.collectors.lock().unwrap().remove(name) {
                    handle.stop();
                }
                let handle = self.build_collector(name, target);
                self.collectors.lock().unwrap().insert(name.clone(), handle);
                self.histories.lock().unwrap().entry(name.clone()).or_insert_with(|| History::new(maxlen, seconds));
            }
        }
        if old["collect"] != new["collect"] {
            for history in self.histories.lock().unwrap().values_mut() {
                history.resize(maxlen, seconds);
            }
        }
        self.store.set_keep_days(new["history"]["keep_days"].as_f64().unwrap_or(90.0));
        if old["probes"] != new["probes"] {
            self.probe_mgr.restart();
            self.hub.broadcast(&json!({"type": "probe", "probes": self.probe_mgr.status()}));
        }
        if old_targets != targets || old["collect"] != new["collect"] || old["alerts"] != new["alerts"] {
            self.hub.broadcast(&json!({"type": "targets", "targets": self.targets_payload()}));
        }
    }

    // ------------------------------------------------------------ 快照

    pub fn snapshot_payload(&self) -> Value {
        // 先算 target_info：meta() 内部会取 collectors 锁，不能和下面的
        // collectors 守卫同时持有（std::sync::Mutex 不可重入，会自锁）。
        let target_info = self.targets_payload();
        // 三把锁只在这个块内持有，出块即释放；后续构造 JSON 不再持任何锁
        let (data, names) = {
            let mut data = Map::new();
            let collectors = self.collectors.lock().unwrap();
            let histories = self.histories.lock().unwrap();
            let last_alerts = self.last_alerts.lock().unwrap();
            for (name, h) in collectors.iter() {
                data.insert(name.clone(), json!({
                    "status": h.status.read().unwrap().clone(),
                    "latest": h.latest.read().unwrap().clone(),
                    "history": histories.get(name).map(|h| h.list()).unwrap_or_default(),
                    "alerts": last_alerts.get(name).cloned().unwrap_or_default(),
                }));
            }
            let names: Vec<String> = collectors.keys().cloned().collect();
            (data, names)
        };
        let (thresholds, interval_s, mock) = {
            let cfg = self.cfg.read().unwrap();
            (
                cfg.get("alerts").cloned().unwrap_or_else(|| json!({})),
                cfg.get("collect").and_then(|c| c.get("interval_s")).cloned().unwrap_or(json!(1)),
                cfg.get("_mock").and_then(|v| v.as_bool()).unwrap_or(false),
            )
        };
        json!({
            "version": VERSION,
            "targets": names,
            "target_info": target_info,
            "thresholds": thresholds,
            "interval_s": interval_s,
            "history_limit": self.history_window().0,
            "desktop": !self.desktop_token.is_empty(),
            "mock": mock,
            "probes": self.probe_mgr.status(),
            "notify": self.notify.status(),
            "data": Value::Object(data),
        })
    }

    /// 启动后台任务：dispatcher 已在外层启动；这里起历史落盘、拨测、心跳。
    pub fn start_background(self: &Arc<Self>) {
        self.store.start();
        self.probe_mgr.restart();
        let st = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            loop {
                let minutes = st
                    .cfg
                    .read()
                    .unwrap()
                    .get("notify")
                    .and_then(|n| n.get("heartbeat_minutes"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                if minutes <= 0.0 {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    continue;
                }
                st.notify.heartbeat().await;
                tokio::time::sleep(std::time::Duration::from_secs_f64(minutes * 60.0)).await;
            }
        });
    }
}
