// 前端主逻辑：只通过 HTTP 快照 + WebSocket 推送取数，绝不直接碰 SSH。
// beta2.2：新增历史趋势（SQLite）、服务拨测、告警推送设置、Windows 服务管理。

import { apiBase, setApiBase, authFetch, clearAuth, login, desktopLogin, savedCreds, token } from './auth.js';
import { connectWS } from './ws.js';
import { makeChart, makeStaticChart } from './charts.js';
import { initServerManager } from './targets.js';
import { esc, toast } from './ui.js';

const $ = (id) => document.getElementById(id);

const DESKTOP = !!(window.__XMM_BOOTSTRAP__ && window.__XMM_BOOTSTRAP__.token);

const state = {
  targets: [],
  info: {},        // name -> {label, host, port, username, status...}
  current: null,
  thresholds: { temp_c: 70, mem_available_percent: 10, root_free_gb: 1 },
  intervalS: 1,
  historyLimit: 300,
  data: {},        // name -> {status, latest, history:[], alerts:[]}
  charts: null,
  ws: null,
  quitting: false,
  probes: [],      // beta2.2：拨测状态
  historyChart: null,
  hisRangeMin: 60,
  // beta1.3：磁盘读写速率单位，mib / gib，记住用户选择
  diskUnit: localStorage.getItem('xmm.diskUnit') === 'gib' ? 'gib' : 'mib',
  // beta2.5：磁盘名称显示方式，dev（/dev/sdxN 设备名，默认）/ mount（挂载点）
  diskNameMode: localStorage.getItem('xmm.diskNameMode') === 'mount' ? 'mount' : 'dev',
  // beta1.4：网络速率显示格式，bits（kbps/Mbps/Gbps 自适应）/ bytes（KiB/s、MiB/s、GiB/s 自适应）
  netUnit: localStorage.getItem('xmm.netUnit') === 'bytes' ? 'bytes' : 'bits',
  // beta1.4：每台主机单独记住"正在监督"的网卡，空字符串 = 自动跟随主网卡
  netIface: (() => { try { return JSON.parse(localStorage.getItem('xmm.netIface') || '{}') || {}; } catch { return {}; } })(),
};

// ---------- 工具 ----------

function fmtKbps(v) {
  if (v == null) return '—';
  return v >= 1000 ? (v / 1000).toFixed(1) + ' Mbps' : v.toFixed(1) + ' kbps';
}
// beta1.4：后端网络速率单位是 kbit/s。两种自适应显示口径：
//   bits 模式：kbps → Mbps → Gbps；bytes 模式：KiB/s → MiB/s → GiB/s。
const NET_BITS_PER_KBPS = 1000; // 后端 kbps = kbit/s（十进制）
function netRateScale(v) {
  const kbps = Math.max(0, Number(v) || 0);
  if (state.netUnit === 'bytes') {
    const perKib = NET_BITS_PER_KBPS / 8 / 1024; // 1 kbit/s = 1/8.192 KiB/s
    if (kbps * perKib >= 1024 * 1024) return { div: perKib / 1024 / 1024, unit: 'GiB/s' };
    if (kbps * perKib >= 1024) return { div: perKib / 1024, unit: 'MiB/s' };
    return { div: perKib, unit: 'KiB/s' };
  }
  if (kbps >= 1e6) return { div: 1e6, unit: 'Gbps' };
  if (kbps >= 1000) return { div: 1000, unit: 'Mbps' };
  return { div: 1, unit: 'kbps' };
}
function fmtNetRate(v) {
  if (v == null) return '—';
  const kbps = Math.max(0, Number(v) || 0);
  const s = netRateScale(kbps);
  const n = kbps / s.div;
  const digits = s.unit === 'KiB/s' || s.unit === 'kbps' ? 1 : 2;
  return n.toFixed(digits) + ' ' + s.unit;
}
// 累计流量（采集端单位是 KB）→ 人类可读，用于每个网卡行的 title 提示。
function fmtTrafficKB(kb) {
  if (kb == null) return '—';
  const units = ['KB', 'MB', 'GB', 'TB'];
  let v = Math.max(0, kb);
  let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return (i === 0 ? v.toFixed(0) : v.toFixed(v >= 100 ? 0 : v >= 10 ? 1 : 2)) + ' ' + units[i];
}
function fmtGB(kb, digits = 1) {
  if (kb == null) return '—';
  return (kb / 1024 / 1024).toFixed(digits);
}
// beta1.3：后端速率统一是 KB/s，按用户选择的单位显示 MiB/s 或 GiB/s。
function fmtDiskRate(kbps) {
  if (kbps == null) return '—';
  const v = Math.max(0, Number(kbps) || 0);
  if (state.diskUnit === 'gib') return (v / 1024 / 1024).toFixed(3) + ' GiB/s';
  return (v / 1024).toFixed(v >= 10240 ? 0 : 1) + ' MiB/s';
}
function fmtUptime(s) {
  if (s == null) return '—';
  const d = Math.floor(s / 86400), h = Math.floor((s % 86400) / 3600), m = Math.floor((s % 3600) / 60);
  return d > 0 ? `${d}天${h}时${m}分` : h > 0 ? `${h}时${m}分` : `${m}分`;
}

// ---------- 登录 ----------

$('server-url').value = apiBase();

// beta1.3：磁盘速率单位切换（本机记住选择）
function applyDiskUnitButtons() {
  document.querySelectorAll('[data-disk-unit]').forEach((btn) => {
    btn.setAttribute('aria-pressed', String(btn.dataset.diskUnit === state.diskUnit));
  });
  const summary = $('disk-summary');
  if (summary) summary.textContent = `可用空间 · 速率 ${state.diskUnit === 'gib' ? 'GiB/s' : 'MiB/s'}`;
}
document.querySelectorAll('[data-disk-unit]').forEach((btn) => {
  btn.addEventListener('click', () => {
    state.diskUnit = btn.dataset.diskUnit === 'gib' ? 'gib' : 'mib';
    localStorage.setItem('xmm.diskUnit', state.diskUnit);
    applyDiskUnitButtons();
    if (state.current) renderAll();
  });
});
applyDiskUnitButtons();

// beta2.5：磁盘名称显示切换（设备名 / 挂载点，本机记住选择）
function diskLabel(d) {
  const dev = String(d.dev || '').trim();
  return state.diskNameMode === 'dev' && dev ? dev : (d.mount || dev || '—');
}
function diskLabelTitle(d) {
  const label = diskLabel(d);
  const mount = d.mount || '';
  return label !== mount && mount ? `${label} · 挂载点 ${mount}` : label;
}
function applyDiskNameButtons() {
  document.querySelectorAll('[data-disk-name]').forEach((btn) => {
    btn.setAttribute('aria-pressed', String(btn.dataset.diskName === state.diskNameMode));
  });
}
document.querySelectorAll('[data-disk-name]').forEach((btn) => {
  btn.addEventListener('click', () => {
    state.diskNameMode = btn.dataset.diskName === 'mount' ? 'mount' : 'dev';
    localStorage.setItem('xmm.diskNameMode', state.diskNameMode);
    applyDiskNameButtons();
    if (state.current) renderAll();
  });
});
applyDiskNameButtons();

// beta1.4：网络速率双格式切换（bit/s 自适应 / 字节 自适应），本机记住选择
function applyNetUnitButtons() {
  document.querySelectorAll('[data-net-unit]').forEach((btn) => {
    btn.setAttribute('aria-pressed', String(btn.dataset.netUnit === state.netUnit));
  });
}
document.querySelectorAll('[data-net-unit]').forEach((btn) => {
  btn.addEventListener('click', () => {
    const next = btn.dataset.netUnit === 'bytes' ? 'bytes' : 'bits';
    if (next === state.netUnit) return;
    state.netUnit = next;
    localStorage.setItem('xmm.netUnit', next);
    applyNetUnitButtons();
    netChartUnit = '';
    if (state.charts && state.charts.net) state.charts.net.reset(); // 不同量纲不混在一张图里
    if (state.current) renderAll();
  });
});
applyNetUnitButtons();

// beta1.4：点击网卡行切换"正在监督"的接口；再次点击同一行恢复自动跟随主网卡。
$('net-list').addEventListener('click', (ev) => {
  const li = ev.target.closest('li[data-iface]');
  if (!li || !state.current) return;
  const iface = li.dataset.iface;
  const prev = state.netIface[state.current] || '';
  state.netIface[state.current] = prev === iface ? '' : iface;
  localStorage.setItem('xmm.netIface', JSON.stringify(state.netIface));
  netChartUnit = '';
  if (state.charts && state.charts.net) state.charts.net.reset();
  renderAll();
  toast(prev === iface ? '已恢复自动跟随主网卡' : `正在监督 ${iface}`);
});

$('login-form').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  $('login-err').textContent = '';
  $('btn-login').disabled = true;
  try {
    setApiBase($('server-url').value);
    await login($('login-username').value.trim(), $('login-password').value, $('remember').checked);
    await enterMain();
  } catch (e) {
    $('login-err').textContent = e.message || String(e);
  } finally {
    $('btn-login').disabled = false;
  }
});

// ---------- 服务器管理 ----------

const manager = initServerManager({
  authFetch,
  onChanged: (focusName) => {
    if (focusName) state.current = focusName;
    reloadSnapshot().catch(() => {});
  },
  onPick: (name) => {
    if (state.targets.includes(name)) {
      $('target-select').value = name;
      switchTarget(name);
    }
  },
});

$('btn-servers').addEventListener('click', () => manager.open());
$('btn-empty-add').addEventListener('click', () => manager.open());

// ---------- 主界面 ----------

async function enterMain() {
  $('login-view').classList.add('hidden');
  $('main-view').classList.remove('hidden');
  await reloadSnapshot();
  openWS();
}

async function reloadSnapshot() {
  const r = await authFetch('/api/snapshot');
  if (r.status === 401) return tryReauth();
  if (!r.ok) throw new Error('获取快照失败');
  const snap = await r.json();
  applySnapshot(snap);
}

async function tryReauth() {
  const c = savedCreds();
  if (c) {
    try { await login(c.username, c.password, true); return await reloadSnapshot(); } catch {}
  }
  clearAuth();
  showLogin('登录已过期，请重新登录');
}

function showLogin(msg) {
  state.ws && state.ws.close();
  state.ws = null;
  $('main-view').classList.add('hidden');
  $('login-view').classList.remove('hidden');
  if (msg) $('login-err').textContent = msg;
}

function ensureTarget(name) {
  if (!state.data[name])
    state.data[name] = { status: 'connecting', latest: null, history: [], alerts: [] };
  return state.data[name];
}

function applySnapshot(snap) {
  state.thresholds = snap.thresholds || state.thresholds;
  state.intervalS = snap.interval_s || 1;
  state.historyLimit = snap.history_limit || Math.ceil(300 / state.intervalS);
  state.targets = snap.targets || [];
  state.info = {};
  (snap.target_info || []).forEach((t) => { state.info[t.name] = t; });
  state.probes = snap.probes || [];
  for (const [name, d] of Object.entries(snap.data || {})) {
    const t = ensureTarget(name);
    t.status = d.status;
    t.latest = d.latest;
    t.history = (d.history || []).slice(-state.historyLimit);
    t.alerts = d.alerts || [];
  }

  const sel = $('target-select');
  sel.innerHTML = '';
  state.targets.forEach((n) => {
    const info = state.info[n] || {};
    const o = document.createElement('option');
    o.value = n;
    o.textContent = (info.label && info.label !== n ? info.label + '（' + n + '）' : n);
    sel.appendChild(o);
  });
  if (!state.targets.length) {
    const o = document.createElement('option');
    o.value = ''; o.textContent = '未配置主机';
    sel.appendChild(o);
  }
  if (!state.current || !state.targets.includes(state.current)) {
    state.current = state.targets[0] || '';
  }
  sel.value = state.current;
  switchTarget(state.current);
  manager.isOpen() && manager.refresh();
}

function switchTarget(name) {
  state.current = name;
  $('detail-title').textContent = name ? (state.info[name]?.label || name) + ' · 资源详情' : '主机详情';
  renderFleet();
  initCharts();
  const t = ensureTarget(name);
  (t.history || []).forEach((f) => pushCharts(f));
  renderAll();
}

$('target-select').addEventListener('change', (ev) => switchTarget(ev.target.value));

function renderFleet() {
  const box = $('fleet-list');
  const nodes = new Map(Array.from(box.children, el => [el.dataset.host, el]));
  let online = 0, alarmed = 0;
  const percent = value => Number.isFinite(value) ? value.toFixed(0) + '%' : '—';
  for (const name of state.targets) {
    const target = state.data[name] || {};
    const frame = target.latest;
    const age = frame ? Math.max(0, Date.now() / 1000 - frame.ts) : null;
    const stale = age !== null && age > Math.max(10, state.intervalS * 3);
    const status = target.status === 'online' ? (stale ? '数据过期' : frame ? '在线' : '等待采样') :
      target.status === 'offline' ? '离线' : '连接中';
    if (status === '在线') online++;
    const alerts = target.alerts?.length || 0;
    if (alerts) alarmed++;
    let card = nodes.get(name);
    if (!card) {
      card = document.createElement('button');
      card.className = 'fleet-card'; card.dataset.host = name;
      card.innerHTML = '<span class="fleet-name"><strong></strong><small></small></span><span class="fleet-metrics"></span><span class="fleet-foot"></span>';
      box.appendChild(card);
    }
    nodes.delete(name);
    card.setAttribute('aria-pressed', String(name === state.current));
    card.querySelector('strong').textContent = state.info[name]?.label || name;
    const badge = card.querySelector('small');
    badge.textContent = status; badge.className = status === '在线' ? 'value-ok' : 'value-warn';
    const avail = frame?.memory?.available_percent;
    card.querySelector('.fleet-metrics').textContent = 'CPU ' + percent(frame?.cpu?.percent) + '　内存 ' + percent(avail == null ? null : 100 - avail);
    const sampled = age == null ? '等待首个样本' : age < 3 ? '刚刚更新' : Math.floor(age) + ' 秒前更新';
    card.querySelector('.fleet-foot').textContent = sampled + ' · ' + alerts + ' 项告警';
    card.classList.toggle('has-alerts', alerts > 0);
  }
  for (const card of nodes.values()) card.remove();
  $('fleet-count').textContent = state.targets.length + ' 台主机 · ' + online + ' 台在线 · ' + alarmed + ' 台有告警';
}
$('fleet-list').addEventListener('click', event => {
  const card = event.target.closest('button[data-host]');
  if (card) { $('target-select').value = card.dataset.host; switchTarget(card.dataset.host); }
});
setInterval(() => { if (!$('main-view').classList.contains('hidden')) renderFleet(); }, 1000);

// ---------- WebSocket ----------

function openWS() {
  if (state.ws) state.ws.close();
  state.ws = connectWS({
    onHello: (m) => applySnapshot(m),
    onMessage: (m) => {
      if (m.type === 'targets') {
        reloadSnapshot().catch(() => {});
        return;
      }
      if (m.type === 'probe') {
        state.probes = m.probes || [];
        if (probesOpen()) renderProbes();
        return;
      }
      const name = m.target;
      if (!(name in state.data) && m.type === 'metrics') ensureTarget(name);
      const t = state.data[name];
      if (!t) return;
      if (m.type === 'metrics') {
        t.latest = m.data;
        if (m.alerts) t.alerts = m.alerts;
        t.history.push(m.data);
        const cutoff = m.data.ts - state.historyLimit * state.intervalS;
        while (t.history.length && (t.history.length > state.historyLimit || t.history[0].ts < cutoff)) t.history.shift();
        if (name === state.current) { pushCharts(m.data); renderAll(); }
      } else if (m.type === 'status') {
        t.status = m.state;
        if (state.info[name]) state.info[name].status = m.state;
        if (name === state.current) renderAll();
        updateHeaderDot();
      } else if (m.type === 'alert') {
        t.alerts = m.items;
        if (name === state.current) renderAll();
      }
    },
    onState: (s) => {
      if (state.quitting) return;
      if (s === 'unauthorized') { clearAuth(); showLogin('登录已过期，请重新登录'); }
    },
  });
}

// ---------- 图表 ----------

// 网络图当前量纲（kbps/Mbps/Gbps 或 KiB/s/MiB/s/GiB/s）。
// 修复：旧版每帧按各自的自适应除数换算，流量跨过 kbps/Mbps 边界时
// 新旧数据点相差 1000 倍，曲线断崖跳变。现在量纲变化时整图重绘，
// 并加滞回（×1.25 选量纲，降到边界的 80% 以下才降级）避免边界抖动反复清图。
let netChartUnit = '';

function initCharts() {
  if (state.charts) {
    Object.values(state.charts).forEach((c) => { if (c) c.destroy(); });
  }
  netChartUnit = '';
  state.charts = {
    gpu: makeChart('chart-gpu', ['#c48730'], ['GPU %'], state.historyLimit, state.intervalS),
    cpu: makeChart('chart-cpu', ['#168c83'], ['CPU %'], state.historyLimit, state.intervalS),
    mem: makeChart('chart-mem', ['#8870d0'], ['已用 %'], state.historyLimit, state.intervalS),
    temp: makeChart('chart-temp', ['#e5b83c'], ['最高温度 ℃'], state.historyLimit, state.intervalS),
    net: makeChart('chart-net', ['#3ac06d', '#3daee9'], ['下行', '上行'], state.historyLimit, state.intervalS),
  };
}

function pushCharts(f) {
  const c = state.charts;
  if (!c || !f) return;
  const t = (f.ts || Date.now() / 1000) * 1000;
  const gpuBusy = (f.gpus || []).map(gpu => gpu.busy_percent).filter(Number.isFinite);
  c.gpu.push(t, [gpuBusy.length ? Math.max(...gpuBusy) : null]);
  c.cpu.push(t, [f.cpu ? f.cpu.percent : null]);
  const mem = f.memory;
  c.mem.push(t, [mem && mem.available_percent != null ? 100 - mem.available_percent : null]);
  const temps = (f.temperatures || []).map((x) => x.temp_c).filter((v) => v != null);
  c.temp.push(t, [temps.length ? Math.max(...temps) : null]);
  // 折线图跟随"正在监督"的接口（默认自动主网卡），数值按当前自适应单位换算。
  const net = pickMonitoredNet(f.network || []);
  const peak = Math.max(net ? net.rx_kbps || 0 : 0, net ? net.tx_kbps || 0 : 0);
  const scale = netRateScale(peak * 1.25); // 滞回：降级需跌破边界 80%
  if (c.net) {
    if (netChartUnit && scale.unit !== netChartUnit) c.net.reset(); // 不同量纲不混在一张图里
    netChartUnit = scale.unit;
    c.net.push(t, [net ? (net.rx_kbps || 0) / scale.div : null,
                   net ? (net.tx_kbps || 0) / scale.div : null]);
    if (c.net.setUnit) c.net.setUnit(scale.unit);
  }
}

// ---------- 渲染 ----------

function updateHeaderDot() {
  const t = state.data[state.current];
  const st = t ? t.status : 'offline';
  $('cur-dot').className = 'dot ' + st;
}

function renderAll() {
  const name = state.current;
  const t = state.data[name];
  const grid = document.querySelector('.grid');
  const latest = t ? t.latest : null;
  const offline = !t || t.status !== 'online';
  const hasTargets = state.targets.length > 0;

  $('empty-state').classList.toggle('hidden', hasTargets);
  ['card-cpu', 'card-mem', 'card-temp', 'card-fan', 'card-gpu', 'card-net', 'card-disk', 'card-docker'].forEach((id) => {
    $(id).classList.toggle('hidden', !hasTargets);
  });

  // 状态徽标
  const pill = $('status-pill');
  const st = t ? t.status : 'connecting';
  pill.className = 'pill ' + st;
  pill.textContent = { online: '在线', offline: '离线', connecting: '连接中…' }[st] || '…';
  updateHeaderDot();

  const ob = $('offline-banner');
  if (!hasTargets) {
    ob.classList.add('hidden');
  } else if (st === 'connecting') {
    ob.textContent = '正在连接主机…';
    ob.classList.remove('hidden');
  } else if (st === 'offline') {
    ob.textContent = '连接已断开，正在显示最后一次数据（自动重连中…）';
    ob.classList.remove('hidden');
  } else {
    ob.classList.add('hidden');
  }
  grid.classList.toggle('stale', offline);

  if (!name) {
    $('os-badge').textContent = '';
    $('uptime').textContent = '';
    $('alert-banner').classList.add('hidden');
    return;
  }
  $('os-badge').textContent = latest && latest.os ? latest.os.pretty_name : '';
  $('uptime').textContent = '运行 ' + (latest ? fmtUptime(latest.uptime_s) : '—');

  // CPU
  const cpu = latest && latest.cpu;
  $('v-cpu').textContent = cpu && cpu.percent != null ? cpu.percent.toFixed(1) : '—';
  $('cpu-cores').textContent = cpu && cpu.cores != null ? `${cpu.cores} 核` : '';
  const bars = $('cpu-bars');
  if (cpu && cpu.per_core && cpu.per_core.length) {
    if (bars.childElementCount !== cpu.per_core.length) {
      bars.innerHTML = cpu.per_core.map((_, i) =>
        `<div class="core">核${i}<div class="bar"><div class="bar-fill"></div></div></div>`).join('');
    }
    cpu.per_core.forEach((v, i) => {
      const fill = bars.children[i].querySelector('.bar-fill');
      fill.style.width = (v != null ? Math.max(0, Math.min(100, v)) : 0) + '%';
    });
  } else bars.innerHTML = '';
  $('v-load').textContent = '负载 ' + (cpu && cpu.load ? cpu.load.join(' / ') : '—');
  setWarn($('v-cpu'), cpu && cpu.percent != null ? cpu.percent / 100 : null, 0.9);

  // 内存
  const mem = latest && latest.memory;
  if (mem && mem.available_percent != null) {
    const used = 100 - mem.available_percent;
    $('v-mem').textContent = used.toFixed(1);
    $('mem-bar').style.width = used + '%';
    $('mem-bar').style.background = used > 100 - state.thresholds.mem_available_percent ? 'var(--bad)' : 'var(--accent)';
    $('mem-detail').textContent = `已用 ${(mem.total_kb / 1024 - mem.available_kb / 1024).toFixed(0)}MB / 共 ${(mem.total_kb / 1024).toFixed(0)}MB · 可用 ${mem.available_percent}%`;
  } else {
    $('v-mem').textContent = '—';
    $('mem-detail').textContent = '';
    $('mem-bar').style.width = '0%';
  }
  setWarn($('v-mem'), mem && mem.available_percent != null ? 100 - mem.available_percent : null,
          100 - state.thresholds.mem_available_percent / 2, 100 - state.thresholds.mem_available_percent);

  // 温度
  const temps = (latest && latest.temperatures) || [];
  const vals = temps.map((x) => x.temp_c).filter((v) => v != null);
  const maxT = vals.length ? Math.max(...vals) : null;
  $('v-temp').textContent = maxT != null ? maxT.toFixed(1) : '—';
  $('temp-count').textContent = temps.length ? `${temps.length} 个传感器` : '无传感器';
  $('temp-list').innerHTML = temps.length
    ? temps.map((x) => `<li><span>${esc(x.type || x.zone)}</span><span class="${tempCls(x.temp_c)}">${x.temp_c != null ? x.temp_c.toFixed(1) + ' ℃' : '—'}</span></li>`).join('')
    : '<li class="muted">—</li>';
  setWarn($('v-temp'), maxT != null ? maxT : null, state.thresholds.temp_c, state.thresholds.temp_c);

  // 风扇（beta2.1）：rpm=0 是有效读数（停转/DC 调速），单列显示不算错误
  const fans = (latest && latest.fans) || [];
  const maxR = fans.length ? Math.max(...fans.map((x) => x.rpm || 0)) : null;
  $('v-fan').textContent = maxR != null ? String(maxR) : '—';
  $('fan-count').textContent = fans.length ? `${fans.length} 个风扇` : '无风扇';
  $('fan-note').textContent = (latest && !fans.length && latest.temperatures && latest.temperatures.length)
    ? '未检测到风扇转速探头（被动散热或未加载 hwmon 驱动）' : '';
  $('fan-list').innerHTML = fans.length
    ? fans.map((x) => `<li><span>${esc(x.name)}</span><span class="${x.rpm > 0 ? '' : 'muted'}">${x.rpm} RPM${x.rpm === 0 ? '（停转）' : ''}</span></li>`).join('')
    : '<li class="muted">—</li>';

  // GPU（beta2.4）：NVIDIA / AMD / Intel，采集端没有显卡时整段缺席
  const gpus = (latest && latest.gpus) || [];
  const busyVals = gpus.map((x) => x.busy_percent).filter((v) => v != null);
  const maxG = busyVals.length ? Math.max(...busyVals) : null;
  $('v-gpu').textContent = maxG != null ? String(maxG) : '—';
  $('gpu-count').textContent = gpus.length ? `${gpus.length} 块显卡` : '无显卡';
  $('gpu-details').hidden = !gpus.length;
  $('gpu-note').textContent = (latest && !gpus.length && hasTargets)
    ? '未检测到 GPU（无独显、未装驱动，或驱动不带 sysfs 接口）' : '';
  $('gpu-list').innerHTML = gpus.length
    ? gpus.map(gpuRow).join('')
    : '<li class="muted">—</li>';
  setWarn($('v-gpu'), maxG, 85, 95);
  for (const [id, value] of [['cpu', cpu?.percent], ['mem', mem?.available_percent == null ? null : 100 - mem.available_percent], ['gpu', maxG]]) {
    const card = $('card-' + id);
    card.style.setProperty('--used', Math.max(0, Math.min(100, value ?? 0)) + '%');
    card.classList.toggle('no-reading', !Number.isFinite(value));
    const [used, free] = card.querySelectorAll('.meter-legend b');
    used.textContent = Number.isFinite(value) ? value.toFixed(1) + '%' : '—';
    free.textContent = Number.isFinite(value) ? (100 - value).toFixed(1) + '%' : '—';
  }

  // 网络：大数字与折线图跟随"正在监督"的接口（默认自动主网卡），列表可点击切换（beta1.4）。
  const nets = (latest && latest.network) || [];
  const n0 = pickMonitoredNet(nets);
  const watched = state.current ? (state.netIface[state.current] || '') : '';
  const isActiveNet = (n) => (n.rx_kbps || 0) > 0 || (n.tx_kbps || 0) > 0
    || (n.rx_total_kb || 0) > 0 || (n.tx_total_kb || 0) > 0;
  const activeCount = nets.filter(isActiveNet).length;
  if (n0) {
    $('v-rx').textContent = fmtNetRate(n0.rx_kbps);
    $('v-tx').textContent = fmtNetRate(n0.tx_kbps);
    $('net-ifaces').textContent = nets.length > 1
      ? `${n0.iface}（共 ${nets.length} 个接口 · ${activeCount} 个有流量）`
      : n0.iface;
    $('net-ifaces').title = watched
      ? `正在监督：${watched}（再点该行恢复自动）`
      : `自动跟随主网卡：${n0.iface}（点击列表可指定监督接口）`;
  } else {
    $('v-rx').textContent = $('v-tx').textContent = '—';
    $('net-ifaces').textContent = '';
    $('net-ifaces').title = '';
  }

  // 全部网卡逐行展示（beta2.3）：被监督接口置顶，其次主网卡/活跃接口，其余按累计流量排序。
  // beta1.4：每行可点击指定监督，再点一次恢复自动；主网卡仅表示后端自动挑选结果。
  const rows = nets.slice().sort((a, b) => {
    const wa = watched && a.iface === watched ? 0 : 1;
    const wb = watched && b.iface === watched ? 0 : 1;
    if (wa !== wb) return wa - wb;
    const pa = a.primary || (n0 && a.iface === n0.iface) ? 0 : 1;
    const pb = b.primary || (n0 && b.iface === n0.iface) ? 0 : 1;
    if (pa !== pb) return pa - pb;
    const aa = isActiveNet(a) ? 0 : 1;
    const ab = isActiveNet(b) ? 0 : 1;
    if (aa !== ab) return aa - ab;
    const sa = (a.rx_total_kb || 0) + (a.tx_total_kb || 0);
    const sb = (b.rx_total_kb || 0) + (b.tx_total_kb || 0);
    if (sa !== sb) return sb - sa;
    return String(a.iface || '').localeCompare(String(b.iface || ''));
  });
  $('net-list').innerHTML = rows.length ? rows.map((n) => {
    const zero = !isActiveNet(n);
    const primary = !!n.primary || (n0 && n.iface === n0.iface);
    const selected = !!watched && watched === n.iface;
    const down = String(n.state || '').toLowerCase() === 'down';
    const title = `点击${selected ? '取消监督' : '监督'} ${n.iface}`
      + ` · 累计 ↓ ${fmtTrafficKB(n.rx_total_kb)} · ↑ ${fmtTrafficKB(n.tx_total_kb)}`
      + (n.state ? ` · 状态 ${n.state}` : '');
    return `<li data-iface="${esc(n.iface)}" class="${zero ? 'net-zero' : ''}${down ? ' net-down' : ''}${selected ? ' net-selected' : ''}" title="${esc(title)}">
      <span class="net-name">${esc(n.iface)}${selected ? '<span class="net-badge net-watch">督</span>' : ''}${primary ? '<span class="net-badge">主</span>' : ''}${down ? '<span class="net-state">未连接</span>' : ''}</span>
      <span class="net-rate">↓ ${fmtNetRate(n.rx_kbps)} · ↑ ${fmtNetRate(n.tx_kbps)}</span>
    </li>`;
  }).join('') : '<li class="muted">—</li>';

  // 磁盘：丢掉 efivarfs 这种总量几乎为 0 的伪挂载
  // beta1.3：每块盘直接标出使用率，并展示按 MiB/s / GiB/s 切换的读写速率
  const disks = ((latest && latest.disks) || []).filter((d) => (d.total_kb || 0) > 1024);
  $('disk-list').innerHTML = disks.length
    ? disks.map((d) => {
        const avail = d.available_kb || 0;
        const usedPct = d.total_kb ? Math.min(100, Math.max(0, 100 * (1 - avail / d.total_kb))) : 0;
        const bad = d.mount === '/' && avail / 1024 / 1024 < state.thresholds.root_free_gb;
        const pctClass = usedPct >= 90 ? 'value-bad' : usedPct >= 75 ? 'value-warn' : '';
        return `<li>
          <span class="mount" title="${esc(diskLabelTitle(d))}">${esc(diskLabel(d))}</span>
          <div class="bar"><div class="bar-fill" style="width:${usedPct.toFixed(0)}%;background:${bad ? 'var(--bad)' : 'var(--accent)'}"></div></div>
          <span class="disk-pct ${pctClass}">已用 ${usedPct.toFixed(0)}%</span>
          <div class="disk-meta">
            <span class="disk-io">↓ ${fmtDiskRate(d.read_kbps)} · ↑ ${fmtDiskRate(d.write_kbps)}</span>
            <span class="avail ${bad ? 'value-bad' : ''}">可用 ${fmtGB(avail)}G / 共 ${fmtGB(d.total_kb)}G</span>
          </div>
        </li>`;
      }).join('')
    : '<li class="muted">—</li>';

  // Docker 负载（beta2.0）
  renderDocker(latest && latest.docker, offline);

  // 告警横幅
  const items = t.alerts || [];
  const banner = $('alert-banner');
  if (items.length) {
    banner.classList.remove('hidden');
    banner.innerHTML = '⚠ ' + items.map((i) => esc(i.text)).join('　｜　');
  } else {
    banner.classList.add('hidden');
  }
}

// 多网卡时挑一张"主"网卡：优先 up 状态、eth/en/wl/br/lan/wan 开头且累计流量最大的那张。
// 路由器上常见 tunl0、ip6tnl0 这类隧道口，按顺序取第一个会一直显示 0。
// 后端解析时已经标了 primary（store.py 历史曲线复用同一口径），这里保留兜底逻辑，
// 兼容旧后端/旧 JSON。
function pickPrimaryNet(nets) {
  if (!nets || !nets.length) return null;
  const marked = nets.find((n) => n.primary);
  if (marked) return marked;
  const score = (n) => (n.rx_total_kb || 0) + (n.tx_total_kb || 0);
  const up = nets.filter((n) => String(n.state || '').toLowerCase() !== 'down');
  const poolUp = up.length ? up : nets;
  const real = poolUp.filter((n) => /^(eth|en|wl|br-|lan|wan|bond|pppoe|ap)/i.test(n.iface || ''));
  const pool = real.length ? real : poolUp;
  return pool.slice().sort((a, b) => score(b) - score(a))[0] || null;
}

// beta1.4：用户在网卡列表里点选的接口优先；该主机没选（或接口已消失）时回退自动主网卡。
function pickMonitoredNet(nets) {
  const saved = state.current ? state.netIface[state.current] : '';
  if (saved) {
    const hit = (nets || []).find((n) => n.iface === saved);
    if (hit) return hit;
  }
  return pickPrimaryNet(nets);
}

// ---------- Docker 负载（beta2.0） ----------

// 目标机没装 docker / 守护进程没起 / 容器缺 stats，都各自有明确的展示，
// 不让"取不到"伪装成"没有"。
function renderDocker(dk, offline) {
  const head = $('docker-table-head');
  const list = $('docker-list');
  const summary = $('docker-summary');
  if (!dk) {
    summary.textContent = '';
    head.classList.add('hidden');
    list.innerHTML = '<div class="dk-note muted">此主机未上报 Docker 信息（未安装或采集脚本较旧）</div>';
    return;
  }
  if (dk.daemon === 'none') {
    summary.textContent = '';
    head.classList.add('hidden');
    list.innerHTML = '<div class="dk-note muted">这台机器上没有安装 Docker</div>';
    return;
  }
  if (dk.daemon === 'down') {
    summary.textContent = '';
    head.classList.add('hidden');
    list.innerHTML = '<div class="dk-note value-warn">Docker 守护进程没有在运行（docker 服务未启动）</div>';
    return;
  }

  const cs = (dk.containers || []).slice();
  summary.textContent = `运行 ${dk.running ?? 0} · 共 ${dk.total ?? cs.length} 个容器`;
  if (!cs.length) {
    head.classList.add('hidden');
    list.innerHTML = '<div class="dk-note muted">Docker 正常，还没有创建任何容器</div>';
    return;
  }
  head.classList.remove('hidden');
  // 运行中的排前面，再按名称
  cs.sort((a, b) => (a.state === 'running' ? 0 : 1) - (b.state === 'running' ? 0 : 1)
    || String(a.name).localeCompare(String(b.name)));
  list.innerHTML = cs.map((c) => {
    const running = c.state === 'running';
    const cpu = c.cpu_percent != null ? c.cpu_percent.toFixed(2) + '%' : '—';
    const memPct = c.mem_percent != null ? Math.min(100, Math.max(0, c.mem_percent)) : null;
    const memCls = memPct != null && memPct >= 80 ? 'var(--bad)' : memPct != null && memPct >= 60 ? 'var(--warn)' : 'var(--accent)';
    const memTxt = c.mem_used != null
      ? `${c.mem_used} / ${c.mem_limit || '—'}${c.mem_percent != null ? `（${c.mem_percent.toFixed(1)}%）` : ''}`
      : '—';
    return `<div class="docker-row ${running ? '' : 'stopped'}">
      <span class="dk-name">
        <b>${esc(c.name || c.id)}</b>
        <small>${esc(c.image || '')}${running ? '' : ` · ${esc(c.status || c.state)}`}</small>
      </span>
      <span class="dk-state ${esc(c.state || '')}">${esc(running ? '运行中' : c.status || c.state || '—')}</span>
      <span class="dk-num ${running ? '' : 'muted'}">${cpu}</span>
      <span class="dk-mem">
        ${memPct != null ? `<div class="bar"><div class="bar-fill" style="width:${memPct.toFixed(0)}%;background:${memCls}"></div></div>` : ''}
        <small>${esc(memTxt)}</small>
      </span>
      <span class="dk-io dk-net">${esc(c.net_io || '—')}</span>
      <span class="dk-io dk-blk">${esc(c.block_io || '—')}</span>
    </div>`;
  }).join('') + (offline ? '<div class="dk-note muted">主机离线，显示最后一次采集结果</div>' : '');
}

function tempCls(v) {  if (v == null) return '';
  if (v > state.thresholds.temp_c) return 'value-bad';
  if (v > state.thresholds.temp_c - 10) return 'value-warn';
  return '';
}

// 单块显卡一行：占用 / 显存 / 温度 / 功耗，缺哪项就少显示哪项
$('gpu-details').addEventListener('click', event => {
  const button = event.currentTarget;
  button.setAttribute('aria-expanded', button.getAttribute('aria-expanded') === 'false');
});

function gpuRow(x) {
  const bits = [];
  if (x.busy_percent != null) bits.push(`占用 ${x.busy_percent}%`);
  if (x.mem_total_mb != null) {
    bits.push(`显存 ${x.mem_used_mb == null ? '—' : Math.round(x.mem_used_mb)}/${Math.round(x.mem_total_mb)} MiB${x.mem_percent != null ? `（${x.mem_percent}%）` : ''}`);
  }
  if (x.temp_c != null) bits.push(`${x.temp_c} ℃`);
  if (x.power_w != null) bits.push(`${x.power_w} W`);
  const drv = x.driver ? ` <span class="muted">· ${esc(x.driver)}</span>` : '';
  return `<li><span>${esc(x.name)}${drv}</span>` +
    `<span class="${tempCls(x.temp_c)}">${bits.join(' · ') || '—'}</span></li>`;
}

function setWarn(el, v, warnAt, badAt) {
  if (v == null) { el.className = ''; return; }
  el.className = badAt != null && v >= badAt ? 'value-bad' : v >= warnAt ? 'value-warn' : '';
}

// ---------- 历史趋势（beta2.2） ----------

const HIS_METRICS = [
  ['cpu.percent', 'CPU %'],
  ['mem.used_percent', '内存已用 %'],
  ['temp.max', '最高温度 ℃'],
  ['fan.max', '风扇 RPM'],
  ['gpu.busy_percent', 'GPU 占用 %'],
  ['gpu.mem_used_percent', 'GPU 显存 %'],
  ['gpu.temp_c', 'GPU 温度 ℃'],
  ['gpu.power_w', 'GPU 功耗 W'],
  ['net.rx_kbps', '下行 kbps'],
  ['net.tx_kbps', '上行 kbps'],
  ['disk.root_used_percent', '根分区已用 %'],
  ['docker.running', 'Docker 运行数'],
];

function hisOpen() { return !$('history-panel').classList.contains('hidden'); }

function openHistory() {
  $('history-mask').classList.remove('hidden');
  $('history-panel').classList.remove('hidden');
  const sel = $('his-host');
  sel.innerHTML = '';
  state.targets.forEach((n) => {
    const info = state.info[n] || {};
    const o = document.createElement('option');
    o.value = n;
    o.textContent = info.label && info.label !== n ? info.label + '（' + n + '）' : n;
    sel.appendChild(o);
  });
  if (!state.targets.length) {
    const o = document.createElement('option');
    o.value = ''; o.textContent = '未配置主机';
    sel.appendChild(o);
  }
  const ms = $('his-metric');
  ms.innerHTML = HIS_METRICS.map(([k, label]) => `<option value="${k}">${label}</option>`).join('');
  document.querySelectorAll('#his-ranges button').forEach((b) =>
    b.classList.toggle('active', +b.dataset.min === state.hisRangeMin));
  if (!sel.value && state.current) sel.value = state.current;
  loadHistory();
}

function closeHistory() {
  $('history-mask').classList.add('hidden');
  $('history-panel').classList.add('hidden');
}

async function loadHistory() {
  const host = $('his-host').value;
  const metric = $('his-metric').value;
  const minutes = state.hisRangeMin;
  const infoEl = $('his-info');
  if (!host || !metric) { infoEl.textContent = '还没有可查询的主机。'; return; }
  infoEl.textContent = '加载中…';
  const label = (HIS_METRICS.find(([k]) => k === metric) || [null, metric])[1];
  try {
    const r = await authFetch(`/api/history?host=${encodeURIComponent(host)}&metric=${encodeURIComponent(metric)}&minutes=${minutes}`);
    if (r.status === 401) return tryReauth();
    if (!r.ok) throw new Error('HTTP ' + r.status);
    const d = await r.json();
    const pts = d.points || [];
    if (!state.historyChart) state.historyChart = makeStaticChart('chart-history', '#3daee9', label, hisFmtX(minutes));
    state.historyChart.render(pts);
    infoEl.textContent = pts.length
      ? `共 ${pts.length} 个采样点 · 数据保存在本机 history.db（保留时长可在设置里调整）`
      : '这个时间范围内没有数据（历史落盘从 beta2.2 起才开始积累）。';
  } catch (e) {
    infoEl.textContent = '加载失败：' + (e.message || e);
  }
}

function hisFmtX(minutes) {
  return (ms) => {
    const d = new Date(ms);
    const p = (x) => String(x).padStart(2, '0');
    if (minutes <= 60 * 26) return p(d.getMonth() + 1) + '-' + p(d.getDate()) + ' ' + p(d.getHours()) + ':' + p(d.getMinutes());
    return p(d.getMonth() + 1) + '-' + p(d.getDate());
  };
}

// ---------- 服务拨测（beta2.2） ----------

function probesOpen() { return !$('probes-panel').classList.contains('hidden'); }

function openProbes() {
  $('probes-mask').classList.remove('hidden');
  $('probes-panel').classList.remove('hidden');
  renderProbes();
  refreshProbes();
}

async function refreshProbes() {
  try {
    const response = await authFetch('/api/probes');
    if (response.ok && probesOpen()) { state.probes = (await response.json()).probes; renderProbes(); }
  } catch {}
}
setInterval(() => { if (probesOpen()) refreshProbes(); }, 5000);

function closeProbes() {
  $('probes-mask').classList.add('hidden');
  $('probes-panel').classList.add('hidden');
}

function renderProbes() {
  const box = $('probe-list');
  if (!state.probes.length) {
    box.innerHTML = '<div class="sv-empty">还没有拨测项。点下面按钮添加，比如探测路由器的 443 端口。</div>';
    return;
  }
  const stTxt = { up: '正常', down: '失败', unknown: '…' };
  box.innerHTML = state.probes.map((p) => `
    <div class="probe-row ${esc(p.state || 'unknown')}" data-name="${esc(p.name)}">
      <div class="probe-main">
        <div class="probe-title">
          <b>${esc(p.name)}</b>
          <span class="sv-key">${esc(p.type)}</span>
          <span class="sv-status ${p.state === 'up' ? 'online' : p.state === 'down' ? 'offline' : ''}">
            ${stTxt[p.state] || '…'}${p.ms != null ? ' · ' + p.ms.toFixed(0) + ' ms' : ''}
          </span>
        </div>
        <div class="probe-sub">${esc(p.host)}${p.port ? ':' + p.port : ''}${p.path && p.path !== '/' ? esc(p.path) : ''}
          ${p.detail ? ' · ' + esc(p.detail) : ''}${p.fails ? ' · 连续失败 ' + p.fails + ' 次' : ''}</div>
      </div>
      <div class="sv-actions">
        <button class="ghost sm" data-act="edit">编辑</button>
        <button class="ghost sm danger" data-act="del">删除</button>
      </div>
    </div>`).join('');
}

$('probe-list').addEventListener('click', async (ev) => {
  const btn = ev.target.closest('button[data-act]');
  if (!btn) return;
  const name = btn.closest('.probe-row').dataset.name;
  if (btn.dataset.act === 'edit') {
    const p = state.probes.find((x) => x.name === name) || {};
    startProbeForm(name, p);
  } else if (btn.dataset.act === 'del') {
    if (!confirm('删除拨测「' + name + '」？')) return;
    try {
      const r = await authFetch('/api/probes/' + encodeURIComponent(name), { method: 'DELETE' });
      if (!r.ok) throw new Error((await r.json().catch(() => ({}))).detail || 'HTTP ' + r.status);
      toast('已删除 ' + name);
    } catch (e) { toast('删除失败：' + e.message, 'err'); }
  }
});

function startProbeForm(name, p) {
  p = p || {};
  $('probe-form-title').textContent = name ? '编辑：' + name : '添加拨测';
  $('pb-name').value = name || '';
  $('pb-name').disabled = !!name;
  $('pb-type').value = p.type || 'tcp';
  $('pb-host').value = p.host || '';
  $('pb-port').value = p.port || 443;
  $('pb-path').value = p.path && p.path !== '/' ? p.path : '';
  $('pb-expect').value = p.expect || '';
  $('pb-codes').value = p.status_codes || '200-399';
  $('pb-max-ms').value = p.max_ms ?? 0;
  $('pb-type').dispatchEvent(new Event('change'));
  $('pb-interval').value = p.interval_s != null ? Math.round(p.interval_s) : 30;
  $('pb-threshold').value = p.fail_threshold || 2;
  $('pb-err').textContent = '';
  $('btn-probe-add').classList.add('hidden');
  $('probe-form').classList.remove('hidden');
}

function hideProbeForm() {
  $('probe-form').classList.add('hidden');
  $('btn-probe-add').classList.remove('hidden');
  $('pb-err').textContent = '';
}

$('pb-type').addEventListener('change', () => {
  const t = $('pb-type').value;
  $('pb-port').disabled = t === 'ping';
  $('pb-path').disabled = $('pb-expect').disabled = t !== 'http';
  $('pb-http').hidden = t !== 'http';
});

$('btn-probe-add').addEventListener('click', () => startProbeForm('', {}));
$('btn-probe-cancel').addEventListener('click', hideProbeForm);

$('probe-form').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const name = $('pb-name').value.trim();
  const body = {
    type: $('pb-type').value,
    host: $('pb-host').value.trim(),
    port: +$('pb-port').value || 443,
    path: $('pb-path').value.trim() || '/',
    expect: $('pb-expect').value.trim(),
    status_codes: $('pb-codes').value.trim(),
    max_ms: +$('pb-max-ms').value,
    interval_s: +$('pb-interval').value || 30,
    fail_threshold: +$('pb-threshold').value || 2,
  };
  if (!/^[A-Za-z0-9_-]{1,32}$/.test(name)) { $('pb-err').textContent = '名称只能是 1-32 位字母、数字、下划线或中划线'; return; }
  if (!body.host) { $('pb-err').textContent = '拨测地址不能为空'; return; }
  if (body.type !== 'ping' && !(body.port >= 1 && body.port <= 65535)) { $('pb-err').textContent = '端口要在 1-65535 之间'; return; }
  try {
    const r = await authFetch('/api/probes/' + encodeURIComponent(name), {
      method: 'PUT', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
    const d = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(d.detail || 'HTTP ' + r.status);
    toast('拨测已保存并生效');
    hideProbeForm();
  } catch (e) {
    $('pb-err').textContent = e.message || String(e);
  }
});

// ---------- 告警推送 / Windows 服务（beta2.2，设置面板内） ----------

function fillNotifyFields(n) {
  n = n || {};
  $('nt-enabled').checked = n.enabled !== false;
  $('nt-dedup').value = n.dedup_minutes != null ? n.dedup_minutes : 10;
  $('nt-heartbeat').value = n.heartbeat_minutes != null ? n.heartbeat_minutes : 0;
  $('nt-quiet-start').value = n.quiet_start || '';
  $('nt-quiet-end').value = n.quiet_end || '';
  $('nt-tg-chat').value = n.telegram_chat || '';
  $('nt-smtp-host').value = n.smtp_host || '';
  $('nt-smtp-port').value = n.smtp_port != null ? n.smtp_port : 465;
  $('nt-smtp-user').value = n.smtp_user || '';
  $('nt-smtp-to').value = n.smtp_to || '';
  $('nt-serverchan').value = ''; $('nt-tg-token').value = ''; $('nt-smtp-pass').value = ''; $('nt-webhook').value = '';
  $('nt-serverchan-has').textContent = n.has_serverchan ? '✓ 已配置 SendKey（留空 = 保持不变）' : '未配置';
  // secret 掩码位
  state._ntHas = { has_telegram: !!n.has_telegram, has_smtp: !!n.has_smtp, has_webhook: !!n.has_webhook };
}

function collectNotify() {
  const body = {
    enabled: $('nt-enabled').checked,
    dedup_minutes: +$('nt-dedup').value || 0,
    heartbeat_minutes: +$('nt-heartbeat').value || 0,
    quiet_start: $('nt-quiet-start').value.trim(),
    quiet_end: $('nt-quiet-end').value.trim(),
    telegram_chat: $('nt-tg-chat').value.trim(),
    smtp_host: $('nt-smtp-host').value.trim(),
    smtp_port: +$('nt-smtp-port').value || 465,
    smtp_user: $('nt-smtp-user').value.trim(),
    smtp_to: $('nt-smtp-to').value.trim(),
  };
  if ($('nt-serverchan').value.trim()) body.serverchan_key = $('nt-serverchan').value.trim();
  if ($('nt-tg-token').value.trim()) body.telegram_token = $('nt-tg-token').value.trim();
  if ($('nt-smtp-pass').value.trim()) body.smtp_pass = $('nt-smtp-pass').value.trim();
  if ($('nt-webhook').value.trim()) body.webhook_url = $('nt-webhook').value.trim();
  return body;
}

async function saveNotifyOnly() {
  const r = await authFetch('/api/config', {
    method: 'PUT', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ notify: collectNotify() }),
  });
  if (r.status === 401) { await tryReauth(); return false; }
  const d = await r.json().catch(() => ({}));
  if (!r.ok) { toast(d.detail || '保存失败', 'err'); return false; }
  return true;
}

async function testChannel(channel) {
  if (!(await saveNotifyOnly())) return;
  toast('测试消息发送中…');
  try {
    const r = await authFetch('/api/notify/test', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ channel }),
    });
    const d = await r.json();
    toast(d.message || (d.ok ? '发送成功' : '发送失败'), d.ok ? 'ok' : 'err', 5000);
  } catch (e) { toast('发送失败：' + e.message, 'err'); }
}

$('btn-test-serverchan').addEventListener('click', () => testChannel('serverchan'));
$('btn-test-telegram').addEventListener('click', () => testChannel('telegram'));
$('btn-test-smtp').addEventListener('click', () => testChannel('smtp'));
$('btn-test-webhook').addEventListener('click', () => testChannel('webhook'));

async function refreshServiceStatus() {
  const info = $('svc-info');
  try {
    const r = await authFetch('/api/service');
    const d = await r.json();
    if (!d.available) {
      info.textContent = '服务组件未打包（源码模式），打包版可用。';
      return;
    }
    const stTxt = d.state === 'running' ? '运行中' : d.state === 'stopped' ? '已停止' : d.installed ? d.state : '未安装';
    info.textContent = `当前状态：${stTxt}。服务开机自启、崩溃自动拉起，监听 127.0.0.1:8787；与桌面窗口模式请二选一运行。`;
  } catch (e) {
    info.textContent = '状态获取失败：' + (e.message || e);
  }
}

$('btn-svc-refresh').addEventListener('click', refreshServiceStatus);
$('btn-svc-install').addEventListener('click', async () => {
  if (!confirm('注册系统服务？服务将开机自启并监听 127.0.0.1:8787（需要管理员权限弹窗确认）。')) return;
  $('svc-err').textContent = '';
  try {
    const r = await authFetch('/api/service', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ action: 'install' }),
    });
    const d = await r.json();
    $('svc-err').textContent = d.message || (d.ok ? '已安装' : '失败');
    $('svc-err').className = d.ok ? 'hint' : 'err';
    refreshServiceStatus();
  } catch (e) { $('svc-err').textContent = e.message || String(e); }
});
$('btn-svc-uninstall').addEventListener('click', async () => {
  if (!confirm('卸载系统服务？')) return;
  $('svc-err').textContent = '';
  try {
    const r = await authFetch('/api/service', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ action: 'uninstall' }),
    });
    const d = await r.json();
    $('svc-err').textContent = d.message || (d.ok ? '已卸载' : '失败');
    $('svc-err').className = d.ok ? 'hint' : 'err';
    refreshServiceStatus();
  } catch (e) { $('svc-err').textContent = e.message || String(e); }
});

// ---------- 历史趋势 / 拨测 抽屉开关 ----------

$('btn-history').addEventListener('click', openHistory);
$('btn-his-close').addEventListener('click', closeHistory);
$('history-mask').addEventListener('click', closeHistory);
$('his-host').addEventListener('change', loadHistory);
$('his-metric').addEventListener('change', loadHistory);
$('his-ranges').addEventListener('click', (ev) => {
  const b = ev.target.closest('button[data-min]');
  if (!b) return;
  state.hisRangeMin = +b.dataset.min;
  document.querySelectorAll('#his-ranges button').forEach((x) =>
    x.classList.toggle('active', x === b));
  loadHistory();
});

$('btn-probes').addEventListener('click', openProbes);
$('btn-probe-close').addEventListener('click', closeProbes);
$('probes-mask').addEventListener('click', closeProbes);

// ---------- 设置面板 ----------

$('btn-config').addEventListener('click', openConfig);
$('btn-cfg-close').addEventListener('click', closeConfig);
$('config-mask').addEventListener('click', closeConfig);

function addRule(rule = {host:'*', metric:'cpu', mount:'*', below:3, recover:5, seconds:30}) {
  const row = document.createElement('fieldset');
  row.className = 'resource-rule';
  row.innerHTML = `<label>主机<select class="sel" data-key="host"></select></label>
    <label>资源<select class="sel" data-key="metric"></select></label>
    <label>磁盘挂载点<input data-key="mount" placeholder="* 或 /data"></label>
    <label>触发剩余 %<input data-key="below" type="number" required min="0" max="100" step="any"></label>
    <label>恢复剩余 %<input data-key="recover" type="number" required min="0" max="100" step="any"></label>
    <label>持续秒数<input data-key="seconds" type="number" required min="0" max="86400"></label>
    <button type="button" class="ghost sm danger">移除规则</button>`;
  const input = key => row.querySelector('[data-key="' + key + '"]');
  for (const host of new Set(['*', ...state.targets, rule.host])) input('host').add(new Option(host === '*' ? '所有主机' : host, host));
  for (const [value,label] of Object.entries({all:'全部资源',cpu:'CPU',memory:'内存',gpu:'GPU',disk:'磁盘'})) input('metric').add(new Option(label,value));
  for (const [key,value] of Object.entries(rule)) if (input(key)) input(key).value = value;
  const scope = () => {
    input('mount').disabled = input('metric').value !== 'disk';
    if (input('mount').disabled) input('mount').value = '*';
  };
  scope(); input('metric').onchange = scope;
  row.querySelector('button').onclick = () => row.remove();
  $('rule-list').appendChild(row);
}

function readRules() {
  return Array.from($('rule-list').children, row => Object.fromEntries(
    Array.from(row.querySelectorAll('[data-key]'), el => {
      if (!el.reportValidity()) throw new Error('请检查资源规则输入');
      return [el.dataset.key, el.type === 'number' ? el.valueAsNumber : el.value.trim()];
    })
  ));
}
$('add-rule').onclick = () => addRule();

async function refreshDeliveries() {
  try {
    const response = await authFetch('/api/notify/deliveries');
    if (!response.ok) throw new Error('读取投递记录失败');
    const {records} = await response.json();
    $('delivery-list').innerHTML = records.length ? records.map(r =>
      `<article><b class="${r.ok ? 'value-ok' : 'value-bad'}">${r.ok ? '通道已接受' : '失败'}</b> · ${esc(r.channel)} · 第 ${r.attempt} 次
      <div>${esc(r.title)}</div><small>${esc(new Date(r.ts*1000).toLocaleString())} · ${esc(r.detail)}</small></article>`).join('') : '暂无投递记录';
  } catch (e) { $('delivery-list').textContent = e.message; }
}
$('refresh-deliveries').onclick = refreshDeliveries;
$('delivery-panel').ontoggle = () => { if ($('delivery-panel').open) refreshDeliveries(); };
setInterval(() => { if ($('delivery-panel').open && !$('config-panel').classList.contains('hidden')) refreshDeliveries(); }, 5000);

async function openConfig() {
  const r = await authFetch('/api/config');
  if (r.status === 401) return tryReauth();
  const c = await r.json();
  $('rule-list').replaceChildren();
  (c.alerts.resource_rules || []).forEach(addRule);
  $('cfg-temp').value = c.alerts.temp_c;
  $('cfg-mem').value = c.alerts.mem_available_percent;
  $('cfg-root').value = c.alerts.root_free_gb;
  $('cfg-interval').value = c.collect.interval_s;
  $('cfg-keep-days').value = c.history?.keep_days ?? 90;
  $('cfg-history').value = c.collect.history_minutes;
  $('cfg-webuser').value = c.auth.username;
  $('cfg-webpass').value = '';
  $('cfg-err').textContent = '';
  fillNotifyFields(c.notify || {});
  refreshServiceStatus();
  $('cfg-info').textContent =
    `版本 ${c.version || 'beta1.4'} · 配置文件：${c.config_path || 'config.toml'}` +
    (c.mock ? ' · 当前是模拟数据模式' : '');
  $('config-mask').classList.remove('hidden');
  $('config-panel').classList.remove('hidden');
}

function closeConfig() {
  $('config-mask').classList.add('hidden');
  $('config-panel').classList.add('hidden');
}

$('btn-cfg-save').addEventListener('click', async () => {
  $('btn-cfg-save').disabled = true;
  $('cfg-err').textContent = '';
  try {
    const body = {
      alerts: {
        resource_rules: readRules(),
        temp_c: +$('cfg-temp').value,
        mem_available_percent: +$('cfg-mem').value,
        root_free_gb: +$('cfg-root').value,
      },
      collect: { interval_s: +$('cfg-interval').value, history_minutes: +$('cfg-history').value },
      history: { keep_days: +$('cfg-keep-days').value },
      auth: { username: $('cfg-webuser').value.trim() },
      notify: collectNotify(),
    };
    if ($('cfg-webpass').value) body.auth.password = $('cfg-webpass').value;
    const r = await authFetch('/api/config', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
    if (r.status === 401) return tryReauth();
    const res = await r.json();
    if (!r.ok) throw new Error(res.detail || '保存失败');
    closeConfig();
    toast('设置已保存并生效');
    setTimeout(reloadSnapshot, 800);
  } catch (e) {
    $('cfg-err').textContent = e.message || String(e);
  } finally {
    $('btn-cfg-save').disabled = false;
  }
});

// ---------- 退出 ----------

async function quitApp() {
  if (state.quitting) return;
  state.quitting = true;
  try { await authFetch('/api/shutdown', { method: 'POST' }); } catch {}
  state.ws && state.ws.close();
  document.body.innerHTML = `
    <div class="bye">
      <h1>监控服务已停止</h1>
      <p class="muted">${DESKTOP
        ? '窗口即将自动关闭。要重新开始，从开始菜单/桌面快捷方式再打开一次即可。'
        : '这个窗口可以直接关掉了。想再开就双击「启动监控」。'}</p>
    </div>`;
  if (DESKTOP && window.__XMM_BOOTSTRAP__.close) {
    try { window.close(); } catch {}
  }
}

$('btn-quit').addEventListener('click', quitApp);
$('btn-cfg-quit').addEventListener('click', quitApp);

$('btn-logout') && $('btn-logout').addEventListener('click', () => {
  clearAuth();
  showLogin();
});

// ---------- 安卓返回手势（beta2.1 起，beta2.3 完善） ----------
// 全面屏左右侧滑 / 三键返回：有面板开着就先关掉（等于"返回上一级"），
// 表单开着先退回列表；关无可关再交给壳收后台。桌面浏览器不调用，无副作用。
window.__xmmHandleBack = function () {
  if (!$('servers-panel').classList.contains('hidden')) {
    manager.back();
    return 'closed';
  }
  if (!$('config-panel').classList.contains('hidden')) { closeConfig(); return 'closed'; }
  if (probesOpen()) {
    if (!$('probe-form').classList.contains('hidden')) hideProbeForm();
    else closeProbes();
    return 'closed';
  }
  if (hisOpen()) { closeHistory(); return 'closed'; }
  return 'default';
};

// ---------- 启动 ----------

async function boot() {
  // 桌面版：壳进程已经注入了端口和一次性令牌，直接换会话
  if (DESKTOP) {
    try {
      await desktopLogin(window.__XMM_BOOTSTRAP__.token);
      await enterMain();
      return;
    } catch (e) {
      $('login-err').textContent = '桌面模式自动登录失败：' + e.message + '（请手动登录）';
      $('server-url').value = '';
    }
  }
  if (!token()) return; // 留在登录页
  try {
    await enterMain();
  } catch {
    await tryReauth().catch(() => showLogin());
  }
}

boot();
