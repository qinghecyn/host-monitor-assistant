//! StreamParser 单测：从 tests/test_parser.py + test_beta23_regression.py 移植，
//! 保证 Rust 解析器与 Python 版行为一致。

use serde_json::Value;
use xmm::parser::{pick_primary_network, StreamParser};
use xmm::store::frame_metrics;

fn sample_full(t: f64) -> String {
    format!(
        "T={:.3}|U=86400|L=0.52,0.41,0.33\
         |CPU=t:100,0,100,600,20,0,0,0;0:50,0,50,300,10,0,0,0;1:50,0,50,300,10,0,0,0\
         |MEM=1048576,524288,65536,262144\
         |DSK=/,8388608,6291456;/data,64000,32000\
         |NET=eth0,1000000,500000;eth1,2000,1000\
         |TMP=soc-thermal,45500;x86_pkg_temp,46800\
         |OS=OpenWrt 23.05.2 r23909",
        t
    )
}

fn sample_next(t: f64) -> String {
    format!(
        "T={:.3}|U=86401|L=0.52,0.41,0.33\
         |CPU=t:150,0,150,700,20,0,0,0;0:75,0,75,350,10,0,0,0;1:75,0,75,350,10,0,0,0\
         |MEM=1048576,500000,65536,262144\
         |DSK=/,8388608,6291456\
         |NET=eth0,1400000,600000;eth1,2000,1000\
         |TMP=soc-thermal,46500\
         |OS=OpenWrt 23.05.2 r23909",
        t
    )
}

#[test]
fn full_frame_fields() {
    let mut p = StreamParser::new();
    let f = p.parse(&sample_full(1000.0)).unwrap();
    assert_eq!(f["os"]["pretty_name"], "OpenWrt 23.05.2 r23909");
    assert_eq!(f["uptime_s"], 86400);
    assert_eq!(f["cpu"]["cores"], 2);
    assert_eq!(f["cpu"]["load"], serde_json::json!([0.52, 0.41, 0.33]));
    assert!(f["cpu"]["percent"].is_null()); // 首帧无差值
    assert_eq!(f["memory"]["total_kb"], 1048576);
    assert_eq!(f["memory"]["available_percent"], 50.0);
    assert_eq!(f["disks"].as_array().unwrap().len(), 2);
    assert_eq!(f["disks"][0]["mount"], "/");
    assert_eq!(f["network"].as_array().unwrap().len(), 2);
    assert!(f["network"][0]["rx_kbps"].is_null()); // 首帧无差值
    assert_eq!(f["network"][0]["rx_total_kb"], 976.6);
    assert_eq!(f["temperatures"][0]["type"], "soc-thermal");
    assert_eq!(f["temperatures"][0]["temp_c"], 45.5);
}

#[test]
fn rate_delta() {
    let mut p = StreamParser::new();
    p.parse(&sample_full(1000.0));
    let f = p.parse(&sample_next(1001.0)).unwrap();
    assert_eq!(f["cpu"]["percent"], 50.0);
    assert_eq!(f["cpu"]["per_core"], serde_json::json!([50.0, 50.0]));
    // eth0: rx delta 400000 bytes / 1s -> 3200 kbps; tx delta 100000 -> 800 kbps
    assert_eq!(f["network"][0]["rx_kbps"], 3200.0);
    assert_eq!(f["network"][0]["tx_kbps"], 800.0);
    assert_eq!(f["network"][1]["rx_kbps"], 0.0); // eth1 无变化
}

#[test]
fn disk_io_rate_from_counters() {
    let mut p = StreamParser::new();
    let first = p
        .parse("T=1000.0|DSK=/,100000,40000,100000,200000;/data,200000,100000,500000,600000")
        .unwrap();
    assert!(first["disks"][0]["read_kbps"].is_null()); // 首帧无差值
    assert_eq!(first["disks"][0]["total_kb"], 100000);

    // 2 秒内 / 读 +4048KB、写 +500KB；/data 读 0、写 +1024KB
    let second = p
        .parse("T=1002.0|DSK=/,100000,30000,104048,200500;/data,200000,100000,500000,601024")
        .unwrap();
    assert_eq!(second["disks"][0]["read_kbps"], 2024.0);
    assert_eq!(second["disks"][0]["write_kbps"], 250.0);
    assert_eq!(second["disks"][1]["read_kbps"], 0.0);
    assert_eq!(second["disks"][1]["write_kbps"], 512.0);
    // 使用率仍按容量算：/ 可用 30%
    assert_eq!(second["disks"][0]["available_kb"], 30000);
}

#[test]
fn disk_device_field_from_sixth_column() {
    let f = StreamParser::new()
        .parse("T=1000.0|DSK=/,100000,40000,100000,200000,/dev/sda1;/mnt/sata5-4,2000000,1000000,500000,600000,/dev/sdb1")
        .unwrap();
    let disks = f["disks"].as_array().unwrap();
    assert_eq!(disks.len(), 2);
    assert_eq!(disks[0]["mount"], "/");
    assert_eq!(disks[0]["dev"], "/dev/sda1");
    assert_eq!(disks[1]["mount"], "/mnt/sata5-4");
    assert_eq!(disks[1]["dev"], "/dev/sdb1");
}

#[test]
fn disk_device_field_falls_back_to_mount_for_legacy_frames() {
    let f = StreamParser::new()
        .parse("T=1000.0|DSK=/data,100000,40000;/empty,200000,100000,0,0,")
        .unwrap();
    let disks = f["disks"].as_array().unwrap();
    assert_eq!(disks[0]["dev"], "/data");
    assert_eq!(disks[1]["dev"], "/empty");
}

#[test]
fn missing_sections_openwrt_style() {
    let mut p = StreamParser::new();
    let f = p
        .parse("T=1000.0|U=100|CPU=t:0,0,0,0,0,0,0,0;0:0,0,0,0,0,0,0,0|MEM=65536,32000,1000,4000")
        .unwrap();
    assert_eq!(f["cpu"]["cores"], 1);
    assert_eq!(f["temperatures"], serde_json::json!([]));
    assert_eq!(f["disks"], serde_json::json!([]));
    assert_eq!(f["network"], serde_json::json!([]));
    assert!(f["os"].is_null());
    assert_eq!(f["memory"]["total_kb"], 65536);
}

#[test]
fn corrupted_segment_does_not_kill_frame() {
    let mut p = StreamParser::new();
    let f = p
        .parse("T=1000.0|MEM=not_a_number|CPU=t:1,2,3,4,5,6,7,8|OS=Debian 12")
        .unwrap();
    assert!(f["memory"].is_null());
    assert_eq!(f["os"]["pretty_name"], "Debian 12");
    assert!(f["cpu"]["cores"].is_null()); // 只有 t 无核
    let f2 = p.parse("T=1001.0|MEM=100,50,10,20|OS=Debian 12").unwrap();
    assert_eq!(f2["memory"]["total_kb"], 100);
}

#[test]
fn counter_reset_gives_none_rate() {
    let mut p = StreamParser::new();
    p.parse(&sample_full(1000.0));
    let f = p.parse(&sample_next(1001.0)).unwrap();
    assert_eq!(f["cpu"]["percent"], 50.0);
    // 计数器回绕（小于上次）
    let f2 = p
        .parse("T=1002.0|CPU=t:10,0,10,20,0,0,0,0;0:5,0,5,10,0,0,0,0;1:5,0,5,10,0,0,0,0")
        .unwrap();
    assert!(f2["cpu"]["percent"].is_null());
    // 之后恢复正常差值
    let f3 = p
        .parse("T=1003.0|CPU=t:60,0,60,70,0,0,0,0;0:30,0,30,35,0,0,0,0;1:30,0,30,35,0,0,0,0")
        .unwrap();
    assert_eq!(f3["cpu"]["percent"], 66.7);
}

#[test]
fn empty_and_garbage() {
    assert!(StreamParser::new().parse("").is_none());
    assert!(StreamParser::new().parse("   \n").is_none());
    let f = StreamParser::new().parse("garbage line without segments").unwrap();
    assert!(f["cpu"].is_null());
}

// ---------- Docker 段 ----------

const DKP: &str = "DKP=abc123def456,web-server,nginx:latest,running,Up 3 days (healthy);fed654321098,old-test,alpine:3.19,exited,Exited (0) 2 weeks ago";
const DKST: &str = "DKST=abc123def456,3.21%,45.2MiB / 1.941GiB,2.28%,1.2kB / 3.4kB,10.3MB / 0B";

#[test]
fn docker_full_frame() {
    let f = StreamParser::new()
        .parse(&format!("T=1000.0|DKS=ok,1,2|{}|{}", DKP, DKST))
        .unwrap();
    let dk = &f["docker"];
    assert_eq!(dk["daemon"], "ok");
    assert_eq!(dk["running"], 1);
    assert_eq!(dk["total"], 2);
    let cs = dk["containers"].as_array().unwrap();
    assert_eq!(cs.len(), 2);
    let web = &cs[0];
    assert_eq!(web["id"], "abc123def456");
    assert_eq!(web["name"], "web-server");
    assert_eq!(web["image"], "nginx:latest");
    assert_eq!(web["state"], "running");
    assert_eq!(web["status"], "Up 3 days (healthy)");
    assert_eq!(web["cpu_percent"], 3.21);
    assert_eq!(web["mem_used"], "45.2MiB");
    assert_eq!(web["mem_limit"], "1.941GiB");
    assert_eq!(web["mem_percent"], 2.28);
    assert_eq!(web["net_io"], "1.2kB / 3.4kB");
    assert_eq!(web["block_io"], "10.3MB / 0B");
    // 停止的容器没有 stats：键在、值为 null
    assert_eq!(cs[1]["state"], "exited");
    assert!(cs[1]["cpu_percent"].is_null());
    assert!(cs[1]["mem_used"].is_null());
}

#[test]
fn docker_status_with_comma() {
    let f = StreamParser::new()
        .parse("T=1000.0|DKS=ok,0,1|DKP=abc123def456,x,alpine,exited,Exited (0), 2 weeks ago")
        .unwrap();
    let c = &f["docker"]["containers"][0];
    assert_eq!(c["state"], "exited");
    assert_eq!(c["status"], "Exited (0), 2 weeks ago");
}

#[test]
fn docker_none_and_down() {
    let f = StreamParser::new().parse("T=1000.0|DKS=none").unwrap();
    assert_eq!(
        f["docker"],
        serde_json::json!({"daemon": "none", "running": 0, "total": 0, "containers": []})
    );
    let f = StreamParser::new().parse("T=1000.0|DKS=down").unwrap();
    assert_eq!(f["docker"]["daemon"], "down");
    let f = StreamParser::new().parse("T=1000.0|DKS=ok,0,0").unwrap();
    assert_eq!(f["docker"]["daemon"], "ok");
    assert_eq!(f["docker"]["containers"], serde_json::json!([]));
    // 旧脚本没有任何 DK 段 → docker 为 null
    let f = StreamParser::new().parse(&sample_full(1000.0)).unwrap();
    assert!(f["docker"].is_null());
}

#[test]
fn docker_corrupted_segment_does_not_kill_frame() {
    let f = StreamParser::new()
        .parse("T=1000.0|MEM=100,50,10,20|DKS=bogus,1,2|DKP=garbage-no-commas")
        .unwrap();
    assert_eq!(f["memory"]["total_kb"], 100);
    assert!(f["docker"].is_null());
    let f2 = StreamParser::new()
        .parse(&format!("T=1001.0|DKS=ok,1,1|{}", DKP))
        .unwrap();
    assert_eq!(f2["docker"]["running"], 1);
}

#[test]
fn docker_stats_only_frame_keeps_running_container_without_stats() {
    let f = StreamParser::new()
        .parse(&format!("T=1000.0|DKS=ok,1,1|{}", DKP))
        .unwrap();
    let c = &f["docker"]["containers"][0];
    assert_eq!(c["name"], "web-server");
    assert!(c["cpu_percent"].is_null());
    assert!(c["mem_used"].is_null());
}

// ---------- 风扇段 ----------

#[test]
fn fan_full_and_zero_rpm() {
    let f = StreamParser::new()
        .parse("T=1000.0|FAN=nct6775:fan1,1820;nct6775:fan2,0|OS=OpenWrt 24.10")
        .unwrap();
    let fans = f["fans"].as_array().unwrap();
    assert_eq!(fans.len(), 2);
    assert_eq!(fans[0], serde_json::json!({"name": "nct6775:fan1", "rpm": 1820}));
    assert_eq!(fans[1], serde_json::json!({"name": "nct6775:fan2", "rpm": 0}));
}

#[test]
fn fan_missing_and_corrupted() {
    let f = StreamParser::new().parse(&sample_full(1000.0)).unwrap();
    assert_eq!(f["fans"], serde_json::json!([]));
    let f = StreamParser::new()
        .parse("T=1000.0|FAN=cpu_fan,1800;bad-entry-no-rpm;,x|MEM=100,50,10,20")
        .unwrap();
    assert_eq!(f["fans"], serde_json::json!([{"name": "cpu_fan", "rpm": 1800}]));
    assert_eq!(f["memory"]["total_kb"], 100);
}

// ---------- beta2.3：多网卡 / 大计数 / 溢出防御 ----------

const REAL_NET: &str = "eth0,1188580000,8231000000,up;\
eth1,10470454000,1170990000,up;\
br-lan,10521892500,8660519000,up;\
utun,10321844000,9751590000,unknown;\
docker0,0,0,down";

#[test]
fn multi_nic_counters_above_2_31_are_positive_and_primary_is_unique() {
    let f = StreamParser::new().parse(&format!("T=1000.0|NET={}", REAL_NET)).unwrap();
    let nets = f["network"].as_array().unwrap();
    let ifaces: Vec<&str> = nets.iter().map(|n| n["iface"].as_str().unwrap()).collect();
    assert_eq!(ifaces, ["eth0", "eth1", "br-lan", "utun", "docker0"]);
    for n in &nets[..4] {
        assert!(n["rx_total_kb"].as_f64().unwrap() > 0.0);
        assert!(n["tx_total_kb"].as_f64().unwrap() > 0.0);
    }
    assert!(nets[2]["rx_total_kb"].as_f64().unwrap() > (2f64.powi(31) / 1024.0));
    let primaries: Vec<&str> = nets
        .iter()
        .filter(|n| n["primary"].as_bool().unwrap_or(false))
        .map(|n| n["iface"].as_str().unwrap())
        .collect();
    assert_eq!(primaries, ["br-lan"]);
    assert_eq!(nets[3]["state"], "unknown");
    assert_eq!(nets[4]["state"], "down");
    // 首帧没有速率，历史不落盘脏点
    let metrics: Vec<String> = frame_metrics(&f, "h1").into_iter().map(|(_, _, m, _)| m).collect();
    assert!(!metrics.contains(&"net.rx_kbps".to_string()));
}

#[test]
fn multi_nic_second_frame_every_interface_has_its_own_positive_rate() {
    let mut p = StreamParser::new();
    p.parse(&format!("T=1000.0|NET={}", REAL_NET));
    let bumped = REAL_NET
        .replace("eth0,1188580000,", "eth0,1188581000,")
        .replace("eth1,10470454000,", "eth1,10470455000,")
        .replace("br-lan,10521892500,", "br-lan,10521893500,")
        .replace("utun,10321844000,", "utun,10321845000,");
    let f = p.parse(&format!("T=1001.0|NET={}", bumped)).unwrap();
    let nets = f["network"].as_array().unwrap();
    assert_eq!(nets.len(), 5);
    for n in nets {
        let iface = n["iface"].as_str().unwrap();
        if ["eth0", "eth1", "br-lan", "utun"].contains(&iface) {
            assert!(n["rx_kbps"].as_f64().unwrap() > 0.0, "{}", iface);
            assert!(n["tx_kbps"].as_f64().unwrap() >= 0.0, "{}", iface);
        }
    }
    let pts: std::collections::HashMap<String, f64> = frame_metrics(&f, "h1")
        .into_iter()
        .map(|(_, _, m, v)| (m, v))
        .collect();
    let brlan = nets.iter().find(|n| n["iface"] == "br-lan").unwrap();
    assert_eq!(pts["net.rx_kbps"], brlan["rx_kbps"].as_f64().unwrap());
}

#[test]
fn negative_counters_are_rejected() {
    let f = StreamParser::new()
        .parse("T=1000.0|NET=eth0,-2097152000,3000,up;eth1,1000,2000,up")
        .unwrap();
    let nets = f["network"].as_array().unwrap();
    assert_eq!(nets.len(), 1);
    assert_eq!(nets[0]["iface"], "eth1");
    assert_eq!(nets[0]["primary"], true);
}

#[test]
fn legacy_three_field_net_is_still_accepted() {
    let f = StreamParser::new().parse("T=1000.0|NET=eth0,1000,2000").unwrap();
    assert!(f["network"][0]["state"].is_null());
    assert_eq!(f["network"][0]["primary"], true);
}

#[test]
fn pick_primary_prefers_up_real_iface() {
    let nets = serde_json::json!([
        {"iface": "docker0", "state": "up", "rx_total_kb": 999999.0, "tx_total_kb": 999999.0},
        {"iface": "br-lan", "state": "up", "rx_total_kb": 100.0, "tx_total_kb": 100.0},
        {"iface": "eth0", "state": "down", "rx_total_kb": 888888.0, "tx_total_kb": 1.0}
    ]);
    let arr = nets.as_array().unwrap();
    let i = pick_primary_network(arr).unwrap();
    assert_eq!(arr[i]["iface"], "br-lan");
    // 全 down 时回退到全集，但仍优先"真实接口"前缀（eth0），不按流量选 tunl0
    let nets2 = serde_json::json!([
        {"iface": "eth0", "state": "down", "rx_total_kb": 1.0, "tx_total_kb": 1.0},
        {"iface": "tunl0", "state": "down", "rx_total_kb": 5.0, "tx_total_kb": 5.0}
    ]);
    let arr2 = nets2.as_array().unwrap();
    let i2 = pick_primary_network(arr2).unwrap();
    assert_eq!(arr2[i2]["iface"], "eth0");
    let _ = Value::Null;
}

// ---------- GPU 段（beta2.4） ----------

#[test]
fn gpu_segment_parses_all_fields() {
    let mut p = StreamParser::new();
    let line = "T=1000|GPU=NVIDIA GeForce RTX 3060,nvidia,35,2048,12288,56.5,115.2\
                ;AMD Radeon 780M (card1),amdgpu,12,512,4096,49.2,";
    let f = p.parse(line).unwrap();
    let gpus = f["gpus"].as_array().unwrap();
    assert_eq!(gpus.len(), 2);
    assert_eq!(gpus[0]["name"], "NVIDIA GeForce RTX 3060");
    assert_eq!(gpus[0]["driver"], "nvidia");
    assert_eq!(gpus[0]["busy_percent"], 35.0);
    assert_eq!(gpus[0]["mem_used_mb"], 2048.0);
    assert_eq!(gpus[0]["mem_total_mb"], 12288.0);
    assert_eq!(gpus[0]["mem_percent"], 16.7); // 100*2048/12288
    assert_eq!(gpus[0]["temp_c"], 56.5);
    assert_eq!(gpus[0]["power_w"], 115.2);
    // 缺失字段一律 null（末尾功耗为空）
    assert_eq!(gpus[1]["power_w"], Value::Null);
    assert_eq!(gpus[1]["mem_percent"], 12.5);
}

#[test]
fn gpu_segment_absent_or_empty() {
    let mut p = StreamParser::new();
    // 没有 GPU 段：空数组而不是缺字段，前端好判空
    let f = p.parse("T=1000|OS=Linux").unwrap();
    assert_eq!(f["gpus"], serde_json::json!([]));
    // 空段 / 只有名称没有数值：容错不崩
    let f2 = p.parse("T=1000|GPU=").unwrap();
    assert_eq!(f2["gpus"], serde_json::json!([]));
    let f3 = p.parse("T=1000|GPU=Some Card,nvidia,abc,,,").unwrap();
    let g = f3["gpus"].as_array().unwrap();
    assert_eq!(g.len(), 1);
    assert_eq!(g[0]["busy_percent"], Value::Null);
    assert_eq!(g[0]["name"], "Some Card");
}

#[test]
fn gpu_metrics_into_store() {
    let mut p = StreamParser::new();
    let f = p
        .parse("T=1000|GPU=CardA,nvidia,80,2048,8192,70.0,120.0;CardB,amdgpu,40,1024,4096,50.0,30.0")
        .unwrap();
    let pts = frame_metrics(&f, "h");
    let get = |m: &str| {
        pts.iter()
            .find(|(_, _, k, _)| k == m)
            .map(|(_, _, _, v)| *v)
    };
    // 汇总口径：占用/显存/温度取最大，功耗取总和
    assert_eq!(get("gpu.busy_percent"), Some(80.0));
    assert_eq!(get("gpu.mem_used_percent"), Some(25.0));
    assert_eq!(get("gpu.temp_c"), Some(70.0));
    assert_eq!(get("gpu.power_w"), Some(150.0));
}

// ---------- 进程 / 连接段（beta1.4.3） ----------

#[test]
fn proc_segments_full_frame() {
    let mut p = StreamParser::new();
    let f = p
        .parse(
            "T=1000|MEM=1048576,524288,65536,262144\
             |PSUM=128,2,1\
             |PTOP=1234,55.5,380000,dockerd;99,,1200,(sd-pam)\
             |CON=24,60,12,8",
        )
        .unwrap();
    let ps = f["proc_summary"].clone();
    assert_eq!(ps["total"], 128);
    assert_eq!(ps["running"], 2);
    assert_eq!(ps["zombie"], 1);

    let procs = f["processes"].as_array().unwrap();
    assert_eq!(procs.len(), 2);
    assert_eq!(procs[0]["pid"], 1234);
    assert_eq!(procs[0]["name"], "dockerd");
    assert_eq!(procs[0]["cpu_percent"], 55.5);
    // mem_percent 用同帧 MEM 总量换算：380000/1048576 ≈ 36.2%
    assert_eq!(procs[0]["mem_percent"], 36.2);
    assert_eq!(procs[0]["rss_kb"], 380000.0);
    // 首帧 cpu% 为空 → null；名字里的括号原样保留
    assert_eq!(procs[1]["cpu_percent"], Value::Null);
    assert_eq!(procs[1]["name"], "(sd-pam)");

    let con = f["net_connections"].clone();
    assert_eq!(con["established"], 24);
    assert_eq!(con["time_wait"], 60);
    assert_eq!(con["listen"], 12);
    assert_eq!(con["udp"], 8);
}

#[test]
fn proc_segments_missing_leave_defaults() {
    let mut p = StreamParser::new();
    let f = p.parse("T=1000|MEM=1048576,524288,65536,262144").unwrap();
    assert_eq!(f["processes"], serde_json::json!([]));
    assert!(f["proc_summary"].is_null());
    assert!(f["net_connections"].is_null());
}

#[test]
fn proc_segments_corrupted_do_not_kill_frame() {
    let mut p = StreamParser::new();
    let f = p.parse("T=1000|U=10|PSUM=x|PTOP=abc,def|CON=").unwrap();
    assert_eq!(f["uptime_s"], 10);
    assert!(f["proc_summary"].is_null());
    assert_eq!(f["processes"], serde_json::json!([]));
    assert!(f["net_connections"].is_null());
}

#[test]
fn proc_mem_percent_null_without_mem_total() {
    let mut p = StreamParser::new();
    let f = p.parse("T=1000|PTOP=1,2.0,4096,bash").unwrap();
    let procs = f["processes"].as_array().unwrap();
    assert_eq!(procs.len(), 1);
    assert_eq!(procs[0]["mem_percent"], Value::Null);
    assert_eq!(procs[0]["cpu_percent"], 2.0);
}

#[test]
fn proc_and_con_metrics_into_store() {
    let mut p = StreamParser::new();
    let f = p
        .parse("T=1000|PSUM=128,2,1|PTOP=1,50.0,4096,bash|CON=24,60,12,8")
        .unwrap();
    let pts = frame_metrics(&f, "h");
    let get = |m: &str| {
        pts.iter()
            .find(|(_, _, k, _)| k == m)
            .map(|(_, _, _, v)| *v)
    };
    assert_eq!(get("proc.count"), Some(128.0));
    assert_eq!(get("proc.running"), Some(2.0));
    assert_eq!(get("proc.zombie"), Some(1.0));
    assert_eq!(get("con.established"), Some(24.0));
    assert_eq!(get("con.time_wait"), Some(60.0));
    assert_eq!(get("con.listen"), Some(12.0));
}
