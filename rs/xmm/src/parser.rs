//! 把 collect.sh 的行协议解析为结构化帧（移植自 collector/parser.py）。
//!
//! 设计要点与 Python 版一致：
//! - 每一段独立容错，一段损坏只影响该字段（null/[]），绝不丢整帧；
//! - CPU 使用率、网速由本地两次采样差值计算；远端只吐累计值；
//! - 计数器回绕/复位（重连、网卡重置）时返回 null，避免负速率尖峰；
//! - NET 段解析全部网卡（含 operstate），并给每帧标出主网卡（primary）。

use serde_json::{json, Map, Value};
use std::collections::HashMap;

use crate::{now_ts, r1};

/// 主网卡候选前缀（对应 Python 的 _REAL_IFACE_RE）：物理口 / 无线 / 网桥 /
/// VLAN / 拨号口。tunl0、docker0、veth* 这类虚拟口不参与"主网卡"评选。
const REAL_IFACE_PREFIXES: [&str; 9] =
    ["eth", "en", "wl", "br-", "lan", "wan", "bond", "pppoe", "ap"];

fn is_real_iface(iface: &str) -> bool {
    let l = iface.to_ascii_lowercase();
    REAL_IFACE_PREFIXES.iter().any(|p| l.starts_with(p))
}

fn total_kb(n: &Value) -> f64 {
    n.get("rx_total_kb").and_then(|v| v.as_f64()).unwrap_or(0.0)
        + n.get("tx_total_kb").and_then(|v| v.as_f64()).unwrap_or(0.0)
}

/// 与前端 pickPrimaryNet 同口径：优先 up 状态的真实接口，再按累计流量最大。
/// 返回主网卡在数组中的下标。
pub fn pick_primary_network(nets: &[Value]) -> Option<usize> {
    if nets.is_empty() {
        return None;
    }
    let not_down = |n: &Value| {
        n.get("state")
            .and_then(|s| s.as_str())
            .map(|s| !s.eq_ignore_ascii_case("down"))
            .unwrap_or(true)
    };
    let up: Vec<usize> = (0..nets.len()).filter(|&i| not_down(&nets[i])).collect();
    let up = if up.is_empty() {
        (0..nets.len()).collect::<Vec<_>>()
    } else {
        up
    };
    let real: Vec<usize> = up
        .iter()
        .copied()
        .filter(|&i| {
            nets[i]
                .get("iface")
                .and_then(|s| s.as_str())
                .map(is_real_iface)
                .unwrap_or(false)
        })
        .collect();
    let pool = if real.is_empty() { up } else { real };
    pool.into_iter()
        .max_by(|&a, &b| total_kb(&nets[a]).partial_cmp(&total_kb(&nets[b])).unwrap_or(std::cmp::Ordering::Equal))
}

fn to_int(s: &str) -> Option<i64> {
    let t = s.trim();
    if let Ok(v) = t.parse::<i64>() {
        return Some(v);
    }
    // 兼容 "123.0" 形式（%.0f 输出的超大数 i64 溢出时兜底）
    t.parse::<f64>().ok().map(|v| v as i64)
}

fn to_float(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok()
}

/// 每条 SSH 连接一个实例：持有上次采样，用于差值计算。
pub struct StreamParser {
    prev_ts: Option<f64>,
    prev_cpu: HashMap<String, [i64; 8]>, // "t" 或核号 -> 8 项累计 jiffies
    prev_net: HashMap<String, (i64, i64)>, // 网卡名 -> (rx_bytes, tx_bytes)
    prev_disk: HashMap<String, (i64, i64)>, // 挂载点 -> (累计读 KB, 累计写 KB)
}

impl Default for StreamParser {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamParser {
    pub fn new() -> Self {
        Self {
            prev_ts: None,
            prev_cpu: HashMap::new(),
            prev_net: HashMap::new(),
            prev_disk: HashMap::new(),
        }
    }

    /// 解析一行；空行返回 None。永不 panic。
    pub fn parse(&mut self, line: &str) -> Option<Value> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let mut f = Map::new();
        f.insert("ts".into(), json!(now_ts()));
        f.insert("status".into(), json!("online"));
        f.insert("os".into(), Value::Null);
        f.insert("uptime_s".into(), Value::Null);
        f.insert("cpu".into(), Value::Null);
        f.insert("memory".into(), Value::Null);
        f.insert("temperatures".into(), json!([]));
        f.insert("fans".into(), json!([]));
        f.insert("gpus".into(), json!([]));
        f.insert("disks".into(), json!([]));
        f.insert("network".into(), json!([]));
        f.insert("docker".into(), Value::Null);
        f.insert("processes".into(), json!([]));
        f.insert("proc_summary".into(), Value::Null);
        f.insert("net_connections".into(), Value::Null);

        let mut ts: Option<f64> = None;
        let mut cpu_raw: Option<&str> = None;
        let mut net_raw: Option<&str> = None;
        let mut disk_raw: Option<&str> = None;
        let mut load: Option<Vec<Option<f64>>> = None;
        let mut dkp_raw: Option<&str> = None;
        let mut dkst_raw: Option<&str> = None;
        let mut ptop_raw: Option<&str> = None;
        let mut psum_raw: Option<&str> = None;
        let mut con_raw: Option<&str> = None;

        for seg in line.split('|') {
            let Some(eq) = seg.find('=') else { continue };
            let key = &seg[..eq];
            let val = &seg[eq + 1..];
            match key {
                "T" => ts = to_float(val),
                "U" => {
                    if let Some(v) = to_int(val) {
                        f.insert("uptime_s".into(), json!(v));
                    }
                }
                "L" => {
                    let parts: Vec<Option<f64>> =
                        val.split(',').take(3).map(to_float).collect();
                    load = if parts.len() == 3 { Some(parts) } else { None };
                }
                "CPU" => cpu_raw = Some(val),
                "MEM" => {
                    if let Some(m) = parse_mem(val) {
                        f.insert("memory".into(), m);
                    }
                }
                "DSK" => disk_raw = Some(val),
                "NET" => net_raw = Some(val),
                "TMP" => {
                    f.insert("temperatures".into(), parse_temps(val));
                }
                "FAN" => {
                    f.insert("fans".into(), parse_fans(val));
                }
                "GPU" => {
                    f.insert("gpus".into(), parse_gpu(val));
                }
                "OS" => {
                    let name = val.trim();
                    f.insert(
                        "os".into(),
                        if name.is_empty() {
                            Value::Null
                        } else {
                            json!({"pretty_name": name})
                        },
                    );
                }
                "DKS" => {
                    if let Some(d) = parse_docker_state(val) {
                        f.insert("docker".into(), d);
                    }
                }
                "DKP" => dkp_raw = Some(val),
                "DKST" => dkst_raw = Some(val),
                "PTOP" => ptop_raw = Some(val),
                "PSUM" => psum_raw = Some(val),
                "CON" => con_raw = Some(val),
                _ => {}
            }
        }

        f.insert("cpu".into(), self.parse_cpu(cpu_raw, load, ts));
        f.insert("network".into(), self.parse_net(net_raw, ts));
        f.insert("disks".into(), self.parse_disks(disk_raw, ts));

        // 进程 / 连接（beta1.4.3）：mem_percent 用同帧 MEM 的总量换算，缺 MEM 就留空
        if let Some(raw) = psum_raw {
            f.insert("proc_summary".into(), parse_proc_summary(raw));
        }
        if let Some(raw) = ptop_raw {
            let mem_total_kb = f
                .get("memory")
                .and_then(|m| m.get("total_kb"))
                .and_then(|v| v.as_f64());
            f.insert("processes".into(), parse_proc_top(raw, mem_total_kb));
        }
        if let Some(raw) = con_raw {
            f.insert("net_connections".into(), parse_con(raw));
        }

        // Docker 容器列表合并（daemon=ok 时）
        let daemon_ok = f
            .get("docker")
            .and_then(|d| d.get("daemon"))
            .and_then(|d| d.as_str())
            == Some("ok");
        if daemon_ok {
            let ps = dkp_raw.map(parse_docker_ps).unwrap_or_default();
            let stats = dkst_raw.map(parse_docker_stats).unwrap_or_default();
            let merged = merge_docker(ps, &stats);
            if let Some(d) = f.get_mut("docker").and_then(|d| d.as_object_mut()) {
                d.insert("containers".into(), Value::Array(merged));
            }
        }

        f.insert("ts".into(), json!(ts.unwrap_or_else(now_ts)));
        self.prev_ts = ts;
        Some(Value::Object(f))
    }

    // ---------- 差值计算 ----------

    fn cpu_pct(prev: &[i64; 8], cur: &[i64; 8]) -> Option<f64> {
        let d: Vec<i64> = cur.iter().zip(prev.iter()).map(|(c, p)| c - p).collect();
        if d.iter().min().copied().unwrap_or(0) < 0 {
            return None; // 计数器复位
        }
        let total: i64 = d.iter().sum();
        if total <= 0 {
            return None;
        }
        let idle = d[3] + d[4]; // idle + iowait
        let pct = 100.0 * (1.0 - idle as f64 / total as f64);
        Some(r1(pct.clamp(0.0, 100.0)))
    }

    fn parse_cpu(
        &mut self,
        raw: Option<&str>,
        load: Option<Vec<Option<f64>>>,
        ts: Option<f64>,
    ) -> Value {
        if raw.is_none() && load.is_none() {
            return Value::Null;
        }
        let mut cpu = Map::new();
        cpu.insert("percent".into(), Value::Null);
        cpu.insert("cores".into(), Value::Null);
        cpu.insert("per_core".into(), json!([]));
        cpu.insert(
            "load".into(),
            load.map(|v| Value::Array(v.into_iter().map(|x| json!(x)).collect()))
                .unwrap_or(Value::Null),
        );

        if let Some(raw) = raw {
            let mut cur: HashMap<String, [i64; 8]> = HashMap::new();
            let mut total: Option<[i64; 8]> = None;
            for ent in raw.split(';') {
                let Some(colon) = ent.find(':') else { continue };
                let k = &ent[..colon];
                let nums = &ent[colon + 1..];
                let mut parts = [0i64; 8];
                for (i, x) in nums.split(',').take(8).enumerate() {
                    parts[i] = to_int(x).unwrap_or(0);
                }
                if k == "t" {
                    total = Some(parts);
                } else {
                    cur.insert(k.to_string(), parts);
                }
            }
            if let Some(total) = total {
                if let (Some(prev), Some(t), Some(pt)) =
                    (self.prev_cpu.get("t").copied(), ts, self.prev_ts)
                {
                    if t > pt {
                        if let Some(p) = Self::cpu_pct(&prev, &total) {
                            cpu.insert("percent".into(), json!(p));
                        }
                    }
                }
                self.prev_cpu.insert("t".into(), total);
            }
            if !cur.is_empty() {
                let mut keys: Vec<&String> = cur.keys().collect();
                keys.sort_by_key(|k| k.parse::<i64>().unwrap_or(0));
                let mut per_core = Vec::new();
                for k in keys {
                    let mut p = Value::Null;
                    if let (Some(prev), Some(t), Some(pt)) =
                        (self.prev_cpu.get(k).copied(), ts, self.prev_ts)
                    {
                        if t > pt {
                            if let Some(v) = Self::cpu_pct(&prev, &cur[k]) {
                                p = json!(v);
                            }
                        }
                    }
                    per_core.push(p);
                }
                cpu.insert("per_core".into(), Value::Array(per_core));
                for (k, v) in &cur {
                    self.prev_cpu.insert(k.clone(), *v);
                }
            }
            if !cur.is_empty() {
                cpu.insert("cores".into(), json!(cur.len()));
            }
        }
        Value::Object(cpu)
    }

    fn parse_net(&mut self, raw: Option<&str>, ts: Option<f64>) -> Value {
        let Some(raw) = raw else { return json!([]) };
        let mut cur: HashMap<String, (i64, i64)> = HashMap::new();
        let mut states: HashMap<String, Option<String>> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for ent in raw.split(';') {
            if ent.is_empty() || !ent.contains(',') {
                continue;
            }
            let mut it = ent.splitn(4, ',');
            let iface = it.next().unwrap_or("").trim().to_string();
            let rx = it.next().and_then(to_int);
            let tx = it.next().and_then(to_int);
            let state = it.next().and_then(|s| {
                let t = s.trim().to_ascii_lowercase();
                if t.is_empty() { None } else { Some(t) }
            });
            let (Some(rx), Some(tx)) = (rx, tx) else { continue };
            // 计数器不可能是负数；旧脚本 %d 溢出/坏数据时直接丢掉
            if iface.is_empty() || rx < 0 || tx < 0 {
                continue;
            }
            cur.insert(iface.clone(), (rx, tx));
            states.insert(iface.clone(), state);
            order.push(iface);
        }
        let prev_ts = self.prev_ts;
        let mut out: Vec<Value> = Vec::new();
        for iface in &order {
            let (rx, tx) = cur[iface];
            let mut item = Map::new();
            item.insert("iface".into(), json!(iface));
            item.insert("rx_kbps".into(), Value::Null);
            item.insert("tx_kbps".into(), Value::Null);
            item.insert("rx_total_kb".into(), json!(r1(rx as f64 / 1024.0)));
            item.insert("tx_total_kb".into(), json!(r1(tx as f64 / 1024.0)));
            item.insert(
                "state".into(),
                states[iface].as_ref().map(|s| json!(s)).unwrap_or(Value::Null),
            );
            item.insert("primary".into(), json!(false));
            if let (Some(prev), Some(t), Some(pt)) = (self.prev_net.get(iface), ts, prev_ts) {
                if t > pt {
                    let dt = t - pt;
                    let drx = rx - prev.0;
                    let dtx = tx - prev.1;
                    if drx >= 0 && dtx >= 0 {
                        item.insert("rx_kbps".into(), json!(r1(drx as f64 * 8.0 / 1000.0 / dt)));
                        item.insert("tx_kbps".into(), json!(r1(dtx as f64 * 8.0 / 1000.0 / dt)));
                    }
                }
            }
            out.push(Value::Object(item));
        }
        if let Some(p) = pick_primary_network(&out) {
            let piface = out[p].get("iface").and_then(|v| v.as_str()).map(|s| s.to_string());
            for item in out.iter_mut() {
                let is_p = item.get("iface").and_then(|v| v.as_str()).map(|s| Some(s.to_string()) == piface).unwrap_or(false);
                if let Some(o) = item.as_object_mut() {
                    o.insert("primary".into(), json!(is_p));
                }
            }
        }
        self.prev_net = cur;
        Value::Array(out)
    }

    /// DSK 段：使用率由 total/available 直接给出，读写速率按相邻两帧差值算。
    /// 采集端给的是累计 KB（/proc/diskstats 扇区×512/1024），与 --once 回退模式兼容。
    fn parse_disks(&mut self, raw: Option<&str>, ts: Option<f64>) -> Value {
        let Some(raw) = raw else { return json!([]) };
        let cur = parse_disk_counters(raw);
        let prev_ts = self.prev_ts;
        let mut out: Vec<Value> = Vec::new();
        for (mount, dev, total, avail, read, write) in &cur {
            let mut item = Map::new();
            item.insert("mount".into(), json!(mount));
            // beta2.5：块设备名（/dev/sdxN）供前端切换显示；老脚本没有该字段时回退到挂载点。
            item.insert("dev".into(), json!(if dev.is_empty() { mount } else { dev }));
            item.insert("total_kb".into(), json!(*total));
            item.insert("available_kb".into(), json!(*avail));
            item.insert("read_total_kb".into(), json!(r1(*read as f64)));
            item.insert("write_total_kb".into(), json!(r1(*write as f64)));
            item.insert("read_kbps".into(), Value::Null);
            item.insert("write_kbps".into(), Value::Null);
            if let (Some(prev), Some(t), Some(pt)) = (self.prev_disk.get(mount), ts, prev_ts) {
                if t > pt {
                    let dr = read - prev.0;
                    let dw = write - prev.1;
                    if dr >= 0 && dw >= 0 {
                        let dt = t - pt;
                        item.insert("read_kbps".into(), json!(r1(dr as f64 / dt)));
                        item.insert("write_kbps".into(), json!(r1(dw as f64 / dt)));
                    }
                }
            }
            out.push(Value::Object(item));
        }
        self.prev_disk = cur
            .into_iter()
            .map(|(mount, _, _, _, read, write)| (mount, (read, write)))
            .collect();
        Value::Array(out)
    }
}

// ---------- 各段 ----------

fn parse_mem(val: &str) -> Option<Value> {
    let mut parts: Vec<i64> = val.split(',').take(4).map(|x| to_int(x).unwrap_or(0)).collect();
    while parts.len() < 4 {
        parts.push(0);
    }
    let (total, avail, buffers, cached) = (parts[0], parts[1], parts[2], parts[3]);
    if total <= 0 {
        return None;
    }
    Some(json!({
        "total_kb": total,
        "available_kb": avail,
        "buffers_kb": buffers,
        "cached_kb": cached,
        "available_percent": r1(100.0 * avail as f64 / total as f64),
    }))
}

/// DSK=<挂载点>,<总KB>,<可用KB>,<累计读KB>,<累计写KB>,<块设备名>;...
/// 第 4、5 项是 beta1.3 新增（旧脚本只有前三项时按 0 处理）；
/// 第 6 项是 beta2.5 新增的 df 设备列（/dev/sdxN），老脚本缺失时回退到挂载点。
fn parse_disk_counters(val: &str) -> Vec<(String, String, i64, i64, i64, i64)> {
    let mut out = Vec::new();
    for ent in val.split(';') {
        if ent.is_empty() {
            continue;
        }
        let mut it = ent.split(',');
        let mnt = it.next().unwrap_or("");
        let total = it.next().and_then(to_int);
        let avail = it.next().and_then(to_int).unwrap_or(0);
        let read = it.next().and_then(to_int).unwrap_or(0);
        let write = it.next().and_then(to_int).unwrap_or(0);
        let dev = it.next().unwrap_or("").trim();
        let Some(total) = total else { continue };
        if mnt.is_empty() || total <= 0 {
            continue;
        }
        out.push((
            mnt.to_string(),
            dev.to_string(),
            total,
            avail.max(0),
            read.max(0),
            write.max(0),
        ));
    }
    out
}

/// PSUM 段（beta1.4.3）：`PSUM=<进程总数>,<运行中>,<僵尸>`。
fn parse_proc_summary(val: &str) -> Value {
    let parts: Vec<Option<i64>> = val.split(',').take(3).map(to_int).collect();
    if parts.len() < 3 {
        return Value::Null;
    }
    let (Some(total), Some(running), Some(zombie)) = (parts[0], parts[1], parts[2]) else {
        return Value::Null;
    };
    json!({"total": total, "running": running, "zombie": zombie})
}

/// PTOP 段（beta1.4.3）：`PTOP=<pid>,<cpu%>,<rssKB>,<进程名>;...`（按 cpu% 降序取前 8）。
/// cpu% 首帧为空 → null；mem_percent 由 rssKB / MEM 总量换算，MEM 缺失留空。
fn parse_proc_top(val: &str, mem_total_kb: Option<f64>) -> Value {
    let mut out = Vec::new();
    for ent in val.split(';') {
        if ent.is_empty() {
            continue;
        }
        let mut it = ent.splitn(4, ',');
        let pid = it.next().unwrap_or("").trim();
        let cpu = it.next().and_then(to_float);
        let rss_kb = it.next().and_then(to_float);
        let name = it.next().unwrap_or("").trim();
        let Some(pid) = to_int(pid) else { continue };
        if name.is_empty() {
            continue;
        }
        let mem_percent = match (rss_kb, mem_total_kb) {
            (Some(r), Some(t)) if t > 0.0 => Some(r1(100.0 * r / t)),
            _ => None,
        };
        out.push(json!({
            "pid": pid,
            "cpu_percent": cpu.map(r1).map(|v| json!(v)).unwrap_or(Value::Null),
            "mem_percent": mem_percent.map(|v| json!(v)).unwrap_or(Value::Null),
            "rss_kb": rss_kb.map(|v| json!(r1(v))).unwrap_or(Value::Null),
            "name": name,
        }));
    }
    Value::Array(out)
}

/// CON 段（beta1.4.3）：`CON=<ESTAB>,<TIME_WAIT>,<LISTEN>,<UDP套接字>`。
fn parse_con(val: &str) -> Value {
    let parts: Vec<Option<i64>> = val.split(',').take(4).map(to_int).collect();
    if parts.len() < 4 {
        return Value::Null;
    }
    let (Some(established), Some(time_wait), Some(listen), Some(udp)) =
        (parts[0], parts[1], parts[2], parts[3])
    else {
        return Value::Null;
    };
    json!({"established": established, "time_wait": time_wait, "listen": listen, "udp": udp})
}

fn parse_temps(val: &str) -> Value {
    let mut out = Vec::new();
    for ent in val.split(';') {
        if ent.is_empty() || !ent.contains(',') {
            continue;
        }
        let mut it = ent.splitn(2, ',');
        let ty = it.next().unwrap_or("");
        let milli = it.next().and_then(to_int);
        let Some(milli) = milli else { continue };
        if ty.is_empty() {
            continue;
        }
        out.push(json!({"zone": ty, "type": ty, "temp_c": r1(milli as f64 / 1000.0)}));
    }
    Value::Array(out)
}

fn parse_fans(val: &str) -> Value {
    let mut out = Vec::new();
    for ent in val.split(';') {
        if ent.is_empty() || !ent.contains(',') {
            continue;
        }
        let mut it = ent.splitn(2, ',');
        let name = it.next().unwrap_or("");
        let rpm = it.next().and_then(to_int);
        let Some(rpm) = rpm else { continue };
        if name.is_empty() {
            continue;
        }
        out.push(json!({"name": name, "rpm": rpm.max(0)}));
    }
    Value::Array(out)
}

/// GPU 段（beta2.4）：
/// `GPU=<名称>,<驱动>,<占用%>,<显存已用MiB>,<显存总MiB>,<温度℃>,<功耗W>;...`
/// 名称里不会有逗号（采集端已替换成下划线），数值缺失是空字段 → null；
/// 显存两项都有时顺带算出 mem_percent，方便前端直接展示。
fn parse_gpu(val: &str) -> Value {
    let mut out = Vec::new();
    for ent in val.split(';') {
        if ent.is_empty() || !ent.contains(',') {
            continue;
        }
        let mut it = ent.splitn(7, ',');
        let name = it.next().unwrap_or("").trim();
        let driver = it.next().unwrap_or("").trim();
        let busy = it.next().and_then(to_float);
        let mem_used = it.next().and_then(to_float);
        let mem_total = it.next().and_then(to_float);
        let temp = it.next().and_then(to_float);
        let power = it.next().and_then(to_float);
        if name.is_empty() {
            continue;
        }
        let mem_percent = match (mem_used, mem_total) {
            (Some(u), Some(t)) if t > 0.0 => Some(r1(100.0 * u / t)),
            _ => None,
        };
        let num = |v: Option<f64>| v.map(|x| json!(r1(x))).unwrap_or(Value::Null);
        out.push(json!({
            "name": name,
            "driver": if driver.is_empty() { Value::Null } else { json!(driver) },
            "busy_percent": num(busy),
            "mem_used_mb": num(mem_used),
            "mem_total_mb": num(mem_total),
            "mem_percent": mem_percent.map(|v| json!(v)).unwrap_or(Value::Null),
            "temp_c": num(temp),
            "power_w": num(power),
        }));
    }
    Value::Array(out)
}

// ---------- Docker ----------

fn parse_docker_state(val: &str) -> Option<Value> {
    let parts: Vec<&str> = val.split(',').collect();
    let daemon = parts[0];
    if !["none", "down", "ok"].contains(&daemon) {
        return None;
    }
    let mut d = Map::new();
    d.insert("daemon".into(), json!(daemon));
    d.insert("running".into(), json!(0));
    d.insert("total".into(), json!(0));
    d.insert("containers".into(), json!([]));
    if daemon == "ok" && parts.len() >= 3 {
        d.insert("running".into(), json!(to_int(parts[1]).unwrap_or(0)));
        d.insert("total".into(), json!(to_int(parts[2]).unwrap_or(0)));
    }
    Some(Value::Object(d))
}

fn parse_docker_ps(val: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for ent in val.split(';') {
        if ent.is_empty() {
            continue;
        }
        // 最后一个字段（状态详情）吞掉内部逗号，前面的字段位置不受影响
        let parts: Vec<&str> = ent.splitn(5, ',').collect();
        if parts.len() < 4 || parts[0].is_empty() {
            continue;
        }
        out.push(json!({
            "id": parts[0],
            "name": parts[1],
            "image": parts[2],
            "state": parts[3],
            "status": if parts.len() > 4 { parts[4] } else { "" },
            // stats 字段先给缺省值，合并 DKST 后被覆盖；schema 稳定
            "cpu_percent": Value::Null,
            "mem_used": Value::Null,
            "mem_limit": Value::Null,
            "mem_percent": Value::Null,
            "net_io": Value::Null,
            "block_io": Value::Null,
        }));
    }
    out
}

fn parse_docker_stats(val: &str) -> HashMap<String, Map<String, Value>> {
    let mut out = HashMap::new();
    for ent in val.split(';') {
        if ent.is_empty() {
            continue;
        }
        // 0=id 1=cpu% 2=内存已用/上限 3=内存% 4=网络IO 5=块IO(吞逗号)
        let parts: Vec<&str> = ent.splitn(6, ',').collect();
        if parts.len() < 6 || parts[0].is_empty() {
            continue;
        }
        let (mem_used, mem_limit) = match parts[2].split_once('/') {
            Some((u, l)) => (u.trim().to_string(), l.trim().to_string()),
            None => (parts[2].trim().to_string(), String::new()),
        };
        let mut m = Map::new();
        m.insert(
            "cpu_percent".into(),
            to_float(parts[1].trim().trim_end_matches('%')).map(|v| json!(v)).unwrap_or(Value::Null),
        );
        m.insert("mem_used".into(), if mem_used.is_empty() { Value::Null } else { json!(mem_used) });
        m.insert("mem_limit".into(), if mem_limit.is_empty() { Value::Null } else { json!(mem_limit) });
        m.insert(
            "mem_percent".into(),
            to_float(parts[3].trim().trim_end_matches('%')).map(|v| json!(v)).unwrap_or(Value::Null),
        );
        let net = parts[4].trim();
        m.insert("net_io".into(), if net.is_empty() { Value::Null } else { json!(net) });
        let block = parts[5].trim();
        m.insert("block_io".into(), if block.is_empty() { Value::Null } else { json!(block) });
        out.insert(parts[0].to_string(), m);
    }
    out
}

fn merge_docker(ps_list: Vec<Value>, stats: &HashMap<String, Map<String, Value>>) -> Vec<Value> {
    ps_list
        .into_iter()
        .map(|mut c| {
            if let (Some(id), Some(obj)) = (
                c.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()),
                c.as_object_mut(),
            ) {
                if let Some(st) = stats.get(&id) {
                    for (k, v) in st {
                        obj.insert(k.clone(), v.clone());
                    }
                }
            }
            c
        })
        .collect()
}
