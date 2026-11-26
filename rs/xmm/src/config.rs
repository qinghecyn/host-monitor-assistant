//! config.toml 读写（移植自 server/config.py）。
//!
//! - 多目标（[targets.<name>]）支持运行时增删改，写入为原子操作并保留上一版备份；
//! - 目标名做合法性校验（只能是 TOML 裸键：字母/数字/下划线/中划线）；
//! - 读写顺序与文件顺序一致，UI 里看到的顺序就是文件里的顺序。
//!
//! 配置在内存中用 serde_json::Value（preserve_order）表示，与 Python dict 语义对齐。

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// 默认配置（对应 Python DEFAULTS）
pub fn defaults() -> Value {
    json!({
        "server": {"listen_host": "0.0.0.0", "listen_port": 8787},
        "auth": {"username": "admin", "password": "admin123"},
        "collect": {"interval_s": 1, "history_minutes": 5},
        "alerts": {"temp_c": 70.0, "mem_available_percent": 0.0, "root_free_gb": 1.0, "resource_rules": crate::alerts::default_rules()},
        "notify": {
            "enabled": true,
            "dedup_minutes": 10.0,
            "quiet_start": "",
            "quiet_end": "",
            "serverchan_key": "",
            "telegram_token": "",
            "telegram_chat": "",
            "smtp_host": "",
            "smtp_port": 465,
            "smtp_user": "",
            "smtp_pass": "",
            "smtp_to": "",
            "webhook_url": "",
            "heartbeat_minutes": 0
        },
        "history": {"keep_days": 90.0},
        "targets": {},
        "probes": {}
    })
}

/// 目标名只允许 TOML 裸键，避免 [targets.a.b] 被解析成嵌套表
pub fn valid_name(name: &str) -> bool {
    let n = name.trim();
    !n.is_empty()
        && n.len() <= 32
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 允许出现在 [targets.*] 里的字段（白名单，写入前过滤）
pub const TARGET_FIELDS: [&str; 9] = [
    "host", "port", "username", "password", "password_env",
    "private_key", "host_key_policy", "label", "note",
];

/// 连接相关字段（变了要重建采集器）
pub const CONN_FIELDS: [&str; 7] = [
    "host", "port", "username", "password", "password_env",
    "private_key", "host_key_policy",
];

// ---------------------------------------------------------------- 解析

fn toml_to_json(v: toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => json!(i),
        toml::Value::Float(f) => json!(f),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Array(a) => Value::Array(a.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => {
            let mut m = Map::new();
            for (k, v) in t {
                m.insert(k, toml_to_json(v));
            }
            Value::Object(m)
        }
        toml::Value::Datetime(d) => Value::String(d.to_string()),
    }
}

fn deep_merge(dst: &mut Value, src: Value) {
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, v) in s {
                let both_objects = v.is_object()
                    && d.get(&k).map(|x| x.is_object()).unwrap_or(false);
                if both_objects {
                    deep_merge(d.get_mut(&k).unwrap(), v);
                } else {
                    d.insert(k, v);
                }
            }
        }
        (d, s) => *d = s,
    }
}

pub fn load_config(path: &Path) -> Value {
    let mut cfg = defaults();
    if path.exists() {
        match std::fs::read_to_string(path) {
            Ok(text) => match text.parse::<toml::Value>() {
                Ok(data) => deep_merge(&mut cfg, toml_to_json(data)),
                Err(e) => {
                    tracing::error!("配置文件解析失败（{}），改用默认值：{}", path.display(), e);
                }
            },
            Err(e) => {
                tracing::error!("配置文件读取失败（{}）：{}", path.display(), e);
            }
        }
    }
    if !cfg.get("targets").map(|t| t.is_object()).unwrap_or(false) {
        cfg.as_object_mut().unwrap().insert("targets".into(), json!({}));
    }
    cfg
}

// ---------------------------------------------------------------- 序列化

fn fmt_value(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if n.as_i64().is_none() && f.fract() == 0.0 && f.is_finite() && f.abs() < 1e15 {
                    // Python repr(70.0) == "70.0"
                    format!("{:.1}", f)
                } else {
                    format!("{}", f)
                }
            } else {
                n.to_string()
            }
        }
        Value::String(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
        Value::Array(_) => toml::Value::try_from(v).expect("validated TOML array").to_string(),
        _ => "null".into(),
    }
}

const HEADER: &str = "\
# ============================================================
#  主机监控小助手 —— 配置文件
#  本文件由程序自动生成/维护；在界面里改更省事，手改也行（改完重启）。
#  密码优先级：环境变量 > 系统密钥环 > 本文件明文（不推荐）
# ============================================================
";

/// 与 Python dump_config 输出格式一致（段顺序、注释、键顺序均保持）。
pub fn dump_config(cfg: &Value) -> String {
    let mut out: Vec<String> = vec![HEADER.trim_end().into()];

    let section = |out: &mut Vec<String>, cfg: &Value, name: &str, comment: &str| {
        if let Some(sec) = cfg.get(name).and_then(|s| s.as_object()) {
            out.push(String::new());
            out.push(format!("[{}]", name));
            if !comment.is_empty() {
                out.push(comment.into());
            }
            for (k, v) in sec {
                if !v.is_object() {
                    out.push(format!("{} = {}", k, fmt_value(v)));
                }
            }
        }
    };

    section(&mut out, cfg, "server", "# 监听地址；127.0.0.1 = 只允许本机访问，0.0.0.0 = 局域网可访问");
    section(&mut out, cfg, "auth", "# 浏览器登录用的账号密码（桌面版由程序自己换 token，不用手输）");
    section(&mut out, cfg, "collect", "# 采集间隔（秒）、折线图窗口（分钟）");
    section(&mut out, cfg, "alerts", "# 告警阈值：温度(℃)、内存可用比例(%)、根分区可用空间(GB)");

    let empty = Map::new();
    let targets = cfg.get("targets").and_then(|t| t.as_object()).unwrap_or(&empty);
    if !targets.is_empty() {
        out.push(String::new());
        out.push("# ------------------------------------------------------------".into());
        out.push("# 被监控主机：一台一段，[targets.外号]，外号只能是字母/数字/下划线".into());
        out.push("#   host / port / username / password（或 private_key）".into());
    }
    for (name, t) in targets {
        out.push(String::new());
        out.push(format!("[targets.{}]", name));
        if let Some(t) = t.as_object() {
            for (k, v) in t {
                if !v.is_object() {
                    out.push(format!("{} = {}", k, fmt_value(v)));
                }
            }
        }
    }

    // beta2.2：告警推送 / 历史保留
    for sec in ["notify", "history"] {
        section(&mut out, cfg, sec, "");
    }

    let probes = cfg.get("probes").and_then(|t| t.as_object()).unwrap_or(&empty);
    if !probes.is_empty() {
        out.push(String::new());
        out.push("# 服务拨测：type = tcp | http | ping".into());
        out.push("#   tcp:   host + port".into());
        out.push("#   http:  host + port + path（也可 host 直接填完整 URL，支持 https）".into());
        out.push("#   ping:  host".into());
        out.push("#   interval_s 间隔 / timeout_s 超时 / fail_threshold 连续失败几次才算宕".into());
    }
    for (name, p) in probes {
        out.push(String::new());
        out.push(format!("[probes.{}]", name));
        if let Some(p) = p.as_object() {
            for (k, v) in p {
                if !v.is_object() {
                    out.push(format!("{} = {}", k, fmt_value(v)));
                }
            }
        }
    }

    let mut s = out.join("\n").trim_end().to_string();
    s.push('\n');
    s
}

/// 原子写：先写临时文件再替换，并保留一份 .bak（避免写一半断电毁配置）。
pub fn save_config(path: &Path, cfg: &Value) -> std::io::Result<()> {
    let mut clean = cfg.clone();
    if let Some(obj) = clean.as_object_mut() {
        obj.retain(|k, _| !k.starts_with('_'));
    }
    let text = dump_config(&clean);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let mut bak = path.as_os_str().to_os_string();
        bak.push(".bak");
        let _ = std::fs::copy(path, PathBuf::from(bak));
    }
    use std::io::Write;
    let directory = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let tmp = directory.join(format!(".cfg-{:016x}.tmp", rand::random::<u64>()));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let result = (|| {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    tracing::info!("配置已写入 {}", path.display());
    Ok(())
}

// ---------------------------------------------------------------- 目标校验

/// 把前端传来的字段清洗成可写入的 target 字典。
/// base 为旧配置（编辑时保留未提交字段）。出错返回中文原因。
pub fn normalize_target(fields: &Value, base: Option<&Value>) -> Result<Value, String> {
    let mut cur = base.cloned().unwrap_or_else(|| json!({}));
    let obj = cur.as_object_mut().unwrap();
    let empty = Map::new();
    let fobj = fields.as_object().unwrap_or(&empty);
    for k in TARGET_FIELDS {
        let Some(v) = fobj.get(k) else { continue };
        if v.is_null() {
            obj.remove(k);
            continue;
        }
        match k {
            "port" => {
                let port = v
                    .as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    .ok_or_else(|| "端口必须是数字".to_string())?;
                if !(1..=65535).contains(&port) {
                    return Err("端口超出范围（1-65535）".into());
                }
                obj.insert(k.into(), json!(port));
            }
            "host_key_policy" => {
                let pol = v.as_str().unwrap_or("").trim();
                let pol = if pol.is_empty() { "autoadd" } else { pol };
                let pol = if ["autoadd", "strict"].contains(&pol) { pol } else { "autoadd" };
                obj.insert(k.into(), json!(pol));
            }
            _ => {
                let s = match v {
                    Value::String(s) if k == "password" => s.clone(),
                    Value::String(s) => s.trim().to_string(),
                    other => other.to_string().trim().to_string(),
                };
                if s.is_empty() {
                    obj.remove(k);
                } else {
                    obj.insert(k.into(), json!(s));
                }
            }
        }
    }
    obj.entry("port").or_insert(json!(22));
    obj.entry("username").or_insert(json!("root"));
    obj.entry("host_key_policy").or_insert(json!("autoadd"));
    Ok(cur)
}

pub const EXAMPLE: &str = "\
# 主机监控小助手 配置（首次运行自动生成；模板见 config.example.toml）

[server]
listen_host = \"0.0.0.0\"
listen_port = 8787

[auth]
username = \"admin\"
password = \"admin123\"

[collect]
interval_s = 1
history_minutes = 5

[alerts]
temp_c = 70.0
mem_available_percent = 0.0
root_free_gb = 1.0

# 被监控主机在界面\"服务器\"里添加，或手写：
# [targets.primary]
# host = \"192.168.1.1\"
# port = 22
# username = \"root\"
# password = \"***\"
";

/// 首次运行自动生成默认配置，然后读入。
pub fn load_or_create(path: &Path) -> Value {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, EXAMPLE);
        tracing::info!("已生成默认配置 {}（Web 登录 admin/admin123，请尽快修改）", path.display());
    }
    load_config(path)
}
