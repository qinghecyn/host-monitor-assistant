// 登录与 token 管理。token 默认存 sessionStorage；勾选“记住密码”时
// 同时把凭据存 localStorage（仅本机），token 过期后自动静默重登。

const API_BASE_KEY = 'xmm_api_base';

export function apiBase() {
  return localStorage.getItem(API_BASE_KEY) || '';
}

export function setApiBase(url) {
  url = (url || '').trim().replace(/\/+$/, '');
  if (url && !/^https?:\/\//.test(url)) url = 'http://' + url;
  if (url) localStorage.setItem(API_BASE_KEY, url);
  else localStorage.removeItem(API_BASE_KEY);
}

function wsBase() {
  const b = apiBase();
  if (!b) {
    const proto = location.protocol === 'https:' ? 'wss://' : 'ws://';
    return proto + location.host;
  }
  return b.replace(/^http/, 'ws');
}

export function wsUrl() {
  return wsBase() + '/ws';
}

export function token() {
  return sessionStorage.getItem('xmm_token') || localStorage.getItem('xmm_token') || null;
}

function saveToken(t, remember) {
  if (remember) localStorage.setItem('xmm_token', t);
  else sessionStorage.setItem('xmm_token', t);
}

export function clearAuth() {
  sessionStorage.removeItem('xmm_token');
  localStorage.removeItem('xmm_token');
}

export function savedCreds() {
  try { return JSON.parse(localStorage.getItem('xmm_creds') || 'null'); }
  catch { return null; }
}

export async function login(username, password, remember) {
  const r = await fetch(apiBase() + '/api/login', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ username, password }),
  });
  if (!r.ok) {
    let msg = '登录失败（HTTP ' + r.status + '）';
    try { msg = (await r.json()).detail || msg; } catch {}
    throw new Error(msg);
  }
  const data = await r.json();
  saveToken(data.token, remember);
  if (remember) localStorage.setItem('xmm_creds', JSON.stringify({ username, password }));
  else localStorage.removeItem('xmm_creds');
  return data;
}

export async function authFetch(path, opts = {}) {
  const headers = Object.assign({ Authorization: 'Bearer ' + token() }, opts.headers || {});
  return fetch(apiBase() + path, Object.assign({}, opts, { headers }));
}

// 桌面版（Tauri 拉起后端）用壳进程注入的一次性令牌换会话，用户不用输账号密码。
export async function desktopLogin(desktopToken) {
  const r = await fetch(apiBase() + '/api/login/desktop', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ token: desktopToken }),
  });
  if (!r.ok) {
    let msg = '桌面令牌失效（HTTP ' + r.status + '）';
    try { msg = (await r.json()).detail || msg; } catch {}
    throw new Error(msg);
  }
  const data = await r.json();
  saveToken(data.token, false);
  return data;
}
