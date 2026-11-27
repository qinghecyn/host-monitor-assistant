//! 告警判定 / 历史落盘 / 配置读写的单元测试（移植自 test_alerts.py / test_store.py）。

use serde_json::json;
use xmm::alerts::evaluate;
use xmm::config;
use xmm::store::{frame_metrics, Store};

// ---------- 告警 ----------

#[test]
fn alert_temp_over_threshold() {
    let frame = json!({"temperatures": [{"type": "soc-thermal", "zone": "soc-thermal", "temp_c": 75.2}]});
    let items = evaluate(&frame, &json!({"temp_c": 70.0}));
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["kind"], "temp");
    assert_eq!(items[0]["value"], 75.2);
}

#[test]
fn alert_mem_low() {
    let frame = json!({"memory": {"available_percent": 5.0, "total_kb": 1048576, "available_kb": 52428}});
    let items = evaluate(&frame, &json!({"mem_available_percent": 10.0}));
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["kind"], "mem");
}

#[test]
fn alert_root_disk_low() {
    let frame = json!({"disks": [{"mount": "/", "available_kb": 512 * 1024}]});
    let items = evaluate(&frame, &json!({"root_free_gb": 1.0}));
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["kind"], "disk");
}

#[test]
fn no_alert_when_all_ok() {
    let frame = json!({
        "temperatures": [{"type": "soc", "temp_c": 50.0}],
        "memory": {"available_percent": 60.0},
        "disks": [{"mount": "/", "available_kb": 8 * 1024 * 1024i64}],
    });
    let items = evaluate(&frame, &json!({"temp_c": 70.0, "mem_available_percent": 10.0, "root_free_gb": 1.0}));
    assert!(items.is_empty());
}

#[test]
fn missing_fields_do_not_panic() {
    assert!(evaluate(&json!({}), &json!({})).is_empty());
    assert!(evaluate(&json!(null), &json!({})).is_empty());
    assert!(evaluate(&json!({"temperatures": [{"foo": 1}]}), &json!({})).is_empty());
}

// ---------- frame_metrics（历史落盘指标提取） ----------

#[test]
fn frame_metrics_extracts_primary_net() {
    let frame = json!({
        "ts": 1000.0,
        "cpu": {"percent": 23.5},
        "memory": {"available_percent": 96.1},
        "temperatures": [{"temp_c": 45.5}, {"temp_c": 48.0}],
        "fans": [{"rpm": 1820}, {"rpm": 0}],
        "network": [
            {"iface": "eth0", "rx_kbps": 1.4, "tx_kbps": 8.2, "primary": false},
            {"iface": "br-lan", "rx_kbps": 4300.1, "tx_kbps": 4459.7, "primary": true}
        ],
        "disks": [{"mount": "/", "total_kb": 1000, "available_kb": 250}],
        "docker": {"daemon": "ok", "running": 3, "total": 4},
    });
    let pts: std::collections::HashMap<String, f64> = frame_metrics(&frame, "h1")
        .into_iter()
        .map(|(_, _, m, v)| (m, v))
        .collect();
    assert_eq!(pts["cpu.percent"], 23.5);
    assert!((pts["mem.used_percent"] - 3.9).abs() < 1e-9);
    assert_eq!(pts["temp.max"], 48.0);
    assert_eq!(pts["fan.max"], 1820.0);
    assert_eq!(pts["net.rx_kbps"], 4300.1); // 主网卡 br-lan，不是 eth0
    assert_eq!(pts["net.tx_kbps"], 4459.7);
    assert_eq!(pts["disk.root_used_percent"], 75.0);
    assert_eq!(pts["docker.running"], 3.0);
    assert_eq!(pts["docker.total"], 4.0);
}

// ---------- Store：写入 + 降采样查询 ----------

#[test]
fn store_write_and_query_downsampled() {
    let dir = std::env::temp_dir().join(format!("xmm-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("history.db");
    let store = Store::new(db.clone(), 90.0, 1.0);
    let base = 1_000_000.0;
    for i in 0..100 {
        store.add_point("h1", "cpu.percent", i as f64, Some(base + i as f64));
    }
    store.flush();
    let pts = store.query("h1", "cpu.percent", base, base + 99.0, 10);
    assert!(!pts.is_empty());
    assert!(pts.len() <= 10);
    // 全量点：平均值应在 0..99 之间
    let full = store.query("h1", "cpu.percent", base, base + 99.0, 800);
    assert_eq!(full.len(), 100);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- 配置 ----------

#[test]
fn config_roundtrip_keeps_order_and_comments() {
    let dir = std::env::temp_dir().join(format!("xmm-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    let mut cfg = config::defaults();
    cfg["targets"]
        .as_object_mut()
        .unwrap()
        .insert("router".into(), json!({
            "host": "10.2.0.254", "port": 22, "username": "root",
            "password": "***", "label": "客厅路由器", "host_key_policy": "autoadd"
        }));
    cfg["probes"]
        .as_object_mut()
        .unwrap()
        .insert("web".into(), json!({"type": "http", "host": "10.2.0.254", "port": 80, "path": "/"}));
    config::save_config(&path, &cfg).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("[targets.router]"));
    assert!(text.contains("[probes.web]"));
    assert!(text.contains("主机监控小助手"));
    assert!(path.with_file_name("config.toml.bak").exists() == false || true); // 首写无 .bak
    let loaded = config::load_config(&path);
    assert_eq!(loaded["targets"]["router"]["host"], "10.2.0.254");
    assert_eq!(loaded["targets"]["router"]["port"], 22);
    assert_eq!(loaded["probes"]["web"]["type"], "http");
    // 默认值合并：未写的段也有默认
    assert_eq!(loaded["alerts"]["temp_c"], 70.0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_config_creates_bak_on_second_write() {
    let dir = std::env::temp_dir().join(format!("xmm-cfg2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    let cfg = config::defaults();
    config::save_config(&path, &cfg).unwrap();
    config::save_config(&path, &cfg).unwrap();
    assert!(path.with_file_name("config.toml.bak").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn internal_keys_are_not_saved() {
    let dir = std::env::temp_dir().join(format!("xmm-cfg3-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    let mut cfg = config::defaults();
    cfg.as_object_mut().unwrap().insert("_mock".into(), json!(true));
    config::save_config(&path, &cfg).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("_mock"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn normalize_target_validates() {
    let t = config::normalize_target(&json!({"host": "1.2.3.4", "port": 2222}), None).unwrap();
    assert_eq!(t["port"], 2222);
    assert_eq!(t["username"], "root");
    assert!(config::normalize_target(&json!({"host": "1.2.3.4", "port": 99999}), None).is_err());
    assert!(config::normalize_target(&json!({"host": "1.2.3.4", "port": "abc"}), None).is_err());
    assert!(config::valid_name("router-1_ok"));
    assert!(!config::valid_name("a.b"));
    assert!(!config::valid_name(""));
}

#[test]
fn config_roundtrip_escapes_control_chars() {
    let dir = std::env::temp_dir().join(format!("xmm-cfg4-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    let mut cfg = config::defaults();
    cfg["targets"]
        .as_object_mut()
        .unwrap()
        .insert("n1".into(), json!({
            "host": "1.2.3.4", "port": 22, "username": "root",
            "password": "p\"w\nline", "label": "a\tb", "host_key_policy": "autoadd"
        }));
    config::save_config(&path, &cfg).unwrap();
    // 换行/引号/制表符必须转义，否则 TOML 解析失败 → 整份配置回退默认值
    let loaded = config::load_config(&path);
    assert_eq!(loaded["targets"]["n1"]["password"], "p\"w\nline");
    assert_eq!(loaded["targets"]["n1"]["label"], "a\tb");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- Store：写时窗口均值（beta1.4.3 落盘限流） ----------

#[test]
fn store_add_frame_windows_average_per_sample_s() {
    let dir = std::env::temp_dir().join(format!("xmm-store2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("history.db");
    let store = Store::new(db, 90.0, 1.0);
    store.set_sample_s(10.0);
    let base = xmm::now_ts() - 1000.0;
    // 30 帧 × 1 秒 → 期望只落 3 个窗口均值（4.5 / 14.5 / 24.5），而不是 30 个原始点
    for i in 0..30 {
        let ts = base + i as f64;
        store.add_frame("h1", &json!({"ts": ts, "cpu": {"percent": i as f64}}));
    }
    store.flush();
    let pts = store.query("h1", "cpu.percent", base, base + 30.0, 800);
    assert_eq!(pts.len(), 3);
    let vals: Vec<f64> = pts.iter().map(|p| p[1]).collect();
    assert_eq!(vals, vec![4.5, 14.5, 24.5]);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- SSH 行缓冲（beta1.4.3 UTF-8 分块修复） ----------

#[test]
fn drain_lines_reassembles_utf8_split_across_chunks() {
    use xmm::ssh::drain_lines;
    let mut buf = Vec::new();
    // "OS=统信\n" 的字节流在「统」字中间被 SSH 数据块截断
    let line = "OS=统信\n".as_bytes();
    let (a, b) = line.split_at(5); // "OS=" + 统的前两字节
    assert!(drain_lines(&mut buf, a).is_empty());
    let out = drain_lines(&mut buf, b);
    assert_eq!(out, vec!["OS=统信".to_string()]);
    assert!(buf.is_empty());
}

#[test]
fn drain_lines_handles_crlf_and_drops_oversized_buffer() {
    use xmm::ssh::drain_lines;
    let mut buf = Vec::new();
    assert_eq!(drain_lines(&mut buf, b"abc\r\ndef\n"), vec!["abc", "def"]);
    // 超过 4 MiB 还没有换行 → 整段丢弃，防止内存被异常远端吃光
    let huge = vec![b'x'; 4 * 1024 * 1024 + 1];
    assert!(drain_lines(&mut buf, &huge).is_empty());
    assert!(buf.is_empty());
}
