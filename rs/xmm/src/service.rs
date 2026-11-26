//! Windows 服务管理（移植自 server/service.py）：用 WinSW 把后端注册成系统服务。
//!
//! 约定：
//! - WinSW 包装器以 `xiaozhushou-service.exe` 名字随包分发（与同名 xml 配对）；
//! - 服务运行的可执行文件 = 当前 xmm-server.exe（安装时写死绝对路径）；
//! - 服务固定监听 127.0.0.1:8787，数据目录与桌面版共用（%APPDATA%\com.xiaozhushou.monitor）；
//! - 服务模式与桌面窗口模式请二选一运行（两者共用配置文件，同时跑会互相干扰）。

use serde_json::{json, Value};
use std::path::PathBuf;

const NAME: &str = "XiaozhushouMonitor";

fn svc_exe() -> PathBuf {
    // 与当前可执行文件同目录的 xiaozhushou-service.exe
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("xiaozhushou-service.exe")))
        .unwrap_or_else(|| PathBuf::from("xiaozhushou-service.exe"))
}

pub fn available() -> bool {
    svc_exe().exists()
}

fn xml_text() -> Result<String, String> {
    let current = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_dir = current.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    // 桌面版进程内嵌了后端，但服务模式必须跑独立 server 可执行文件：
    // 安装包把 xmm-server.exe 与 xiaozhushou-service.exe 放在 exe 同目录。
    let candidate = exe_dir.join("xmm-server.exe");
    let server_exe = if candidate.exists() {
        candidate
    } else {
        current
    }
    .canonicalize()
    .map_err(|e| e.to_string())?;
    let appdata = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| server_exe.parent().unwrap().to_path_buf());
    let data_dir = appdata.join("com.xiaozhushou.monitor");
    Ok(format!(
        "<service>\n  <id>{}</id>\n  <name>主机监控小助手服务</name>\n  <description>主机监控小助手后台采集服务（数据目录与桌面版共用）</description>\n  <executable>{}</executable>\n  <arguments>--host 127.0.0.1 --port 8787 --data-dir \"{}\"</arguments>\n  <onfailure action=\"restart\" delay=\"10 sec\"/>\n  <log mode=\"roll-by-size\"><sizeThreshold>1024</sizeThreshold><keepFiles>3</keepFiles></log>\n</service>\n",
        NAME,
        server_exe.display(),
        data_dir.display()
    ))
}

async fn run_svc(args: &[&str], timeout_secs: u64) -> Result<(i32, String), String> {
    let exe = svc_exe();
    let cwd = exe.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let child = tokio::process::Command::new(&exe)
        .args(args)
        .current_dir(cwd)
        .output();
    let out = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), child)
        .await
        .map_err(|_| "服务命令超时".to_string())?
        .map_err(|e| e.to_string())?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.code().unwrap_or(-1), text.trim().to_string()))
}

pub async fn install() -> Result<String, String> {
    if !available() {
        return Err("服务组件缺失（xiaozhushou-service.exe 不存在），请重新打包".into());
    }
    let xml = svc_exe().with_file_name("xiaozhushou-service.xml");
    std::fs::write(&xml, xml_text()?).map_err(|e| e.to_string())?;
    let (rc, out) = run_svc(&["install"], 60).await?;
    if rc != 0 {
        return Err(if out.is_empty() { format!("install 失败（rc={}）", rc) } else { out });
    }
    start().await.map_err(|e| format!("服务已安装，但启动失败：{}", e))?;
    tracing::info!("服务已安装并启动");
    Ok("服务已安装并启动（开机自启，监听 127.0.0.1:8787）".into())
}

pub async fn uninstall() -> Result<String, String> {
    if !available() {
        return Err("服务组件缺失".into());
    }
    let _ = run_svc(&["stop"], 30).await;
    let (rc, out) = run_svc(&["uninstall"], 60).await?;
    if rc != 0 {
        return Err(if out.is_empty() { "uninstall 失败".into() } else { out });
    }
    Ok("服务已卸载".into())
}

pub async fn start() -> Result<String, String> {
    let (rc, out) = run_svc(&["start"], 60).await?;
    if rc != 0 {
        return Err(if out.is_empty() { "start 失败".into() } else { out });
    }
    Ok("服务已启动".into())
}

pub async fn stop() -> Result<String, String> {
    let (rc, out) = run_svc(&["stop"], 60).await?;
    if rc != 0 {
        return Err(if out.is_empty() { "stop 失败".into() } else { out });
    }
    Ok("服务已停止".into())
}

pub async fn status() -> Value {
    if !available() {
        return json!({"available": false, "installed": false, "state": ""});
    }
    match run_svc(&["status"], 15).await {
        Ok((_rc, out)) => {
            let text = out.to_lowercase();
            let (installed, state) = if text.contains("nonexistent") {
                (false, String::new())
            } else if text.contains("started") {
                (true, "running".to_string())
            } else if text.contains("stopped") {
                (true, "stopped".to_string())
            } else if text.contains("starting") || text.contains("stopping") || text.contains("paused") {
                (true, out.lines().next().unwrap_or("changing").to_string())
            } else {
                (false, out.chars().take(80).collect())
            };
            json!({"available": true, "installed": installed, "state": state})
        }
        Err(e) => json!({"available": true, "installed": false, "state": "", "error": e}),
    }
}
