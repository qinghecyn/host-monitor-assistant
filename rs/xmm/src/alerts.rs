//! 告警判定：温度过高 / 内存可用过低 / 根分区可用不足（移植自 server/alerts.py）。
//!
//! 每一项独立容错：任何一个字段缺失都不影响其余判定。

use serde_json::{json, Value};

use crate::{r1, r2};

fn f(v: Option<&Value>, default: f64) -> f64 {
    v.and_then(|v| v.as_f64()).unwrap_or(default)
}

pub fn evaluate(frame: &Value, thresholds: &Value) -> Vec<Value> {
    let mut items = Vec::new();
    if !frame.is_object() {
        return items;
    }

    // 温度
    let t_th = f(thresholds.get("temp_c"), 70.0);
    if let Some(temps) = frame.get("temperatures").and_then(|t| t.as_array()) {
        for t in temps {
            let Some(v) = t.get("temp_c").and_then(|v| v.as_f64()) else { continue };
            if v > t_th {
                let zone = t
                    .get("type")
                    .or_else(|| t.get("zone"))
                    .and_then(|z| z.as_str())
                    .unwrap_or("?");
                items.push(json!({
                    "kind": "temp", "zone": zone,
                    "value": r1(v), "threshold": t_th, "level": "high",
                    "text": format!("温度过高 {} {:.1}℃ (阈值 {:.0}℃)", t.get("type").and_then(|z| z.as_str()).unwrap_or(""), v, t_th),
                }));
            }
        }
    }

    // 内存
    let m_th = f(thresholds.get("mem_available_percent"), 10.0);
    if let Some(mem) = frame.get("memory") {
        if let Some(v) = mem.get("available_percent").and_then(|v| v.as_f64()) {
            if v < m_th {
                let total_gb = f(mem.get("total_kb"), 0.0) / 1024.0 / 1024.0;
                let avail_gb = f(mem.get("available_kb"), 0.0) / 1024.0 / 1024.0;
                items.push(json!({
                    "kind": "mem", "zone": "",
                    "value": r1(v), "threshold": m_th, "level": "high",
                    "text": format!("内存可用不足 {:.1}%（可用 {:.2}G / 共 {:.2}G）", v, avail_gb, total_gb),
                }));
            }
        }
    }

    // 根分区
    let d_th = f(thresholds.get("root_free_gb"), 1.0);
    if let Some(disks) = frame.get("disks").and_then(|d| d.as_array()) {
        for d in disks {
            if d.get("mount").and_then(|m| m.as_str()) != Some("/") {
                continue;
            }
            let Some(v) = d.get("available_kb").and_then(|v| v.as_f64()) else { continue };
            let gb = v / 1024.0 / 1024.0;
            if gb < d_th {
                items.push(json!({
                    "kind": "disk", "zone": "/",
                    "value": r2(gb), "threshold": d_th, "level": "high",
                    "text": format!("根分区可用不足 {:.0}G（当前 {:.2}G）", d_th, gb),
                }));
            }
        }
    }
    items
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceRule {
    pub host: String,
    pub metric: String,
    pub mount: String,
    pub below: f64,
    pub recover: f64,
    pub seconds: u32,
}

pub fn default_rules() -> Value {
    json!([{"host":"*", "metric":"all", "mount":"*", "below":3, "recover":5, "seconds":30}])
}

pub fn parse_rules(value: &Value) -> Result<Vec<ResourceRule>, String> {
    let rules: Vec<ResourceRule> = serde_json::from_value(value.clone())
        .map_err(|_| "资源规则格式不正确".to_string())?;
    let mut scopes = std::collections::HashSet::new();
    for rule in &rules {
        if !(rule.host == "*" || crate::config::valid_name(&rule.host))
            || !["all", "cpu", "memory", "gpu", "disk"].contains(&rule.metric.as_str())
            || !(rule.mount == "*" || (rule.metric == "disk" && rule.mount.starts_with('/')))
            || !(0.0..=100.0).contains(&rule.below) || !(rule.below..=100.0).contains(&rule.recover)
            || rule.seconds > 86400 {
            return Err("请检查作用范围、阈值与持续时间（0–86400 秒），恢复比例应大于或等于触发比例".into());
        }
        if !scopes.insert((&rule.host, &rule.metric, &rule.mount)) {
            return Err("同一主机、资源和挂载点只能配置一条规则".into());
        }
    }
    Ok(rules)
}

#[derive(Default)]
pub struct ResourceTracker {
    entries: std::collections::BTreeMap<(String, String), Reading>,
}

struct Reading {
    rule: ResourceRule,
    since: f64,
    last: f64,
    active: bool,
    alert: Value,
}

impl ResourceTracker {
    pub fn interrupted(&mut self) {
        self.entries.retain(|_, r| r.active);
    }

    pub fn update(&mut self, host: &str, frame: &Value, rules: &[ResourceRule], now: f64, gap: f64) -> Vec<Value> {
        let select = |metric: &str, zone: &str| rules.iter().filter(|r|
            (r.host == "*" || r.host == host) && (r.metric == "all" || r.metric == metric)
            && (r.mount == "*" || r.mount == zone)
        ).max_by_key(|r| (r.host != "*", r.metric != "all", r.mount != "*"));

        let mut values = Vec::new();
        if let Some(cpu) = frame["cpu"]["percent"].as_f64() {
            values.push(("cpu", String::new(), "CPU".to_string(), 100.0 - cpu));
        }
        if let Some(mem) = frame["memory"]["available_percent"].as_f64() {
            values.push(("memory", String::new(), "内存".to_string(), mem));
        }
        for (i, gpu) in frame["gpus"].as_array().into_iter().flatten().enumerate() {
            if let Some(busy) = gpu["busy_percent"].as_f64() {
                let zone = format!("{} #{}", gpu["name"].as_str().unwrap_or("GPU"), i + 1);
                values.push(("gpu", zone.clone(), zone, 100.0 - busy));
            }
        }
        for disk in frame["disks"].as_array().into_iter().flatten() {
            if let (Some(zone), Some(total), Some(free)) = (disk["mount"].as_str(), disk["total_kb"].as_f64(), disk["available_kb"].as_f64()) {
                if total > 0.0 { values.push(("disk", zone.into(), format!("磁盘 {}", zone), free * 100.0 / total)); }
            }
        }
        self.entries.retain(|(metric, zone), r| {
            select(metric, zone) == Some(&r.rule) && (r.active || (now-r.last <= gap &&
                values.iter().any(|(m,z,_,_)| *m == metric && z == zone)))
        });
        for (metric, zone, label, free) in values {
            let Some(rule) = select(metric, &zone) else { continue };
            let key = (metric.to_string(), zone.clone());
            if free > rule.below && !(free <= rule.recover && self.entries.get(&key).is_some_and(|r| r.active)) {
                self.entries.remove(&key);
                continue;
            }
            let entry = self.entries.entry(key).or_insert_with(|| Reading {
                rule: rule.clone(), since: now, last: now, active: false, alert: Value::Null,
            });
            entry.last = now;
            entry.active |= now - entry.since >= f64::from(rule.seconds);
            entry.alert = json!({"kind":format!("resource_{}", metric), "zone":zone, "since":entry.since,
                "value":free, "threshold":rule.below, "text":format!("{} 剩余 {:.1}%（触发线 {}%）",label,free,rule.below)});
        }
        self.entries.values().filter(|r| r.active).map(|r| r.alert.clone()).collect()
    }
}

#[cfg(test)]
mod sustained_tests {
    use super::*;

    #[test]
    fn inclusive_trigger_duration_interruption_and_recovery() {
        let rules = parse_rules(&default_rules()).unwrap();
        let mut tracker = ResourceTracker::default();
        let sample = |p| json!({"cpu":{"percent":p}});
        for t in 0..29 {
            assert!(tracker.update("a", &sample(97), &rules, t as f64, 5.0).is_empty());
        }
        assert!(tracker.update("a", &json!({}), &rules, 29.0, 5.0).is_empty());
        for t in 30..60 {
            assert!(tracker.update("a", &sample(97), &rules, t as f64, 5.0).is_empty());
        }
        assert_eq!(tracker.update("a", &sample(97), &rules, 60.0, 5.0)[0]["since"],30.0);
        assert_eq!(tracker.update("a", &json!({}), &rules, 61.0, 5.0).len(),1);
        assert_eq!(tracker.update("a", &sample(96), &rules, 62.0, 5.0).len(),1);
        assert!(tracker.update("a", &sample(94), &rules, 63.0, 5.0).is_empty());
    }

    #[test]
    fn host_and_mount_override_applies_only_to_matching_disk() {
        let mut config = default_rules();
        config.as_array_mut().unwrap().push(json!({"host":"a","metric":"disk","mount":"/data","below":10,"recover":15,"seconds":0}));
        let rules = parse_rules(&config).unwrap();
        let frame = json!({"disks":[{"mount":"/data","total_kb":100,"available_kb":8},{"mount":"/","total_kb":100,"available_kb":8}]});
        let mut tracker = ResourceTracker::default();
        let items = tracker.update("a",&frame,&rules,0.0,5.0);
        assert_eq!(items.len(),1);
        assert_eq!(items[0]["zone"],"/data");
        assert!(ResourceTracker::default().update("b",&frame,&rules,0.0,5.0).is_empty());
    }
}
