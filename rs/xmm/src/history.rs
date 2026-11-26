//! 每主机 5 分钟历史：内存环形缓冲（移植自 server/history.py）。

use serde_json::Value;
use std::collections::VecDeque;

pub struct History {
    d: VecDeque<Value>,
    maxlen: usize,
    window_secs: f64,
}

impl History {
    pub fn new(maxlen: usize, window_secs: f64) -> Self {
        Self {
            d: VecDeque::new(),
            maxlen: maxlen.max(1),
            window_secs,
        }
    }

    pub fn push(&mut self, frame: Value) {
        if self.d.len() >= self.maxlen {
            self.d.pop_front();
        }
        self.d.push_back(frame);
        self.trim_time();
    }

    pub fn resize(&mut self, maxlen: usize, window_secs: f64) {
        self.maxlen = maxlen.max(1);
        let excess = self.d.len().saturating_sub(self.maxlen);
        self.d.drain(..excess);
        self.window_secs = window_secs;
        self.trim_time();
    }

    fn trim_time(&mut self) {
        let Some(latest) = self.d.back().and_then(|f| f["ts"].as_f64()) else { return };
        let cutoff = latest - self.window_secs;
        while self.d.front().and_then(|f| f["ts"].as_f64()).is_some_and(|ts| ts < cutoff) {
            self.d.pop_front();
        }
    }

    pub fn list(&self) -> Vec<Value> {
        self.d.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resize_preserves_recent_samples_and_expires_gaps() {
        let mut history = History::new(4, 40.0);
        for ts in [10, 20, 30, 40] { history.push(json!({"ts": ts})); }
        history.resize(2, 20.0);
        assert_eq!(history.list(), vec![json!({"ts": 30}), json!({"ts": 40})]);
        history.push(json!({"ts": 150}));
        assert_eq!(history.list(), vec![json!({"ts": 150})]);
    }
}
