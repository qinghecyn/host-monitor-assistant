//! HTTP API 与 WebSocket 端点（移植自 server/api.py，axum 实现）。
//!
//! 鉴权：POST /api/login 换 token；后续请求 Authorization: Bearer <token> 或 ?token=；
//! WS 连接后 10 秒内发 {"type":"auth","token":...}，否则关闭(4001)。
//! 桌面版用 POST /api/login/desktop 换 token（一次性令牌，环境变量注入）。

use axum::extract::{Path, Query, State as AxState, WebSocketUpgrade};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post, put};
use axum::Router;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

use crate::config;
use crate::secrets;
use crate::state::State;
use crate::{now_ts, VERSION};

type Shared = Arc<State>;

type ApiErr = (StatusCode, Json<Value>);

fn err(status: StatusCode, msg: impl Into<String>) -> ApiErr {
    (status, Json(json!({"detail": msg.into()})))
}

// ---------- 鉴权 ----------

fn token_from(headers: &HeaderMap, q: &Query<HashMap<String, String>>) -> String {
    if let Some(h) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(t) = h.to_lowercase().strip_prefix("bearer ") {
            let _ = t;
            return h[7..].trim().to_string();
        }
    }
    q.get("token").cloned().unwrap_or_default()
}

fn auth(st: &State, headers: &HeaderMap, q: &Query<HashMap<String, String>>) -> Result<String, ApiErr> {
    st.tokens
        .verify(&token_from(headers, q))
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "未登录或登录已过期"))
}

async fn read_body_json(req: axum::extract::Request) -> Result<Value, ApiErr> {
    let bytes = axum::body::to_bytes(req.into_body(), 1024 * 1024)
        .await
        .map_err(|_| err(StatusCode::BAD_REQUEST, "请求格式错误"))?;
    serde_json::from_slice(&bytes).map_err(|_| err(StatusCode::BAD_REQUEST, "请求格式错误"))
}

// ---------- 路由 ----------

pub fn router(st: Shared, web_dir: &std::path::Path) -> Router {
    let api = Router::new()
        .route("/ping", get(ping))
        .route("/login", post(login))
        .route("/login/desktop", post(login_desktop))
        .route("/snapshot", get(snapshot))
        .route("/targets", get(list_targets).post(create_target))
        .route("/targets/{name}", put(update_target).delete(delete_target))
        .route("/targets/test", post(test_target))
        .route("/config", get(get_config).put(put_config))
        .route("/shutdown", post(shutdown))
        .route("/history", get(get_history))
        .route("/probes", get(list_probes).post(create_probe))
        .route("/probes/{name}", put(update_probe).delete(delete_probe))
        .route("/notify/deliveries", get(list_deliveries))
        .route("/notify/test", post(notify_test))
        .route("/service", get(service_status).post(service_action));

    Router::new()
        .nest("/api", api)
        .route("/ws", get(ws_handler))
        .fallback_service(ServeDir::new(web_dir).append_index_html_on_directories(true))
        .layer(axum::middleware::from_fn(no_stale_static))
        .layer(CorsLayer::permissive())
        .with_state(st)
}

/// 网页静态资源禁止启发式缓存（升级换包后必须重新校验；304 开销可忽略）。
async fn no_stale_static(req: axum::extract::Request, next: Next) -> Response {
    let is_api = req.uri().path().starts_with("/api") || req.uri().path().starts_with("/ws");
    let mut resp = next.run(req).await;
    if !is_api {
        resp.headers_mut()
            .insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-cache"));
    }
    resp
}

// ---------- 基础 ----------

async fn ping() -> Json<Value> {
    Json(json!({"ok": true, "app": "xiaozhushou-monitor", "version": VERSION}))
}

async fn login(AxState(st): AxState<Shared>, req: axum::extract::Request) -> Result<Json<Value>, ApiErr> {
    let body = read_body_json(req).await?;
    let username = body.get("username").and_then(|v| v.as_str()).unwrap_or("");
    let password = body.get("password").and_then(|v| v.as_str()).unwrap_or("");
    if username.is_empty() || password.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "请输入账号和密码"));
    }
    // 用块把 RwLock 守卫的生命周期严格限制住，避免跨 await 让 future 非 Send
    let (want_u, want_p) = {
        let cfg = st.cfg.read().unwrap();
        let u = cfg
            .get("auth")
            .and_then(|a| a.get("username"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("admin")
            .to_string();
        let p = secrets::get_web_password(cfg.get("auth").unwrap_or(&json!({})));
        (u, p)
    };
    // 两个比较都算完再 AND（避免 && 短路让密码比较被跳过，泄漏账号是否存在）
    let ok_u = constant_time_eq(username.as_bytes(), want_u.as_bytes());
    let ok_p = constant_time_eq(password.as_bytes(), want_p.as_bytes());
    let ok = ok_u & ok_p;
    if !ok {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await; // 降低爆破速率
        return Err(err(StatusCode::UNAUTHORIZED, "账号或密码错误"));
    }
    let token = st.tokens.issue(username);
    Ok(Json(json!({"token": token, "username": username})))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    // hmac.compare_digest 的简易等价：长度先异或，避免提前退出
    let max = a.len().max(b.len());
    let mut diff = a.len() ^ b.len();
    for i in 0..max {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= (x ^ y) as usize;
    }
    diff == 0
}

async fn login_desktop(AxState(st): AxState<Shared>, req: axum::extract::Request) -> Result<Json<Value>, ApiErr> {
    if st.desktop_token.is_empty() {
        return Err(err(StatusCode::NOT_FOUND, "未启用桌面模式"));
    }
    let body = read_body_json(req).await?;
    let got = body.get("token").and_then(|v| v.as_str()).unwrap_or("");
    if !constant_time_eq(got.as_bytes(), st.desktop_token.as_bytes()) {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        return Err(err(StatusCode::UNAUTHORIZED, "桌面令牌无效"));
    }
    let token = st.tokens.issue("desktop");
    Ok(Json(json!({"token": token, "username": "desktop", "desktop": true})))
}

// ---------- 数据 ----------

async fn snapshot(
    AxState(st): AxState<Shared>,
    headers: HeaderMap,
    q: Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiErr> {
    auth(&st, &headers, &q)?;
    Ok(Json(st.snapshot_payload()))
}

// ---------- 服务器管理 ----------

async fn list_targets(
    AxState(st): AxState<Shared>,
    headers: HeaderMap,
    q: Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiErr> {
    auth(&st, &headers, &q)?;
    let mock = st.cfg.read().unwrap().get("_mock").and_then(|v| v.as_bool()).unwrap_or(false);
    Ok(Json(json!({
        "targets": st.targets_payload(),
        "mock": mock,
        "desktop": !st.desktop_token.is_empty(),
    })))
}

fn edit_config<T>(st: &State, edit: impl FnOnce(&mut Value) -> Result<T, ApiErr>) -> Result<T, ApiErr> {
    let _serial = st.config_edit.lock().unwrap();
    let before = st.cfg.read().unwrap().clone();
    let mut next = before.clone();
    let result = edit(&mut next)?;
    config::save_config(&st.cfg_path, &next)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("配置写入失败：{}", e)))?;
    *st.cfg.write().unwrap() = next.clone();
    st.apply_config(&before, &next);
    Ok(result)
}

fn set_target(cfg: &mut Value, name: &str, fields: &Value) -> Result<(), ApiErr> {
    if !config::valid_name(name) || name.trim() != name {
        return Err(err(StatusCode::BAD_REQUEST, "服务器名只能是 1-32 位字母、数字、下划线或中划线"));
    }
    let old = cfg["targets"].get(name);
    let target = config::normalize_target(fields, old).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    if target["host"].as_str().unwrap_or("").is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "主机地址不能为空"));
    }
    if old.is_none() && !["password", "private_key", "password_env"].iter()
        .any(|key| !target[*key].as_str().unwrap_or("").is_empty()) {
        return Err(err(StatusCode::BAD_REQUEST, "请填 SSH 密码，或指定私钥"));
    }
    cfg["targets"][name] = target;
    Ok(())
}

async fn create_target(
    AxState(st): AxState<Shared>,
    headers: HeaderMap,
    q: Query<HashMap<String, String>>,
    req: axum::extract::Request,
) -> Result<Json<Value>, ApiErr> {
    auth(&st, &headers, &q)?;
    let mut body = read_body_json(req).await?;
    if !body.is_object() {
        return Err(err(StatusCode::BAD_REQUEST, "请求格式错误"));
    }
    let name = body
        .as_object_mut()
        .and_then(|o| o.remove("name"))
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default();
    let name = name.trim();
    edit_config(&st, |cfg| {
        if cfg["targets"].get(name).is_some() {
            return Err(err(StatusCode::BAD_REQUEST, "已经有同名服务器了，换个名字"));
        }
        set_target(cfg, name, &body)
    })?;
    let info = st.meta(name);
    Ok(Json(json!({"ok": true, "target": info})))
}

async fn update_target(
    AxState(st): AxState<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
    q: Query<HashMap<String, String>>,
    req: axum::extract::Request,
) -> Result<Json<Value>, ApiErr> {
    auth(&st, &headers, &q)?;
    let mut body = read_body_json(req).await?;
    if !body.is_object() {
        return Err(err(StatusCode::BAD_REQUEST, "请求格式错误"));
    }
    body.as_object_mut().map(|o| o.remove("name"));
    edit_config(&st, |cfg| {
        if cfg["targets"].get(&name).is_none() {
            return Err(err(StatusCode::BAD_REQUEST, format!("没有这台服务器：{}", name)));
        }
        set_target(cfg, &name, &body)
    })?;
    let info = st.meta(&name);
    Ok(Json(json!({"ok": true, "target": info})))
}

async fn delete_target(
    AxState(st): AxState<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
    q: Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiErr> {
    auth(&st, &headers, &q)?;
    edit_config(&st, |cfg| {
        cfg["targets"].as_object_mut().unwrap().remove(&name)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("没有这台服务器：{}", name)))?;
        Ok(())
    })?;
    Ok(Json(json!({"ok": true})))
}

async fn test_target(
    AxState(st): AxState<Shared>,
    headers: HeaderMap,
    q: Query<HashMap<String, String>>,
    req: axum::extract::Request,
) -> Result<Json<Value>, ApiErr> {
    auth(&st, &headers, &q)?;
    let mut body = read_body_json(req).await?;
    if !body.is_object() {
        return Err(err(StatusCode::BAD_REQUEST, "请求格式错误"));
    }
    let name = body
        .as_object_mut()
        .and_then(|o| o.remove("name"))
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default();
    let base = if name.is_empty() {
        None
    } else {
        st.cfg.read().unwrap().get("targets").and_then(|t| t.get(&name)).cloned()
    };
    let mut tcfg = config::normalize_target(&body, base.as_ref())
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    if let Some(pw) = body.get("password").and_then(|v| v.as_str()) {
        if !pw.is_empty() {
            tcfg.as_object_mut().unwrap().insert("password".into(), json!(pw));
        }
    }
    if tcfg.get("host").and_then(|v| v.as_str()).map(|s| s.is_empty()).unwrap_or(true) {
        return Err(err(StatusCode::BAD_REQUEST, "主机地址不能为空"));
    }
    if st.cfg.read().unwrap().get("_mock").and_then(|v| v.as_bool()).unwrap_or(false) {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        return Ok(Json(json!({"ok": true, "message": "模拟模式：假装连接成功", "os": "Mock Linux", "cores": 4})));
    }
    let result = crate::ssh::probe_target(&tcfg, Some(&st.data_dir), std::time::Duration::from_secs(8)).await;
    Ok(Json(result))
}

// ---------- 全局配置 ----------

const ALLOWED_TOP: [&str; 7] = ["alerts", "collect", "auth", "targets", "notify", "history", "probes"];
const ALLOWED_ALERTS: [&str; 4] = ["temp_c", "mem_available_percent", "root_free_gb", "resource_rules"];
const ALLOWED_COLLECT: [&str; 2] = ["interval_s", "history_minutes"];
const ALLOWED_AUTH: [&str; 2] = ["username", "password"];
const ALLOWED_NOTIFY: [&str; 15] = [
    "enabled", "dedup_minutes", "quiet_start", "quiet_end",
    "serverchan_key", "telegram_token", "telegram_chat",
    "smtp_host", "smtp_port", "smtp_user", "smtp_pass", "smtp_to",
    "webhook_url", "heartbeat_minutes", "_pad",
];
const ALLOWED_HISTORY: [&str; 2] = ["keep_days", "sample_s"];
const ALLOWED_PROBE: [&str; 10] = crate::probes::PROBE_FIELDS;

fn keys_exceed(body: &Value, allowed: &[&str]) -> bool {
    body.as_object()
        .map(|o| o.keys().any(|k| !allowed.contains(&k.as_str())))
        .unwrap_or(true)
}

fn num(v: Option<&Value>, lo: f64, hi: f64, default: f64) -> f64 {
    v.and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .unwrap_or(default)
        .clamp(lo, hi)
}

async fn get_config(
    AxState(st): AxState<Shared>,
    headers: HeaderMap,
    q: Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiErr> {
    auth(&st, &headers, &q)?;
    let cfg = st.cfg.read().unwrap().clone();
    let mut notify = cfg.get("notify").cloned().unwrap_or_else(|| json!({}));
    // 凭据只回"有没有"，不回内容
    for (k, flag) in [
        ("serverchan_key", "has_serverchan"),
        ("telegram_token", "has_telegram"),
        ("smtp_pass", "has_smtp"),
        ("webhook_url", "has_webhook"),
    ] {
        let has = notify.get(k).and_then(|v| v.as_str()).map(|s| !s.trim().is_empty()).unwrap_or(false);
        notify.as_object_mut().unwrap().insert(flag.into(), json!(has));
        notify.as_object_mut().unwrap().remove(k);
    }
    let targets_pub: serde_json::Map<String, Value> = cfg
        .get("targets")
        .and_then(|t| t.as_object())
        .map(|o| {
            o.iter()
                .map(|(n, t)| {
                    let mut t = t.clone();
                    t.as_object_mut().map(|x| x.remove("password"));
                    (n.clone(), t)
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Json(json!({
        "alerts": cfg.get("alerts").cloned().unwrap_or(json!({})),
        "collect": cfg.get("collect").cloned().unwrap_or(json!({})),
        "auth": {"username": cfg.get("auth").and_then(|a| a.get("username")).and_then(|v| v.as_str()).unwrap_or("admin")},
        "notify": notify,
        "history": cfg.get("history").cloned().unwrap_or(json!({})),
        "probes": cfg.get("probes").cloned().unwrap_or(json!({})),
        "targets": Value::Object(targets_pub),
        "server": {
            "listen_host": cfg.get("server").and_then(|s| s.get("listen_host")).cloned().unwrap_or(Value::Null),
            "listen_port": cfg.get("server").and_then(|s| s.get("listen_port")).cloned().unwrap_or(Value::Null),
        },
        "mock": cfg.get("_mock").and_then(|v| v.as_bool()).unwrap_or(false),
        "desktop": !st.desktop_token.is_empty(),
        "version": VERSION,
        "config_path": st.cfg_path.to_string_lossy(),
    })))
}

async fn put_config(
    AxState(st): AxState<Shared>,
    headers: HeaderMap,
    q: Query<HashMap<String, String>>,
    req: axum::extract::Request,
) -> Result<Json<Value>, ApiErr> {
    auth(&st, &headers, &q)?;
    let body = read_body_json(req).await?;
    if !body.is_object() || keys_exceed(&body, &ALLOWED_TOP) {
        return Err(err(StatusCode::BAD_REQUEST, "包含不支持的配置项"));
    }
    let (need_restart, warnings) = edit_config(&st, |cfg| apply_settings(cfg, &body))?;
    Ok(Json(json!({"ok": true, "restarted": need_restart, "warnings": warnings})))
}

fn apply_settings(cfg: &mut Value, body: &Value) -> Result<(bool, Vec<String>), ApiErr> {
    let mut warnings: Vec<String> = vec![];
    let before_interval = cfg["collect"]["interval_s"].as_f64();

    // alerts
    if let Some(a) = body.get("alerts") {
        if !a.is_object() || keys_exceed(a, &ALLOWED_ALERTS) {
            return Err(err(StatusCode::BAD_REQUEST, "alerts 配置项不支持"));
        }
        let cur = cfg.as_object_mut().unwrap()
            .entry("alerts").or_insert(json!({})).as_object_mut().unwrap();
        // 先取出旧值再写，避免闭包持有不可变借用时又做可变插入
        if let Some(rules) = a.get("resource_rules") {
            crate::alerts::parse_rules(rules).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
            cur.insert("resource_rules".into(), rules.clone());
        }
        let old_temp = cur.get("temp_c").and_then(|v| v.as_f64()).unwrap_or(70.0);
        let old_mem = cur.get("mem_available_percent").and_then(|v| v.as_f64()).unwrap_or(10.0);
        let old_disk = cur.get("root_free_gb").and_then(|v| v.as_f64()).unwrap_or(1.0);
        cur.insert("temp_c".into(), json!(num(a.get("temp_c"), 0.0, 200.0, old_temp)));
        cur.insert("mem_available_percent".into(), json!(num(a.get("mem_available_percent"), 0.0, 100.0, old_mem)));
        cur.insert("root_free_gb".into(), json!(num(a.get("root_free_gb"), 0.0, 10000.0, old_disk)));
    }

    // collect
    if let Some(c) = body.get("collect") {
        if !c.is_object() || keys_exceed(c, &ALLOWED_COLLECT) {
            return Err(err(StatusCode::BAD_REQUEST, "collect 配置项不支持"));
        }
        let cur = cfg.as_object_mut().unwrap()
            .entry("collect").or_insert(json!({})).as_object_mut().unwrap();
        let old_interval = cur.get("interval_s").and_then(|v| v.as_f64()).unwrap_or(1.0);
        let old_minutes = cur.get("history_minutes").and_then(|v| v.as_f64()).unwrap_or(5.0);
        cur.insert("interval_s".into(), json!(num(c.get("interval_s"), 0.5, 60.0, old_interval)));
        cur.insert("history_minutes".into(), json!(num(c.get("history_minutes"), 1.0, 1440.0, old_minutes)));
    }

    // auth
    if let Some(a) = body.get("auth") {
        if !a.is_object() || keys_exceed(a, &ALLOWED_AUTH) {
            return Err(err(StatusCode::BAD_REQUEST, "auth 配置项不支持"));
        }
        let cur = cfg.as_object_mut().unwrap()
            .entry("auth").or_insert(json!({})).as_object_mut().unwrap();
        if let Some(u) = a.get("username").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            cur.insert("username".into(), json!(u.trim()));
        }
        if let Some(p) = a.get("password").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            cur.insert("password".into(), json!(p));
            warnings.push("Web 登录密码已修改（保存在 config.toml）".into());
        }
    }

    // 兼容旧前端：targets 走同一套 CRUD 逻辑
    if let Some(ts) = body.get("targets") {
        if !ts.is_object() {
            return Err(err(StatusCode::BAD_REQUEST, "targets 配置项不支持"));
        }
        for (name, t) in ts.as_object().unwrap() {
            if !t.is_object() {
                return Err(err(StatusCode::BAD_REQUEST, "targets 配置项不支持"));
            }
            set_target(cfg, name, t)?;
        }
    }

    // notify
    if let Some(n) = body.get("notify") {
        if !n.is_object() || keys_exceed(n, &ALLOWED_NOTIFY) {
            return Err(err(StatusCode::BAD_REQUEST, "notify 配置项不支持"));
        }
        let cur = cfg.as_object_mut().unwrap()
            .entry("notify").or_insert(json!({})).as_object_mut().unwrap();
        for k in ALLOWED_NOTIFY.iter().take(14) {
            let Some(v) = n.get(*k) else { continue };
            match *k {
                "enabled" => {
                    let b = v.as_bool().unwrap_or_else(|| v.as_str().map(|s| s == "true").unwrap_or(false));
                    cur.insert(k.to_string(), json!(b));
                }
                "dedup_minutes" | "heartbeat_minutes" => {
                    let d = cur.get(*k).and_then(|x| x.as_f64()).unwrap_or(0.0);
                    cur.insert(k.to_string(), json!(num(Some(v), 0.0, 1440.0, d)));
                }
                "smtp_port" => {
                    let p = v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                        .unwrap_or_else(|| cur.get("smtp_port").and_then(|x| x.as_i64()).unwrap_or(465));
                    cur.insert(k.to_string(), json!(p));
                }
                _ => {
                    let s = v.as_str().map(|s| s.trim().to_string()).unwrap_or_default();
                    cur.insert(k.to_string(), json!(s));
                }
            }
        }
    }

    // history
    if let Some(h) = body.get("history") {
        if !h.is_object() || keys_exceed(h, &ALLOWED_HISTORY) {
            return Err(err(StatusCode::BAD_REQUEST, "history 配置项不支持"));
        }
        let cur = cfg.as_object_mut().unwrap()
            .entry("history").or_insert(json!({})).as_object_mut().unwrap();
        let old_keep = cur.get("keep_days").and_then(|x| x.as_f64()).unwrap_or(90.0);
        cur.insert("keep_days".into(), json!(num(h.get("keep_days"), 1.0, 365.0, old_keep)));
        let old_sample = cur.get("sample_s").and_then(|x| x.as_f64()).unwrap_or(60.0);
        cur.insert("sample_s".into(), json!(num(h.get("sample_s"), 5.0, 3600.0, old_sample)));
    }

    // probes（PUT /api/config 内嵌写法：host 清空 = 删除）
    if let Some(ps) = body.get("probes") {
        if !ps.is_object() {
            return Err(err(StatusCode::BAD_REQUEST, "probes 配置项不支持"));
        }
        for (name, p) in ps.as_object().unwrap() {
            if !config::valid_name(name) {
                return Err(err(StatusCode::BAD_REQUEST, "拨测名只能是 1-32 位字母、数字、下划线或中划线"));
            }
            if !p.is_object() || keys_exceed(p, &ALLOWED_PROBE) {
                return Err(err(StatusCode::BAD_REQUEST, "probes 配置项不支持"));
            }
            let host = p.get("host").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            let probes = cfg.as_object_mut().unwrap()
                .entry("probes").or_insert(json!({})).as_object_mut().unwrap();
            if host.is_empty() {
                probes.remove(name);
                continue;
            }
            upsert_probe(cfg, name, p, true)?;
        }
    }

    Ok((cfg["collect"]["interval_s"].as_f64() != before_interval, warnings))
}

// ---------- 退出 ----------

async fn shutdown(
    AxState(st): AxState<Shared>,
    headers: HeaderMap,
    q: Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiErr> {
    auth(&st, &headers, &q)?;
    tracing::info!("收到退出请求，服务即将停止");
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        std::process::exit(0);
    });
    Ok(Json(json!({"ok": true, "message": "服务正在停止"})))
}

// ---------- 历史趋势 ----------

async fn get_history(
    AxState(st): AxState<Shared>,
    headers: HeaderMap,
    q: Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiErr> {
    auth(&st, &headers, &q)?;
    let host = q.get("host").cloned().unwrap_or_default();
    let metric = q.get("metric").cloned().unwrap_or_default();
    let minutes = q
        .get("minutes")
        .and_then(|m| m.parse::<f64>().ok())
        .unwrap_or(60.0)
        .clamp(1.0, 525600.0);
    let to_ts = now_ts();
    let points = st.store.query(&host, &metric, to_ts - minutes * 60.0, to_ts, 800);
    Ok(Json(json!({"host": host, "metric": metric, "minutes": minutes, "points": points})))
}

// ---------- 服务拨测 ----------

async fn list_probes(
    AxState(st): AxState<Shared>,
    headers: HeaderMap,
    q: Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiErr> {
    auth(&st, &headers, &q)?;
    let cfg = st.cfg.read().unwrap();
    Ok(Json(json!({
        "probes": st.probe_mgr.status(),
        "config": cfg.get("probes").cloned().unwrap_or(json!({})),
    })))
}

/// POST/PUT 拨测共用：校验 + 写配置 + 热重载 + 广播。
fn upsert_probe(
    cfg: &mut Value,
    name: &str,
    body: &Value,
    update: bool,
) -> Result<(), ApiErr> {
    let base = if update { cfg["probes"].get(name) } else { None };
    if !config::valid_name(name) {
        return Err(err(StatusCode::BAD_REQUEST, "拨测名只能是 1-32 位字母、数字、下划线或中划线"));
    }
    let get = |k: &str| -> Option<Value> {
        body.get(k).cloned().or_else(|| base.and_then(|b| b.get(k)).cloned())
    };
    let t = get("type")
        .and_then(|v| v.as_str().map(|s| s.to_lowercase()))
        .unwrap_or_else(|| "tcp".into());
    if !["tcp", "http", "ping"].contains(&t.as_str()) {
        return Err(err(StatusCode::BAD_REQUEST, "拨测类型只支持 tcp / http / ping"));
    }
    let host = get("host")
        .and_then(|v| v.as_str().map(|s| s.trim().to_string()))
        .unwrap_or_default();
    if host.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "拨测地址不能为空"));
    }
    let mut entry = base.cloned().unwrap_or_else(|| json!({}));
    let e = entry.as_object_mut().unwrap();
    e.insert("type".into(), json!(t));
    e.insert("host".into(), json!(host));
    if t == "tcp" || t == "http" {
        let default_port = if t == "http" { 80 } else { 22 };
        let port = get("port")
            .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(default_port);
        e.insert("port".into(), json!(port));
    }
    if t == "http" {
        let spec = get("status_codes").unwrap_or_else(|| json!("200-399"));
        if spec.as_str().and_then(crate::probes::status_ranges).is_none() {
            return Err(err(StatusCode::BAD_REQUEST, "HTTP 状态码应为 100–599 的单值或范围，可用逗号分隔"));
        }
        let ms = get("max_ms").unwrap_or(json!(0));
        if !ms.as_f64().is_some_and(|n| (0.0..=30000.0).contains(&n)) {
            return Err(err(StatusCode::BAD_REQUEST, "响应耗时上限应在 0–30000 毫秒之间"));
        }
        e.insert("status_codes".into(), spec);
        e.insert("max_ms".into(), ms);
    }
    e.insert("path".into(), json!(get("path").and_then(|v| v.as_str().map(|s| s.to_string())).unwrap_or_else(|| "/".into())));
    e.insert("expect".into(), json!(get("expect").and_then(|v| v.as_str().map(|s| s.to_string())).unwrap_or_default()));
    e.insert("interval_s".into(), json!(get("interval_s").and_then(|v| v.as_f64()).unwrap_or(30.0)));
    e.insert("timeout_s".into(), json!(get("timeout_s").and_then(|v| v.as_f64()).unwrap_or(5.0)));
    e.insert("fail_threshold".into(), json!(get("fail_threshold").and_then(|v| v.as_i64()).unwrap_or(2)));
    cfg["probes"][name] = entry;
    Ok(())
}

async fn create_probe(
    AxState(st): AxState<Shared>,
    headers: HeaderMap,
    q: Query<HashMap<String, String>>,
    req: axum::extract::Request,
) -> Result<Json<Value>, ApiErr> {
    auth(&st, &headers, &q)?;
    let mut body = read_body_json(req).await?;
    let name = body
        .as_object_mut()
        .and_then(|o| o.remove("name"))
        .and_then(|v| v.as_str().map(|s| s.trim().to_string()))
        .unwrap_or_default();
    edit_config(&st, |cfg| upsert_probe(cfg, &name, &body, false))?;
    Ok(Json(json!({"ok": true, "probes": st.probe_mgr.status()})))
}

async fn update_probe(
    AxState(st): AxState<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
    q: Query<HashMap<String, String>>,
    req: axum::extract::Request,
) -> Result<Json<Value>, ApiErr> {
    auth(&st, &headers, &q)?;
    let body = read_body_json(req).await?;
    edit_config(&st, |cfg| upsert_probe(cfg, &name, &body, true))?;
    Ok(Json(json!({"ok": true, "probes": st.probe_mgr.status()})))
}

async fn delete_probe(
    AxState(st): AxState<Shared>,
    headers: HeaderMap,
    Path(name): Path<String>,
    q: Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiErr> {
    auth(&st, &headers, &q)?;
    edit_config(&st, |cfg| {
        cfg["probes"].as_object_mut().unwrap().remove(&name)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("没有这条拨测：{}", name)))?;
        Ok(())
    })?;
    Ok(Json(json!({"ok": true})))
}

// ---------- 告警推送 ----------

async fn list_deliveries(AxState(st): AxState<Shared>, headers: HeaderMap, q: Query<HashMap<String, String>>) -> Result<Json<Value>, ApiErr> {
    auth(&st, &headers, &q)?;
    let records = st.store.deliveries().map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "投递记录不可用"))?;
    Ok(Json(json!({"records": records})))
}

async fn notify_test(
    AxState(st): AxState<Shared>,
    headers: HeaderMap,
    q: Query<HashMap<String, String>>,
    req: axum::extract::Request,
) -> Result<Json<Value>, ApiErr> {
    auth(&st, &headers, &q)?;
    let body = read_body_json(req).await?;
    let channel = body.get("channel").and_then(|v| v.as_str()).unwrap_or("");
    let (ok, msg) = st.notify.test(channel).await;
    Ok(Json(json!({"ok": ok, "message": msg})))
}

// ---------- Windows 服务 ----------

async fn service_status(
    AxState(st): AxState<Shared>,
    headers: HeaderMap,
    q: Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiErr> {
    auth(&st, &headers, &q)?;
    Ok(Json(crate::service::status().await))
}

async fn service_action(
    AxState(st): AxState<Shared>,
    headers: HeaderMap,
    q: Query<HashMap<String, String>>,
    req: axum::extract::Request,
) -> Result<Json<Value>, ApiErr> {
    auth(&st, &headers, &q)?;
    let body = read_body_json(req).await?;
    let action = body.get("action").and_then(|v| v.as_str()).unwrap_or("");
    let result = match action {
        "install" => crate::service::install().await,
        "uninstall" => crate::service::uninstall().await,
        "start" => crate::service::start().await,
        "stop" => crate::service::stop().await,
        _ => return Err(err(StatusCode::BAD_REQUEST, "action 只支持 install / uninstall / start / stop")),
    };
    match result {
        Ok(message) => Ok(Json(json!({"ok": true, "message": message, "status": crate::service::status().await}))),
        Err(e) => Ok(Json(json!({"ok": false, "message": e}))),
    }
}

// ---------- WebSocket ----------

async fn ws_handler(
    AxState(st): AxState<Shared>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, st))
}

async fn handle_ws(mut socket: axum::extract::ws::WebSocket, st: Shared) {
    use axum::extract::ws::Message;
    // 10 秒内必须发 auth，否则关闭
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), socket.recv()).await;
    let authed = match first {
        Ok(Some(Ok(Message::Text(text)))) => {
            let ok = serde_json::from_str::<Value>(&text)
                .ok()
                .filter(|m| m.get("type").and_then(|v| v.as_str()) == Some("auth"))
                .and_then(|m| m.get("token").and_then(|v| v.as_str()).map(|s| s.to_string()))
                .and_then(|t| st.tokens.verify(&t));
            ok.is_some()
        }
        _ => false,
    };
    if !authed {
        let _ = socket
            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: 4001,
                reason: "unauthorized".into(),
            })))
            .await;
        return;
    }

    // hello：完整快照
    let mut hello = st.snapshot_payload();
    hello.as_object_mut().unwrap().insert("type".into(), json!("hello"));
    // hello 的字段顺序：type 在最前（Python 是 {"type":"hello", **payload}）
    let mut ordered = serde_json::Map::new();
    ordered.insert("type".into(), json!("hello"));
    if let Some(o) = hello.as_object() {
        for (k, v) in o {
            if k != "type" {
                ordered.insert(k.clone(), v.clone());
            }
        }
    }
    if socket
        .send(Message::Text(Value::Object(ordered).to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    let mut rx = st.hub.subscribe();
    loop {
        tokio::select! {
            // 服务端 → 客户端：广播消息
            msg = rx.recv() => {
                match msg {
                    Ok(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            // 客户端 → 服务端：ping/pong
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(m) = serde_json::from_str::<Value>(&text) {
                            if m.get("type").and_then(|v| v.as_str()) == Some("ping") {
                                if socket.send(Message::Text("{\"type\":\"pong\"}".into())).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
}
