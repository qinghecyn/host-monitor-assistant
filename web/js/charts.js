// 图表封装：优先 Chart.js（本地 vendor）；缺失时回退到内置 MiniChart 画布。
const THEME = {
  get text() { return getComputedStyle(document.documentElement).getPropertyValue('--muted').trim(); },
  get grid() { return getComputedStyle(document.documentElement).getPropertyValue('--border').trim(); },
};

window.addEventListener('appearancechange', () => {
  for (const chart of Object.values(window.Chart?.instances || {})) {
    for (const scale of Object.values(chart.options.scales || {})) {
      scale.ticks.color = THEME.text;
      scale.grid.color = THEME.grid;
    }
    if (chart.options.plugins.legend?.labels) chart.options.plugins.legend.labels.color = THEME.text;
    chart.update('none');
  }
});

function fmtTime(ts) {
  const d = new Date(ts);
  const p = (x) => String(x).padStart(2, '0');
  return p(d.getHours()) + ':' + p(d.getMinutes()) + ':' + p(d.getSeconds());
}

function hasChartJs() {
  return typeof window.Chart !== 'undefined';
}

function chartJsLine(canvas, colors, labels) {
  const data = {
    labels: [],
    datasets: colors.map((c, i) => ({
      label: labels[i], data: [], borderColor: c, backgroundColor: c,
      borderWidth: 1.6, pointRadius: 0, tension: 0.25, fill: false,
    })),
  };
  const chart = new window.Chart(canvas, {
    type: 'line',
    data,
    options: {
      responsive: true, maintainAspectRatio: false, animation: false,
      elements: { point: { radius: 0 } },
      scales: {
        x: { ticks: { maxTicksLimit: 5, color: THEME.text, font: { size: 10 } }, grid: { color: THEME.grid } },
        y: { ticks: { maxTicksLimit: 4, color: THEME.text, font: { size: 10 } }, grid: { color: THEME.grid } },
      },
      plugins: { legend: { display: colors.length > 1, labels: { color: THEME.text, boxWidth: 10, font: { size: 11 } } } },
    },
  });
  let pending = 0;
  let unit = '';
  return {
    setUnit(u) {
      if (u === unit) return;
      unit = u;
      chart.options.scales.y.title = { display: !!u, text: u, color: THEME.text, font: { size: 10 } };
      chart.update('none');
    },
    shift(n) {
      chart.data.labels.splice(0, n);
      chart.data.datasets.forEach(d => d.data.splice(0, n));
    },
    push(t, values) {
      chart.data.labels.push(fmtTime(t));
      values.forEach((v, i) => chart.data.datasets[i].data.push(v));
      if (!pending) pending = requestAnimationFrame(() => {
        pending = 0;
        chart.update('none');
      });
    },
    reset() {
      chart.data.labels = [];
      chart.data.datasets.forEach((d) => (d.data = []));
      chart.update('none');
    },
    destroy() {
      cancelAnimationFrame(pending);
      try { chart.destroy(); } catch {}
    },
  };
}

// 极简画布折线（无依赖兜底）
function miniLine(canvas, colors) {
  const ctx = canvas.getContext('2d');
  const series = colors.map(() => []);
  function draw() {
    const w = (canvas.width = canvas.clientWidth * devicePixelRatio);
    const h = (canvas.height = canvas.clientHeight * devicePixelRatio);
    ctx.clearRect(0, 0, w, h);
    const n = Math.max(...series.map((s) => s.length), 0);
    if (!n) return;
    let lo = Infinity, hi = -Infinity;
    series.forEach((s) => s.forEach((v) => { if (v != null) { lo = Math.min(lo, v); hi = Math.max(hi, v); } }));
    if (lo === Infinity) return;
    if (hi - lo < 1e-6) { hi += 1; lo -= 1; }
    const pad = 6 * devicePixelRatio;
    const X = (i) => (n <= 1 ? pad : (i / (n - 1)) * (w - pad * 2) + pad);
    const Y = (v) => h - pad - ((v - lo) / (hi - lo)) * (h - pad * 2);
    ctx.lineWidth = 1.5 * devicePixelRatio;
    series.forEach((s, si) => {
      ctx.strokeStyle = colors[si];
      ctx.beginPath();
      let started = false;
      s.forEach((v, i) => {
        if (v == null) { started = false; return; }
        if (!started) { ctx.moveTo(X(i), Y(v)); started = true; }
        else ctx.lineTo(X(i), Y(v));
      });
      ctx.stroke();
    });
  }
  return {
    shift(n) { series.forEach((s) => s.splice(0, n)); },
    push(t, values) {
      series.forEach((s, i) => s.push(values[i]));
      draw();
    },
    reset() { series.forEach((s) => (s.length = 0)); draw(); },
    setUnit() {},
    destroy() {},
  };
}

export function makeChart(canvasId, colors, labels, limit = 300, intervalSeconds = 1) {
  const canvas = document.getElementById(canvasId);
  if (!canvas) return null;
  const impl = hasChartJs() ? chartJsLine(canvas, colors, labels) : miniLine(canvas, colors);
  const times = [];
  return {
    push(t, values) {
      const cutoff = t - limit * intervalSeconds * 1000;
      let expired = 0;
      while (expired < times.length && times[expired] < cutoff) expired++;
      expired = Math.max(expired, times.length + 1 - limit);
      if (expired > 0) {
        times.splice(0, expired);
        impl.shift(expired);
      }
      times.push(t);
      impl.push(t, values);
    },
    reset() { times.length = 0; impl.reset(); },
    setUnit(u) { if (impl.setUnit) impl.setUnit(u); },
    destroy() { impl.destroy(); },
  };
}

// 静态整段曲线（历史趋势用）：一次性给定 [ts, value] 点列，不做滚动窗口。
// x 轴用 linear（Chart.js 的 time 尺度需要额外的日期适配器，本地化包里没有）。
export function makeStaticChart(canvasId, color, label, fmtX) {
  const canvas = document.getElementById(canvasId);
  if (!canvas || !hasChartJs()) return null;
  let chart = null;

  function render(points) {
    const data = points.map((p) => ({ x: p[0] * 1000, y: p[1] }));
    if (chart) { chart.destroy(); chart = null; }
    // 单点/少点时 linear 轴会自动撑到 0 起点，x 域必须手动钉住；点少时给可见圆点
    const xs = data.map((d) => d.x);
    const lo = xs.length ? Math.min(...xs) : 0;
    const hi = xs.length ? Math.max(...xs) : 1;
    const pad = Math.max(60000, (hi - lo) * 0.04);
    chart = new window.Chart(canvas, {
      type: 'line',
      data: { datasets: [{
        label, data, borderColor: color, backgroundColor: color,
        borderWidth: 1.6, pointRadius: data.length <= 2 ? 3 : 0, tension: 0.2, fill: false,
      }]},
      options: {
        responsive: true, maintainAspectRatio: false, animation: false,
        parsing: false, normalized: true,
        elements: { point: { radius: data.length <= 2 ? 3 : 0 } },
        scales: {
          x: { type: 'linear', min: lo - pad, max: hi + pad,
               ticks: { maxTicksLimit: 6, color: THEME.text, font: { size: 10 },
                        callback: (v) => (fmtX ? fmtX(v) : v) },
               grid: { color: THEME.grid } },
          y: { ticks: { maxTicksLimit: 5, color: THEME.text, font: { size: 10 } },
               grid: { color: THEME.grid } },
        },
        plugins: { legend: { display: false },
                   tooltip: { callbacks: { title: (items) => (fmtX ? fmtX(items[0].parsed.x) : '') } } },
      },
    });
  }

  return {
    render,
    destroy() { try { chart && chart.destroy(); } catch {} chart = null; },
  };
}
