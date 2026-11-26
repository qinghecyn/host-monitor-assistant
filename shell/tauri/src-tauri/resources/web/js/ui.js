// 通用小组件：轻提示、HTML 转义。

export function esc(s) {
  return String(s == null ? '' : s).replace(/[&<>"']/g, (c) => (
    { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]
  ));
}

let timer = null;

export function toast(msg, type = 'ok', ms = 2600) {
  const el = document.getElementById('toast');
  if (!el) return;
  el.textContent = msg;
  el.className = 'toast ' + type;
  clearTimeout(timer);
  timer = setTimeout(() => { el.className = 'toast hidden'; }, ms);
}
