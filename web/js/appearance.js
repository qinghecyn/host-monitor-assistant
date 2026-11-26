// Desktop uses a new loopback port at each launch. Cookies keep display preferences across ports.
(() => {
  const root = document.documentElement;
  function saved(key) {
    return document.cookie.split('; ').find(item => item.startsWith(key + '='))?.split('=')[1];
  }
  const system = matchMedia('(prefers-color-scheme: dark)');
  function apply() {
    root.dataset.theme = saved('xmm_theme') || (system.matches ? 'dark' : 'light');
    root.dataset.mode = saved('xmm_view') === 'classic' ? 'classic' : 'ring';
    document.querySelectorAll('[data-mode]').forEach(button => {
      if (button.tagName === 'BUTTON') button.setAttribute('aria-pressed', button.dataset.mode === root.dataset.mode);
    });
    document.querySelectorAll('[data-theme-toggle]').forEach(button => {
      button.textContent = root.dataset.theme === 'dark' ? '☀ 浅色' : '☾ 深色';
      button.title = '切换到' + (root.dataset.theme === 'dark' ? '浅色模式' : '深色模式');
    });
    window.dispatchEvent(new Event('appearancechange'));
  }
  function save(key, value) {
    document.cookie = key + '=' + value + '; Path=/; Max-Age=31536000; SameSite=Lax';
    apply();
  }
  apply();
  document.addEventListener('DOMContentLoaded', apply);
  system.addEventListener('change', apply);
  document.addEventListener('click', event => {
    const button = event.target.closest('button');
    if (!button) return;
    if (button.hasAttribute('data-theme-toggle')) save('xmm_theme', root.dataset.theme === 'dark' ? 'light' : 'dark');
    if (button.dataset.mode) save('xmm_view', button.dataset.mode);
  });
})();
