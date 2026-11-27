//! 模拟采集器：生成合成数据（移植自 collector/mock.py）。
//!
//! 用途：无目标机时演示界面、自动化测试、前端联调。

use serde_json::{json, Value};
use std::f64::consts::PI;
use std::sync::Arc;
use std::time::Instant;

use crate::now_ts;
use crate::state::{CollectorEvent, EventTx};

pub struct MockCollector {
    pub name: String,
    pub interval: f64,
    pub dropout: bool,
}

const DROP_PERIOD: f64 = 120.0;
const DROP_DURATION: f64 = 12.0;

impl MockCollector {
    pub fn new(name: &str, collect_cfg: &Value, dropout: bool) -> Self {
        Self {
            name: name.to_string(),
            interval: collect_cfg
                .get("interval_s")
                .and_then(|v| v.as_f64())
                .unwrap_or(1.0)
                .max(0.05),
            dropout,
        }
    }

    pub fn spawn(self: Arc<Self>, tx: EventTx) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let t0 = Instant::now();
            let name = self.name.clone();
            let send_status = |s: &str| {
                let _ = tx.send(CollectorEvent::Status { name: name.clone(), status: s.into() });
            };
            send_status("online");
            let mut n: u64 = 0;
            let mut offline = false;
            loop {
                if self.dropout {
                    let phase = t0.elapsed().as_secs_f64() % DROP_PERIOD;
                    if phase <= DROP_DURATION {
                        if !offline {
                            offline = true;
                            send_status("offline");
                        }
                        tokio::time::sleep(std::time::Duration::from_secs_f64(self.interval)).await;
                        continue;
                    }
                    if offline {
                        offline = false;
                        send_status("online");
                    }
                }
                let frame = self.gen_frame(n);
                let _ = tx.send(CollectorEvent::Frame { name: self.name.clone(), frame });
                n += 1;
                tokio::time::sleep(std::time::Duration::from_secs_f64(self.interval)).await;
            }
        })
    }

    fn gen_frame(&self, n: u64) -> Value {
        let t = n as f64 * self.interval;
        let wob = |period: f64, amp: f64| amp * (2.0 * PI * t / period).sin();
        let cpu = (30.0 + wob(60.0, 20.0) + wob(7.0, 5.0)).clamp(2.0, 99.0);
        let per_core: Vec<f64> = (0..4)
            .map(|i| (cpu + wob(11.0 + i as f64 * 3.0, 10.0)).clamp(1.0, 100.0))
            .collect();
        let total_kb = 1024.0 * 1000.0;
        let avail = (total_kb * (0.62 + wob(90.0, 0.12))) as i64;
        let temp = 45.0 + wob(120.0, 4.0);

        json!({
            "ts": now_ts(),
            "status": "online",
            "os": {"pretty_name": "OpenWrt 23.05.2 (模拟数据)"},
            "uptime_s": (3600 * 41) as i64 + t as i64,
            "cpu": {
                "percent": crate::r1(cpu),
                "cores": 4,
                "per_core": per_core.iter().map(|x| crate::r1(*x)).collect::<Vec<_>>(),
                "load": [crate::r2(cpu / 40.0), crate::r2(cpu / 50.0), crate::r2(cpu / 60.0)],
            },
            "memory": {
                "total_kb": total_kb as i64,
                "available_kb": avail,
                "buffers_kb": 64 * 1024,
                "cached_kb": 180 * 1024,
                "available_percent": crate::r1(100.0 * avail as f64 / total_kb),
            },
            "temperatures": [
                {"zone": "thermal_zone0", "type": "soc-thermal", "temp_c": crate::r1(temp)},
                {"zone": "thermal_zone1", "type": "x86_pkg_temp", "temp_c": crate::r1(temp - 3.5)},
            ],
            "fans": [
                {"name": "nct6775:fan1", "rpm": (1820.0 + wob(47.0, 160.0)).max(0.0) as i64},
                {"name": "nct6775:fan2", "rpm": (940.0 + wob(83.0, 60.0)).max(0.0) as i64},
            ],
            "gpus": [
                {"name": "NVIDIA GeForce RTX 3060", "driver": "nvidia",
                 "busy_percent": crate::r1((35.0 + wob(29.0, 25.0)).clamp(0.0, 100.0)),
                 "mem_used_mb": 2048.0 + 900.0 * wob(41.0, 0.5),
                 "mem_total_mb": 12288.0,
                 "mem_percent": crate::r1(100.0 * (2048.0 + 900.0 * wob(41.0, 0.5)) / 12288.0),
                 "temp_c": crate::r1(56.0 + wob(73.0, 6.0)),
                 "power_w": crate::r1((115.0 + wob(37.0, 60.0)).max(5.0))},
                {"name": "AMD Radeon 780M (card1)", "driver": "amdgpu",
                 "busy_percent": crate::r1((12.0 + wob(53.0, 10.0)).clamp(0.0, 100.0)),
                 "mem_used_mb": 512.0,
                 "mem_total_mb": 4096.0,
                 "mem_percent": 12.5,
                 "temp_c": crate::r1(49.0 + wob(67.0, 3.0)),
                 "power_w": Value::Null},
            ],
            "disks": [
                {"mount": "/", "dev": "/dev/sda1", "total_kb": 8 * 1024 * 1024i64, "available_kb": (8.0 * 1024.0 * 1024.0 * 0.72) as i64,
                 "read_kbps": crate::r1((1200.0 + wob(23.0, 800.0)).max(0.0)),
                 "write_kbps": crate::r1((350.0 + wob(37.0, 220.0)).max(0.0))},
                {"mount": "/mnt/data", "dev": "/dev/sdb1", "total_kb": 64 * 1024 * 1024i64, "available_kb": (64.0 * 1024.0 * 1024.0 * 0.4) as i64,
                 "read_kbps": crate::r1((8200.0 + wob(17.0, 3000.0)).max(0.0)),
                 "write_kbps": crate::r1((1500.0 + wob(29.0, 900.0)).max(0.0))},
            ],
            "network": [
                {"iface": "eth0", "state": "up", "primary": false,
                 "rx_kbps": crate::r1((8500.0 + wob(45.0, 6000.0)).max(0.0)),
                 "tx_kbps": crate::r1((2200.0 + wob(33.0, 1500.0)).max(0.0)),
                 "rx_total_kb": crate::r1(98765432.0 + t * 800.0),
                 "tx_total_kb": crate::r1(12345678.0 + t * 260.0)},
                {"iface": "eth1", "state": "up", "primary": false,
                 "rx_kbps": crate::r1((940.0 + wob(51.0, 500.0)).max(0.0)),
                 "tx_kbps": crate::r1((310.0 + wob(29.0, 180.0)).max(0.0)),
                 "rx_total_kb": crate::r1(5432100.0 + t * 90.0),
                 "tx_total_kb": crate::r1(7654321.0 + t * 40.0)},
                {"iface": "br-lan", "state": "up", "primary": true,
                 "rx_kbps": crate::r1((4300.0 + wob(47.0, 2600.0)).max(0.0)),
                 "tx_kbps": crate::r1((5100.0 + wob(31.0, 2200.0)).max(0.0)),
                 "rx_total_kb": crate::r1(188765432.0 + t * 1200.0),
                 "tx_total_kb": crate::r1(22345678.0 + t * 640.0)},
                {"iface": "tunl0", "state": "unknown", "primary": false,
                 "rx_kbps": 0.0, "tx_kbps": 0.0, "rx_total_kb": 0, "tx_total_kb": 0},
            ],
            "proc_summary": {
                "total": 128,
                "running": 2 + ((t / 30.0) as i64 % 3),
                "zombie": (t / 200.0) as i64 % 2,
            },
            "processes": [
                {"pid": 1234, "cpu_percent": crate::r1((22.0 + wob(23.0, 18.0)).clamp(0.0, 99.0)),
                 "mem_percent": crate::r1((18.5 + wob(71.0, 3.0)).clamp(0.1, 99.0)),
                 "rss_kb": 380000.0 + 40000.0 * wob(71.0, 1.0).abs(), "name": "dockerd"},
                {"pid": 1288, "cpu_percent": crate::r1((11.0 + wob(37.0, 9.0)).clamp(0.0, 99.0)),
                 "mem_percent": crate::r1((9.2 + wob(53.0, 2.0)).clamp(0.1, 99.0)),
                 "rss_kb": 188000.0 + 12000.0 * wob(53.0, 1.0).abs(), "name": "python3 main.py"},
                {"pid": 2201, "cpu_percent": crate::r1((6.0 + wob(29.0, 5.0)).clamp(0.0, 99.0)),
                 "mem_percent": crate::r1((4.1 + wob(41.0, 1.0)).clamp(0.1, 99.0)),
                 "rss_kb": 84000.0, "name": "nginx worker"},
                {"pid": 3010, "cpu_percent": crate::r1((2.0 + wob(67.0, 2.0)).clamp(0.0, 99.0)),
                 "mem_percent": crate::r1((1.2 + wob(47.0, 0.5)).clamp(0.1, 99.0)),
                 "rss_kb": 25000.0, "name": "dropbear"},
                {"pid": 1, "cpu_percent": 0.1, "mem_percent": 0.3, "rss_kb": 6200.0, "name": "init"},
            ],
            "net_connections": {
                "established": 24 + ((t / 17.0) as i64 % 11),
                "time_wait": 60 + ((t / 7.0) as i64 % 40),
                "listen": 12,
                "udp": 8,
            },
            "docker": {
                "daemon": "ok",
                "running": 3,
                "total": 4,
                "containers": [
                    {"id": "a1b2c3d4e5f6", "name": "nginx-proxy", "image": "nginx:1.27",
                     "state": "running", "status": "Up 3 days (healthy)",
                     "cpu_percent": crate::r2((4.0 + wob(37.0, 3.0)).max(0.1)),
                     "mem_used": format!("{:.1}MiB", 48.0 + 6.0 * wob(53.0, 1.0).abs()),
                     "mem_limit": "1.9GiB", "mem_percent": 2.4,
                     "net_io": format!("{:.1}kB / {:.1}kB", 120.0 + wob(29.0, 40.0), 95.0 + wob(23.0, 30.0)),
                     "block_io": "10.3MB / 0B"},
                    {"id": "b2c3d4e5f6a1", "name": "homeassistant", "image": "homeassistant/home-assistant:stable",
                     "state": "running", "status": "Up 12 days",
                     "cpu_percent": crate::r2((11.0 + wob(61.0, 7.0)).max(0.2)),
                     "mem_used": format!("{:.0}MiB", 380.0 + 40.0 * wob(71.0, 1.0).abs()),
                     "mem_limit": "1.9GiB", "mem_percent": 19.6,
                     "net_io": "2.1MB / 5.6MB", "block_io": "156MB / 89MB"},
                    {"id": "c3d4e5f6a1b2", "name": "portainer", "image": "portainer/portainer-ce:latest",
                     "state": "running", "status": "Up 12 days",
                     "cpu_percent": crate::r2((0.6 + wob(43.0, 0.4)).max(0.05)),
                     "mem_used": "32.5MiB", "mem_limit": "1.9GiB", "mem_percent": 1.6,
                     "net_io": "860kB / 1.2MB", "block_io": "21MB / 3.4MB"},
                    {"id": "d4e5f6a1b2c3", "name": "old-experiment", "image": "alpine:3.19",
                     "state": "exited", "status": "Exited (0) 2 weeks ago"},
                ],
            },
        })
    }
}
