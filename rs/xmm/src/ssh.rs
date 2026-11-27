//! SSH 长连接采集器（移植自 collector/ssh_client.py + connection.py + server/probe.py）。
//!
//! 一条连接上运行远端 while 循环脚本（collect.sh），本地逐行解析；
//! 断线自动重连（指数退避 1s 起，封顶 30s）；重连期间 status=offline，
//! latest 保留最后一帧不清空。远端脚本挂起时由看门狗兜底切离线。
//!
//! 主机指纹策略（host_key_policy）：
//!   autoadd（默认）—— 已记录过指纹的主机严格校验；新主机首次连接后自动记录
//!   strict        —— 只要 known_hosts 存在就严格校验，未记录的主机直接拒绝

use russh::client;
use russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path};
use russh::keys::{self, PrivateKeyWithHashAlg};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::parser::StreamParser;
use crate::secrets::get_ssh_password;
use crate::state::{CollectorEvent, EventTx};

/// 一次性采集必须用 `--` 显式结束 sh 的选项（beta2.3 修复，缺了会进死循环）。
const ONESHOT_CMD: &str = "sh -s -- --once";

/// 远端采集脚本：编译期内嵌，核心源码唯一真相不变（collector/remote/collect.sh）。
const COLLECT_SCRIPT: &str = include_str!("../../../collector/remote/collect.sh");

// ---------------------------------------------------------------- 连接参数

pub fn known_hosts_path(tcfg: &Value, data_dir: Option<&Path>) -> PathBuf {
    if let Some(p) = tcfg.get("known_hosts_file").and_then(|v| v.as_str()) {
        if !p.is_empty() {
            let expanded = shellexpand(p);
            return PathBuf::from(expanded);
        }
    }
    if let Some(dir) = data_dir {
        return dir.join("known_hosts");
    }
    dirs_home().join(".xmm_known_hosts")
}

fn shellexpand(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        return dirs_home().join(rest).to_string_lossy().to_string();
    }
    p.to_string()
}

fn dirs_home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// known_hosts 里是否已记录过这台主机（宽松匹配三种写法）。
fn host_recorded(kh: &Path, host: &str, port: u16) -> bool {
    if !kh.exists() {
        return false;
    }
    let host_expr = if port == 22 {
        host.to_string()
    } else {
        format!("[{}]:{}", host, port)
    };
    let Ok(text) = std::fs::read_to_string(kh) else { return false };
    text.lines().any(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return false;
        }
        let first = line.split(' ').next().unwrap_or("");
        first == host_expr || first == host || first == format!("{}:{}", host, port)
    })
}

pub enum AuthMethod {
    Password(String),
    PrivateKey(PathBuf),
}

pub struct ConnectParams {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
    pub policy_strict: bool,
    pub recorded: bool,
    pub kh: PathBuf,
    pub timeout: Duration,
    /// known_hosts 里这台主机记录过的密钥类型（按文件顺序）。
    /// 用来把协商顺序调成"先要已记录的算法"，否则旧版 asyncssh 记录的 RSA
    /// 指纹在 russh 优先协商 ed25519 时会被误判成"指纹不匹配"。
    pub recorded_algos: Vec<keys::Algorithm>,
}

/// known_hosts 里的密钥类型名 → russh 的 Algorithm
fn algo_from_name(name: &str) -> Option<keys::Algorithm> {
    use keys::{Algorithm, EcdsaCurve, HashAlg};
    match name {
        "ssh-ed25519" => Some(Algorithm::Ed25519),
        "ssh-rsa" => Some(Algorithm::Rsa { hash: None }),
        "rsa-sha2-256" => Some(Algorithm::Rsa { hash: Some(HashAlg::Sha256) }),
        "rsa-sha2-512" => Some(Algorithm::Rsa { hash: Some(HashAlg::Sha512) }),
        "ecdsa-sha2-nistp256" => Some(Algorithm::Ecdsa { curve: EcdsaCurve::NistP256 }),
        "ecdsa-sha2-nistp384" => Some(Algorithm::Ecdsa { curve: EcdsaCurve::NistP384 }),
        "ecdsa-sha2-nistp521" => Some(Algorithm::Ecdsa { curve: EcdsaCurve::NistP521 }),
        "ssh-dss" => Some(Algorithm::Dsa),
        _ => None,
    }
}

/// 读取这台主机在 known_hosts 里记录过的密钥类型（去重、保序）。
fn recorded_key_algos(kh: &Path, host: &str, port: u16) -> Vec<keys::Algorithm> {
    let Ok(text) = std::fs::read_to_string(kh) else { return vec![] };
    let host_expr = if port == 22 { host.to_string() } else { format!("[{}]:{}", host, port) };
    let mut out: Vec<keys::Algorithm> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        let Some(h) = it.next() else { continue };
        if !(h == host_expr || h == host || h == format!("{}:{}", host, port)) {
            continue;
        }
        if let Some(a) = it.next().and_then(algo_from_name) {
            if !out.contains(&a) {
                out.push(a);
            }
        }
    }
    out
}

/// 对应 Python build_connect_kwargs：校验并组装连接参数。
pub fn build_connect_params(
    tcfg: &Value,
    data_dir: Option<&Path>,
    timeout: Duration,
) -> Result<ConnectParams, String> {
    let host = tcfg
        .get("host")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if host.is_empty() {
        return Err("未填写主机地址".into());
    }
    let port = tcfg.get("port").and_then(|v| v.as_i64()).unwrap_or(22).max(1) as u16;
    let username = tcfg
        .get("username")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "root".into());
    let policy_strict = tcfg
        .get("host_key_policy")
        .and_then(|v| v.as_str())
        .unwrap_or("autoadd")
        == "strict";
    let kh = known_hosts_path(tcfg, data_dir);
    let recorded = host_recorded(&kh, &host, port);
    let recorded_algos = recorded_key_algos(&kh, &host, port);

    let key = tcfg
        .get("private_key")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let auth = if let Some(k) = key {
        AuthMethod::PrivateKey(PathBuf::from(shellexpand(&k)))
    } else {
        let pw = get_ssh_password(tcfg, None);
        if pw.is_empty() {
            return Err("没有可用的 SSH 凭据（密码 / 私钥都没配）".into());
        }
        AuthMethod::Password(pw)
    };
    Ok(ConnectParams {
        host,
        port,
        username,
        auth,
        policy_strict,
        recorded,
        kh,
        timeout,
        recorded_algos,
    })
}

// ---------------------------------------------------------------- 指纹校验

struct KeyHandler {
    host: String,
    port: u16,
    policy_strict: bool,
    recorded: bool,
    kh: PathBuf,
    learned: Arc<Mutex<Option<keys::PublicKey>>>,
    key_mismatch: Arc<AtomicBool>,
}

impl client::Handler for KeyHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        if self.policy_strict || self.recorded {
            // 严格校验：必须能在 known_hosts 里找到且匹配
            match check_known_hosts_path(
                &self.host,
                self.port,
                server_public_key,
                &self.kh,
            ) {
                Ok(true) => Ok(true),
                _ => {
                    self.key_mismatch.store(true, Ordering::SeqCst);
                    Ok(false)
                }
            }
        } else {
            // autoadd 且未记录：先接受，连接成功后落盘
            *self.learned.lock().unwrap() = Some(server_public_key.clone());
            Ok(true)
        }
    }
}

fn persist_host_key(kh: &Path, host: &str, port: u16, learned: &Arc<Mutex<Option<keys::PublicKey>>>, tag: &str) {
    let key = learned.lock().unwrap().take();
    let Some(key) = key else { return };
    if host_recorded(kh, host, port) {
        return; // 已有记录（可能别的并发连接先写了）
    }
    match learn_known_hosts_path(host, port, &key, kh) {
        Ok(()) => tracing::info!("采集器[{}] 已记录主机指纹 {}:{}", tag, host, port),
        Err(e) => tracing::warn!("采集器[{}] 记录主机指纹失败：{}", tag, e),
    }
}

// ---------------------------------------------------------------- 连接与认证

pub struct SshConn {
    pub(crate) session: client::Handle<KeyHandler>,
    pub(crate) learned: Arc<Mutex<Option<keys::PublicKey>>>,
    pub(crate) key_mismatch: Arc<AtomicBool>,
}

pub async fn connect(params: &ConnectParams) -> Result<SshConn, String> {
    // 把 known_hosts 里记录过的算法提到最前，让服务器用它已备案的那把主机密钥。
    let mut preferred = russh::Preferred::default();
    if !params.recorded_algos.is_empty() {
        let mut key_order: Vec<keys::Algorithm> = params.recorded_algos.clone();
        for a in preferred.key.iter() {
            if !key_order.contains(a) {
                key_order.push(a.clone());
            }
        }
        preferred.key = std::borrow::Cow::Owned(key_order);
    }
    let config = Arc::new(client::Config {
        preferred,
        inactivity_timeout: Some(Duration::from_secs(60)),
        keepalive_interval: Some(Duration::from_secs(15)),
        ..Default::default()
    });
    let learned = Arc::new(Mutex::new(None));
    let key_mismatch = Arc::new(AtomicBool::new(false));
    let handler = KeyHandler {
        host: params.host.clone(),
        port: params.port,
        policy_strict: params.policy_strict,
        recorded: params.recorded,
        kh: params.kh.clone(),
        learned: learned.clone(),
        key_mismatch: key_mismatch.clone(),
    };
    let mut session = tokio::time::timeout(
        params.timeout,
        client::connect(config, (params.host.as_str(), params.port), handler),
    )
    .await
    .map_err(|_| "连接超时".to_string())?
    .map_err(|e| {
        if key_mismatch.load(Ordering::SeqCst) {
            "主机指纹不匹配".to_string()
        } else {
            format!("{}", e)
        }
    })?;

    match &params.auth {
        AuthMethod::Password(pw) => {
            let r = session
                .authenticate_password(&params.username, pw)
                .await
                .map_err(|e| format!("{}", e))?;
            if !r.success() {
                // 有些服务器只开 keyboard-interactive：密码再试一次
                let ok = keyboard_interactive(&mut session, &params.username, pw).await?;
                if !ok {
                    return Err("认证失败".into());
                }
            }
        }
        AuthMethod::PrivateKey(path) => {
            let key = keys::load_secret_key(path, None)
                .map_err(|e| format!("私钥读取失败：{}", e))?;
            let hash = session
                .best_supported_rsa_hash()
                .await
                .ok()
                .flatten()
                .flatten();
            let r = session
                .authenticate_publickey(
                    &params.username,
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                )
                .await
                .map_err(|e| format!("{}", e))?;
            if !r.success() {
                return Err("认证失败（密钥未被接受）".into());
            }
        }
    }
    Ok(SshConn { session, learned, key_mismatch })
}

async fn keyboard_interactive(
    session: &mut client::Handle<KeyHandler>,
    user: &str,
    pw: &str,
) -> Result<bool, String> {
    use russh::client::KeyboardInteractiveAuthResponse as R;
    let mut resp = session
        .authenticate_keyboard_interactive_start(user, None)
        .await
        .map_err(|e| format!("{}", e))?;
    for _ in 0..8 {
        match resp {
            R::Success => return Ok(true),
            R::Failure { .. } => return Ok(false),
            R::InfoRequest { prompts, .. } => {
                resp = session
                    .authenticate_keyboard_interactive_respond(vec![pw.to_string(); prompts.len()])
                    .await
                    .map_err(|e| format!("{}", e))?;
            }
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------- 采集器

/// 守卫析构时 abort 子任务。tokio 任务被 abort 时其 future 会被 drop，
/// 本守卫的 Drop 随之把看门狗一并 abort；否则只 drop JoinHandle 是 detach，
/// 删除目标 / 重建采集器会泄漏一个永循环任务，还持续给已删除目标发幽灵状态。
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub struct HostCollector {
    pub name: String,
    pub cfg: Value,
    pub interval: f64,
    pub mode: String, // stream | oneshot
    pub data_dir: Option<PathBuf>,
    pub status: Arc<RwLock<String>>,
    pub latest: Arc<RwLock<Option<Value>>>,
}

impl HostCollector {
    pub fn new(name: &str, target_cfg: &Value, collect_cfg: &Value, data_dir: Option<PathBuf>) -> Self {
        Self {
            name: name.to_string(),
            cfg: target_cfg.clone(),
            interval: collect_cfg
                .get("interval_s")
                .and_then(|v| v.as_f64())
                .unwrap_or(1.0)
                .max(0.5),
            mode: collect_cfg
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("stream")
                .to_string(),
            data_dir,
            status: Arc::new(RwLock::new("connecting".to_string())),
            latest: Arc::new(RwLock::new(None)),
        }
    }

    pub fn spawn(self: Arc<Self>, tx: EventTx) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run(tx).await;
        })
    }

    async fn set_status(&self, tx: &EventTx, s: &str) {
        let changed = {
            let mut cur = self.status.write().unwrap();
            if *cur != s {
                *cur = s.to_string();
                true
            } else {
                false
            }
        };
        if changed {
            let _ = tx.send(CollectorEvent::Status {
                name: self.name.clone(),
                status: s.to_string(),
            });
        }
    }

    async fn run(self: Arc<Self>, tx: EventTx) {
        let mut backoff = 1.0f64;
        // 看门狗：远端挂起时兜底切离线，恢复后切回。
        // 用 AbortOnDrop 守卫：采集器被 abort（删目标 / 改连接参数重建）时，
        // 看门狗必须跟着结束，不能 detach 泄漏。
        let last_frame = Arc::new(Mutex::new(None::<Instant>));
        let _watchdog = {
            let (last_frame, wd_tx) = (last_frame.clone(), tx.clone());
            let (wd_status, wd_name, wd_interval) =
                (self.status.clone(), self.name.clone(), self.interval);
            AbortOnDrop(tokio::spawn(async move {
                let threshold = (wd_interval * 10.0).max(10.0);
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let last = *last_frame.lock().unwrap();
                    let Some(last) = last else { continue };
                    let age = last.elapsed().as_secs_f64();
                    let cur = wd_status.read().unwrap().clone();
                    if cur == "online" && age > threshold {
                        *wd_status.write().unwrap() = "offline".into();
                        let _ = wd_tx.send(CollectorEvent::Status { name: wd_name.clone(), status: "offline".into() });
                    } else if cur == "offline" && age <= threshold {
                        *wd_status.write().unwrap() = "online".into();
                        let _ = wd_tx.send(CollectorEvent::Status { name: wd_name.clone(), status: "online".into() });
                    }
                }
            }))
        };

        loop {
            self.set_status(&tx, "connecting").await;
            let started = Instant::now();
            match self.run_once(&tx, &last_frame).await {
                Ok(()) => {
                    tracing::warn!("采集器[{}] 远端数据流结束", self.name);
                }
                Err(e) => {
                    tracing::warn!("采集器[{}] 连接中断：{}", self.name, e);
                }
            }
            // 会话已结束：清掉最后一帧时间戳，否则退避等待期间看门狗看到
            // "offline 且最后一帧还没过期" 会把状态误翻回 online。
            *last_frame.lock().unwrap() = None;
            self.set_status(&tx, "offline").await;
            // 上一条连接稳定过一段时间，这次断开按全新起点退避，不沿用旧的 30s。
            if started.elapsed() >= Duration::from_secs(60) {
                backoff = 1.0;
            }
            let delay = backoff;
            backoff = (backoff * 2.0).min(30.0);
            tokio::time::sleep(Duration::from_secs_f64(delay)).await;
        }
    }

    /// 一次完整的连接生命周期；返回后由外层决定重连。
    async fn run_once(
        &self,
        tx: &EventTx,
        last_frame: &Arc<Mutex<Option<Instant>>>,
    ) -> Result<(), String> {
        let params = build_connect_params(&self.cfg, self.data_dir.as_deref(), Duration::from_secs(10))?;
        let first_time = !params.recorded && !params.policy_strict;
        let mut conn = connect(&params).await?;
        if first_time {
            persist_host_key(&params.kh, &params.host, params.port, &conn.learned, &self.name);
        }
        let mut parser = StreamParser::new();
        *last_frame.lock().unwrap() = None;

        if self.mode == "oneshot" {
            self.set_status(tx, "online").await;
            loop {
                let line = exec_read_lines(&mut conn.session, ONESHOT_CMD, Some(COLLECT_SCRIPT))
                    .await?
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                if !line.is_empty() {
                    self.handle_line(tx, &mut parser, &line, last_frame);
                }
                tokio::time::sleep(Duration::from_secs_f64(self.interval)).await;
            }
        } else {
            let mut channel = conn
                .session
                .channel_open_session()
                .await
                .map_err(|e| e.to_string())?;
            channel.exec(false, "sh -s").await.map_err(|e| e.to_string())?;
            let script = format!("INTERVAL={}\n{}", self.interval as i64, COLLECT_SCRIPT);
            channel.data(&script.as_bytes()[..]).await.map_err(|e| e.to_string())?;
            channel.eof().await.map_err(|e| e.to_string())?;
            self.set_status(tx, "online").await;

            let mut buf: Vec<u8> = Vec::new();
            loop {
                let msg = channel.wait().await;
                match msg {
                    Some(russh::ChannelMsg::Data { data }) => {
                        // 按字节攒行再解码：多字节 UTF-8（中文 OS 名/容器名）被
                        // SSH 数据块边界截断时不会变乱码（beta1.4.3 修复）
                        for line in drain_lines(&mut buf, &data) {
                            self.handle_line(tx, &mut parser, &line, last_frame);
                        }
                    }
                    Some(russh::ChannelMsg::Close) | None => {
                        return Err("远端数据流结束".into());
                    }
                    Some(_) => {}
                }
            }
        }
    }

    fn handle_line(
        &self,
        tx: &EventTx,
        parser: &mut StreamParser,
        line: &str,
        last_frame: &Arc<Mutex<Option<Instant>>>,
    ) {
        if let Some(frame) = parser.parse(line) {
            *self.latest.write().unwrap() = Some(frame.clone());
            *last_frame.lock().unwrap() = Some(Instant::now());
            let _ = tx.send(CollectorEvent::Frame {
                name: self.name.clone(),
                frame,
            });
        }
    }
}

/// 把 SSH 数据块追加到行缓冲，返回其中的完整行（按 \n 切分，兼容 \r\n）。
///
/// 按字节切分后再整体解码：多字节 UTF-8 字符被数据块边界截断时，
/// `String::from_utf8_lossy` 不会把它变成 U+FFFD 乱码。
/// 缓冲超过 4 MiB 仍未见换行时整体丢弃（防远端异常/卡死把内存吃光）。
pub fn drain_lines(buf: &mut Vec<u8>, chunk: &[u8]) -> Vec<String> {
    buf.extend_from_slice(chunk);
    let mut out = Vec::new();
    let mut start = 0usize;
    while let Some(pos) = buf[start..].iter().position(|&b| b == b'\n') {
        let end = start + pos;
        out.push(String::from_utf8_lossy(&buf[start..end]).trim_end_matches('\r').to_string());
        start = end + 1;
    }
    if start > 0 {
        buf.drain(..start);
    }
    if buf.len() > 4 * 1024 * 1024 {
        buf.clear();
    }
    out
}

/// 执行一条远端命令，把 stdout 按行收齐（用于 oneshot / 测试连接）。
async fn exec_read_lines(
    session: &mut client::Handle<KeyHandler>,
    cmd: &str,
    stdin_text: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut channel = session.channel_open_session().await.map_err(|e| e.to_string())?;
    channel.exec(false, cmd).await.map_err(|e| e.to_string())?;
    if let Some(text) = stdin_text {
        channel.data(&text.as_bytes()[..]).await.map_err(|e| e.to_string())?;
    }
    channel.eof().await.map_err(|e| e.to_string())?;
    let mut out: Vec<u8> = Vec::new();
    loop {
        match channel.wait().await {
            Some(russh::ChannelMsg::Data { data }) => out.extend_from_slice(&data),
            Some(russh::ChannelMsg::Close) | None => break,
            Some(_) => {}
        }
    }
    Ok(String::from_utf8_lossy(&out).lines().map(|s| s.to_string()).collect())
}

// ---------------------------------------------------------------- 测试连接

const PROBE_CMD: &str = "uname -sr 2>/dev/null; ( . /etc/os-release 2>/dev/null && echo \"$PRETTY_NAME\" ) 2>/dev/null; nproc 2>/dev/null || grep -c '^processor' /proc/cpuinfo 2>/dev/null";

fn friendly_error(msg: &str) -> String {
    let low = msg.to_lowercase();
    if low.contains("认证失败") || (low.contains("auth") && low.contains("fail")) || low.contains("permission denied") {
        "认证失败：账号或密码不对（也可能是该账号禁止密码登录，需改用密钥）".into()
    } else if low.contains("指纹") || low.contains("host key") {
        "主机指纹校验失败：known_hosts 里没有该主机（或该密钥类型）的记录，或与记录不一致。\
         确认这台机器没被换过/重装过之后，删掉 known_hosts 里的旧记录再试".into()
    } else if low.contains("超时") || low.contains("timeout") || low.contains("timed out") {
        "连接超时：IP 不通、端口被防火墙拦了，或对方机器没开 SSH".into()
    } else if low.contains("refused") {
        "连接被拒绝：该端口没有 SSH 服务（端口号填错了？）".into()
    } else if low.contains("unreachable") || low.contains("no route") {
        "网络不可达：检查 IP 是否正确、是否在同一网段/VPN".into()
    } else if low.contains("私钥") {
        "私钥文件不存在或无法读取：检查私钥路径".into()
    } else {
        msg.to_string()
    }
}

/// 返回 {ok, message, os, cores, latency_ms}。永不 panic。
pub async fn probe_target(tcfg: &Value, data_dir: Option<&Path>, timeout: Duration) -> Value {
    let t0 = Instant::now();
    let latency = || t0.elapsed().as_millis() as i64;
    let params = match build_connect_params(tcfg, data_dir, timeout) {
        Ok(p) => p,
        Err(e) => {
            return json!({"ok": false, "message": friendly_error(&e), "os": "", "cores": Value::Null, "latency_ms": latency()})
        }
    };
    let conn = match connect(&params).await {
        Ok(c) => c,
        Err(e) => {
            return json!({"ok": false, "message": friendly_error(&e), "os": "", "cores": Value::Null, "latency_ms": latency()})
        }
    };
    let mut session = conn.session;

    let mut osname = String::new();
    let mut cores: Option<i64> = None;
    if let Ok(lines) = tokio::time::timeout(timeout, exec_read_lines(&mut session, PROBE_CMD, None)).await {
        if let Ok(lines) = lines {
            let lines: Vec<&str> = lines.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
            if let Some(first) = lines.first() {
                osname = first.to_string();
            }
            for ln in lines.iter().skip(1) {
                if ln.chars().all(|c| c.is_ascii_digit()) {
                    cores = ln.parse().ok();
                } else if osname.is_empty() || ln.len() > osname.len() {
                    osname = ln.to_string();
                }
            }
        }
    }

    // 首次连接顺手记录指纹，和采集器行为保持一致
    persist_host_key(&params.kh, &params.host, params.port, &conn.learned, "probe");
    let _ = session.disconnect(russh::Disconnect::ByApplication, "", "").await;

    json!({
        "ok": true,
        "message": format!("连接成功（{} ms）", latency()),
        "os": osname,
        "cores": cores,
        "latency_ms": latency(),
    })
}

#[cfg(test)]
mod watchdog_tests {
    use super::AbortOnDrop;

    /// 回归：采集器任务被 abort 时，看门狗守卫必须把子任务一并 abort，
    /// 否则删除目标 / 重建采集器会泄漏永循环任务。
    #[tokio::test]
    async fn abort_on_drop_stops_child_task() {
        let handle = tokio::spawn(async {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });
        let abort = handle.abort_handle();
        let guard = AbortOnDrop(handle);
        assert!(!abort.is_finished());
        drop(guard);
        for _ in 0..100 {
            tokio::task::yield_now().await;
            if abort.is_finished() {
                break;
            }
        }
        assert!(abort.is_finished(), "看门狗应随守卫一并被 abort");
    }
}
