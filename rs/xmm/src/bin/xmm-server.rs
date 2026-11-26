//! xmm-server —— 主机监控小助手 Rust 后端（独立服务模式 / 浏览器版）。
//!
//! 用法对应 Python 的 run.py：
//!   xmm-server [--config config.toml] [--data-dir DIR] [--host H] [--port P]
//!              [--mock] [--mock-dropout] [--open-browser]

use std::path::PathBuf;

use xmm::config;
use xmm::state::State;
use xmm::VERSION;

fn parse_args() -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--mock" | "--mock-dropout" | "--open-browser" => {
                out.insert(a.trim_start_matches('-').to_string(), "true".into());
            }
            "--config" | "--data-dir" | "--host" | "--port" => {
                if let Some(v) = it.next() {
                    out.insert(a.trim_start_matches('-').to_string(), v);
                }
            }
            other if other.starts_with("--") => {
                if let Some((k, v)) = other[2..].split_once('=') {
                    out.insert(k.to_string(), v.to_string());
                }
            }
            _ => {}
        }
    }
    out
}

/// 简单滚动日志：server.log 超过 1MB 时轮转成 .1/.2（对齐 Python RotatingFileHandler）。
fn setup_logging(data_dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(data_dir);
    let log_path = data_dir.join("server.log");
    if let Ok(meta) = std::fs::metadata(&log_path) {
        if meta.len() > 1_000_000 {
            let bak1 = data_dir.join("server.log.1");
            let bak2 = data_dir.join("server.log.2");
            let _ = std::fs::rename(&bak1, &bak2);
            let _ = std::fs::rename(&log_path, &bak1);
        }
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();

    let env = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    match file {
        Some(f) => {
            let (writer, _guard) = tracing_appender_non_blocking(f);
            tracing_subscriber::fmt()
                .with_env_filter(env)
                .with_writer(move || writer.clone())
                .with_ansi(false)
                .init();
        }
        None => {
            tracing_subscriber::fmt().with_env_filter(env).init();
        }
    }
}

// 给 tracing 写一个最小的非阻塞 writer（避免再引一个 crate）
struct NonBlocking(std::sync::Arc<std::sync::Mutex<std::fs::File>>);
impl std::io::Write for NonBlocking {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().map(|mut f| std::io::Write::write(&mut *f, buf)).unwrap_or(Ok(buf.len()))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Clone for NonBlocking {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
fn tracing_appender_non_blocking(f: std::fs::File) -> (NonBlocking, ()) {
    (NonBlocking(std::sync::Arc::new(std::sync::Mutex::new(f))), ())
}

#[tokio::main]
async fn main() {
    let args = parse_args();

    let data_dir = args
        .get("data-dir")
        .map(PathBuf::from)
        .or_else(|| args.get("config").map(|c| PathBuf::from(c).parent().unwrap().to_path_buf()))
        .unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("."))
        });
    let data_dir = std::fs::canonicalize(&data_dir).unwrap_or(data_dir);
    let _ = std::fs::create_dir_all(&data_dir);
    setup_logging(&data_dir);

    let cfg_path = args
        .get("config")
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("config.toml"));
    let mut cfg = config::load_or_create(&cfg_path);
    if args.contains_key("mock") {
        cfg.as_object_mut().unwrap().insert("_mock".into(), serde_json::json!(true));
    }
    if args.contains_key("mock-dropout") {
        cfg.as_object_mut().unwrap().insert("_mock_dropout".into(), serde_json::json!(true));
    }

    let host = args
        .get("host")
        .cloned()
        .or_else(|| cfg.get("server").and_then(|s| s.get("listen_host")).and_then(|v| v.as_str()).map(|s| s.to_string()))
        .unwrap_or_else(|| "0.0.0.0".into());
    let port: u16 = args
        .get("port")
        .and_then(|p| p.parse().ok())
        .or_else(|| cfg.get("server").and_then(|s| s.get("listen_port")).and_then(|v| v.as_i64()).map(|v| v as u16))
        .unwrap_or(8787);

    // web/ 目录：exe 同级的 web/，或开发时项目根的 web/
    let web_dir = ["web", "../../web", "../../../web", "../../../../web"]
        .iter()
        .map(|rel| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join(rel)))
                .unwrap_or_default()
        })
        .find(|p| p.join("index.html").exists())
        .unwrap_or_else(|| PathBuf::from("web"));

    let (st, rx) = State::new(cfg.clone(), &cfg_path, Some(&data_dir));
    st.spawn_dispatcher(rx);
    st.start_background();

    let username = cfg
        .get("auth")
        .and_then(|a| a.get("username"))
        .and_then(|v| v.as_str())
        .unwrap_or("admin");
    let password_hint = if std::env::var("MON_WEB_PASSWORD").is_ok() {
        "环境变量 MON_WEB_PASSWORD".to_string()
    } else {
        cfg.get("auth")
            .and_then(|a| a.get("password"))
            .and_then(|v| v.as_str())
            .unwrap_or("admin123")
            .to_string()
    };
    let targets: Vec<String> = cfg
        .get("targets")
        .and_then(|t| t.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    let sep = "=".repeat(56);
    let mut banner = vec![
        sep.clone(),
        format!("  主机监控小助手  {}   http://127.0.0.1:{}", VERSION, port),
        format!("  配置文件： {}", cfg_path.display()),
        format!("  数据目录： {}", data_dir.display()),
        format!("  网页登录： {} / {}（可在设置里改）", username, password_hint),
        format!("  监控目标： {}", if targets.is_empty() { "(无，请在界面里添加)".into() } else { targets.join(", ") }),
    ];
    if args.contains_key("mock") {
        banner.push("  当前为模拟数据模式（--mock）".into());
    }
    banner.push(sep);
    println!("{}", banner.join("\n"));

    if args.contains_key("open-browser") {
        let url = format!("http://127.0.0.1:{}", port);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
            let _ = open_url(&url);
        });
    }

    let app = xmm::api::router(st, &web_dir);

    let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("端口 {} 被占用或无权监听：{}", port, e);
            tracing::error!("端口 {} 被占用或无权监听：{}", port, e);
            std::process::exit(2);
        }
    };
    tracing::info!("服务已启动，监听 {}:{}", host, port);
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!("服务异常退出：{}", e);
    }
}

#[cfg(windows)]
fn open_url(url: &str) -> std::io::Result<()> {
    std::process::Command::new("cmd")
        .args(["/c", "start", "", url])
        .spawn()?;
    Ok(())
}

#[cfg(not(windows))]
fn open_url(url: &str) -> std::io::Result<()> {
    std::process::Command::new("xdg-open").arg(url).spawn()?;
    Ok(())
}
