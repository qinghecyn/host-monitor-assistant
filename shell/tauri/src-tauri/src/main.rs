// 主机监控小助手 —— Tauri 桌面壳（Rust 后端内嵌版）
//
// 与旧版（beta2.3 / Python sidecar）最大的不同：
//   旧版：壳进程拉起 PyInstaller 打包的 xiaozhushou-server.exe 子进程，
//         Python 运行时 + FastAPI + asyncssh 常驻，实测 ~75MB 内存、124MB 磁盘。
//   现在：Rust 后端（xmm crate）与壳在**同一个进程**里跑 axum，
//         不再有子进程、不再有 Python 运行时，托盘/免登录/退出联动保持不变。
//
//   1. 挑空闲端口，生成本次运行的一次性令牌，注入 XMM_DESKTOP_TOKEN；
//   2. 专用线程起 tokio runtime，跑 axum（/api、/ws、静态 web/）+ 采集器；
//   3. 轮询 /api/ping 就绪后创建窗口，注入令牌自动登录；
//   4. 托盘常驻：关窗口 = 收进托盘；界面"退出"→ /api/shutdown → 整个进程退出。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};

static QUITTING: AtomicBool = AtomicBool::new(false);
/// 单实例锁端口。可用 XMM_INSTANCE_PORT 覆盖（并行测试/多份安装时用）。
fn instance_port() -> u16 {
    std::env::var("XMM_INSTANCE_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(47823)
}

struct Backend {
    port: u16,
    token: String,
}

// ---------------------------------------------------------------- 工具函数

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(8787)
}

fn gen_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let mix = nanos ^ (pid << 61) ^ (nanos >> 13);
    format!("{:032x}{:032x}", mix, mix.rotate_left(29))
}

/// 后端是否已经能应答 /api/ping
fn ping(port: u16) -> bool {
    let addr = match format!("127.0.0.1:{}", port).parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_millis(800)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(1200)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(800)));
    if stream
        .write_all(b"GET /api/ping HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut buf = String::new();
    let _ = stream.read_to_string(&mut buf);
    buf.contains("\"ok\":true")
}

/// web/ 静态资源目录：安装包里是 resources/web，开发时是项目根 web/。
fn locate_web(app: &AppHandle) -> PathBuf {
    if let Ok(res) = app.path().resource_dir() {
        let p = res.join("web");
        if p.join("index.html").exists() {
            return p;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../web")
}

/// 简单文件日志（无控制台也能查问题）。
fn setup_logging(data_dir: &PathBuf) {
    let _ = std::fs::create_dir_all(data_dir);
    let path = data_dir.join("server.log");
    if let Ok(m) = std::fs::metadata(&path) {
        if m.len() > 1_000_000 {
            let _ = std::fs::rename(&path, data_dir.join("server.log.1"));
        }
    }
    let Some(f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()
    else {
        return;
    };
    let file = Arc::new(std::sync::Mutex::new(f));
    #[derive(Clone)]
    struct Writer(Arc<std::sync::Mutex<std::fs::File>>);
    impl std::io::Write for Writer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            match self.0.lock() {
                Ok(mut f) => std::io::Write::write(&mut *f, buf),
                Err(_) => Ok(buf.len()),
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let writer = Writer(file);
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .try_init();
}

/// 在同一进程内启动 Rust 后端（专用线程 + tokio runtime）。
fn start_server(
    port: u16,
    token: &str,
    data_dir: &PathBuf,
    web_dir: PathBuf,
    ready: Arc<AtomicBool>,
) -> Result<(), String> {
    std::env::set_var("XMM_DESKTOP_TOKEN", token);
    let cfg_path = data_dir.join("config.toml");
    let cfg = xmm::config::load_or_create(&cfg_path);
    let data_dir = data_dir.clone();

    std::thread::Builder::new()
        .name("xmm-backend".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!("tokio runtime 启动失败：{}", e);
                    return;
                }
            };
            rt.block_on(async move {
                // 采集器在 State::new 里就会 spawn，必须在 tokio runtime 内创建
                let (st, rx) = xmm::state::State::new(cfg, &cfg_path, Some(&data_dir));
                st.spawn_dispatcher(rx);
                st.start_background();
                let app = xmm::api::router(st, &web_dir);
                let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::error!("后端监听 127.0.0.1:{} 失败：{}", port, e);
                        return;
                    }
                };
                tracing::info!("Rust 后端已就绪：127.0.0.1:{}", port);
                ready.store(true, Ordering::SeqCst);
                if let Err(e) = axum::serve(listener, app).await {
                    tracing::error!("后端异常退出：{}", e);
                }
            });
        })
        .map_err(|e| format!("无法启动后端线程：{}", e))?;
    Ok(())
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// 单实例：抢到端口的就是"第一个实例"，抢不到的把已有窗口叫出来然后自己退出。
fn claim_single_instance(app: &AppHandle) -> bool {
    match TcpListener::bind(("127.0.0.1", instance_port())) {
        Ok(listener) => {
            let handle = app.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stream.is_ok() {
                        show_main(&handle);
                    }
                }
            });
            true
        }
        Err(_) => {
            if let Ok(mut s) = TcpStream::connect(("127.0.0.1", instance_port())) {
                let _ = s.set_write_timeout(Some(Duration::from_millis(500)));
                let _ = s.write_all(b"show\n");
            }
            false
        }
    }
}

fn error_html(msg: &str, data_dir: &str) -> String {
    let escaped = msg.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let dir = data_dir.replace('&', "&amp;").replace('<', "&lt;");
    format!(
        r#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8">
<style>
 body{{margin:0;height:100vh;display:flex;align-items:center;justify-content:center;
      background:#0d1117;color:#e6edf3;font:15px/1.6 "Microsoft YaHei",system-ui,sans-serif}}
 .box{{max-width:560px;padding:28px 30px;border:1px solid #2b3441;border-radius:14px;background:#161b22}}
 h1{{font-size:19px;margin:0 0 12px;color:#e5534b}}
 code{{background:#0f141b;padding:2px 6px;border-radius:5px;font-size:13px;word-break:break-all}}
 p{{margin:8px 0;color:#8b98a5;font-size:13.5px}}
</style></head><body><div class="box">
<h1>监控服务没能启动</h1>
<p>原因：{msg}</p>
<p>日志在：<code>{dir}\server.log</code></p>
<p>可以先把窗口关掉，再重新打开程序试一次；如果一直这样，把上面那个日志文件发出来看看。</p>
</div></body></html>"#,
        msg = escaped,
        dir = dir
    )
}

// ---------------------------------------------------------------- main

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let handle = app.handle().clone();
            if !claim_single_instance(&handle) {
                std::process::exit(0);
            }

            let data_dir = app
                .path()
                .app_config_dir()
                .unwrap_or_else(|_| PathBuf::from("."));
            setup_logging(&data_dir);

            let port = free_port();
            let token = gen_token();
            let web_dir = locate_web(&handle);
            let mut startup_error: Option<String> = None;

            if !web_dir.join("index.html").exists() {
                startup_error = Some(format!("找不到界面资源（{}）", web_dir.display()));
            } else {
                let ready = Arc::new(AtomicBool::new(false));
                match start_server(port, &token, &data_dir, web_dir, ready.clone()) {
                    Ok(()) => {
                        let deadline = Instant::now() + Duration::from_secs(25);
                        let mut ok = false;
                        while Instant::now() < deadline {
                            if ping(port) {
                                ok = true;
                                break;
                            }
                            if !ready.load(Ordering::SeqCst) {
                                // 后端线程还没起来，继续等
                            }
                            std::thread::sleep(Duration::from_millis(150));
                        }
                        if !ok {
                            startup_error = Some("后端在 25 秒内没有应答（端口被占用或启动时报错）".into());
                        }
                    }
                    Err(e) => startup_error = Some(e),
                }
            }
            app.manage(Backend {
                port,
                token: token.clone(),
            });

            let url = match &startup_error {
                None => WebviewUrl::External(
                    format!("http://127.0.0.1:{}/", port).parse().unwrap(),
                ),
                Some(msg) => WebviewUrl::External(
                    format!(
                        "data:text/html;charset=utf-8,{}",
                        urlencode(&error_html(msg, &data_dir.display().to_string()))
                    )
                    .parse()
                    .unwrap(),
                ),
            };

            let init_script = format!(
                "window.__XMM_BOOTSTRAP__ = {{ token: \"{}\", port: {}, desktop: true }};",
                token, port
            );
            let win = WebviewWindowBuilder::new(app, "main", url)
                .title("主机监控小助手 beta1.4")
                .inner_size(1280.0, 840.0)
                .min_inner_size(380.0, 520.0)
                .center()
                .resizable(true)
                .initialization_script(&init_script)
                .build()?;
            let _ = win.show();

            let show_item = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "退出（停止监控）", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_item, &quit_item])?;
            let mut tray = TrayIconBuilder::with_id("main-tray")
                .tooltip("主机监控小助手 beta1.4")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => show_main(app),
                    "quit" => {
                        QUITTING.store(true, Ordering::SeqCst);
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if QUITTING.load(Ordering::SeqCst) {
                    return;
                }
                // 关窗口 = 收进托盘，监控继续跑；真要退出请用托盘菜单或界面里的"退出"
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("启动 Tauri 应用失败")
        .run(|_app, event| {
            if let RunEvent::Exit = event {
                QUITTING.store(true, Ordering::SeqCst);
                // 后端在本进程内，进程退出时 tokio 线程一并结束，无需杀子进程。
                // 若用户点了界面的"退出"，/api/shutdown 会直接结束整个进程。
            }
        });
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}
