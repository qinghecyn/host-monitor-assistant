// WebSocket：断线自动重连（指数退避，封顶 15s），先发 auth 再收数据。

import { wsUrl, token } from './auth.js';

export function connectWS({ onHello, onMessage, onState }) {
  let ws = null;
  let backoff = 1000;
  let closed = false;

  function open() {
    if (closed) return;
    onState && onState('connecting');
    try { ws = new WebSocket(wsUrl()); } catch { schedule(); return; }

    ws.onopen = () => ws.send(JSON.stringify({ type: 'auth', token: token() }));

    ws.onmessage = (ev) => {
      let m = null;
      try { m = JSON.parse(ev.data); } catch { return; }
      if (m.type === 'hello') {
        backoff = 1000;
        onHello && onHello(m);
      } else if (m.type === 'pong') {
        // ignore
      } else {
        onMessage && onMessage(m);
      }
    };

    ws.onclose = (ev) => {
      if (closed) return;
      if (ev.code === 4001) { onState && onState('unauthorized'); return; }
      onState && onState('ws-down');
      schedule();
    };
    ws.onerror = () => { try { ws.close(); } catch {} };
  }

  function schedule() {
    setTimeout(open, backoff);
    backoff = Math.min(15000, backoff * 2);
  }

  function send(obj) {
    if (ws && ws.readyState === 1) ws.send(JSON.stringify(obj));
  }

  function close() {
    closed = true;
    try { ws && ws.close(); } catch {}
  }

  open();
  const keepalive = setInterval(() => send({ type: 'ping' }), 25000);
  return { close, stopKeepalive: () => clearInterval(keepalive) };
}
