//! WebSocket 广播中心（替代 server/ws.py 的 Hub）。
//!
//! 用 tokio broadcast 通道：每个 WS 连接一个订阅者；慢客户端丢帧不拖累别人
//! （指标帧每秒一条，丢一帧无所谓，下帧即到）。

use serde_json::Value;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct Hub {
    tx: broadcast::Sender<String>,
}

impl Hub {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.tx.subscribe()
    }

    pub fn broadcast(&self, payload: &Value) {
        if self.tx.receiver_count() == 0 {
            return;
        }
        let _ = self.tx.send(payload.to_string());
    }

    pub fn client_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}
