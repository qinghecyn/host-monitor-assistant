// 服务器管理抽屉：把 config.toml 的 [targets.*] 变成可视化的增删改查。
// 所有写操作都打后端 API，后端负责落盘 + 立即重建采集器。

import { esc, toast } from './ui.js';

const $ = (id) => document.getElementById(id);
const NAME_RE = /^[A-Za-z0-9_-]{1,32}$/;

const STATUS_TEXT = { online: '在线', offline: '离线', connecting: '连接中' };

// 安卓 App 里没有导入私钥的入口（应用私有目录外部放不进文件），
// 别拿 Windows 路径误导用户；桌面端保持原样。
const IS_ANDROID = /Android/i.test(navigator.userAgent);
if (IS_ANDROID) {
  $('sv-key').placeholder = '手机版建议用密码登录';
}

export function initServerManager({ authFetch, onChanged, onPick }) {
  let targets = [];
  let mode = 'add';          // add | edit
  let editing = null;        // 正在编辑的 name
  let pendingDelete = null;  // 等待二次确认删除的 name

  // ---------- 打开/关闭 ----------

  function open(editName) {
    $('servers-mask').classList.remove('hidden');
    $('servers-panel').classList.remove('hidden');
    pendingDelete = null;
    hideForm();
    refresh().then(() => {
      if (editName) startEdit(editName);
    });
  }

  function close() {
    $('servers-mask').classList.add('hidden');
    $('servers-panel').classList.add('hidden');
    hideForm();
  }

  // 安卓侧滑返回的"上一级"：表单开着先退回列表，列表开着才关抽屉。
  function back() {
    if ($('servers-panel').classList.contains('hidden')) return false;
    if (!$('sv-form').classList.contains('hidden')) {
      hideForm();
      return true;
    }
    close();
    return true;
  }

  async function refresh() {
    try {
      const r = await authFetch('/api/targets');
      if (!r.ok) throw new Error('HTTP ' + r.status);
      const d = await r.json();
      targets = d.targets || [];
      renderList();
    } catch (e) {
      $('sv-list').innerHTML = `<p class="err">读取服务器列表失败：${esc(e.message)}</p>`;
    }
  }

  // ---------- 列表 ----------

  function renderList() {
    const box = $('sv-list');
    if (!targets.length) {
      box.innerHTML = `<div class="sv-empty">还没有服务器。点下面按钮添加第一台。</div>`;
      return;
    }
    box.innerHTML = targets.map((t) => {
      const st = t.status || 'offline';
      const confirming = pendingDelete === t.name;
      return `
        <div class="sv-card ${st}" data-name="${esc(t.name)}">
          <div class="sv-main">
            <div class="sv-title">
              <span class="dot ${st}"></span>
              <b>${esc(t.label || t.name)}</b>
              <span class="sv-key">${esc(t.name)}</span>
              <span class="sv-status ${st}">${STATUS_TEXT[st] || st}</span>
            </div>
            <div class="sv-sub">${esc(t.host)}:${t.port} · ${esc(t.username)}
              ${t.has_password ? '' : (t.private_key ? ' · 密钥登录' : ' · <span class="warn-text">未设密码</span>')}
              ${t.note ? ' · ' + esc(t.note) : ''}
            </div>
          </div>
          <div class="sv-actions">
            ${confirming ? `
              <span class="sv-confirm">确定删除？</span>
              <button class="ghost sm danger" data-act="del-yes">删除</button>
              <button class="ghost sm" data-act="del-no">取消</button>
            ` : `
              <button class="ghost sm" data-act="use">查看</button>
              <button class="ghost sm" data-act="test">测试</button>
              <button class="ghost sm" data-act="edit">编辑</button>
              <button class="ghost sm danger" data-act="del">删除</button>
            `}
          </div>
        </div>`;
    }).join('');
  }

  $('sv-list').addEventListener('click', async (ev) => {
    const btn = ev.target.closest('button[data-act]');
    if (!btn) return;
    const card = btn.closest('.sv-card');
    const name = card && card.dataset.name;
    const act = btn.dataset.act;
    if (!name) return;

    if (act === 'edit') { startEdit(name); return; }
    if (act === 'use') { onPick && onPick(name); close(); return; }
    if (act === 'del') { pendingDelete = name; renderList(); return; }
    if (act === 'del-no') { pendingDelete = null; renderList(); return; }
    if (act === 'del-yes') {
      pendingDelete = null;
      btn.disabled = true;
      try {
        const r = await authFetch('/api/targets/' + encodeURIComponent(name), { method: 'DELETE' });
        if (!r.ok) throw new Error((await r.json().catch(() => ({}))).detail || 'HTTP ' + r.status);
        toast('已删除 ' + name);
        await refresh();
        onChanged && onChanged();
      } catch (e) {
        toast('删除失败：' + e.message, 'err');
      }
      return;
    }
    if (act === 'test') {
      btn.disabled = true;
      btn.textContent = '测试中…';
      try {
        const r = await authFetch('/api/targets/test', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ name }),
        });
        const d = await r.json();
        if (d.ok) toast(`连接成功 · ${d.os || ''} ${d.latency_ms != null ? d.latency_ms + 'ms' : ''}`.trim());
        else toast('连接失败：' + (d.message || d.detail || '未知错误'), 'err', 6000);
        await refresh();
      } catch (e) {
        toast('测试失败：' + e.message, 'err');
      } finally {
        btn.disabled = false;
        btn.textContent = '测试';
      }
    }
  });

  // ---------- 表单 ----------

  function hideForm() {
    $('sv-form').classList.add('hidden');
    $('sv-err').textContent = '';
    $('sv-test').classList.add('hidden');
    $('btn-sv-add').classList.remove('hidden');
  }

  function startAdd() {
    mode = 'add';
    editing = null;
    $('sv-form-title').textContent = '添加服务器';
    $('sv-name').value = '';
    $('sv-name').disabled = false;
    $('sv-label').value = '';
    $('sv-host').value = '';
    $('sv-port').value = 22;
    $('sv-user').value = 'root';
    $('sv-pass').value = '';
    $('sv-pass').placeholder = 'SSH 登录密码';
    $('sv-key').value = '';
    $('sv-note').value = '';
    $('sv-hint').textContent = IS_ANDROID
      ? '密码会明文存在本机的 config.toml 里（仅本机可读）。手机版建议直接用密码登录。'
      : '密码会明文存在本机的 config.toml 里（仅本机可读）；也可以改用私钥登录。';
    $('sv-err').textContent = '';
    $('sv-test').classList.add('hidden');
    $('btn-sv-add').classList.add('hidden');
    $('sv-form').classList.remove('hidden');
    $('sv-name').focus();
  }

  function startEdit(name) {
    const t = targets.find((x) => x.name === name);
    if (!t) return;
    mode = 'edit';
    editing = name;
    $('sv-form-title').textContent = '编辑：' + (t.label || t.name);
    $('sv-name').value = t.name;
    $('sv-name').disabled = true;
    $('sv-label').value = t.label && t.label !== t.name ? t.label : '';
    $('sv-host').value = t.host || '';
    $('sv-port').value = t.port || 22;
    $('sv-user').value = t.username || '';
    $('sv-pass').value = '';
    $('sv-pass').placeholder = t.has_password ? '留空 = 保持原密码' : '这台还没设密码';
    $('sv-key').value = t.private_key || '';
    $('sv-note').value = t.note || '';
    $('sv-hint').textContent = '名称不可改（它是配置文件里的键）。要改名就先删了重建。';
    $('sv-err').textContent = '';
    $('sv-test').classList.add('hidden');
    $('btn-sv-add').classList.add('hidden');
    $('sv-form').classList.remove('hidden');
    $('sv-host').focus();
  }

  function collect() {
    const body = {
      label: $('sv-label').value.trim(),
      host: $('sv-host').value.trim(),
      port: +$('sv-port').value || 22,
      username: $('sv-user').value.trim() || 'root',
      private_key: $('sv-key').value.trim(),
      note: $('sv-note').value.trim(),
    };
    const pw = $('sv-pass').value;
    if (pw) body.password = pw;
    return body;
  }

  function validate(body) {
    if (mode === 'add') {
      const name = $('sv-name').value.trim();
      if (!NAME_RE.test(name)) return '名称只能是 1-32 位字母、数字、下划线或中划线';
      if (targets.some((t) => t.name === name)) return '已经有同名服务器了';
    }
    if (!body.host) return '地址不能为空';
    if (!/^[^\s]+$/.test(body.host)) return '地址里不能有空格';
    if (!(body.port >= 1 && body.port <= 65535)) return '端口要在 1-65535 之间';
    if (!body.username) return '用户名不能为空';
    if (mode === 'add' && !body.password && !body.private_key) return '请填密码，或指定私钥路径';
    return '';
  }

  $('btn-sv-add').addEventListener('click', startAdd);
  $('btn-empty-add') && $('btn-empty-add').addEventListener('click', () => open(null));
  $('btn-sv-close').addEventListener('click', close);
  $('servers-mask').addEventListener('click', close);
  $('btn-sv-cancel').addEventListener('click', hideForm);

  $('btn-sv-test').addEventListener('click', async () => {
    const body = collect();
    // 测试时允许"编辑模式不改密码"，所以只在明显缺字段时报错
    if (!body.host || !body.username) {
      $('sv-err').textContent = '先填地址和用户名';
      return;
    }
    const btn = $('btn-sv-test');
    btn.disabled = true;
    btn.textContent = '测试中…';
    $('sv-test').classList.add('hidden');
    $('sv-err').textContent = '';
    try {
      const payload = Object.assign({}, body);
      if (mode === 'edit') payload.name = editing;
      const r = await authFetch('/api/targets/test', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(payload),
      });
      const d = await r.json();
      const el = $('sv-test');
      el.classList.remove('hidden');
      if (d.ok) {
        el.className = 'test-result ok';
        el.textContent = `✓ ${d.message}${d.os ? ' · ' + d.os : ''}${d.cores ? ' · ' + d.cores + ' 核' : ''}`;
      } else {
        el.className = 'test-result bad';
        el.textContent = '✗ ' + (d.message || d.detail || '连接失败');
      }
    } catch (e) {
      $('sv-test').classList.remove('hidden');
      $('sv-test').className = 'test-result bad';
      $('sv-test').textContent = '✗ ' + e.message;
    } finally {
      btn.disabled = false;
      btn.textContent = '测试连接';
    }
  });

  $('sv-form').addEventListener('submit', async (ev) => {
    ev.preventDefault();
    const body = collect();
    const err = validate(body);
    if (err) { $('sv-err').textContent = err; return; }
    const btn = $('btn-sv-save');
    btn.disabled = true;
    $('sv-err').textContent = '';
    try {
      let r;
      if (mode === 'add') {
        body.name = $('sv-name').value.trim();
        r = await authFetch('/api/targets', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(body),
        });
      } else {
        r = await authFetch('/api/targets/' + encodeURIComponent(editing), {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(body),
        });
      }
      const d = await r.json().catch(() => ({}));
      if (!r.ok) throw new Error(d.detail || 'HTTP ' + r.status);
      toast(mode === 'add' ? '已添加，正在连接…' : '已保存，配置已生效');
      hideForm();
      await refresh();
      onChanged && onChanged(mode === 'add' ? body.name : editing);
    } catch (e) {
      $('sv-err').textContent = e.message;
    } finally {
      btn.disabled = false;
    }
  });

  return { open, close, back, refresh, isOpen: () => !$('servers-panel').classList.contains('hidden') };
}
